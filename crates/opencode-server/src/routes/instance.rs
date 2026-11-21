// Config, agents, commands, paths, projects, TUI events and the routes whose subsystem is not ported yet.
use crate::error::{ApiError, ApiResult, json};
use crate::routes::session::payload;
use crate::state::{AppState, Instance, Params, check_id};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use opencode_core::agent::{self, Agents};
use opencode_core::config::{self, Info};
use opencode_core::skill::{self, DiscoverInput, Registry};
use opencode_core::{auth, flag};
use opencode_storage::{ProjectCommands, ProjectIcon, Session, id};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::OnceLock;

pub async fn health() -> Response {
    json(json!({"healthy": true, "version": opencode_session::VERSION}))
}

static DOC: OnceLock<String> = OnceLock::new();

fn strip_samples(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.shift_remove("x-codeSamples");
            map.values_mut().for_each(strip_samples);
        }
        Value::Array(list) => list.iter_mut().for_each(strip_samples),
        _ => {}
    }
}

// The original document; `/doc` serves it without the generator's `x-codeSamples`.
pub fn openapi() -> &'static str {
    DOC.get_or_init(|| {
        let source = include_str!("../../../../../opencode-ts/packages/sdk/openapi.json");
        let mut document: Value = serde_json::from_str(source).unwrap_or(Value::Null);
        strip_samples(&mut document);
        document.to_string()
    })
}

pub async fn doc() -> Response {
    ([(header::CONTENT_TYPE, "application/json")], openapi()).into_response()
}

pub async fn config_get(State(state): State<AppState>, instance: Instance) -> ApiResult {
    Ok(json(state.load_config(&instance)?.config))
}

pub async fn global_config(State(state): State<AppState>) -> ApiResult {
    Ok(json(config::load_global(&state.engine.paths).map_err(|e| ApiError::config(&e))?))
}

fn config_payload(body: &Bytes) -> ApiResult<Value> {
    let patch: Value = payload(body)?;
    if !patch.is_object() {
        return Err(ApiError::schema("Payload", "Expected object"));
    }
    Info::from_value(&patch).map_err(|e| ApiError::schema("Payload", e.to_string()))?;
    Ok(patch)
}

// Writes `<directory>/config.json`, answers with the payload, then disposes the instance.
pub async fn config_update(State(state): State<AppState>, instance: Instance, body: Bytes) -> ApiResult {
    let patch = config_payload(&body)?;
    config::update_project(&PathBuf::from(&instance.directory), &patch).map_err(|e| ApiError::config(&e))?;
    dispose_later(&state, &instance);
    Ok(json(patch))
}

// Writes the global config file and answers with that file's config; a change disposes every instance.
pub async fn global_config_update(State(state): State<AppState>, body: Bytes) -> ApiResult {
    let patch = config_payload(&body)?;
    let paths = &state.engine.paths;
    let (file, changed) = config::update_global(paths, &patch).map_err(|e| ApiError::config(&e))?;
    let text = std::fs::read_to_string(&file).map_err(ApiError::internal)?;
    let next = config::parse_text(&text, &paths.config, &paths.home, &file.to_string_lossy()).map_err(|e| ApiError::config(&e))?;
    if changed {
        let state = state.clone();
        tokio::spawn(async move {
            state.dispose_all().await;
            state.bus.publish_global(Some("global"), None, "global.disposed", json!({}));
        });
    }
    Ok(json(next))
}

struct Loaded {
    info: Info,
    directories: Vec<PathBuf>,
}

fn load(state: &AppState, instance: &Instance) -> ApiResult<Loaded> {
    let loaded = state.load_config(instance)?;
    let info = loaded.info().map_err(|e| ApiError::config(&e))?;
    Ok(Loaded { info, directories: loaded.directories })
}

fn root(instance: &Instance) -> PathBuf {
    instance.worktree.clone().unwrap_or_else(|| PathBuf::from("/"))
}

pub async fn agents(State(state): State<AppState>, instance: Instance) -> ApiResult {
    let loaded = load(&state, &instance)?;
    let worktree = root(&instance);
    let context = agent::Context { paths: &state.engine.paths, worktree: &worktree, skill_dirs: &[], reference_dirs: &[] };
    Ok(json(Agents::build(&loaded.info, &context).list()))
}

fn skills(state: &AppState, instance: &Instance, loaded: &Loaded) -> ApiResult<Registry> {
    let flags = flag::Flags::from_env().map_err(|e| ApiError::internal(format!("{}={}", e.key, e.value)))?;
    let discovered = skill::discover(&DiscoverInput {
        home: &state.engine.paths.home,
        cwd: &PathBuf::from(&instance.directory),
        worktree: instance.worktree.as_deref(),
        config_dirs: &loaded.directories,
        config: &loaded.info.skills,
        disable_external: flags.disable_external_skills,
        disable_claude_code: flags.disable_claude_code_skills,
    });
    Ok(Registry::load(&discovered))
}

pub async fn commands(State(state): State<AppState>, instance: Instance) -> ApiResult {
    load(&state, &instance)?;
    state.mcp(&instance.directory).await?;
    let commands = state.engine.commands(&PathBuf::from(&instance.directory)).await;
    Ok(json(commands.map_err(|error| ApiError::unknown(StatusCode::BAD_REQUEST, error.to_string()))?))
}

pub fn skill_values(state: &AppState, instance: &Instance) -> ApiResult<Vec<Value>> {
    let loaded = load(state, instance)?;
    Ok(skills(state, instance, &loaded)?.all().into_iter().map(|skill| json!(skill)).collect())
}

pub async fn skill_list(State(state): State<AppState>, instance: Instance) -> ApiResult {
    Ok(json(skill_values(&state, &instance)?))
}

pub async fn path(State(state): State<AppState>, instance: Instance) -> ApiResult {
    let paths = &state.engine.paths;
    Ok(json(json!({
        "home": paths.home.to_string_lossy(),
        "state": paths.state.to_string_lossy(),
        "config": paths.config.to_string_lossy(),
        "worktree": instance.worktree_string(),
        "directory": instance.directory,
    })))
}

pub async fn projects(State(state): State<AppState>, instance: Instance) -> ApiResult {
    state.project(&instance)?;
    Ok(json(state.engine.store.list_projects()?))
}

pub async fn project_current(State(state): State<AppState>, instance: Instance) -> ApiResult {
    Ok(json(state.project(&instance)?))
}

#[derive(Deserialize)]
struct ProjectUpdate {
    name: Option<String>,
    icon: Option<ProjectIcon>,
    commands: Option<ProjectCommands>,
}

pub async fn project_update(State(state): State<AppState>, Path(project_id): Path<String>, body: Bytes) -> ApiResult {
    let body: ProjectUpdate = payload(&body)?;
    let store = &state.engine.store;
    let Some(mut project) = store.get_project(&project_id)? else {
        return Err(ApiError::tagged_not_found("ProjectNotFoundError", "projectID", &project_id, format!("Project not found: {project_id}")));
    };
    // Absent keys leave the column untouched, as an undefined does in the original's update.
    if let Some(name) = body.name {
        project.name = Some(name);
    }
    if let Some(icon) = body.icon {
        let current = project.icon.get_or_insert_default();
        current.url = icon.url.or(current.url.take());
        current.r#override = icon.r#override.or(current.r#override.take());
        current.color = icon.color.or(current.color.take());
    }
    if let Some(commands) = body.commands {
        project.commands = Some(commands);
    }
    project.time.updated = id::now_ms();
    store.upsert_project(&project)?;
    state.bus.publish_global(Some("global"), Some(&project.id), "project.updated", json!(project));
    Ok(json(project))
}

pub async fn log(body: Bytes) -> ApiResult {
    let body: Value = payload(&body)?;
    let field = |key: &str| body[key].as_str().ok_or_else(|| ApiError::schema("Payload", format!("Missing key at [\"{key}\"]")));
    let (service, level, message) = (field("service")?, field("level")?, field("message")?);
    if !matches!(level, "debug" | "info" | "error" | "warn") {
        return Err(ApiError::schema("Payload", "Expected \"debug\" | \"info\" | \"error\" | \"warn\" at [\"level\"]"));
    }
    eprintln!("{level} service={service} {message}");
    Ok(json(true))
}

pub async fn auth_set(State(state): State<AppState>, Path(provider_id): Path<String>, body: Bytes) -> ApiResult {
    let info: auth::Info = payload(&body)?;
    auth::set(&state.engine.paths.auth_file(), &provider_id, info).map_err(ApiError::internal)?;
    Ok(json(true))
}

pub async fn auth_remove(State(state): State<AppState>, Path(provider_id): Path<String>) -> ApiResult {
    auth::remove(&state.engine.paths.auth_file(), &provider_id).map_err(ApiError::internal)?;
    Ok(json(true))
}

pub async fn tool_ids() -> ApiResult {
    Ok(json(opencode_tools::builtin().iter().map(|tool| tool.id().to_string()).collect::<Vec<_>>()))
}

pub async fn tool_list(params: Params) -> ApiResult {
    params.required("provider")?;
    let model = params.required("model")?;
    let patch = opencode_tools::uses_apply_patch(model);
    let tools: Vec<Value> = opencode_tools::builtin()
        .iter()
        .filter(|tool| match tool.id() {
            "apply_patch" => patch,
            "edit" | "write" => !patch,
            _ => true,
        })
        .map(|tool| json!({"id": tool.id(), "description": tool.description(), "parameters": tool.parameters()}))
        .collect();
    Ok(json(tools))
}

pub async fn global_sessions(State(state): State<AppState>, instance: Instance, params: Params) -> ApiResult {
    let limit = params.number("limit")?.map_or(100, |n| n.max(0.0) as usize);
    let roots = params.bool("roots")?.unwrap_or(false);
    let archived = params.bool("archived")?.unwrap_or(false);
    let start = params.number("start")?.filter(|n| *n != 0.0);
    let cursor = params.number("cursor")?.filter(|n| *n != 0.0);
    let search = params.get("search").filter(|s| !s.is_empty()).map(str::to_lowercase);
    let directory = instance.query_directory.then_some(instance.directory.as_str());
    let store = &state.engine.store;
    let mut rows: Vec<(Session, Value)> = Vec::new();
    for project in store.list_projects()? {
        let mut summary = serde_json::Map::new();
        summary.insert("id".into(), project.id.clone().into());
        if let Some(name) = &project.name {
            summary.insert("name".into(), name.clone().into());
        }
        summary.insert("worktree".into(), project.worktree.clone().into());
        for session in store.list_sessions(&project.id)? {
            rows.push((session, Value::Object(summary.clone())));
        }
    }
    rows.retain(|(session, _)| {
        let updated = session.time.updated as f64;
        directory.is_none_or(|d| d == session.directory)
            && (!roots || session.parent_id.is_none())
            && start.is_none_or(|start| updated >= start)
            && cursor.is_none_or(|cursor| updated < cursor)
            && search.as_ref().is_none_or(|needle| session.title.to_lowercase().contains(needle))
            && (archived || session.time.archived.is_none())
    });
    rows.sort_by(|a, b| (b.0.time.updated, &b.0.id).cmp(&(a.0.time.updated, &a.0.id)));
    let more = rows.len() > limit;
    rows.truncate(limit);
    let next = rows.last().filter(|_| more).map(|(session, _)| session.time.updated.to_string());
    let list: Vec<Value> = rows
        .into_iter()
        .map(|(session, project)| {
            let mut value = json!(session);
            value["project"] = project;
            value
        })
        .collect();
    let mut response = json(list);
    if let Some(next) = next.and_then(|n| n.parse().ok()) {
        response.headers_mut().insert("x-next-cursor", next);
    }
    Ok(response)
}

fn tui(state: &AppState, instance: &Instance, kind: &str, properties: Value) -> ApiResult {
    state.bus.publish(&state.instance_location(instance), kind, properties);
    Ok(json(true))
}

fn tui_command(state: &AppState, instance: &Instance, command: Option<&str>) -> ApiResult {
    tui(state, instance, "tui.command.execute", command.map_or_else(|| json!({}), |command| json!({"command": command})))
}

macro_rules! tui_command_route {
    ($name:ident, $command:literal) => {
        pub async fn $name(State(state): State<AppState>, instance: Instance) -> ApiResult {
            tui_command(&state, &instance, Some($command))
        }
    };
}

tui_command_route!(tui_open_help, "help.show");
tui_command_route!(tui_open_sessions, "session.list");
// The original publishes `session.list` here too.
tui_command_route!(tui_open_themes, "session.list");
tui_command_route!(tui_open_models, "model.list");
tui_command_route!(tui_submit_prompt, "prompt.submit");
tui_command_route!(tui_clear_prompt, "prompt.clear");

fn string<'a>(body: &'a Value, key: &str) -> ApiResult<&'a str> {
    body[key].as_str().ok_or_else(|| ApiError::schema("Payload", format!("Expected string at [\"{key}\"]")))
}

pub async fn tui_append_prompt(State(state): State<AppState>, instance: Instance, body: Bytes) -> ApiResult {
    let body: Value = payload(&body)?;
    tui(&state, &instance, "tui.prompt.append", json!({"text": string(&body, "text")?}))
}

pub async fn tui_execute_command(State(state): State<AppState>, instance: Instance, body: Bytes) -> ApiResult {
    let body: Value = payload(&body)?;
    let alias = match string(&body, "command")? {
        "session_new" => Some("session.new"),
        "session_share" => Some("session.share"),
        "session_interrupt" => Some("session.interrupt"),
        "session_compact" => Some("session.compact"),
        "messages_page_up" => Some("session.page.up"),
        "messages_page_down" => Some("session.page.down"),
        "messages_line_up" => Some("session.line.up"),
        "messages_line_down" => Some("session.line.down"),
        "messages_half_page_up" => Some("session.half.page.up"),
        "messages_half_page_down" => Some("session.half.page.down"),
        "messages_first" => Some("session.first"),
        "messages_last" => Some("session.last"),
        "agent_cycle" => Some("agent.cycle"),
        _ => None,
    };
    tui_command(&state, &instance, alias)
}

fn toast(body: &Value) -> ApiResult<Value> {
    string(body, "message")?;
    if !matches!(string(body, "variant")?, "info" | "success" | "warning" | "error") {
        return Err(ApiError::schema("Payload", "Expected \"info\" | \"success\" | \"warning\" | \"error\" at [\"variant\"]"));
    }
    Ok(body.clone())
}

pub async fn tui_show_toast(State(state): State<AppState>, instance: Instance, body: Bytes) -> ApiResult {
    let body: Value = payload(&body)?;
    tui(&state, &instance, "tui.toast.show", toast(&body)?)
}

pub async fn tui_publish(State(state): State<AppState>, instance: Instance, body: Bytes) -> ApiResult {
    let body: Value = payload(&body)?;
    let properties = &body["properties"];
    match string(&body, "type")? {
        "tui.prompt.append" => tui(&state, &instance, "tui.prompt.append", json!({"text": string(properties, "text")?})),
        "tui.command.execute" => tui(&state, &instance, "tui.command.execute", json!({"command": string(properties, "command")?})),
        "tui.toast.show" => tui(&state, &instance, "tui.toast.show", toast(properties)?),
        "tui.session.select" => {
            tui(&state, &instance, "tui.session.select", json!({"sessionID": string(properties, "sessionID")?}))
        }
        _ => Err(ApiError::schema("Payload", "Expected a tui event at [\"type\"]")),
    }
}

pub async fn tui_select_session(State(state): State<AppState>, instance: Instance, body: Bytes) -> ApiResult {
    let body: Value = payload(&body)?;
    let session_id = string(&body, "sessionID")?;
    if !session_id.starts_with("ses") {
        return Err(ApiError::bad_request());
    }
    state.require_session(session_id)?;
    tui(&state, &instance, "tui.session.select", json!({"sessionID": session_id}))
}

// After the response, as the original's dispose middleware does: per-directory state (MCP servers) goes away.
fn dispose_later(state: &AppState, instance: &Instance) {
    let directory = instance.directory.clone();
    let state = state.clone();
    tokio::spawn(async move {
        tokio::task::yield_now().await;
        state.dispose_instance(&directory).await;
    });
}

pub async fn instance_dispose(State(state): State<AppState>, instance: Instance) -> ApiResult {
    dispose_later(&state, &instance);
    Ok(json(true))
}

pub async fn global_dispose(State(state): State<AppState>) -> ApiResult {
    state.dispose_all().await;
    state.bus.publish_global(Some("global"), None, "global.disposed", json!({}));
    Ok(json(true))
}

pub async fn global_upgrade(body: Bytes) -> ApiResult {
    let body: Value = payload(&body)?;
    string(&body, "target")?;
    Ok((StatusCode::BAD_REQUEST, json(json!({"success": false, "error": "Unknown installation method"}))).into_response())
}

fn workspace(state: &AppState, instance: &Instance) -> ApiResult<std::sync::Arc<opencode_session::Workspace>> {
    state.engine.workspace(&PathBuf::from(&instance.directory)).map_err(|error| ApiError::unknown(StatusCode::BAD_REQUEST, error.to_string()))
}

// `LSP.status`: one row per connected client.
pub async fn lsp_status(State(state): State<AppState>, instance: Instance) -> ApiResult {
    Ok(json(workspace(&state, &instance)?.lsp_status()))
}

// `Format.status`: every configured formatter and whether its binary is available.
pub async fn formatter_status(State(state): State<AppState>, instance: Instance) -> ApiResult {
    Ok(json(workspace(&state, &instance)?.formatter_status().await))
}

// The original's handler returns an empty list for every query.
pub async fn find_symbol(params: Params) -> ApiResult {
    params.required("query")?;
    Ok(json(Vec::<Value>::new()))
}

pub async fn capabilities() -> Response {
    json(json!({"backgroundSubagents": false}))
}

pub async fn session_background(Path(session_id): Path<String>) -> ApiResult {
    check_id("ses", "sessionID", &session_id)?;
    Ok(json(false))
}


const API_ROOTS: [&str; 28] = [
    "api", "global", "auth", "log", "doc", "event", "config", "provider", "agent", "command", "skill", "path", "project", "experimental",
    "session", "permission", "question", "instance", "vcs", "file", "find", "lsp", "formatter", "mcp", "pty", "sync", "tui", "openapi.json",
];

const NO_UI: &str = "<!doctype html>\n<html lang=\"en\">\n<head><meta charset=\"utf-8\"><title>opencode server</title></head>\n<body>\n<h1>opencode server</h1>\n<p>This server exposes the opencode HTTP API only; it does not bundle the web UI.</p>\n<p>The API is described at <a href=\"/doc\">/doc</a>.</p>\n</body>\n</html>\n";

// Unknown paths: the original serves its web UI here. This build has none, so a browser navigation gets a short page
// and every other request a JSON 404 (the v2 SDK reads a `text/html` answer as "server too old").
pub async fn fallback(method: axum::http::Method, uri: Uri, headers: axum::http::HeaderMap) -> Response {
    let path = uri.path();
    // With the web UI embedded, every unmatched non-v2 path serves it, as the original's `/*` route does.
    if crate::ui::available() && !path.starts_with("/api/") {
        return crate::ui::serve(path);
    }
    let root = path.trim_start_matches('/').split('/').next().unwrap_or_default();
    let navigation = method == axum::http::Method::GET
        && headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()).is_some_and(|accept| accept.contains("text/html"));
    if navigation && !API_ROOTS.contains(&root) {
        return ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], NO_UI).into_response();
    }
    if path.starts_with("/api/") {
        return crate::v2::not_found(uri).await.into_response();
    }
    ApiError::not_found(format!("Route not found: {path}")).into_response()
}
