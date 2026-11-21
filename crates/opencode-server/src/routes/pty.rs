// `/pty*` and `/api/pty*`: the same sessions behind two error vocabularies and response envelopes.
use crate::error::{ApiError, ApiResult, json};
use crate::pty::{AttachError, CreateInput, Info, serve_socket, shells};
use crate::state::{AppState, Instance, Params};
use crate::v2::Loc;
use axum::body::Bytes;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

fn not_found(pty_id: &str) -> ApiError {
    ApiError::tagged_not_found("PtyNotFoundError", "ptyID", pty_id, format!("PTY session not found: {pty_id}"))
}

fn check(pty_id: &str, v2: bool) -> ApiResult<()> {
    if pty_id.starts_with("pty") {
        return Ok(());
    }
    let reason = format!("Expected a string starting with \"pty\", got \"{pty_id}\" at [\"ptyID\"]");
    Err(if v2 { ApiError::invalid_request("Params", reason) } else { ApiError::schema("Params", reason) })
}

fn body_json(body: &Bytes, v2: bool) -> ApiResult<Value> {
    let rejected = |reason: String| if v2 { ApiError::invalid_request("Payload", reason) } else { ApiError::schema("Payload", reason) };
    let value: Value = serde_json::from_slice(body).map_err(|e| rejected(e.to_string()))?;
    if value.is_object() { Ok(value) } else { Err(rejected("Expected object".into())) }
}

fn create_input(body: &Value, v2: bool) -> ApiResult<CreateInput> {
    let rejected = |key: &str| {
        let reason = format!("Invalid value at [\"{key}\"]");
        if v2 { ApiError::invalid_request("Payload", reason) } else { ApiError::schema("Payload", reason) }
    };
    let text = |key: &str| match &body[key] {
        Value::Null => Ok(None),
        Value::String(text) => Ok(Some(text.clone())),
        _ => Err(rejected(key)),
    };
    let args = match &body["args"] {
        Value::Null => Vec::new(),
        Value::Array(list) => list.iter().map(|item| item.as_str().map(str::to_string).ok_or_else(|| rejected("args"))).collect::<ApiResult<_>>()?,
        _ => return Err(rejected("args")),
    };
    let env = match &body["env"] {
        Value::Null => Vec::new(),
        Value::Object(map) => {
            map.iter().map(|(key, value)| value.as_str().map(|v| (key.clone(), v.to_string())).ok_or_else(|| rejected("env"))).collect::<ApiResult<_>>()?
        }
        _ => return Err(rejected("env")),
    };
    Ok(CreateInput { command: text("command")?, args, cwd: text("cwd")?, title: text("title")?, env })
}

// The plugins' `shell.env` hook runs first with the terminal's cwd; its variables win over the payload's.
async fn create(state: &AppState, instance: &Instance, body: &Bytes, v2: bool) -> ApiResult<Info> {
    let mut input = create_input(&body_json(body, v2)?, v2)?;
    let shell = state.load_config(instance).ok().and_then(|loaded| loaded.config.get("shell").and_then(Value::as_str).map(str::to_string));
    let failed = |e: String| if v2 { ApiError::v2_unknown(e) } else { ApiError::internal(e) };
    let cwd = input.cwd.clone().filter(|c| !c.is_empty()).unwrap_or_else(|| instance.directory.clone());
    let extra = state.engine.shell_env(std::path::Path::new(&instance.directory), &cwd).await.map_err(|e| failed(e.to_string()))?;
    input.env.extend(extra);
    state.pty.create(&instance.directory, shell.as_deref(), input).map_err(failed)
}

// (title, (rows, cols))
type Update = (Option<String>, Option<(u16, u16)>);

fn update_input(body: &Value, v2: bool) -> ApiResult<Update> {
    let rejected = |reason: &str| if v2 { ApiError::invalid_request("Payload", reason) } else { ApiError::schema("Payload", reason) };
    let title = match &body["title"] {
        Value::Null => None,
        Value::String(title) => Some(title.clone()),
        _ => return Err(rejected("Expected string at [\"title\"]")),
    };
    let size = match &body["size"] {
        Value::Null => None,
        size => {
            let side = |key: &str| size[key].as_u64().filter(|n| (1..=u64::from(u16::MAX)).contains(n)).map(|n| n as u16);
            Some(side("rows").zip(side("cols")).ok_or_else(|| rejected("Expected a positive integer at [\"size\"]"))?)
        }
    };
    Ok((title, size))
}

fn origin_host(origin: &str) -> Option<&str> {
    let rest = origin.split_once("://")?.1;
    Some(rest.split(['/', '?', '#']).next().unwrap_or(rest))
}

// `isAllowedRequestOrigin`: no origin, same host, or an origin the CORS policy allows.
fn allowed_origin(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()).filter(|o| !o.is_empty()) else { return true };
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    origin_host(origin).is_some_and(|name| Some(name) == host) || crate::is_allowed_origin(Some(origin), &state.cors)
}

fn ticket_request(state: &AppState, headers: &HeaderMap) -> bool {
    headers.get("x-opencode-ticket").and_then(|v| v.to_str().ok()) == Some("1") && allowed_origin(state, headers)
}

fn connect(state: AppState, directory: String, pty_id: String, headers: HeaderMap, params: Params, upgrade: Option<WebSocketUpgrade>, v2: bool) -> Response {
    let empty = |status: StatusCode| status.into_response();
    if !allowed_origin(&state, &headers) {
        return empty(StatusCode::FORBIDDEN);
    }
    if let Some(ticket) = params.get("ticket").filter(|t| !t.is_empty())
        && !state.pty.consume_ticket(&directory, &pty_id, ticket)
    {
        return empty(StatusCode::FORBIDDEN);
    }
    // Legacy hides exited sessions; v2 reports them on the socket.
    let exists = state.pty.get(&directory, &pty_id).is_some_and(|info| v2 || info.status == "running");
    if !exists {
        return empty(StatusCode::NOT_FOUND);
    }
    let cursor = params.get("cursor").and_then(|raw| raw.trim().parse::<f64>().ok()).filter(|n| n.fract() == 0.0 && *n >= -1.0 && n.abs() <= 9_007_199_254_740_991.0).map(|n| n as i64);
    let Some(upgrade) = upgrade else { return empty(StatusCode::BAD_REQUEST) };
    upgrade.on_upgrade(move |socket| async move {
        let closing = state.pty.closing();
        let attached = if closing { Err(AttachError::NotFound) } else { state.pty.attach(&directory, &pty_id, cursor) };
        serve_socket(socket, attached, if v2 { "session exited" } else { "session not found" }, closing).await;
    })
}

pub async fn shell_list() -> Response {
    json(shells())
}

pub async fn list(State(state): State<AppState>, instance: Instance) -> Response {
    json(state.pty.list(&instance.directory).into_iter().filter(|info| info.status == "running").collect::<Vec<_>>())
}

pub async fn create_legacy(State(state): State<AppState>, instance: Instance, body: Bytes) -> ApiResult {
    Ok(json(create(&state, &instance, &body, false).await?))
}

fn running(state: &AppState, instance: &Instance, pty_id: &str) -> ApiResult<Info> {
    check(pty_id, false)?;
    state.pty.get(&instance.directory, pty_id).filter(|info| info.status == "running").ok_or_else(|| not_found(pty_id))
}

pub async fn get(State(state): State<AppState>, instance: Instance, Path(pty_id): Path<String>) -> ApiResult {
    Ok(json(running(&state, &instance, &pty_id)?))
}

pub async fn update(State(state): State<AppState>, instance: Instance, Path(pty_id): Path<String>, body: Bytes) -> ApiResult {
    check(&pty_id, false)?;
    let (title, size) = update_input(&body_json(&body, false)?, false)?;
    running(&state, &instance, &pty_id)?;
    Ok(json(state.pty.update(&instance.directory, &pty_id, title.as_deref(), size).ok_or_else(|| not_found(&pty_id))?))
}

pub async fn remove(State(state): State<AppState>, instance: Instance, Path(pty_id): Path<String>) -> ApiResult {
    running(&state, &instance, &pty_id)?;
    if !state.pty.remove(&instance.directory, &pty_id) {
        return Err(not_found(&pty_id));
    }
    Ok(json(true))
}

pub async fn connect_token(State(state): State<AppState>, instance: Instance, Path(pty_id): Path<String>, headers: HeaderMap) -> ApiResult {
    check(&pty_id, false)?;
    if !ticket_request(&state, &headers) {
        return Err(ApiError::tagged(StatusCode::FORBIDDEN, "PtyForbiddenError", json!({"message": "Invalid PTY connect token request"})));
    }
    running(&state, &instance, &pty_id)?;
    Ok(json(state.pty.issue_ticket(&instance.directory, &pty_id)))
}

pub async fn connect_legacy(
    State(state): State<AppState>,
    instance: Instance,
    Path(pty_id): Path<String>,
    headers: HeaderMap,
    params: Params,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    connect(state, instance.directory, pty_id, headers, params, upgrade.ok(), false)
}

fn found(state: &AppState, loc: &Loc, pty_id: &str) -> ApiResult<Info> {
    check(pty_id, true)?;
    state.pty.get(&loc.instance.directory, pty_id).ok_or_else(|| not_found(pty_id))
}

pub async fn v2_list(State(state): State<AppState>, loc: Loc) -> ApiResult {
    loc.wrap(&state, state.pty.list(&loc.instance.directory))
}

pub async fn v2_create(State(state): State<AppState>, loc: Loc, body: Bytes) -> ApiResult {
    loc.wrap(&state, create(&state, &loc.instance, &body, true).await?)
}

pub async fn v2_get(State(state): State<AppState>, loc: Loc, Path(pty_id): Path<String>) -> ApiResult {
    loc.wrap(&state, found(&state, &loc, &pty_id)?)
}

pub async fn v2_update(State(state): State<AppState>, loc: Loc, Path(pty_id): Path<String>, body: Bytes) -> ApiResult {
    check(&pty_id, true)?;
    let (title, size) = update_input(&body_json(&body, true)?, true)?;
    let info = state.pty.update(&loc.instance.directory, &pty_id, title.as_deref(), size).ok_or_else(|| not_found(&pty_id))?;
    loc.wrap(&state, info)
}

pub async fn v2_remove(State(state): State<AppState>, loc: Loc, Path(pty_id): Path<String>) -> ApiResult {
    check(&pty_id, true)?;
    if !state.pty.remove(&loc.instance.directory, &pty_id) {
        return Err(not_found(&pty_id));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub async fn v2_connect_token(State(state): State<AppState>, loc: Loc, Path(pty_id): Path<String>, headers: HeaderMap) -> ApiResult {
    check(&pty_id, true)?;
    if !ticket_request(&state, &headers) {
        return Err(ApiError::tagged(StatusCode::FORBIDDEN, "ForbiddenError", json!({"message": "Invalid PTY connect token request"})));
    }
    found(&state, &loc, &pty_id)?;
    loc.wrap(&state, state.pty.issue_ticket(&loc.instance.directory, &pty_id))
}

pub async fn v2_connect(
    State(state): State<AppState>,
    loc: Loc,
    Path(pty_id): Path<String>,
    headers: HeaderMap,
    params: Params,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    connect(state, loc.instance.directory, pty_id, headers, params, upgrade.ok(), true)
}
