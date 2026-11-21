//! Bun sidecar process and its JSON-RPC transport. Protocol: see `lib.rs`.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use opencode_tools::{PermissionRequest, Tool};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

use crate::discover::{self, PluginSpec};
use crate::hooks::{Event, Hook, HookName};
use crate::tool::PluginTool;

pub const HOST_SCRIPT: &str = include_str!("../js/host.mjs");
const STDERR_TAIL: usize = 20;

pub type LogFn = Arc<dyn Fn(&str) + Send + Sync>;
/// Answers the sidecar's `auth.get {provider}` (the `getAuth` argument of `auth.loader`) with the stored credential.
pub type AuthFn = Arc<dyn Fn(&str) -> Option<Value> + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("plugins need Bun: {hint}")]
    BunNotFound { hint: String },
    #[error("failed to start plugin host: {0}")]
    Spawn(String),
    #[error("plugin host request `{method}` timed out after {timeout:?}")]
    Timeout { method: String, timeout: Duration },
    #[error("plugin host crashed: {0}")]
    Crashed(String),
    #[error("plugin host is shut down")]
    Closed,
    #[error("{message}")]
    Remote { method: String, code: i64, message: String, data: Box<Value> },
    #[error("plugin host protocol error: {0}")]
    Protocol(String),
}

#[derive(Clone)]
pub struct PluginOptions {
    pub specs: Vec<PluginSpec>,
    pub tool_files: Vec<PathBuf>,
    pub directory: PathBuf,
    pub worktree: PathBuf,
    pub project_id: String,
    /// Full v1 `Project` snapshot handed to plugins; defaults to `{id, worktree}`.
    pub project: Option<Value>,
    pub server_url: String,
    /// Extra SDK client headers (e.g. `Authorization` from [`basic_auth_header`]).
    pub server_headers: BTreeMap<String, String>,
    /// opencode cache directory: host script goes to `<cache_dir>/plugin-host`, npm plugins are read from
    /// `<cache_dir>/packages`.
    pub cache_dir: PathBuf,
    pub bun_path: Option<PathBuf>,
    /// Directories searched (after a tool's own directory) for `@opencode-ai/sdk` and `zod`; pass the config dirs.
    pub resolve_dirs: Vec<PathBuf>,
    /// Module path or URL exporting `createOpencodeClient`; overrides resolution of `@opencode-ai/sdk`.
    pub sdk_path: Option<String>,
    /// Module path or URL of Zod v4, used when `zod` cannot be resolved next to a tool.
    pub zod_path: Option<String>,
    /// Checked against npm plugins' `engines.opencode`.
    pub opencode_version: Option<String>,
    pub env: Vec<(String, String)>,
    pub init_timeout: Duration,
    pub request_timeout: Duration,
    pub tool_timeout: Option<Duration>,
    pub shutdown_timeout: Duration,
    pub log: Option<LogFn>,
    pub auth: Option<AuthFn>,
}

impl Default for PluginOptions {
    fn default() -> Self {
        Self {
            specs: Vec::new(),
            tool_files: Vec::new(),
            directory: PathBuf::new(),
            worktree: PathBuf::new(),
            project_id: String::new(),
            project: None,
            server_url: "http://localhost:4096".to_string(),
            server_headers: BTreeMap::new(),
            cache_dir: PathBuf::new(),
            bun_path: None,
            resolve_dirs: Vec::new(),
            sdk_path: None,
            zod_path: None,
            opencode_version: None,
            env: Vec::new(),
            init_timeout: Duration::from_secs(60),
            request_timeout: Duration::from_secs(120),
            tool_timeout: None,
            shutdown_timeout: Duration::from_secs(5),
            log: None,
            auth: None,
        }
    }
}

/// `Authorization` value of `ServerAuth.header` (`opencode/src/server/auth.ts:36-42`).
pub fn basic_auth_header(username: Option<&str>, password: &str) -> String {
    format!("Basic {}", BASE64.encode(format!("{}:{password}", username.unwrap_or("opencode"))))
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct LoadedPlugin {
    pub index: usize,
    pub spec: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub hooks: Vec<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub auth: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ToolInfo {
    pub id: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub parameters: Value,
    /// `zod`, `legacy` (raw JSON-Schema properties) or `fallback` (no Zod module resolvable).
    #[serde(default)]
    pub converter: String,
}

/// `stage`: `install` | `entry` | `missing` | `compatibility` | `load` | `apply` | `tool`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct LoadError {
    pub stage: String,
    pub spec: String,
    pub message: String,
}

impl LoadError {
    /// The `session.error` text the original publishes (`opencode/src/plugin/index.ts:193-215`).
    pub fn session_message(&self) -> String {
        match self.stage.as_str() {
            "install" => {
                let (pkg, version) = discover::parse_plugin_specifier(&self.spec);
                format!("Failed to install plugin {pkg}@{version}: {}", self.message)
            }
            "compatibility" => format!("Plugin {} skipped: {}", self.spec, self.message),
            _ => format!("Failed to load plugin {}: {}", self.spec, self.message),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct InitResult {
    #[serde(default)]
    pub bun: String,
    /// Resolved SDK module, `None` when `client` is the throwing placeholder.
    #[serde(default)]
    pub sdk: Option<String>,
    #[serde(default)]
    pub plugins: Vec<LoadedPlugin>,
    #[serde(default)]
    pub hooks: Vec<String>,
    #[serde(default)]
    pub tools: Vec<ToolInfo>,
    #[serde(default)]
    pub workspaces: Vec<String>,
    #[serde(default)]
    pub errors: Vec<LoadError>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct AuthLoaded {
    #[serde(default)]
    pub options: Map<String, Value>,
    /// Function-valued loader options by key (e.g. `fetch`) → handle for [`PluginHost::fetch`].
    #[serde(default)]
    pub functions: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct FetchRequest {
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

pub struct FetchResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    /// Body chunks in order; the channel closes at end of body.
    pub body: mpsc::UnboundedReceiver<Result<Vec<u8>, PluginError>>,
}

impl FetchResponse {
    pub async fn bytes(mut self) -> Result<Vec<u8>, PluginError> {
        let mut out = Vec::new();
        while let Some(chunk) = self.body.recv().await {
            out.extend(chunk?);
        }
        Ok(out)
    }
}

pub(crate) struct Ask {
    pub id: Value,
    pub request: PermissionRequest,
}

type Reply = Result<Value, PluginError>;
type Chunk = Result<Vec<u8>, PluginError>;

pub(crate) struct Inner {
    writer: mpsc::UnboundedSender<String>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Reply>>>,
    asks: Mutex<HashMap<u64, mpsc::UnboundedSender<Ask>>>,
    streams: Mutex<HashMap<u64, mpsc::UnboundedSender<Chunk>>>,
    next_id: AtomicU64,
    dead: Mutex<Option<String>>,
    closing: AtomicBool,
    stderr: Mutex<VecDeque<String>>,
    child: tokio::sync::Mutex<Child>,
    pid: Option<u32>,
    log: Option<LogFn>,
    auth: Option<AuthFn>,
    pub(crate) request_timeout: Duration,
    pub(crate) tool_timeout: Option<Duration>,
    shutdown_timeout: Duration,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Inner {
    fn log(&self, line: &str) {
        if let Some(log) = &self.log {
            log(line);
        }
    }

    fn gone(&self) -> Option<PluginError> {
        if let Some(message) = lock(&self.dead).clone() {
            return Some(if self.closing.load(Ordering::SeqCst) {
                PluginError::Closed
            } else {
                PluginError::Crashed(message)
            });
        }
        self.closing.load(Ordering::SeqCst).then_some(PluginError::Closed)
    }

    fn write(&self, message: Value) {
        let _ = self.writer.send(message.to_string());
    }

    pub(crate) fn notify(&self, method: &str, params: Value) {
        self.write(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    pub(crate) fn respond(&self, id: Value, result: Result<Value, String>) {
        self.write(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(message) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": message}}),
        });
    }

    pub(crate) fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    pub(crate) fn begin(&self, id: u64, method: &str, params: Value) -> Result<oneshot::Receiver<Reply>, PluginError> {
        if let Some(error) = self.gone() {
            return Err(error);
        }
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(id, tx);
        if let Some(message) = lock(&self.dead).clone() {
            lock(&self.pending).remove(&id);
            return Err(PluginError::Crashed(message));
        }
        self.write(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        Ok(rx)
    }

    pub(crate) fn cancel(&self, id: u64) {
        lock(&self.pending).remove(&id);
        lock(&self.asks).remove(&id);
        self.notify("$/cancel", json!({"id": id}));
    }

    pub(crate) fn register_asks(&self, id: u64, tx: mpsc::UnboundedSender<Ask>) {
        lock(&self.asks).insert(id, tx);
    }

    pub(crate) fn unregister_asks(&self, id: u64) {
        lock(&self.asks).remove(&id);
    }

    pub(crate) fn lost(&self) -> PluginError {
        self.gone().unwrap_or(PluginError::Closed)
    }

    pub(crate) async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, PluginError> {
        let id = self.next_id();
        let rx = self.begin(id, method, params)?;
        let reply = match timeout {
            None => rx.await,
            Some(limit) => match tokio::time::timeout(limit, rx).await {
                Ok(reply) => reply,
                Err(_) => {
                    self.cancel(id);
                    return Err(PluginError::Timeout { method: method.to_string(), timeout: limit });
                }
            },
        };
        match reply {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(PluginError::Remote { code, message, data, .. })) => {
                Err(PluginError::Remote { method: method.to_string(), code, message, data })
            }
            Ok(Err(error)) => Err(error),
            Err(_) => Err(self.lost()),
        }
    }

    fn dispatch(&self, line: &str) {
        let message = match serde_json::from_str::<Value>(line) {
            Ok(Value::Object(map)) if map.contains_key("jsonrpc") => map,
            _ => {
                if !line.trim().is_empty() {
                    self.log(&format!("[stdout] {line}"));
                }
                return;
            }
        };
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match (message.get("method").and_then(Value::as_str), message.get("id")) {
            (Some(method), Some(id)) if !id.is_null() => self.incoming(method, id.clone(), params),
            (Some(method), _) => self.notification(method, params),
            (None, Some(id)) => {
                let Some(tx) = id.as_u64().and_then(|id| lock(&self.pending).remove(&id)) else {
                    return;
                };
                let reply = match message.get("error") {
                    Some(error) if !error.is_null() => Err(PluginError::Remote {
                        method: String::new(),
                        code: error.get("code").and_then(Value::as_i64).unwrap_or(-32000),
                        message: error.get("message").and_then(Value::as_str).unwrap_or("plugin error").to_string(),
                        data: Box::new(error.get("data").cloned().unwrap_or(Value::Null)),
                    }),
                    _ => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = tx.send(reply);
            }
            _ => {}
        }
    }

    fn incoming(&self, method: &str, id: Value, params: Value) {
        match method {
            "tool.ask" => {
                let strings = |key: &str| -> Vec<String> {
                    params
                        .get(key)
                        .and_then(Value::as_array)
                        .map(|list| list.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                        .unwrap_or_default()
                };
                let request = PermissionRequest {
                    permission: params.get("permission").and_then(Value::as_str).unwrap_or_default().to_string(),
                    patterns: strings("patterns"),
                    always: strings("always"),
                    metadata: params.get("metadata").cloned().unwrap_or_else(|| json!({})),
                };
                let call = params.get("requestID").and_then(Value::as_u64);
                let tx = call.and_then(|call| lock(&self.asks).get(&call).cloned());
                let ask = Ask { id: id.clone(), request };
                if tx.is_none_or(|tx| tx.send(ask).is_err()) {
                    self.respond(id, Err("no active tool call for this permission request".to_string()));
                }
            }
            "auth.get" => {
                let provider = params.get("provider").and_then(Value::as_str).unwrap_or_default();
                match &self.auth {
                    Some(auth) => self.respond(id, Ok(auth(provider).unwrap_or(Value::Null))),
                    None => self.respond(id, Err("auth.get is not available".to_string())),
                }
            }
            other => self.respond(id, Err(format!("Method not found: {other}"))),
        }
    }

    fn notification(&self, method: &str, params: Value) {
        let stream = params.get("stream").and_then(Value::as_u64);
        match (method, stream) {
            ("fetch.chunk", Some(stream)) => {
                let Some(tx) = lock(&self.streams).get(&stream).cloned() else {
                    return;
                };
                let data = params.get("data").and_then(Value::as_str).unwrap_or_default();
                let chunk = BASE64
                    .decode(data)
                    .map_err(|e| PluginError::Protocol(format!("fetch.chunk is not base64: {e}")));
                if tx.send(chunk).is_err() {
                    lock(&self.streams).remove(&stream);
                    self.notify("fetch.cancel", json!({"stream": stream}));
                }
            }
            ("fetch.end", Some(stream)) => {
                let Some(tx) = lock(&self.streams).remove(&stream) else {
                    return;
                };
                if let Some(error) = params.get("error").and_then(Value::as_str) {
                    let _ = tx.send(Err(PluginError::Remote {
                        method: "fetch.proxy".to_string(),
                        code: -32000,
                        message: error.to_string(),
                        data: Box::new(Value::Null),
                    }));
                }
            }
            _ => {}
        }
    }

    async fn exited(&self) {
        let status = {
            let mut child = self.child.lock().await;
            match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
                Ok(Ok(status)) => status.to_string(),
                _ => "stdout closed".to_string(),
            }
        };
        let tail = lock(&self.stderr).iter().cloned().collect::<Vec<_>>().join("\n");
        let message = if tail.is_empty() { status } else { format!("{status}\n{tail}") };
        let closing = self.closing.load(Ordering::SeqCst);
        *lock(&self.dead) = Some(message.clone());
        let fail = || if closing { PluginError::Closed } else { PluginError::Crashed(message.clone()) };
        for (_, tx) in lock(&self.pending).drain() {
            let _ = tx.send(Err(fail()));
        }
        lock(&self.asks).clear();
        for (_, tx) in lock(&self.streams).drain() {
            let _ = tx.send(Err(fail()));
        }
    }
}

async fn read_lines<R: AsyncRead + Unpin>(reader: R, mut each: impl FnMut(&str)) {
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => each(String::from_utf8_lossy(&buf).trim_end_matches(['\r', '\n'])),
        }
    }
}

pub(crate) fn kill_tree(pid: u32) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let taskkill = std::env::var_os("SystemRoot")
            .map(|root| PathBuf::from(root).join("System32").join("taskkill.exe"))
            .filter(|path| path.exists())
            .unwrap_or_else(|| PathBuf::from("taskkill"));
        let _ = std::process::Command::new(taskkill)
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x0800_0000)
            .status();
    }
    #[cfg(unix)]
    unsafe {
        libc::killpg(pid as libc::pid_t, libc::SIGKILL);
    }
}

fn write_host_script(cache_dir: &Path) -> Result<PathBuf, PluginError> {
    let dir = cache_dir.join("plugin-host");
    let file = dir.join("host.mjs");
    if std::fs::read_to_string(&file).is_ok_and(|current| current == HOST_SCRIPT) {
        return Ok(file);
    }
    std::fs::create_dir_all(&dir)
        .and_then(|_| std::fs::write(&file, HOST_SCRIPT))
        .map_err(|e| PluginError::Spawn(format!("cannot write {}: {e}", file.display())))?;
    Ok(file)
}

/// Handle to the plugin sidecar. Inert (no process, no JS) when started without plugins or tool files.
pub struct PluginHost {
    inner: Option<Arc<Inner>>,
    info: InitResult,
}

impl PluginHost {
    pub async fn start(options: PluginOptions) -> Result<PluginHost, PluginError> {
        if options.specs.is_empty() && options.tool_files.is_empty() {
            return Ok(PluginHost { inner: None, info: InitResult::default() });
        }
        let bun = discover::find_bun(options.bun_path.as_deref())?;
        let script = write_host_script(&options.cache_dir)?;

        let mut command = Command::new(&bun);
        command
            .arg("--no-install")
            .arg("run")
            .arg(&script)
            .current_dir(&options.directory)
            .envs(options.env.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child =
            command.spawn().map_err(|e| PluginError::Spawn(format!("{}: {e}", bun.display())))?;
        let pid = child.id();
        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let (writer, mut outgoing) = mpsc::unbounded_channel::<String>();
        let inner = Arc::new(Inner {
            writer,
            pending: Mutex::default(),
            asks: Mutex::default(),
            streams: Mutex::default(),
            next_id: AtomicU64::new(1),
            dead: Mutex::default(),
            closing: AtomicBool::new(false),
            stderr: Mutex::default(),
            child: tokio::sync::Mutex::new(child),
            pid,
            log: options.log.clone(),
            auth: options.auth.clone(),
            request_timeout: options.request_timeout,
            tool_timeout: options.tool_timeout,
            shutdown_timeout: options.shutdown_timeout,
        });

        tokio::spawn(async move {
            while let Some(mut line) = outgoing.recv().await {
                line.push('\n');
                if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                    return;
                }
            }
        });
        let weak = Arc::downgrade(&inner);
        tokio::spawn(async move {
            read_lines(stderr, |line| {
                if let Some(inner) = weak.upgrade() {
                    let mut tail = lock(&inner.stderr);
                    if tail.len() == STDERR_TAIL {
                        tail.pop_front();
                    }
                    tail.push_back(line.to_string());
                    drop(tail);
                    inner.log(line);
                }
            })
            .await;
        });
        let reader = inner.clone();
        tokio::spawn(async move {
            read_lines(stdout, |line| reader.dispatch(line)).await;
            reader.exited().await;
        });

        let mut host = PluginHost { inner: Some(inner.clone()), info: InitResult::default() };
        let plugins: Vec<Value> = options
            .specs
            .iter()
            .map(|item| {
                let npm = discover::npm_target(&item.spec, &options.cache_dir)
                    .map(|target| json!({"name": target.name, "dir": target.dir}));
                json!({"spec": item.spec, "options": item.options, "npm": npm})
            })
            .collect();
        let project = options
            .project
            .clone()
            .unwrap_or_else(|| json!({"id": options.project_id, "worktree": options.worktree}));
        let params = json!({
            "directory": options.directory,
            "worktree": options.worktree,
            "project": project,
            "serverUrl": options.server_url,
            "headers": if options.server_headers.is_empty() { Value::Null } else { json!(options.server_headers) },
            "plugins": plugins,
            "toolFiles": options.tool_files,
            "resolveDirs": options.resolve_dirs,
            "sdkPath": options.sdk_path,
            "zodPath": options.zod_path,
            "opencodeVersion": options.opencode_version,
        });
        let result = match inner.request("init", params, Some(options.init_timeout)).await {
            Ok(result) => result,
            Err(error) => {
                host.shutdown().await;
                return Err(error);
            }
        };
        let info = serde_json::from_value::<InitResult>(result)
            .map_err(|e| PluginError::Protocol(format!("invalid init result: {e}")));
        match info {
            Ok(info) => {
                host.info = info;
                Ok(host)
            }
            Err(error) => {
                host.shutdown().await;
                Err(error)
            }
        }
    }

    /// False for the inert host created when nothing is configured.
    pub fn is_active(&self) -> bool {
        self.inner.is_some()
    }

    pub fn pid(&self) -> Option<u32> {
        self.inner.as_ref().and_then(|inner| inner.pid)
    }

    pub fn info(&self) -> &InitResult {
        &self.info
    }

    /// Whether any loaded plugin declares this hook (`event`, `config` and trigger names alike).
    pub fn has_hook(&self, name: &str) -> bool {
        self.info.hooks.iter().any(|hook| hook == name)
    }

    fn active(&self) -> Result<&Arc<Inner>, PluginError> {
        self.inner.as_ref().ok_or(PluginError::Closed)
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, PluginError> {
        let inner = self.active()?;
        inner.request(method, params, Some(inner.request_timeout)).await
    }

    /// Runs every plugin's hook in load order and returns the mutated `output`. No round trip when no plugin
    /// declares the hook. A throwing hook fails the call, like the original's defect.
    pub async fn trigger(&self, name: HookName, input: Value, output: Value) -> Result<Value, PluginError> {
        let Some(inner) = &self.inner else {
            return Ok(output);
        };
        if let Some(error) = inner.gone() {
            return Err(error);
        }
        if !self.has_hook(name.as_str()) {
            return Ok(output);
        }
        let params = json!({"name": name.as_str(), "input": input, "output": output});
        let mut result = inner.request("hook.trigger", params, Some(inner.request_timeout)).await?;
        Ok(result.get_mut("output").map(Value::take).unwrap_or(Value::Null))
    }

    pub async fn trigger_typed<H: Hook>(&self, input: &H::Input, output: H::Output) -> Result<H::Output, PluginError> {
        if !self.has_hook(H::NAME.as_str()) && self.inner.as_ref().is_none_or(|inner| inner.gone().is_none()) {
            return Ok(output);
        }
        let encode = |e: serde_json::Error| PluginError::Protocol(format!("{}: {e}", H::NAME));
        let input = serde_json::to_value(input).map_err(encode)?;
        let output = serde_json::to_value(&output).map_err(encode)?;
        serde_json::from_value(self.trigger(H::NAME, input, output).await?).map_err(encode)
    }

    /// The `config` hook: every plugin sees and may mutate the config; returns the result.
    pub async fn config(&self, config: Value) -> Result<Value, PluginError> {
        if !self.has_hook("config") {
            return Ok(config);
        }
        let mut result = self.call("config", json!({"config": config})).await?;
        Ok(result.get_mut("config").map(Value::take).unwrap_or(Value::Null))
    }

    /// Fire-and-forget bus event.
    pub fn event(&self, event: &Event) -> Result<(), PluginError> {
        let Some(inner) = &self.inner else {
            return Ok(());
        };
        if let Some(error) = inner.gone() {
            return Err(error);
        }
        if self.has_hook("event") {
            inner.notify("event", json!({"event": event}));
        }
        Ok(())
    }

    /// Custom tool files first, then plugin `tool` entries; a later duplicate id replaces an earlier one.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let Some(inner) = &self.inner else {
            return Vec::new();
        };
        self.info
            .tools
            .iter()
            .map(|info| Arc::new(PluginTool::new(inner.clone(), info.clone())) as Arc<dyn Tool>)
            .collect()
    }

    /// `[{plugin, provider, loader, methods: [{type, label, authorize, prompts?}]}]`.
    pub async fn auth_list(&self) -> Result<Value, PluginError> {
        if self.inner.is_none() {
            return Ok(json!([]));
        }
        self.call("auth.list", Value::Null).await
    }

    /// Result of the method's `authorize(inputs)`; a function-valued `callback` is replaced by `handle`.
    pub async fn auth_authorize(&self, provider: &str, method: usize, inputs: Option<Value>) -> Result<Value, PluginError> {
        self.call("auth.authorize", json!({"provider": provider, "method": method, "inputs": inputs})).await
    }

    pub async fn auth_callback(&self, handle: &str, code: Option<&str>) -> Result<Value, PluginError> {
        self.call("auth.callback", json!({"handle": handle, "code": code})).await
    }

    /// Runs `auth.loader(getAuth, provider_info)`; `auth` is the fallback `getAuth` result when no [`AuthFn`] is set.
    pub async fn auth_loader(&self, provider: &str, auth: Value, provider_info: Value) -> Result<AuthLoaded, PluginError> {
        let params = json!({"provider": provider, "auth": auth, "providerInfo": provider_info});
        serde_json::from_value(self.call("auth.loader", params).await?)
            .map_err(|e| PluginError::Protocol(format!("invalid auth.loader result: {e}")))
    }

    pub async fn provider_list(&self) -> Result<Value, PluginError> {
        if self.inner.is_none() {
            return Ok(json!([]));
        }
        self.call("provider.list", Value::Null).await
    }

    pub async fn provider_models(&self, id: &str, provider: Value, auth: Option<Value>) -> Result<Value, PluginError> {
        self.call("provider.models", json!({"id": id, "provider": provider, "auth": auth})).await
    }

    /// Performs a request through a plugin-provided `fetch` (handle from [`AuthLoaded::functions`]). Resolves when
    /// the response head arrives; the body streams afterwards. No timeout is applied here.
    pub async fn fetch(&self, handle: &str, request: FetchRequest) -> Result<FetchResponse, PluginError> {
        let inner = self.active()?;
        let stream = inner.next_id();
        let (tx, body) = mpsc::unbounded_channel();
        lock(&inner.streams).insert(stream, tx);
        let params = json!({
            "handle": handle,
            "stream": stream,
            "url": request.url,
            "method": request.method,
            "headers": request.headers,
            "body": request.body.as_deref().map(|body| BASE64.encode(body)),
        });
        let head = match inner.request("fetch.proxy", params, None).await {
            Ok(head) => head,
            Err(error) => {
                lock(&inner.streams).remove(&stream);
                return Err(error);
            }
        };
        let headers = head.get("headers").cloned().unwrap_or_else(|| json!([]));
        Ok(FetchResponse {
            status: head.get("status").and_then(Value::as_u64).unwrap_or(0) as u16,
            status_text: head.get("statusText").and_then(Value::as_str).unwrap_or_default().to_string(),
            headers: serde_json::from_value(headers).unwrap_or_default(),
            body,
        })
    }

    /// Runs plugin `dispose` hooks, then kills the sidecar and everything it spawned. Idempotent.
    pub async fn shutdown(&self) {
        let Some(inner) = &self.inner else {
            return;
        };
        if lock(&inner.dead).is_none() && !inner.closing.load(Ordering::SeqCst) {
            let _ = inner.request("shutdown", Value::Null, Some(inner.shutdown_timeout)).await;
        }
        if inner.closing.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(pid) = inner.pid.filter(|_| lock(&inner.dead).is_none()) {
            let _ = tokio::task::spawn_blocking(move || kill_tree(pid)).await;
        }
        let mut child = inner.child.lock().await;
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    }
}

impl Drop for PluginHost {
    fn drop(&mut self) {
        let Some(inner) = &self.inner else {
            return;
        };
        if inner.closing.swap(true, Ordering::SeqCst) || lock(&inner.dead).is_some() {
            return;
        }
        if let Some(pid) = inner.pid {
            kill_tree(pid);
        }
    }
}
