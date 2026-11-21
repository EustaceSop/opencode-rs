// Workspaces (`control-plane/workspace.ts` with the built-in worktree adapter), the `/sync/*` routes over the durable
// event log, and `/experimental/control-plane/move-session` (`core/src/control-plane/move-session.ts`).
use crate::error::{ApiError, ApiResult, json};
use crate::routes::session::{delete_session, payload};
use crate::routes::vcs::{GitOutput, apply_patch, canonical, create_from_info, make_worktree_info, parse_worktrees, plain, raw_diff, remove_worktree, spawn};
use crate::state::{AppState, Instance, Params, lock};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use opencode_core::flag;
use opencode_storage::{Project, Session, StoredEvent, Vcs, WorkspaceInfo, id, identity};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path as FsPath, PathBuf};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_millis(5000);

// `RuntimeFlags.experimentalWorkspaces`
pub fn enabled() -> bool {
    flag::effect_bool_or_experimental(&flag::process_env, "OPENCODE_EXPERIMENTAL_WORKSPACES").unwrap_or(false)
}

fn create_error(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, json!({"name": "WorkspaceCreateError", "data": {"message": message.into()}}))
}

fn warp_error(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, json!({"name": "WorkspaceWarpError", "data": {"message": message.into()}}))
}

fn move_error(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, json!({"name": "MoveSessionError", "data": {"message": message.into()}}))
}

// The `{name, data: {message}}` text of a failure raised by the worktree helpers.
fn message_of(error: &ApiError) -> String {
    error.body.pointer("/data/message").and_then(Value::as_str).unwrap_or("Workspace creation failed").to_string()
}

fn text_field(body: &Value, key: &str) -> ApiResult<Option<String>> {
    match &body[key] {
        Value::Null => Ok(None),
        Value::String(text) => Ok(Some(text.clone())),
        _ => Err(ApiError::schema("Payload", format!("Expected string at [\"{key}\"]"))),
    }
}

fn prefixed(body: &Value, key: &str, prefix: &str) -> ApiResult<Option<String>> {
    let value = text_field(body, key)?;
    if value.as_deref().is_some_and(|v| !v.starts_with(prefix)) {
        return Err(ApiError::schema("Payload", format!("Expected a string starting with \"{prefix}\" at [\"{key}\"]")));
    }
    Ok(value)
}

// The routing workspace of the request: `OPENCODE_WORKSPACE_ID`, else `?workspace=`, else `x-opencode-workspace`.
fn routed_workspace(params: &Params, headers: &HeaderMap) -> Option<String> {
    let configured = std::env::var("OPENCODE_WORKSPACE_ID").ok().filter(|id| !id.is_empty());
    let header = headers.get("x-opencode-workspace").and_then(|v| v.to_str().ok()).filter(|v| !v.is_empty()).map(str::to_string);
    configured.or_else(|| params.get("workspace").filter(|w| !w.is_empty()).map(str::to_string)).or(header)
}

impl crate::state::Inner {
    // `setStatus`: a change is announced on the global bus.
    fn set_status(&self, workspace_id: &str, status: &'static str) {
        {
            let mut connections = lock(&self.connections);
            match connections.iter_mut().find(|(id, _)| id == workspace_id) {
                Some((_, current)) if *current == status => return,
                Some((_, current)) => *current = status,
                None => connections.push((workspace_id.to_string(), status)),
            }
        }
        self.bus.publish_global(Some("global"), None, "workspace.status", json!({"workspaceID": workspace_id, "status": status}));
    }

    // `startSync` of a local target: connected while its directory exists.
    fn start_sync(&self, workspace: &WorkspaceInfo) {
        if !enabled() {
            return;
        }
        let exists = workspace.directory.as_deref().is_some_and(|d| FsPath::new(d).exists());
        self.set_status(&workspace.id, if exists { "connected" } else { "error" });
    }
}

// `Workspace.list`: nothing unless workspaces are enabled.
fn listed(state: &AppState, project: &Project) -> ApiResult<Vec<WorkspaceInfo>> {
    if !enabled() {
        return Ok(Vec::new());
    }
    Ok(state.engine.store.list_workspaces(&project.id)?)
}

pub async fn list(State(state): State<AppState>, instance: Instance) -> ApiResult {
    let project = state.project(&instance)?;
    Ok(json(listed(&state, &project)?))
}

// `listAdapters`: the built-in worktree adapter (plugin adapters are not loaded here).
pub async fn adapters() -> Response {
    json(json!([{"type": "worktree", "name": "Worktree", "description": "Create a git worktree"}]))
}

pub async fn status(State(state): State<AppState>, instance: Instance) -> ApiResult {
    let project = state.project(&instance)?;
    let ids: HashSet<String> = listed(&state, &project)?.into_iter().map(|w| w.id).collect();
    let connections = lock(&state.connections);
    let out: Vec<Value> = connections.iter().filter(|(id, _)| ids.contains(id)).map(|(id, status)| json!({"workspaceID": id, "status": status})).collect();
    Ok(json(out))
}

pub async fn create(State(state): State<AppState>, instance: Instance, body: Bytes) -> ApiResult {
    let body: Value = payload(&body)?;
    if !body.is_object() {
        return Err(ApiError::schema("Payload", "Expected object"));
    }
    let kind = text_field(&body, "type")?.ok_or_else(|| ApiError::schema("Payload", "Missing key at [\"type\"]"))?;
    let id = prefixed(&body, "id", "wrk")?.unwrap_or_else(|| id::ascending(id::Prefix::Workspace));
    let branch = text_field(&body, "branch")?;
    let extra = body.get("extra").filter(|e| !e.is_null()).cloned();
    if kind != "worktree" {
        return Err(create_error(format!("Unknown workspace adapter: {kind}")));
    }
    // `WorktreeAdapter.configure`: a detached worktree's name and directory.
    let (project, worktree, mut planned) = make_worktree_info(&state, &instance, None, true).await.map_err(|e| create_error(message_of(&e)))?;
    let info = WorkspaceInfo {
        id,
        kind,
        branch: branch.clone(),
        name: planned.name.clone(),
        directory: Some(planned.directory.to_string_lossy().into_owned()),
        extra,
        project_id: project.id.clone(),
        time_used: id::now_ms(),
    };
    state.engine.store.insert_workspace(&info)?;
    planned.branch = branch.filter(|b| !b.is_empty());
    create_from_info(&state, project, &worktree, &planned, String::new()).await.map_err(|e| create_error(message_of(&e)))?;
    // The workspace's status settles the wait; without the flag nothing ever does.
    if !enabled() {
        tokio::time::sleep(TIMEOUT).await;
        return Err(create_error("Timed out waiting for global event"));
    }
    state.start_sync(&info);
    Ok(json(info))
}

pub async fn remove(State(state): State<AppState>, instance: Instance, Path(workspace_id): Path<String>) -> ApiResult {
    if !workspace_id.starts_with("wrk") {
        return Err(ApiError::schema("Params", format!("Expected a string starting with \"wrk\", got \"{workspace_id}\" at [\"id\"]")));
    }
    let sessions = state.engine.store.list_workspace_sessions(&workspace_id)?;
    let ids: HashSet<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
    for session in sessions.iter().filter(|s| s.parent_id.as_deref().is_none_or(|parent| !ids.contains(parent))) {
        if state.engine.store.get_session(&session.id)?.is_some() {
            delete_session(&state, session)?;
        }
    }
    let Some(info) = state.engine.store.get_workspace(&workspace_id)? else { return Ok(StatusCode::OK.into_response()) };
    lock(&state.connections).retain(|(id, _)| id != &workspace_id);
    // An adapter that cannot remove its workspace is only logged.
    match (&info.kind[..], &info.directory) {
        ("worktree", Some(directory)) => {
            if let Err(error) = remove_worktree(&state, &instance, directory).await {
                tracing::error!(workspace = %workspace_id, message = %message_of(&error), "adapter not available when removing workspace");
            }
        }
        _ => tracing::error!(kind = %info.kind, "adapter not available when removing workspace"),
    }
    state.engine.store.delete_workspace(&workspace_id)?;
    Ok(json(info))
}

// `Workspace.syncList`: worktrees the adapter lists that no workspace has the name of yet become workspaces.
pub async fn sync_list(State(state): State<AppState>, instance: Instance) -> ApiResult {
    let project = state.project(&instance)?;
    let mut names: HashSet<String> = listed(&state, &project)?.into_iter().map(|w| w.name).collect();
    for (name, branch, directory) in worktrees(&project, &instance).await {
        if !names.insert(name.clone()) {
            continue;
        }
        let info = WorkspaceInfo {
            id: id::ascending(id::Prefix::Workspace),
            kind: "worktree".into(),
            branch,
            name,
            directory: Some(directory),
            extra: None,
            project_id: project.id.clone(),
            time_used: id::now_ms(),
        };
        state.engine.store.insert_workspace(&info)?;
        state.start_sync(&info);
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

// `Worktree.list`: the project's linked worktrees as (name, branch, canonical directory); a failure lists none.
async fn worktrees(project: &Project, instance: &Instance) -> Vec<(String, Option<String>, String)> {
    let (Some(worktree), Some(Vcs::Git)) = (&instance.worktree, project.vcs) else { return Vec::new() };
    let result = plain(&["worktree", "list", "--porcelain"], worktree).await;
    if !result.ok() {
        return Vec::new();
    }
    let primary = canonical(&project.worktree);
    let base = |path: &str| FsPath::new(path).file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let primary_name = base(&primary);
    let mut out = Vec::new();
    for (path, branch) in parse_worktrees(&result.text) {
        let directory = canonical(&path);
        if directory == primary {
            continue;
        }
        let mut name = base(&directory);
        if name == primary_name {
            name = FsPath::new(&directory).parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        }
        let branch = branch.map(|b| b.strip_prefix("refs/heads/").unwrap_or(&b).to_string());
        out.push((name, branch, directory));
    }
    out
}

// `Workspace.startWorkspaceSyncing` for the project's workspaces.
pub async fn sync_start(State(state): State<AppState>, instance: Instance) -> ApiResult {
    if enabled() {
        let project = state.project(&instance)?;
        for workspace in state.engine.store.list_workspaces(&project.id)? {
            state.start_sync(&workspace);
        }
    }
    Ok(json(true))
}

// The local directory of a workspace's target (every adapter here is local).
fn target(workspace: &WorkspaceInfo) -> Option<String> {
    workspace.directory.clone().filter(|_| workspace.kind == "worktree")
}

// `Workspace.sessionWarp` with local targets: the session's changes may follow it, then it belongs to the workspace (or
// to none).
pub async fn warp(State(state): State<AppState>, instance: Instance, body: Bytes) -> ApiResult {
    let body: Value = payload(&body)?;
    if !body.is_object() || !body.as_object().is_some_and(|b| b.contains_key("id")) {
        return Err(ApiError::schema("Payload", "Missing key at [\"id\"]"));
    }
    let workspace_id = prefixed(&body, "id", "wrk")?;
    let session_id = prefixed(&body, "sessionID", "ses")?.ok_or_else(|| ApiError::schema("Payload", "Missing key at [\"sessionID\"]"))?;
    let copy = match &body["copyChanges"] {
        Value::Null => false,
        Value::Bool(copy) => *copy,
        _ => return Err(ApiError::schema("Payload", "Expected boolean at [\"copyChanges\"]")),
    };
    let store = &state.engine.store;
    let current = store.get_session(&session_id)?.and_then(|s| s.workspace_id);
    if let Some(previous) = current.as_deref().map(|id| store.get_workspace(id)).transpose()?.flatten() {
        state.abort(&session_id);
        // Later events from the old workspace are ignored.
        store.claim_events(&session_id, workspace_id.as_deref().unwrap_or(&previous.project_id))?;
    }
    let local = |directory: Option<String>| directory.map_or_else(|| instance.clone(), |d| state.instance_for(&d, false));
    let source = match current.as_deref().filter(|_| copy) {
        Some(id) => store.get_workspace(id)?.and_then(|w| target(&w)).map(|d| local(Some(d))),
        None => None,
    };
    let patch = match &source {
        Some(source) => raw_diff(source).await,
        None => String::new(),
    };
    if !patch.is_empty() {
        let destination = match workspace_id.as_deref() {
            None => Some(instance.clone()),
            Some(id) => store.get_workspace(id)?.and_then(|w| target(&w)).map(|d| local(Some(d))),
        };
        if let Some(destination) = destination {
            apply_patch(&destination, &patch).await?;
        }
    }
    let Some(workspace_id) = workspace_id else {
        store.patch_session(&session_id, |s| s.workspace_id = None).map_err(|e| warp_error(e.to_string()))?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    if store.get_workspace(&workspace_id)?.is_none() {
        return Err(ApiError::not_found(format!("Workspace not found: {workspace_id}")));
    }
    store.patch_session(&session_id, |s| s.workspace_id = Some(workspace_id.clone())).map_err(|e| warp_error(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

fn replay_event(value: &Value, index: usize) -> ApiResult<StoredEvent> {
    let bad = |key: &str, what: &str| ApiError::schema("Payload", format!("Expected {what} at [\"events\"][{index}][\"{key}\"]"));
    let text = |key: &str| value[key].as_str().map(str::to_string).ok_or_else(|| bad(key, "string"));
    let id = text("id")?;
    if !id.starts_with("evt_") {
        return Err(bad("id", "a string starting with \"evt_\""));
    }
    let seq = value["seq"].as_u64().ok_or_else(|| bad("seq", "a non-negative integer"))?;
    let data = value.get("data").filter(|d| d.is_object()).cloned().ok_or_else(|| bad("data", "object"))?;
    Ok(StoredEvent { id, aggregate_id: text("aggregateID")?, seq, kind: text("type")?, data })
}

// `EventV2.replayAll` into this instance's log, owned by the routing workspace; replayed rows are not announced.
pub async fn sync_replay(State(state): State<AppState>, params: Params, headers: HeaderMap, body: Bytes) -> ApiResult {
    let body: Value = payload(&body)?;
    text_field(&body, "directory")?.ok_or_else(|| ApiError::schema("Payload", "Missing key at [\"directory\"]"))?;
    let list = body["events"].as_array().ok_or_else(|| ApiError::schema("Payload", "Expected array at [\"events\"]"))?;
    if list.is_empty() {
        return Err(ApiError::schema("Payload", "Expected an array of at least 1 item(s) at [\"events\"]"));
    }
    let events = list.iter().enumerate().map(|(index, value)| replay_event(value, index)).collect::<ApiResult<Vec<_>>>()?;
    let owner = routed_workspace(&params, &headers);
    let source = state.engine.store.replay_events(&events, owner.as_deref(), true)?;
    Ok(json(json!({"sessionID": source})))
}

// Every logged event, minus what the caller already has: `{aggregateID: last seq}`.
pub async fn sync_history(State(state): State<AppState>, body: Bytes) -> ApiResult {
    let body: Value = payload(&body)?;
    let map = body.as_object().ok_or_else(|| ApiError::schema("Payload", "Expected object"))?;
    let mut known = Vec::new();
    for (aggregate, seq) in map {
        let seq = seq.as_u64().ok_or_else(|| ApiError::schema("Payload", format!("Expected a non-negative integer at [\"{aggregate}\"]")))?;
        known.push((aggregate.clone(), seq));
    }
    let rows: Vec<Value> = state.engine.store.event_history(&known)?.iter().map(StoredEvent::to_row).collect();
    Ok(json(rows))
}

fn scope(worktree: &FsPath, path: &FsPath) -> String {
    let relative = opencode_tools::util::relative(worktree, path).replace('\\', "/");
    if relative.is_empty() { ".".into() } else { relative }
}

fn failure(result: &GitOutput, fallback: &str) -> String {
    [result.stderr.trim(), result.text.trim(), fallback].into_iter().find(|t| !t.is_empty()).unwrap_or_default().to_string()
}

// `Git.change.capture`: tracked changes against HEAD, then every untracked file as an addition.
async fn capture(worktree: &FsPath, path: &FsPath) -> Result<String, String> {
    let scope = scope(worktree, path);
    let tracked = plain(&["diff", "--binary", "HEAD", "--", &scope], worktree).await;
    if !tracked.ok() {
        return Err(failure(&tracked, "Failed to capture tracked changes"));
    }
    let untracked = plain(&["ls-files", "--others", "--exclude-standard", "-z", "--", &scope], worktree).await;
    if !untracked.ok() {
        return Err(failure(&untracked, "Failed to list untracked changes"));
    }
    let mut parts = vec![tracked.text];
    for file in untracked.text.split('\0').filter(|f| !f.is_empty()) {
        let created = plain(&["diff", "--binary", "--no-index", "--", "/dev/null", file], worktree).await;
        if !matches!(created.code, 0 | 1) {
            return Err(failure(&created, &format!("Failed to capture untracked change: {file}")));
        }
        parts.push(created.text);
    }
    parts.retain(|part| !part.is_empty());
    Ok(parts.join("\n"))
}

// `Git.change.discard` with the index preserved and untracked files removed.
async fn discard(worktree: &FsPath, path: &FsPath) -> Result<(), String> {
    let scope = scope(worktree, path);
    let restore = plain(&["checkout", "--", &scope], worktree).await;
    if !restore.ok() {
        return Err(failure(&restore, "Failed to restore tracked changes"));
    }
    let clean = plain(&["clean", "-fd", "--", &scope], worktree).await;
    if !clean.ok() {
        return Err(failure(&clean, "Failed to clean untracked changes"));
    }
    Ok(())
}

pub async fn move_session(State(state): State<AppState>, body: Bytes) -> ApiResult {
    let body: Value = payload(&body)?;
    let session_id = prefixed(&body, "sessionID", "ses")?.ok_or_else(|| ApiError::schema("Payload", "Missing key at [\"sessionID\"]"))?;
    let directory = body["destination"]["directory"].as_str().ok_or_else(|| ApiError::schema("Payload", "Expected string at [\"destination\"][\"directory\"]"))?;
    if !FsPath::new(directory).is_absolute() {
        return Err(ApiError::schema("Payload", "Expected an absolute path at [\"destination\"][\"directory\"]"));
    }
    let move_changes = match &body["moveChanges"] {
        Value::Null => false,
        Value::Bool(value) => *value,
        _ => return Err(ApiError::schema("Payload", "Expected boolean at [\"moveChanges\"]")),
    };
    let current: Session = state.engine.store.get_session(&session_id)?.ok_or_else(|| move_error(format!("Session not found: {session_id}")))?;
    if current.directory == directory {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let destination_dir = PathBuf::from(directory);
    let source = identity::resolve(FsPath::new(&current.directory));
    let destination = identity::resolve(&destination_dir);
    if current.project_id != destination.id {
        return Err(move_error("Destination directory belongs to another project"));
    }
    let moving = move_changes && source.directory != destination.directory;
    let repository = if moving { identity::discover(FsPath::new(&current.directory)) } else { None };
    if moving && repository.is_none() {
        return Err(move_error("Source is not a Git repository"));
    }
    let patch = match &repository {
        Some(repository) => capture(&repository.worktree, FsPath::new(&current.directory)).await.map_err(move_error)?,
        None => String::new(),
    };
    let applied = "Unable to apply your changes in the destination directory. The files may conflict with existing changes.";
    if !patch.is_empty() {
        if identity::discover(&destination_dir).is_none() {
            return Err(move_error(applied));
        }
        let all: Vec<&str> = vec!["apply", "-"];
        if !spawn("git", &all, &destination_dir, Some(&patch)).await.ok() {
            return Err(move_error(applied));
        }
    }
    let subdirectory = opencode_tools::util::relative(&destination.directory, &destination_dir).replace('\\', "/");
    let moved = |row: &mut Session| {
        row.directory = directory.to_string();
        row.path = Some(subdirectory.clone());
        row.workspace_id = None;
        row.time.updated = id::now_ms();
    };
    state.engine.store.project_session(&session_id, moved)?;
    let data = json!({"location": {"directory": directory}, "subdirectory": subdirectory});
    state.v2_session_event(directory, &session_id, "session.next.moved", data);
    if !patch.is_empty() {
        let Some(repository) = identity::discover(FsPath::new(&current.directory)) else { return Err(move_error("Source is not a Git repository")) };
        discard(&repository.worktree, FsPath::new(&current.directory)).await.map_err(move_error)?;
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_are_relative_and_slashed() {
        let root = if cfg!(windows) { PathBuf::from(r"C:\repo") } else { PathBuf::from("/repo") };
        assert_eq!(scope(&root, &root), ".");
        assert_eq!(scope(&root, &root.join("a").join("b")), "a/b");
        let output = GitOutput { code: 1, text: " out ".into(), stderr: String::new() };
        assert_eq!(failure(&output, "fallback"), "out");
    }
}
