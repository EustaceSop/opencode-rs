// The v2 `/api/*` surface: location selection, the v2 event envelope, and the catalog-style routes.
pub mod copy;
pub mod session;

use crate::error::{ApiError, ApiResult, json};
use crate::routes::{file, provider};
use crate::state::{AppState, Inner, Instance, Params, Pending, decode_component, lock, query_get, query_pairs};
use axum::body::{Body, Bytes};
use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use futures_util::stream;
use opencode_core::auth;
use opencode_core::permission::{self, Action};
use opencode_session::PermissionAnswer;
use opencode_storage::id::{self, Prefix};
use opencode_storage::{StoredEvent, events};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

// `EventManifest.ServerDefinitions` plus `server.connected`: what `/api/event` can encode.
pub const EVENT_TYPES: [&str; 59] = [
    "models-dev.refreshed",
    "integration.updated",
    "integration.connection.updated",
    "catalog.updated",
    "session.created",
    "session.updated",
    "session.deleted",
    "message.updated",
    "message.removed",
    "message.part.updated",
    "message.part.removed",
    "session.next.agent.switched",
    "session.next.model.switched",
    "session.next.moved",
    "session.next.prompted",
    "session.next.prompt.admitted",
    "session.next.context.updated",
    "session.next.synthetic",
    "session.next.shell.started",
    "session.next.shell.ended",
    "session.next.step.started",
    "session.next.step.ended",
    "session.next.step.failed",
    "session.next.text.started",
    "session.next.text.delta",
    "session.next.text.ended",
    "session.next.reasoning.started",
    "session.next.reasoning.delta",
    "session.next.reasoning.ended",
    "session.next.tool.input.started",
    "session.next.tool.input.delta",
    "session.next.tool.input.ended",
    "session.next.tool.called",
    "session.next.tool.progress",
    "session.next.tool.success",
    "session.next.tool.failed",
    "session.next.retried",
    "session.next.compaction.started",
    "session.next.compaction.delta",
    "session.next.compaction.ended",
    "session.next.revert.staged",
    "session.next.revert.cleared",
    "session.next.revert.committed",
    "file.edited",
    "reference.updated",
    "permission.v2.asked",
    "permission.v2.replied",
    "plugin.added",
    "project.directories.updated",
    "file.watcher.updated",
    "pty.created",
    "pty.updated",
    "pty.exited",
    "pty.deleted",
    "question.v2.asked",
    "question.v2.replied",
    "question.v2.rejected",
    "todo.updated",
    "server.connected",
];

pub const QUEUE: usize = 256;
const HEARTBEAT: Duration = Duration::from_secs(15);

pub struct Attempt {
    pub provider_id: String,
    pub directory: String,
    pub created: u64,
    pub expires: u64,
    // pending | complete | failed
    pub status: String,
    pub message: Option<String>,
}

pub struct V2 {
    pub events: broadcast::Sender<Arc<Value>>,
    // Every durable `session.next.*` event once it is in the store's event log; wakes `/api/session/{id}/event`.
    pub durable: broadcast::Sender<Arc<Value>>,
    // What has already been reported for each part of a session.
    pub stages: Mutex<HashMap<String, HashMap<String, session::Stage>>>,
    pub attempts: Mutex<HashMap<String, Attempt>>,
    // Admitted turns per session that have not finished, including those the engine has not seen yet.
    pub turns: Mutex<HashMap<String, usize>>,
}

impl Default for V2 {
    fn default() -> Self {
        Self {
            events: broadcast::channel(QUEUE).0,
            durable: broadcast::channel(4096).0,
            stages: Mutex::default(),
            attempts: Mutex::default(),
            turns: Mutex::default(),
        }
    }
}

fn envelope(directory: Option<&str>, kind: &str, data: Value) -> Map<String, Value> {
    let mut frame = Map::new();
    frame.insert("id".into(), id::ascending(Prefix::Event).into());
    frame.insert("type".into(), kind.into());
    frame.insert("data".into(), data);
    if let Some(directory) = directory {
        frame.insert("location".into(), json!({"directory": directory}));
    }
    frame
}

impl Inner {
    // An event only the v2 stream carries.
    pub fn v2_publish(&self, directory: Option<&str>, kind: &str, data: Value) {
        if EVENT_TYPES.contains(&kind) {
            let _ = self.v2.events.send(Arc::new(Value::Object(envelope(directory, kind, data))));
        }
    }

    // A `session.next.*` event: durable ones are committed to the store's event log under the session, whose next
    // sequence number they get (shared with the v1 events of its row writes, counting from 0).
    pub fn v2_session_event(&self, directory: &str, session_id: &str, kind: &str, mut data: Value) -> u64 {
        data["timestamp"] = id::now_ms().into();
        data["sessionID"] = session_id.into();
        let mut frame = envelope(Some(directory), kind, data);
        let Some(version) = events::session_next_version(kind) else {
            let _ = self.v2.events.send(Arc::new(Value::Object(frame)));
            return 0;
        };
        let event_id = frame["id"].as_str().unwrap_or_default().to_string();
        let seq = match self.engine.store.append_event(&event_id, session_id, &events::versioned_type(kind, version), &frame["data"]) {
            Ok(seq) => seq,
            Err(error) => {
                tracing::error!(session = session_id, kind, %error, "durable event not committed");
                return 0;
            }
        };
        frame.insert("durable".into(), json!({"aggregateID": session_id, "seq": seq, "version": version}));
        let event = Arc::new(Value::Object(frame));
        let _ = self.v2.events.send(event.clone());
        let _ = self.v2.durable.send(event);
        seq
    }

    // Legacy bus events of the types the v2 stream carries are mirrored into it.
    pub fn v2_mirror(&self, event: &crate::bus::Event) {
        if !event.global_only && EVENT_TYPES.contains(&event.kind.as_str()) {
            let frame = envelope(event.directory.as_deref(), &event.kind, event.properties.clone());
            let _ = self.v2.events.send(Arc::new(Value::Object(frame)));
        }
    }
}

// The location a v2 request addresses: `location[directory]`, else `x-opencode-directory`, else the cwd.
pub struct Loc {
    pub instance: Instance,
    pub workspace: Option<String>,
}

impl FromRequestParts<AppState> for Loc {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let pairs = query_pairs(parts.uri.query());
        let header = |name: &str| parts.headers.get(name).and_then(|v| v.to_str().ok()).filter(|v| !v.is_empty());
        let directory = match query_get(&pairs, "location[directory]").filter(|d| !d.is_empty()) {
            Some(directory) => directory.to_string(),
            None => header("x-opencode-directory").map_or_else(|| state.cwd.clone(), decode_component),
        };
        let workspace = query_get(&pairs, "location[workspace]").filter(|w| !w.is_empty()).or(header("x-opencode-workspace")).map(str::to_string);
        state.touch(&directory);
        Ok(Loc { instance: state.instance_for(&directory, false), workspace })
    }
}

impl Loc {
    pub fn info(&self, state: &AppState) -> ApiResult<Value> {
        let project = state.project(&self.instance).map_err(ApiError::into_v2)?;
        let root = self.instance.worktree.as_ref().map_or_else(|| self.instance.directory.clone(), |w| w.to_string_lossy().into_owned());
        let mut out = Map::new();
        out.insert("directory".into(), self.instance.directory.clone().into());
        if let Some(workspace) = &self.workspace {
            out.insert("workspaceID".into(), workspace.clone().into());
        }
        out.insert("project".into(), json!({"id": project.id, "directory": root}));
        Ok(Value::Object(out))
    }

    // `{location, data}`
    pub fn wrap(&self, state: &AppState, data: impl serde::Serialize) -> ApiResult {
        Ok(json(json!({"location": self.info(state)?, "data": data})))
    }
}

pub fn payload(body: &Bytes) -> ApiResult<Value> {
    let value: Value = serde_json::from_slice(body).map_err(|e| ApiError::invalid_request("Payload", e.to_string()))?;
    if value.is_object() { Ok(value) } else { Err(ApiError::invalid_request("Payload", "Expected object")) }
}

pub fn required<'a>(body: &'a Value, key: &str) -> ApiResult<&'a str> {
    body[key].as_str().ok_or_else(|| ApiError::invalid_request("Payload", format!("Expected string at [\"{key}\"]")))
}

fn no_content() -> ApiResult {
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub async fn health() -> Response {
    json(json!({"healthy": true}))
}

static V2_DOC: std::sync::OnceLock<String> = std::sync::OnceLock::new();

// The `/api/*` half of the document, as the v2-only server variant serves it.
pub async fn openapi() -> Response {
    let text = V2_DOC.get_or_init(|| {
        let mut document: Value = serde_json::from_str(crate::openapi()).unwrap_or(Value::Null);
        if let Some(paths) = document.get_mut("paths").and_then(Value::as_object_mut) {
            paths.retain(|path, _| path.starts_with("/api/") || path.starts_with("/experimental/project/{projectID}/copy") && !path.ends_with("generate-name"));
        }
        document.to_string()
    });
    ([(header::CONTENT_TYPE, "application/json")], text.as_str()).into_response()
}

pub async fn location(State(state): State<AppState>, loc: Loc) -> ApiResult {
    Ok(json(loc.info(&state)?))
}

fn model_ref(provider_id: &Value, model_id: &Value, variant: Option<&Value>) -> Value {
    let mut out = json!({"id": model_id, "providerID": provider_id});
    if let Some(variant) = variant.filter(|v| v.is_string()) {
        out["variant"] = variant.clone();
    }
    out
}

fn copy(from: &Value, to: &mut Map<String, Value>, keys: &[(&str, &str)]) {
    for (source, target) in keys {
        if let Some(value) = from.get(*source).filter(|v| !v.is_null()) {
            to.insert((*target).to_string(), value.clone());
        }
    }
}

pub async fn agents(State(state): State<AppState>, loc: Loc) -> ApiResult {
    let loaded = state.load_config(&loc.instance).map_err(ApiError::into_v2)?;
    let info = loaded.info().map_err(|e| ApiError::config(&e).into_v2())?;
    let worktree = loc.instance.worktree.clone().unwrap_or_else(|| PathBuf::from("/"));
    let context = opencode_core::agent::Context { paths: &state.engine.paths, worktree: &worktree, skill_dirs: &[], reference_dirs: &[] };
    let agents = opencode_core::agent::Agents::build(&info, &context);
    let list: Vec<Value> = agents
        .list()
        .into_iter()
        .map(|agent| {
            let agent = json!(agent);
            let mut out = Map::new();
            out.insert("id".into(), agent["name"].clone());
            if agent["model"].is_object() {
                out.insert("model".into(), model_ref(&agent["model"]["providerID"], &agent["model"]["modelID"], agent.get("variant")));
            }
            let body = agent.get("options").filter(|o| o.is_object()).cloned().unwrap_or_else(|| json!({}));
            out.insert("request".into(), json!({"headers": {}, "body": body}));
            copy(&agent, &mut out, &[("prompt", "system"), ("description", "description")]);
            out.insert("mode".into(), agent["mode"].clone());
            out.insert("hidden".into(), agent["hidden"].as_bool().unwrap_or(false).into());
            copy(&agent, &mut out, &[("color", "color"), ("steps", "steps")]);
            let rules: Vec<Value> = agent["permission"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|rule| json!({"action": rule["permission"], "resource": rule["pattern"], "effect": rule["action"]}))
                .collect();
            out.insert("permissions".into(), rules.into());
            Value::Object(out)
        })
        .collect();
    loc.wrap(&state, list)
}

// "YYYY-MM-DD" as epoch milliseconds; anything else is 0.
fn released(date: &str) -> i64 {
    let mut fields = date.splitn(3, '-').map(|part| part.parse::<i64>().ok());
    let (Some(Some(year)), Some(Some(month)), Some(Some(day))) = (fields.next(), fields.next(), fields.next()) else { return 0 };
    let (y, m) = if month <= 2 { (year - 1, month + 9) } else { (year, month - 3) };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468) * 86_400_000
}

fn modalities(sides: &Value) -> Vec<&str> {
    ["text", "audio", "image", "video", "pdf"].into_iter().filter(|kind| sides[*kind] == true).collect()
}

fn model_v2(model: &Value) -> Value {
    let mut api = json!({"id": model["api"]["id"], "type": "aisdk", "package": model["api"]["npm"]});
    if let Some(url) = model["api"]["url"].as_str().filter(|u| !u.is_empty()) {
        api["url"] = url.into();
    }
    let mut out = Map::new();
    out.insert("id".into(), model["id"].clone());
    out.insert("providerID".into(), model["providerID"].clone());
    if let Some(family) = model["family"].as_str().filter(|f| !f.is_empty()) {
        out.insert("family".into(), family.into());
    }
    out.insert("name".into(), model["name"].clone());
    out.insert("api".into(), api);
    let capabilities = &model["capabilities"];
    out.insert(
        "capabilities".into(),
        json!({"tools": capabilities["toolcall"].as_bool().unwrap_or(true), "input": modalities(&capabilities["input"]), "output": modalities(&capabilities["output"])}),
    );
    let headers = model.get("headers").filter(|h| h.is_object()).cloned().unwrap_or_else(|| json!({}));
    let body = model.get("options").filter(|o| o.is_object()).cloned().unwrap_or_else(|| json!({}));
    out.insert("request".into(), json!({"headers": headers, "body": body}));
    out.insert("variants".into(), json!([]));
    out.insert("time".into(), json!({"released": released(model["release_date"].as_str().unwrap_or_default())}));
    out.insert("cost".into(), json!([{"input": model["cost"]["input"], "output": model["cost"]["output"], "cache": model["cost"]["cache"]}]));
    let status = model["status"].as_str().filter(|s| matches!(*s, "alpha" | "beta" | "deprecated" | "active")).unwrap_or("active");
    out.insert("status".into(), status.into());
    out.insert("enabled".into(), true.into());
    let mut limit = json!({"context": model["limit"]["context"], "output": model["limit"]["output"]});
    if let Some(input) = model["limit"].get("input").filter(|i| i.is_u64()) {
        limit["input"] = input.clone();
    }
    out.insert("limit".into(), limit);
    Value::Object(out)
}

fn provider_v2(provider: &Value) -> Value {
    let package = provider["models"].as_object().and_then(|m| m.values().next()).and_then(|m| m["api"]["npm"].as_str()).unwrap_or("@ai-sdk/openai-compatible");
    let mut api = json!({"type": "aisdk", "package": package});
    if let Some(url) = provider["options"]["baseURL"].as_str() {
        api["url"] = url.into();
    }
    json!({"id": provider["id"], "integrationID": provider["id"], "name": provider["name"], "api": api, "request": {"headers": {}, "body": {}}})
}

async fn connected(state: &AppState, loc: &Loc) -> ApiResult<Vec<Value>> {
    provider::connected_json(state, &loc.instance).await.map_err(ApiError::into_v2)
}

pub async fn models(State(state): State<AppState>, loc: Loc) -> ApiResult {
    let list: Vec<Value> =
        connected(&state, &loc).await?.iter().flat_map(|p| p["models"].as_object().into_iter().flat_map(|m| m.values().map(model_v2))).collect();
    loc.wrap(&state, list)
}

pub async fn providers(State(state): State<AppState>, loc: Loc) -> ApiResult {
    loc.wrap(&state, connected(&state, &loc).await?.iter().map(provider_v2).collect::<Vec<_>>())
}

pub async fn provider_get(State(state): State<AppState>, loc: Loc, Path(provider_id): Path<String>) -> ApiResult {
    let list = connected(&state, &loc).await?;
    let found = list.iter().find(|p| p["id"] == provider_id.as_str()).ok_or_else(|| {
        ApiError::v2_not_found("ProviderNotFoundError", "providerID", &provider_id, format!("Provider not found: {provider_id}"))
    })?;
    loc.wrap(&state, provider_v2(found))
}

async fn integration_list(state: &AppState, loc: &Loc) -> ApiResult<Vec<Value>> {
    let (all, _) = provider::all_json(state, &loc.instance).await.map_err(ApiError::into_v2)?;
    let stored = auth::read_all(&state.engine.paths.auth_file());
    let oauth = state.engine.provider_auth_methods(&PathBuf::from(&loc.instance.directory)).await.unwrap_or_else(|_| json!({}));
    Ok(all
        .iter()
        .map(|provider| {
            let provider_id = provider["id"].as_str().unwrap_or_default();
            let names: Vec<&str> = provider["env"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            let mut methods: Vec<Value> = oauth[provider_id]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .filter(|(_, method)| method["type"] == "oauth")
                .map(|(index, method)| json!({"id": index.to_string(), "type": "oauth", "label": method["label"].as_str().unwrap_or("OAuth")}))
                .collect();
            methods.push(json!({"type": "key"}));
            if !names.is_empty() {
                methods.push(json!({"type": "env", "names": names}));
            }
            let mut connections = Vec::new();
            if stored.iter().any(|(key, _)| key == provider_id) {
                connections.push(json!({"type": "credential", "id": provider_id, "label": provider["name"]}));
            }
            connections.extend(names.iter().filter(|name| std::env::var(name).is_ok_and(|v| !v.is_empty())).map(|name| json!({"type": "env", "name": name})));
            json!({"id": provider_id, "name": provider["name"], "methods": methods, "connections": connections})
        })
        .collect())
}

pub async fn integrations(State(state): State<AppState>, loc: Loc) -> ApiResult {
    loc.wrap(&state, integration_list(&state, &loc).await?)
}

fn unknown_integration(integration_id: &str) -> ApiError {
    ApiError::invalid_request("Params", format!("Integration not found: {integration_id}"))
}

pub async fn integration_get(State(state): State<AppState>, loc: Loc, Path(integration_id): Path<String>) -> ApiResult {
    let list = integration_list(&state, &loc).await?;
    let found = list.into_iter().find(|i| i["id"] == integration_id.as_str()).ok_or_else(|| unknown_integration(&integration_id))?;
    loc.wrap(&state, found)
}

fn integration_changed(state: &AppState, loc: &Loc, integration_id: &str) {
    let directory = Some(loc.instance.directory.as_str());
    state.v2_publish(directory, "integration.connection.updated", json!({"integrationID": integration_id}));
    state.v2_publish(directory, "integration.updated", json!({}));
}

pub async fn connect_key(State(state): State<AppState>, loc: Loc, Path(integration_id): Path<String>, body: Bytes) -> ApiResult {
    let body = payload(&body)?;
    let key = required(&body, "key")?;
    let info: auth::Info = serde_json::from_value(json!({"type": "api", "key": key})).map_err(ApiError::v2_unknown)?;
    auth::set(&state.engine.paths.auth_file(), &integration_id, info).map_err(ApiError::v2_unknown)?;
    integration_changed(&state, &loc, &integration_id);
    no_content()
}

fn attempt_time(attempt: &Attempt) -> Value {
    json!({"created": attempt.created, "expires": attempt.expires})
}

pub async fn connect_oauth(State(state): State<AppState>, loc: Loc, Path(integration_id): Path<String>, body: Bytes) -> ApiResult {
    let body = payload(&body)?;
    let method = required(&body, "methodID")?.parse::<u64>().map_err(|_| ApiError::invalid_request("Payload", "Unknown method at [\"methodID\"]"))?;
    let inputs = body.get("inputs").filter(|i| i.is_object()).cloned();
    let directory = PathBuf::from(&loc.instance.directory);
    let result = state
        .engine
        .provider_auth_authorize(&directory, &integration_id, method, inputs)
        .await
        .map_err(|error| ApiError::invalid_request("Payload", error.to_string()))?
        .ok_or_else(|| ApiError::invalid_request("Payload", "The method did not start an authorization"))?;
    let attempt_id = uuid::Uuid::new_v4().to_string();
    let created = id::now_ms();
    let attempt = Attempt {
        provider_id: integration_id.clone(),
        directory: loc.instance.directory.clone(),
        created,
        expires: created + 10 * 60 * 1000,
        status: "pending".into(),
        message: None,
    };
    let mode = if result["method"] == "code" { "code" } else { "auto" };
    let data = json!({
        "attemptID": attempt_id,
        "url": result["url"].as_str().unwrap_or_default(),
        "instructions": result["instructions"].as_str().unwrap_or_default(),
        "mode": mode,
        "time": attempt_time(&attempt),
    });
    lock(&state.v2.attempts).insert(attempt_id.clone(), attempt);
    if mode == "auto" {
        let waiter = state.clone();
        let task = tokio::spawn(async move {
            let outcome = waiter.engine.provider_auth_callback(&directory, &integration_id, None).await;
            finish_attempt(&waiter, &attempt_id, outcome.map_err(|e| e.to_string()));
        });
        lock(&state.tasks).push(task);
    }
    loc.wrap(&state, data)
}

fn finish_attempt(state: &AppState, attempt_id: &str, outcome: Result<(), String>) {
    let changed = {
        let mut attempts = lock(&state.v2.attempts);
        let Some(attempt) = attempts.get_mut(attempt_id) else { return };
        match outcome {
            Ok(()) => attempt.status = "complete".into(),
            Err(message) => {
                attempt.status = "failed".into();
                attempt.message = Some(message);
            }
        }
        (attempt.directory.clone(), attempt.provider_id.clone())
    };
    state.v2_publish(Some(&changed.0), "integration.connection.updated", json!({"integrationID": changed.1}));
    state.v2_publish(Some(&changed.0), "integration.updated", json!({}));
}

fn unknown_attempt(attempt_id: &str) -> ApiError {
    ApiError::invalid_request("Params", format!("Attempt not found: {attempt_id}"))
}

pub async fn attempt_status(State(state): State<AppState>, loc: Loc, Path(attempt_id): Path<String>) -> ApiResult {
    let data = {
        let attempts = lock(&state.v2.attempts);
        let attempt = attempts.get(&attempt_id).ok_or_else(|| unknown_attempt(&attempt_id))?;
        let expired = attempt.status == "pending" && id::now_ms() > attempt.expires;
        let mut data = json!({"status": if expired { "expired" } else { attempt.status.as_str() }, "time": attempt_time(attempt)});
        if attempt.status == "failed" {
            data["message"] = attempt.message.clone().unwrap_or_default().into();
        }
        data
    };
    loc.wrap(&state, data)
}

pub async fn attempt_cancel(State(state): State<AppState>, Path(attempt_id): Path<String>) -> ApiResult {
    lock(&state.v2.attempts).remove(&attempt_id);
    no_content()
}

pub async fn attempt_complete(State(state): State<AppState>, Path(attempt_id): Path<String>, body: Bytes) -> ApiResult {
    let body = if body.iter().all(u8::is_ascii_whitespace) { json!({}) } else { payload(&body)? };
    let (directory, provider_id) = {
        let attempts = lock(&state.v2.attempts);
        let attempt = attempts.get(&attempt_id).ok_or_else(|| unknown_attempt(&attempt_id))?;
        (PathBuf::from(&attempt.directory), attempt.provider_id.clone())
    };
    let outcome = state.engine.provider_auth_callback(&directory, &provider_id, body["code"].as_str()).await.map_err(|e| e.to_string());
    let failed = outcome.clone().err();
    finish_attempt(&state, &attempt_id, outcome);
    match failed {
        Some(message) => Err(ApiError::invalid_request("Payload", message)),
        None => no_content(),
    }
}

// Stored credentials carry no label here; the update is accepted and changes nothing.
pub async fn credential_update(Path(_credential_id): Path<String>, body: Bytes) -> ApiResult {
    required(&payload(&body)?, "label")?;
    no_content()
}

pub async fn credential_remove(State(state): State<AppState>, loc: Loc, Path(credential_id): Path<String>) -> ApiResult {
    auth::remove(&state.engine.paths.auth_file(), &credential_id).map_err(ApiError::v2_unknown)?;
    integration_changed(&state, &loc, &credential_id);
    no_content()
}

pub fn permission_v2(info: &Value) -> Value {
    let mut out = Map::new();
    out.insert("id".into(), info["id"].clone());
    out.insert("sessionID".into(), info["sessionID"].clone());
    out.insert("action".into(), info["permission"].clone());
    out.insert("resources".into(), info["patterns"].clone());
    out.insert("save".into(), info["always"].clone());
    if info["metadata"].is_object() {
        out.insert("metadata".into(), info["metadata"].clone());
    }
    if info["tool"].is_object() {
        out.insert("source".into(), json!({"type": "tool", "messageID": info["tool"]["messageID"], "callID": info["tool"]["callID"]}));
    }
    Value::Object(out)
}

fn in_directory(state: &AppState, directory: &str, info: &Value) -> bool {
    let session_id = info["sessionID"].as_str().unwrap_or_default();
    state.engine.store.get_session(session_id).ok().flatten().is_some_and(|session| session.directory == directory)
}

pub async fn permission_requests(State(state): State<AppState>, loc: Loc) -> ApiResult {
    let list: Vec<Value> =
        state.pending_permissions().iter().filter(|info| in_directory(&state, &loc.instance.directory, info)).map(permission_v2).collect();
    loc.wrap(&state, list)
}

fn saved_id(permission: &str, pattern: &str) -> String {
    format!("{permission}\0{pattern}").bytes().map(|b| format!("{b:02x}")).collect()
}

pub async fn permission_saved(State(state): State<AppState>, loc: Loc, params: Params) -> ApiResult {
    let project_id = match params.get("projectID") {
        Some(project_id) => project_id.to_string(),
        None => state.project(&loc.instance).map_err(ApiError::into_v2)?.id,
    };
    let list: Vec<Value> = lock(&state.approved)
        .iter()
        .map(|rule| json!({"id": saved_id(&rule.permission, &rule.pattern), "projectID": project_id, "action": rule.permission, "resource": rule.pattern}))
        .collect();
    Ok(json(json!({"data": list})))
}

// The engine's "always" approval goes with it, so the loop asks again.
pub async fn permission_saved_remove(State(state): State<AppState>, Path(saved): Path<String>) -> ApiResult {
    let removed: Vec<permission::Rule> = {
        let mut approved = lock(&state.approved);
        let (gone, kept) = approved.drain(..).partition(|rule| saved_id(&rule.permission, &rule.pattern) == saved);
        *approved = kept;
        gone
    };
    for rule in &removed {
        state.engine.revoke_approval(&rule.permission, &rule.pattern);
    }
    no_content()
}

pub async fn session_permission_create(State(state): State<AppState>, Path(session_id): Path<String>, body: Bytes) -> ApiResult {
    let session = session::require(&state, &session_id)?;
    let body = payload(&body)?;
    let action = required(&body, "action")?.to_string();
    let resources: Vec<String> = body["resources"]
        .as_array()
        .ok_or_else(|| ApiError::invalid_request("Payload", "Expected array at [\"resources\"]"))?
        .iter()
        .filter_map(|r| r.as_str().map(str::to_string))
        .collect();
    let request_id = match body["id"].as_str() {
        Some(given) if given.starts_with("per") => given.to_string(),
        Some(_) => return Err(ApiError::invalid_request("Payload", "Expected a string starting with \"per\" at [\"id\"]")),
        None => id::ascending(Prefix::Permission),
    };
    let rules = opencode_session_rules(&session);
    let effect = {
        let approved = lock(&state.approved);
        let sets: [&[permission::Rule]; 2] = [&rules, &approved];
        let mut effect = "allow";
        for resource in &resources {
            match permission::evaluate(&action, resource, &sets).action {
                Action::Deny => {
                    effect = "deny";
                    break;
                }
                Action::Ask => effect = "ask",
                Action::Allow => {}
            }
        }
        effect
    };
    if effect == "ask" {
        let always: Vec<String> = body["save"].as_array().into_iter().flatten().filter_map(|r| r.as_str().map(str::to_string)).collect();
        let mut info = json!({"id": request_id, "sessionID": session.id, "permission": action, "patterns": resources, "metadata": body.get("metadata").filter(|m| m.is_object()).cloned().unwrap_or_else(|| json!({})), "always": always});
        if body["source"]["type"] == "tool" {
            info["tool"] = json!({"messageID": body["source"]["messageID"], "callID": body["source"]["callID"]});
        }
        let location = state.location(&session);
        // Nothing waits on this request; it is answered through the reply routes like any other.
        let (reply, _) = tokio::sync::oneshot::channel::<PermissionAnswer>();
        lock(&state.pending).push(Pending {
            info: info.clone(),
            session_id: session.id.clone(),
            permission: action,
            patterns: resources,
            always,
            location: location.clone(),
            reply,
        });
        state.v2_publish(Some(&location.directory), "permission.v2.asked", permission_v2(&info));
        state.bus.publish(&location, "permission.asked", info);
    }
    Ok(json(json!({"data": {"id": request_id, "effect": effect}})))
}

fn opencode_session_rules(session: &opencode_storage::Session) -> Vec<permission::Rule> {
    session
        .permission
        .iter()
        .flatten()
        .map(|rule| {
            let action = match rule.action {
                opencode_storage::PermissionAction::Allow => Action::Allow,
                opencode_storage::PermissionAction::Deny => Action::Deny,
                opencode_storage::PermissionAction::Ask => Action::Ask,
            };
            permission::Rule::new(rule.permission.clone(), rule.pattern.clone(), action)
        })
        .collect()
}

fn session_permissions(state: &AppState, session_id: &str) -> Vec<Value> {
    state.pending_permissions().iter().filter(|info| info["sessionID"] == session_id).map(permission_v2).collect()
}

pub async fn session_permission_list(State(state): State<AppState>, Path(session_id): Path<String>) -> ApiResult {
    session::require(&state, &session_id)?;
    Ok(json(json!({"data": session_permissions(&state, &session_id)})))
}

fn missing_permission(request_id: &str) -> ApiError {
    ApiError::v2_not_found("PermissionNotFoundError", "requestID", request_id, format!("Permission request not found: {request_id}"))
}

pub async fn session_permission_get(State(state): State<AppState>, Path((session_id, request_id)): Path<(String, String)>) -> ApiResult {
    session::require(&state, &session_id)?;
    let found = session_permissions(&state, &session_id).into_iter().find(|info| info["id"] == request_id.as_str());
    Ok(json(json!({"data": found.ok_or_else(|| missing_permission(&request_id))?})))
}

pub async fn session_permission_reply(State(state): State<AppState>, Path((session_id, request_id)): Path<(String, String)>, body: Bytes) -> ApiResult {
    session::require(&state, &session_id)?;
    let body = payload(&body)?;
    let reply = match required(&body, "reply")? {
        "once" => opencode_session::Reply::Once,
        "always" => opencode_session::Reply::Always,
        "reject" => opencode_session::Reply::Reject,
        _ => return Err(ApiError::invalid_request("Payload", "Expected \"once\" | \"always\" | \"reject\" at [\"reply\"]")),
    };
    if !session_permissions(&state, &session_id).iter().any(|info| info["id"] == request_id.as_str()) {
        return Err(missing_permission(&request_id));
    }
    state.reply_permission(&request_id, reply, body["message"].as_str()).map_err(|_| missing_permission(&request_id))?;
    no_content()
}

pub async fn question_requests(State(state): State<AppState>, loc: Loc) -> ApiResult {
    let list: Vec<Value> = state.pending_questions().into_iter().filter(|info| in_directory(&state, &loc.instance.directory, info)).collect();
    loc.wrap(&state, list)
}

fn session_questions(state: &AppState, session_id: &str) -> Vec<Value> {
    state.pending_questions().into_iter().filter(|info| info["sessionID"] == session_id).collect()
}

pub async fn session_question_list(State(state): State<AppState>, Path(session_id): Path<String>) -> ApiResult {
    session::require(&state, &session_id)?;
    Ok(json(json!({"data": session_questions(&state, &session_id)})))
}

fn missing_question(request_id: &str) -> ApiError {
    ApiError::v2_not_found("QuestionNotFoundError", "requestID", request_id, format!("Question request not found: {request_id}"))
}

fn answer_question(state: &AppState, session_id: &str, request_id: &str, answers: Option<Vec<Vec<String>>>) -> ApiResult {
    session::require(state, session_id)?;
    if !session_questions(state, session_id).iter().any(|info| info["id"] == request_id) {
        return Err(missing_question(request_id));
    }
    state.reply_question(request_id, answers).map_err(|_| missing_question(request_id))?;
    no_content()
}

pub async fn session_question_reply(State(state): State<AppState>, Path((session_id, request_id)): Path<(String, String)>, body: Bytes) -> ApiResult {
    let body = payload(&body)?;
    let answers: Vec<Vec<String>> = serde_json::from_value(body["answers"].clone()).map_err(|e| ApiError::invalid_request("Payload", format!("{e} at [\"answers\"]")))?;
    answer_question(&state, &session_id, &request_id, Some(answers))
}

pub async fn session_question_reject(State(state): State<AppState>, Path((session_id, request_id)): Path<(String, String)>) -> ApiResult {
    answer_question(&state, &session_id, &request_id, None)
}

pub async fn fs_read(State(_state): State<AppState>, loc: Loc, uri: Uri) -> ApiResult {
    let raw = uri.path().strip_prefix("/api/fs/read/").unwrap_or_default();
    let target = file::resolve(&loc.instance.directory, &decode_component(raw)).map_err(ApiError::into_v2)?;
    if !target.is_file() {
        return Err(ApiError::v2_unknown("Path is not a file"));
    }
    let bytes = tokio::fs::read(&target).await.map_err(ApiError::v2_unknown)?;
    Ok(([(header::CONTENT_TYPE, file::mime_type(&target))], bytes).into_response())
}

fn entry(path: String) -> Value {
    let kind = if path.ends_with(std::path::MAIN_SEPARATOR) { "directory" } else { "file" };
    json!({"path": path, "type": kind})
}

pub async fn fs_list(State(state): State<AppState>, loc: Loc, params: Params) -> ApiResult {
    let directory = std::path::Path::new(&loc.instance.directory);
    let target = file::resolve(&loc.instance.directory, params.get("path").unwrap_or(".")).map_err(ApiError::into_v2)?;
    if !target.is_dir() {
        return Err(ApiError::v2_unknown("Path is not a directory"));
    }
    let mut entries: Vec<(bool, String)> = Vec::new();
    for item in std::fs::read_dir(&target).map_err(ApiError::v2_unknown)?.flatten() {
        let Ok(kind) = item.file_type() else { continue };
        if !kind.is_dir() && !kind.is_file() {
            continue;
        }
        let mut relative = opencode_tools::util::relative(directory, &target.join(item.file_name()));
        if cfg!(windows) {
            relative = relative.replace('/', "\\");
        }
        if kind.is_dir() {
            relative.push(std::path::MAIN_SEPARATOR);
        }
        entries.push((kind.is_dir(), relative));
    }
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| file::locale_cmp(&a.1, &b.1)));
    loc.wrap(&state, entries.into_iter().map(|(_, path)| entry(path)).collect::<Vec<_>>())
}

pub async fn fs_find(State(state): State<AppState>, loc: Loc, params: Params) -> ApiResult {
    let invalid = |reason: &str| ApiError::invalid_request("Query", reason);
    let query = params.get("query").ok_or_else(|| invalid("Missing key at [\"query\"]"))?;
    let kind = match params.get("type") {
        None => None,
        Some(kind @ ("file" | "directory")) => Some(kind),
        Some(_) => return Err(invalid("Expected \"file\" | \"directory\" at [\"type\"]")),
    };
    let limit = match params.get("limit").map(|raw| raw.trim().parse::<usize>()) {
        None => 50,
        Some(Ok(limit)) if limit >= 1 => limit,
        Some(_) => return Err(invalid("Expected a positive integer at [\"limit\"]")),
    };
    let found = file::search(&loc.instance, query, kind, limit).await.map_err(ApiError::into_v2)?;
    loc.wrap(&state, found.into_iter().map(entry).collect::<Vec<_>>())
}

pub async fn commands(State(state): State<AppState>, loc: Loc) -> ApiResult {
    state.mcp(&loc.instance.directory).await.map_err(ApiError::into_v2)?;
    let listed = state.engine.commands(&PathBuf::from(&loc.instance.directory)).await.map_err(|e| ApiError::invalid_request("Query", e.to_string()))?;
    let list: Vec<Value> = listed
        .iter()
        .map(|command| {
            let command = json!(command);
            let mut out = Map::new();
            out.insert("name".into(), command["name"].clone());
            out.insert("template".into(), command["template"].as_str().unwrap_or_default().into());
            copy(&command, &mut out, &[("description", "description"), ("agent", "agent")]);
            if let Some((provider_id, model_id)) = command["model"].as_str().and_then(|m| m.split_once('/')) {
                out.insert("model".into(), json!({"id": model_id, "providerID": provider_id}));
            }
            copy(&command, &mut out, &[("subtask", "subtask")]);
            Value::Object(out)
        })
        .collect();
    loc.wrap(&state, list)
}

pub async fn skills(State(state): State<AppState>, loc: Loc) -> ApiResult {
    let list = crate::routes::instance::skill_values(&state, &loc.instance).map_err(ApiError::into_v2)?;
    let list: Vec<Value> = list
        .iter()
        .map(|skill| {
            let mut out = Map::new();
            copy(skill, &mut out, &[("name", "name"), ("description", "description"), ("location", "location"), ("content", "content")]);
            Value::Object(out)
        })
        .collect();
    loc.wrap(&state, list)
}

// Reference repositories are not ported; a directory without any answers the same.
pub async fn references(State(state): State<AppState>, loc: Loc) -> ApiResult {
    loc.wrap(&state, Vec::<Value>::new())
}

pub async fn not_found(uri: Uri) -> ApiError {
    ApiError::tagged(StatusCode::NOT_FOUND, "NotFoundError", json!({"message": format!("Route not found: {}", uri.path())}))
}

// `data: <json>` frames, `: heartbeat` comments; the stream ends when this subscriber falls 256 events behind.
pub async fn events(State(state): State<AppState>) -> Response {
    let beat = HEARTBEAT.mul_f64(state.heartbeat.as_secs_f64() / crate::routes::event::HEARTBEAT.as_secs_f64());
    sse(event_stream(state.v2.events.subscribe(), beat))
}

fn sse(body: Body) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache, no-transform"),
            (header::HeaderName::from_static("x-accel-buffering"), "no"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        body,
    )
        .into_response()
}

struct Feed {
    first: Option<Value>,
    events: broadcast::Receiver<Arc<Value>>,
    heartbeat: tokio::time::Interval,
}

fn event_stream(events: broadcast::Receiver<Arc<Value>>, beat: Duration) -> Body {
    let mut heartbeat = tokio::time::interval(beat.max(Duration::from_millis(1)));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let feed = Feed { first: Some(Value::Object(envelope(None, "server.connected", json!({})))), events, heartbeat };
    Body::from_stream(stream::unfold(feed, |mut feed| async move {
        if let Some(first) = feed.first.take() {
            return Some((Ok::<_, Infallible>(Bytes::from(format!("data: {first}\n\n"))), feed));
        }
        tokio::select! {
            event = feed.events.recv() => match event {
                Ok(event) => Some((Ok(Bytes::from(format!("data: {event}\n\n"))), feed)),
                Err(RecvError::Lagged(_) | RecvError::Closed) => None,
            },
            _ = feed.heartbeat.tick() => Some((Ok(Bytes::from_static(b": heartbeat\n\n")), feed)),
        }
    }))
}

struct Replay {
    // Weak, so an open stream never keeps a stopped server's state alive.
    state: std::sync::Weak<Inner>,
    backlog: std::collections::VecDeque<Value>,
    live: broadcast::Receiver<Arc<Value>>,
    session_id: String,
    last: Option<u64>,
    read: bool,
}

fn durable_frame(event: &Value) -> Bytes {
    Bytes::from(format!("id: {}\nevent: {}\ndata: {event}\n\n", event["durable"]["seq"], event["type"].as_str().unwrap_or_default()))
}

// A logged `session.next.*` event as `EventV2.readAggregate` decodes it: the unversioned type, no location.
pub fn logged_event(event: &StoredEvent) -> Value {
    let (kind, version) = event.kind.rsplit_once('.').unwrap_or((event.kind.as_str(), "1"));
    let version: u64 = version.parse().unwrap_or(1);
    json!({"id": event.id, "type": kind, "durable": {"aggregateID": event.aggregate_id, "seq": event.seq, "version": version}, "data": event.data})
}

// The session's durable `session.next.*` events after `after` (all when None), read from the store's log.
pub fn session_events(state: &AppState, session_id: &str, after: Option<u64>, limit: Option<usize>) -> ApiResult<Vec<Value>> {
    let logged = state.engine.store.list_events(session_id, after, Some(&events::SESSION_NEXT_DURABLE), limit).map_err(ApiError::v2_unknown)?;
    Ok(logged.iter().map(logged_event).collect())
}

// `EventV2.durable`: the log after `after`, then a re-read whenever the session's next durable event is committed.
pub fn session_stream(state: &AppState, session_id: &str, after: Option<u64>) -> Response {
    let live = state.v2.durable.subscribe();
    let replay = Replay { state: Arc::downgrade(state), backlog: Default::default(), live, session_id: session_id.to_string(), last: after, read: false };
    sse(Body::from_stream(stream::unfold(replay, |mut replay| async move {
        loop {
            if let Some(event) = replay.backlog.pop_front() {
                replay.last = event["durable"]["seq"].as_u64().or(replay.last);
                return Some((Ok::<_, Infallible>(durable_frame(&event)), replay));
            }
            if replay.read {
                match replay.live.recv().await {
                    Ok(event) if event["durable"]["aggregateID"] != replay.session_id.as_str() => continue,
                    Ok(_) | Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => return None,
                }
            }
            replay.read = true;
            let state = replay.state.upgrade()?;
            replay.backlog = session_events(&state, &replay.session_id, replay.last, None).ok()?.into();
        }
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_set_and_dates() {
        let mut unique = EVENT_TYPES.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), 59);
        assert_eq!((released("1970-01-02"), released("2024-03-01"), released("soon")), (86_400_000, 1_709_251_200_000, 0));
    }
}
