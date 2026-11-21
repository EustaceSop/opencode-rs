// Git-backed routes: `/vcs*`, `/project/git/init`, worktrees, plus the small legacy routes around them.
use crate::error::{ApiError, ApiResult, json};
use crate::routes::session::payload;
use crate::state::{AppState, Instance, Params, check_id, lock};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use opencode_storage::{Project, Vcs, id, slug};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;

const CONFIG: [&str; 11] = [
    "--no-optional-locks",
    "-c",
    "core.autocrlf=false",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.longpaths=true",
    "-c",
    "core.symlinks=true",
    "-c",
    "core.quotepath=false",
];
const PATCH_CONTEXT_LINES: u64 = 2_147_483_647;
const MAX_PATCH_BYTES: usize = 10_000_000;

pub struct GitOutput {
    pub code: i32,
    pub text: String,
    pub stderr: String,
}

impl GitOutput {
    pub(crate) fn ok(&self) -> bool {
        self.code == 0
    }

    pub(crate) fn message(&self, fallback: &str) -> String {
        [self.stderr.as_str(), self.text.as_str(), fallback].into_iter().find(|text| !text.is_empty()).unwrap_or_default().to_string()
    }
}

pub(crate) async fn spawn(program: &str, args: &[&str], cwd: &FsPath, stdin: Option<&str>) -> GitOutput {
    let failed = |error: std::io::Error| GitOutput { code: 1, text: String::new(), stderr: error.to_string() };
    let mut command = tokio::process::Command::new(program);
    command.args(args).current_dir(cwd).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    command.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return failed(error),
    };
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let _ = pipe.write_all(text.as_bytes()).await;
    }
    match child.wait_with_output().await {
        Ok(output) => GitOutput {
            code: output.status.code().unwrap_or(1),
            text: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        },
        Err(error) => failed(error),
    }
}

// `Git.run`: with the fixed `-c` settings of the original.
async fn git(args: &[&str], cwd: &FsPath) -> GitOutput {
    let all: Vec<&str> = CONFIG.iter().copied().chain(args.iter().copied()).collect();
    spawn("git", &all, cwd, None).await
}

// The plain `git` the project and worktree services run.
pub(crate) async fn plain(args: &[&str], cwd: &FsPath) -> GitOutput {
    spawn("git", args, cwd, None).await
}

fn nuls(text: &str) -> Vec<&str> {
    text.split('\0').filter(|item| !item.is_empty()).collect()
}

fn kind(code: &str) -> &'static str {
    if code == "??" {
        "added"
    } else if code.contains('U') {
        "modified"
    } else if code.contains('A') && !code.contains('D') {
        "added"
    } else if code.contains('D') && !code.contains('A') {
        "deleted"
    } else {
        "modified"
    }
}

#[derive(Clone)]
struct Item {
    file: String,
    code: String,
    status: &'static str,
}

async fn branch(cwd: &FsPath) -> Option<String> {
    let result = git(&["symbolic-ref", "--quiet", "--short", "HEAD"], cwd).await;
    Some(result.text.trim().to_string()).filter(|name| result.ok() && !name.is_empty())
}

async fn lines(args: &[&str], cwd: &FsPath) -> Vec<String> {
    git(args, cwd).await.text.lines().map(str::trim).filter(|line| !line.is_empty()).map(str::to_string).collect()
}

// (name, ref)
async fn default_branch(cwd: &FsPath) -> Option<(String, String)> {
    let remotes = lines(&["remote"], cwd).await;
    let has = |name: &str| remotes.iter().any(|remote| remote == name);
    let remote = if has("origin") {
        Some("origin".to_string())
    } else if remotes.len() == 1 || !has("upstream") {
        remotes.first().cloned()
    } else {
        Some("upstream".to_string())
    };
    if let Some(remote) = remote {
        let head = git(&["symbolic-ref", &format!("refs/remotes/{remote}/HEAD")], cwd).await;
        if head.ok() {
            let trimmed = head.text.trim();
            let reference = trimmed.strip_prefix("refs/remotes/").unwrap_or(trimmed).to_string();
            if let Some(name) = reference.strip_prefix(&format!("{remote}/")).filter(|name| !name.is_empty()) {
                return Some((name.to_string(), reference.clone()));
            }
        }
    }
    let list = lines(&["for-each-ref", "--format=%(refname:short)", "refs/heads"], cwd).await;
    let configured = git(&["config", "init.defaultBranch"], cwd).await.text.trim().to_string();
    if !configured.is_empty() && list.contains(&configured) {
        return Some((configured.clone(), configured));
    }
    ["main", "master"].into_iter().find(|name| list.iter().any(|item| item == name)).map(|name| (name.to_string(), name.to_string()))
}

async fn has_head(cwd: &FsPath) -> bool {
    git(&["rev-parse", "--verify", "HEAD"], cwd).await.ok()
}

async fn status(cwd: &FsPath) -> Vec<Item> {
    let out = git(&["status", "--porcelain=v1", "--untracked-files=all", "--no-renames", "-z", "--", "."], cwd).await;
    nuls(&out.text)
        .into_iter()
        .filter(|item| item.len() > 3 && item.is_char_boundary(3))
        .map(|item| Item { file: item[3..].to_string(), code: item[..2].to_string(), status: kind(&item[..2]) })
        .collect()
}

async fn changed(cwd: &FsPath, reference: &str) -> Vec<Item> {
    let out = git(&["diff", "--no-ext-diff", "--no-renames", "--name-status", "-z", reference, "--", "."], cwd).await;
    nuls(&out.text).chunks(2).filter(|pair| pair.len() == 2).map(|pair| Item { file: pair[1].to_string(), code: pair[0].to_string(), status: kind(pair[0]) }).collect()
}

fn count(text: &str) -> u64 {
    text.parse().unwrap_or(0)
}

async fn stats(cwd: &FsPath, reference: &str) -> HashMap<String, (u64, u64)> {
    let out = git(&["diff", "--no-ext-diff", "--no-renames", "--numstat", "-z", reference, "--", "."], cwd).await;
    nuls(&out.text)
        .into_iter()
        .filter_map(|item| {
            let mut fields = item.splitn(3, '\t');
            let (adds, dels, file) = (fields.next()?, fields.next()?, fields.next()?);
            (!file.is_empty()).then(|| (file.to_string(), (count(adds), count(dels))))
        })
        .collect()
}

async fn stat_untracked(cwd: &FsPath, file: &str) -> Option<(u64, u64)> {
    let out = git(&["diff", "--no-index", "--numstat", "--", "/dev/null", file], cwd).await;
    let line = out.text.lines().next()?;
    let mut fields = line.split('\t');
    Some((count(fields.next()?), count(fields.next()?)))
}

// `formatPatch(structuredPatch(file, file, "", "", "", "", {context: 0}))`
fn empty_patch(file: &str) -> String {
    format!("Index: {file}\n{}\n--- {file}\t\n+++ {file}\t\n", "=".repeat(67))
}

async fn patch(cwd: &FsPath, reference: Option<&str>, item: &Item, context: u64) -> String {
    let unified = format!("--unified={context}");
    let out = match reference.filter(|_| item.code != "??") {
        Some(reference) => git(&["diff", "--patch", "--no-ext-diff", "--no-renames", &unified, reference, "--", &item.file], cwd).await,
        None => git(&["diff", "--no-index", "--patch", "--no-ext-diff", "--no-renames", &unified, "--", "/dev/null", &item.file], cwd).await,
    };
    if out.text.is_empty() || out.text.len() > MAX_PATCH_BYTES { empty_patch(&item.file) } else { out.text }
}

async fn files(cwd: &FsPath, reference: Option<&str>, mut list: Vec<Item>, map: &HashMap<String, (u64, u64)>, context: u64) -> Vec<Value> {
    list.sort_by(|a, b| a.file.cmp(&b.file));
    let mut out = Vec::new();
    let mut total = 0usize;
    for item in list {
        let stat = match map.get(&item.file) {
            Some(stat) => Some(*stat),
            None if item.status == "added" => stat_untracked(cwd, &item.file).await,
            None => None,
        };
        let mut text = if total >= MAX_PATCH_BYTES { empty_patch(&item.file) } else { patch(cwd, reference, &item, context).await };
        if total + text.len() > MAX_PATCH_BYTES {
            text = empty_patch(&item.file);
            total = MAX_PATCH_BYTES;
        } else {
            total += text.len();
        }
        let (additions, deletions) = stat.unwrap_or((0, 0));
        out.push(json!({"file": item.file, "patch": text, "additions": additions, "deletions": deletions, "status": item.status}));
    }
    out
}

async fn diff_against(cwd: &FsPath, reference: &str, context: u64) -> Vec<Value> {
    let mut list = changed(cwd, reference).await;
    for item in status(cwd).await.into_iter().filter(|item| item.code == "??") {
        if !list.iter().any(|known| known.file == item.file) {
            list.push(item);
        }
    }
    files(cwd, Some(reference), list, &stats(cwd, reference).await, context).await
}

fn repository(instance: &Instance) -> Option<PathBuf> {
    instance.worktree.as_ref().map(|_| PathBuf::from(&instance.directory))
}

pub async fn info(instance: Instance) -> ApiResult {
    let mut out = serde_json::Map::new();
    if let Some(cwd) = repository(&instance) {
        if let Some(name) = branch(&cwd).await {
            out.insert("branch".into(), name.into());
        }
        if let Some((name, _)) = default_branch(&cwd).await {
            out.insert("default_branch".into(), name.into());
        }
    }
    Ok(json(out))
}

// (file, additions, deletions, status) of every changed or untracked file, sorted by file.
async fn changes(instance: &Instance) -> Vec<(String, u64, u64, &'static str)> {
    let Some(cwd) = repository(instance) else { return Vec::new() };
    let map = if has_head(&cwd).await { stats(&cwd, "HEAD").await } else { HashMap::new() };
    let mut list = status(&cwd).await;
    list.sort_by(|a, b| a.file.cmp(&b.file));
    let mut out = Vec::new();
    for item in list {
        let stat = match map.get(&item.file) {
            Some(stat) => Some(*stat),
            None if item.status == "added" => stat_untracked(&cwd, &item.file).await,
            None => None,
        };
        let (additions, deletions) = stat.unwrap_or((0, 0));
        out.push((item.file, additions, deletions, item.status));
    }
    out
}

pub async fn file_status(instance: Instance) -> ApiResult {
    let list = changes(&instance).await;
    Ok(json(list.into_iter().map(|(file, additions, deletions, status)| json!({"file": file, "additions": additions, "deletions": deletions, "status": status})).collect::<Vec<_>>()))
}

pub async fn diff(instance: Instance, params: Params) -> ApiResult {
    let mode = params.required("mode")?;
    if !matches!(mode, "git" | "branch") {
        return Err(ApiError::schema("Query", format!("Expected \"git\" | \"branch\", got \"{mode}\" at [\"mode\"]")));
    }
    let context = params.number("context")?.map_or(PATCH_CONTEXT_LINES, |n| n.max(0.0) as u64);
    let Some(cwd) = repository(&instance) else { return Ok(json(Vec::<Value>::new())) };
    if mode == "git" {
        return Ok(json(if has_head(&cwd).await {
            diff_against(&cwd, "HEAD", context).await
        } else {
            files(&cwd, None, status(&cwd).await, &HashMap::new(), context).await
        }));
    }
    let empty = || Ok(json(Vec::<Value>::new()));
    let Some((name, reference)) = default_branch(&cwd).await else { return empty() };
    if branch(&cwd).await.as_deref() == Some(name.as_str()) {
        return empty();
    }
    let base = git(&["merge-base", &reference, "HEAD"], &cwd).await;
    let base_ref = base.text.trim();
    if !base.ok() || base_ref.is_empty() {
        return empty();
    }
    Ok(json(diff_against(&cwd, base_ref, context).await))
}

pub async fn diff_raw(instance: Instance) -> ApiResult {
    Ok(([(header::CONTENT_TYPE, "text/x-diff; charset=utf-8")], raw_diff(&instance).await).into_response())
}

// `Vcs.diffRaw`: the patch of every change against HEAD, untracked files included.
pub(crate) async fn raw_diff(instance: &Instance) -> String {
    let mut parts = Vec::new();
    if let Some(cwd) = repository(instance) {
        if has_head(&cwd).await {
            parts.push(git(&["diff", "--patch", "--no-ext-diff", "--no-renames", "--unified=3", "HEAD", "--", "."], &cwd).await.text);
        }
        for item in status(&cwd).await.into_iter().filter(|item| item.code == "??") {
            let args = ["diff", "--no-index", "--patch", "--no-ext-diff", "--no-renames", "--unified=3", "--", "/dev/null", &item.file];
            parts.push(git(&args, &cwd).await.text);
        }
    }
    parts.retain(|part| !part.is_empty());
    parts.join("\n")
}

fn apply_error(message: &str, reason: &str) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, json!({"name": "VcsApplyError", "data": {"message": message, "reason": reason}}))
}

pub async fn apply(instance: Instance, body: Bytes) -> ApiResult {
    let body: Value = payload(&body)?;
    let text = body["patch"].as_str().ok_or_else(|| ApiError::schema("Payload", "Expected string at [\"patch\"]"))?;
    apply_patch(&instance, text).await?;
    Ok(json(json!({"applied": true})))
}

// `Vcs.apply`: a failure is a `VcsApplyError`.
pub(crate) async fn apply_patch(instance: &Instance, text: &str) -> ApiResult<()> {
    let Some(cwd) = repository(instance) else {
        return Err(apply_error("Patch can't be applied because the project is not git-based", "non-git"));
    };
    let all: Vec<&str> = CONFIG.iter().copied().chain(["apply", "-"]).collect();
    if !spawn("git", &all, &cwd, Some(text)).await.ok() {
        return Err(apply_error("Patch can't be applied", "not-clean"));
    }
    Ok(())
}

pub async fn init_git(State(state): State<AppState>, instance: Instance) -> ApiResult {
    let project = state.project(&instance)?;
    if project.vcs == Some(Vcs::Git) && instance.worktree.is_some() {
        return Ok(json(project));
    }
    let result = plain(&["init", "--quiet"], FsPath::new(&instance.directory)).await;
    if !result.ok() {
        let text = [result.stderr.trim(), result.text.trim(), "Failed to initialize git repository"].into_iter().find(|t| !t.is_empty()).unwrap_or_default();
        return Err(ApiError::internal(text));
    }
    // `Project.fromDirectory`: the row of the directory's project now says git; a repository without commits stays
    // in the global project, whose worktree follows the directory.
    let reloaded = state.instance_for(&instance.directory, false);
    let mut next = state.project(&reloaded)?;
    next.vcs = Some(Vcs::Git);
    if next.id == "global" {
        next.worktree = reloaded.worktree_string();
    }
    next.time.updated = id::now_ms();
    state.engine.store.upsert_project(&next)?;
    state.bus.publish_global(Some("global"), Some(&next.id), "project.updated", json!(next));
    // The instance is reloaded so everything derived from the old project goes away.
    state.dispose_instance(&instance.directory).await;
    Ok(json(next))
}

// `Project.directories`: every worktree root opened for the project and every copy made of it.
pub async fn project_directories(State(state): State<AppState>, Path(project_id): Path<String>) -> ApiResult {
    Ok(json(state.engine.store.list_project_directories(&project_id)?))
}

// Without a context the original answers with a random slug; with one it asks the small model and falls back to a slug.
pub async fn generate_name(State(state): State<AppState>, instance: Instance, Path(_project_id): Path<String>, body: Bytes) -> ApiResult {
    let body: Value = payload(&body)?;
    if !body.is_object() {
        return Err(ApiError::schema("Payload", "Expected object"));
    }
    let context = match &body["context"] {
        Value::Null => None,
        Value::String(text) => Some(text.as_str()),
        _ => return Err(ApiError::schema("Payload", "Expected string at [\"context\"]")),
    };
    Ok(json(json!({"name": state.engine.generate_name(FsPath::new(&instance.directory), context).await})))
}

fn slugify(input: &str) -> String {
    let mut out = String::new();
    for c in input.trim().to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

fn worktree_error(name: &str, message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, json!({"name": name, "data": {"message": message.into()}}))
}

pub(crate) fn canonical(input: &str) -> String {
    let path = PathBuf::from(input);
    let real = path.canonicalize().unwrap_or(path);
    let text = real.to_string_lossy().trim_start_matches(r"\\?\").to_string();
    if cfg!(windows) { text.replace('/', "\\").to_lowercase() } else { text }
}

pub(crate) fn parse_worktrees(text: &str) -> Vec<(String, Option<String>)> {
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(path) = line.strip_prefix("worktree ") {
            out.push((path.trim().to_string(), None));
        } else if let (Some(name), Some(last)) = (line.strip_prefix("branch "), out.last_mut()) {
            last.1 = Some(name.trim().to_string());
        }
    }
    out
}

pub(crate) fn git_project(state: &AppState, instance: &Instance) -> ApiResult<(Project, PathBuf)> {
    let project = state.project(instance)?;
    match (&instance.worktree, project.vcs) {
        (Some(worktree), Some(Vcs::Git)) => Ok((project, worktree.clone())),
        _ => Err(worktree_error("WorktreeNotGitError", "Worktrees are only supported for git projects")),
    }
}

fn save_sandboxes(state: &AppState, mut project: Project, change: impl FnOnce(&mut Vec<String>)) -> ApiResult<()> {
    change(&mut project.sandboxes);
    project.time.updated = id::now_ms();
    state.engine.store.upsert_project(&project)?;
    state.bus.publish_global(Some("global"), Some(&project.id), "project.updated", json!(project));
    Ok(())
}

pub async fn worktree_list(State(state): State<AppState>, instance: Instance) -> ApiResult {
    let project = state.project(&instance)?;
    Ok(json(project.sandboxes.into_iter().filter(|dir| FsPath::new(dir).is_dir()).collect::<Vec<_>>()))
}

async fn run_start(directory: &FsPath, command: &str) -> bool {
    let text = command.trim();
    if text.is_empty() {
        return true;
    }
    let result = if cfg!(windows) { spawn("cmd", &["/c", text], directory, None).await } else { spawn("bash", &["-lc", text], directory, None).await };
    if !result.ok() {
        tracing::error!(directory = %directory.display(), message = %result.stderr, "worktree start command failed");
    }
    result.ok()
}

async fn start_scripts(state: &AppState, project_id: &str, directory: &FsPath, extra: &str) {
    let startup = state.engine.store.get_project(project_id).ok().flatten().and_then(|p| p.commands).and_then(|c| c.start).unwrap_or_default();
    if run_start(directory, &startup).await {
        run_start(directory, extra).await;
    }
}

// `Worktree.Info`
pub(crate) struct WorktreeInfo {
    pub(crate) name: String,
    pub(crate) branch: Option<String>,
    pub(crate) directory: PathBuf,
}

// `Worktree.makeWorktreeInfo`: a free name under `<data>/worktree/<project>`, and an unused `opencode/<name>` branch
// unless detached. Also hands back the project and its worktree.
pub(crate) async fn make_worktree_info(state: &AppState, instance: &Instance, name: Option<&str>, detached: bool) -> ApiResult<(Project, PathBuf, WorktreeInfo)> {
    let (project, worktree) = git_project(state, instance)?;
    let root = state.engine.paths.data.join("worktree").join(&project.id);
    std::fs::create_dir_all(&root).map_err(ApiError::internal)?;
    let base = name.map(slugify).unwrap_or_default();
    for attempt in 0..26 {
        let name = match (base.is_empty(), attempt) {
            (true, _) => slug(),
            (false, 0) => base.clone(),
            (false, _) => format!("{base}-{}", slug()),
        };
        let directory = root.join(&name);
        let branch = (!detached).then(|| format!("opencode/{name}"));
        if directory.exists() {
            continue;
        }
        if let Some(branch) = &branch
            && plain(&["show-ref", "--verify", "--quiet", &format!("refs/heads/{branch}")], &worktree).await.ok()
        {
            continue;
        }
        return Ok((project, worktree, WorktreeInfo { name, branch, directory }));
    }
    Err(worktree_error("WorktreeNameGenerationFailedError", "Failed to generate a unique worktree name"))
}

// `Worktree.createFromInfo`: `git worktree add` (on a new branch, or detached at HEAD), the sandbox entry, then the
// checkout and start scripts in the background.
pub(crate) async fn create_from_info(state: &AppState, project: Project, worktree: &FsPath, info: &WorktreeInfo, extra: String) -> ApiResult<()> {
    let target = info.directory.to_string_lossy().into_owned();
    let created = match &info.branch {
        Some(branch) => plain(&["worktree", "add", "--no-checkout", "-b", branch, &target], worktree).await,
        None => plain(&["worktree", "add", "--no-checkout", "--detach", &target, "HEAD"], worktree).await,
    };
    if !created.ok() {
        return Err(worktree_error("WorktreeCreateFailedError", created.message("Failed to create git worktree")));
    }
    let project_id = project.id.clone();
    let added = target.clone();
    let _ = save_sandboxes(state, project, |list| {
        if !list.contains(&added) {
            list.push(added);
        }
    });
    let mut ready = json!({"name": info.name});
    if let Some(branch) = &info.branch {
        ready["branch"] = branch.clone().into();
    }
    let boot = state.clone();
    let checkout = info.directory.clone();
    let task = tokio::spawn(async move {
        let dir = checkout.to_string_lossy().into_owned();
        let populated = plain(&["reset", "--hard"], &checkout).await;
        if !populated.ok() {
            let message = populated.message("Failed to populate worktree");
            boot.bus.publish_global(Some(&dir), Some(&project_id), "worktree.failed", json!({"message": message}));
            return;
        }
        boot.bus.publish_global(Some(&dir), Some(&project_id), "worktree.ready", ready);
        start_scripts(&boot, &project_id, &checkout, &extra).await;
    });
    lock(&state.tasks).push(task);
    Ok(())
}

pub async fn worktree_create(State(state): State<AppState>, instance: Instance, body: Bytes) -> ApiResult {
    let body: Value = if body.iter().all(u8::is_ascii_whitespace) { json!({}) } else { payload(&body)? };
    let (project, worktree, info) = make_worktree_info(&state, &instance, body["name"].as_str(), false).await?;
    let extra = body["startCommand"].as_str().unwrap_or_default().to_string();
    create_from_info(&state, project, &worktree, &info, extra).await?;
    Ok(json(json!({"name": info.name, "branch": info.branch, "directory": info.directory.to_string_lossy()})))
}

fn directory_field(body: &Bytes) -> ApiResult<String> {
    let body: Value = payload(body)?;
    body["directory"].as_str().map(str::to_string).ok_or_else(|| ApiError::schema("Payload", "Expected string at [\"directory\"]"))
}

async fn clean_directory(target: &str) -> Result<(), String> {
    let attempts = if cfg!(windows) { 50 } else { 5 };
    for attempt in 0..attempts {
        match tokio::fs::remove_dir_all(target).await {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) if attempt == attempts - 1 => return Err(error.to_string()),
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
        }
    }
    Ok(())
}

async fn stop_fsmonitor(target: &str) {
    if FsPath::new(target).exists() {
        plain(&["fsmonitor--daemon", "stop"], FsPath::new(target)).await;
    }
}

fn locate<'a>(entries: &'a [(String, Option<String>)], directory: &str) -> Option<&'a (String, Option<String>)> {
    entries.iter().find(|(path, _)| canonical(path) == directory)
}

pub async fn worktree_remove(State(state): State<AppState>, instance: Instance, body: Bytes) -> ApiResult {
    let input = directory_field(&body)?;
    remove_worktree(&state, &instance, &input).await?;
    Ok(json(true))
}

// `Worktree.remove`: the git worktree, its directory and branch, and the project's sandbox entry.
pub(crate) async fn remove_worktree(state: &AppState, instance: &Instance, input: &str) -> ApiResult<()> {
    let input = input.to_string();
    let (project, worktree) = git_project(state, instance)?;
    let failed = |message: String| worktree_error("WorktreeRemoveFailedError", message);
    let directory = canonical(&input);
    if directory != canonical(&worktree.to_string_lossy()) {
        state.dispose_instance(&input).await;
    }
    let list = plain(&["worktree", "list", "--porcelain"], &worktree).await;
    if !list.ok() {
        return Err(failed(list.message("Failed to read git worktrees")));
    }
    match locate(&parse_worktrees(&list.text), &directory).cloned() {
        None => {
            if FsPath::new(&directory).exists() {
                stop_fsmonitor(&directory).await;
                clean_directory(&directory).await.map_err(failed)?;
            }
        }
        Some((path, branch)) => {
            state.dispose_instance(&path).await;
            stop_fsmonitor(&path).await;
            let removed = plain(&["worktree", "remove", "--force", &path], &worktree).await;
            if !removed.ok() {
                let next = plain(&["worktree", "list", "--porcelain"], &worktree).await;
                if !next.ok() || locate(&parse_worktrees(&next.text), &directory).is_some() {
                    return Err(failed(removed.message(&next.message("Failed to remove git worktree"))));
                }
            }
            clean_directory(&path).await.map_err(failed)?;
            if let Some(branch) = branch.as_deref().map(|b| b.strip_prefix("refs/heads/").unwrap_or(b)) {
                let deleted = plain(&["branch", "-D", branch], &worktree).await;
                if !deleted.ok() {
                    return Err(failed(deleted.message("Failed to delete worktree branch")));
                }
            }
        }
    }
    save_sandboxes(state, project, |list| list.retain(|item| item != &input))
}

pub async fn worktree_reset(State(state): State<AppState>, instance: Instance, body: Bytes) -> ApiResult {
    let input = directory_field(&body)?;
    let (project, worktree) = git_project(&state, &instance)?;
    let failed = |message: String| worktree_error("WorktreeResetFailedError", message);
    let directory = canonical(&input);
    if directory == canonical(&worktree.to_string_lossy()) {
        return Err(failed("Cannot reset the primary workspace".into()));
    }
    let list = plain(&["worktree", "list", "--porcelain"], &worktree).await;
    if !list.ok() {
        return Err(failed(list.message("Failed to read git worktrees")));
    }
    let Some((path, _)) = locate(&parse_worktrees(&list.text), &directory).cloned() else {
        return Err(failed("Worktree not found".into()));
    };
    let target = PathBuf::from(&path);
    let Some((name, reference)) = default_branch(&worktree).await else {
        return Err(failed("Default branch not found".into()));
    };
    if let Some((remote, remote_branch)) = reference.split_once('/').filter(|(remote, _)| reference != name && !remote.is_empty()) {
        let fetched = plain(&["fetch", remote, remote_branch], &worktree).await;
        if !fetched.ok() {
            return Err(failed(fetched.message(&format!("Failed to fetch {reference}"))));
        }
    }
    let steps: [(&[&str], &str); 5] = [
        (&["reset", "--hard", &reference], "Failed to reset worktree to target"),
        (&["clean", "-ffdx"], "Failed to clean worktree"),
        (&["submodule", "update", "--init", "--recursive", "--force"], "Failed to update submodules"),
        (&["submodule", "foreach", "--recursive", "git", "reset", "--hard"], "Failed to reset submodules"),
        (&["submodule", "foreach", "--recursive", "git", "clean", "-fdx"], "Failed to clean submodules"),
    ];
    for (args, fallback) in steps {
        let result = plain(args, &target).await;
        if !result.ok() {
            return Err(failed(result.message(fallback)));
        }
    }
    let status = plain(&["-c", "core.fsmonitor=false", "status", "--porcelain=v1"], &target).await;
    if !status.ok() {
        return Err(failed(status.message("Failed to read git status")));
    }
    if !status.text.trim().is_empty() {
        return Err(failed(format!("Worktree reset left local changes:\n{}", status.text.trim())));
    }
    let boot = state.clone();
    let task = tokio::spawn(async move { start_scripts(&boot, &project.id, &target, "").await });
    lock(&state.tasks).push(task);
    Ok(json(true))
}

// Sharing was discontinued: sharing fails, unsharing only clears the local link.
pub async fn share(State(state): State<AppState>, Path(session_id): Path<String>) -> ApiResult {
    check_id("ses", "sessionID", &session_id)?;
    state.require_session(&session_id)?;
    Err(ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, json!({"_tag": "InternalServerError"})))
}

pub async fn unshare(State(state): State<AppState>, Path(session_id): Path<String>) -> ApiResult {
    check_id("ses", "sessionID", &session_id)?;
    state.require_session(&session_id)?;
    Ok(json(state.engine.store.patch_session(&session_id, |session| session.share = None)?))
}

// Long-polls the queue the TUI's control requests are put on.
pub async fn tui_control_next(State(state): State<AppState>) -> Response {
    json(state.tui_requests.next().await)
}

pub async fn tui_control_response(State(state): State<AppState>, body: Bytes) -> ApiResult {
    let body: Value = payload(&body)?;
    state.tui_responses.push(body);
    Ok(json(true))
}

pub async fn sync_steal(State(state): State<AppState>, body: Bytes) -> ApiResult {
    let body: Value = payload(&body)?;
    let session_id = body["sessionID"]
        .as_str()
        .filter(|id| id.starts_with("ses"))
        .ok_or_else(|| ApiError::schema("Payload", "Expected a string starting with \"ses\" at [\"sessionID\"]"))?;
    let Some(workspace_id) = std::env::var("OPENCODE_WORKSPACE_ID").ok().filter(|id| !id.is_empty()) else {
        return Err(ApiError::bad_request());
    };
    state.require_session(session_id)?;
    state.engine.store.patch_session(session_id, |session| session.workspace_id = Some(workspace_id))?;
    Ok(json(json!({"sessionID": session_id})))
}

// No console accounts can be logged in here: the state of a server without any.
pub async fn console() -> Response {
    json(json!({"consoleManagedProviders": [], "switchableOrgCount": 0}))
}

pub async fn console_orgs() -> Response {
    json(json!({"orgs": []}))
}

pub async fn console_switch(body: Bytes) -> ApiResult {
    let body: Value = payload(&body)?;
    for key in ["accountID", "orgID"] {
        if !body[key].is_string() {
            return Err(ApiError::schema("Payload", format!("Expected string at [\"{key}\"]")));
        }
    }
    Err(ApiError::bad_request())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes_map_to_kinds() {
        assert_eq!([kind("??"), kind("A "), kind(" D"), kind("UU"), kind("AD"), kind(" M")], ["added", "added", "deleted", "modified", "modified", "modified"]);
    }

    #[test]
    fn worktree_listing_and_slugs() {
        let listed = parse_worktrees("worktree /a\nHEAD 1\nbranch refs/heads/main\n\nworktree /b\nHEAD 2\ndetached\n");
        assert_eq!(listed, vec![("/a".to_string(), Some("refs/heads/main".to_string())), ("/b".to_string(), None)]);
        assert_eq!(slugify("  Fix: The Bug!! "), "fix-the-bug");
        assert!(empty_patch("a.txt").starts_with("Index: a.txt\n====="));
    }
}
