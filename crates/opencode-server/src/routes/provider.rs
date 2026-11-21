// Provider/model listings in the original's public `Provider` shape.
use crate::error::{ApiError, ApiResult, json};
use crate::state::{AppState, Instance};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use opencode_session::ProviderAuthError;
use std::path::PathBuf;
use opencode_core::auth;
use opencode_llm::provider::{self, Model, Provider};
use serde_json::{Map, Value, json};

const PRIORITY: [&str; 4] = ["gpt-5", "claude-sonnet-4", "big-pickle", "gemini-3-pro"];

fn number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9e15 { (value as i64).into() } else { value.into() }
}

fn modalities(sources: &[Option<&Value>], side: &str, text_default: bool) -> Value {
    let list = sources.iter().flatten().find_map(|m| m.pointer(&format!("/modalities/{side}")).and_then(Value::as_array));
    let has = |kind: &str, default: bool| list.map_or(default, |l| l.iter().any(|v| v == kind));
    json!({
        "text": has("text", text_default),
        "audio": has("audio", false),
        "image": has("image", false),
        "video": has("video", false),
        "pdf": has("pdf", false),
    })
}

// `configured` and `catalog` are the raw model entries the engine's flattened Model was built from.
fn model_json(model: &Model, configured: Option<&Value>, catalog: Option<&Value>) -> Value {
    let sources = [configured, catalog];
    let pick = |pointer: &str| sources.iter().flatten().find_map(|m| m.pointer(pointer)).filter(|v| !v.is_null());
    let cost = |pointer: &str| number(pick(pointer).and_then(Value::as_f64).unwrap_or(0.0));
    let interleaved = match pick("/interleaved") {
        Some(Value::String(field)) => json!({"field": field}),
        Some(other) => other.clone(),
        None if catalog.is_none() && model.npm == "@ai-sdk/openai-compatible" && model.api_id.contains("deepseek") => {
            json!({"field": "reasoning_content"})
        }
        None => Value::Bool(false),
    };
    let mut out = Map::new();
    out.insert("id".into(), model.id.clone().into());
    out.insert("providerID".into(), model.provider_id.clone().into());
    out.insert("api".into(), json!({"id": model.api_id, "url": model.api_url, "npm": model.npm}));
    out.insert("name".into(), model.name.clone().into());
    if let Some(family) = pick("/family").and_then(Value::as_str) {
        out.insert("family".into(), family.into());
    } else if configured.is_some() {
        out.insert("family".into(), "".into());
    }
    out.insert(
        "capabilities".into(),
        json!({
            "temperature": model.temperature,
            "reasoning": model.reasoning,
            "attachment": model.attachment,
            "toolcall": pick("/tool_call").and_then(Value::as_bool).unwrap_or(true),
            "input": modalities(&sources, "input", configured.is_some()),
            "output": modalities(&sources, "output", configured.is_some()),
            "interleaved": interleaved,
        }),
    );
    out.insert(
        "cost".into(),
        json!({
            "input": number(model.cost_input),
            "output": cost("/cost/output"),
            "cache": {"read": cost("/cost/cache_read"), "write": cost("/cost/cache_write")},
        }),
    );
    let mut limit = Map::new();
    limit.insert("context".into(), model.limit_context.into());
    if let Some(input) = pick("/limit/input").and_then(Value::as_f64) {
        limit.insert("input".into(), number(input));
    }
    limit.insert("output".into(), model.limit_output.into());
    out.insert("limit".into(), limit.into());
    out.insert("status".into(), model.status.clone().into());
    out.insert("options".into(), model.options.clone());
    out.insert("headers".into(), Value::Object(model.headers.clone()));
    out.insert("release_date".into(), model.release_date.clone().into());
    out.insert("variants".into(), json!({}));
    Value::Object(out)
}

// The public `Provider.Model` object of each model, keyed by model id. `catalog` is the models.dev catalog (`Engine::catalog_value`).
pub fn model_objects(provider: &Provider, config: &Value, catalog: &Value) -> Map<String, Value> {
    let mut models = Map::new();
    for model in &provider.models {
        let configured = config.pointer(&format!("/provider/{}/models", provider.id)).and_then(|m| m.get(&model.id));
        let listed = catalog.get(&provider.id).and_then(|p| p.get("models")).and_then(|m| m.get(&model.id));
        models.insert(model.id.clone(), model_json(model, configured, listed));
    }
    models
}

fn provider_json(provider: &Provider, source: &str, config: &Value, catalog: &Value) -> Value {
    let models = model_objects(provider, config, catalog);
    let mut out = Map::new();
    out.insert("id".into(), provider.id.clone().into());
    out.insert("name".into(), provider.name.clone().into());
    out.insert("source".into(), source.into());
    out.insert("env".into(), provider.env.clone().into());
    if let Some(key) = &provider.key {
        out.insert("key".into(), key.clone().into());
    }
    out.insert("options".into(), provider.options.clone());
    out.insert("models".into(), models.into());
    Value::Object(out)
}

// Later stages overwrite earlier ones in the original: env, then stored key, then config.
fn source(provider: &Provider, config: &Value, stored: &auth::Entries) -> &'static str {
    if config.pointer(&format!("/provider/{}", provider.id)).is_some() {
        "config"
    } else if stored.iter().any(|(id, info)| id == &provider.id && matches!(info, auth::Info::Api { .. })) {
        "api"
    } else if provider.env.iter().any(|name| std::env::var(name).is_ok_and(|v| !v.is_empty())) {
        "env"
    } else {
        "custom"
    }
}

// `Provider.sort(models)[0]`: priority family, then "latest", then id descending.
fn default_model(models: &Map<String, Value>) -> Option<String> {
    let rank = |id: &str| PRIORITY.iter().position(|needle| id.contains(needle)).map_or(-1, |i| i as i64);
    let mut ids: Vec<&str> = models.values().filter_map(|m| m["id"].as_str()).collect();
    ids.sort_by(|a, b| rank(b).cmp(&rank(a)).then(a.contains("latest").cmp(&b.contains("latest")).reverse()).then(b.cmp(a)));
    ids.first().map(|id| id.to_string())
}

fn defaults(providers: &[Value]) -> Value {
    let mut out = Map::new();
    for provider in providers {
        if let (Some(id), Some(models)) = (provider["id"].as_str(), provider["models"].as_object())
            && let Some(model) = default_model(models)
        {
            out.insert(id.to_string(), model.into());
        }
    }
    Value::Object(out)
}

struct Loaded {
    config: Value,
    catalog: std::sync::Arc<Value>,
    stored: auth::Entries,
    connected: Vec<Provider>,
}

// `connected` is the engine's own list, so discovery and plugin-provided models appear as the loop sees them; the
// catalog is the one the engine built them from.
async fn load(state: &AppState, instance: &Instance) -> ApiResult<Loaded> {
    let config = state.load_config(instance)?.config;
    let connected = state
        .engine
        .providers(&PathBuf::from(&instance.directory))
        .await
        .map_err(|error| ApiError::unknown(StatusCode::BAD_REQUEST, error.to_string()))?;
    let catalog = state.engine.catalog_value().await;
    let stored = auth::read_all(&state.engine.paths.auth_file());
    Ok(Loaded { config, catalog, stored, connected })
}

pub async fn config_providers(State(state): State<AppState>, instance: Instance) -> ApiResult {
    let loaded = load(&state, &instance).await?;
    let providers: Vec<Value> = loaded
        .connected
        .iter()
        .map(|p| provider_json(p, source(p, &loaded.config, &loaded.stored), &loaded.config, &loaded.catalog))
        .collect();
    Ok(json(json!({"default": defaults(&providers), "providers": providers})))
}

// The connected providers in the public shape.
pub async fn connected_json(state: &AppState, instance: &Instance) -> ApiResult<Vec<Value>> {
    let loaded = load(state, instance).await?;
    Ok(loaded.connected.iter().map(|p| provider_json(p, source(p, &loaded.config, &loaded.stored), &loaded.config, &loaded.catalog)).collect())
}

pub async fn list(State(state): State<AppState>, instance: Instance) -> ApiResult {
    let (all, connected) = all_json(&state, &instance).await?;
    Ok(json(json!({"all": all, "default": defaults(&all), "connected": connected})))
}

// Every known provider, and the ids of those that are connected.
pub async fn all_json(state: &AppState, instance: &Instance) -> ApiResult<(Vec<Value>, Vec<String>)> {
    let loaded = load(state, instance).await?;
    let strings = |key: &str| -> Option<Vec<&str>> { Some(loaded.config.get(key)?.as_array()?.iter().filter_map(Value::as_str).collect()) };
    let disabled = strings("disabled_providers").unwrap_or_default();
    let enabled = strings("enabled_providers");

    // Every catalog provider, replaced by its connected form when there is one.
    let mut all: Vec<Value> = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    let unconnected = catalog_only(&loaded.catalog);
    for provider in &unconnected {
        if disabled.contains(&provider.id.as_str()) || enabled.as_ref().is_some_and(|allow| !allow.contains(&provider.id.as_str())) {
            continue;
        }
        let value = match loaded.connected.iter().find(|c| c.id == provider.id) {
            Some(connected) => provider_json(connected, source(connected, &loaded.config, &loaded.stored), &loaded.config, &loaded.catalog),
            None => provider_json(provider, "custom", &json!({}), &loaded.catalog),
        };
        seen.push(&provider.id);
        all.push(value);
    }
    for connected in loaded.connected.iter().filter(|c| !seen.contains(&c.id.as_str())) {
        all.push(provider_json(connected, source(connected, &loaded.config, &loaded.stored), &loaded.config, &loaded.catalog));
    }
    let connected: Vec<String> = all
        .iter()
        .filter_map(|p| p["id"].as_str())
        .filter(|id| loaded.connected.iter().any(|c| c.id == *id) || loaded.stored.iter().any(|(key, _)| key == id))
        .map(str::to_string)
        .collect();
    Ok((all, connected))
}

// The catalog as providers, before any activation or model filtering.
fn catalog_only(catalog: &Value) -> Vec<Provider> {
    let mut out = Vec::new();
    for (id, entry) in catalog.as_object().into_iter().flatten() {
        // A config entry activates the provider without changing its catalog models.
        let config = json!({"provider": {id: {}}});
        out.extend(provider::build(&json!({id: entry}), &config, &Vec::new()).into_iter().map(|mut p| {
            p.key = None;
            p.options = json!({});
            p
        }));
    }
    out
}

// `ProviderAuthApiError`: `{name, data}` with status 400.
fn auth_error(error: ProviderAuthError) -> ApiError {
    let (name, data) = match error {
        ProviderAuthError::OauthMissing { provider_id } => ("ProviderAuthOauthMissing", json!({"providerID": provider_id})),
        ProviderAuthError::OauthCodeMissing { provider_id } => ("ProviderAuthOauthCodeMissing", json!({"providerID": provider_id})),
        ProviderAuthError::OauthCallbackFailed => ("ProviderAuthOauthCallbackFailed", json!({})),
        ProviderAuthError::ValidationFailed { field, message } => ("ProviderAuthValidationFailed", json!({"field": field, "message": message})),
        ProviderAuthError::Other(_) => ("BadRequest", json!({})),
    };
    ApiError::new(StatusCode::BAD_REQUEST, json!({"name": name, "data": data}))
}

// Auth methods contributed by plugins, by provider id.
pub async fn auth_methods(State(state): State<AppState>, instance: Instance) -> ApiResult {
    Ok(json(state.engine.provider_auth_methods(&PathBuf::from(&instance.directory)).await.map_err(auth_error)?))
}

pub async fn oauth_authorize(State(state): State<AppState>, instance: Instance, Path(provider_id): Path<String>, body: Bytes) -> ApiResult {
    let bad = || auth_error(ProviderAuthError::Other(String::new()));
    let payload: Value = serde_json::from_slice(&body).map_err(|_| bad())?;
    let method = payload.get("method").and_then(Value::as_u64).ok_or_else(bad)?;
    let inputs = payload.get("inputs").filter(|inputs| inputs.is_object()).cloned();
    let directory = PathBuf::from(&instance.directory);
    let result = state.engine.provider_auth_authorize(&directory, &provider_id, method, inputs).await.map_err(auth_error)?;
    Ok(json(result))
}

pub async fn oauth_callback(State(state): State<AppState>, instance: Instance, Path(provider_id): Path<String>, body: Bytes) -> ApiResult {
    let payload: Value = crate::routes::session::payload(&body)?;
    if !payload.get("method").is_some_and(Value::is_number) {
        return Err(ApiError::schema("Payload", "Expected a number at [\"method\"]"));
    }
    let directory = PathBuf::from(&instance.directory);
    state.engine.provider_auth_callback(&directory, &provider_id, payload.get("code").and_then(Value::as_str)).await.map_err(auth_error)?;
    Ok(json(true))
}
