// Azure OpenAI / Foundry: endpoint layout, resource lookup and Microsoft Entra ID tokens through the Azure CLI.
use crate::Error;
use crate::oauth::now_ms;
use opencode_core::auth;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;

pub const PROVIDER_ID: &str = "azure";
pub const NPM: &str = "@ai-sdk/azure";
pub const COGNITIVE_SERVICES_SCOPE: &str = "https://cognitiveservices.azure.com/.default";
pub const FOUNDRY_SCOPE: &str = "https://ai.azure.com/.default";
pub const API_KEY_HEADER: &str = "api-key";
pub const API_KEY_ENV: &str = "AZURE_API_KEY";
pub const RESOURCE_ENV: &str = "AZURE_RESOURCE_NAME";
pub const DEFAULT_API_VERSION: &str = "v1";
pub const LOGIN_INSTRUCTIONS: &str = "Sign in with `az login` before continuing.";
const REFRESH_BUFFER_MS: u64 = 60_000;
const LOGIN_LIFETIME_MS: u64 = 365 * 24 * 60 * 60 * 1000;

static TOKENS: Mutex<Option<HashMap<String, (String, u64)>>> = Mutex::new(None);

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    // The `az` executable.
    pub program: String,
}

impl Default for Config {
    fn default() -> Self {
        Config { program: "az".into() }
    }
}

// Foundry hosts take their own audience, except on the `/models` routes.
pub fn scope_for(url: &str) -> &'static str {
    match reqwest::Url::parse(url) {
        Ok(url) if url.host_str().is_some_and(|h| h.ends_with(".services.ai.azure.com")) && !url.path().starts_with("/models") => FOUNDRY_SCOPE,
        _ => COGNITIVE_SERVICES_SCOPE,
    }
}

pub fn base_url(resource: &str) -> String {
    format!("https://{resource}.openai.azure.com/openai")
}

pub fn cognitive_services_base_url(resource: &str, deployment_based: bool) -> String {
    format!("https://{resource}.cognitiveservices.azure.com/openai{}", if deployment_based { "" } else { "/v1" })
}

// The request URL for `path` ("/responses", "/chat/completions") under a base URL prefix.
pub fn url(prefix: &str, configured: bool, deployment_based: bool, model: &str, api_version: &str, path: &str) -> String {
    let prefix = prefix.trim_end_matches('/');
    let azure_host = !configured || reqwest::Url::parse(prefix).ok().and_then(|u| u.host_str().map(|h| h.ends_with(".openai.azure.com"))).unwrap_or(false);
    let full = if deployment_based {
        format!("{prefix}/deployments/{model}{path}")
    } else if azure_host {
        format!("{prefix}/v1{path}")
    } else {
        format!("{prefix}{path}")
    };
    if !(azure_host || deployment_based) {
        return full;
    }
    match reqwest::Url::parse(&full) {
        Ok(mut url) => {
            let others: Vec<(String, String)> = url.query_pairs().filter(|(k, _)| k != "api-version").map(|(k, v)| (k.to_string(), v.to_string())).collect();
            url.query_pairs_mut().clear().extend_pairs(others).append_pair("api-version", api_version);
            url.to_string()
        }
        Err(_) => full,
    }
}

// First non-blank of: configured option, api-key metadata, the Entra login's account id, the environment.
pub fn resource_name(options: &Value, stored: Option<&auth::Info>, env: Option<String>) -> Option<String> {
    let clean = |value: Option<&str>| value.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let from_auth = match stored {
        Some(auth::Info::Api { metadata, .. }) => clean(metadata.as_ref().and_then(|m| m.get("resourceName")).and_then(Value::as_str)),
        Some(auth::Info::Oauth { account_id, .. }) => clean(account_id.as_deref()),
        _ => None,
    };
    clean(options.get("resourceName").and_then(Value::as_str)).or(from_auth).or_else(|| clean(env.as_deref()))
}

pub fn cli_available(config: &Config) -> bool {
    let finder = if cfg!(windows) { "where" } else { "which" };
    std::process::Command::new(finder).arg(&config.program).output().is_ok_and(|o| o.status.success())
}

// (token, expiry in ms) from `az account get-access-token --output json`.
pub fn parse_cli_token(stdout: &str) -> Result<(String, u64), Error> {
    let invalid = || Error::Config("Azure CLI returned an invalid token".into());
    let data: Value = serde_json::from_str(stdout).map_err(|_| invalid())?;
    let token = data.get("accessToken").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(invalid)?;
    let expires = match data.get("expires_on").and_then(Value::as_f64) {
        Some(seconds) => Some((seconds * 1000.0) as u64),
        None => data.get("expiresOn").and_then(Value::as_str).and_then(|s| httpdate::parse_http_date(s).ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64),
    };
    Ok((token.to_string(), expires.ok_or_else(|| Error::Config("Azure CLI returned an invalid token expiration".into()))?))
}

// A bearer token for `scope`, cached until a minute before it expires.
pub async fn token(config: &Config, scope: &str) -> Result<String, Error> {
    if let Some((token, expires)) = TOKENS.lock().unwrap().as_ref().and_then(|m| m.get(scope)).cloned()
        && expires.saturating_sub(now_ms()) > REFRESH_BUFFER_MS
    {
        return Ok(token);
    }
    let mut command = if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd");
        c.args(["/C", &config.program]);
        c
    } else {
        tokio::process::Command::new(&config.program)
    };
    let output = command.args(["account", "get-access-token", "--scope", scope, "--output", "json"]).output().await.map_err(|e| Error::Config(format!("failed to run the Azure CLI: {e}")))?;
    if !output.status.success() {
        return Err(Error::Config(format!("Azure CLI failed: {}", String::from_utf8_lossy(&output.stderr).trim())));
    }
    let (token, expires) = parse_cli_token(&String::from_utf8_lossy(&output.stdout))?;
    TOKENS.lock().unwrap().get_or_insert_with(HashMap::new).insert(scope.to_string(), (token.clone(), expires));
    Ok(token)
}

// The entry stored after an Entra ID login: the CLI keeps the real credential, auth.json only records the resource.
pub fn login_entry(resource_name: &str) -> auth::Info {
    auth::Info::Oauth {
        refresh: auth::OAUTH_DUMMY_KEY.into(),
        access: auth::OAUTH_DUMMY_KEY.into(),
        expires: now_ms() + LOGIN_LIFETIME_MS,
        account_id: Some(resource_name.to_string()),
        enterprise_url: None,
    }
}

// "Microsoft Entra ID (Azure CLI)": checks that the CLI can mint a token, then returns the entry to store under `azure`.
pub async fn login(config: &Config, resource_name: Option<&str>) -> Result<auth::Info, Error> {
    let env = std::env::var(RESOURCE_ENV).ok();
    let Some(resource) = resource_name.map(str::to_string).or(env).filter(|s| !s.trim().is_empty()) else {
        return Err(Error::Config("Azure Resource Name is required".into()));
    };
    token(config, COGNITIVE_SERVICES_SCOPE).await?;
    Ok(login_entry(&resource))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn urls_scopes_and_cli_output() {
        assert_eq!(url(&base_url("res"), false, false, "gpt-5", "v1", "/responses"), "https://res.openai.azure.com/openai/v1/responses?api-version=v1");
        assert_eq!(url(&base_url("res"), false, true, "dep", "2025-01-01", "/chat/completions"), "https://res.openai.azure.com/openai/deployments/dep/chat/completions?api-version=2025-01-01");
        assert_eq!(url("https://gw.example/openai/", true, false, "m", "v1", "/responses"), "https://gw.example/openai/responses");
        assert_eq!(url("https://x.openai.azure.com/openai", true, false, "m", "v1", "/responses"), "https://x.openai.azure.com/openai/v1/responses?api-version=v1");
        assert_eq!(cognitive_services_base_url("r", false), "https://r.cognitiveservices.azure.com/openai/v1");
        assert_eq!(scope_for("https://r.services.ai.azure.com/anthropic/v1/messages"), FOUNDRY_SCOPE);
        assert_eq!(scope_for("https://r.services.ai.azure.com/models/chat/completions"), COGNITIVE_SERVICES_SCOPE);
        assert_eq!(scope_for("https://r.openai.azure.com/openai/v1/responses"), COGNITIVE_SERVICES_SCOPE);

        assert_eq!(parse_cli_token(r#"{"accessToken":"tok","expires_on":1700000000,"expiresOn":"x"}"#).unwrap(), ("tok".into(), 1_700_000_000_000));
        assert!(parse_cli_token(r#"{"accessToken":"tok"}"#).is_err() && parse_cli_token(r#"{"accessToken":"","expires_on":1}"#).is_err() && parse_cli_token("nope").is_err());

        let api = auth::Info::Api { key: "k".into(), metadata: Some(json!({"resourceName": "from-key"}).as_object().unwrap().clone()) };
        assert_eq!(resource_name(&json!({"resourceName": " "}), Some(&api), Some("env".into())).as_deref(), Some("from-key"));
        assert_eq!(resource_name(&json!({"resourceName": "opt"}), Some(&api), None).as_deref(), Some("opt"));
        let entry = login_entry("entra");
        assert_eq!(resource_name(&json!({}), Some(&entry), Some("env".into())).as_deref(), Some("entra"));
        assert_eq!(resource_name(&json!({}), None, Some("env".into())).as_deref(), Some("env"));
        assert!(matches!(entry, auth::Info::Oauth { ref access, ref refresh, .. } if access == auth::OAUTH_DUMMY_KEY && refresh == auth::OAUTH_DUMMY_KEY));
    }
}
