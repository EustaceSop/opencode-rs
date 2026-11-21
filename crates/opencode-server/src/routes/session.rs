use crate::error::{ApiError, ApiResult, json};
use crate::host::{Job, run_turn};
use crate::state::{AppState, Instance, Params, check_id};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::IntoResponse;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use opencode_session::{CommandInput, CreateSession, PromptInput, Reply, ShellInput};
use opencode_storage::{FilePart, JsonMap, Message, MessageWithParts, Part, PermissionRule, Session, SessionModel, id as idgen};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;

// Payload routes reject a missing or malformed body with the schema-rejection shape.
pub fn payload<T: DeserializeOwned>(body: &Bytes) -> ApiResult<T> {
    serde_json::from_slice(body).map_err(|e| ApiError::schema("Payload", e.to_string()))
}

// Raw-body routes: empty means `{}`; anything undecodable is a handler-raised BadRequest.
fn optional_payload<T: DeserializeOwned + Default>(body: &Bytes) -> ApiResult<T> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice(body).map_err(|_| ApiError::bad_request())
}

pub async fn list(State(state): State<AppState>, instance: Instance, params: Params) -> ApiResult {
    let scope = match params.get("scope") {
        None => None,
        Some("project") => Some("project"),
        Some(other) => return Err(ApiError::schema("Query", format!("Expected \"project\", got \"{other}\" at [\"scope\"]"))),
    };
    let roots = params.bool("roots")?.unwrap_or(false);
    let start = params.number("start")?;
    let limit = params.number("limit")?.map_or(100, |n| n.max(0.0) as usize);
    let search = params.get("search").filter(|s| !s.is_empty()).map(str::to_lowercase);
    let path = params.get("path");
    let directory = instance.query_directory.then_some(instance.directory.as_str());

    let project = state.project(&instance)?;
    let sessions: Vec<Session> = state
        .engine
        .store
        .list_sessions(&project.id)?
        .into_iter()
        .filter(|session| match path {
            Some("") => true,
            Some(path) => {
                let under = session.path.as_deref().is_some_and(|p| p == path || p.starts_with(&format!("{path}/")));
                under || (session.path.is_none() && directory.is_some_and(|d| d == session.directory))
            }
            None => scope == Some("project") || directory.is_none_or(|d| d == session.directory),
        })
        .filter(|session| !roots || session.parent_id.is_none())
        .filter(|session| start.is_none_or(|start| start == 0.0 || session.time.updated as f64 >= start))
        .filter(|session| search.as_ref().is_none_or(|needle| session.title.to_lowercase().contains(needle)))
        .take(limit)
        .collect();
    Ok(json(sessions))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CreateBody {
    #[serde(rename = "parentID")]
    parent_id: Option<String>,
    title: Option<String>,
    agent: Option<String>,
    model: Option<SessionModel>,
    metadata: Option<JsonMap>,
    permission: Option<Vec<PermissionRule>>,
    #[serde(rename = "workspaceID")]
    workspace_id: Option<String>,
}

pub async fn create(State(state): State<AppState>, instance: Instance, body: Bytes) -> ApiResult {
    let body: CreateBody = optional_payload(&body)?;
    if body.parent_id.as_ref().is_some_and(|id| !id.starts_with("ses")) {
        return Err(ApiError::bad_request());
    }
    let input = CreateSession {
        id: None,
        parent_id: body.parent_id,
        workspace_id: body.workspace_id,
        title: body.title,
        agent: body.agent,
        model: body.model,
        metadata: body.metadata,
        permission: body.permission,
    };
    let session = state.engine.create_session(&PathBuf::from(&instance.directory), input).map_err(|e| engine_error("", e))?;
    Ok(json(session))
}

pub async fn status(State(state): State<AppState>) -> ApiResult {
    Ok(json(state.statuses()))
}

pub async fn get(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    check_id("ses", "sessionID", &id)?;
    Ok(json(state.require_session(&id)?))
}

pub async fn children(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    check_id("ses", "sessionID", &id)?;
    state.require_session(&id)?;
    Ok(json(state.engine.store.list_children(&id)?))
}

fn remove_tree(state: &AppState, session: &Session) -> ApiResult<()> {
    state.abort(&session.id);
    for child in state.engine.store.list_children(&session.id)? {
        remove_tree(state, &child)?;
    }
    state.bus.publish(&state.location(session), "session.deleted", json!({"sessionID": session.id, "info": session}));
    Ok(())
}

pub async fn remove(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    check_id("ses", "sessionID", &id)?;
    let session = state.require_session(&id)?;
    delete_session(&state, &session)?;
    Ok(json(true))
}

// `Session.remove`: the session and its descendants, each announced as deleted.
pub(crate) fn delete_session(state: &AppState, session: &Session) -> ApiResult<()> {
    remove_tree(state, session)?;
    // Rust build: its background shells end with it.
    opencode_tools::bash::background::kill_session(&session.id);
    state.engine.store.delete_session(&session.id)?;
    state.v2_forget(&session.id);
    Ok(())
}

#[derive(Deserialize)]
struct UpdateTime {
    archived: Option<f64>,
}

#[derive(Deserialize)]
struct UpdateBody {
    title: Option<String>,
    metadata: Option<JsonMap>,
    permission: Option<Vec<PermissionRule>>,
    time: Option<UpdateTime>,
}

pub async fn update(State(state): State<AppState>, Path(id): Path<String>, body: Bytes) -> ApiResult {
    check_id("ses", "sessionID", &id)?;
    let body: UpdateBody = payload(&body)?;
    state.require_session(&id)?;
    // Each patch is announced by the store watcher.
    let store = &state.engine.store;
    if let Some(title) = body.title {
        store.patch_session(&id, |s| s.title = title)?;
    }
    if let Some(metadata) = body.metadata {
        store.patch_session(&id, |s| {
            s.metadata = Some(metadata);
            s.time.updated = idgen::now_ms();
        })?;
    }
    if let Some(rules) = body.permission {
        store.patch_session(&id, |s| {
            s.permission.get_or_insert_default().extend(rules);
            s.time.updated = idgen::now_ms();
        })?;
    }
    if let Some(archived) = body.time.and_then(|t| t.archived) {
        store.patch_session(&id, |s| s.time.archived = Some(archived))?;
    }
    Ok(json(state.require_session(&id)?))
}

fn with_parts((info, parts): (Message, Vec<Part>)) -> MessageWithParts {
    MessageWithParts { info, parts }
}

#[derive(Deserialize)]
struct Cursor {
    id: String,
    time: f64,
}

fn decode_cursor(raw: &str) -> Option<Cursor> {
    let bytes = URL_SAFE_NO_PAD.decode(raw.trim_end_matches('=')).ok()?;
    let cursor: Cursor = serde_json::from_slice(&bytes).ok()?;
    (cursor.id.starts_with("msg") && cursor.time >= 0.0 && cursor.time.is_finite()).then_some(cursor)
}

fn js_number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9e15 { (value as i64).into() } else { value.into() }
}

pub async fn messages(
    State(state): State<AppState>,
    Path(id): Path<String>,
    params: Params,
    uri: Uri,
    headers: HeaderMap,
) -> ApiResult {
    check_id("ses", "sessionID", &id)?;
    let limit = match params.number("limit")? {
        None => None,
        Some(n) if n >= 0.0 && n.fract() == 0.0 => Some(n as usize),
        Some(n) => return Err(ApiError::schema("Query", format!("Expected a non-negative integer, got {n} at [\"limit\"]"))),
    };
    let before = params.get("before").filter(|b| !b.is_empty());
    if before.is_some() && limit.is_none() {
        return Err(ApiError::bad_request());
    }
    let before = before.map(|raw| decode_cursor(raw).ok_or_else(ApiError::bad_request)).transpose()?;
    state.require_session(&id)?;
    let rows = state.engine.store.list_messages(&id)?;
    let Some(limit) = limit.filter(|limit| *limit > 0) else {
        return Ok(json(rows.into_iter().map(with_parts).collect::<Vec<_>>()));
    };

    // Newest first by (time.created, id), like the original's page query.
    let mut newest: Vec<(Message, Vec<Part>)> = rows;
    newest.sort_by(|a, b| (b.0.time_created(), b.0.id()).cmp(&(a.0.time_created(), a.0.id())));
    if let Some(cursor) = &before {
        newest.retain(|(message, _)| {
            let created = message.time_created() as f64;
            created < cursor.time || (created == cursor.time && message.id() < cursor.id.as_str())
        });
    }
    let more = newest.len() > limit;
    newest.truncate(limit);
    let tail = newest.last().map(|(message, _)| (message.id().to_string(), message.time_created()));
    newest.reverse();
    let items: Vec<MessageWithParts> = newest.into_iter().map(with_parts).collect();
    let Some((tail_id, tail_time)) = tail.filter(|_| more) else {
        return Ok(json(items));
    };

    let cursor = URL_SAFE_NO_PAD.encode(json!({"id": tail_id, "time": js_number(tail_time as f64)}).to_string());
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("localhost");
    let scheme = headers.get("x-forwarded-proto").and_then(|h| h.to_str().ok()).unwrap_or("http");
    let mut pairs: Vec<(String, String)> = params.0.into_iter().filter(|(k, _)| k != "limit" && k != "before").collect();
    pairs.push(("limit".into(), limit.to_string()));
    pairs.push(("before".into(), cursor.clone()));
    let query: Vec<String> = pairs.iter().map(|(k, v)| format!("{}={}", encode_query(k), encode_query(v))).collect();
    let link = format!("<{scheme}://{host}{}?{}>; rel=\"next\"", uri.path(), query.join("&"));
    let mut response = json(items);
    let out = response.headers_mut();
    out.insert("access-control-expose-headers", "Link, X-Next-Cursor".parse().unwrap());
    if let (Ok(link), Ok(cursor)) = (link.parse(), cursor.parse()) {
        out.insert("link", link);
        out.insert("x-next-cursor", cursor);
    }
    Ok(response)
}

// application/x-www-form-urlencoded serialization, as URLSearchParams does.
fn encode_query(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => out.push(byte as char),
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn require_message(state: &AppState, session_id: &str, message_id: &str) -> ApiResult<(Message, Vec<Part>)> {
    state
        .engine
        .store
        .get_message(message_id)?
        .filter(|(message, _)| message.session_id() == session_id)
        .ok_or_else(|| ApiError::not_found(format!("Message not found: {message_id}")))
}

pub async fn message(State(state): State<AppState>, Path((id, message_id)): Path<(String, String)>) -> ApiResult {
    check_id("ses", "sessionID", &id)?;
    check_id("msg", "messageID", &message_id)?;
    Ok(json(with_parts(require_message(&state, &id, &message_id)?)))
}

#[derive(Deserialize)]
struct PromptModel {
    #[serde(rename = "providerID")]
    provider_id: String,
    #[serde(rename = "modelID")]
    model_id: String,
}

#[derive(Deserialize)]
struct PromptBody {
    #[serde(rename = "messageID")]
    message_id: Option<String>,
    model: Option<PromptModel>,
    agent: Option<String>,
    #[serde(rename = "noReply")]
    no_reply: Option<bool>,
    tools: Option<BTreeMap<String, bool>>,
    system: Option<String>,
    variant: Option<String>,
    parts: Vec<Value>,
}

fn file_part(part: &Value) -> ApiResult<FilePart> {
    serde_json::from_value(part.clone()).map_err(|e| ApiError::schema("Payload", format!("{e} at [\"parts\"]")))
}

fn prompt_job(session: &Session, body: &Bytes) -> ApiResult<Job> {
    let body: PromptBody = payload(body)?;
    if body.message_id.as_ref().is_some_and(|id| !id.starts_with("msg")) {
        return Err(ApiError::schema("Payload", "Expected a string starting with \"msg\" at [\"messageID\"]"));
    }
    let mut text = Vec::new();
    let mut files = Vec::new();
    for part in &body.parts {
        match part["type"].as_str() {
            Some("text") => {
                let value = part["text"].as_str().ok_or_else(|| ApiError::schema("Payload", "Missing key at [\"parts\"][\"text\"]"))?;
                text.push(value.to_string());
            }
            Some("file") => files.push(file_part(part)?),
            // The engine takes agent mentions and subtasks from the text and from commands only.
            Some("agent" | "subtask") => {}
            _ => return Err(ApiError::schema("Payload", "Expected a text, file, agent or subtask part at [\"parts\"]")),
        }
    }
    #[allow(clippy::needless_update)]
    let input = PromptInput {
        directory: PathBuf::from(&session.directory),
        session_id: Some(session.id.clone()),
        text: text.join("\n"),
        model: body.model.map(|m| format!("{}/{}", m.provider_id, m.model_id)),
        agent: body.agent,
        files,
        system: body.system,
        variant: body.variant,
        tools: body.tools,
        no_reply: body.no_reply.unwrap_or(false),
        message_id: body.message_id,
        ..Default::default()
    };
    Ok(Job::Prompt(Box::new(input)))
}

// Detached, so a dropped connection does not cancel the turn.
async fn run(state: &AppState, session: Session, job: Job) -> ApiResult<Option<(Message, Vec<Part>)>> {
    tokio::spawn(run_turn(state.clone(), session, job)).await.map_err(ApiError::internal)?
}

pub async fn prompt(State(state): State<AppState>, Path(id): Path<String>, body: Bytes) -> ApiResult {
    check_id("ses", "sessionID", &id)?;
    let session = state.require_session(&id)?;
    let job = prompt_job(&session, &body)?;
    let row = run(&state, session, job).await?.ok_or_else(ApiError::bad_request)?;
    Ok(json(with_parts(row)))
}

pub async fn prompt_async(State(state): State<AppState>, Path(id): Path<String>, body: Bytes) -> ApiResult {
    check_id("ses", "sessionID", &id)?;
    let session = state.require_session(&id)?;
    let job = prompt_job(&session, &body)?;
    tokio::spawn(run_turn(state.clone(), session, job));
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct CommandBody {
    #[serde(rename = "messageID")]
    message_id: Option<String>,
    agent: Option<String>,
    model: Option<String>,
    arguments: String,
    command: String,
    variant: Option<String>,
    parts: Option<Vec<Value>>,
}

pub async fn command(State(state): State<AppState>, Path(id): Path<String>, body: Bytes) -> ApiResult {
    check_id("ses", "sessionID", &id)?;
    let body: CommandBody = payload(&body)?;
    let session = state.require_session(&id)?;
    let files = body.parts.iter().flatten().map(file_part).collect::<ApiResult<Vec<_>>>()?;
    #[allow(clippy::needless_update)]
    let input = CommandInput {
        directory: PathBuf::from(&session.directory),
        session_id: Some(session.id.clone()),
        command: body.command,
        arguments: body.arguments,
        agent: body.agent,
        model: body.model,
        variant: body.variant,
        files,
        message_id: body.message_id,
        ..Default::default()
    };
    let row = run(&state, session, Job::Command(Box::new(input))).await?.ok_or_else(ApiError::bad_request)?;
    Ok(json(with_parts(row)))
}

pub async fn init(State(state): State<AppState>, Path(id): Path<String>, body: Bytes) -> ApiResult {
    check_id("ses", "sessionID", &id)?;
    let body: Value = payload(&body)?;
    let field = |key: &str| body[key].as_str().map(str::to_string).ok_or_else(|| ApiError::schema("Payload", format!("Missing key at [\"{key}\"]")));
    let (model_id, provider_id, message_id) = (field("modelID")?, field("providerID")?, field("messageID")?);
    let session = state.require_session(&id)?;
    #[allow(clippy::needless_update)]
    let input = CommandInput {
        directory: PathBuf::from(&session.directory),
        session_id: Some(session.id.clone()),
        command: opencode_core::command::INIT.to_string(),
        model: Some(format!("{provider_id}/{model_id}")),
        message_id: Some(message_id),
        ..Default::default()
    };
    run(&state, session, Job::Command(Box::new(input))).await?;
    Ok(json(true))
}

pub async fn summarize(State(state): State<AppState>, Path(id): Path<String>, body: Bytes) -> ApiResult {
    check_id("ses", "sessionID", &id)?;
    let body: Value = payload(&body)?;
    for key in ["providerID", "modelID"] {
        if !body[key].is_string() {
            return Err(ApiError::schema("Payload", format!("Missing key at [\"{key}\"]")));
        }
    }
    let session = state.require_session(&id)?;
    run(&state, session, Job::Summarize { auto: body["auto"].as_bool().unwrap_or(false) }).await?;
    Ok(json(true))
}

fn engine_error(session_id: &str, error: opencode_session::Error) -> ApiError {
    match error {
        opencode_session::Error::Busy(_) => ApiError::busy(session_id),
        opencode_session::Error::Storage(error) => error.into(),
        _ => ApiError::bad_request(),
    }
}

pub async fn revert(State(state): State<AppState>, Path(id): Path<String>, body: Bytes) -> ApiResult {
    check_id("ses", "sessionID", &id)?;
    let body: Value = payload(&body)?;
    let message_id = body["messageID"]
        .as_str()
        .filter(|m| m.starts_with("msg"))
        .ok_or_else(|| ApiError::schema("Payload", "Expected a string starting with \"msg\" at [\"messageID\"]"))?;
    state.require_session(&id)?;
    let session = state.engine.revert_files(&id, message_id, body["partID"].as_str()).await.map_err(|e| engine_error(&id, e))?;
    Ok(json(session))
}

pub async fn unrevert(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    check_id("ses", "sessionID", &id)?;
    state.require_session(&id)?;
    let session = state.engine.unrevert_files(&id).await.map_err(|e| engine_error(&id, e))?;
    Ok(json(session))
}

pub async fn question_list(State(state): State<AppState>) -> ApiResult {
    Ok(json(state.pending_questions()))
}

#[derive(Deserialize)]
struct QuestionReply {
    answers: Vec<Vec<String>>,
}

pub async fn question_reply(State(state): State<AppState>, Path(request_id): Path<String>, body: Bytes) -> ApiResult {
    check_id("que", "requestID", &request_id)?;
    let body: QuestionReply = payload(&body)?;
    state.reply_question(&request_id, Some(body.answers))?;
    Ok(json(true))
}

pub async fn question_reject(State(state): State<AppState>, Path(request_id): Path<String>) -> ApiResult {
    check_id("que", "requestID", &request_id)?;
    state.reply_question(&request_id, None)?;
    Ok(json(true))
}

pub async fn abort(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    check_id("ses", "sessionID", &id)?;
    state.abort(&id);
    Ok(json(true))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ForkBody {
    #[serde(rename = "messageID")]
    message_id: Option<String>,
}

pub async fn fork(State(state): State<AppState>, Path(id): Path<String>, body: Bytes) -> ApiResult {
    check_id("ses", "sessionID", &id)?;
    let body: ForkBody = optional_payload(&body)?;
    state.require_session(&id)?;
    Ok(json(state.engine.fork(&id, body.message_id.as_deref()).map_err(|e| engine_error(&id, e))?))
}

fn reply_kind(value: &Value, key: &str) -> ApiResult<Reply> {
    match value[key].as_str() {
        Some("once") => Ok(Reply::Once),
        Some("always") => Ok(Reply::Always),
        Some("reject") => Ok(Reply::Reject),
        _ => Err(ApiError::schema("Payload", format!("Expected \"once\" | \"always\" | \"reject\" at [\"{key}\"]"))),
    }
}

pub async fn permission_respond(
    State(state): State<AppState>,
    Path((id, permission_id)): Path<(String, String)>,
    body: Bytes,
) -> ApiResult {
    check_id("ses", "sessionID", &id)?;
    check_id("per", "permissionID", &permission_id)?;
    let reply = reply_kind(&payload::<Value>(&body)?, "response")?;
    state.require_session(&id)?;
    state.reply_permission(&permission_id, reply, None)?;
    Ok(json(true))
}

pub async fn permission_list(State(state): State<AppState>) -> ApiResult {
    Ok(json(state.pending_permissions()))
}

pub async fn permission_reply(State(state): State<AppState>, Path(request_id): Path<String>, body: Bytes) -> ApiResult {
    check_id("per", "requestID", &request_id)?;
    let body: Value = payload(&body)?;
    let reply = reply_kind(&body, "reply")?;
    let message = match &body["message"] {
        Value::Null => None,
        Value::String(message) => Some(message.as_str()),
        _ => return Err(ApiError::schema("Payload", "Expected string at [\"message\"]")),
    };
    state.reply_permission(&request_id, reply, message)?;
    Ok(json(true))
}

// Row removals and updates below are announced by the store watcher.
pub async fn delete_message(State(state): State<AppState>, Path((id, message_id)): Path<(String, String)>) -> ApiResult {
    check_id("ses", "sessionID", &id)?;
    check_id("msg", "messageID", &message_id)?;
    state.require_session(&id)?;
    if state.is_busy(&id) {
        return Err(ApiError::busy(&id));
    }
    state.engine.store.delete_message(&message_id)?;
    Ok(json(true))
}

pub async fn delete_part(State(state): State<AppState>, Path((id, message_id, part_id)): Path<(String, String, String)>) -> ApiResult {
    check_id("ses", "sessionID", &id)?;
    check_id("msg", "messageID", &message_id)?;
    check_id("prt", "partID", &part_id)?;
    state.require_session(&id)?;
    state.engine.store.delete_part(&part_id)?;
    Ok(json(true))
}

pub async fn update_part(
    State(state): State<AppState>,
    Path((id, message_id, part_id)): Path<(String, String, String)>,
    body: Bytes,
) -> ApiResult {
    check_id("ses", "sessionID", &id)?;
    check_id("msg", "messageID", &message_id)?;
    check_id("prt", "partID", &part_id)?;
    let part: Part = payload(&body)?;
    state.require_session(&id)?;
    if part.id != part_id || part.message_id != message_id || part.session_id != id {
        return Err(ApiError::bad_request());
    }
    require_message(&state, &id, &message_id)?;
    state.engine.store.upsert_part(&part)?;
    Ok(json(part))
}

#[derive(Deserialize)]
struct ShellBody {
    #[serde(rename = "messageID")]
    message_id: Option<String>,
    agent: String,
    model: Option<PromptModel>,
    command: String,
}

pub async fn shell(State(state): State<AppState>, Path(id): Path<String>, body: Bytes) -> ApiResult {
    check_id("ses", "sessionID", &id)?;
    let body: ShellBody = payload(&body)?;
    let session = state.require_session(&id)?;
    if state.is_busy(&id) {
        return Err(ApiError::busy(&id));
    }
    #[allow(clippy::needless_update)]
    let input = ShellInput {
        session_id: id.clone(),
        agent: body.agent,
        model: body.model.map(|m| format!("{}/{}", m.provider_id, m.model_id)),
        command: body.command,
        message_id: body.message_id,
        ..Default::default()
    };
    let row = run(&state, session, Job::Shell(Box::new(input))).await?.ok_or_else(ApiError::bad_request)?;
    Ok(json(with_parts(row)))
}

pub async fn todo(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    check_id("ses", "sessionID", &id)?;
    state.require_session(&id)?;
    Ok(json(state.engine.todos(&id).map_err(|e| engine_error(&id, e))?))
}

pub async fn diff(State(state): State<AppState>, Path(id): Path<String>, params: Params) -> ApiResult {
    check_id("ses", "sessionID", &id)?;
    let message_id = params.get("messageID");
    if let Some(message_id) = message_id {
        check_id("msg", "messageID", message_id)?;
    }
    Ok(json(state.engine.message_diff(&id, message_id).map_err(|e| engine_error(&id, e))?))
}

