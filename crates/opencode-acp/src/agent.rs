// The ACP agent over the engine: session lifecycle, prompt turns, streamed updates and permission round trips
// (`acp/service.ts`, `acp/event.ts`, `acp/permission.ts`, `acp/error.ts`, `acp/usage.ts`).
use crate::config_option;
use crate::content::{self, ContentBlock, PromptPart};
use crate::directory::{self, ModelRef, Snapshot, has_variant};
use crate::permission;
use crate::rpc::{Peer, RpcError};
use crate::tool::{self, string};
use opencode_session::{CommandInput, CreateSession, Engine, Host, PermissionAnswer, PermissionAsk, PromptInput, Reply, Update};
use opencode_storage::{AssistantError, AssistantMessage, Message, Part, PartBody, Session, SessionModel, ToolPart, ToolState, ToolStateRunning, id};
use opencode_tools::PermissionRequest;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

pub const AUTH_METHOD_ID: &str = "opencode-login";
const LIST_LIMIT: usize = 100;
const RECENT_MESSAGES: usize = 20;

#[derive(Debug, Clone, PartialEq)]
pub enum AcpError {
    SessionNotFound(String),
    InvalidConfigOption(String),
    InvalidModel { provider_id: Option<String>, model_id: String },
    InvalidEffort(String),
    InvalidMode(String),
    AuthRequired(Option<String>),
    UnknownAuthMethod(String),
    Service { message: String, service: Option<&'static str>, name: Option<String> },
}

impl From<AcpError> for RpcError {
    fn from(error: AcpError) -> Self {
        match error {
            AcpError::SessionNotFound(id) => RpcError::invalid_params(Some(json!({"sessionId": id})), Some(&format!("session not found: {id}"))),
            AcpError::InvalidConfigOption(id) => RpcError::invalid_params(Some(json!({"configId": id})), Some(&format!("unknown config option: {id}"))),
            AcpError::InvalidModel { provider_id, model_id } => {
                let mut data = Map::new();
                if let Some(provider_id) = provider_id {
                    data.insert("providerId".into(), provider_id.into());
                }
                data.insert("modelId".into(), model_id.clone().into());
                RpcError::invalid_params(Some(Value::Object(data)), Some(&format!("model not found: {model_id}")))
            }
            AcpError::InvalidEffort(effort) => RpcError::invalid_params(Some(json!({"effort": effort})), Some(&format!("effort not found: {effort}"))),
            AcpError::InvalidMode(mode) => RpcError::invalid_params(Some(json!({"mode": mode})), Some(&format!("mode not found: {mode}"))),
            AcpError::AuthRequired(provider_id) => {
                let data = provider_id.map_or_else(|| json!({}), |id| json!({"providerId": id}));
                RpcError::auth_required(Some(data), Some("provider authentication required"))
            }
            AcpError::UnknownAuthMethod(id) => RpcError::invalid_params(Some(json!({"methodId": id})), Some(&format!("unknown auth method: {id}"))),
            AcpError::Service { message, service, name } => {
                let mut data = Map::new();
                if let Some(service) = service {
                    data.insert("service".into(), service.into());
                }
                if let Some(name) = name {
                    data.insert("errorName".into(), name.into());
                }
                RpcError::internal(Some(Value::Object(data)), Some(&message))
            }
        }
    }
}

// `fromUnknownError`: auth failures by name, anything else as a generic service failure.
fn failure(service: &'static str, error: impl std::fmt::Display) -> AcpError {
    let text = error.to_string();
    if text.contains("ProviderAuthError") || text.contains("LoadAPIKeyError") {
        return AcpError::AuthRequired(None);
    }
    AcpError::Service { message: "OpenCode service failure".into(), service: Some(service), name: None }
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|e| RpcError::invalid_params(Some(json!({"_errors": [e.to_string()]})), None))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

// A JS number: integral values without a fraction.
pub fn num(value: f64) -> Value {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        return Value::from(value as i64);
    }
    serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
}

fn iso(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64).unwrap_or_default().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

// `Number(text)` for a cursor.
fn js_number(text: &str) -> f64 {
    let text = text.trim();
    if text.is_empty() {
        return 0.0;
    }
    text.parse().unwrap_or(f64::NAN)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitializeParams {
    protocol_version: Value,
    #[serde(default)]
    client_capabilities: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthenticateParams {
    method_id: String,
}

#[derive(Debug, Clone, Deserialize)]
struct NameValue {
    name: String,
    value: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RemoteKind {
    Http,
    Sse,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum McpServer {
    Remote {
        #[serde(rename = "type")]
        _kind: RemoteKind,
        name: String,
        url: String,
        headers: Vec<NameValue>,
    },
    Stdio {
        name: String,
        command: String,
        args: Vec<String>,
        env: Vec<NameValue>,
    },
}

impl McpServer {
    // The opencode config a client-supplied server becomes (`mcpConfig`).
    fn config(&self) -> (&str, Value) {
        let pairs = |items: &[NameValue]| items.iter().map(|i| (i.name.clone(), Value::from(i.value.clone()))).collect::<Map<_, _>>();
        match self {
            McpServer::Remote { name, url, headers, .. } => (name, json!({"type": "remote", "url": url, "headers": pairs(headers)})),
            McpServer::Stdio { name, command, args, env } => {
                let command: Vec<&str> = std::iter::once(command.as_str()).chain(args.iter().map(String::as_str)).collect();
                (name, json!({"type": "local", "command": command, "environment": pairs(env)}))
            }
        }
    }
}

fn stable_stringify(value: &Value) -> String {
    match value {
        Value::Array(items) => format!("[{}]", items.iter().map(stable_stringify).collect::<Vec<_>>().join(",")),
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            format!("{{{}}}", entries.iter().map(|(k, v)| format!("{}:{}", Value::from(k.as_str()), stable_stringify(v))).collect::<Vec<_>>().join(","))
        }
        other => other.to_string(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NewSessionParams {
    cwd: String,
    mcp_servers: Vec<McpServer>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoadSessionParams {
    cwd: String,
    mcp_servers: Vec<McpServer>,
    session_id: String,
}

// `session/resume` and `session/fork`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExistingSessionParams {
    cwd: String,
    session_id: String,
    #[serde(default)]
    mcp_servers: Option<Vec<McpServer>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListSessionsParams {
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionParams {
    session_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetConfigOptionParams {
    session_id: String,
    config_id: String,
    value: Value,
    #[serde(default, rename = "type")]
    kind: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetModeParams {
    session_id: String,
    mode_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetModelParams {
    session_id: String,
    model_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PromptParams {
    session_id: String,
    prompt: Vec<ContentBlock>,
    #[serde(default)]
    message_id: Option<String>,
}

// What ACP keeps per session besides the engine's row (`ACPSession.Info`).
#[derive(Debug, Clone)]
struct State {
    id: String,
    cwd: String,
    created: u64,
    model: Option<ModelRef>,
    variant: Option<String>,
    mode_id: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct PartMeta {
    kind: &'static str,
    role: &'static str,
    ignored: bool,
}

#[derive(Default)]
struct Tracker {
    started: HashSet<String>,
    shell: HashMap<String, String>,
    parts: HashMap<String, PartMeta>,
}

struct Restored {
    model: ModelRef,
    variant: Option<String>,
    mode_id: Option<String>,
}

pub struct Agent {
    engine: Arc<Engine>,
    pub(crate) peer: Peer,
    version: String,
    directory: PathBuf,
    sessions: Mutex<HashMap<String, State>>,
    directories: Mutex<HashMap<String, Arc<OnceCell<Arc<Snapshot>>>>>,
    snapshots: Mutex<HashMap<String, Arc<Snapshot>>>,
    mcp: Mutex<HashMap<String, HashSet<String>>>,
    tracker: Mutex<Tracker>,
}

fn available_commands(session_id: &str, snapshot: &Snapshot) -> Value {
    let commands: Vec<Value> = snapshot.commands.iter().map(|c| json!({"name": c.name, "description": c.description})).collect();
    json!({"sessionId": session_id, "update": {"sessionUpdate": "available_commands_update", "availableCommands": commands}})
}

fn usage(message: &AssistantMessage) -> Value {
    let tokens = &message.tokens;
    let mut out = Map::new();
    out.insert("inputTokens".into(), num(tokens.input));
    out.insert("outputTokens".into(), num(tokens.output));
    out.insert("totalTokens".into(), num(tokens.input + tokens.output + tokens.reasoning + tokens.cache.read + tokens.cache.write));
    for (key, value) in [("thoughtTokens", tokens.reasoning), ("cachedReadTokens", tokens.cache.read), ("cachedWriteTokens", tokens.cache.write)] {
        if value > 0.0 {
            out.insert(key.into(), num(value));
        }
    }
    Value::Object(out)
}

// `promptResponse`: the stop reason from the turn's last assistant message.
fn prompt_response(info: Option<&AssistantMessage>, aborted: bool, message_id: Option<&str>) -> Result<Value, AcpError> {
    let reply = |stop: &str, info: Option<&AssistantMessage>| {
        let mut out = Map::new();
        out.insert("stopReason".into(), stop.into());
        if let Some(info) = info {
            out.insert("usage".into(), usage(info));
        }
        if let Some(id) = message_id {
            out.insert("userMessageId".into(), id.into());
        }
        out.insert("_meta".into(), json!({}));
        Ok(Value::Object(out))
    };
    let Some(error) = info.and_then(|i| i.error.as_ref()) else {
        return reply(if aborted { "cancelled" } else { "end_turn" }, info);
    };
    match error {
        AssistantError::MessageAbortedError { .. } => reply("cancelled", info),
        AssistantError::MessageOutputLengthError {} => reply("max_tokens", info),
        AssistantError::ContentFilterError { .. } => reply("refusal", info),
        AssistantError::ProviderAuthError { provider_id, .. } => Err(AcpError::AuthRequired(Some(provider_id.clone()))),
        other => {
            let value = serde_json::to_value(other).unwrap_or_default();
            let message = value.pointer("/data/message").and_then(Value::as_str).unwrap_or("OpenCode prompt failed").to_string();
            let name = value.get("name").and_then(Value::as_str).map(str::to_string);
            Err(AcpError::Service { message, service: Some("session"), name })
        }
    }
}

fn selected_answer(result: &Value) -> Option<PermissionAnswer> {
    let outcome = result.get("outcome")?;
    if outcome.get("outcome")?.as_str()? != "selected" {
        return None;
    }
    match outcome.get("optionId")?.as_str()? {
        "once" => Some(PermissionAnswer::Once),
        "always" => Some(PermissionAnswer::Always),
        _ => None,
    }
}

impl Agent {
    pub fn new(engine: Arc<Engine>, peer: Peer, version: String, directory: PathBuf) -> Self {
        Self {
            engine,
            peer,
            version,
            directory,
            sessions: Mutex::default(),
            directories: Mutex::default(),
            snapshots: Mutex::default(),
            mcp: Mutex::default(),
            tracker: Mutex::default(),
        }
    }

    // A request; `after` collects `session/update` notifications that go out once the response has.
    pub async fn request(self: &Arc<Self>, method: &str, params: Value, after: &mut Vec<Value>) -> Result<Value, RpcError> {
        Ok(match method {
            "initialize" => self.initialize(parse(params)?)?,
            "authenticate" => self.authenticate(parse(params)?)?,
            "session/new" => self.new_session(parse(params)?, after).await?,
            "session/load" => self.load_session(parse(params)?).await?,
            "session/list" => self.list_sessions(parse(params)?)?,
            "session/resume" => self.resume_session(parse(params)?, after).await?,
            "session/close" => self.close_session(parse(params)?),
            "session/fork" => self.fork_session(parse(params)?).await?,
            "session/set_config_option" => self.set_config_option(parse(params)?).await?,
            "session/set_mode" => self.set_mode(parse(params)?).await?,
            "session/set_model" => self.set_model(parse(params)?).await?,
            "session/prompt" => self.prompt(parse(params)?).await?,
            other => return Err(RpcError::method_not_found(other)),
        })
    }

    pub async fn notification(self: &Arc<Self>, method: &str, params: Value) -> Result<(), RpcError> {
        match method {
            "session/cancel" => Ok(self.cancel(parse(params)?)?),
            "document/didOpen" | "document/didChange" | "document/didClose" | "document/didSave" | "document/didFocus" | "nes/accept" | "nes/reject" => Ok(()),
            other => Err(RpcError::method_not_found(other)),
        }
    }

    // The connection is gone: stop every turn it started.
    pub fn shutdown(&self) {
        let ids: Vec<String> = lock(&self.sessions).keys().cloned().collect();
        for id in ids {
            self.engine.abort(&id);
        }
    }

    fn initialize(&self, params: InitializeParams) -> Result<Value, RpcError> {
        let version = params.protocol_version.as_f64().filter(|v| v.fract() == 0.0 && (0.0..=65535.0).contains(v));
        if version.is_none() {
            return Err(RpcError::invalid_params(Some(json!({"_errors": [], "protocolVersion": {"_errors": ["Invalid input"]}})), None));
        }
        let terminal = params.client_capabilities.as_ref().and_then(|c| c.pointer("/_meta/terminal-auth")) == Some(&Value::Bool(true));
        let mut method = json!({"description": "Run `opencode auth login` in the terminal", "name": "Login with opencode", "id": AUTH_METHOD_ID});
        if terminal {
            method["_meta"] = json!({"terminal-auth": {"command": "opencode", "args": ["auth", "login"], "label": "OpenCode Login"}});
        }
        Ok(json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "loadSession": true,
                "mcpCapabilities": {"http": true, "sse": true},
                "promptCapabilities": {"embeddedContext": true, "image": true},
                "sessionCapabilities": {"close": {}, "fork": {}, "list": {}, "resume": {}},
            },
            "authMethods": [method],
            "agentInfo": {"name": "OpenCode", "version": self.version},
        }))
    }

    fn authenticate(&self, params: AuthenticateParams) -> Result<Value, AcpError> {
        if params.method_id != AUTH_METHOD_ID {
            return Err(AcpError::UnknownAuthMethod(params.method_id));
        }
        Ok(json!({}))
    }

    async fn snapshot(&self, cwd: &str) -> Result<Arc<Snapshot>, AcpError> {
        let cell = lock(&self.directories).entry(cwd.to_string()).or_default().clone();
        cell.get_or_try_init(|| async { directory::load(&self.engine, cwd).await.map(Arc::new).map_err(|e| failure("directory", e)) }).await.cloned()
    }

    async fn config_snapshot(&self, state: &State) -> Result<Arc<Snapshot>, AcpError> {
        if let Some(snapshot) = lock(&self.snapshots).get(&state.id).cloned() {
            return Ok(snapshot);
        }
        let snapshot = self.snapshot(&state.cwd).await?;
        lock(&self.snapshots).insert(state.id.clone(), snapshot.clone());
        Ok(snapshot)
    }

    fn try_session(&self, id: &str) -> Option<State> {
        lock(&self.sessions).get(id).cloned()
    }

    fn session(&self, id: &str) -> Result<State, AcpError> {
        self.try_session(id).ok_or_else(|| AcpError::SessionNotFound(id.to_string()))
    }

    fn update_session(&self, id: &str, change: impl FnOnce(&mut State)) -> Result<State, AcpError> {
        let mut sessions = lock(&self.sessions);
        let state = sessions.get_mut(id).ok_or_else(|| AcpError::SessionNotFound(id.to_string()))?;
        change(state);
        Ok(state.clone())
    }

    fn track(&self, state: &State, snapshot: &Arc<Snapshot>) {
        lock(&self.sessions).insert(state.id.clone(), state.clone());
        lock(&self.snapshots).insert(state.id.clone(), snapshot.clone());
    }

    fn options(snapshot: &Snapshot, state: &State, fallback: &ModelRef) -> Value {
        config_option::build(snapshot, state.model.as_ref().unwrap_or(fallback), state.variant.as_deref(), state.mode_id.as_deref())
    }

    fn notify_update(&self, session_id: &str, kind: &str, body: Map<String, Value>) {
        let mut update = Map::new();
        update.insert("sessionUpdate".into(), kind.into());
        update.extend(body);
        self.peer.notify("session/update", json!({"sessionId": session_id, "update": update}));
    }

    fn notify_config(&self, session_id: &str, options: &Value) {
        self.peer.notify("session/update", json!({"sessionId": session_id, "update": {"sessionUpdate": "config_option_update", "configOptions": options}}));
    }

    // Client MCP servers, registered once per session and config (`registerMcpServers`); failures are ignored.
    async fn register_mcp(&self, cwd: &str, session_id: &str, servers: &[McpServer]) {
        let pending: Vec<(String, Value, String)> = {
            let mut registered = lock(&self.mcp);
            let current = registered.entry(session_id.to_string()).or_default();
            let mut seen = HashSet::new();
            servers
                .iter()
                .map(|server| server.config())
                .filter_map(|(name, config)| {
                    let key = format!("{name}:{}", stable_stringify(&config));
                    (!current.contains(&key) && seen.insert(key.clone())).then(|| (name.to_string(), config, key))
                })
                .collect()
        };
        if pending.is_empty() {
            return;
        }
        let Ok(manager) = self.engine.mcp(Path::new(cwd)).await else { return };
        let added = futures_util::future::join_all(pending.into_iter().map(|(name, config, key)| {
            let manager = manager.clone();
            async move {
                let server: opencode_core::config::McpServer = serde_json::from_value(config).ok()?;
                manager.add(&name, server).await;
                Some(key)
            }
        }))
        .await;
        lock(&self.mcp).entry(session_id.to_string()).or_default().extend(added.into_iter().flatten());
    }

    fn backing(&self, session_id: &str) -> Result<Session, AcpError> {
        match self.engine.store.get_session(session_id) {
            Ok(Some(session)) => Ok(session),
            Ok(None) => Err(failure("session", format!("Session not found: {session_id}"))),
            Err(error) => Err(failure("session", error)),
        }
    }

    fn messages(&self, session_id: &str, recent: Option<usize>) -> Result<Vec<(Message, Vec<Part>)>, AcpError> {
        let mut rows = self.engine.store.list_messages(session_id).map_err(|e| failure("session", e))?;
        if let Some(limit) = recent {
            rows.drain(..rows.len().saturating_sub(limit));
        }
        Ok(rows)
    }

    async fn new_session(&self, params: NewSessionParams, after: &mut Vec<Value>) -> Result<Value, AcpError> {
        let snapshot = self.snapshot(&params.cwd).await?;
        let selected = snapshot.default_selection();
        let variant = snapshot.select_variant(&selected);
        let mode_id = snapshot.default_mode();
        let model = SessionModel { id: selected.model_id.clone(), provider_id: selected.provider_id.clone(), variant: variant.clone() };
        let created = self
            .engine
            .create_session(Path::new(&params.cwd), CreateSession { agent: mode_id.clone(), model: Some(model), ..Default::default() })
            .map_err(|e| failure("session", e))?;
        let state = State { id: created.id, cwd: params.cwd.clone(), created: id::now_ms(), model: Some(selected.clone()), variant, mode_id };
        self.track(&state, &snapshot);
        self.register_mcp(&params.cwd, &state.id, &params.mcp_servers).await;
        after.push(available_commands(&state.id, &snapshot));
        Ok(json!({"sessionId": state.id, "configOptions": Self::options(&snapshot, &state, &selected)}))
    }

    // `restoreSession`: the session row's model and agent, else the history's, else the defaults.
    fn restore(snapshot: &Snapshot, backing: &Session, messages: &[(Message, Vec<Part>)]) -> Restored {
        let user = messages.iter().rev().find_map(|(m, _)| m.as_user().filter(|u| !u.model.provider_id.is_empty() && !u.model.model_id.is_empty()));
        let assistant = || messages.iter().rev().find_map(|(m, _)| m.as_assistant().filter(|a| !a.provider_id.is_empty() && !a.model_id.is_empty()));
        let history: (Option<ModelRef>, Option<String>, Option<String>) = match (user, assistant()) {
            (Some(u), _) => (Some(ModelRef::new(&u.model.provider_id, &u.model.model_id)), u.model.variant.clone(), Some(u.agent.clone())),
            (None, Some(a)) => (Some(ModelRef::new(&a.provider_id, &a.model_id)), a.variant.clone(), Some(a.mode.clone())),
            (None, None) => (None, None, None),
        };
        let durable = backing.model.as_ref().map(|m| (ModelRef::new(&m.provider_id, &m.id), m.variant.clone()));
        let known = |m: &ModelRef| snapshot.model(m).is_some();
        let model = durable
            .as_ref()
            .map(|d| d.0.clone())
            .filter(known)
            .or_else(|| history.0.clone().filter(known))
            .unwrap_or_else(|| snapshot.default_selection());
        let variant = snapshot.variants(&model).and_then(|variants| {
            let usable = |source: Option<(&ModelRef, Option<&String>)>| {
                source.and_then(|(m, v)| (m == &model).then_some(v).flatten()).filter(|v| !v.is_empty() && has_variant(variants, v)).cloned()
            };
            usable(durable.as_ref().map(|d| (&d.0, d.1.as_ref())))
                .or_else(|| usable(history.0.as_ref().map(|m| (m, history.1.as_ref()))))
                .or_else(|| snapshot.select_variant(&model))
        });
        let mode_id = [backing.agent.clone(), history.2.clone()].into_iter().flatten().find(|m| snapshot.has_mode(Some(m.as_str()))).or_else(|| snapshot.default_mode());
        Restored { model, variant, mode_id }
    }

    // Shared by load, resume and fork: track the session as restored and register its MCP servers.
    async fn adopt(&self, session_id: &str, cwd: &str, servers: &[McpServer], snapshot: &Arc<Snapshot>, backing: &Session, messages: &[(Message, Vec<Part>)]) -> State {
        let restored = Self::restore(snapshot, backing, messages);
        let state = State { id: session_id.to_string(), cwd: cwd.to_string(), created: id::now_ms(), model: Some(restored.model), variant: restored.variant, mode_id: restored.mode_id };
        self.track(&state, snapshot);
        self.register_mcp(cwd, session_id, servers).await;
        state
    }

    async fn load_session(&self, params: LoadSessionParams) -> Result<Value, AcpError> {
        let snapshot = self.snapshot(&params.cwd).await?;
        let backing = self.backing(&params.session_id)?;
        let messages = self.messages(&params.session_id, None)?;
        let state = self.adopt(&params.session_id, &params.cwd, &params.mcp_servers, &snapshot, &backing, &messages).await;
        self.peer.notify("session/update", available_commands(&state.id, &snapshot));
        self.replay(&messages);
        Ok(json!({"configOptions": Self::options(&snapshot, &state, &snapshot.default_selection())}))
    }

    async fn resume_session(&self, params: ExistingSessionParams, after: &mut Vec<Value>) -> Result<Value, AcpError> {
        let snapshot = self.snapshot(&params.cwd).await?;
        let backing = self.backing(&params.session_id)?;
        let messages = self.messages(&params.session_id, Some(RECENT_MESSAGES))?;
        let state = self.adopt(&params.session_id, &params.cwd, params.mcp_servers.as_deref().unwrap_or_default(), &snapshot, &backing, &messages).await;
        after.push(available_commands(&state.id, &snapshot));
        Ok(json!({"configOptions": Self::options(&snapshot, &state, &snapshot.default_selection())}))
    }

    async fn fork_session(&self, params: ExistingSessionParams) -> Result<Value, AcpError> {
        let snapshot = self.snapshot(&params.cwd).await?;
        let forked = self.engine.fork(&params.session_id, None).map_err(|e| failure("session", e))?;
        let messages = self.messages(&forked.id, Some(RECENT_MESSAGES))?;
        let state = self.adopt(&forked.id, &params.cwd, params.mcp_servers.as_deref().unwrap_or_default(), &snapshot, &forked, &messages).await;
        self.peer.notify("session/update", available_commands(&state.id, &snapshot));
        self.replay(&messages);
        Ok(json!({"sessionId": state.id, "configOptions": Self::options(&snapshot, &state, &snapshot.default_selection())}))
    }

    fn list_sessions(&self, params: ListSessionsParams) -> Result<Value, AcpError> {
        let cursor = params.cursor.as_deref().filter(|c| !c.is_empty()).map(js_number).filter(|c| c.is_finite());
        let directory = params.cwd.as_ref().map(PathBuf::from).unwrap_or_else(|| self.directory.clone());
        let project = self.engine.project(&directory).map_err(|e| failure("session", e))?;
        let stored = self.engine.store.list_sessions(&project.id).map_err(|e| failure("session", e))?;
        let server: Vec<(u64, String, Value)> = stored
            .into_iter()
            .filter(|s| params.cwd.as_ref().is_none_or(|cwd| &s.directory == cwd))
            .filter(|s| s.parent_id.is_none())
            .take(LIST_LIMIT)
            .map(|s| (s.time.updated, s.id.clone(), json!({"sessionId": s.id, "cwd": s.directory, "title": s.title, "updatedAt": iso(s.time.updated)})))
            .collect();
        let mut live: Vec<State> = lock(&self.sessions)
            .values()
            .filter(|s| params.cwd.as_ref().is_none_or(|cwd| &s.cwd == cwd))
            .filter(|s| !server.iter().any(|(_, id, _)| id == &s.id))
            .cloned()
            .collect();
        live.sort_by_key(|s| std::cmp::Reverse(s.created));
        let mut entries: Vec<(u64, Value)> = live.into_iter().map(|s| (s.created, json!({"sessionId": s.id, "cwd": s.cwd, "updatedAt": iso(s.created)}))).collect();
        entries.extend(server.into_iter().map(|(updated, _, value)| (updated, value)));
        entries.sort_by_key(|e| std::cmp::Reverse(e.0));
        let filtered: Vec<(u64, Value)> = entries.into_iter().filter(|(updated, _)| cursor.is_none_or(|c| (*updated as f64) < c)).collect();
        let next = (filtered.len() > LIST_LIMIT).then(|| filtered[LIST_LIMIT - 1].0.to_string());
        let mut out = json!({"sessions": filtered.into_iter().take(LIST_LIMIT).map(|(_, v)| v).collect::<Vec<_>>()});
        if let Some(next) = next {
            out["nextCursor"] = next.into();
        }
        Ok(out)
    }

    fn close_session(&self, params: SessionParams) -> Value {
        let removed = lock(&self.sessions).remove(&params.session_id);
        lock(&self.mcp).remove(&params.session_id);
        lock(&self.snapshots).remove(&params.session_id);
        if removed.is_some() {
            self.engine.abort(&params.session_id);
        }
        json!({})
    }

    fn cancel(&self, params: SessionParams) -> Result<(), AcpError> {
        self.session(&params.session_id)?;
        self.engine.abort(&params.session_id);
        Ok(())
    }

    fn selected_model(snapshot: &Snapshot, value: &str) -> Result<(ModelRef, Option<String>), AcpError> {
        let (selected, variant) = config_option::parse_model_selection(value, &snapshot.providers);
        let Some(model) = snapshot.model(&selected) else {
            return Err(AcpError::InvalidModel { provider_id: Some(selected.provider_id), model_id: value.to_string() });
        };
        if let Some(variant) = variant.as_ref().filter(|v| !model.variants.contains(v)) {
            return Err(AcpError::InvalidEffort(variant.clone()));
        }
        Ok((selected, variant))
    }

    // `selectModelVariant`
    fn model_variant(snapshot: &Snapshot, current: &State, model: &ModelRef, variant: Option<String>) -> Option<String> {
        let variants = snapshot.variants(model)?;
        if variant.as_ref().is_some_and(|v| !v.is_empty()) {
            return variant;
        }
        if current.model.as_ref() == Some(model)
            && let Some(current) = current.variant.as_ref().filter(|v| !v.is_empty() && has_variant(variants, v))
        {
            return Some(current.clone());
        }
        snapshot.select_variant(model)
    }

    async fn set_config_option(&self, params: SetConfigOptionParams) -> Result<Value, RpcError> {
        let valid = params.value.is_string() || (params.value.is_boolean() && params.kind.as_deref() == Some("boolean"));
        if !valid {
            return Err(RpcError::invalid_params(Some(json!({"_errors": ["Invalid input"]})), None));
        }
        let current = self.session(&params.session_id)?;
        let snapshot = self.config_snapshot(&current).await?;
        let Some(value) = params.value.as_str() else { return Err(AcpError::InvalidConfigOption(params.config_id).into()) };
        let id = &params.session_id;
        match params.config_id.as_str() {
            "model" => {
                let (model, variant) = Self::selected_model(&snapshot, value)?;
                let variant = Self::model_variant(&snapshot, &current, &model, variant);
                let state = self.update_session(id, |s| {
                    s.variant = variant;
                    s.model = Some(model.clone());
                })?;
                let options = Self::options(&snapshot, &state, &model);
                self.notify_config(id, &options);
                Ok(json!({"configOptions": options}))
            }
            "effort" => {
                let model = current.model.clone().unwrap_or_else(|| snapshot.default_selection());
                if !snapshot.variants(&model).is_some_and(|variants| has_variant(variants, value)) {
                    return Err(AcpError::InvalidEffort(value.to_string()).into());
                }
                let state = self.update_session(id, |s| s.variant = Some(value.to_string()))?;
                Ok(json!({"configOptions": Self::options(&snapshot, &state, &model)}))
            }
            "mode" => {
                if !snapshot.has_mode(Some(value)) {
                    return Err(AcpError::InvalidMode(value.to_string()).into());
                }
                let state = self.update_session(id, |s| s.mode_id = Some(value.to_string()))?;
                Ok(json!({"configOptions": Self::options(&snapshot, &state, &snapshot.default_selection())}))
            }
            other => Err(AcpError::InvalidConfigOption(other.to_string()).into()),
        }
    }

    async fn set_mode(&self, params: SetModeParams) -> Result<Value, AcpError> {
        let current = self.session(&params.session_id)?;
        let snapshot = self.config_snapshot(&current).await?;
        if !snapshot.has_mode(Some(params.mode_id.as_str())) {
            return Err(AcpError::InvalidMode(params.mode_id));
        }
        self.update_session(&params.session_id, |s| s.mode_id = Some(params.mode_id.clone()))?;
        Ok(json!({}))
    }

    async fn set_model(&self, params: SetModelParams) -> Result<Value, AcpError> {
        let current = self.session(&params.session_id)?;
        let snapshot = self.config_snapshot(&current).await?;
        let (model, variant) = Self::selected_model(&snapshot, &params.model_id)?;
        let variant = Self::model_variant(&snapshot, &current, &model, variant);
        let state = self.update_session(&params.session_id, |s| {
            s.variant = variant;
            s.model = Some(model.clone());
        })?;
        self.notify_config(&params.session_id, &Self::options(&snapshot, &state, &model));
        Ok(json!({}))
    }

    fn latest_message(&self, session_id: &str) -> Option<String> {
        let rows = self.engine.store.list_messages(session_id).ok()?;
        rows.last().map(|(m, _)| m.id().to_string())
    }

    // The newest assistant message of the turn, what the original's prompt call returns.
    fn turn_reply(&self, session_id: &str, before: Option<&str>) -> Option<AssistantMessage> {
        let rows = self.engine.store.list_messages(session_id).ok()?;
        rows.iter().rev().filter_map(|(m, _)| m.as_assistant()).find(|m| before.is_none_or(|b| m.id.as_str() > b)).cloned()
    }

    // `usage_update` after a turn: context used by the last assistant message and the session's total cost.
    async fn usage_update(&self, session_id: &str, cwd: &str) {
        let Ok(rows) = self.engine.store.list_messages(session_id) else { return };
        let assistants: Vec<&AssistantMessage> = rows.iter().filter_map(|(m, _)| m.as_assistant()).collect();
        let Some(latest) = assistants.last() else { return };
        if latest.provider_id.is_empty() || latest.model_id.is_empty() {
            return;
        }
        let Ok(providers) = self.engine.providers(Path::new(cwd)).await else { return };
        let size = providers.iter().find(|p| p.id == latest.provider_id).and_then(|p| p.models.iter().find(|m| m.id == latest.model_id)).map_or(0, |m| m.limit_context);
        if size == 0 {
            return;
        }
        let tokens = &latest.tokens;
        let cost: f64 = assistants.iter().map(|m| m.cost).sum();
        let update = json!({"sessionUpdate": "usage_update", "used": num(tokens.input + tokens.cache.read + tokens.cache.write), "size": size, "cost": {"amount": num(cost), "currency": "USD"}});
        self.peer.notify("session/update", json!({"sessionId": session_id, "update": update}));
    }

    async fn prompt(self: &Arc<Self>, params: PromptParams) -> Result<Value, AcpError> {
        let current = self.session(&params.session_id)?;
        let snapshot = self.snapshot(&current.cwd).await?;
        let selected = match &current.model {
            Some(model) => model.clone(),
            None => {
                let model = snapshot.default_selection();
                self.update_session(&current.id, |s| s.model = Some(model.clone()))?;
                model
            }
        };
        let variant = current.variant.clone().or_else(|| snapshot.select_variant(&selected));
        let mode_id = current.mode_id.clone().or_else(|| snapshot.default_mode());
        let parts = content::prompt_parts(&params.prompt);
        let message_id = params.message_id.as_deref().filter(|m| !m.is_empty());
        let host: Arc<dyn Host> = Arc::new(AcpHost { agent: self.clone(), session_id: current.id.clone() });
        let directory = PathBuf::from(&current.cwd);
        let before = self.latest_message(&current.id);

        let result = match content::slash_command(&parts) {
            None => {
                let mut texts = Vec::new();
                let mut files = Vec::new();
                for part in parts {
                    match part {
                        PromptPart::Text { text, ignored: false, .. } => texts.push(text),
                        PromptPart::Text { .. } => {}
                        PromptPart::File(file) => files.push(file),
                    }
                }
                let input = PromptInput {
                    directory,
                    session_id: Some(current.id.clone()),
                    text: texts.join("\n"),
                    files,
                    model: Some(selected.key()),
                    agent: mode_id,
                    variant,
                    ..Default::default()
                };
                self.engine.prompt(input, host, CancellationToken::new()).await.map(drop)
            }
            Some((name, arguments)) if snapshot.commands.iter().any(|c| c.name == name) => {
                let input = CommandInput {
                    directory,
                    session_id: Some(current.id.clone()),
                    command: name,
                    arguments,
                    agent: mode_id,
                    model: Some(selected.key()),
                    variant,
                    ..Default::default()
                };
                self.engine.command(input, host, CancellationToken::new()).await.map(drop)
            }
            Some((name, _)) => {
                if name == "compact" {
                    match self.engine.summarize(&current.id, false, host, CancellationToken::new()).await {
                        Ok(_) | Err(opencode_session::Error::Aborted) => {}
                        Err(error) => return Err(failure("session", error)),
                    }
                }
                self.usage_update(&current.id, &current.cwd).await;
                return prompt_response(None, false, message_id);
            }
        };

        let info = self.turn_reply(&current.id, before.as_deref());
        let aborted = matches!(result, Err(opencode_session::Error::Aborted));
        if let Err(error) = &result
            && !aborted
            && info.as_ref().is_none_or(|i| i.error.is_none())
        {
            return Err(failure("session", error));
        }
        self.usage_update(&current.id, &current.cwd).await;
        prompt_response(info.as_ref(), aborted, message_id)
    }

    fn part_meta(&self, message_id: &str, part_id: &str) -> Option<PartMeta> {
        if let Some(meta) = lock(&self.tracker).parts.get(part_id) {
            return Some(*meta);
        }
        let part = self.engine.store.get_part(part_id).ok().flatten()?;
        let (message, _) = self.engine.store.get_message(message_id).ok().flatten()?;
        let meta = Self::remember(&mut lock(&self.tracker), &part, message.role());
        Some(meta)
    }

    fn remember(tracker: &mut Tracker, part: &Part, role: &'static str) -> PartMeta {
        let ignored = matches!(&part.body, PartBody::Text(t) if t.ignored == Some(true));
        let meta = PartMeta { kind: part.body.kind(), role, ignored };
        tracker.parts.insert(part.id.clone(), meta);
        meta
    }

    // `message.part.delta`: text as message chunks, reasoning as thought chunks.
    fn delta(&self, session_id: &str, message_id: &str, part_id: &str, field: &str, delta: &str) {
        if self.try_session(session_id).is_none() || field != "text" {
            return;
        }
        let Some(meta) = self.part_meta(message_id, part_id).filter(|m| m.role == "assistant") else { return };
        let (kind, id) = match meta.kind {
            "text" if !meta.ignored => ("agent_message_chunk", message_id),
            "reasoning" => ("agent_thought_chunk", part_id),
            _ => return,
        };
        let mut body = Map::new();
        body.insert("messageId".into(), id.into());
        body.insert("content".into(), json!({"type": "text", "text": delta}));
        self.notify_update(session_id, kind, body);
    }

    // `handleToolPart`: one `tool_call` per call id, then an update per state change.
    fn tool_part(&self, part: &Part, cwd: &str) {
        let PartBody::Tool(tool) = &part.body else { return };
        let session_id = &part.session_id;
        self.tool_start(session_id, tool, cwd);
        match &tool.state {
            ToolState::Pending(_) => {
                lock(&self.tracker).shell.remove(&tool.call_id);
            }
            ToolState::Running(state) => self.running_tool(session_id, tool, state, cwd),
            ToolState::Completed(state) => {
                self.clear_tool(&tool.call_id);
                self.notify_update(session_id, "tool_call_update", tool::completed(&tool.call_id, &tool.tool, state));
            }
            ToolState::Error(state) => {
                self.clear_tool(&tool.call_id);
                self.notify_update(session_id, "tool_call_update", tool::failed(&tool.call_id, &tool.tool, state, Some(cwd)));
            }
        }
    }

    fn tool_start(&self, session_id: &str, tool: &ToolPart, cwd: &str) {
        if !lock(&self.tracker).started.insert(tool.call_id.clone()) {
            return;
        }
        let title = match &tool.state {
            ToolState::Running(state) => state.title.as_deref(),
            ToolState::Completed(state) => Some(state.title.as_str()),
            _ => None,
        };
        self.notify_update(session_id, "tool_call", tool::pending(&tool.call_id, &tool.tool, tool.state.input(), title, Some(cwd)));
    }

    fn running_tool(&self, session_id: &str, tool: &ToolPart, state: &ToolStateRunning, cwd: &str) {
        let output = if tool.tool == "bash" { tool::shell_output(state.metadata.as_ref()) } else { None };
        if let Some(output) = output {
            let mut tracker = lock(&self.tracker);
            if tracker.shell.get(&tool.call_id).map(String::as_str) == Some(output) {
                drop(tracker);
                self.notify_update(session_id, "tool_call_update", tool::running(&tool.call_id, &tool.tool, &state.input, state.title.as_deref(), None, Some(cwd)));
                return;
            }
            tracker.shell.insert(tool.call_id.clone(), output.to_string());
        }
        self.notify_update(session_id, "tool_call_update", tool::running(&tool.call_id, &tool.tool, &state.input, state.title.as_deref(), output, Some(cwd)));
    }

    fn clear_tool(&self, call_id: &str) {
        let mut tracker = lock(&self.tracker);
        tracker.started.remove(call_id);
        tracker.shell.remove(call_id);
    }

    // `replayMessage`: stored history as the updates a live turn would have sent.
    fn replay(&self, messages: &[(Message, Vec<Part>)]) {
        for (message, parts) in messages {
            let cwd = message.as_assistant().map_or_else(|| self.directory.to_string_lossy().into_owned(), |a| a.path.cwd.clone());
            for part in parts {
                Self::remember(&mut lock(&self.tracker), part, message.role());
                if matches!(part.body, PartBody::Tool(_)) {
                    self.tool_part(part, &cwd);
                    continue;
                }
                let reasoning = matches!(part.body, PartBody::Reasoning(_));
                let kind = match (reasoning, message.role()) {
                    (true, _) => "agent_thought_chunk",
                    (false, "user") => "user_message_chunk",
                    _ => "agent_message_chunk",
                };
                let id = if reasoning { part.id.as_str() } else { message.id() };
                for chunk in content::part_chunks(part) {
                    let mut body = Map::new();
                    body.insert("messageId".into(), id.into());
                    if let Value::Object(chunk) = chunk {
                        body.extend(chunk);
                    }
                    self.notify_update(&part.session_id, kind, body);
                }
            }
        }
    }

    // `ACPPermission.Handler.process`; a subagent's request is shown on the session the client knows.
    async fn permission(&self, ask: &PermissionAsk, fallback: &str) -> PermissionAnswer {
        let Some(state) = self.try_session(&ask.session_id).or_else(|| self.try_session(fallback)) else { return PermissionAnswer::Reject };
        let input = ask.request.metadata.as_object().cloned().unwrap_or_default();
        let call_id = ask.tool.as_ref().map_or_else(|| id::create("per", false, None), |(_, call)| call.clone());
        let params = json!({"sessionId": state.id, "toolCall": permission::tool_call(&call_id, &ask.request.permission, &input), "options": permission::options()});
        let answer = self.peer.request("session/request_permission", params).await.ok().as_ref().and_then(selected_answer);
        let Some(answer) = answer else { return PermissionAnswer::Reject };
        if ask.request.permission == "edit" {
            self.write_proposed_edit(&state.id, &input);
        }
        answer
    }

    // Pushes an approved edit to the client's buffer (`writeProposedEdit`); the response is not awaited.
    fn write_proposed_edit(&self, session_id: &str, input: &Map<String, Value>) {
        let path = string(input.get("filepath")).filter(|p| !p.is_empty());
        let diff = string(input.get("diff")).filter(|d| !d.is_empty());
        let (Some(path), Some(diff)) = (path, diff) else { return };
        let Some((_, content)) = permission::proposed(path, diff) else { return };
        let peer = self.peer.clone();
        let params = json!({"sessionId": session_id, "path": path, "content": content});
        tokio::spawn(async move {
            let _ = peer.request("fs/write_text_file", params).await;
        });
    }
}

struct AcpHost {
    agent: Arc<Agent>,
    session_id: String,
}

#[async_trait::async_trait]
impl Host for AcpHost {
    fn update(&self, update: Update) {
        match update {
            Update::Delta { session_id, message_id, part_id, field, delta } => self.agent.delta(&session_id, &message_id, &part_id, field, &delta),
            Update::Tool(part) => {
                if let Some(state) = self.agent.try_session(&part.session_id) {
                    self.agent.tool_part(&part, &state.cwd);
                }
            }
            _ => {}
        }
    }

    async fn permission(&self, _request: &PermissionRequest) -> Reply {
        Reply::Reject
    }

    async fn ask(&self, ask: &PermissionAsk) -> PermissionAnswer {
        self.agent.permission(ask, &self.session_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_and_errors_serialize_like_the_original() {
        assert_eq!(num(20.0), json!(20));
        assert_eq!(num(0.25), json!(0.25));
        assert_eq!(iso(0), "1970-01-01T00:00:00.000Z");
        let error: RpcError = AcpError::SessionNotFound("ses_x".into()).into();
        assert_eq!(error.to_json(), json!({"code": -32602, "message": "Invalid params: session not found: ses_x", "data": {"sessionId": "ses_x"}}));
        let error: RpcError = AcpError::Service { message: "boom".into(), service: Some("session"), name: Some("APIError".into()) }.into();
        assert_eq!(error.to_json(), json!({"code": -32603, "message": "Internal error: boom", "data": {"service": "session", "errorName": "APIError"}}));
        let error: RpcError = AcpError::AuthRequired(Some("openai".into())).into();
        assert_eq!(error.to_json(), json!({"code": -32000, "message": "Authentication required: provider authentication required", "data": {"providerId": "openai"}}));
        assert_eq!(RpcError::method_not_found("x/y").to_json(), json!({"code": -32601, "message": "\"Method not found\": x/y", "data": {"method": "x/y"}}));
    }

    #[test]
    fn stop_reasons_follow_the_assistant_error() {
        let message = |error: Option<AssistantError>| AssistantMessage {
            id: "msg_1".into(),
            session_id: "ses_1".into(),
            time: opencode_storage::AssistantTime { created: 0, completed: None },
            error,
            parent_id: "msg_0".into(),
            model_id: "m".into(),
            provider_id: "p".into(),
            mode: "build".into(),
            agent: "build".into(),
            path: Default::default(),
            summary: None,
            cost: 0.0,
            tokens: opencode_storage::Tokens { total: None, input: 10.0, output: 5.0, reasoning: 2.0, cache: opencode_storage::TokenCache { read: 1.0, write: 0.0 } },
            structured: None,
            variant: None,
            finish: None,
        };
        let ok = prompt_response(Some(&message(None)), false, Some("u1")).unwrap();
        assert_eq!(ok, json!({"stopReason": "end_turn", "usage": {"inputTokens": 10, "outputTokens": 5, "totalTokens": 18, "thoughtTokens": 2, "cachedReadTokens": 1}, "userMessageId": "u1", "_meta": {}}));
        assert_eq!(prompt_response(Some(&message(Some(AssistantError::aborted()))), false, None).unwrap()["stopReason"], "cancelled");
        assert_eq!(prompt_response(Some(&message(Some(AssistantError::MessageOutputLengthError {}))), false, None).unwrap()["stopReason"], "max_tokens");
        assert_eq!(prompt_response(None, true, None).unwrap(), json!({"stopReason": "cancelled", "_meta": {}}));
        let failed = prompt_response(Some(&message(Some(AssistantError::UnknownError { message: "bad".into(), r#ref: None }))), false, None).unwrap_err();
        assert_eq!(failed, AcpError::Service { message: "bad".into(), service: Some("session"), name: Some("UnknownError".into()) });
    }
}
