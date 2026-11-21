use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{broadcast, mpsc, oneshot};

use super::diagnostic::{Diagnostic, dedupe};
use super::language::language_id;
use crate::util;

const DIAGNOSTICS_DEBOUNCE: Duration = Duration::from_millis(150);
const DIAGNOSTICS_DOCUMENT_WAIT_TIMEOUT: Duration = Duration::from_millis(5_000);
const DIAGNOSTICS_FULL_WAIT_TIMEOUT: Duration = Duration::from_millis(10_000);
const DIAGNOSTICS_REQUEST_TIMEOUT: Duration = Duration::from_millis(3_000);
const INITIALIZE_TIMEOUT: Duration = Duration::from_millis(45_000);

const FILE_CHANGE_CREATED: u8 = 1;
const FILE_CHANGE_CHANGED: u8 = 2;
const TEXT_DOCUMENT_SYNC_INCREMENTAL: u64 = 2;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("LSP initialize failed for {server_id}: {cause}")]
    Initialize { server_id: String, cause: String },
    #[error("LSP request failed: {0}")]
    Request(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticsMode {
    Document,
    Full,
}

#[derive(Debug, Clone)]
struct Registration {
    identifier: Option<String>,
    workspace: bool,
}

#[derive(Default)]
struct State {
    push: IndexMap<PathBuf, Vec<Diagnostic>>,
    pull: IndexMap<PathBuf, Vec<Diagnostic>>,
    published: HashMap<PathBuf, (Instant, Option<i64>)>,
    registrations: IndexMap<String, Registration>,
}

#[derive(Default)]
struct RequestResult {
    handled: bool,
    matched: bool,
    by_file: IndexMap<PathBuf, Vec<Diagnostic>>,
}

type Pending = HashMap<u64, oneshot::Sender<Result<Value, String>>>;

struct Inner {
    server_id: String,
    root: PathBuf,
    initialization: Option<Value>,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    pending: Mutex<Pending>,
    next_id: AtomicU64,
    state: Mutex<State>,
    diagnostic_events: broadcast::Sender<PathBuf>,
    registration_events: broadcast::Sender<()>,
}

pub struct Client {
    inner: Arc<Inner>,
    directory: PathBuf,
    child: Mutex<Option<Child>>,
    reader: tokio::task::JoinHandle<()>,
    sync_kind: Option<u64>,
    static_pull: bool,
    files: tokio::sync::Mutex<HashMap<PathBuf, (i64, String)>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn file_path(uri: &str) -> Option<PathBuf> {
    util::uri_to_path(uri).map(|p| util::normalize_path(&p))
}

fn parse_diagnostics(value: Option<&Value>) -> Option<Vec<Diagnostic>> {
    let items = value?.as_array()?;
    Some(
        items
            .iter()
            .filter_map(|item| serde_json::from_value(item.clone()).ok())
            .collect(),
    )
}

fn end_position(text: &str) -> Value {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    json!({ "line": lines.len() - 1, "character": lines.last().map_or(0, |l| l.encode_utf16().count()) })
}

fn configuration_value(settings: Option<&Value>, section: Option<&str>) -> Value {
    let Some(section) = section.filter(|s| !s.is_empty()) else {
        return settings.cloned().unwrap_or(Value::Null);
    };
    let mut current = settings;
    for key in section.split('.') {
        current = current
            .and_then(Value::as_object)
            .and_then(|map| map.get(key));
    }
    current.cloned().unwrap_or(Value::Null)
}

impl Inner {
    async fn write(&self, message: &Value) -> Result<(), ClientError> {
        let body = message.to_string();
        let frame = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        let mut guard = self.stdin.lock().await;
        let stdin = guard
            .as_mut()
            .ok_or_else(|| ClientError::Request("connection closed".into()))?;
        stdin.write_all(frame.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), ClientError> {
        self.write(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(id, tx);
        let outcome = async {
            self.write(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
                .await?;
            match rx.await {
                Ok(result) => result.map_err(ClientError::Request),
                Err(_) => Err(ClientError::Request("connection closed".into())),
            }
        };
        let result = match timeout {
            Some(limit) => tokio::time::timeout(limit, outcome)
                .await
                .unwrap_or_else(|_| Err(ClientError::Request(format!("{method} timed out")))),
            None => outcome.await,
        };
        lock(&self.pending).remove(&id);
        result
    }

    fn on_publish(&self, params: &Value) {
        let Some(path) = params
            .get("uri")
            .and_then(Value::as_str)
            .and_then(file_path)
        else {
            return;
        };
        let diagnostics = parse_diagnostics(params.get("diagnostics")).unwrap_or_default();
        let version = params.get("version").and_then(Value::as_i64);
        {
            let mut state = lock(&self.state);
            state
                .published
                .insert(path.clone(), (Instant::now(), version));
            // TypeScript pushes aggressively on first open: seed the cache without waking waiters.
            let seed = self.server_id == "typescript" && !state.push.contains_key(&path);
            state.push.insert(path.clone(), diagnostics);
            if seed {
                return;
            }
        }
        let _ = self.diagnostic_events.send(path);
    }

    fn on_request(&self, method: &str, params: &Value) -> Result<Value, (i64, String)> {
        match method {
            "window/workDoneProgress/create" | "workspace/diagnostic/refresh" => Ok(Value::Null),
            "workspace/configuration" => {
                let items = params
                    .get("items")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                Ok(Value::Array(
                    items
                        .iter()
                        .map(|item| {
                            configuration_value(
                                self.initialization.as_ref(),
                                item.get("section").and_then(Value::as_str),
                            )
                        })
                        .collect(),
                ))
            }
            "client/registerCapability" => {
                let mut changed = false;
                for item in params
                    .get("registrations")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if item.get("method").and_then(Value::as_str) != Some("textDocument/diagnostic")
                    {
                        continue;
                    }
                    let Some(id) = item.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    let options = item.get("registerOptions");
                    lock(&self.state).registrations.insert(
                        id.to_string(),
                        Registration {
                            identifier: options
                                .and_then(|o| o.get("identifier"))
                                .and_then(Value::as_str)
                                .map(String::from),
                            workspace: options.and_then(|o| o.get("workspaceDiagnostics"))
                                == Some(&Value::Bool(true)),
                        },
                    );
                    changed = true;
                }
                if changed {
                    let _ = self.registration_events.send(());
                }
                Ok(Value::Null)
            }
            "client/unregisterCapability" => {
                let mut changed = false;
                // "unregisterations" is the spelling in the LSP specification.
                for item in params
                    .get("unregisterations")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if item.get("method").and_then(Value::as_str) != Some("textDocument/diagnostic")
                    {
                        continue;
                    }
                    if let Some(id) = item.get("id").and_then(Value::as_str) {
                        lock(&self.state).registrations.shift_remove(id);
                        changed = true;
                    }
                }
                if changed {
                    let _ = self.registration_events.send(());
                }
                Ok(Value::Null)
            }
            "workspace/workspaceFolders" => {
                Ok(json!([{ "name": "workspace", "uri": util::path_to_uri(&self.root) }]))
            }
            other => Err((-32601, format!("Unhandled method {other}"))),
        }
    }

    async fn dispatch(&self, message: Value) {
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id").filter(|id| !id.is_null());
        match (method, id) {
            (Some(method), Some(id)) => {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                let reply = match self.on_request(method, &params) {
                    Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                    Err((code, text)) => {
                        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": text } })
                    }
                };
                let _ = self.write(&reply).await;
            }
            (Some("textDocument/publishDiagnostics"), None) => {
                self.on_publish(message.get("params").unwrap_or(&Value::Null));
            }
            (Some(_), None) => {}
            (None, Some(id)) => {
                let Some(tx) = id.as_u64().and_then(|id| lock(&self.pending).remove(&id)) else {
                    return;
                };
                let result = match message.get("error") {
                    Some(error) if !error.is_null() => Err(error
                        .get("message")
                        .and_then(Value::as_str)
                        .map_or_else(|| error.to_string(), String::from)),
                    _ => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = tx.send(result);
            }
            (None, None) => {}
        }
    }

    fn merge_results(&self, path: &Path, results: &[RequestResult]) -> (bool, bool) {
        let handled = results.iter().any(|r| r.handled);
        let matched = results.iter().any(|r| r.matched);
        if !handled {
            return (false, false);
        }
        let mut merged: IndexMap<PathBuf, Vec<Diagnostic>> = IndexMap::new();
        for result in results {
            for (target, items) in &result.by_file {
                merged
                    .entry(target.clone())
                    .or_default()
                    .extend(items.iter().cloned());
            }
        }
        if matched && !merged.contains_key(path) {
            merged.insert(path.to_path_buf(), Vec::new());
        }
        let mut state = lock(&self.state);
        for (target, items) in merged {
            state.pull.insert(target, dedupe(items));
        }
        (handled, matched)
    }

    async fn request_report(&self, path: &Path, identifier: Option<&str>) -> RequestResult {
        let mut params = json!({ "textDocument": { "uri": util::path_to_uri(path) } });
        if let Some(identifier) = identifier.filter(|i| !i.is_empty()) {
            params["identifier"] = json!(identifier);
        }
        let report = self
            .request(
                "textDocument/diagnostic",
                params,
                Some(DIAGNOSTICS_REQUEST_TIMEOUT),
            )
            .await;
        let Ok(report) = report else {
            return RequestResult::default();
        };
        if report.is_null() {
            return RequestResult::default();
        }
        let mut result = RequestResult::default();
        if let Some(items) = parse_diagnostics(report.get("items")) {
            result
                .by_file
                .entry(path.to_path_buf())
                .or_default()
                .extend(items);
            result.handled = true;
            result.matched = true;
        }
        for (uri, related) in report
            .get("relatedDocuments")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            let (Some(related_path), Some(items)) =
                (file_path(uri), parse_diagnostics(related.get("items")))
            else {
                continue;
            };
            result.matched = result.matched || related_path == path;
            result
                .by_file
                .entry(related_path)
                .or_default()
                .extend(items);
            result.handled = true;
        }
        result
    }

    async fn request_workspace_report(
        &self,
        path: &Path,
        identifier: Option<&str>,
    ) -> RequestResult {
        let mut params = json!({ "previousResultIds": [] });
        if let Some(identifier) = identifier.filter(|i| !i.is_empty()) {
            params["identifier"] = json!(identifier);
        }
        let report = self
            .request(
                "workspace/diagnostic",
                params,
                Some(DIAGNOSTICS_REQUEST_TIMEOUT),
            )
            .await;
        let Ok(report) = report else {
            return RequestResult::default();
        };
        if report.is_null() {
            return RequestResult::default();
        }
        let mut result = RequestResult {
            handled: true,
            ..Default::default()
        };
        for item in report
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let related = item.get("uri").and_then(Value::as_str).and_then(file_path);
            let (Some(related_path), Some(items)) = (related, parse_diagnostics(item.get("items")))
            else {
                continue;
            };
            result.matched = result.matched || related_path == path;
            result
                .by_file
                .entry(related_path)
                .or_default()
                .extend(items);
        }
        result
    }

    fn pull_state(&self, workspace: bool) -> (Vec<String>, usize) {
        let state = lock(&self.state);
        let matching: Vec<&Registration> = state
            .registrations
            .values()
            .filter(|r| r.workspace == workspace)
            .collect();
        let mut identifiers: Vec<String> = Vec::new();
        for id in matching.iter().filter_map(|r| r.identifier.clone()) {
            if !identifiers.contains(&id) {
                identifiers.push(id);
            }
        }
        (identifiers, matching.len())
    }
}

async fn read_loop(stdout: ChildStdout, inner: Arc<Inner>) {
    let mut reader = BufReader::new(stdout);
    loop {
        let mut length: Option<usize> = None;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => {
                    lock(&inner.pending).clear();
                    return;
                }
                Ok(_) => {}
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.trim().eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().ok();
            }
        }
        let Some(length) = length else { continue };
        let mut body = vec![0u8; length];
        if reader.read_exact(&mut body).await.is_err() {
            lock(&inner.pending).clear();
            return;
        }
        if let Ok(message) = serde_json::from_slice::<Value>(&body) {
            inner.dispatch(message).await;
        }
    }
}

pub struct ClientInput {
    pub server_id: String,
    pub root: PathBuf,
    pub directory: PathBuf,
    pub initialization: Option<Value>,
}

impl Client {
    /// Attach to a spawned server (piped stdin/stdout) and run the `initialize` handshake.
    pub async fn create(input: ClientInput, mut child: Child) -> Result<Self, ClientError> {
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.start_kill();
            return Err(ClientError::Request("Process output not available".into()));
        };
        if let Some(mut stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
            });
        }
        let inner = Arc::new(Inner {
            server_id: input.server_id.clone(),
            root: input.root.clone(),
            initialization: input.initialization.clone(),
            stdin: tokio::sync::Mutex::new(Some(stdin)),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            state: Mutex::new(State::default()),
            diagnostic_events: broadcast::channel(256).0,
            registration_events: broadcast::channel(16).0,
        });
        let reader = tokio::spawn(read_loop(stdout, inner.clone()));

        let root_uri = util::path_to_uri(&input.root);
        let params = json!({
            "rootUri": root_uri,
            // The source passes the server's own pid here.
            "processId": child.id(),
            "workspaceFolders": [{ "name": "workspace", "uri": root_uri }],
            "initializationOptions": input.initialization.clone().unwrap_or_else(|| json!({})),
            "capabilities": {
                "window": { "workDoneProgress": true },
                "workspace": {
                    "configuration": true,
                    "didChangeWatchedFiles": { "dynamicRegistration": true },
                    "diagnostics": { "refreshSupport": false },
                },
                "textDocument": {
                    "synchronization": { "didOpen": true, "didChange": true },
                    "diagnostic": { "dynamicRegistration": true, "relatedDocumentSupport": true },
                    "publishDiagnostics": { "versionSupport": false },
                },
            },
        });
        let handshake = async {
            let initialized = inner
                .request("initialize", params, Some(INITIALIZE_TIMEOUT))
                .await?;
            inner.notify("initialized", json!({})).await?;
            if let Some(settings) = &input.initialization {
                inner
                    .notify(
                        "workspace/didChangeConfiguration",
                        json!({ "settings": settings }),
                    )
                    .await?;
            }
            Ok::<Value, ClientError>(initialized)
        };
        let initialized = match handshake.await {
            Ok(value) => value,
            Err(err) => {
                reader.abort();
                stop(&mut child).await;
                return Err(ClientError::Initialize {
                    server_id: input.server_id,
                    cause: err.to_string(),
                });
            }
        };

        let capabilities = initialized.get("capabilities");
        let sync = capabilities.and_then(|c| c.get("textDocumentSync"));
        let sync_kind = match sync {
            Some(Value::Number(n)) => n.as_u64(),
            Some(other) => other.get("change").and_then(Value::as_u64),
            None => None,
        };
        let static_pull = capabilities
            .and_then(|c| c.get("diagnosticProvider"))
            .is_some_and(|v| !matches!(v, Value::Null | Value::Bool(false)));

        Ok(Self {
            inner,
            directory: input.directory,
            child: Mutex::new(Some(child)),
            reader,
            sync_kind,
            static_pull,
            files: Default::default(),
        })
    }

    pub fn server_id(&self) -> &str {
        &self.inner.server_id
    }

    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    fn normalize(&self, path: &Path) -> PathBuf {
        util::normalize_path(&util::absolute(&self.directory, path))
    }

    /// Raw request; errors are surfaced so callers can pick the source's per-operation fallback.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, ClientError> {
        self.inner.request(method, params, None).await
    }

    /// `notify.open`: didOpen on first touch, full-text didChange afterwards. Returns the document version.
    pub async fn open(&self, path: &Path) -> Result<i64, ClientError> {
        let path = self.normalize(path);
        let text = tokio::fs::read_to_string(&path).await?;
        let uri = util::path_to_uri(&path);
        let mut files = self.files.lock().await;

        if let Some((version, previous)) = files.get(&path).cloned() {
            self.inner
                .notify(
                    "workspace/didChangeWatchedFiles",
                    json!({ "changes": [{ "uri": uri, "type": FILE_CHANGE_CHANGED }] }),
                )
                .await?;
            let next = version + 1;
            let changes = if self.sync_kind == Some(TEXT_DOCUMENT_SYNC_INCREMENTAL) {
                json!([{
                    "range": { "start": { "line": 0, "character": 0 }, "end": end_position(&previous) },
                    "text": text,
                }])
            } else {
                json!([{ "text": text }])
            };
            files.insert(path, (next, text));
            self.inner
                .notify(
                    "textDocument/didChange",
                    json!({ "textDocument": { "uri": uri, "version": next }, "contentChanges": changes }),
                )
                .await?;
            return Ok(next);
        }

        self.inner
            .notify(
                "workspace/didChangeWatchedFiles",
                json!({ "changes": [{ "uri": uri, "type": FILE_CHANGE_CREATED }] }),
            )
            .await?;
        {
            let mut state = lock(&self.inner.state);
            state.push.shift_remove(&path);
            state.pull.shift_remove(&path);
        }
        let language = language_id(util::extname(&path.to_string_lossy()));
        self.inner
            .notify(
                "textDocument/didOpen",
                json!({ "textDocument": { "uri": uri, "languageId": language, "version": 0, "text": text } }),
            )
            .await?;
        files.insert(path, (0, text));
        Ok(0)
    }

    /// Push and pull diagnostics merged per file.
    pub fn diagnostics(&self) -> IndexMap<PathBuf, Vec<Diagnostic>> {
        let state = lock(&self.inner.state);
        let mut keys: Vec<&PathBuf> = state.push.keys().collect();
        keys.extend(state.pull.keys().filter(|k| !state.push.contains_key(*k)));
        keys.into_iter()
            .map(|key| {
                let merged = state
                    .push
                    .get(key)
                    .into_iter()
                    .chain(state.pull.get(key))
                    .flatten()
                    .cloned()
                    .collect();
                (key.clone(), dedupe(merged))
            })
            .collect()
    }

    // Identifier pulls run in parallel; return as soon as one batch has diagnostics for this file and
    // let the slower ones keep merging in the background.
    async fn request_document_diagnostics(&self, path: &Path) -> (bool, bool) {
        let (identifiers, registered) = self.inner.pull_state(false);
        if !self.static_pull && registered == 0 {
            return (false, false);
        }
        let requests: Vec<Option<String>> = std::iter::once(None)
            .chain(identifiers.into_iter().map(Some))
            .collect();
        let total = requests.len();
        let (tx, mut rx) = mpsc::unbounded_channel();
        for identifier in requests {
            let (inner, path, tx) = (self.inner.clone(), path.to_path_buf(), tx.clone());
            tokio::spawn(async move {
                let _ = tx.send(inner.request_report(&path, identifier.as_deref()).await);
            });
        }
        drop(tx);
        let (done_tx, done_rx) = oneshot::channel();
        let (inner, path) = (self.inner.clone(), path.to_path_buf());
        tokio::spawn(async move {
            let mut results: Vec<RequestResult> = Vec::new();
            let mut done = Some(done_tx);
            while let Some(result) = rx.recv().await {
                results.push(result);
                let merged = inner.merge_results(&path, &results);
                let current = results
                    .iter()
                    .any(|r| r.by_file.get(&path).is_some_and(|items| !items.is_empty()));
                if (current || results.len() == total)
                    && let Some(tx) = done.take()
                {
                    let _ = tx.send(merged);
                }
            }
        });
        done_rx.await.unwrap_or((false, false))
    }

    async fn request_full_diagnostics(&self, path: &Path) -> (bool, bool) {
        let (document_ids, document_registered) = self.inner.pull_state(false);
        let (workspace_ids, workspace_registered) = self.inner.pull_state(true);
        let document = self.static_pull || document_registered > 0;
        let workspace = workspace_registered > 0;
        if !document && !workspace {
            return (false, false);
        }
        let mut handles = Vec::new();
        let mut launch = |workspace: bool, identifier: Option<String>| {
            let (inner, path) = (self.inner.clone(), path.to_path_buf());
            handles.push(tokio::spawn(async move {
                if workspace {
                    inner
                        .request_workspace_report(&path, identifier.as_deref())
                        .await
                } else {
                    inner.request_report(&path, identifier.as_deref()).await
                }
            }));
        };
        if document {
            launch(false, None);
        }
        document_ids
            .into_iter()
            .for_each(|id| launch(false, Some(id)));
        if workspace {
            launch(true, None);
        }
        workspace_ids
            .into_iter()
            .for_each(|id| launch(true, Some(id)));
        let mut results = Vec::new();
        for handle in handles {
            results.push(handle.await.unwrap_or_default());
        }
        self.inner.merge_results(path, &results)
    }

    // Resolves `true` once a push for this document has been quiet for the debounce window.
    async fn wait_for_fresh_push(
        &self,
        mut events: broadcast::Receiver<PathBuf>,
        path: PathBuf,
        version: i64,
        after: Instant,
        timeout: Duration,
    ) -> bool {
        if timeout.is_zero() {
            return false;
        }
        let deadline = tokio::time::Instant::now() + timeout;
        let schedule = || {
            let (at, hit_version) = *lock(&self.inner.state).published.get(&path)?;
            if hit_version.is_some_and(|v| v != version) {
                return None;
            }
            if at < after && hit_version != Some(version) {
                return None;
            }
            Some(tokio::time::Instant::from_std(at + DIAGNOSTICS_DEBOUNCE))
        };
        let mut debounce = schedule();
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => return false,
                _ = tokio::time::sleep_until(debounce.unwrap_or(deadline)), if debounce.is_some() => return true,
                event = events.recv() => match event {
                    Ok(event) if event != path => {}
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {
                        if let Some(next) = schedule() {
                            debounce = Some(next);
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return false,
                },
            }
        }
    }

    /// Wait until the server has produced diagnostics for `path` at `version` (document ≤ 5 s, full ≤ 10 s).
    pub async fn wait_for_diagnostics(
        &self,
        path: &Path,
        version: i64,
        mode: DiagnosticsMode,
        after: Option<Instant>,
    ) {
        let path = self.normalize(path);
        let limit = match mode {
            DiagnosticsMode::Document => DIAGNOSTICS_DOCUMENT_WAIT_TIMEOUT,
            DiagnosticsMode::Full => DIAGNOSTICS_FULL_WAIT_TIMEOUT,
        };
        let started = after.unwrap_or_else(Instant::now);
        let events = self.inner.diagnostic_events.subscribe();
        let push_wait = self.wait_for_fresh_push(events, path.clone(), version, started, limit);
        tokio::pin!(push_wait);

        while started.elapsed() < limit {
            let (handled, matched) = match mode {
                DiagnosticsMode::Document => self.request_document_diagnostics(&path).await,
                DiagnosticsMode::Full => self.request_full_diagnostics(&path).await,
            };
            if matched || (mode == DiagnosticsMode::Full && handled) {
                return;
            }
            let Some(remaining) = limit
                .checked_sub(started.elapsed())
                .filter(|d| !d.is_zero())
            else {
                return;
            };
            let mut registrations = self.inner.registration_events.subscribe();
            let registered = tokio::select! {
                _ = &mut push_wait => false,
                changed = tokio::time::timeout(remaining, registrations.recv()) => changed.is_ok(),
            };
            if !registered {
                return;
            }
        }
    }

    /// The source ends the connection and stops the process without LSP `shutdown`/`exit`.
    pub async fn shutdown(&self) {
        self.inner.stdin.lock().await.take();
        self.reader.abort();
        lock(&self.inner.pending).clear();
        let child = lock(&self.child).take();
        if let Some(mut child) = child {
            stop(&mut child).await;
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.reader.abort();
        if let Some(child) = lock(&self.child).as_mut() {
            let _ = child.start_kill();
        }
    }
}

pub(crate) async fn stop(child: &mut Child) {
    // Servers launched through .cmd shims leave the real process as a grandchild on Windows.
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        let pid = pid.to_string();
        util::run(
            "taskkill",
            &["/T", "/F", "/PID", &pid],
            util::RunOptions::default(),
        )
        .await;
    }
    let _ = child.kill().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_position_counts_utf16() {
        assert_eq!(end_position(""), json!({ "line": 0, "character": 0 }));
        assert_eq!(
            end_position("a\r\nbc\rd\n"),
            json!({ "line": 3, "character": 0 })
        );
        assert_eq!(end_position("x\n😀y"), json!({ "line": 1, "character": 3 }));
    }

    #[test]
    fn configuration_lookup() {
        let settings = json!({ "a": { "b": 1 }, "c": null });
        assert_eq!(configuration_value(Some(&settings), None), settings);
        assert_eq!(configuration_value(Some(&settings), Some("a.b")), json!(1));
        assert_eq!(
            configuration_value(Some(&settings), Some("a.x.y")),
            Value::Null
        );
        assert_eq!(configuration_value(Some(&settings), Some("c")), Value::Null);
        assert_eq!(configuration_value(None, None), Value::Null);
    }
}
