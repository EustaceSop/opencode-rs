// Google Vertex AI: endpoint layout, project/location lookup and Application Default Credentials.
use crate::oauth::{self, now_ms};
use crate::{Error, bedrock::encode_component};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

pub const NPM: &str = "@ai-sdk/google-vertex";
pub const ANTHROPIC_NPM: &str = "@ai-sdk/google-vertex/anthropic";
pub const API_KEY_ENV: &str = "GOOGLE_VERTEX_API_KEY";
pub const EXPRESS_BASE_URL: &str = "https://aiplatform.googleapis.com/v1/publishers/google";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const METADATA_TOKEN_URL: &str = "http://169.254.169.254/computeMetadata/v1/instance/service-accounts/default/token";
pub const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
pub const ANTHROPIC_VERSION: &str = "vertex-2023-10-16";
const PROJECT_ENV: [&str; 4] = ["GOOGLE_VERTEX_PROJECT", "GOOGLE_CLOUD_PROJECT", "GCP_PROJECT", "GCLOUD_PROJECT"];
const LOCATION_ENV: [&str; 3] = ["GOOGLE_VERTEX_LOCATION", "GOOGLE_CLOUD_LOCATION", "VERTEX_LOCATION"];
const REFRESH_BUFFER_MS: u64 = 60_000;

static TOKEN: Mutex<Option<(String, String, u64)>> = Mutex::new(None);

pub fn host(location: &str) -> String {
    match location {
        "global" => "aiplatform.googleapis.com".into(),
        "eu" | "us" => format!("aiplatform.{location}.rep.googleapis.com"),
        _ => format!("{location}-aiplatform.googleapis.com"),
    }
}

pub fn base_url(project: &str, location: &str) -> String {
    format!("https://{}/v1beta1/projects/{project}/locations/{location}/publishers/google", host(location))
}

pub fn anthropic_base_url(project: &str, location: &str) -> String {
    format!("https://{}/v1/projects/{project}/locations/{location}/publishers/anthropic/models", host(location))
}

pub fn anthropic_url(base: &str, model: &str) -> String {
    format!("{}/{model}:streamRawPredict", base.trim_end_matches('/'))
}

fn first(var: &dyn Fn(&str) -> Option<String>, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| var(name))
}

fn text(options: &Value, key: &str) -> Option<String> {
    options.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

// `google-vertex`: options, then the Vertex and generic Google Cloud variables.
pub fn project(options: &Value, var: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    text(options, "project").or_else(|| first(var, &PROJECT_ENV))
}

pub fn location(options: &Value, var: &dyn Fn(&str) -> Option<String>) -> String {
    text(options, "location").or_else(|| first(var, &LOCATION_ENV)).unwrap_or_else(|| "us-central1".into())
}

// `google-vertex-anthropic` ignores the Vertex-specific variables and defaults to the global endpoint.
pub fn anthropic_project(var: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    first(var, &PROJECT_ENV[1..])
}

pub fn anthropic_location(var: &dyn Fn(&str) -> Option<String>) -> String {
    first(var, &LOCATION_ENV[1..]).unwrap_or_else(|| "global".into())
}

// Values for `${GOOGLE_VERTEX_PROJECT}`, `${GOOGLE_VERTEX_LOCATION}` and `${GOOGLE_VERTEX_ENDPOINT}` in catalog URLs.
pub fn vars(project: &str, location: &str) -> Vec<(String, String)> {
    vec![("GOOGLE_VERTEX_PROJECT".into(), project.into()), ("GOOGLE_VERTEX_LOCATION".into(), location.into()), ("GOOGLE_VERTEX_ENDPOINT".into(), host(location))]
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    // Explicit credentials file; None follows `GOOGLE_APPLICATION_CREDENTIALS`, then the gcloud well-known file.
    pub credentials_file: Option<PathBuf>,
    pub token_url: String,
    pub metadata_url: String,
}

impl Default for Config {
    fn default() -> Self {
        Config { credentials_file: None, token_url: TOKEN_URL.into(), metadata_url: METADATA_TOKEN_URL.into() }
    }
}

fn well_known() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(explicit) = var("GOOGLE_APPLICATION_CREDENTIALS") {
        return Some(explicit);
    }
    let root = if cfg!(windows) { var("APPDATA")? } else { var("HOME")?.join(".config") };
    Some(root.join("gcloud").join("application_default_credentials.json")).filter(|p| p.exists())
}

async fn exchange(http: &reqwest::Client, request: reqwest::RequestBuilder) -> Result<(String, u64), Error> {
    let _ = http;
    let response = request.send().await?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(Error::Config(format!("Google access token request failed ({}): {body}", status.as_u16())));
    }
    let data: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let token = data.get("access_token").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(|| Error::Config("Google access token response has no access_token".into()))?;
    Ok((token.to_string(), now_ms() + data.get("expires_in").and_then(Value::as_u64).unwrap_or(3600) * 1000))
}

// An access token from Application Default Credentials: a user credentials file, else the metadata server.
pub async fn token(http: &reqwest::Client, config: &Config) -> Result<String, Error> {
    let file = config.credentials_file.clone().or_else(well_known);
    let key = format!("{}|{}", file.as_ref().map(|p| p.display().to_string()).unwrap_or_default(), config.token_url);
    if let Some((cached_key, token, expires)) = TOKEN.lock().unwrap().clone()
        && cached_key == key
        && expires.saturating_sub(now_ms()) > REFRESH_BUFFER_MS
    {
        return Ok(token);
    }
    let (token, expires) = match &file {
        Some(file) => {
            let text = std::fs::read_to_string(file).map_err(|e| Error::Config(format!("cannot read Google credentials {}: {e}", file.display())))?;
            let data: Value = serde_json::from_str(&text).map_err(|e| Error::Config(format!("invalid Google credentials {}: {e}", file.display())))?;
            let field = |name: &str| data.get(name).and_then(Value::as_str).unwrap_or_default();
            match field("type") {
                "authorized_user" => {
                    let form = oauth::form(&[("refresh_token", field("refresh_token")), ("client_id", field("client_id")), ("client_secret", field("client_secret")), ("grant_type", "refresh_token")]);
                    exchange(http, http.post(&config.token_url).header("Content-Type", "application/x-www-form-urlencoded").body(form)).await?
                }
                other => return Err(Error::Config(format!("not supported yet: Google credentials of type {other:?} (use `gcloud auth application-default login` or {API_KEY_ENV})"))),
            }
        }
        None => exchange(http, http.get(&config.metadata_url).header("Metadata-Flavor", "Google").timeout(Duration::from_secs(3)))
            .await
            .map_err(|_| Error::Config("Could not load Google Application Default Credentials (run `gcloud auth application-default login`)".into()))?,
    };
    *TOKEN.lock().unwrap() = Some((key, token.clone(), expires));
    Ok(token)
}

// The Messages body as Vertex takes it: the model lives in the URL and the API version in the body.
pub fn anthropic_body(body: &mut Value) {
    if let Some(map) = body.as_object_mut() {
        map.remove("model");
        map.insert("anthropic_version".into(), ANTHROPIC_VERSION.into());
    }
}

pub fn gemini_path(model: &str) -> String {
    format!("/models/{}:streamGenerateContent?alt=sse", encode_component(model).replace("%2F", "/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn endpoints_and_lookup_order() {
        assert_eq!(base_url("p", "us-central1"), "https://us-central1-aiplatform.googleapis.com/v1beta1/projects/p/locations/us-central1/publishers/google");
        assert_eq!(base_url("p", "global"), "https://aiplatform.googleapis.com/v1beta1/projects/p/locations/global/publishers/google");
        assert_eq!(anthropic_base_url("p", "eu"), "https://aiplatform.eu.rep.googleapis.com/v1/projects/p/locations/eu/publishers/anthropic/models");
        assert_eq!(anthropic_url(&anthropic_base_url("p", "global"), "claude-sonnet-4-5@20250929"), "https://aiplatform.googleapis.com/v1/projects/p/locations/global/publishers/anthropic/models/claude-sonnet-4-5@20250929:streamRawPredict");
        let env = |name: &str| match name {
            "GOOGLE_VERTEX_PROJECT" => Some("vertex".to_string()),
            "GOOGLE_CLOUD_PROJECT" => Some("cloud".to_string()),
            "VERTEX_LOCATION" => Some("europe-west4".to_string()),
            _ => None,
        };
        let none = |_: &str| None;
        assert_eq!((project(&json!({}), &env).as_deref(), project(&json!({"project": "opt"}), &env).as_deref(), project(&json!({}), &none)), (Some("vertex"), Some("opt"), None));
        assert_eq!((location(&json!({}), &env), location(&json!({}), &none), anthropic_location(&none)), ("europe-west4".to_string(), "us-central1".to_string(), "global".to_string()));
        assert_eq!(anthropic_project(&env).as_deref(), Some("cloud"));
        assert_eq!(vars("p", "us")[2].1, "aiplatform.us.rep.googleapis.com");
        let mut body = json!({"model": "claude", "max_tokens": 1});
        anthropic_body(&mut body);
        assert_eq!(body, json!({"max_tokens": 1, "anthropic_version": "vertex-2023-10-16"}));
    }
}
