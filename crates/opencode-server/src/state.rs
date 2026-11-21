// Shared server state and the per-request instance (directory, worktree, project).
use crate::bus::{Bus, Location};
use crate::error::{ApiError, ApiResult};
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use opencode_core::config::{self, LoadOptions, Loaded};
use opencode_core::flag;
use opencode_core::permission::Rule;
use opencode_session::{Engine, PermissionAnswer};
use opencode_storage::{Project, ProjectTime, Session, Vcs, id, identity};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::oneshot;

pub struct Pending {
    pub info: Value,
    pub session_id: String,
    pub permission: String,
    pub patterns: Vec<String>,
    pub always: Vec<String>,
    pub location: Location,
    pub reply: oneshot::Sender<PermissionAnswer>,
}

pub struct PendingQuestion {
    pub info: Value,
    pub session_id: String,
    pub location: Location,
    pub reply: oneshot::Sender<Option<Vec<Vec<String>>>>,
}

#[derive(Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

pub struct Inner {
    pub engine: Arc<Engine>,
    pub bus: Bus,
    pub credentials: Option<Credentials>,
    pub cors: Vec<String>,
    pub cwd: String,
    pub heartbeat: std::time::Duration,
    // Insertion order is ask order.
    pub pending: Mutex<Vec<Pending>>,
    pub questions: Mutex<Vec<PendingQuestion>>,
    pub approved: Mutex<Vec<Rule>>,
    // Where each known session publishes its events.
    pub locations: Mutex<HashMap<String, Location>>,
    // Directories whose MCP events are being forwarded to the bus.
    pub mcp_watched: Mutex<std::collections::HashSet<String>>,
    pub pty: Arc<crate::pty::PtyManager>,
    pub v2: crate::v2::V2,
    pub tui_requests: AsyncQueue<Value>,
    pub tui_responses: AsyncQueue<Value>,
    // Background work that must not outlive the server.
    pub tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    // When each instance directory was last used, and how many `/event` streams it has open.
    pub touched: Mutex<HashMap<String, std::time::Instant>>,
    pub streams: Mutex<HashMap<String, usize>>,
    // `Workspace` connection statuses in the order they were first set: (workspace id, status).
    pub connections: Mutex<Vec<(String, &'static str)>>,
}

// `AsyncQueue`: pushes are kept until someone asks for the next item.
pub struct AsyncQueue<T> {
    tx: tokio::sync::mpsc::UnboundedSender<T>,
    rx: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<T>>,
}

impl<T> Default for AsyncQueue<T> {
    fn default() -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self { tx, rx: tokio::sync::Mutex::new(rx) }
    }
}

impl<T> AsyncQueue<T> {
    pub fn push(&self, item: T) {
        let _ = self.tx.send(item);
    }

    pub async fn next(&self) -> T {
        loop {
            if let Some(item) = self.rx.lock().await.recv().await {
                return item;
            }
        }
    }
}

pub type AppState = Arc<Inner>;

pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

#[derive(Debug, Clone)]
pub struct Instance {
    pub directory: String,
    pub worktree: Option<PathBuf>,
    // True when `?directory=` was present; `GET /session` only filters by directory in that case.
    pub query_directory: bool,
}

// decodeURIComponent, falling back to the raw string on malformed input.
pub fn decode_component(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok());
            match hex {
                Some(byte) => {
                    out.push(byte);
                    i += 3;
                }
                None => return raw.to_string(),
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| raw.to_string())
}

pub fn query_pairs(query: Option<&str>) -> Vec<(String, String)> {
    let decode = |part: &str| decode_component(&part.replace('+', " "));
    query
        .unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(key), decode(value))
        })
        .collect()
}

pub fn query_get<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

// `^/session/([^/]+)(/|$)` minus `/session/status`, or `^/experimental/session/([^/]+)/background$`.
pub fn session_id_from_path(path: &str) -> Option<&str> {
    if let Some(rest) = path.strip_prefix("/session/") {
        let id = rest.split('/').next().filter(|id| !id.is_empty())?;
        return (path != "/session/status").then_some(id);
    }
    let rest = path.strip_prefix("/experimental/session/")?;
    let id = rest.strip_suffix("/background")?;
    (!id.is_empty() && !id.contains('/')).then_some(id)
}

// First non-empty of: session directory, `?directory=`, `x-opencode-directory`, cwd; then decoded once more.
pub fn select_directory(session: Option<&str>, query: Option<&str>, header: Option<&str>, cwd: &str) -> String {
    let chosen = [session, query, header].into_iter().flatten().find(|value| !value.is_empty()).unwrap_or(cwd);
    decode_component(chosen)
}

impl Inner {
    pub fn instance_for(&self, directory: &str, query_directory: bool) -> Instance {
        Instance { directory: directory.to_string(), worktree: config::find_worktree(&PathBuf::from(directory)), query_directory }
    }

    pub fn resolve(&self, parts: &Parts) -> Instance {
        let session = session_id_from_path(parts.uri.path())
            .and_then(|id| self.engine.store.get_session(id).ok().flatten())
            .map(|session| session.directory);
        let pairs = query_pairs(parts.uri.query());
        let query = query_get(&pairs, "directory").filter(|value| !value.is_empty());
        let header = parts.headers.get("x-opencode-directory").and_then(|value| value.to_str().ok());
        let directory = select_directory(session.as_deref(), query, header, &self.cwd);
        self.touch(&directory);
        self.instance_for(&directory, query.is_some())
    }

    pub fn touch(&self, directory: &str) {
        lock(&self.touched).insert(directory.to_string(), std::time::Instant::now());
    }

    pub fn load_config(&self, instance: &Instance) -> ApiResult<Loaded> {
        let opts = LoadOptions { cwd: PathBuf::from(&instance.directory), worktree: instance.worktree.clone() };
        config::load_with(&self.engine.paths, &opts, &flag::process_env).map_err(|e| ApiError::config(&e))
    }

    // The project row for a directory, created on first use like the original's instance boot.
    // The directory's worktree root is remembered among the project's directories (`Project.fromDirectory`).
    pub fn project(&self, instance: &Instance) -> ApiResult<Project> {
        let resolved = identity::resolve(&PathBuf::from(&instance.directory));
        if let Some(project) = self.engine.store.get_project(&resolved.id)? {
            opencode_session::remember_directory(&self.engine.store, &project.id, &resolved.directory);
            return Ok(project);
        }
        let now = id::now_ms();
        let project = Project {
            id: resolved.id,
            worktree: instance.worktree_string(),
            vcs: instance.worktree.as_ref().map(|_| Vcs::Git),
            name: None,
            icon: None,
            commands: None,
            time: ProjectTime { created: now, updated: now, initialized: None },
            sandboxes: Vec::new(),
        };
        self.engine.store.upsert_project(&project)?;
        opencode_session::remember_directory(&self.engine.store, &project.id, &resolved.directory);
        Ok(project)
    }

    pub fn location(&self, session: &Session) -> Location {
        Location { directory: session.directory.clone(), project: session.project_id.clone() }
    }

    pub fn instance_location(&self, instance: &Instance) -> Location {
        let project = identity::resolve(&PathBuf::from(&instance.directory)).id;
        Location { directory: instance.directory.clone(), project }
    }

    pub fn require_session(&self, id: &str) -> ApiResult<Session> {
        self.engine.store.get_session(id)?.ok_or_else(|| ApiError::not_found(format!("Session not found: {id}")))
    }
}

impl Instance {
    pub fn worktree_string(&self) -> String {
        self.worktree.as_ref().map_or_else(|| "/".to_string(), |w| w.to_string_lossy().into_owned())
    }
}

impl FromRequestParts<AppState> for Instance {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        Ok(state.resolve(parts))
    }
}

// Decoded query pairs with the D7 wire rules: booleans are exactly "true"/"false", numbers are strings.
pub struct Params(pub Vec<(String, String)>);

impl Params {
    pub fn get(&self, key: &str) -> Option<&str> {
        query_get(&self.0, key)
    }

    pub fn bool(&self, key: &str) -> ApiResult<Option<bool>> {
        match self.get(key) {
            None => Ok(None),
            Some("true") => Ok(Some(true)),
            Some("false") => Ok(Some(false)),
            Some(other) => Err(ApiError::schema("Query", format!("Expected \"true\" | \"false\", got \"{other}\" at [\"{key}\"]"))),
        }
    }

    pub fn number(&self, key: &str) -> ApiResult<Option<f64>> {
        match self.get(key) {
            None => Ok(None),
            Some(raw) => raw
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|n| n.is_finite() && !raw.trim().is_empty())
                .map(Some)
                .ok_or_else(|| ApiError::schema("Query", format!("Expected a number, got \"{raw}\" at [\"{key}\"]"))),
        }
    }

    pub fn required(&self, key: &str) -> ApiResult<&str> {
        self.get(key).ok_or_else(|| ApiError::schema("Query", format!("Missing key at [\"{key}\"]")))
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Params {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Params(query_pairs(parts.uri.query())))
    }
}

pub fn check_id(prefix: &str, name: &str, value: &str) -> ApiResult<()> {
    if value.starts_with(prefix) {
        return Ok(());
    }
    Err(ApiError::schema("Params", format!("Expected a string starting with \"{prefix}\", got \"{value}\" at [\"{name}\"]")))
}
