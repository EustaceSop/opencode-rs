// The console account service (`opencode/src/account/account.ts`): device login, token refresh, orgs and the
// active org's remote config.
use opencode_storage::account::{AccountInfo, AccountRow, NewAccount, normalize_server_url};
use opencode_storage::{Store, id};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::time::Duration;

pub use opencode_storage::account::AccountInfo as Info;

pub const CLIENT_ID: &str = "opencode-cli";
const EAGER_REFRESH_MS: i64 = 5 * 60 * 1000;
const READ_RETRIES: u32 = 2;

#[derive(Debug, Clone, thiserror::Error)]
pub enum AccountError {
    // `AccountServiceError`
    #[error("{message}")]
    Service { message: String, cause: Option<String> },
    // `AccountTransportError`: no HTTP response at all.
    #[error("{}", transport_message(.method, .url, .description.as_deref()))]
    Transport { method: String, url: String, description: Option<String> },
    // `AccountRepoError`
    #[error("Database operation failed")]
    Repo(String),
}

fn transport_message(method: &str, url: &str, description: Option<&str>) -> String {
    let mut lines = vec![format!("Could not reach {method} {url}."), "This failed before the server returned an HTTP response.".to_string()];
    lines.extend(description.filter(|d| !d.is_empty()).map(str::to_string));
    lines.push("Check your network, proxy, or VPN configuration and try again.".into());
    lines.join("\n")
}

impl From<opencode_storage::StoreError> for AccountError {
    fn from(error: opencode_storage::StoreError) -> Self {
        AccountError::Repo(error.to_string())
    }
}

fn service(message: &str, cause: impl ToString) -> AccountError {
    AccountError::Service { message: message.into(), cause: Some(cause.to_string()) }
}

pub type Result<T> = std::result::Result<T, AccountError>;

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Org {
    pub id: String,
    pub name: String,
}

// `Login`: what `poll` needs to finish a device login.
#[derive(Debug, Clone, PartialEq)]
pub struct Login {
    pub code: String,
    pub user: String,
    pub url: String,
    pub server: String,
    pub expiry: Duration,
    pub interval: Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Poll {
    Success { email: String },
    Pending,
    Slow,
    Expired,
    Denied,
    Error(String),
}

// The active org's config, ready for `config::load_with_remote`.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveConfig {
    pub account_id: String,
    pub org_id: String,
    pub url: String,
    pub config: Option<Map<String, Value>>,
    pub token: Option<String>,
}

#[derive(Deserialize)]
struct DeviceAuth {
    device_code: String,
    user_code: String,
    verification_uri_complete: String,
    expires_in: f64,
    interval: f64,
}

#[derive(Deserialize)]
struct DeviceTokenSuccess {
    access_token: String,
    refresh_token: String,
    token_type: String,
    expires_in: f64,
}

#[derive(Deserialize)]
struct DeviceTokenError {
    error: String,
    #[allow(dead_code)]
    error_description: String,
}

#[derive(Deserialize)]
struct TokenRefresh {
    access_token: String,
    refresh_token: String,
    expires_in: f64,
}

#[derive(Deserialize)]
struct User {
    id: String,
    email: String,
}

#[derive(Deserialize)]
struct RemoteConfig {
    config: Map<String, Value>,
}

fn seconds(value: f64) -> Duration {
    Duration::from_secs_f64(value.max(0.0))
}

fn now() -> i64 {
    id::now_ms() as i64
}

fn fresh(expiry: Option<i64>, now: i64) -> bool {
    expiry.is_some_and(|expiry| expiry > now + EAGER_REFRESH_MS)
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|e| service("Failed to decode response", e))
}

// `HttpClient.retryTransient`'s transient responses.
fn transient(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
}

struct Reply {
    status: u16,
    body: Vec<u8>,
}

impl Reply {
    fn ok(self, message: &str) -> Result<Reply> {
        if (200..300).contains(&self.status) {
            Ok(self)
        } else {
            Err(AccountError::Service { message: message.into(), cause: Some(format!("StatusCode: {}", self.status)) })
        }
    }
}

#[derive(Clone)]
pub struct Accounts {
    store: Store,
    http: reqwest::Client,
    // Concurrent callers share one refresh, like the original's refresh cache.
    refreshing: std::sync::Arc<tokio::sync::Mutex<()>>,
}

impl Accounts {
    pub fn new(store: Store, http: reqwest::Client) -> Self {
        Self { store, http, refreshing: Default::default() }
    }

    // One request; `read` requests are retried on transport failures and transient statuses.
    async fn execute(&self, method: reqwest::Method, url: &str, bearer: Option<&str>, headers: &[(&str, &str)], body: Option<Value>, read: bool) -> Result<Reply> {
        let mut attempt = 0;
        loop {
            let mut request = self.http.request(method.clone(), url).header("accept", "application/json");
            if let Some(token) = bearer {
                request = request.header("authorization", format!("Bearer {token}"));
            }
            for (key, value) in headers {
                request = request.header(*key, *value);
            }
            if let Some(body) = &body {
                request = request.header("content-type", "application/json").body(body.to_string());
            }
            let result = match request.send().await {
                Ok(response) => {
                    let status = response.status().as_u16();
                    match response.bytes().await {
                        Ok(bytes) => Ok(Reply { status, body: bytes.to_vec() }),
                        Err(error) => Err(error),
                    }
                }
                Err(error) => Err(error),
            };
            let retry = match &result {
                Ok(reply) => transient(reply.status),
                Err(_) => true,
            };
            if !read || !retry || attempt >= READ_RETRIES {
                return result.map_err(|error| {
                    let description = std::error::Error::source(&error).map_or_else(|| error.to_string(), |source| source.to_string());
                    AccountError::Transport { method: method.to_string(), url: url.to_string(), description: Some(description) }
                });
            }
            let base = 200u64 << attempt;
            let jitter = 80 + u64::from(id::now_ms() as u32 % 41);
            tokio::time::sleep(Duration::from_millis(base * jitter / 100)).await;
            attempt += 1;
        }
    }

    pub fn active(&self) -> Result<Option<AccountInfo>> {
        Ok(self.store.account_active()?)
    }

    pub fn list(&self) -> Result<Vec<AccountInfo>> {
        Ok(self.store.account_list()?)
    }

    pub fn use_org(&self, account_id: &str, org_id: Option<&str>) -> Result<()> {
        Ok(self.store.account_use(account_id, org_id)?)
    }

    async fn refresh(&self, row: &AccountRow) -> Result<String> {
        let started = now();
        let body = json!({"grant_type": "refresh_token", "refresh_token": row.refresh_token, "client_id": CLIENT_ID});
        let reply = self.execute(reqwest::Method::POST, &format!("{}/auth/device/token", row.url), None, &[], Some(body), false).await?;
        let parsed: TokenRefresh = decode(&reply.ok("HTTP request failed")?.body)?;
        let expiry = started + seconds(parsed.expires_in).as_millis() as i64;
        self.store.account_persist_token(&row.id, &parsed.access_token, &parsed.refresh_token, Some(expiry))?;
        Ok(parsed.access_token)
    }

    async fn access(&self, account_id: &str) -> Result<Option<(AccountRow, String)>> {
        let Some(row) = self.store.account_row(account_id)? else { return Ok(None) };
        if fresh(row.token_expiry, now()) {
            let token = row.access_token.clone();
            return Ok(Some((row, token)));
        }
        let _turn = self.refreshing.lock().await;
        let Some(row) = self.store.account_row(account_id)? else {
            return Err(AccountError::Service { message: "Account not found during token refresh".into(), cause: None });
        };
        if fresh(row.token_expiry, now()) {
            let token = row.access_token.clone();
            return Ok(Some((row, token)));
        }
        let token = self.refresh(&row).await?;
        Ok(Some((row, token)))
    }

    pub async fn token(&self, account_id: &str) -> Result<Option<String>> {
        Ok(self.access(account_id).await?.map(|(_, token)| token))
    }

    async fn fetch_orgs(&self, url: &str, token: &str) -> Result<Vec<Org>> {
        let reply = self.execute(reqwest::Method::GET, &format!("{url}/api/orgs"), Some(token), &[], None, true).await?;
        decode(&reply.ok("HTTP request failed")?.body)
    }

    async fn fetch_user(&self, url: &str, token: &str) -> Result<User> {
        let reply = self.execute(reqwest::Method::GET, &format!("{url}/api/user"), Some(token), &[], None, true).await?;
        decode(&reply.ok("HTTP request failed")?.body)
    }

    pub async fn orgs(&self, account_id: &str) -> Result<Vec<Org>> {
        match self.access(account_id).await? {
            Some((row, token)) => self.fetch_orgs(&row.url, &token).await,
            None => Ok(Vec::new()),
        }
    }

    // Every account with its orgs; an account whose orgs cannot be fetched has none.
    pub async fn orgs_by_account(&self) -> Result<Vec<(AccountInfo, Vec<Org>)>> {
        let accounts = self.list()?;
        let orgs = futures_util::future::join_all(accounts.iter().map(|account| self.orgs(&account.id))).await;
        Ok(accounts.into_iter().zip(orgs).map(|(account, orgs)| (account, orgs.unwrap_or_default())).collect())
    }

    // Removing the active account makes the first org of the remaining accounts active.
    pub async fn remove(&self, account_id: &str) -> Result<()> {
        let active = self.active()?;
        self.store.account_remove(account_id)?;
        if active.is_none_or(|active| active.id != account_id) {
            return Ok(());
        }
        let next = self.orgs_by_account().await?.into_iter().find_map(|(account, orgs)| orgs.into_iter().next().map(|org| (account.id, org.id)));
        if let Some((account_id, org_id)) = next {
            self.store.account_use(&account_id, Some(&org_id))?;
        }
        Ok(())
    }

    // `<url>/api/config` for an org: none when the server has none (404).
    pub async fn config(&self, account_id: &str, org_id: &str) -> Result<Option<Map<String, Value>>> {
        let Some((row, token)) = self.access(account_id).await? else { return Ok(None) };
        let reply = self.execute(reqwest::Method::GET, &format!("{}/api/config", row.url), Some(&token), &[("x-org-id", org_id)], None, true).await?;
        if reply.status == 404 {
            return Ok(None);
        }
        let parsed: RemoteConfig = decode(&reply.ok("Account service operation failed")?.body)?;
        Ok(Some(parsed.config))
    }

    // What config loading takes from the console: the active org's config and the account token. Errors are the
    // caller's to ignore, as the original does.
    pub async fn active_config(&self) -> Result<Option<ActiveConfig>> {
        let Some(account) = self.active()? else { return Ok(None) };
        let Some(org_id) = account.active_org_id.clone() else { return Ok(None) };
        let (config, token) = futures_util::future::join(self.config(&account.id, &org_id), self.token(&account.id)).await;
        Ok(Some(ActiveConfig { account_id: account.id, org_id, url: account.url, config: config?, token: token? }))
    }

    pub async fn login(&self, server: &str) -> Result<Login> {
        let server = normalize_server_url(server).map_err(|e| service("Invalid URL", e))?;
        let body = json!({"client_id": CLIENT_ID});
        let reply = self.execute(reqwest::Method::POST, &format!("{server}/auth/device/code"), None, &[], Some(body), false).await?;
        let parsed: DeviceAuth = decode(&reply.ok("HTTP request failed")?.body)?;
        let base = url::Url::parse(&format!("{server}/")).map_err(|e| service("Invalid device verification URL", e))?;
        let verification = base.join(&parsed.verification_uri_complete).map_err(|e| service("Invalid device verification URL", e))?;
        if !matches!(verification.scheme(), "http" | "https") {
            return Err(service("Invalid device verification URL", "expected HTTP(S)"));
        }
        Ok(Login {
            code: parsed.device_code,
            user: parsed.user_code,
            url: verification.to_string(),
            server,
            expiry: seconds(parsed.expires_in),
            interval: seconds(parsed.interval),
        })
    }

    pub async fn poll(&self, login: &Login) -> Result<Poll> {
        let body = json!({"grant_type": "urn:ietf:params:oauth:grant-type:device_code", "device_code": login.code, "client_id": CLIENT_ID});
        let reply = self.execute(reqwest::Method::POST, &format!("{}/auth/device/token", login.server), None, &[], Some(body), false).await?;
        let success = serde_json::from_slice::<DeviceTokenSuccess>(&reply.body).ok().filter(|s| s.token_type == "Bearer");
        let Some(success) = success else {
            let failure: DeviceTokenError = decode(&reply.body)?;
            return Ok(match failure.error.as_str() {
                "authorization_pending" => Poll::Pending,
                "slow_down" => Poll::Slow,
                "expired_token" => Poll::Expired,
                "access_denied" => Poll::Denied,
                other => Poll::Error(other.to_string()),
            });
        };
        let (user, orgs) = futures_util::future::join(self.fetch_user(&login.server, &success.access_token), self.fetch_orgs(&login.server, &success.access_token)).await;
        let (user, orgs) = (user?, orgs?);
        let expiry = now() + seconds(success.expires_in).as_millis() as i64;
        self.store.account_persist(&NewAccount {
            id: user.id,
            email: user.email.clone(),
            url: login.server.clone(),
            access_token: success.access_token,
            refresh_token: success.refresh_token,
            expiry,
            org_id: orgs.first().map(|org| org.id.clone()),
        })?;
        Ok(Poll::Success { email: user.email })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    type Seen = Arc<Mutex<Vec<String>>>;

    // A console stand-in: `(method path) -> (status, body)`; every request line, header block and body is recorded.
    async fn console(routes: Vec<(&'static str, u16, Value)>, seen: Seen) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let routes = Arc::new(Mutex::new(routes));
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let (routes, seen) = (routes.clone(), seen.clone());
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let (head, body) = loop {
                        let mut chunk = [0u8; 4096];
                        let n = socket.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..n]);
                        let Some(split) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                        let head = String::from_utf8_lossy(&buffer[..split]).into_owned();
                        let length = head.lines().find_map(|l| l.to_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))).unwrap_or(0);
                        if buffer.len() >= split + 4 + length {
                            break (head, String::from_utf8_lossy(&buffer[split + 4..split + 4 + length]).into_owned());
                        }
                    };
                    let line = head.lines().next().unwrap_or_default().to_string();
                    let key: String = line.split(' ').take(2).collect::<Vec<_>>().join(" ");
                    seen.lock().unwrap().push(format!("{head}\n\n{body}"));
                    let reply = {
                        let mut routes = routes.lock().unwrap();
                        match routes.iter().position(|(route, _, _)| *route == key) {
                            // A route listed twice answers once per entry, in order.
                            Some(index) if routes.iter().filter(|(route, _, _)| *route == key).count() > 1 => Some(routes.remove(index)),
                            Some(index) => Some(routes[index].clone()),
                            None => None,
                        }
                    };
                    let (status, body) = reply.map_or((404, json!({"error": "missing"})), |(_, status, body)| (status, body));
                    let text = body.to_string();
                    let response = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{text}", text.len());
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        url
    }

    fn accounts() -> Accounts {
        Accounts::new(Store::open_in_memory().unwrap(), reqwest::Client::new())
    }

    #[tokio::test]
    async fn device_login_persists_the_account_with_its_first_org() {
        let seen = Seen::default();
        let url = console(
            vec![
                ("POST /console/auth/device/code", 200, json!({"device_code": "dc", "user_code": "ABCD", "verification_uri_complete": "/device?code=ABCD", "expires_in": 600, "interval": 1})),
                ("POST /console/auth/device/token", 400, json!({"error": "authorization_pending", "error_description": "wait"})),
                ("POST /console/auth/device/token", 400, json!({"error": "slow_down", "error_description": "slow"})),
                ("POST /console/auth/device/token", 200, json!({"access_token": "at", "refresh_token": "rt", "token_type": "Bearer", "expires_in": 3600})),
                ("GET /console/api/user", 200, json!({"id": "acc_1", "email": "me@test"})),
                ("GET /console/api/orgs", 200, json!([{"id": "org_1", "name": "One"}, {"id": "org_2", "name": "Two"}])),
            ],
            seen.clone(),
        )
        .await;
        let service = accounts();
        let login = service.login(&format!("{url}/console/?ignored=1")).await.unwrap();
        assert_eq!((login.code.as_str(), login.user.as_str()), ("dc", "ABCD"));
        assert_eq!(login.url, format!("{url}/device?code=ABCD"));
        assert_eq!((login.server.clone(), login.expiry, login.interval), (format!("{url}/console"), Duration::from_secs(600), Duration::from_secs(1)));
        assert_eq!(service.poll(&login).await.unwrap(), Poll::Pending);
        assert_eq!(service.poll(&login).await.unwrap(), Poll::Slow);
        assert_eq!(service.poll(&login).await.unwrap(), Poll::Success { email: "me@test".into() });

        let active = service.active().unwrap().unwrap();
        assert_eq!((active.id.as_str(), active.active_org_id.as_deref(), active.url), ("acc_1", Some("org_1"), format!("{url}/console")));
        let requests = seen.lock().unwrap().clone();
        assert!(requests[0].contains(r#"{"client_id":"opencode-cli"}"#), "{}", requests[0]);
        assert!(requests[1].contains(r#""grant_type":"urn:ietf:params:oauth:grant-type:device_code""#) && requests[1].contains(r#""device_code":"dc""#));
        assert!(requests.iter().filter(|r| r.contains("/api/")).all(|r| r.to_lowercase().contains("authorization: bearer at")));
    }

    #[tokio::test]
    async fn stale_tokens_refresh_and_config_sends_the_org() {
        let seen = Seen::default();
        let url = console(
            vec![
                ("POST /auth/device/token", 200, json!({"access_token": "fresh", "refresh_token": "rt2", "expires_in": 3600})),
                ("GET /api/config", 200, json!({"config": {"model": "remote/m"}})),
                ("GET /api/orgs", 200, json!([{"id": "org_9", "name": "Nine"}])),
            ],
            seen.clone(),
        )
        .await;
        let service = accounts();
        service
            .store
            .account_persist(&NewAccount { id: "a".into(), email: "a@test".into(), url: url.clone(), access_token: "old".into(), refresh_token: "rt".into(), expiry: 0, org_id: Some("org_9".into()) })
            .unwrap();
        let active = service.active_config().await.unwrap().unwrap();
        assert_eq!(active.config.unwrap()["model"], "remote/m");
        assert_eq!(active.token.as_deref(), Some("fresh"));
        let row = service.store.account_row("a").unwrap().unwrap();
        assert_eq!(row.refresh_token, "rt2");
        assert!(row.token_expiry.unwrap() > now() + 3000 * 1000);
        let requests = seen.lock().unwrap().clone();
        let config = requests.iter().find(|r| r.starts_with("GET /api/config")).unwrap().to_lowercase();
        assert!(config.contains("x-org-id: org_9") && config.contains("authorization: bearer fresh"), "{config}");
        assert!(requests.iter().any(|r| r.contains(r#""grant_type":"refresh_token""#) && r.contains(r#""refresh_token":"rt""#)));

        // A fresh token is used as stored; a 404 config is no config.
        assert_eq!(service.token("a").await.unwrap().as_deref(), Some("fresh"));
        let orgs = service.orgs_by_account().await.unwrap();
        assert_eq!(orgs[0].1, vec![Org { id: "org_9".into(), name: "Nine".into() }]);
    }

    #[tokio::test]
    async fn failures_map_to_the_original_errors() {
        let url = console(vec![("POST /auth/device/code", 500, json!({}))], Seen::default()).await;
        let service = accounts();
        let error = service.login(&url).await.unwrap_err();
        assert_eq!(error.to_string(), "HTTP request failed");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let error = service.login(&closed).await.unwrap_err();
        let text = error.to_string();
        assert!(text.starts_with(&format!("Could not reach POST {closed}/auth/device/code.\nThis failed before the server returned an HTTP response.\n")), "{text}");
        assert!(text.ends_with("Check your network, proxy, or VPN configuration and try again."), "{text}");
        assert!(service.active_config().await.unwrap().is_none());
    }
}
