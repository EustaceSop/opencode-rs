pub mod client;
pub mod diagnostic;
mod install;
pub mod language;
pub mod server;

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{OnceCell, broadcast};

pub use client::{Client, ClientError, ClientInput, DiagnosticsMode};
pub use diagnostic::{Diagnostic, Position, Range};
pub use install::{InstallState, InstallStatus};
pub use server::{Context, Launch, Root, SERVERS, ServerDef};

use crate::util;
use install::{Installer, Stop};

/// `write.ts` `MAX_PROJECT_DIAGNOSTICS_FILES`.
pub const MAX_PROJECT_DIAGNOSTICS_FILES: usize = 5;

// SymbolKind: Class, Function, Method, Interface, Variable, Constant, Struct, Enum.
const KINDS: [u64; 8] = [5, 12, 6, 11, 13, 14, 23, 10];

/// One row of `GET /lsp`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub id: String,
    pub name: String,
    pub root: String,
    pub status: String,
}

/// Zero-based position, as the LSP service takes it (the `lsp` tool subtracts 1 from its 1-based input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub file: PathBuf,
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Entry {
    disabled: Option<bool>,
    command: Option<Vec<String>>,
    extensions: Option<Vec<String>>,
    env: Option<HashMap<String, String>>,
    initialization: Option<Value>,
}

#[derive(Debug, Clone)]
enum Spawn {
    Builtin(&'static str),
    Custom {
        command: Vec<String>,
        env: HashMap<String, String>,
        initialization: Option<Value>,
    },
}

#[derive(Debug, Clone)]
struct Server {
    id: String,
    extensions: Vec<String>,
    root: Root,
    spawn: Spawn,
}

type Slot = Arc<OnceCell<Option<Arc<Client>>>>;

#[derive(Default)]
struct State {
    clients: Vec<Arc<Client>>,
    broken: HashSet<String>,
    spawning: HashMap<String, Slot>,
    // Server id and root of every install that ran and failed.
    failed: Vec<(String, PathBuf)>,
}

pub struct Lsp {
    ctx: Context,
    servers: IndexMap<String, Server>,
    installer: Installer,
    state: Mutex<State>,
    updates: broadcast::Sender<()>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn text_document(file: &Path) -> Value {
    json!({ "uri": util::path_to_uri(file) })
}

fn position_params(input: &Location) -> Value {
    json!({
        "textDocument": text_document(&input.file),
        "position": { "line": input.line, "character": input.character },
    })
}

// `results.flat().filter(Boolean)`.
fn flatten(results: Vec<Value>) -> Vec<Value> {
    let truthy = |v: &Value| match v {
        Value::Null | Value::Bool(false) => false,
        Value::String(s) => !s.is_empty(),
        Value::Number(n) => n.as_f64() != Some(0.0),
        _ => true,
    };
    results
        .into_iter()
        .flat_map(|item| match item {
            Value::Array(items) => items,
            other => vec![other],
        })
        .filter(truthy)
        .collect()
}

// Node `path.relative`.
fn relative(from: &Path, to: &Path) -> String {
    let (from, to) = (util::lexical_normalize(from), util::lexical_normalize(to));
    let from: Vec<Component> = from.components().collect();
    let to: Vec<Component> = to.components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in common..from.len() {
        out.push("..");
    }
    for part in &to[common..] {
        out.push(part.as_os_str());
    }
    out.to_string_lossy().into_owned()
}

impl Lsp {
    /// `config` is the raw `lsp` config value: absent/false ⇒ no servers, `true` ⇒ built-ins, object ⇒ overrides.
    pub fn new(config: Option<&Value>, ctx: Context) -> Self {
        Self::with_installer(config, ctx, Installer::from_env())
    }

    fn with_installer(config: Option<&Value>, ctx: Context, installer: Installer) -> Self {
        let mut servers: IndexMap<String, Server> = IndexMap::new();
        let on = match config {
            None | Some(Value::Null) | Some(Value::Bool(false)) => false,
            Some(Value::Number(n)) => n.as_f64() != Some(0.0),
            Some(Value::String(s)) => !s.is_empty(),
            Some(_) => true,
        };
        if on {
            for def in SERVERS {
                // ty and pyright are mutually exclusive.
                if def.id == if ctx.experimental_ty { "pyright" } else { "ty" } {
                    continue;
                }
                servers.insert(
                    def.id.to_string(),
                    Server {
                        id: def.id.to_string(),
                        extensions: def.extensions.iter().map(|s| s.to_string()).collect(),
                        root: def.root,
                        spawn: Spawn::Builtin(def.id),
                    },
                );
            }
        }
        if let Some(Value::Object(map)) = config {
            for (name, raw) in map {
                let item: Entry = serde_json::from_value(raw.clone()).unwrap_or_default();
                if item.disabled == Some(true) {
                    servers.shift_remove(name);
                    continue;
                }
                let existing = servers.get(name);
                let server = Server {
                    id: name.clone(),
                    root: existing.map_or(Root::Directory, |s| s.root),
                    extensions: item
                        .extensions
                        .or_else(|| existing.map(|s| s.extensions.clone()))
                        .unwrap_or_default(),
                    spawn: Spawn::Custom {
                        command: item.command.unwrap_or_default(),
                        env: item.env.unwrap_or_default(),
                        initialization: item.initialization,
                    },
                };
                servers.insert(name.clone(), server);
            }
        }
        Self {
            ctx,
            servers,
            installer,
            state: Mutex::new(State::default()),
            updates: broadcast::channel(64).0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn server_ids(&self) -> Vec<&str> {
        self.servers.keys().map(String::as_str).collect()
    }

    /// Fires once per newly connected client; the server publishes `lsp.updated` for each.
    pub fn subscribe_updates(&self) -> broadcast::Receiver<()> {
        self.updates.subscribe()
    }

    fn absolute(&self, file: &Path) -> PathBuf {
        util::absolute(&self.ctx.directory, file)
    }

    // `containsPath`: non-git projects use "/" as worktree, which must not match everything.
    fn contains(&self, file: &Path) -> bool {
        if util::contains(&self.ctx.directory, file) {
            return true;
        }
        let worktree = &self.ctx.worktree;
        if worktree.as_os_str().is_empty() || worktree.parent().is_none() {
            return false;
        }
        util::contains(worktree, file)
    }

    // `path.parse(file).ext || file`; an empty extension list matches every file.
    fn matching(&self, file: &Path) -> Vec<(&Server, PathBuf, String)> {
        let text = file.to_string_lossy();
        let ext = util::extname(&text);
        let extension = if ext.is_empty() { &*text } else { ext };
        self.servers
            .values()
            .filter(|server| {
                server.extensions.is_empty() || server.extensions.iter().any(|e| e == extension)
            })
            .filter_map(|server| {
                let root = server.root.resolve(file, &self.ctx)?;
                let key = format!("{}{}", root.to_string_lossy(), server.id);
                Some((server, root, key))
            })
            .collect()
    }

    async fn schedule(&self, server: &Server, root: &Path) -> Option<Arc<Client>> {
        let launch = match &server.spawn {
            Spawn::Builtin(id) => {
                let env = self.installer.env(&self.ctx);
                match server::resolve_with(id, root, env).await {
                    Some(launch) => launch,
                    // Missing: acquire it the way the source's `spawn` does, then start it.
                    None => match self.installer.ensure(id, root, &self.ctx).await {
                        Ok(launch) => launch,
                        Err(Stop::Unavailable(_)) => return None,
                        Err(Stop::Failed(_)) => {
                            lock(&self.state)
                                .failed
                                .push((server.id.clone(), root.to_path_buf()));
                            return None;
                        }
                    },
                }
            }
            Spawn::Custom {
                command,
                env,
                initialization,
            } => Launch {
                command: command.clone(),
                env: env.clone(),
                initialization: initialization.clone(),
            },
        };
        let (program, args) = launch.command.split_first()?;
        let child = util::command(program)
            .args(args)
            .current_dir(root)
            .envs(&launch.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;
        let input = ClientInput {
            server_id: server.id.clone(),
            root: root.to_path_buf(),
            directory: self.ctx.directory.clone(),
            initialization: launch.initialization,
        };
        Client::create(input, child).await.ok().map(Arc::new)
    }

    // The client for one server and root, spawned (and installed) on first use; the flag is set for the
    // caller that connected it.
    async fn client_for(
        &self,
        server: &Server,
        root: PathBuf,
        key: String,
    ) -> Option<(Arc<Client>, bool)> {
        let slot = {
            let mut state = lock(&self.state);
            if state.broken.contains(&key) {
                return None;
            }
            if let Some(hit) = state
                .clients
                .iter()
                .find(|c| c.root() == root && c.server_id() == server.id)
            {
                return Some((hit.clone(), false));
            }
            state.spawning.entry(key.clone()).or_default().clone()
        };
        let mut fresh = false;
        let client = slot
            .get_or_init(|| async {
                fresh = true;
                self.schedule(server, &root).await
            })
            .await
            .clone();
        if fresh {
            let mut state = lock(&self.state);
            state.spawning.remove(&key);
            match &client {
                Some(client) => state.clients.push(client.clone()),
                None => {
                    state.broken.insert(key);
                }
            }
        }
        client.map(|client| (client, fresh))
    }

    async fn clients_for(&self, file: &Path) -> Vec<Arc<Client>> {
        if !self.contains(file) {
            return Vec::new();
        }
        // Servers start side by side, so one slow install does not hold back the others.
        let pending: Vec<Pin<Box<dyn Future<Output = _> + Send + '_>>> = self
            .matching(file)
            .into_iter()
            .map(|(server, root, key)| Box::pin(self.client_for(server, root, key)) as _)
            .collect();
        let mut result = Vec::new();
        for (client, fresh) in util::join_all(pending).await.into_iter().flatten() {
            result.push(client);
            if fresh {
                let _ = self.updates.send(());
            }
        }
        result
    }

    fn all(&self) -> Vec<Arc<Client>> {
        lock(&self.state).clients.clone()
    }

    async fn each<F, Fut>(clients: Vec<Arc<Client>>, f: F) -> Vec<Value>
    where
        F: Fn(Arc<Client>) -> Fut,
        Fut: Future<Output = Value> + Send + 'static,
    {
        let handles: Vec<_> = clients
            .into_iter()
            .map(|client| tokio::spawn(f(client)))
            .collect();
        let mut out = Vec::with_capacity(handles.len());
        for handle in handles {
            out.push(handle.await.unwrap_or(Value::Null));
        }
        out
    }

    async fn run(
        &self,
        file: &Path,
        method: &'static str,
        params: Value,
        fallback: Value,
    ) -> Vec<Value> {
        let clients = self.clients_for(&self.absolute(file)).await;
        Self::each(clients, |client| {
            let (params, fallback) = (params.clone(), fallback.clone());
            async move { client.request(method, params).await.unwrap_or(fallback) }
        })
        .await
    }

    /// `GET /lsp`: one row per connected client, plus an `error` row per server whose install failed.
    pub fn status(&self) -> Vec<Status> {
        let state = lock(&self.state);
        let row = |id: &str, root: &Path, status: &str| Status {
            id: id.to_string(),
            name: id.to_string(),
            root: relative(&self.ctx.directory, root),
            status: status.to_string(),
        };
        let connected = state
            .clients
            .iter()
            .map(|client| row(client.server_id(), client.root(), "connected"));
        let failed = state.failed.iter().map(|(id, root)| row(id, root, "error"));
        connected.chain(failed).collect()
    }

    /// Built-in servers that were missing when first needed: being installed, installed, failed (with the
    /// error) or unavailable (with the reason).
    pub fn install_status(&self) -> Vec<InstallStatus> {
        self.installer.status()
    }

    /// A server is configured for this file and has not failed to start.
    pub fn has_clients(&self, file: &Path) -> bool {
        let state = lock(&self.state);
        self.matching(&self.absolute(file))
            .iter()
            .any(|(_, _, key)| !state.broken.contains(key))
    }

    /// Open or refresh `file` in every matching server (spawning them on first touch), optionally waiting
    /// for diagnostics.
    pub async fn touch_file(&self, file: &Path, diagnostics: Option<DiagnosticsMode>) {
        let file = self.absolute(file);
        let clients = self.clients_for(&file).await;
        let handles: Vec<_> = clients
            .into_iter()
            .map(|client| {
                let file = file.clone();
                tokio::spawn(async move {
                    let after = Instant::now();
                    let Ok(version) = client.open(&file).await else {
                        return;
                    };
                    if let Some(mode) = diagnostics {
                        client
                            .wait_for_diagnostics(&file, version, mode, Some(after))
                            .await;
                    }
                })
            })
            .collect();
        for handle in handles {
            let _ = handle.await;
        }
    }

    /// Every client's cached diagnostics, keyed by normalised absolute path.
    pub fn diagnostics(&self) -> IndexMap<String, Vec<Diagnostic>> {
        let mut results: IndexMap<String, Vec<Diagnostic>> = IndexMap::new();
        for client in self.all() {
            for (path, items) in client.diagnostics() {
                results
                    .entry(path.to_string_lossy().into_owned())
                    .or_default()
                    .extend(items);
            }
        }
        results
    }

    /// The key `file` has in [`Lsp::diagnostics`].
    pub fn diagnostics_key(&self, file: &Path) -> String {
        util::normalize_path(&self.absolute(file))
            .to_string_lossy()
            .into_owned()
    }

    /// `edit`/`apply_patch` suffix: `"\n\nLSP errors detected in this file, please fix:\n<block>"` or empty.
    pub fn edit_report(&self, file: &Path) -> String {
        let key = util::normalize_path(&self.absolute(file))
            .to_string_lossy()
            .into_owned();
        let diagnostics = self.diagnostics();
        let block = diagnostic::report(
            &file.to_string_lossy(),
            diagnostics.get(&key).map_or(&[], Vec::as_slice),
        );
        if block.is_empty() {
            block
        } else {
            format!("\n\nLSP errors detected in this file, please fix:\n{block}")
        }
    }

    /// `write` suffix: this file's errors plus up to five other files with errors.
    pub fn write_report(&self, file: &Path) -> String {
        let key = util::normalize_path(&self.absolute(file))
            .to_string_lossy()
            .into_owned();
        let name = file.to_string_lossy();
        let mut output = String::new();
        let mut others = 0;
        for (path, issues) in self.diagnostics() {
            let current = path == key;
            if !current && others >= MAX_PROJECT_DIAGNOSTICS_FILES {
                continue;
            }
            let block = diagnostic::report(if current { &name } else { &path }, &issues);
            if block.is_empty() {
                continue;
            }
            if current {
                output.push_str(&format!(
                    "\n\nLSP errors detected in this file, please fix:\n{block}"
                ));
                continue;
            }
            others += 1;
            output.push_str(&format!("\n\nLSP errors detected in other files:\n{block}"));
        }
        output
    }

    /// One entry per client: the raw hover result, or `null` when the request failed.
    pub async fn hover(&self, input: &Location) -> Vec<Value> {
        self.run(
            &input.file,
            "textDocument/hover",
            position_params(input),
            Value::Null,
        )
        .await
    }

    pub async fn definition(&self, input: &Location) -> Vec<Value> {
        flatten(
            self.run(
                &input.file,
                "textDocument/definition",
                position_params(input),
                Value::Null,
            )
            .await,
        )
    }

    pub async fn references(&self, input: &Location) -> Vec<Value> {
        let mut params = position_params(input);
        params["context"] = json!({ "includeDeclaration": true });
        flatten(
            self.run(&input.file, "textDocument/references", params, json!([]))
                .await,
        )
    }

    pub async fn implementation(&self, input: &Location) -> Vec<Value> {
        flatten(
            self.run(
                &input.file,
                "textDocument/implementation",
                position_params(input),
                Value::Null,
            )
            .await,
        )
    }

    /// `uri` is a `file://` URI, as in the source.
    pub async fn document_symbol(&self, uri: &str) -> Vec<Value> {
        let Some(file) = util::uri_to_path(uri) else {
            return Vec::new();
        };
        let params = json!({ "textDocument": { "uri": uri } });
        flatten(
            self.run(&file, "textDocument/documentSymbol", params, json!([]))
                .await,
        )
    }

    /// Across all connected clients: the first ten symbols per server of the kinds the source keeps.
    pub async fn workspace_symbol(&self, query: &str) -> Vec<Value> {
        let params = json!({ "query": query });
        let results = Self::each(self.all(), |client| {
            let params = params.clone();
            async move {
                let result = client
                    .request("workspace/symbol", params)
                    .await
                    .unwrap_or(Value::Null);
                let items = result.as_array().cloned().unwrap_or_default();
                let keep = |item: &Value| {
                    item.get("kind")
                        .and_then(Value::as_u64)
                        .is_some_and(|k| KINDS.contains(&k))
                };
                Value::Array(items.into_iter().filter(keep).take(10).collect())
            }
        })
        .await;
        flatten(results)
    }

    pub async fn prepare_call_hierarchy(&self, input: &Location) -> Vec<Value> {
        flatten(
            self.run(
                &input.file,
                "textDocument/prepareCallHierarchy",
                position_params(input),
                json!([]),
            )
            .await,
        )
    }

    async fn call_hierarchy(&self, input: &Location, direction: &'static str) -> Vec<Value> {
        let clients = self.clients_for(&self.absolute(&input.file)).await;
        let params = position_params(input);
        let results = Self::each(clients, |client| {
            let params = params.clone();
            async move {
                let items = client
                    .request("textDocument/prepareCallHierarchy", params)
                    .await
                    .unwrap_or(json!([]));
                let Some(first) = items.as_array().and_then(|items| items.first()) else {
                    return json!([]);
                };
                client
                    .request(direction, json!({ "item": first }))
                    .await
                    .unwrap_or(json!([]))
            }
        })
        .await;
        flatten(results)
    }

    pub async fn incoming_calls(&self, input: &Location) -> Vec<Value> {
        self.call_hierarchy(input, "callHierarchy/incomingCalls")
            .await
    }

    pub async fn outgoing_calls(&self, input: &Location) -> Vec<Value> {
        self.call_hierarchy(input, "callHierarchy/outgoingCalls")
            .await
    }

    /// Stop every server process.
    pub async fn shutdown(&self) {
        let clients = std::mem::take(&mut lock(&self.state).clients);
        for client in clients {
            client.shutdown().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(experimental_ty: bool) -> Context {
        Context {
            directory: PathBuf::from("."),
            worktree: PathBuf::from("."),
            cache_dir: None,
            experimental_ty,
        }
    }

    #[test]
    fn disabled_unless_configured() {
        assert!(Lsp::new(None, ctx(false)).is_empty());
        assert!(Lsp::new(Some(&json!(false)), ctx(false)).is_empty());
        let all = Lsp::new(Some(&json!(true)), ctx(false));
        assert_eq!(all.server_ids().len(), 37);
        assert!(all.server_ids().contains(&"pyright") && !all.server_ids().contains(&"ty"));
        let ty = Lsp::new(Some(&json!(true)), ctx(true));
        assert!(ty.server_ids().contains(&"ty") && !ty.server_ids().contains(&"pyright"));
    }

    #[test]
    fn config_overrides() {
        let config = json!({
            "rust": {"disabled": true},
            "gopls": {"command": ["my-gopls", "serve"], "env": {"A": "1"}, "initialization": {"x": 1}},
            "custom": {"command": ["c"], "extensions": [".foo"]}
        });
        let lsp = Lsp::new(Some(&config), ctx(false));
        assert!(!lsp.server_ids().contains(&"rust"));
        let gopls = &lsp.servers["gopls"];
        assert_eq!(gopls.extensions, vec![".go"]);
        assert_eq!(gopls.root, Root::Gopls);
        assert!(
            matches!(&gopls.spawn, Spawn::Custom { command, .. } if command == &["my-gopls", "serve"])
        );
        let custom = &lsp.servers["custom"];
        assert_eq!(custom.root, Root::Directory);
        assert_eq!(custom.extensions, vec![".foo"]);
        assert_eq!(lsp.server_ids().last(), Some(&"custom"));
    }

    #[test]
    fn relative_matches_node() {
        assert_eq!(relative(Path::new("/a/b"), Path::new("/a/b")), "");
        assert_eq!(relative(Path::new("/a/b"), Path::new("/a/b/c")), "c");
        assert_eq!(
            relative(Path::new("/a/b"), Path::new("/a/x")),
            Path::new("..").join("x").to_string_lossy()
        );
    }

    #[test]
    fn flatten_drops_falsy() {
        assert_eq!(
            flatten(vec![
                json!([1, null]),
                Value::Null,
                json!({"a": 1}),
                json!([])
            ]),
            vec![json!(1), json!({"a": 1})]
        );
    }
}
