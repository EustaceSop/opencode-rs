// ChatGPT-login (Codex OAuth) support for the `openai` provider.
use crate::{Error, oauth};
use base64::Engine;
use opencode_core::{auth, paths::Paths};
use regex::Regex;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const ISSUER: &str = "https://auth.openai.com";
pub const API_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
pub const PROVIDER_ID: &str = "openai";
const ALLOWED_MODELS: [&str; 6] = ["gpt-5.5", "gpt-5.3-codex-spark", "gpt-5.4", "gpt-5.4-mini", "gpt-6-sol", "gpt-6-luna"];
const DISALLOWED_MODELS: [&str; 2] = ["gpt-5.5-pro", "gpt-5.6"];
static GPT_VERSION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^gpt-(\d+)(?:\.(\d+))?").unwrap());
static REFRESH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub auth_file: PathBuf,
    pub issuer: String,
    pub endpoint: String,
}

impl Default for Config {
    fn default() -> Self {
        Config { auth_file: Paths::from_env().auth_file(), issuer: ISSUER.into(), endpoint: API_ENDPOINT.into() }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub access: String,
    pub account_id: Option<String>,
}

// Models a ChatGPT subscription may call.
pub fn allows_model(api_id: &str) -> bool {
    if ALLOWED_MODELS.contains(&api_id) {
        return true;
    }
    if DISALLOWED_MODELS.contains(&api_id) {
        return false;
    }
    let Some(version) = GPT_VERSION.captures(api_id) else { return false };
    let number = |i: usize| version.get(i).and_then(|m| m.as_str().parse::<u64>().ok()).unwrap_or(0);
    number(1) > 5 || (number(1) == 5 && number(2) > 4)
}

pub fn jwt_claims(token: &str) -> Option<Value> {
    let parts: Vec<&str> = token.split('.').collect();
    let [_, payload, _] = parts.as_slice() else { return None };
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn claim<'a>(claims: &'a Value, pointer: &str) -> Option<&'a str> {
    claims.pointer(pointer).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn account_from_claims(claims: &Value) -> Option<String> {
    claim(claims, "/chatgpt_account_id")
        .or_else(|| claim(claims, "/https:~1~1api.openai.com~1auth/chatgpt_account_id"))
        .or_else(|| claim(claims, "/organizations/0/id"))
        .map(str::to_string)
}

// The id token wins when it names an account; otherwise the access token decides.
pub fn account_id(id_token: Option<&str>, access_token: Option<&str>) -> Option<String> {
    if let Some(found) = id_token.filter(|t| !t.is_empty()).and_then(jwt_claims).and_then(|c| account_from_claims(&c)) {
        return Some(found);
    }
    access_token.filter(|t| !t.is_empty()).and_then(jwt_claims).and_then(|c| account_from_claims(&c))
}

pub fn residency(access_token: &str) -> Option<String> {
    let claims = jwt_claims(access_token)?;
    let value = claim(&claims, "/https:~1~1api.openai.com~1auth/chatgpt_compute_residency").or_else(|| claim(&claims, "/chatgpt_compute_residency"))?;
    (value != "no_constraint").then(|| value.to_string())
}

// Requests aimed at the public API are redirected to the ChatGPT backend; any query is dropped.
pub fn rewrite_url(url: &str, endpoint: &str) -> Option<String> {
    let path = reqwest::Url::parse(url).ok()?.path().to_string();
    (path.contains("/v1/responses") || path.contains("/chat/completions")).then(|| endpoint.to_string())
}

fn platform() -> &'static str {
    match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    }
}

fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    }
}

// Headers every request to the `openai` provider carries, whatever the credential type.
pub fn headers(session_id: Option<&str>) -> Vec<(String, String)> {
    let mut out = vec![
        ("originator".to_string(), "opencode".to_string()),
        ("User-Agent".to_string(), format!("opencode/{} ({} unknown; {})", env!("CARGO_PKG_VERSION"), platform(), arch())),
    ];
    if let Some(id) = session_id {
        out.push(("session-id".to_string(), id.to_string()));
    }
    out
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn stored(config: &Config) -> Result<(String, String, u64, Option<String>), Error> {
    match auth::read_all(&config.auth_file).into_iter().find(|(id, _)| id == PROVIDER_ID) {
        Some((_, auth::Info::Oauth { refresh, access, expires, account_id, .. })) => Ok((refresh, access, expires, account_id)),
        _ => Err(Error::Config("openai is not logged in with ChatGPT (no oauth entry in auth.json)".into())),
    }
}

// A usable access token; refreshes and persists it once expired. auth.json is re-read on every call.
pub async fn token(http: &reqwest::Client, config: &Config) -> Result<Token, Error> {
    let (_, access, expires, account) = stored(config)?;
    if !access.is_empty() && expires >= now_ms() {
        return Ok(Token { access, account_id: account });
    }
    let _guard = REFRESH.lock().await;
    // Another task may have refreshed while this one waited.
    let (refresh, access, expires, account) = stored(config)?;
    if !access.is_empty() && expires >= now_ms() {
        return Ok(Token { access, account_id: account });
    }
    let form = [("grant_type", "refresh_token"), ("refresh_token", refresh.as_str()), ("client_id", CLIENT_ID)];
    let body = reqwest::Url::parse_with_params("http://localhost/", form).map_err(|e| Error::Config(e.to_string()))?.query().unwrap_or_default().to_string();
    let response = http
        .post(format!("{}/oauth/token", config.issuer.trim_end_matches('/')))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        return Err(Error::Config(format!("Token refresh failed: {}", status.as_u16())));
    }
    let tokens: Value = serde_json::from_str(&response.text().await?).map_err(|e| Error::Config(format!("Token refresh failed: {e}")))?;
    let field = |key: &str| tokens.get(key).and_then(Value::as_str).filter(|s| !s.is_empty());
    let access = field("access_token").ok_or_else(|| Error::Config("Token refresh failed: no access_token".into()))?.to_string();
    let account_id = account_id(field("id_token"), Some(&access)).or(account);
    let info = auth::Info::Oauth {
        refresh: field("refresh_token").unwrap_or(&refresh).to_string(),
        access: access.clone(),
        expires: now_ms() + tokens.get("expires_in").and_then(Value::as_u64).unwrap_or(3600) * 1000,
        account_id: account_id.clone(),
        enterprise_url: None,
    };
    auth::set(&config.auth_file, PROVIDER_ID, info).map_err(|e| Error::Config(format!("failed to save refreshed OpenAI credentials: {e}")))?;
    Ok(Token { access, account_id })
}

pub const OAUTH_PORT: u16 = 1455;
pub const SCOPE: &str = "openid profile email offline_access";
pub const BROWSER_INSTRUCTIONS: &str = "Complete authorization in your browser. This window will close automatically.";

pub fn authorize_url(issuer: &str, redirect_uri: &str, challenge: &str, state: &str) -> String {
    let query = oauth::form(&[
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", redirect_uri),
        ("scope", SCOPE),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("state", state),
        ("originator", "opencode"),
    ]);
    format!("{}/oauth/authorize?{query}", issuer.trim_end_matches('/'))
}

fn login_entry(tokens: &Value) -> Result<auth::Info, Error> {
    let field = |key: &str| tokens.get(key).and_then(Value::as_str).filter(|s| !s.is_empty());
    let access = field("access_token").ok_or_else(|| Error::Config("Token exchange failed: no access_token".into()))?;
    Ok(auth::Info::Oauth {
        refresh: field("refresh_token").unwrap_or_default().to_string(),
        access: access.to_string(),
        expires: now_ms() + tokens.get("expires_in").and_then(Value::as_u64).unwrap_or(3600) * 1000,
        account_id: account_id(field("id_token"), Some(access)),
        enterprise_url: None,
    })
}

async fn exchange(http: &reqwest::Client, issuer: &str, code: &str, redirect_uri: &str, verifier: &str) -> Result<auth::Info, Error> {
    let form = oauth::form(&[("grant_type", "authorization_code"), ("code", code), ("redirect_uri", redirect_uri), ("client_id", CLIENT_ID), ("code_verifier", verifier)]);
    let response = http.post(format!("{}/oauth/token", issuer.trim_end_matches('/'))).header("Content-Type", "application/x-www-form-urlencoded").body(form).send().await?;
    let status = response.status();
    if !status.is_success() {
        return Err(Error::Config(format!("Token exchange failed: {}", status.as_u16())));
    }
    login_entry(&serde_json::from_str(&response.text().await?).unwrap_or(Value::Null))
}

// A pending "ChatGPT Pro/Plus (browser)" login; the callback listener lives as long as this value.
pub struct Login {
    pub url: String,
    pub instructions: &'static str,
    pub redirect_uri: String,
    issuer: String,
    state: String,
    verifier: String,
    listener: oauth::Listener,
}

// Starts the PKCE flow with the callback server on port 1455.
pub async fn login_start(issuer: &str) -> Result<Login, Error> {
    login_start_on(issuer, OAUTH_PORT).await
}

pub(crate) async fn login_start_on(issuer: &str, port: u16) -> Result<Login, Error> {
    let listener = oauth::Listener::bind("127.0.0.1", port).await?;
    let redirect_uri = format!("http://localhost:{}/auth/callback", listener.port);
    let codes = oauth::pkce(43);
    let state = oauth::base64url(&oauth::random_bytes(32));
    Ok(Login { url: authorize_url(issuer, &redirect_uri, &codes.challenge, &state), instructions: BROWSER_INSTRUCTIONS, redirect_uri, issuer: issuer.to_string(), state, verifier: codes.verifier, listener })
}

impl Login {
    // Waits for the redirect (five minutes at most) and exchanges the code; store the entry under `openai`.
    pub async fn wait(self, http: &reqwest::Client) -> Result<auth::Info, Error> {
        let state = self.state.clone();
        let redirected = self.listener.serve(|request| {
            let fail = |status: u16, message: &str| (oauth::Reply::html(status, "Authorization failed", message), Some(Err(Error::Config(message.to_string()))));
            match request.path.as_str() {
                "/auth/callback" => {
                    if let Some(error) = request.param("error") {
                        return fail(200, request.param("error_description").unwrap_or(error));
                    }
                    let Some(code) = request.param("code") else { return fail(400, "Missing authorization code") };
                    if request.param("state") != Some(state.as_str()) {
                        return fail(400, "Invalid state - potential CSRF attack");
                    }
                    (oauth::Reply::html(200, "Authorization successful", "You can close this window and return to OpenCode."), Some(Ok(code.to_string())))
                }
                "/cancel" => (oauth::Reply::text(200, "Login cancelled"), Some(Err(Error::Config("Login cancelled".into())))),
                _ => (oauth::Reply::text(404, "Not found"), None),
            }
        });
        let code = tokio::time::timeout(oauth::CALLBACK_TIMEOUT, redirected).await.map_err(|_| Error::Config("OAuth callback timeout - authorization took too long".into()))???;
        exchange(http, &self.issuer, &code, &self.redirect_uri, &self.verifier).await
    }
}

// A pending "ChatGPT Pro/Plus (headless)" login.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceLogin {
    pub url: String,
    pub user_code: String,
    pub device_auth_id: String,
    pub interval_ms: u64,
    pub issuer: String,
}

impl DeviceLogin {
    pub fn instructions(&self) -> String {
        format!("Enter code: {}", self.user_code)
    }
}

fn device_post(http: &reqwest::Client, url: String, body: Value) -> reqwest::RequestBuilder {
    http.post(url).header("Content-Type", "application/json").header("User-Agent", crate::shape::user_agent()).body(body.to_string())
}

pub async fn device_start(http: &reqwest::Client, issuer: &str) -> Result<DeviceLogin, Error> {
    let issuer = issuer.trim_end_matches('/');
    let response = device_post(http, format!("{issuer}/api/accounts/deviceauth/usercode"), serde_json::json!({"client_id": CLIENT_ID})).send().await?;
    if !response.status().is_success() {
        return Err(Error::Config("Failed to initiate device authorization".into()));
    }
    let data: Value = serde_json::from_str(&response.text().await?).unwrap_or(Value::Null);
    let field = |key: &str| data.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
    // The interval arrives as a string of seconds.
    let seconds = data.get("interval").and_then(|v| v.as_str().and_then(|s| s.trim().parse::<u64>().ok()).or(v.as_u64())).filter(|s| *s > 0).unwrap_or(5);
    Ok(DeviceLogin { url: format!("{issuer}/codex/device"), user_code: field("user_code"), device_auth_id: field("device_auth_id"), interval_ms: seconds.max(1) * 1000, issuer: issuer.to_string() })
}

// One poll; 403 and 404 mean the code has not been entered yet. Store a `Success` entry under `openai`.
pub async fn device_poll(http: &reqwest::Client, login: &DeviceLogin) -> Result<oauth::Poll, Error> {
    let body = serde_json::json!({"device_auth_id": login.device_auth_id, "user_code": login.user_code});
    let response = device_post(http, format!("{}/api/accounts/deviceauth/token", login.issuer), body).send().await?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Ok(if status == 403 || status == 404 { oauth::Poll::Pending { wait_ms: login.interval_ms + oauth::POLL_MARGIN_MS } } else { oauth::Poll::Failed });
    }
    let data: Value = serde_json::from_str(&response.text().await?).unwrap_or(Value::Null);
    let field = |key: &str| data.get(key).and_then(Value::as_str).unwrap_or_default();
    let redirect = format!("{}/deviceauth/callback", login.issuer);
    exchange(http, &login.issuer, field("authorization_code"), &redirect, field("code_verifier")).await.map(oauth::Poll::Success)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub fn jwt(claims: &Value) -> String {
        let encode = |v: &Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string());
        format!("{}.{}.sig", encode(&json!({"alg": "none"})), encode(claims))
    }

    #[test]
    fn model_allow_list() {
        for id in ["gpt-5.5", "gpt-5.4", "gpt-5.4-mini", "gpt-5.3-codex-spark", "gpt-6-sol", "gpt-5.6-luna", "gpt-6.1-sol", "gpt-5.5-codex"] {
            assert!(allows_model(id), "{id}");
        }
        for id in ["gpt-5.5-pro", "gpt-5.6", "gpt-5.4-nano", "gpt-5.3-codex", "gpt-5", "gpt-4o", "o3", "chatgpt-image-latest"] {
            assert!(!allows_model(id), "{id}");
        }
    }

    #[test]
    fn claims_account_and_residency() {
        let nested = jwt(&json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acct_nested", "chatgpt_compute_residency": "eu"}}));
        let org = jwt(&json!({"organizations": [{"id": "org_1"}], "chatgpt_compute_residency": "no_constraint"}));
        assert_eq!(account_id(Some(&jwt(&json!({"chatgpt_account_id": "acct_top"}))), Some(&nested)).as_deref(), Some("acct_top"));
        assert_eq!(account_id(Some(&jwt(&json!({"email": "x"}))), Some(&nested)).as_deref(), Some("acct_nested"));
        assert_eq!(account_id(None, Some(&org)).as_deref(), Some("org_1"));
        assert_eq!(account_id(Some("not-a-jwt"), None), None);
        assert_eq!(residency(&nested).as_deref(), Some("eu"));
        assert_eq!(residency(&org), None);
    }

    #[test]
    fn url_rewrite() {
        assert_eq!(rewrite_url("https://api.openai.com/v1/responses?x=1", API_ENDPOINT).as_deref(), Some(API_ENDPOINT));
        assert_eq!(rewrite_url("https://proxy.example/openai/chat/completions", API_ENDPOINT).as_deref(), Some(API_ENDPOINT));
        assert_eq!(rewrite_url("https://api.openai.com/v1/models", API_ENDPOINT), None);
    }
}
