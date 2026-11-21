// Snowflake Cortex: external-browser OAuth (PKCE on a loopback port), token refresh, and the request/response fixes.
use crate::oauth::{self, Listener, Reply, now_ms};
use crate::{Error, shape};
use opencode_core::{auth, paths::Paths};
use regex::Regex;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::LazyLock;

pub const PROVIDER_ID: &str = "snowflake-cortex";
pub const CLIENT_ID: &str = "LOCAL_APPLICATION";
pub const CALLBACK_HOST: &str = "127.0.0.1";
pub const REFRESH_SKEW_MS: u64 = 120_000;
pub const LOGIN_INSTRUCTIONS: &str = "Complete Snowflake sign-in in your browser. OpenCode will capture the OAuth callback and store the bearer token automatically.";
static EMPTY_ROLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""role"\s*:\s*"""#).unwrap());
static PLAIN_ROLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[-_A-Za-z0-9]+$").unwrap());
static REFRESH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub fn normalize_account(input: &str) -> String {
    let rest = input.trim();
    let rest = rest.strip_prefix("https://").or_else(|| rest.strip_prefix("http://")).unwrap_or(rest);
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    rest.strip_suffix(".snowflakecomputing.com").unwrap_or(rest).trim_end_matches('/').to_string()
}

pub fn origin(account: &str) -> String {
    format!("https://{account}.snowflakecomputing.com")
}

pub fn base_url(account: &str) -> String {
    format!("{}/api/v2/cortex/v1", origin(account))
}

pub fn scope(role: Option<&str>) -> String {
    match role.filter(|r| !r.is_empty()) {
        None => "refresh_token".into(),
        Some(role) if PLAIN_ROLE.is_match(role) => format!("refresh_token session:role:{role}"),
        Some(role) => format!("refresh_token session:role-encoded:{}", crate::bedrock::encode_component(role)),
    }
}

fn basic() -> String {
    use base64::Engine;
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{CLIENT_ID}:{CLIENT_ID}")))
}

async fn token_request(http: &reqwest::Client, origin: &str, pairs: &[(&str, &str)], what: &str) -> Result<Value, Error> {
    let response = http
        .post(format!("{origin}/oauth/token-request"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .header("User-Agent", shape::user_agent())
        .header("Authorization", basic())
        .body(oauth::form(pairs))
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(Error::Config(format!("Snowflake token {what} failed ({}){}", status.as_u16(), if body.is_empty() { String::new() } else { format!(": {body}") })));
    }
    let data: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    if data.get("access_token").and_then(Value::as_str).is_none_or(str::is_empty) {
        return Err(Error::Config(format!("Snowflake {what} response did not include access_token")));
    }
    Ok(data)
}

fn entry(tokens: &Value, old_refresh: Option<&str>, account: &str) -> Option<auth::Info> {
    let field = |key: &str| tokens.get(key).and_then(Value::as_str).filter(|s| !s.is_empty());
    Some(auth::Info::Oauth {
        access: field("access_token")?.to_string(),
        refresh: field("refresh_token").or(old_refresh)?.to_string(),
        expires: now_ms() + tokens.get("expires_in").and_then(Value::as_u64).unwrap_or(600) * 1000,
        account_id: Some(account.to_string()),
        enterprise_url: None,
    })
}

// A pending browser login; the callback listener lives as long as this value.
pub struct Login {
    pub url: String,
    pub instructions: &'static str,
    pub redirect_uri: String,
    account: String,
    origin: String,
    state: String,
    verifier: String,
    listener: Listener,
}

pub async fn authorize(account: &str, role: Option<&str>) -> Result<Login, Error> {
    let account = normalize_account(account);
    if account.is_empty() {
        return Err(Error::Config("Snowflake account is required".into()));
    }
    let origin = origin(&account);
    authorize_at(&account, role, &origin).await
}

pub(crate) async fn authorize_at(account: &str, role: Option<&str>, origin: &str) -> Result<Login, Error> {
    let listener = Listener::bind(CALLBACK_HOST, 0).await?;
    let redirect_uri = format!("http://{CALLBACK_HOST}:{}/", listener.port);
    let codes = oauth::pkce(64);
    let state = oauth::random_string(64);
    let role = role.map(str::trim).filter(|r| !r.is_empty());
    let query = oauth::form(&[
        ("client_id", CLIENT_ID),
        ("response_type", "code"),
        ("redirect_uri", &redirect_uri),
        ("scope", &scope(role)),
        ("state", &state),
        ("code_challenge", &codes.challenge),
        ("code_challenge_method", "S256"),
    ]);
    Ok(Login { url: format!("{origin}/oauth/authorize?{query}"), instructions: LOGIN_INSTRUCTIONS, redirect_uri, account: account.to_string(), origin: origin.to_string(), state, verifier: codes.verifier, listener })
}

impl Login {
    // Waits for the redirect (five minutes at most) and exchanges the code; store the entry under `snowflake-cortex`.
    pub async fn wait(self, http: &reqwest::Client) -> Result<auth::Info, Error> {
        let state = self.state.clone();
        let received = self.listener.serve(|request| {
            if request.path != "/" {
                return (Reply::text(404, "Not found"), None);
            }
            let fail = |status: u16, message: &str| (Reply::html(status, "Authorization failed", message), Some(Err(Error::Config(message.to_string()))));
            if request.param("state") != Some(state.as_str()) {
                return fail(400, "Invalid state - potential CSRF attack");
            }
            if let Some(error) = request.param("error") {
                return fail(200, request.param("error_description").unwrap_or(error));
            }
            match request.param("code") {
                Some(code) => (Reply::html(200, "Authorization successful", "You can close this window and return to OpenCode."), Some(Ok(code.to_string()))),
                None => fail(400, "Missing authorization code"),
            }
        });
        let code = tokio::time::timeout(oauth::CALLBACK_TIMEOUT, received).await.map_err(|_| Error::Config("Snowflake OAuth callback timeout - authorization took too long".into()))???;
        let pairs = [("grant_type", "authorization_code"), ("code", code.as_str()), ("redirect_uri", self.redirect_uri.as_str()), ("client_id", CLIENT_ID), ("code_verifier", self.verifier.as_str())];
        let tokens = token_request(http, &self.origin, &pairs, "exchange").await?;
        if tokens.get("refresh_token").and_then(Value::as_str).is_none_or(str::is_empty) {
            return Err(Error::Config("Snowflake token response did not include refresh_token. Ensure integration issues refresh tokens and scope includes refresh_token.".into()));
        }
        entry(&tokens, None, &self.account).ok_or_else(|| Error::Config("Snowflake token response did not include access_token".into()))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub auth_file: PathBuf,
    // Overrides `https://<account>.snowflakecomputing.com` for the token endpoint.
    pub origin: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config { auth_file: Paths::from_env().auth_file(), origin: None }
    }
}

// A usable access token; refreshed when it expires within two minutes, or always with `force` (after a 401).
pub async fn token(http: &reqwest::Client, config: &Config, force: bool) -> Result<String, Error> {
    let stored = || match auth::read_all(&config.auth_file).into_iter().find(|(id, _)| id == PROVIDER_ID) {
        Some((_, auth::Info::Oauth { refresh, access, expires, account_id: Some(account), .. })) => Ok((refresh, access, expires, account)),
        Some((_, auth::Info::Oauth { .. })) => Err(Error::Config("Snowflake OAuth auth is missing accountId".into())),
        _ => Err(Error::Config("snowflake-cortex is not logged in with OAuth (no oauth entry in auth.json)".into())),
    };
    let fresh = |access: &str, expires: u64| !access.is_empty() && expires != 0 && expires.saturating_sub(now_ms()) > REFRESH_SKEW_MS;
    let (_, access, expires, _) = stored()?;
    if !force && fresh(&access, expires) {
        return Ok(access);
    }
    let _guard = REFRESH.lock().await;
    let (refresh, current, expires, account) = stored()?;
    if fresh(&current, expires) && (!force || current != access) {
        return Ok(current);
    }
    let origin = config.origin.clone().unwrap_or_else(|| origin(&account));
    let tokens = token_request(http, &origin, &[("grant_type", "refresh_token"), ("refresh_token", &refresh), ("client_id", CLIENT_ID)], "refresh").await?;
    let info = entry(&tokens, Some(&refresh), &account).ok_or_else(|| Error::Config("Snowflake refresh response did not include access_token".into()))?;
    let auth::Info::Oauth { access, .. } = &info else { unreachable!() };
    let access = access.clone();
    let _ = auth::set(&config.auth_file, PROVIDER_ID, info);
    Ok(access)
}

// Cortex takes the output cap as `max_completion_tokens`.
pub fn rewrite_body(body: &mut Value) {
    if let Some(map) = body.as_object_mut()
        && let Some(max) = map.remove("max_tokens")
    {
        map.insert("max_completion_tokens".into(), max);
    }
}

// A 400 saying the conversation is complete is an empty, successful turn.
pub fn conversation_complete(status: u16, body: &str) -> bool {
    if status != 400 {
        return false;
    }
    let data: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let text = |key: &str| data.get(key).and_then(Value::as_str).filter(|s| !s.is_empty());
    text("message").or_else(|| text("error")).is_some_and(|m| m.to_lowercase().contains("conversation complete"))
}

// Cortex streams deltas with an empty role.
pub fn fix_roles(chunk: &str) -> String {
    EMPTY_ROLE.replace_all(chunk, "\"role\":\"assistant\"").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accounts_scopes_and_fixes() {
        assert_eq!(normalize_account(" https://myorg-acct.snowflakecomputing.com/ "), "myorg-acct");
        assert_eq!(normalize_account("myorg-acct//"), "myorg-acct");
        assert_eq!(base_url("a"), "https://a.snowflakecomputing.com/api/v2/cortex/v1");
        assert_eq!((scope(None), scope(Some("PUBLIC")), scope(Some("my role"))), ("refresh_token".to_string(), "refresh_token session:role:PUBLIC".to_string(), "refresh_token session:role-encoded:my%20role".to_string()));
        assert_eq!(basic(), "Basic TE9DQUxfQVBQTElDQVRJT046TE9DQUxfQVBQTElDQVRJT04=");
        let mut body = json!({"model": "m", "max_tokens": 5});
        rewrite_body(&mut body);
        assert_eq!(body, json!({"model": "m", "max_completion_tokens": 5}));
        assert!(conversation_complete(400, r#"{"message":"Conversation Complete."}"#) && conversation_complete(400, r#"{"error":"conversation complete"}"#));
        assert!(!conversation_complete(500, r#"{"message":"conversation complete"}"#) && !conversation_complete(400, "nope"));
        assert_eq!(fix_roles(r#"{"delta":{"role" : "","content":"x"}}"#), r#"{"delta":{"role":"assistant","content":"x"}}"#);
    }
}
