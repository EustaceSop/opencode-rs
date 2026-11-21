// DigitalOcean: implicit-grant browser login and Inference Router discovery.
use crate::oauth::{self, Listener, Reply, now_ms};
use crate::provider::Model;
use crate::{Error, provider, shape};
use opencode_core::auth;
use serde_json::{Map, Value, json};
use std::path::Path;
use std::time::Duration;

pub const PROVIDER_ID: &str = "digitalocean";
pub const CLIENT_ID: &str = "b1a6c5158156caac821fd1b30253ca8acb52454a48fa744420e41889cb589f82";
pub const AUTHORIZE_URL: &str = "https://cloud.digitalocean.com/v1/oauth/authorize";
pub const API_BASE: &str = "https://api.digitalocean.com";
pub const INFERENCE_BASE: &str = "https://inference.do-ai.run/v1";
pub const OAUTH_PORT: u16 = 1456;
pub const REDIRECT_PATH: &str = "/auth/callback";
pub const TOKEN_PATH: &str = "/auth/token";
pub const SCOPES: &str = "genai:read inference:query";
pub const ROUTER_REFRESH_MS: u64 = 5 * 60 * 1000;
pub const LOGIN_INSTRUCTIONS: &str = "Sign in to DigitalOcean in your browser. OpenCode will use your DigitalOcean API token directly for inference and load your Inference Routers. Re-run /connect to refresh routers later.";
const DEFAULT_LIFETIME_S: u64 = 60 * 60 * 24 * 30;

// The implicit grant returns the token in the URL fragment, which only the browser sees: this page posts it back.
const BOOTSTRAP: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>OpenCode - DigitalOcean</title></head><body><h1 id="h">Finishing sign-in...</h1><script>
(function(){var h=new URLSearchParams((window.location.hash||"").slice(1)),s=new URLSearchParams(window.location.search);
var e=h.get("error")||s.get("error"),d=h.get("error_description")||s.get("error_description");
var b=e?{error:e,error_description:d||""}:{access_token:h.get("access_token")||"",expires_in:h.get("expires_in")||"0",state:h.get("state")||""};
fetch(new URL("__TOKEN_PATH__",window.location.origin).href,{method:"POST",headers:{"Content-Type":"application/json"},body:JSON.stringify(b)}).then(function(r){
document.getElementById("h").textContent=r.ok&&!e?"Authorization successful. You can close this window.":"Authorization failed";history.replaceState(null,"",window.location.pathname);
}).catch(function(){document.getElementById("h").textContent="Authorization failed";});})();
</script></body></html>"#;

#[derive(Debug, Clone, PartialEq)]
pub struct Router {
    pub name: String,
    pub uuid: Option<String>,
    pub description: Option<String>,
}

fn router(value: &Value) -> Option<Router> {
    let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
    Some(Router { name: text("name")?, uuid: text("uuid"), description: text("description") })
}

fn routers_json(routers: &[Router]) -> String {
    Value::Array(routers.iter().map(|r| json!({"name": r.name, "uuid": r.uuid, "description": r.description})).collect()).to_string()
}

pub fn parse_routers(raw: Option<&str>) -> Vec<Router> {
    let parsed: Value = raw.and_then(|r| serde_json::from_str(r).ok()).unwrap_or(Value::Null);
    parsed.as_array().into_iter().flatten().filter_map(router).collect()
}

// Err carries the HTTP status (0 when the request itself failed).
pub async fn list_routers(http: &reqwest::Client, api_base: &str, bearer: &str) -> Result<Vec<Router>, u16> {
    let response = http
        .get(format!("{api_base}/v2/gen-ai/models/routers"))
        .header("Authorization", format!("Bearer {bearer}"))
        .header("Accept", "application/json")
        .header("User-Agent", shape::user_agent())
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|_| 0u16)?;
    if !response.status().is_success() {
        return Err(response.status().as_u16());
    }
    let body: Value = serde_json::from_str(&response.text().await.unwrap_or_default()).unwrap_or(Value::Null);
    Ok(body.get("model_routers").and_then(Value::as_array).into_iter().flatten().filter_map(router).collect())
}

pub fn router_model(router: &Router) -> Model {
    let id = format!("router:{}", router.name);
    provider::model_from_info(
        PROVIDER_ID,
        &id,
        &json!({
            "id": id, "name": router.name, "family": "digitalocean-inference-routers",
            "api": {"id": id, "url": INFERENCE_BASE, "npm": "@ai-sdk/openai-compatible"},
            "status": "active", "limit": {"context": 128_000, "output": 8192},
            "capabilities": {"temperature": true, "reasoning": false, "attachment": false, "toolcall": true, "input": {"text": true}},
            "variants": {}
        }),
    )
}

// The `provider.models` hook: catalog models plus one `router:<name>` model per Inference Router. The router list is
// cached in the stored entry's metadata and refreshed every five minutes while the OAuth token is valid.
pub async fn models(http: &reqwest::Client, api_base: &str, auth_file: &Path, stored: Option<&auth::Info>, mut base: Vec<Model>) -> Vec<Model> {
    let Some(auth::Info::Api { key, metadata }) = stored else { return base };
    let metadata = metadata.clone().unwrap_or_default();
    let text = |name: &str| metadata.get(name).and_then(Value::as_str);
    let number = |name: &str| text(name).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let mut routers = parse_routers(text("routers"));
    let stale = now_ms().saturating_sub(number("routers_fetched_at")) > ROUTER_REFRESH_MS;
    if let Some(access) = text("oauth_access").filter(|a| !a.is_empty() && number("oauth_expires") > now_ms() && stale)
        && let Ok(found) = list_routers(http, api_base, access).await
    {
        routers = found;
        let mut updated = metadata.clone();
        updated.insert("routers".into(), routers_json(&routers).into());
        updated.insert("routers_fetched_at".into(), now_ms().to_string().into());
        let _ = auth::set(auth_file, PROVIDER_ID, auth::Info::Api { key: key.clone(), metadata: Some(updated) });
    }
    for router in &routers {
        let id = format!("router:{}", router.name);
        if !base.iter().any(|m| m.id == id) {
            base.push(router_model(router));
        }
    }
    base
}

pub struct Login {
    pub url: String,
    pub instructions: &'static str,
    state: String,
    listener: Listener,
}

pub async fn authorize() -> Result<Login, Error> {
    authorize_on(OAUTH_PORT, OAUTH_PORT).await
}

// `advertised` is the port in the redirect URI registered for the client; tests listen elsewhere.
pub(crate) async fn authorize_on(port: u16, advertised: u16) -> Result<Login, Error> {
    let listener = Listener::bind("127.0.0.1", port).await?;
    let state = oauth::random_bytes(32).iter().map(|b| format!("{b:02x}")).collect::<String>();
    let redirect = format!("http://localhost:{advertised}{REDIRECT_PATH}");
    let query = oauth::form(&[("response_type", "token"), ("client_id", CLIENT_ID), ("redirect_uri", &redirect), ("scope", SCOPES), ("state", &state)]);
    Ok(Login { url: format!("{AUTHORIZE_URL}?{query}"), instructions: LOGIN_INSTRUCTIONS, state, listener })
}

impl Login {
    pub fn port(&self) -> u16 {
        self.listener.port
    }

    // Waits for the browser to relay the token (five minutes at most), loads the routers, and returns the entry to
    // store under `digitalocean`: an api key whose metadata remembers the OAuth grant.
    pub async fn wait(self, http: &reqwest::Client, api_base: &str) -> Result<auth::Info, Error> {
        let state = self.state.clone();
        let relayed = self.listener.serve(|request| {
            if request.method == "GET" && request.path == REDIRECT_PATH {
                return (Reply { status: 200, content_type: "text/html", body: BOOTSTRAP.replace("__TOKEN_PATH__", TOKEN_PATH) }, None);
            }
            if request.method != "POST" || request.path != TOKEN_PATH {
                return (Reply::text(404, "Not found"), None);
            }
            let body: Value = serde_json::from_str(&request.body).unwrap_or(Value::Null);
            let text = |key: &str| body.get(key).and_then(Value::as_str).filter(|s| !s.is_empty());
            if let Some(error) = text("error") {
                let message = text("error_description").unwrap_or(error).to_string();
                return (Reply::json(200, json!({"ok": true})), Some(Err(Error::Config(message))));
            }
            let Some(token) = text("access_token") else {
                return (Reply::json(400, json!({"error": "missing_access_token"})), Some(Err(Error::Config("Missing access_token in callback".into()))));
            };
            if text("state") != Some(state.as_str()) {
                return (Reply::json(400, json!({"error": "invalid_state"})), Some(Err(Error::Config("Invalid state - potential CSRF attack".into()))));
            }
            let expires = body.get("expires_in").and_then(|v| v.as_str().and_then(|s| s.parse::<u64>().ok()).or(v.as_u64())).filter(|n| *n > 0).unwrap_or(DEFAULT_LIFETIME_S);
            (Reply::json(200, json!({"ok": true})), Some(Ok((token.to_string(), expires))))
        });
        let (token, expires_in) = tokio::time::timeout(oauth::CALLBACK_TIMEOUT, relayed).await.map_err(|_| Error::Config("OAuth callback timeout - authorization took too long".into()))???;
        let routers = list_routers(http, api_base, &token).await.unwrap_or_default();
        let mut metadata = Map::new();
        metadata.insert("oauth_access".into(), token.clone().into());
        metadata.insert("oauth_expires".into(), (now_ms() + expires_in * 1000).to_string().into());
        metadata.insert("oauth_scopes".into(), SCOPES.into());
        metadata.insert("routers".into(), routers_json(&routers).into());
        metadata.insert("routers_fetched_at".into(), now_ms().to_string().into());
        Ok(auth::Info::Api { key: token, metadata: Some(metadata) })
    }
}
