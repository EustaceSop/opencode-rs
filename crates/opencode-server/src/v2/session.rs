// `/api/session*`: v1 rows presented in the v2 shapes, and `session.next.*` events derived from what the engine stores.
use crate::error::{ApiError, ApiResult, json};
use crate::host::{Job, run_turn};
use crate::state::{AppState, Inner, Params, lock};
use crate::v2::{payload, required};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use opencode_session::{CreateSession, PromptInput, Status};
use opencode_storage::{FilePart, Message, Part, Session, SessionModel, id};
use serde_json::{Map, Value, json};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    Started,
    Called,
    Ended,
}

fn missing(session_id: &str) -> ApiError {
    ApiError::v2_not_found("SessionNotFoundError", "sessionID", session_id, format!("Session not found: {session_id}"))
}

// `SessionLocationMiddleware`: a malformed id is a 400, an unknown one a 404.
pub fn require(state: &AppState, session_id: &str) -> ApiResult<Session> {
    if !session_id.starts_with("ses") {
        let fields = json!({"message": "Invalid session ID", "field": "sessionID"});
        return Err(ApiError::tagged(StatusCode::BAD_REQUEST, "InvalidRequestError", fields));
    }
    state.engine.store.get_session(session_id).map_err(ApiError::v2_unknown)?.ok_or_else(|| missing(session_id))
}

fn no_content() -> ApiResult {
    Ok(StatusCode::NO_CONTENT.into_response())
}

fn tokens(value: &Value) -> Value {
    let number = |pointer: &str| value.pointer(pointer).filter(|n| n.is_number()).cloned().unwrap_or_else(|| 0.into());
    json!({"input": number("/input"), "output": number("/output"), "reasoning": number("/reasoning"), "cache": {"read": number("/cache/read"), "write": number("/cache/write")}})
}

pub fn info(session: &Session) -> Value {
    let row = json!(session);
    let mut out = Map::new();
    out.insert("id".into(), row["id"].clone());
    for (source, target) in [("parentID", "parentID"), ("projectID", "projectID"), ("agent", "agent"), ("model", "model")] {
        if let Some(value) = row.get(source).filter(|v| !v.is_null()) {
            out.insert(target.into(), value.clone());
        }
    }
    out.insert("cost".into(), row.get("cost").filter(|c| c.is_number()).cloned().unwrap_or_else(|| 0.into()));
    out.insert("tokens".into(), tokens(&row["tokens"]));
    let mut time = json!({"created": session.time.created, "updated": session.time.updated});
    if let Some(archived) = row["time"].get("archived").filter(|a| a.is_number()) {
        time["archived"] = archived.clone();
    }
    out.insert("time".into(), time);
    out.insert("title".into(), row["title"].clone());
    let mut location = json!({"directory": session.directory});
    if let Some(workspace) = &session.workspace_id {
        location["workspaceID"] = workspace.clone().into();
    }
    out.insert("location".into(), location);
    if let Some(path) = session.path.as_deref().filter(|p| !p.is_empty()) {
        out.insert("subpath".into(), path.into());
    }
    if let Some(revert) = row.get("revert").filter(|r| r.is_object()) {
        out.insert("revert".into(), revert.clone());
    }
    Value::Object(out)
}

fn encode_cursor(value: &Value) -> String {
    URL_SAFE_NO_PAD.encode(value.to_string())
}

fn decode_cursor(raw: &str) -> Option<Value> {
    let bytes = URL_SAFE_NO_PAD.decode(raw.trim_end_matches('=')).ok()?;
    serde_json::from_slice::<Value>(&bytes).ok().filter(Value::is_object)
}

fn invalid_cursor(message: &str) -> ApiError {
    ApiError::tagged(StatusCode::BAD_REQUEST, "InvalidCursorError", json!({"message": message}))
}

fn positive(params: &Params, key: &str, max: usize) -> ApiResult<Option<usize>> {
    match params.get(key) {
        None => Ok(None),
        Some(raw) => match raw.trim().parse::<usize>() {
            Ok(n) if (1..=max).contains(&n) => Ok(Some(n)),
            _ => Err(ApiError::invalid_request("Query", format!("Expected a positive integer, got \"{raw}\" at [\"{key}\"]"))),
        },
    }
}

fn order_param(params: &Params) -> ApiResult<Option<&str>> {
    match params.get("order") {
        None => Ok(None),
        Some(order @ ("asc" | "desc")) => Ok(Some(order)),
        Some(other) => Err(ApiError::invalid_request("Query", format!("Expected \"asc\" | \"desc\", got \"{other}\" at [\"order\"]"))),
    }
}

// Keeps what lies beyond the anchor in list order: after it for `next`, the page before it for `previous`.
fn page<T>(mut items: Vec<T>, anchor: Option<(usize, bool)>, limit: usize) -> Vec<T> {
    match anchor {
        Some((index, true)) => items.drain(..=index).for_each(drop),
        Some((index, false)) => {
            items.truncate(index);
            let excess = items.len().saturating_sub(limit);
            items.drain(..excess);
        }
        None => {}
    }
    items.truncate(limit);
    items
}

pub async fn list(State(state): State<AppState>, params: Params) -> ApiResult {
    let limit = positive(&params, "limit", usize::MAX)?.unwrap_or(50);
    let mut query = Map::new();
    let mut anchor = None;
    match params.get("cursor") {
        Some(raw) => {
            let decoded = decode_cursor(raw).ok_or_else(|| invalid_cursor("Invalid cursor"))?;
            let Value::Object(mut fields) = decoded else { return Err(invalid_cursor("Invalid cursor")) };
            let point = fields.remove("anchor").filter(|a| a["id"].is_string()).ok_or_else(|| invalid_cursor("Invalid cursor"))?;
            anchor = Some((point["id"].as_str().unwrap_or_default().to_string(), point["direction"] != "previous"));
            query = fields;
        }
        None => {
            if let Some(order) = order_param(&params)? {
                query.insert("order".into(), order.into());
            }
            for key in ["workspace", "search", "directory", "project", "subpath"] {
                if let Some(value) = params.get(key) {
                    query.insert(key.into(), value.into());
                }
            }
        }
    }
    let text = |key: &str| query.get(key).and_then(Value::as_str);
    let store = &state.engine.store;
    let mut sessions: Vec<Session> = Vec::new();
    for project in store.list_projects().map_err(ApiError::v2_unknown)? {
        if text("project").is_none_or(|wanted| wanted == project.id) {
            sessions.extend(store.list_sessions(&project.id).map_err(ApiError::v2_unknown)?);
        }
    }
    let search = text("search").map(str::to_lowercase);
    sessions.retain(|session| {
        text("directory").is_none_or(|d| d == session.directory)
            && text("workspace").is_none_or(|w| session.workspace_id.as_deref() == Some(w))
            && text("subpath").is_none_or(|p| session.path.as_deref() == Some(p))
            && search.as_ref().is_none_or(|needle| session.title.to_lowercase().contains(needle))
    });
    sessions.sort_by(|a, b| (a.time.created, &a.id).cmp(&(b.time.created, &b.id)));
    if text("order") != Some("asc") {
        sessions.reverse();
    }
    let anchor = match anchor {
        Some((anchor_id, forward)) => Some((sessions.iter().position(|s| s.id == anchor_id).ok_or_else(|| invalid_cursor("Invalid cursor"))?, forward)),
        None => None,
    };
    let sessions = page(sessions, anchor, limit);
    let cursor = |session: Option<&Session>, direction: &str| {
        session.map(|session| {
            let mut fields = query.clone();
            fields.insert("anchor".into(), json!({"id": session.id, "time": session.time.created, "direction": direction}));
            encode_cursor(&Value::Object(fields))
        })
    };
    let mut cursors = Map::new();
    if let Some(previous) = cursor(sessions.first(), "previous") {
        cursors.insert("previous".into(), previous.into());
    }
    if let Some(next) = cursor(sessions.last(), "next") {
        cursors.insert("next".into(), next.into());
    }
    Ok(json(json!({"data": sessions.iter().map(info).collect::<Vec<_>>(), "cursor": cursors})))
}

fn model_input(value: &Value) -> ApiResult<Option<SessionModel>> {
    if value.is_null() {
        return Ok(None);
    }
    let field = |key: &str| value[key].as_str().map(str::to_string).ok_or_else(|| ApiError::invalid_request("Payload", format!("Expected string at [\"model\"][\"{key}\"]")));
    Ok(Some(SessionModel { id: field("id")?, provider_id: field("providerID")?, variant: value["variant"].as_str().map(str::to_string) }))
}

pub async fn create(State(state): State<AppState>, body: Bytes) -> ApiResult {
    let body = if body.iter().all(u8::is_ascii_whitespace) { json!({}) } else { payload(&body)? };
    if body["id"].as_str().is_some_and(|given| !given.starts_with("ses")) {
        return Err(ApiError::invalid_request("Payload", "Expected a string starting with \"ses\" at [\"id\"]"));
    }
    let directory = body["location"]["directory"].as_str().map_or_else(|| state.cwd.clone(), str::to_string);
    let input = CreateSession {
        id: body["id"].as_str().map(str::to_string),
        workspace_id: body["location"]["workspaceID"].as_str().map(str::to_string),
        agent: body["agent"].as_str().map(str::to_string),
        model: model_input(&body["model"])?,
        ..Default::default()
    };
    let session = state.engine.create_session(&PathBuf::from(directory), input).map_err(|e| ApiError::invalid_request("Payload", e.to_string()))?;
    Ok(json(json!({"data": info(&session)})))
}

pub async fn active(State(state): State<AppState>) -> ApiResult {
    let admitted: Vec<String> = lock(&state.v2.turns).keys().cloned().collect();
    let data: Map<String, Value> = state.engine.busy().into_iter().chain(admitted).map(|session_id| (session_id, json!({"type": "running"}))).collect();
    Ok(json(json!({"data": data})))
}

pub async fn get(State(state): State<AppState>, Path(session_id): Path<String>) -> ApiResult {
    Ok(json(json!({"data": info(&require(&state, &session_id)?)})))
}

pub async fn switch_agent(State(state): State<AppState>, Path(session_id): Path<String>, body: Bytes) -> ApiResult {
    let session = require(&state, &session_id)?;
    let body = payload(&body)?;
    let agent = required(&body, "agent")?.to_string();
    // The `AgentSwitched` projection; the event is the log entry.
    let projected = |row: &mut Session| {
        row.agent = Some(agent.clone());
        row.time.updated = id::now_ms();
    };
    state.engine.store.project_session(&session_id, projected).map_err(ApiError::v2_unknown)?;
    state.v2_session_event(&session.directory, &session_id, "session.next.agent.switched", json!({"messageID": id::message(), "agent": agent}));
    no_content()
}

pub async fn switch_model(State(state): State<AppState>, Path(session_id): Path<String>, body: Bytes) -> ApiResult {
    let session = require(&state, &session_id)?;
    let body = payload(&body)?;
    let model = model_input(&body["model"])?.ok_or_else(|| ApiError::invalid_request("Payload", "Missing key at [\"model\"]"))?;
    let reference = json!(model);
    let projected = |row: &mut Session| {
        row.model = Some(model);
        row.time.updated = id::now_ms();
    };
    state.engine.store.project_session(&session_id, projected).map_err(ApiError::v2_unknown)?;
    state.v2_session_event(&session.directory, &session_id, "session.next.model.switched", json!({"messageID": id::message(), "model": reference}));
    no_content()
}

fn mime_of(uri: &str) -> String {
    if let Some(rest) = uri.strip_prefix("data:") {
        return rest.split([';', ',']).next().filter(|m| !m.is_empty()).unwrap_or("text/plain").to_string();
    }
    let path = uri.split(['?', '#']).next().unwrap_or(uri);
    if path.ends_with('/') {
        return "application/x-directory".into();
    }
    let extension = path.rsplit('/').next().and_then(|name| name.rsplit_once('.')).map(|(_, e)| e.to_ascii_lowercase());
    match extension.as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("pdf") => "application/pdf",
        _ => "text/plain",
    }
    .to_string()
}

pub async fn prompt(State(state): State<AppState>, Path(session_id): Path<String>, body: Bytes) -> ApiResult {
    let session = require(&state, &session_id)?;
    let body = payload(&body)?;
    let text = body["prompt"]["text"].as_str().ok_or_else(|| ApiError::invalid_request("Payload", "Expected string at [\"prompt\"][\"text\"]"))?;
    let delivery = match body["delivery"].as_str() {
        None => "steer",
        Some(delivery @ ("steer" | "queue")) => delivery,
        Some(_) => return Err(ApiError::invalid_request("Payload", "Expected \"steer\" | \"queue\" at [\"delivery\"]")),
    };
    let message_id = match body["id"].as_str() {
        Some(given) if given.starts_with("msg_") => given.to_string(),
        Some(_) => return Err(ApiError::invalid_request("Payload", "Expected a string starting with \"msg_\" at [\"id\"]")),
        None => id::message(),
    };
    if state.engine.store.get_message(&message_id).map_err(ApiError::v2_unknown)?.is_some() {
        let message = format!("Prompt message ID conflicts with an existing durable record: {message_id}");
        return Err(ApiError::tagged(StatusCode::CONFLICT, "ConflictError", json!({"message": message, "resource": message_id})));
    }
    let mut files = Vec::new();
    let mut stored = Vec::new();
    for file in body["prompt"]["files"].as_array().into_iter().flatten() {
        let uri = file["uri"].as_str().ok_or_else(|| ApiError::invalid_request("Payload", "Expected string at [\"prompt\"][\"files\"][\"uri\"]"))?;
        let mime = mime_of(uri);
        files.push(FilePart { mime: mime.clone(), filename: file["name"].as_str().map(str::to_string), url: uri.to_string(), source: None });
        let mut entry = file.as_object().cloned().unwrap_or_default();
        entry.insert("mime".into(), mime.into());
        stored.push(Value::Object(entry));
    }
    let mut prompt = json!({"text": text});
    if !stored.is_empty() {
        prompt["files"] = stored.into();
    }
    if let Some(agents) = body["prompt"].get("agents").filter(|a| a.is_array()) {
        prompt["agents"] = agents.clone();
    }
    let created = id::now_ms();
    let data = json!({"messageID": message_id, "prompt": prompt, "delivery": delivery});
    let admitted = state.v2_session_event(&session.directory, &session_id, "session.next.prompt.admitted", data.clone());
    let promoted = state.v2_session_event(&session.directory, &session_id, "session.next.prompted", data);
    #[allow(clippy::needless_update)]
    let input = PromptInput {
        directory: PathBuf::from(&session.directory),
        session_id: Some(session_id.clone()),
        text: text.to_string(),
        files,
        message_id: Some(message_id.clone()),
        // Rust build: a queued prompt waits for the session to go idle; a steer joins the running loop.
        delivery: if delivery == "queue" { opencode_session::Delivery::Queue } else { opencode_session::Delivery::Steer },
        ..Default::default()
    };
    admit(&state, session, Job::Prompt(Box::new(input)));
    Ok(json(json!({"data": {
        "admittedSeq": admitted,
        "id": message_id,
        "sessionID": session_id,
        "prompt": prompt,
        "delivery": delivery,
        "timeCreated": created,
        "promotedSeq": promoted,
    }})))
}

// Counts a turn as in flight from admission until it has run, so `wait` and `active` see it before the engine does.
struct Turn {
    state: AppState,
    session_id: String,
}

impl Drop for Turn {
    fn drop(&mut self) {
        let mut turns = lock(&self.state.v2.turns);
        if let Some(count) = turns.get_mut(&self.session_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                turns.remove(&self.session_id);
            }
        }
    }
}

fn admit(state: &AppState, session: Session, job: Job) {
    *lock(&state.v2.turns).entry(session.id.clone()).or_default() += 1;
    let turn = Turn { state: state.clone(), session_id: session.id.clone() };
    let task = tokio::spawn({
        let state = state.clone();
        async move {
            let _turn = turn;
            let _ = run_turn(state, session, job).await;
        }
    });
    let mut tasks = lock(&state.tasks);
    tasks.retain(|task| !task.is_finished());
    tasks.push(task);
}

pub async fn compact(State(state): State<AppState>, Path(session_id): Path<String>) -> ApiResult {
    let session = require(&state, &session_id)?;
    admit(&state, session, Job::Summarize { auto: false });
    no_content()
}

pub async fn wait(State(state): State<AppState>, Path(session_id): Path<String>) -> ApiResult {
    require(&state, &session_id)?;
    while lock(&state.v2.turns).contains_key(&session_id) || state.engine.status(&session_id) != Status::Idle {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    no_content()
}

fn revert_state(session: &Session) -> Value {
    session.revert.as_ref().map_or(Value::Null, |revert| json!(revert))
}

pub async fn revert_stage(State(state): State<AppState>, Path(session_id): Path<String>, body: Bytes) -> ApiResult {
    let session = require(&state, &session_id)?;
    let body = payload(&body)?;
    let message_id = required(&body, "messageID")?;
    if !message_id.starts_with("msg_") {
        return Err(ApiError::invalid_request("Payload", "Expected a string starting with \"msg_\" at [\"messageID\"]"));
    }
    let known = state.engine.store.get_message(message_id).map_err(ApiError::v2_unknown)?.is_some_and(|(message, _)| message.session_id() == session_id);
    if !known {
        let fields = json!({"sessionID": session_id, "messageID": message_id, "message": format!("Message not found: {message_id}")});
        return Err(ApiError::tagged(StatusCode::NOT_FOUND, "MessageNotFoundError", fields));
    }
    let next = state.engine.revert_files(&session_id, message_id, None).await.map_err(|error| match error {
        opencode_session::Error::Busy(_) => ApiError::tagged(StatusCode::CONFLICT, "ConflictError", json!({"message": format!("Session is busy: {session_id}")})),
        other => ApiError::v2_unknown(other),
    })?;
    let mut revert = revert_state(&next);
    if revert.is_null() {
        revert = json!({"messageID": message_id});
    }
    state.v2_session_event(&session.directory, &session_id, "session.next.revert.staged", json!({"revert": revert}));
    Ok(json(json!({"data": revert})))
}

pub async fn revert_clear(State(state): State<AppState>, Path(session_id): Path<String>) -> ApiResult {
    let session = require(&state, &session_id)?;
    state.engine.unrevert_files(&session_id).await.map_err(ApiError::v2_unknown)?;
    state.v2_session_event(&session.directory, &session_id, "session.next.revert.cleared", json!({}));
    no_content()
}

// Deletes what the staged revert hides (`SessionRevert.cleanup`), as the engine does before the next prompt.
pub async fn revert_commit(State(state): State<AppState>, Path(session_id): Path<String>) -> ApiResult {
    let session = require(&state, &session_id)?;
    let Some(revert) = &session.revert else { return no_content() };
    state.engine.revert_cleanup(&session).map_err(ApiError::v2_unknown)?;
    state.v2_session_event(&session.directory, &session_id, "session.next.revert.committed", json!({"messageID": revert.message_id}));
    no_content()
}

fn tool_content(part: &Value) -> Value {
    let state = &part["state"];
    let input = state.get("input").filter(|i| i.is_object()).cloned().unwrap_or_else(|| json!({}));
    let structured = state.get("metadata").filter(|m| m.is_object()).cloned().unwrap_or_else(|| json!({}));
    let mut time = json!({"created": state["time"]["start"].as_u64().unwrap_or(0)});
    let status = match state["status"].as_str() {
        Some("pending") => json!({"status": "pending", "input": state["raw"].as_str().unwrap_or_default()}),
        Some("running") => {
            time["ran"] = state["time"]["start"].clone();
            json!({"status": "running", "input": input, "structured": structured, "content": []})
        }
        Some("completed") => {
            time["ran"] = state["time"]["start"].clone();
            time["completed"] = state["time"]["end"].clone();
            if let Some(pruned) = state["time"].get("compacted").filter(|c| c.is_u64()) {
                time["pruned"] = pruned.clone();
            }
            json!({"status": "completed", "input": input, "structured": structured, "content": [{"type": "text", "text": state["output"].as_str().unwrap_or_default()}]})
        }
        _ => {
            time["ran"] = state["time"]["start"].clone();
            time["completed"] = state["time"]["end"].clone();
            let error = json!({"type": "unknown", "message": state["error"].as_str().unwrap_or_default()});
            json!({"status": "error", "input": input, "structured": structured, "content": [], "error": error})
        }
    };
    json!({"type": "tool", "id": part["callID"], "name": part["tool"], "state": status, "time": time})
}

// One stored message in the v2 shape; rows with no v2 counterpart (an empty compaction request) give None.
pub fn message_v2(message: &Message, parts: &[Part]) -> Option<Value> {
    let row = json!(message);
    let parts: Vec<Value> = parts.iter().map(|part| json!(part)).collect();
    let texts = |synthetic: bool| -> String {
        let own = parts.iter().filter(|p| p["type"] == "text" && (p["synthetic"] == true) == synthetic);
        own.filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n")
    };
    match message {
        Message::User(user) => {
            if let Some(compaction) = parts.iter().find(|p| p["type"] == "compaction") {
                let reason = if compaction["auto"] == true { "auto" } else { "manual" };
                return Some(json!({"type": "compaction", "reason": reason, "summary": "", "recent": "", "id": user.id, "time": {"created": user.time.created}}));
            }
            let mut out = json!({"id": user.id, "time": {"created": user.time.created}, "text": texts(false), "type": "user"});
            let files: Vec<Value> = parts
                .iter()
                .filter(|p| p["type"] == "file")
                .map(|p| {
                    let mut file = json!({"uri": p["url"], "mime": p["mime"]});
                    if let Some(name) = p["filename"].as_str() {
                        file["name"] = name.into();
                    }
                    file
                })
                .collect();
            if !files.is_empty() {
                out["files"] = files.into();
            }
            let agents: Vec<Value> = parts.iter().filter(|p| p["type"] == "agent").map(|p| json!({"name": p["name"]})).collect();
            if !agents.is_empty() {
                out["agents"] = agents.into();
            }
            Some(out)
        }
        Message::Assistant(_) => {
            let mut time = json!({"created": row["time"]["created"]});
            if let Some(completed) = row["time"].get("completed").filter(|c| c.is_u64()) {
                time["completed"] = completed.clone();
            }
            if row["summary"] == true {
                return Some(json!({"type": "compaction", "reason": "manual", "summary": texts(false), "recent": "", "id": row["id"], "time": {"created": row["time"]["created"]}}));
            }
            let mut model = json!({"id": row["modelID"], "providerID": row["providerID"]});
            if let Some(variant) = row.get("variant").filter(|v| v.is_string()) {
                model["variant"] = variant.clone();
            }
            let content: Vec<Value> = parts
                .iter()
                .filter_map(|p| match p["type"].as_str() {
                    Some("text") => Some(json!({"type": "text", "id": p["id"], "text": p["text"]})),
                    Some("reasoning") => {
                        let mut reasoning = json!({"type": "reasoning", "id": p["id"], "text": p["text"]});
                        if let Some(start) = p["time"]["start"].as_u64() {
                            reasoning["time"] = json!({"created": start});
                            if let Some(end) = p["time"]["end"].as_u64() {
                                reasoning["time"]["completed"] = end.into();
                            }
                        }
                        Some(reasoning)
                    }
                    Some("tool") => Some(tool_content(p)),
                    _ => None,
                })
                .collect();
            let mut out = json!({"id": row["id"], "time": time, "type": "assistant", "agent": row["agent"], "model": model, "content": content});
            if let Some(finish) = row.get("finish").filter(|f| f.is_string()) {
                out["finish"] = finish.clone();
            }
            out["cost"] = row.get("cost").filter(|c| c.is_number()).cloned().unwrap_or_else(|| 0.into());
            out["tokens"] = tokens(&row["tokens"]);
            if row["error"].is_object() {
                let text = row["error"]["data"]["message"].as_str().or(row["error"]["name"].as_str()).unwrap_or("Unknown error");
                out["error"] = json!({"type": "unknown", "message": text});
            }
            Some(out)
        }
    }
}

fn converted(state: &AppState, session_id: &str) -> ApiResult<Vec<Value>> {
    let rows = state.engine.store.list_messages(session_id).map_err(ApiError::v2_unknown)?;
    Ok(rows.iter().filter_map(|(message, parts)| message_v2(message, parts)).collect())
}

pub async fn context(State(state): State<AppState>, Path(session_id): Path<String>) -> ApiResult {
    require(&state, &session_id)?;
    Ok(json(json!({"data": converted(&state, &session_id)?})))
}

pub async fn message(State(state): State<AppState>, Path((session_id, message_id)): Path<(String, String)>) -> ApiResult {
    require(&state, &session_id)?;
    let found = state
        .engine
        .store
        .get_message(&message_id)
        .map_err(ApiError::v2_unknown)?
        .filter(|(message, _)| message.session_id() == session_id)
        .and_then(|(message, parts)| message_v2(&message, &parts));
    match found {
        Some(message) => Ok(json(json!({"data": message}))),
        None => {
            let fields = json!({"sessionID": session_id, "messageID": message_id, "message": format!("Message not found: {message_id}")});
            Err(ApiError::tagged(StatusCode::NOT_FOUND, "MessageNotFoundError", fields))
        }
    }
}

pub async fn messages(State(state): State<AppState>, Path(session_id): Path<String>, params: Params) -> ApiResult {
    require(&state, &session_id)?;
    let limit = positive(&params, "limit", 200)?.unwrap_or(50);
    let requested = order_param(&params)?;
    let decoded = match params.get("cursor") {
        Some(_) if requested.is_some() => return Err(invalid_cursor("Cursor cannot be combined with order")),
        Some(raw) => {
            let cursor = decode_cursor(raw).ok_or_else(|| invalid_cursor("Invalid cursor"))?;
            let valid = cursor["id"].as_str().is_some_and(|c| c.starts_with("msg_"))
                && matches!(cursor["order"].as_str(), Some("asc" | "desc"))
                && matches!(cursor["direction"].as_str(), Some("previous" | "next"));
            if !valid {
                return Err(invalid_cursor("Invalid cursor"));
            }
            Some(cursor)
        }
        None => None,
    };
    let order = decoded.as_ref().and_then(|c| c["order"].as_str()).or(requested).unwrap_or("desc").to_string();
    let mut list = converted(&state, &session_id)?;
    if order == "desc" {
        list.reverse();
    }
    let anchor = match &decoded {
        Some(cursor) => {
            let index = list.iter().position(|m| m["id"] == cursor["id"]).ok_or_else(|| invalid_cursor("Invalid cursor"))?;
            Some((index, cursor["direction"] == "next"))
        }
        None => None,
    };
    let list = page(list, anchor, limit);
    let cursor = |message: Option<&Value>, direction: &str| message.map(|m| encode_cursor(&json!({"id": m["id"], "order": order, "direction": direction})));
    let mut cursors = Map::new();
    if let Some(previous) = cursor(list.first(), "previous") {
        cursors.insert("previous".into(), previous.into());
    }
    if let Some(next) = cursor(list.last(), "next") {
        cursors.insert("next".into(), next.into());
    }
    Ok(json(json!({"data": list, "cursor": cursors})))
}

// Omitted means everything (`after` -1 in the original).
fn after_param(params: &Params) -> ApiResult<Option<u64>> {
    match params.get("after") {
        None => Ok(None),
        Some(raw) => raw.trim().parse::<u64>().map(Some).map_err(|_| ApiError::invalid_request("Query", format!("Expected a non-negative integer, got \"{raw}\" at [\"after\"]"))),
    }
}

// `EventV2.readAggregate` with the `SessionDurable` manifest: one more row than the page tells whether there are more.
pub async fn history(State(state): State<AppState>, Path(session_id): Path<String>, params: Params) -> ApiResult {
    require(&state, &session_id)?;
    let limit = positive(&params, "limit", 100)?.unwrap_or(50);
    let after = after_param(&params)?;
    let mut events = crate::v2::session_events(&state, &session_id, after, Some(limit + 1))?;
    let more = events.len() > limit;
    events.truncate(limit);
    Ok(json(json!({"data": events, "hasMore": more})))
}

pub async fn events(State(state): State<AppState>, Path(session_id): Path<String>, params: Params) -> Response {
    let checked = require(&state, &session_id).and_then(|_| after_param(&params));
    match checked {
        Ok(after) => crate::v2::session_stream(&state, &session_id, after),
        Err(error) => error.into_response(),
    }
}

pub async fn interrupt(State(state): State<AppState>, Path(session_id): Path<String>) -> ApiResult {
    require(&state, &session_id)?;
    state.abort(&session_id);
    no_content()
}

impl Inner {
    // True the first time a part reaches `stage`.
    fn advance(&self, session_id: &str, part_id: &str, stage: Stage) -> bool {
        let mut stages = lock(&self.v2.stages);
        let known = stages.entry(session_id.to_string()).or_default();
        if known.get(part_id).is_some_and(|seen| *seen >= stage) {
            return false;
        }
        known.insert(part_id.to_string(), stage);
        true
    }

    pub fn v2_forget(&self, session_id: &str) {
        lock(&self.v2.stages).remove(session_id);
    }

    // What a stored assistant part means in `session.next.*` terms; each transition is reported once.
    pub fn v2_part(&self, directory: &str, part: &Part) {
        let Ok(Some((Message::Assistant(_), _))) = self.engine.store.get_message(&part.message_id) else { return };
        let session_id = part.session_id.as_str();
        let value = json!(part);
        let emit = |kind: &str, mut data: Value| {
            data["assistantMessageID"] = part.message_id.clone().into();
            self.v2_session_event(directory, session_id, kind, data);
        };
        let first = |stage: Stage| self.advance(session_id, &part.id, stage);
        match value["type"].as_str() {
            Some("step-start") => {
                if first(Stage::Started) {
                    let message = self.engine.store.get_message(&part.message_id).ok().flatten().map(|(message, _)| json!(message)).unwrap_or_default();
                    let mut data = json!({"agent": message["agent"], "model": {"id": message["modelID"], "providerID": message["providerID"]}});
                    if let Some(snapshot) = value["snapshot"].as_str() {
                        data["snapshot"] = snapshot.into();
                    }
                    emit("session.next.step.started", data);
                }
            }
            Some("step-finish") => {
                if first(Stage::Ended) {
                    let mut data = json!({"finish": value["reason"], "cost": value["cost"], "tokens": tokens(&value["tokens"])});
                    if let Some(snapshot) = value["snapshot"].as_str() {
                        data["snapshot"] = snapshot.into();
                    }
                    emit("session.next.step.ended", data);
                }
            }
            Some(kind @ ("text" | "reasoning")) => {
                let key = if kind == "text" { "textID" } else { "reasoningID" };
                if first(Stage::Started) {
                    emit(&format!("session.next.{kind}.started"), json!({key: part.id}));
                }
                if value["time"]["end"].is_u64() && first(Stage::Ended) {
                    emit(&format!("session.next.{kind}.ended"), json!({key: part.id, "text": value["text"]}));
                }
            }
            Some("tool") => {
                let state = &value["state"];
                let call = &value["callID"];
                let status = state["status"].as_str().unwrap_or_default();
                if first(Stage::Started) {
                    emit("session.next.tool.input.started", json!({"callID": call, "name": value["tool"]}));
                }
                let input = state.get("input").filter(|i| i.is_object()).cloned().unwrap_or_else(|| json!({}));
                if status != "pending" && first(Stage::Called) {
                    emit("session.next.tool.called", json!({"callID": call, "tool": value["tool"], "input": input, "provider": {"executed": false}}));
                }
                if matches!(status, "completed" | "error") && first(Stage::Ended) {
                    let structured = state.get("metadata").filter(|m| m.is_object()).cloned().unwrap_or_else(|| json!({}));
                    if status == "completed" {
                        let content = json!([{"type": "text", "text": state["output"].as_str().unwrap_or_default()}]);
                        emit("session.next.tool.success", json!({"callID": call, "structured": structured, "content": content, "provider": {"executed": false}}));
                    } else {
                        let error = json!({"type": "unknown", "message": state["error"].as_str().unwrap_or_default()});
                        emit("session.next.tool.failed", json!({"callID": call, "error": error, "provider": {"executed": false}}));
                    }
                }
            }
            _ => {}
        }
    }

    // A streamed chunk of a text or reasoning part.
    pub fn v2_delta(&self, directory: &str, session_id: &str, message_id: &str, part_id: &str, kind: &str, delta: &str) {
        let key = if kind == "reasoning" { "reasoningID" } else { "textID" };
        let data = json!({"assistantMessageID": message_id, key: part_id, "delta": delta});
        self.v2_session_event(directory, session_id, &format!("session.next.{kind}.delta"), data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_follow_the_anchor() {
        let items = vec![1, 2, 3, 4, 5];
        assert_eq!(page(items.clone(), None, 2), [1, 2]);
        assert_eq!(page(items.clone(), Some((1, true)), 2), [3, 4]);
        assert_eq!(page(items.clone(), Some((3, false)), 2), [2, 3]);
        assert_eq!(page(items, Some((0, false)), 2), Vec::<i32>::new());
    }

    #[test]
    fn cursors_and_mimes() {
        let cursor = json!({"id": "msg_1", "order": "asc", "direction": "next"});
        assert_eq!(decode_cursor(&encode_cursor(&cursor)), Some(cursor));
        assert_eq!(decode_cursor("!!"), None);
        assert_eq!((mime_of("file:///a/b.PNG"), mime_of("file:///a/b.rs?start=1"), mime_of("data:image/gif;base64,AA"), mime_of("file:///a/")), ("image/png".to_string(), "text/plain".to_string(), "image/gif".to_string(), "application/x-directory".to_string()));
    }
}
