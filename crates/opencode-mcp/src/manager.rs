//! Server lifecycle and catalogue (`mcp/index.ts`).

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use futures_util::future::{BoxFuture, join_all};
use indexmap::IndexMap;
use opencode_core::config::{McpConfig, McpOAuth, McpOAuthSetting, McpServer};
use opencode_tools::Tool;
use reqwest::Url;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};

use crate::auth::{AuthStore, expired};
use crate::catalog::{self, DEFAULT_TIMEOUT_MS, ToolDef};
use crate::client::{Client, Inbound, RequestOptions, Transport};
use crate::error::{ManagerError, McpError};
use crate::http::{SseTransport, StreamableHttpTransport};
use crate::oauth::{OAUTH_CALLBACK_PATH, OAuthProvider, OAuthSettings, random_hex};
use crate::stdio::StdioTransport;
use crate::tool::McpTool;
use crate::{browser, callback};

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Status {
    Connected,
    Disabled,
    Failed { error: String },
    NeedsAuth,
    NeedsClientRegistration { error: String },
}

impl Status {
    fn failed(error: impl Into<String>) -> Self {
        Self::Failed {
            error: error.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthStatus {
    Authenticated,
    Expired,
    NotAuthenticated,
}

#[derive(Debug, Clone, PartialEq)]
pub enum McpEvent {
    /// `mcp.tools.changed`
    ToolsChanged { server: String },
    /// `mcp.browser.open.failed`
    BrowserOpenFailed { mcp_name: String, url: String },
    /// `TuiEvent.ToastShow`
    Toast {
        title: String,
        message: String,
        variant: String,
        duration: u64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServerInstructions {
    pub name: String,
    pub instructions: String,
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthStart {
    pub authorization_url: String,
    pub oauth_state: String,
}

pub type BrowserOpener =
    Arc<dyn Fn(String) -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

#[derive(Clone)]
pub struct McpOptions {
    /// Instance directory: base for relative `cwd` and the `roots/list` answer.
    pub cwd: PathBuf,
    /// Base environment of local servers (`process.env`).
    pub env: HashMap<String, String>,
    /// `Global.Path.data`, home of `mcp-auth.json`.
    pub data_dir: PathBuf,
    /// `experimental.mcp_timeout`
    pub default_timeout: Option<u64>,
    /// `clientInfo.version`
    pub version: String,
    /// Replaces the system browser launcher.
    pub browser: Option<BrowserOpener>,
}

impl McpOptions {
    pub fn new(
        cwd: impl Into<PathBuf>,
        env: HashMap<String, String>,
        data_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            cwd: cwd.into(),
            env,
            data_dir: data_dir.into(),
            default_timeout: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            browser: None,
        }
    }
}

#[derive(Default)]
struct State {
    base: IndexMap<String, McpConfig>,
    runtime: IndexMap<String, McpServer>,
    status: IndexMap<String, Status>,
    clients: IndexMap<String, Client>,
    defs: IndexMap<String, Vec<ToolDef>>,
    instructions: IndexMap<String, String>,
    pending_oauth: HashMap<String, Arc<OAuthProvider>>,
}

impl State {
    fn config(&self, name: &str) -> Option<McpServer> {
        if let Some(server) = self.runtime.get(name) {
            return Some(server.clone());
        }
        match self.base.get(name) {
            Some(McpConfig::Server(server)) => Some(server.clone()),
            _ => None,
        }
    }

    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .base
            .iter()
            .filter(|(_, entry)| matches!(entry, McpConfig::Server(_)))
            .map(|(name, _)| name.clone())
            .collect();
        for name in self.runtime.keys() {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        names
    }

    fn connected(&self, only: Option<&str>) -> Vec<(String, Client)> {
        let mut names = self.names();
        for name in self.clients.keys() {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        names
            .into_iter()
            .filter(|name| only.is_none_or(|only| only == name))
            .filter(|name| self.status.get(name) == Some(&Status::Connected))
            .filter_map(|name| {
                self.clients
                    .get(&name)
                    .cloned()
                    .map(|client| (name, client))
            })
            .collect()
    }

    fn is_current(&self, name: &str, client: &Client) -> bool {
        self.clients
            .get(name)
            .is_some_and(|current| current.same(client))
    }
}

struct Inner {
    options: McpOptions,
    store: Arc<AuthStore>,
    http: reqwest::Client,
    events: broadcast::Sender<McpEvent>,
    state: Mutex<State>,
}

struct Created {
    client: Option<Client>,
    status: Status,
    defs: Vec<ToolDef>,
    instructions: Option<String>,
}

impl Created {
    fn status(status: Status) -> Self {
        Self {
            client: None,
            status,
            defs: Vec::new(),
            instructions: None,
        }
    }
}

fn server_timeout(server: &McpServer) -> Option<u64> {
    match server {
        McpServer::Local { timeout, .. } | McpServer::Remote { timeout, .. } => *timeout,
    }
}

fn server_enabled(server: &McpServer) -> bool {
    match server {
        McpServer::Local { enabled, .. } | McpServer::Remote { enabled, .. } => {
            enabled.unwrap_or(true)
        }
    }
}

fn oauth_config(setting: &Option<McpOAuthSetting>) -> (bool, McpOAuth) {
    match setting {
        Some(McpOAuthSetting::Toggle(false)) => (true, McpOAuth::default()),
        Some(McpOAuthSetting::Config(config)) => (false, config.clone()),
        _ => (false, McpOAuth::default()),
    }
}

fn spawn_close(client: Client) {
    tokio::spawn(async move { client.close().await });
}

type TransportParts = (Box<dyn Transport>, mpsc::UnboundedReceiver<Inbound>);

/// Connected MCP servers of one instance directory.
#[derive(Clone)]
pub struct McpManager {
    inner: Arc<Inner>,
}

impl McpManager {
    /// Connects every enabled server concurrently; failures only show up in `status()`.
    pub async fn connect(servers: IndexMap<String, McpConfig>, options: McpOptions) -> Self {
        let store = Arc::new(AuthStore::new(&options.data_dir));
        let manager = Self {
            inner: Arc::new(Inner {
                options,
                store,
                http: reqwest::Client::new(),
                events: broadcast::channel(64).0,
                state: Mutex::new(State {
                    base: servers.clone(),
                    ..State::default()
                }),
            }),
        };
        let jobs = servers.into_iter().filter_map(|(name, entry)| {
            let McpConfig::Server(server) = entry else {
                return None;
            };
            let manager = manager.clone();
            Some(async move {
                if !server_enabled(&server) {
                    manager.state().status.insert(name, Status::Disabled);
                    return;
                }
                let created = manager.create(&name, &server).await;
                manager
                    .state()
                    .status
                    .insert(name.clone(), created.status.clone());
                if let Some(client) = created.client {
                    manager.store_client(
                        &name,
                        client,
                        created.defs,
                        created.instructions,
                        server_timeout(&server),
                    );
                }
            })
        });
        join_all(jobs).await;
        manager
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.state.lock().unwrap()
    }

    fn publish(&self, event: McpEvent) {
        let _ = self.inner.events.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<McpEvent> {
        self.inner.events.subscribe()
    }

    pub fn auth_store(&self) -> &Arc<AuthStore> {
        &self.inner.store
    }

    fn roots_uri(&self) -> String {
        Url::from_file_path(&self.inner.options.cwd)
            .map(|url| url.to_string())
            .unwrap_or_else(|_| format!("file://{}", self.inner.options.cwd.display()))
    }

    // `connectTransport`: transport setup plus `initialize` under one timeout; closed on failure.
    async fn establish(
        &self,
        make: impl Future<Output = Result<TransportParts, McpError>>,
        timeout_ms: Option<u64>,
    ) -> Result<Client, McpError> {
        let started: Mutex<Option<Client>> = Mutex::new(None);
        let attempt = async {
            let (transport, inbound) = make.await?;
            let client = Client::start(transport, inbound, self.roots_uri());
            *started.lock().unwrap() = Some(client.clone());
            client.initialize(&self.inner.options.version).await?;
            Ok::<_, McpError>(client)
        };
        let result = match timeout_ms {
            Some(ms) => match tokio::time::timeout(Duration::from_millis(ms), attempt).await {
                Ok(result) => result,
                Err(_) => Err(McpError::Other(format!("Operation timed out after {ms}ms"))),
            },
            None => attempt.await,
        };
        if result.is_err()
            && let Some(client) = started.lock().unwrap().take()
        {
            spawn_close(client);
        }
        result
    }

    fn provider(
        &self,
        name: &str,
        url: &str,
        settings: OAuthSettings,
        headers: &IndexMap<String, String>,
        pending: bool,
    ) -> Arc<OAuthProvider> {
        Arc::new(OAuthProvider::new(
            name,
            url,
            settings,
            self.inner.store.clone(),
            self.inner.http.clone(),
            headers.clone(),
            pending,
        ))
    }

    async fn connect_remote(
        &self,
        name: &str,
        raw_url: &str,
        headers: &IndexMap<String, String>,
        oauth: &Option<McpOAuthSetting>,
        timeout: Option<u64>,
    ) -> (Option<Client>, Status) {
        let Ok(url) = Url::parse(raw_url) else {
            return (
                None,
                Status::failed(format!("Invalid MCP URL for \"{name}\"")),
            );
        };
        let (oauth_disabled, config) = oauth_config(oauth);
        let provider = (!oauth_disabled).then(|| {
            let settings = OAuthSettings {
                client_id: config.client_id.clone(),
                client_secret: config.client_secret.clone(),
                scope: config.scope.clone(),
                callback_port: config.callback_port,
                redirect_uri: config.redirect_uri.clone(),
            };
            self.provider(name, raw_url, settings, headers, false)
        });
        let connect_timeout = Some(timeout.unwrap_or(DEFAULT_TIMEOUT_MS));
        let mut last = None;
        for streamable in [true, false] {
            let http = self.inner.http.clone();
            let (url, headers, auth) = (url.clone(), headers.clone(), provider.clone());
            let make = async move {
                let (tx, rx) = mpsc::unbounded_channel();
                let transport: Box<dyn Transport> = if streamable {
                    Box::new(StreamableHttpTransport::new(http, url, headers, auth, tx))
                } else {
                    Box::new(SseTransport::connect(http, url, headers, auth, tx).await?)
                };
                Ok((transport, rx))
            };
            let error = match self.establish(make, connect_timeout).await {
                Ok(client) => return (Some(client), Status::Connected),
                Err(error) => error,
            };
            let message = error.to_string();
            let is_auth = matches!(error, McpError::Unauthorized(_))
                || (provider.is_some() && message.contains("OAuth"));
            if !is_auth {
                last = Some(Status::failed(message));
                continue;
            }
            if message.contains("registration") || message.contains("client_id") {
                self.publish(McpEvent::Toast {
                    title: "MCP Authentication Required".into(),
                    message: format!("Server \"{name}\" requires a pre-registered client ID. Add clientId to your config."),
                    variant: "warning".into(),
                    duration: 8000,
                });
                return (
                    None,
                    Status::NeedsClientRegistration {
                        error: "Server does not support dynamic client registration. Please provide clientId in config."
                            .into(),
                    },
                );
            }
            if let Some(provider) = &provider {
                self.state()
                    .pending_oauth
                    .insert(name.to_string(), provider.clone());
            }
            self.publish(McpEvent::Toast {
                title: "MCP Authentication Required".into(),
                message: format!(
                    "Server \"{name}\" requires authentication. Run: opencode mcp auth {name}"
                ),
                variant: "warning".into(),
                duration: 8000,
            });
            return (None, Status::NeedsAuth);
        }
        (
            None,
            last.unwrap_or_else(|| Status::failed("Unknown error")),
        )
    }

    async fn connect_local(
        &self,
        command: &[String],
        cwd: &Option<String>,
        environment: &IndexMap<String, String>,
        timeout: Option<u64>,
    ) -> (Option<Client>, Status) {
        let base = &self.inner.options.cwd;
        let cwd = match cwd.as_deref().filter(|dir| !dir.is_empty()) {
            Some(dir) => base.join(dir),
            None => base.clone(),
        };
        let mut env = self.inner.options.env.clone();
        let mut put = |key: &str, value: &str| {
            if cfg!(windows) {
                env.retain(|existing, _| !existing.eq_ignore_ascii_case(key));
            }
            env.insert(key.to_string(), value.to_string());
        };
        if command.first().map(String::as_str) == Some("opencode") {
            put("BUN_BE_BUN", "1");
        }
        for (key, value) in environment {
            put(key, value);
        }
        let make = async {
            let (tx, rx) = mpsc::unbounded_channel();
            let transport: Box<dyn Transport> =
                Box::new(StdioTransport::spawn(command, &cwd, &env, tx)?);
            Ok((transport, rx))
        };
        match self
            .establish(make, Some(timeout.unwrap_or(DEFAULT_TIMEOUT_MS)))
            .await
        {
            Ok(client) => (Some(client), Status::Connected),
            Err(error) => (None, Status::failed(error.to_string())),
        }
    }

    async fn create(&self, name: &str, server: &McpServer) -> Created {
        if !server_enabled(server) {
            return Created::status(Status::Disabled);
        }
        let (client, status) = match server {
            McpServer::Remote {
                url,
                headers,
                oauth,
                timeout,
                ..
            } => {
                self.connect_remote(name, url, headers, oauth, *timeout)
                    .await
            }
            McpServer::Local {
                command,
                cwd,
                environment,
                timeout,
                ..
            } => {
                self.connect_local(command, cwd, environment, *timeout)
                    .await
            }
        };
        let Some(client) = client else {
            return Created::status(status);
        };
        match Self::list_defs(&client, server_timeout(server)).await {
            Some(defs) => Created {
                instructions: client.instructions(),
                client: Some(client),
                status,
                defs,
            },
            None => {
                client.close().await;
                Created::status(Status::failed("Failed to get tools"))
            }
        }
    }

    async fn list_defs(client: &Client, timeout: Option<u64>) -> Option<Vec<ToolDef>> {
        if !client.has_capability("tools") {
            return Some(Vec::new());
        }
        catalog::list_tools(client, timeout).await.ok()
    }

    fn store_client(
        &self,
        name: &str,
        client: Client,
        defs: Vec<ToolDef>,
        instructions: Option<String>,
        timeout: Option<u64>,
    ) -> Status {
        let previous = {
            let mut state = self.state();
            state.status.insert(name.to_string(), Status::Connected);
            state.defs.insert(name.to_string(), defs);
            match instructions {
                Some(text) => state.instructions.insert(name.to_string(), text),
                None => state.instructions.shift_remove(name),
            };
            state.clients.insert(name.to_string(), client.clone())
        };
        self.watch(name, client, timeout);
        if let Some(previous) = previous {
            spawn_close(previous);
        }
        Status::Connected
    }

    // Connection loss and `notifications/tools/list_changed`.
    fn watch(&self, name: &str, client: Client, timeout: Option<u64>) {
        let weak: Weak<Inner> = Arc::downgrade(&self.inner);
        let name = name.to_string();
        let mut notifications = client.notifications();
        tokio::spawn(async move {
            loop {
                let notification = tokio::select! {
                    _ = client.closed() => None,
                    notification = notifications.recv() => match notification {
                        Ok(notification) => Some(notification),
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => None,
                    },
                };
                let Some(inner) = weak.upgrade() else { return };
                let manager = McpManager { inner };
                let Some(notification) = notification else {
                    {
                        let mut state = manager.state();
                        if !state.is_current(&name, &client) {
                            return;
                        }
                        state.clients.shift_remove(&name);
                        state.defs.shift_remove(&name);
                        state.instructions.shift_remove(&name);
                        state
                            .status
                            .insert(name.clone(), Status::failed("Connection closed"));
                    }
                    manager.publish(McpEvent::ToolsChanged { server: name });
                    return;
                };
                if notification.method != "notifications/tools/list_changed"
                    || !client.has_capability("tools")
                {
                    continue;
                }
                let current = |state: &State| {
                    state.is_current(&name, &client)
                        && state.status.get(&name) == Some(&Status::Connected)
                };
                if !current(&manager.state()) {
                    continue;
                }
                let Ok(listed) = catalog::list_tools(&client, timeout).await else {
                    continue;
                };
                {
                    let mut state = manager.state();
                    if !current(&state) {
                        continue;
                    }
                    state.defs.insert(name.clone(), listed);
                }
                manager.publish(McpEvent::ToolsChanged {
                    server: name.clone(),
                });
            }
        });
    }

    async fn close_client(&self, name: &str) {
        let client = {
            let mut state = self.state();
            state.defs.shift_remove(name);
            state.instructions.shift_remove(name);
            state.clients.shift_remove(name)
        };
        if let Some(client) = client {
            client.close().await;
        }
    }

    async fn create_and_store(&self, name: &str, server: &McpServer) -> Status {
        let created = self.create(name, server).await;
        self.state()
            .status
            .insert(name.to_string(), created.status.clone());
        match created.client {
            Some(client) => self.store_client(
                name,
                client,
                created.defs,
                created.instructions,
                server_timeout(server),
            ),
            None => {
                self.close_client(name).await;
                created.status
            }
        }
    }

    fn require_config(&self, name: &str) -> Result<McpServer, ManagerError> {
        self.state()
            .config(name)
            .ok_or_else(|| ManagerError::NotFound {
                name: name.to_string(),
            })
    }

    fn request_timeout(&self, state: &State, name: &str) -> Option<u64> {
        let configured = match state.base.get(name) {
            Some(McpConfig::Server(server)) => server_timeout(server),
            _ => None,
        };
        state
            .runtime
            .get(name)
            .and_then(server_timeout)
            .or(configured)
            .or(self.inner.options.default_timeout)
    }

    /// Every configured server, in config order, then runtime additions.
    pub fn status(&self) -> IndexMap<String, Status> {
        let state = self.state();
        state
            .names()
            .into_iter()
            .map(|name| {
                let status = state.status.get(&name).cloned().unwrap_or(Status::Disabled);
                (name, status)
            })
            .collect()
    }

    pub fn clients(&self) -> IndexMap<String, Client> {
        self.state().clients.clone()
    }

    /// Connected servers that advertise `resources`, sorted; non-empty gates the resource tools.
    pub fn resource_servers(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .state()
            .clients
            .iter()
            .filter(|(_, client)| client.has_capability("resources"))
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        names
    }

    pub fn instructions(&self) -> Vec<ServerInstructions> {
        let state = self.state();
        let mut list: Vec<ServerInstructions> = state
            .instructions
            .iter()
            .filter(|(name, _)| state.status.get(*name) == Some(&Status::Connected))
            .map(|(name, text)| ServerInstructions {
                name: name.clone(),
                instructions: text.clone(),
                tools: state
                    .defs
                    .get(name)
                    .map(|defs| {
                        defs.iter()
                            .map(|def| catalog::tool_name(name, &def.name))
                            .collect()
                    })
                    .unwrap_or_default(),
            })
            .collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }

    /// Tools keyed by exposed name; later servers win name collisions.
    pub fn mcp_tools(&self) -> IndexMap<String, Arc<McpTool>> {
        let state = self.state();
        let mut tools = IndexMap::new();
        for (server, client) in state.connected(None) {
            let Some(defs) = state.defs.get(&server) else {
                continue;
            };
            let timeout = self.request_timeout(&state, &server);
            for def in defs {
                let id = catalog::tool_name(&server, &def.name);
                let tool = McpTool {
                    id: id.clone(),
                    server: server.clone(),
                    def: def.clone(),
                    client: client.clone(),
                    timeout,
                };
                tools.insert(id, Arc::new(tool));
            }
        }
        tools
    }

    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.mcp_tools()
            .into_values()
            .map(|tool| tool as Arc<dyn Tool>)
            .collect()
    }

    async fn collect<F>(
        &self,
        only: Option<&str>,
        key: impl Fn(&str, &Value) -> Option<String>,
        list: impl Fn(Client, Option<u64>) -> F,
    ) -> IndexMap<String, Value>
    where
        F: Future<Output = Result<Vec<Value>, McpError>>,
    {
        let targets: Vec<(String, Client, Option<u64>)> = {
            let state = self.state();
            state
                .connected(only)
                .into_iter()
                .map(|(name, client)| {
                    let timeout = self.request_timeout(&state, &name);
                    (name, client, timeout)
                })
                .collect()
        };
        let fetched = join_all(targets.into_iter().map(|(name, client, timeout)| {
            let listing = list(client, timeout);
            async move { (name, listing.await) }
        }))
        .await;
        let mut result = IndexMap::new();
        for (name, items) in fetched {
            for mut item in items.unwrap_or_default() {
                let Some(key) = key(&name, &item) else {
                    continue;
                };
                item["client"] = name.clone().into();
                result.insert(key, item);
            }
        }
        result
    }

    /// Prompts keyed `sanitize(server):sanitize(name)`, each with a `client` field.
    pub async fn prompts(&self) -> IndexMap<String, Value> {
        let key = |server: &str, item: &Value| {
            Some(catalog::prompt_key(server, item.get("name")?.as_str()?))
        };
        self.collect(None, key, |client, timeout| async move {
            catalog::list_prompts(&client, timeout).await
        })
        .await
    }

    /// Resources keyed `escape(server):uri`, each with a `client` field.
    pub async fn resources(&self, server: Option<&str>) -> IndexMap<String, Value> {
        let key = |server: &str, item: &Value| {
            Some(catalog::resource_key(server, item.get("uri")?.as_str()?))
        };
        self.collect(server, key, |client, timeout| async move {
            catalog::list_resources(&client, timeout).await
        })
        .await
    }

    pub async fn resource_templates(&self, server: Option<&str>) -> IndexMap<String, Value> {
        let key = |server: &str, item: &Value| {
            Some(catalog::resource_key(
                server,
                item.get("uriTemplate")?.as_str()?,
            ))
        };
        self.collect(server, key, |client, timeout| async move {
            catalog::list_resource_templates(&client, timeout).await
        })
        .await
    }

    async fn with_client(&self, server: &str, method: &str, params: Value) -> Option<Value> {
        let (client, timeout) = {
            let state = self.state();
            (
                state.clients.get(server)?.clone(),
                self.request_timeout(&state, server),
            )
        };
        client
            .request(method, Some(params), RequestOptions::timeout_ms(timeout))
            .await
            .ok()
    }

    /// `prompts/get`; `None` when the server is unknown or the request fails.
    pub async fn get_prompt(
        &self,
        server: &str,
        name: &str,
        args: Option<IndexMap<String, String>>,
    ) -> Option<Value> {
        let mut params = json!({"name": name});
        if let Some(args) = args {
            params["arguments"] = json!(args);
        }
        self.with_client(server, "prompts/get", params).await
    }

    /// `resources/read`; `None` when the server is unknown or the request fails.
    pub async fn read_resource(&self, server: &str, uri: &str) -> Option<Value> {
        self.with_client(server, "resources/read", json!({"uri": uri}))
            .await
    }

    /// Registers a runtime-only server and connects it. Returns the raw status table.
    pub async fn add(&self, name: &str, server: McpServer) -> IndexMap<String, Status> {
        self.state()
            .runtime
            .insert(name.to_string(), server.clone());
        self.create_and_store(name, &server).await;
        self.state().status.clone()
    }

    /// Disconnects a server and forgets its runtime entry; a config-file entry falls back to `disabled`.
    pub async fn remove(&self, name: &str) -> Result<(), ManagerError> {
        self.require_config(name)?;
        self.close_client(name).await;
        let mut state = self.state();
        state.runtime.shift_remove(name);
        state.pending_oauth.remove(name);
        if state.config(name).is_some() {
            state.status.insert(name.to_string(), Status::Disabled);
        } else {
            state.status.shift_remove(name);
        }
        Ok(())
    }

    /// (Re)connects `name`, ignoring `enabled: false`.
    pub async fn connect_server(&self, name: &str) -> Result<Status, ManagerError> {
        let server = enabled(self.require_config(name)?);
        Ok(self.create_and_store(name, &server).await)
    }

    pub async fn disconnect(&self, name: &str) -> Result<(), ManagerError> {
        self.require_config(name)?;
        self.close_client(name).await;
        self.state()
            .status
            .insert(name.to_string(), Status::Disabled);
        Ok(())
    }

    /// Closes every client and kills local server process trees.
    pub async fn shutdown(&self) {
        let clients: Vec<Client> = {
            let mut state = self.state();
            state.defs.clear();
            state.instructions.clear();
            state.pending_oauth.clear();
            std::mem::take(&mut state.clients).into_values().collect()
        };
        join_all(clients.iter().map(Client::close)).await;
    }

    async fn begin_auth(&self, name: &str) -> Result<(AuthStart, Option<Client>), ManagerError> {
        let McpServer::Remote {
            url: raw_url,
            headers,
            oauth,
            ..
        } = self.require_config(name)?
        else {
            return Err(ManagerError::Other(format!(
                "MCP server {name} is not a remote server"
            )));
        };
        let (oauth_disabled, config) = oauth_config(&oauth);
        if oauth_disabled {
            return Err(ManagerError::Other(format!(
                "MCP server {name} has OAuth explicitly disabled"
            )));
        }
        let url = Url::parse(&raw_url)
            .map_err(|_| ManagerError::Other(format!("Invalid MCP URL for \"{name}\"")))?;
        let redirect_uri = config
            .redirect_uri
            .clone()
            .filter(|uri| !uri.is_empty())
            .or_else(|| {
                config
                    .callback_port
                    .filter(|port| *port != 0)
                    .map(|port| format!("http://127.0.0.1:{port}{OAUTH_CALLBACK_PATH}"))
            });
        callback::ensure_running(redirect_uri.as_deref())
            .await
            .map_err(|e| ManagerError::Other(e.to_string()))?;

        let oauth_state = random_hex(32);
        self.inner
            .store
            .update_oauth_state(name, &oauth_state)
            .await;
        let settings = OAuthSettings {
            client_id: config.client_id.clone(),
            client_secret: config.client_secret.clone(),
            scope: config.scope.clone(),
            callback_port: None,
            redirect_uri,
        };
        let provider = self.provider(name, &raw_url, settings, &headers, true);
        let http = self.inner.http.clone();
        let auth = Some(provider.clone());
        let make = async move {
            let (tx, rx) = mpsc::unbounded_channel();
            let transport: Box<dyn Transport> =
                Box::new(StreamableHttpTransport::new(http, url, headers, auth, tx));
            Ok((transport, rx))
        };
        match self.establish(make, None).await {
            Ok(client) => {
                provider.commit().await;
                let start = AuthStart {
                    authorization_url: String::new(),
                    oauth_state,
                };
                Ok((start, Some(client)))
            }
            Err(McpError::Unauthorized(_)) if provider.captured_url().is_some() => {
                let authorization_url = provider.captured_url().unwrap_or_default();
                self.state()
                    .pending_oauth
                    .insert(name.to_string(), provider);
                let start = AuthStart {
                    authorization_url,
                    oauth_state,
                };
                Ok((start, None))
            }
            Err(error) => Err(ManagerError::Other(error.to_string())),
        }
    }

    /// Prepares the authorization URL (`POST /mcp/:name/auth`). An empty URL means already authorized.
    pub async fn start_auth(&self, name: &str) -> Result<AuthStart, ManagerError> {
        let (start, client) = self.begin_auth(name).await?;
        if let Some(client) = client {
            spawn_close(client);
        }
        Ok(start)
    }

    /// Full interactive flow: callback server, browser, code exchange, reconnect.
    pub async fn authenticate(
        &self,
        name: &str,
        on_authorization: Option<&(dyn Fn(&str) + Send + Sync)>,
    ) -> Result<Status, ManagerError> {
        let (start, client) = self.begin_auth(name).await?;
        if start.authorization_url.is_empty() {
            let Some(client) = client else {
                return Ok(Status::failed("Failed to get tools"));
            };
            let server = match self.require_config(name) {
                Ok(server) => server,
                Err(error) => {
                    client.close().await;
                    return Err(error);
                }
            };
            let Some(defs) = Self::list_defs(&client, server_timeout(&server)).await else {
                client.close().await;
                return Ok(Status::failed("Failed to get tools"));
            };
            self.inner.store.clear_oauth_state(name).await;
            let instructions = client.instructions();
            return Ok(self.store_client(
                name,
                client,
                defs,
                instructions,
                server_timeout(&server),
            ));
        }

        let callback = callback::wait_for_callback(&start.oauth_state, Some(name));
        if let Some(notify) = on_authorization {
            notify(&start.authorization_url);
        }
        let opened = match &self.inner.options.browser {
            Some(open) => open(start.authorization_url.clone()).await,
            None => browser::open(&start.authorization_url).await,
        };
        if opened.is_err() {
            self.publish(McpEvent::BrowserOpenFailed {
                mcp_name: name.to_string(),
                url: start.authorization_url.clone(),
            });
        }
        let code = callback
            .await
            .map_err(|e| ManagerError::Other(e.to_string()))?;

        let stored = self.inner.store.get_oauth_state(name).await;
        self.inner.store.clear_oauth_state(name).await;
        if stored.as_deref() != Some(start.oauth_state.as_str()) {
            return Err(ManagerError::other(
                "OAuth state mismatch - potential CSRF attack",
            ));
        }
        self.finish_auth(name, &code).await
    }

    /// Exchanges the authorization code and reconnects (`POST /mcp/:name/auth/callback`).
    pub async fn finish_auth(&self, name: &str, code: &str) -> Result<Status, ManagerError> {
        self.require_config(name)?;
        let provider = self.state().pending_oauth.get(name).cloned();
        let Some(provider) = provider else {
            return Err(ManagerError::Other(format!(
                "No pending OAuth flow for MCP server: {name}"
            )));
        };
        if let Err(error) = provider.finish(code).await {
            return Ok(Status::failed(format!("OAuth completion failed: {error}")));
        }
        provider.commit().await;
        self.inner.store.clear_code_verifier(name).await;
        self.state().pending_oauth.remove(name);
        let server = enabled(self.require_config(name)?);
        Ok(self.create_and_store(name, &server).await)
    }

    pub async fn remove_auth(&self, name: &str) {
        self.inner.store.remove(name).await;
        callback::cancel_pending(name);
        self.state().pending_oauth.remove(name);
    }

    pub fn supports_oauth(&self, name: &str) -> Result<bool, ManagerError> {
        Ok(match self.require_config(name)? {
            McpServer::Remote { oauth, .. } => !oauth_config(&oauth).0,
            McpServer::Local { .. } => false,
        })
    }

    pub async fn has_stored_tokens(&self, name: &str) -> bool {
        self.inner
            .store
            .get(name)
            .await
            .is_some_and(|entry| entry.tokens.is_some())
    }

    pub async fn auth_status(&self, name: &str) -> AuthStatus {
        let Some(McpServer::Remote { url, .. }) = self.state().config(name) else {
            return AuthStatus::NotAuthenticated;
        };
        match self
            .inner
            .store
            .get_for_url(name, &url)
            .await
            .and_then(|entry| entry.tokens)
        {
            None => AuthStatus::NotAuthenticated,
            Some(tokens) if expired(&tokens.expires_at) => AuthStatus::Expired,
            Some(_) => AuthStatus::Authenticated,
        }
    }
}

fn enabled(server: McpServer) -> McpServer {
    match server {
        McpServer::Local {
            command,
            cwd,
            environment,
            timeout,
            ..
        } => McpServer::Local {
            command,
            cwd,
            environment,
            timeout,
            enabled: Some(true),
        },
        McpServer::Remote {
            url,
            headers,
            oauth,
            timeout,
            ..
        } => McpServer::Remote {
            url,
            headers,
            oauth,
            timeout,
            enabled: Some(true),
        },
    }
}
