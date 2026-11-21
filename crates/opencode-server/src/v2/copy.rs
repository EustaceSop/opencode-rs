// `/experimental/project/{projectID}/copy*` (`core/src/project/copy.ts` with its git worktree strategy).
use crate::error::{ApiError, ApiResult};
use crate::routes::vcs::{GitOutput, plain};
use crate::state::AppState;
use crate::v2::{Loc, payload, required};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use opencode_session::resolve_directory;
use opencode_storage::{identity, slug};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path as FsPath, PathBuf};

const STRATEGY: &str = "git_worktree";

// `ProjectCopyError`; `forceRequired` only for git worktree failures.
fn copy_error(message: impl Into<String>, force_required: Option<bool>) -> ApiError {
    let mut data = json!({"message": message.into()});
    if let Some(force) = force_required {
        data["forceRequired"] = force.into();
    }
    ApiError::new(StatusCode::BAD_REQUEST, json!({"name": "ProjectCopyError", "data": data}))
}

fn unavailable(directory: &str) -> ApiError {
    copy_error(format!("Project copy directory unavailable: {directory}"), None)
}

// `Git.WorktreeError` of one operation; `remove` says when only `--force` would do.
fn worktree_error(result: &GitOutput, remove: bool) -> ApiError {
    let message = [result.stderr.trim(), result.text.trim(), "Git failed"].into_iter().find(|t| !t.is_empty()).unwrap_or_default().to_string();
    let lower = message.to_lowercase();
    let force = remove && (lower.contains("contains modified or untracked files") || lower.contains("is dirty"));
    copy_error(message, Some(force))
}

// The resolved path of an existing directory.
fn canonical(directory: &str) -> ApiResult<String> {
    let resolved = resolve_directory(FsPath::new(directory));
    if FsPath::new(&resolved).is_dir() { Ok(resolved) } else { Err(unavailable(directory)) }
}

fn absolute(body: &Value, key: &str) -> ApiResult<String> {
    let text = required(body, key)?;
    if !FsPath::new(text).is_absolute() {
        return Err(ApiError::invalid_request("Payload", format!("Expected an absolute path at [\"{key}\"]")));
    }
    Ok(text.to_string())
}

fn strategy(id: &str) -> ApiResult<()> {
    if id.trim() == STRATEGY { Ok(()) } else { Err(copy_error(format!("Project copy strategy unavailable: {}", id.trim()), None)) }
}

fn changed(state: &AppState, loc: &Loc, project_id: &str, update: bool) {
    if update {
        let location = state.instance_location(&loc.instance);
        state.bus.publish(&location, "project.directories.updated", json!({"projectID": project_id}));
    }
}

// `git worktree list` of a repository: (directory, main) per entry, the first being the main one.
async fn list(directory: &str) -> ApiResult<Vec<(String, bool)>> {
    let Some(repository) = identity::discover(FsPath::new(directory)) else { return Err(unavailable(directory)) };
    let result = plain(&["worktree", "list", "--porcelain"], &repository.worktree).await;
    if !result.ok() {
        return Err(worktree_error(&result, false));
    }
    let entries = result.text.lines().filter_map(|line| line.strip_prefix("worktree ")).map(|path| repository.worktree.join(path.trim()));
    Ok(entries.enumerate().filter_map(|(index, path)| canonical(&path.to_string_lossy()).ok().map(|d| (d, index == 0))).collect())
}

pub async fn create(State(state): State<AppState>, loc: Loc, Path(project_id): Path<String>, body: Bytes) -> ApiResult {
    let body = payload(&body)?;
    let chosen = required(&body, "strategy")?.to_string();
    let parent = absolute(&body, "directory")?;
    let name = match &body["name"] {
        Value::Null => slug(),
        Value::String(name) => name.clone(),
        _ => return Err(ApiError::invalid_request("Payload", "Expected string at [\"name\"]")),
    };
    strategy(&chosen)?;
    let root = loc.instance.worktree.as_ref().map_or_else(|| loc.instance.directory.clone(), |w| w.to_string_lossy().into_owned());
    let source = canonical(&root)?;
    if state.engine.store.get_project_directory(&project_id, &source)?.is_none() {
        return Err(copy_error(format!("Project copy source not found: {source}"), None));
    }
    std::fs::create_dir_all(&parent).map_err(ApiError::v2_unknown)?;
    let mut target = PathBuf::from(&parent).join(&name);
    let mut suffix = 1;
    while target.exists() {
        suffix += 1;
        if suffix > 10 {
            return Err(copy_error(format!("Project copy destination already exists: {}", target.display()), None));
        }
        target = PathBuf::from(&parent).join(format!("{name}-{suffix}"));
    }
    let Some(repository) = identity::discover(FsPath::new(&source)) else { return Err(unavailable(&source)) };
    let text = target.to_string_lossy().into_owned();
    let added = plain(&["worktree", "add", "--detach", &text, "HEAD"], &repository.worktree).await;
    if !added.ok() {
        return Err(worktree_error(&added, false));
    }
    if identity::discover(&target).is_none() {
        return Err(copy_error("Created worktree could not be opened", Some(false)));
    }
    let directory = canonical(&text)?;
    let update = state.engine.store.add_project_directory(&project_id, &directory, Some(STRATEGY), true)?;
    changed(&state, &loc, &project_id, update);
    Ok(crate::error::json(json!({"directory": directory})))
}

pub async fn remove(State(state): State<AppState>, loc: Loc, Path(project_id): Path<String>, body: Bytes) -> ApiResult {
    let body = payload(&body)?;
    let input = absolute(&body, "directory")?;
    let force = body["force"].as_bool().ok_or_else(|| ApiError::invalid_request("Payload", "Expected boolean at [\"force\"]"))?;
    let directory = canonical(&input)?;
    let stored = state.engine.store.get_project_directory(&project_id, &directory)?;
    let Some(chosen) = stored.and_then(|s| s.strategy) else {
        return Err(copy_error(format!("Invalid project copy directory: {directory}"), None));
    };
    strategy(&chosen)?;
    let Some(repository) = identity::discover(FsPath::new(&directory)) else { return Err(unavailable(&directory)) };
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&directory);
    let removed = plain(&args, &repository.common_directory).await;
    if !removed.ok() {
        return Err(worktree_error(&removed, true));
    }
    let update = state.engine.store.remove_project_directory(&project_id, &directory)?;
    changed(&state, &loc, &project_id, update);
    Ok(StatusCode::NO_CONTENT.into_response())
}

// Copies found next to the project's source directories are recorded; directories that are gone are dropped.
pub async fn refresh(State(state): State<AppState>, loc: Loc, Path(project_id): Path<String>) -> ApiResult {
    let store = &state.engine.store;
    let stored = store.list_project_directories(&project_id)?;
    let mut discovered: BTreeMap<String, Option<&str>> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for item in stored.iter().filter(|item| item.strategy.is_none() && FsPath::new(&item.directory).is_dir()) {
        let entries = match list(&item.directory).await {
            Ok(entries) => entries,
            Err(error) if error.body.pointer("/data/forceRequired").is_none() => Vec::new(),
            Err(error) => return Err(error),
        };
        for (directory, main) in entries {
            if !discovered.contains_key(&directory) {
                order.push(directory.clone());
            }
            discovered.insert(directory, (!main).then_some(STRATEGY));
        }
    }
    let mut update = false;
    for directory in &order {
        update |= store.add_project_directory(&project_id, directory, discovered[directory], true)?;
    }
    for item in stored.iter().filter(|item| !FsPath::new(&item.directory).is_dir()) {
        update |= store.remove_project_directory(&project_id, &item.directory)?;
    }
    changed(&state, &loc, &project_id, update);
    Ok(StatusCode::NO_CONTENT.into_response())
}
