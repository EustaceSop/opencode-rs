// xAI SuperGrok login: OAuth device flow and access-token refresh for the `xai` provider.
use crate::oauth::{self, Poll, now_ms};
use crate::{Error, shape};
use opencode_core::{auth, paths::Paths};
use serde_json::Value;
use std::path::PathBuf;

pub const PROVIDER_ID: &str = "xai";
pub const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
pub const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
pub const DEVICE_URL: &str = "https://auth.x.ai/oauth2/device/code";
pub const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
pub const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
pub const REFRESH_SKEW_MS: u64 = 120_000;
const DEFAULT_INTERVAL_MS: u64 = 5000;
const MIN_INTERVAL_MS: u64 = 1000;
const SLOW_DOWN_MS: u64 = 5000;
const DEFAULT_EXPIRES_MS: u64 = 5 * 60 * 1000;

static REFRESH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub auth_file: PathBuf,
    pub token_url: String,
}

impl Default for Config {
    fn default() -> Self {
        Config { auth_file: Paths::from_env().auth_file(), token_url: TOKEN_URL.into() }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    // Grows by five seconds on every `slow_down`.
    pub interval_ms: u64,
    pub deadline_ms: u64,
    pub token_url: String,
}

impl Device {
    // The page to open in a browser.
    pub fn url(&self) -> &str {
        self.verification_uri_complete.as_deref().unwrap_or(&self.verification_uri)
    }

    pub fn instructions(&self) -> String {
        format!("Open {} on any device and enter code: {}", self.verification_uri, self.user_code)
    }
}

fn post(http: &reqwest::Client, url: &str, pairs: &[(&str, &str)]) -> reqwest::RequestBuilder {
    http.post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .header("User-Agent", shape::user_agent())
        .body(oauth::form(pairs))
}

fn seconds_to_ms(value: Option<&Value>, default: u64) -> u64 {
    value.and_then(Value::as_f64).filter(|s| s.is_finite() && *s > 0.0).map_or(default, |s| (s * 1000.0) as u64)
}

fn detail(status: u16, body: &str, what: &str) -> Error {
    Error::Config(format!("xAI {what} failed ({status}){}", if body.is_empty() { String::new() } else { format!(": {body}") }))
}

pub async fn device_start_at(http: &reqwest::Client, device_url: &str, token_url: &str) -> Result<Device, Error> {
    let response = post(http, device_url, &[("client_id", CLIENT_ID), ("scope", SCOPE), ("referrer", "opencode")]).send().await?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(detail(status.as_u16(), &body, "device code request"));
    }
    let data: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let field = |key: &str| data.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    let (Some(device_code), Some(user_code), Some(verification_uri)) = (field("device_code"), field("user_code"), field("verification_uri")) else {
        return Err(Error::Config("xAI device code response is missing device_code / user_code / verification_uri".into()));
    };
    Ok(Device {
        device_code,
        user_code,
        verification_uri,
        verification_uri_complete: field("verification_uri_complete"),
        interval_ms: seconds_to_ms(data.get("interval"), DEFAULT_INTERVAL_MS).max(MIN_INTERVAL_MS),
        deadline_ms: now_ms() + seconds_to_ms(data.get("expires_in"), DEFAULT_EXPIRES_MS),
        token_url: token_url.to_string(),
    })
}

pub async fn device_start(http: &reqwest::Client) -> Result<Device, Error> {
    device_start_at(http, DEVICE_URL, TOKEN_URL).await
}

fn entry(tokens: &Value, old_refresh: Option<&str>) -> Option<auth::Info> {
    let field = |key: &str| tokens.get(key).and_then(Value::as_str).filter(|s| !s.is_empty());
    Some(auth::Info::Oauth {
        access: field("access_token")?.to_string(),
        refresh: field("refresh_token").or(old_refresh)?.to_string(),
        expires: now_ms() + tokens.get("expires_in").and_then(Value::as_u64).unwrap_or(3600) * 1000,
        account_id: None,
        enterprise_url: None,
    })
}

// One poll of the token endpoint; store a `Success` entry under `xai`.
pub async fn device_poll(http: &reqwest::Client, device: &mut Device) -> Result<Poll, Error> {
    let now = now_ms();
    if now >= device.deadline_ms {
        return Ok(Poll::Failed);
    }
    let response = post(http, &device.token_url, &[("grant_type", DEVICE_GRANT), ("client_id", CLIENT_ID), ("device_code", &device.device_code)]).send().await?;
    let ok = response.status().is_success();
    let data: Value = serde_json::from_str(&response.text().await.unwrap_or_default()).unwrap_or(Value::Null);
    if ok {
        return Ok(entry(&data, None).map_or(Poll::Failed, Poll::Success));
    }
    let remaining = device.deadline_ms.saturating_sub(now_ms());
    Ok(match data.get("error").and_then(Value::as_str) {
        Some("authorization_pending") => Poll::Pending { wait_ms: (device.interval_ms + oauth::POLL_MARGIN_MS).min(remaining) },
        Some("slow_down") => {
            device.interval_ms += SLOW_DOWN_MS;
            Poll::Pending { wait_ms: (device.interval_ms + oauth::POLL_MARGIN_MS).min(remaining) }
        }
        _ => Poll::Failed,
    })
}

fn stored(config: &Config) -> Result<(String, String, u64), Error> {
    match auth::read_all(&config.auth_file).into_iter().find(|(id, _)| id == PROVIDER_ID) {
        Some((_, auth::Info::Oauth { refresh, access, expires, .. })) => Ok((refresh, access, expires)),
        _ => Err(Error::Config("xai is not logged in with OAuth (no oauth entry in auth.json)".into())),
    }
}

fn expiring(access: &str, expires: u64) -> bool {
    expires == 0 || expires.saturating_sub(now_ms()) <= REFRESH_SKEW_MS || oauth::jwt_expiring(access, REFRESH_SKEW_MS)
}

// A usable access token; refreshed and persisted when it expires within two minutes. auth.json is re-read on every call.
pub async fn token(http: &reqwest::Client, config: &Config) -> Result<String, Error> {
    let (_, access, expires) = stored(config)?;
    if !expiring(&access, expires) {
        return Ok(access);
    }
    let _guard = REFRESH.lock().await;
    let (refresh, access, expires) = stored(config)?;
    if !expiring(&access, expires) {
        return Ok(access);
    }
    let response = post(http, &config.token_url, &[("grant_type", "refresh_token"), ("refresh_token", &refresh), ("client_id", CLIENT_ID)]).send().await?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(detail(status.as_u16(), &body, "token refresh"));
    }
    let tokens: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let Some(info) = entry(&tokens, Some(&refresh)) else {
        return Err(Error::Config("xAI token refresh failed: no access_token".into()));
    };
    let auth::Info::Oauth { access, .. } = &info else { unreachable!() };
    let access = access.clone();
    // A failed write is not fatal: the fresh token still serves this request.
    let _ = auth::set(&config.auth_file, PROVIDER_ID, info);
    Ok(access)
}
