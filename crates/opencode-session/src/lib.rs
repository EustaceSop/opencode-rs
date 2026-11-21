// The agent loop: prompt -> model -> tools -> model ... with everything persisted as session/message/part rows.
pub mod account;
mod compaction;
mod generate;
mod goal;
mod history;
mod inbox;
mod mcp;
mod plugins;
mod profile;
mod registry;
mod run;
mod shell;
mod system;
mod tools;
mod usage;
mod workspace;

use history::Row;
use indexmap::IndexMap;
use opencode_core::agent::{self, Agent, Agents};
use opencode_core::command::{self, Command};
use opencode_core::config::{self, AgentMode, LoadOptions};
use opencode_core::paths::Paths;
use opencode_core::permission::{self, Action, Decision, Rule};
use opencode_core::{auth, flag, instruction, skill};
use opencode_llm::provider::{self, Model, Provider};
use opencode_storage::{
    AgentPart, AssistantError, CompactionPart, FilePart, FilePartSource, JsonMap, Message, NewSession, Part, PartBody,
    PermissionAction, PermissionRule, Project, ProjectTime, Session, SessionModel, Store, SubtaskModel,
    SubtaskPart, TextPart, Tokens, UserMessage, UserModel, UserTime, Vcs, id, identity,
};
use opencode_tools::{PermissionAsker, PermissionRequest, Tool, ToolContext, ToolError};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub use generate::TextRequest;
pub use goal::{Goal, Status as GoalStatus};
pub use inbox::Delivery;
pub use opencode_core::command::Command as SlashCommand;
pub use opencode_lsp::Workspace;
pub use opencode_mcp::McpManager;
pub use opencode_plugin::HookName;
pub use opencode_storage::Todo;
pub use plugins::{PluginSettings, PluginStatus, ProviderAuthError, ServerInfo, ServerLauncher};
pub use shell::ShellInput;
pub use tools::{QuestionInfo, QuestionOption, QuestionRequest};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Config(String),
    #[error("{0}")]
    Storage(#[from] opencode_storage::StoreError),
    #[error("{0}")]
    Llm(#[from] opencode_llm::Error),
    #[error("Aborted")]
    Aborted,
    // `Session.BusyError`
    #[error("Session {0} is busy")]
    Busy(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    Once,
    Always,
    Reject,
}

// A permission request as the original publishes it (`permission.asked`).
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionAsk {
    // The asking session; a subagent's own session, not its parent's.
    pub session_id: String,
    pub request: PermissionRequest,
    // (message id, call id) of the asking tool call.
    pub tool: Option<(String, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PermissionAnswer {
    Once,
    Always,
    Reject,
    // Rejected with feedback for the model; the loop goes on.
    Corrected(String),
}

impl From<Reply> for PermissionAnswer {
    fn from(reply: Reply) -> Self {
        match reply {
            Reply::Once => Self::Once,
            Reply::Always => Self::Always,
            Reply::Reject => Self::Reject,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Idle,
    Busy,
    // `next` is the epoch-ms time of the next attempt.
    Retry { attempt: u32, message: String, next: u64 },
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Update {
    Text(String),
    Reasoning(String),
    // Sent on every state change of a tool call.
    Tool(Box<Part>),
    StepFinish { reason: String, tokens: Tokens },
    Retry { attempt: u32, message: String, delay_ms: u64 },
    Status { session_id: String, status: Status },
    Title { session_id: String, title: String },
    Compacted { session_id: String },
    Todos { session_id: String, todos: Vec<Todo> },
    // A failure recorded on the session, message only. Not sent for aborts.
    Error { session_id: String, message: String },
    // Everything a subagent session emits, wrapped with its session id.
    Child { session_id: String, update: Box<Update> },
    // A streamed chunk of a stored text or reasoning part (`message.part.delta`); the same chunk also arrives as `Text`/`Reasoning`.
    Delta { session_id: String, message_id: String, part_id: String, field: &'static str, delta: String },
    // `session.error` with the original's error object; sent for aborts and recoverable overflows too.
    SessionError { session_id: String, error: AssistantError },
    // Any other instance bus event raised during the turn (`file.edited`, `file.watcher.updated`, `session.diff`).
    Event { directory: String, kind: String, properties: Value },
}

// An instance bus event the engine raises outside the store (`Engine::watch_events`).
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceEvent {
    pub directory: String,
    pub kind: String,
    pub properties: Value,
}

#[async_trait::async_trait]
pub trait Host: Send + Sync {
    fn update(&self, update: Update);
    async fn permission(&self, request: &PermissionRequest) -> Reply;
    // What the engine calls; override to see the asking session and tool call, or to reject with feedback.
    async fn ask(&self, ask: &PermissionAsk) -> PermissionAnswer {
        self.permission(&ask.request).await.into()
    }
    // Answers in question order (selected labels or typed text); None dismisses the question.
    async fn question(&self, _request: &QuestionRequest) -> Option<Vec<Vec<String>>> {
        None
    }
}

#[derive(Debug, Clone, Default)]
pub struct PromptInput {
    pub directory: PathBuf,
    pub session_id: Option<String>,
    pub text: String,
    // "provider/model"
    pub model: Option<String>,
    pub agent: Option<String>,
    // Attachments: `file:`, `data:` or remote urls.
    pub files: Vec<FilePart>,
    // Extra system text for this message.
    pub system: Option<String>,
    pub variant: Option<String>,
    // Legacy per-tool switches; when non-empty they replace the session permission rules.
    pub tools: Option<BTreeMap<String, bool>>,
    // Persist the message without running the loop.
    pub no_reply: bool,
    pub message_id: Option<String>,
    // Rust build: how a prompt sent while the session runs is delivered (v2 steer / queue).
    pub delivery: Delivery,
}

#[derive(Debug, Clone, Default)]
pub struct CommandInput {
    pub directory: PathBuf,
    pub session_id: Option<String>,
    pub command: String,
    pub arguments: String,
    pub agent: Option<String>,
    // "provider/model"
    pub model: Option<String>,
    pub variant: Option<String>,
    pub files: Vec<FilePart>,
    pub message_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CreateSession {
    pub id: Option<String>,
    pub parent_id: Option<String>,
    pub workspace_id: Option<String>,
    pub title: Option<String>,
    pub agent: Option<String>,
    pub model: Option<SessionModel>,
    pub metadata: Option<JsonMap>,
    pub permission: Option<Vec<PermissionRule>>,
}

enum RunEnd {
    Running,
    Ok,
    Failed,
}

enum Acquire {
    Owner(CancellationToken),
    Join(watch::Receiver<RunEnd>),
}

struct Run {
    cancel: CancellationToken,
    done: watch::Sender<RunEnd>,
    status: Status,
}

pub struct Engine {
    pub paths: Paths,
    pub store: Store,
    http: reqwest::Client,
    approvals: Arc<Approvals>,
    claims: instruction::Claims,
    running: Mutex<HashMap<String, Run>>,
    background: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    // One set of MCP connections per instance directory, connected on first use.
    mcp: Mutex<HashMap<PathBuf, Arc<tokio::sync::OnceCell<McpManager>>>>,
    // Snapshots, formatters and language servers, one bundle per instance directory.
    workspaces: Mutex<HashMap<PathBuf, Arc<Workspace>>>,
    events: Arc<workspace::Events>,
    // Diff summaries run one at a time, in the order they were started.
    summaries: Arc<tokio::sync::Mutex<()>>,
    // One plugin sidecar per instance directory, started on first use when something was discovered.
    plugins: Mutex<HashMap<PathBuf, plugins::Slot>>,
    // Why a directory has no plugins although some were configured.
    plugin_failures: Mutex<HashMap<PathBuf, Vec<String>>>,
    plugin_settings: Mutex<PluginSettings>,
    server: Mutex<Option<ServerInfo>>,
    launcher: Mutex<Option<Arc<dyn ServerLauncher>>>,
    catalog: Arc<Mutex<registry::CatalogState>>,
    catalog_load: tokio::sync::Mutex<()>,
    // Discovery and plugin-hook results per instance directory, with the key of the inputs they came from.
    registries: Mutex<HashMap<PathBuf, (u64, Arc<opencode_llm::hooks::Overrides>)>>,
    // The active console org's config, fetched once per (account, org, url).
    console: Mutex<Option<ConsoleConfig>>,
    console_load: tokio::sync::Mutex<()>,
    // Rust build: this engine, once a caller shares it (`attach`), so background work can start a run later.
    this: std::sync::OnceLock<std::sync::Weak<Engine>>,
    // Rust build: queued prompts wait for the session's run to end, one at a time per session.
    queues: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    // Rust build: `/goal` state per session (v2 `plugin/goal.ts`).
    goals: Mutex<HashMap<String, goal::Goal>>,
}

struct ConsoleConfig {
    key: (String, String, String),
    remote: Option<config::RemoteConfig>,
}

// A directory's config after the plugins' `config` hook, with its running plugins.
struct Base {
    worktree: Option<PathBuf>,
    opts: LoadOptions,
    loaded: config::Loaded,
    plugins: Option<Arc<plugins::Plugins>>,
}

// Everything derived from one instance directory.
struct Ctx {
    cwd: PathBuf,
    worktree: Option<PathBuf>,
    root: PathBuf,
    info: config::Info,
    flags: flag::Flags,
    agents: Agents,
    providers: Vec<Provider>,
    skills: skill::Registry,
    instruction: instruction::Context,
    tools: Vec<Arc<dyn Tool>>,
    // Built-ins the model sees under the "agent" profile's names (none in the "opencode" profile).
    names: profile::Names,
    paths_data: PathBuf,
    workspace: Arc<Workspace>,
    plugins: Option<Arc<plugins::Plugins>>,
}

impl Ctx {
    fn model(&self, provider_id: &str, model_id: &str) -> Result<(&Provider, &Model), Error> {
        self.providers
            .iter()
            .find(|p| p.id == provider_id)
            .and_then(|p| p.models.iter().find(|m| m.id == model_id).map(|m| (p, m)))
            .ok_or_else(|| Error::Config(format!("Model not found: {provider_id}/{model_id}.")))
    }

    fn agent(&self, name: &str) -> Result<&Agent, Error> {
        self.agents.get(name).ok_or_else(|| {
            let available: Vec<&str> = self.agents.list().into_iter().filter(|a| a.hidden != Some(true)).map(|a| a.name.as_str()).collect();
            let hint = if available.is_empty() { String::new() } else { format!(" Available agents: {}", available.join(", ")) };
            Error::Config(format!("Agent not found: \"{name}\".{hint}"))
        })
    }

    fn default_agent(&self) -> Result<&Agent, Error> {
        self.agents.default_info().map_err(|e| Error::Config(e.to_string()))
    }

    // Built-ins, config commands, `extra` (MCP prompts), then skills.
    fn commands(&self, extra: Vec<Command>) -> IndexMap<String, Command> {
        let listed: Vec<skill::Skill> = self.skills.all().into_iter().cloned().collect();
        let mut commands = command::registry(&self.info, &self.root, extra, &listed);
        // Rust build: v2's built-in `/goal`, unless a command of that name exists.
        commands.entry("goal".to_string()).or_insert_with(goal::command);
        commands
    }

    // `Provider.getSmallModel`
    fn small_model(&self, provider_id: &str) -> Option<(&Provider, &Model)> {
        if let Some(small) = self.info.small_model_ref() {
            return self.model(&small.provider_id, &small.model_id).ok();
        }
        let provider = self.providers.iter().find(|p| p.id == provider_id)?;
        if provider_id == "azure" || provider_id == "azure-cognitive-services" {
            return None;
        }
        let priority: &[&str] = if provider_id.starts_with("opencode") {
            &["gpt-nano"]
        } else if provider_id.starts_with("github-copilot") {
            &["gpt-mini", "gemini-flash", "gpt-nano", "claude-haiku"]
        } else {
            &["gemini-flash", "gpt-nano", "claude-haiku"]
        };
        let mut models: Vec<&Model> = provider.models.iter().collect();
        models.sort_by(|a, b| b.release_date.cmp(&a.release_date).then_with(|| b.id.cmp(&a.id)));
        for family in priority {
            let mut candidates = models.iter().filter(|m| m.family == *family);
            if provider_id == "amazon-bedrock" {
                let all: Vec<&&Model> = candidates.collect();
                let prefixed = |prefix: &str| all.iter().find(|m| m.id.starts_with(prefix)).map(|m| **m);
                let region = provider.options.get("region").and_then(Value::as_str).and_then(|r| r.split('-').next());
                let regional = region.filter(|r| matches!(*r, "us" | "eu")).and_then(|r| prefixed(&format!("{r}.")));
                let plain = || all.iter().find(|m| !["global.", "us.", "eu."].iter().any(|p| m.id.starts_with(p))).map(|m| **m);
                if let Some(model) = prefixed("global.").or(regional).or_else(plain) {
                    return Some((provider, model));
                }
                continue;
            }
            if let Some(model) = candidates.next() {
                return Some((provider, model));
            }
        }
        None
    }

    fn plan_path(&self, session: &Session) -> PathBuf {
        let base = match &self.worktree {
            Some(worktree) => worktree.join(".opencode").join("plans"),
            None => self.paths_data.join("plans"),
        };
        base.join(format!("{}-{}.md", session.time.created, session.slug))
    }
}

// The "always" approvals of `Permission`'s state; unlike `permission::Approvals` they can be revoked.
#[derive(Default)]
struct Approvals {
    rules: Mutex<Vec<Rule>>,
}

impl Approvals {
    fn new() -> Self {
        Self::default()
    }

    fn rules(&self) -> Vec<Rule> {
        self.rules.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn approve<'a>(&self, permission: &str, always: impl IntoIterator<Item = &'a str>) {
        let mut rules = self.rules.lock().unwrap_or_else(|e| e.into_inner());
        rules.extend(always.into_iter().map(|pattern| Rule::new(permission, pattern, Action::Allow)));
    }

    fn revoke(&self, permission: &str, pattern: &str) -> bool {
        let mut rules = self.rules.lock().unwrap_or_else(|e| e.into_inner());
        let before = rules.len();
        rules.retain(|rule| rule.permission != permission || rule.pattern != pattern);
        rules.len() != before
    }

    // `Permission.ask` up to the point of prompting: any deny wins, else ask if any pattern asks.
    fn check(&self, permission: &str, patterns: &[&str], ruleset: &[Rule]) -> Decision {
        let approved = self.rules();
        let mut ask = false;
        for pattern in patterns {
            match permission::evaluate(permission, pattern, &[ruleset, &approved]).action {
                Action::Deny => {
                    let relevant = ruleset.iter().filter(|r| permission::wildcard_match(permission, &r.permission)).cloned().collect();
                    return Decision::Deny(relevant);
                }
                Action::Allow => {}
                Action::Ask => ask = true,
            }
        }
        if ask { Decision::Ask } else { Decision::Allow }
    }
}

struct Asker {
    host: Arc<dyn Host>,
    approvals: Arc<Approvals>,
    ruleset: Vec<Rule>,
    session_id: String,
    // One prompt at a time per step; a rejection fails every request still waiting behind it.
    gate: tokio::sync::Mutex<()>,
    rejected: AtomicBool,
}

impl Asker {
    fn new(host: Arc<dyn Host>, approvals: Arc<Approvals>, ruleset: Vec<Rule>, session_id: &str) -> Self {
        Self {
            host,
            approvals,
            ruleset,
            session_id: session_id.to_string(),
            gate: tokio::sync::Mutex::new(()),
            rejected: AtomicBool::new(false),
        }
    }

    // The asker a single tool call sees.
    fn for_call(self: &Arc<Self>, message_id: &str, call_id: &str) -> Arc<dyn PermissionAsker> {
        Arc::new(CallAsker { asker: self.clone(), tool: (message_id.to_string(), call_id.to_string()) })
    }

    async fn ask_with(&self, req: PermissionRequest, ruleset: &[Rule], tool: Option<(String, String)>) -> Result<(), ToolError> {
        let patterns: Vec<&str> = req.patterns.iter().map(String::as_str).collect();
        match self.approvals.check(&req.permission, &patterns, ruleset) {
            Decision::Allow => return Ok(()),
            Decision::Deny(rules) => return Err(ToolError::Other(permission::denied_message(&rules))),
            Decision::Ask => {}
        }
        let _turn = self.gate.lock().await;
        if self.rejected.load(Ordering::SeqCst) {
            return Err(ToolError::Rejected);
        }
        // An earlier "always" reply may already cover this request.
        if self.approvals.check(&req.permission, &patterns, ruleset) == Decision::Allow {
            return Ok(());
        }
        let ask = PermissionAsk { session_id: self.session_id.clone(), request: req, tool };
        match self.host.ask(&ask).await {
            PermissionAnswer::Once => Ok(()),
            PermissionAnswer::Always => {
                self.approvals.approve(&ask.request.permission, ask.request.always.iter().map(String::as_str));
                Ok(())
            }
            PermissionAnswer::Corrected(feedback) if !feedback.is_empty() => {
                self.rejected.store(true, Ordering::SeqCst);
                Err(ToolError::Other(permission::corrected_message(&feedback)))
            }
            PermissionAnswer::Reject | PermissionAnswer::Corrected(_) => {
                self.rejected.store(true, Ordering::SeqCst);
                Err(ToolError::Rejected)
            }
        }
    }
}

struct CallAsker {
    asker: Arc<Asker>,
    tool: (String, String),
}

#[async_trait::async_trait]
impl PermissionAsker for CallAsker {
    async fn ask(&self, req: PermissionRequest) -> Result<(), ToolError> {
        self.asker.ask_with(req, &self.asker.ruleset, Some(self.tool.clone())).await
    }
}

struct AllowAll;

#[async_trait::async_trait]
impl PermissionAsker for AllowAll {
    async fn ask(&self, _req: PermissionRequest) -> Result<(), ToolError> {
        Ok(())
    }
}

struct ChildHost {
    parent: Arc<dyn Host>,
    session_id: String,
}

#[async_trait::async_trait]
impl Host for ChildHost {
    fn update(&self, update: Update) {
        self.parent.update(Update::Child { session_id: self.session_id.clone(), update: Box::new(update) });
    }

    async fn permission(&self, request: &PermissionRequest) -> Reply {
        self.parent.permission(request).await
    }

    async fn ask(&self, ask: &PermissionAsk) -> PermissionAnswer {
        self.parent.ask(ask).await
    }

    async fn question(&self, request: &QuestionRequest) -> Option<Vec<Vec<String>>> {
        self.parent.question(request).await
    }
}

fn unknown_error(message: impl Into<String>) -> AssistantError {
    AssistantError::UnknownError { message: message.into(), r#ref: None }
}

// `errorMessage`-style text of an error object: its message, else its name.
fn error_text(error: &AssistantError) -> String {
    let value = serde_json::to_value(error).unwrap_or_default();
    let message = value.pointer("/data/message").and_then(Value::as_str).filter(|m| !m.is_empty());
    message.or_else(|| value.get("name").and_then(Value::as_str)).unwrap_or("UnknownError").to_string()
}

// Reports a failure in both forms.
fn report(host: &Arc<dyn Host>, session_id: &str, error: AssistantError) {
    host.update(Update::SessionError { session_id: session_id.to_string(), error: error.clone() });
    host.update(Update::Error { session_id: session_id.to_string(), message: error_text(&error) });
}

fn object(value: Value) -> JsonMap {
    match value {
        Value::Object(map) => map,
        _ => JsonMap::new(),
    }
}

enum PartInput {
    Text { text: String, synthetic: bool },
    File(FilePart),
    Agent(String),
    Subtask(SubtaskPart),
}

// The prompt part shapes plugins see in `command.execute.before`.
fn part_input_json(part: &PartInput) -> Value {
    let typed = |value: Result<Value, serde_json::Error>, kind: &str| {
        let mut value = value.unwrap_or_else(|_| json!({}));
        if let Some(map) = value.as_object_mut() {
            map.insert("type".into(), kind.into());
        }
        value
    };
    match part {
        PartInput::Text { text, synthetic: true } => json!({"type": "text", "text": text, "synthetic": true}),
        PartInput::Text { text, synthetic: false } => json!({"type": "text", "text": text}),
        PartInput::File(file) => typed(serde_json::to_value(file), "file"),
        PartInput::Agent(name) => json!({"type": "agent", "name": name}),
        PartInput::Subtask(task) => typed(serde_json::to_value(task), "subtask"),
    }
}

fn part_input_from(value: Value) -> Option<PartInput> {
    match value.get("type")?.as_str()? {
        "text" => Some(PartInput::Text { text: value.get("text")?.as_str()?.to_string(), synthetic: value.get("synthetic") == Some(&Value::Bool(true)) }),
        "file" => serde_json::from_value(value).ok().map(PartInput::File),
        "agent" => Some(PartInput::Agent(value.get("name")?.as_str()?.to_string())),
        "subtask" => serde_json::from_value(value).ok().map(PartInput::Subtask),
        _ => None,
    }
}

#[derive(Default)]
struct Submission {
    agent: Option<String>,
    model: Option<(String, String)>,
    variant: Option<String>,
    system: Option<String>,
    tools: Option<BTreeMap<String, bool>>,
    parts: Vec<PartInput>,
    no_reply: bool,
    message_id: Option<String>,
}

fn parse_model(spec: &str) -> Result<(String, String), Error> {
    provider::parse_model(spec)
        .map(|(p, m)| (p.to_string(), m.to_string()))
        .ok_or_else(|| Error::Config("model must look like provider/model".into()))
}

// Windows verbatim paths (\\?\C:\...) never match the plain paths models and tools produce.
fn plain_directory(directory: PathBuf) -> PathBuf {
    match directory.to_str().and_then(|d| d.strip_prefix(r"\\?\")).filter(|d| !d.starts_with("UNC")) {
        Some(plain) => PathBuf::from(plain),
        None => directory,
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = (bytes[i] == b'%').then(|| text.get(i + 1..i + 3)).flatten().and_then(|h| u8::from_str_radix(h, 16).ok());
        match hex {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// `fileURLToPath` plus the raw query string.
fn file_url_path(url: &str) -> Option<(PathBuf, &str)> {
    let rest = url.strip_prefix("file://")?;
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let decoded = percent_decode(path);
    let drive = decoded.strip_prefix('/').filter(|p| p.as_bytes().get(1) == Some(&b':'));
    let path = match drive {
        Some(windows) => PathBuf::from(windows.replace('/', "\\")),
        None => PathBuf::from(decoded),
    };
    Some((path, query))
}

// `pathToFileURL(path).href`, the form `PromptInput::files` expects for local files.
pub fn file_url(path: &Path) -> String {
    path_to_file_url(path)
}

fn path_to_file_url(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file://");
    if !text.starts_with('/') {
        out.push('/');
    }
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' | b'@' | b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'=' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn query_number(query: &str, key: &str) -> Option<u64> {
    query.split('&').find_map(|pair| pair.split_once('=').filter(|(k, _)| *k == key)).and_then(|(_, v)| v.parse().ok())
}

fn base64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn decode_data_url(url: &str) -> String {
    use base64::Engine as _;
    let Some((head, data)) = url.split_once(',') else { return String::new() };
    if head.ends_with(";base64") {
        let bytes = base64::engine::general_purpose::STANDARD.decode(data.trim()).unwrap_or_default();
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        percent_decode(data)
    }
}

fn synthetic(text: impl Into<String>) -> PartBody {
    PartBody::Text(TextPart { text: text.into(), synthetic: Some(true), ignored: None, time: None, metadata: None })
}

// `Provider.sort`: preferred families first, "latest" ids first, then id descending.
fn best_model(models: &[Model]) -> Option<&Model> {
    const PRIORITY: [&str; 4] = ["gpt-5", "claude-sonnet-4", "big-pickle", "gemini-3-pro"];
    let rank = |m: &Model| PRIORITY.iter().position(|p| m.id.contains(p)).map_or(-1, |i| i as i64);
    models.iter().max_by(|a, b| {
        rank(a).cmp(&rank(b)).then_with(|| a.id.contains("latest").cmp(&b.id.contains("latest"))).then_with(|| a.id.cmp(&b.id))
    })
}

// `FSUtil.resolve`: the real path, else the path as given; no verbatim prefix and platform separators.
pub fn resolve_directory(directory: &Path) -> String {
    let real = std::fs::canonicalize(directory).unwrap_or_else(|_| directory.to_path_buf());
    let text = plain_directory(real).to_string_lossy().into_owned();
    if cfg!(windows) { text.replace('/', "\\") } else { text }
}

// `Project.saveProjectDirectory`: the global project keeps no directories, and a failure does not fail the caller.
pub fn remember_directory(store: &opencode_storage::Store, project_id: &str, directory: &Path) {
    if project_id == opencode_storage::GLOBAL_PROJECT_ID {
        return;
    }
    let _ = store.add_project_directory(project_id, &resolve_directory(directory), None, false);
}

fn forked_title(title: &str) -> String {
    let numbered = title.strip_suffix(')').and_then(|t| t.rsplit_once(" (fork #")).filter(|(base, _)| !base.is_empty());
    match numbered.and_then(|(base, n)| n.parse::<u64>().ok().filter(|_| n.bytes().all(|b| b.is_ascii_digit())).map(|n| (base, n))) {
        Some((base, n)) => format!("{base} (fork #{})", n + 1),
        None => format!("{title} (fork #1)"),
    }
}

impl Engine {
    pub fn new(paths: Paths) -> Result<Self, Error> {
        // The original creates its directories at startup; a fresh machine has none of them.
        for dir in [&paths.data, &paths.state, &paths.cache] {
            std::fs::create_dir_all(dir).map_err(|e| Error::Config(format!("cannot create {}: {e}", dir.display())))?;
        }
        // Managed ripgrep lives under the same cache dir the original uses.
        opencode_tools::ripgrep::set_cache_dir(paths.cache.clone());
        let store = Store::open(Store::default_path(&paths.data))?;
        Ok(Self::with_store(paths, store))
    }

    pub fn with_store(paths: Paths, store: Store) -> Self {
        Self {
            paths,
            store,
            http: reqwest::Client::new(),
            approvals: Arc::new(Approvals::new()),
            claims: instruction::Claims::new(),
            running: Mutex::new(HashMap::new()),
            background: Mutex::new(Vec::new()),
            mcp: Mutex::new(HashMap::new()),
            workspaces: Mutex::new(HashMap::new()),
            events: Arc::new(workspace::Events::default()),
            summaries: Arc::new(tokio::sync::Mutex::new(())),
            plugins: Mutex::new(HashMap::new()),
            plugin_failures: Mutex::new(HashMap::new()),
            plugin_settings: Mutex::new(PluginSettings::default()),
            server: Mutex::new(None),
            launcher: Mutex::new(None),
            catalog: Arc::default(),
            catalog_load: tokio::sync::Mutex::new(()),
            registries: Mutex::new(HashMap::new()),
            console: Mutex::new(None),
            console_load: tokio::sync::Mutex::new(()),
            this: std::sync::OnceLock::new(),
            queues: Mutex::new(HashMap::new()),
            goals: Mutex::new(HashMap::new()),
        }
    }

    // Rust build: lets work that outlives a turn (background shells, `/goal`) start runs on this engine.
    pub fn attach(self: &Arc<Self>) {
        let _ = self.this.set(Arc::downgrade(self));
    }

    fn console_key(&self) -> Option<(String, String, String)> {
        let active = self.store.account_active().ok().flatten()?;
        Some((active.id, active.active_org_id?, active.url))
    }

    // Fetches the active org's config when the active account or org changed since the last fetch. Failures leave
    // the org config out, as the original does.
    pub(crate) async fn refresh_console(&self) {
        let Some(key) = self.console_key() else { return };
        let fetched = |console: &Option<ConsoleConfig>| console.as_ref().is_some_and(|c| c.key == key);
        if fetched(&self.console.lock().unwrap_or_else(|e| e.into_inner())) {
            return;
        }
        let _turn = self.console_load.lock().await;
        if fetched(&self.console.lock().unwrap_or_else(|e| e.into_inner())) {
            return;
        }
        let accounts = account::Accounts::new(self.store.clone(), self.http.clone());
        let remote = match accounts.active_config().await {
            Ok(Some(found)) => found.config.map(|config| config::RemoteConfig { source: format!("{}/api/config", found.url), config }),
            _ => None,
        };
        *self.console.lock().unwrap_or_else(|e| e.into_inner()) = Some(ConsoleConfig { key, remote });
    }

    fn console_remote(&self) -> Option<config::RemoteConfig> {
        let key = self.console_key()?;
        let console = self.console.lock().unwrap_or_else(|e| e.into_inner());
        console.as_ref().filter(|c| c.key == key)?.remote.clone()
    }

    // `Config.get` for a directory: every local layer plus the console org config last fetched.
    pub(crate) fn load_config(&self, opts: &LoadOptions) -> Result<config::Loaded, Error> {
        let remote = self.console_remote();
        config::load_with_remote(&self.paths, opts, &flag::process_env, remote.as_ref()).map_err(|e| Error::Config(e.to_string()))
    }

    // Instance bus events that do not come from store rows: `file.edited`, `file.watcher.updated`, `session.diff`,
    // `lsp.updated`. Events raised during a turn also reach that turn's host as `Update::Event`.
    pub fn watch_events(&self, listener: impl Fn(&InstanceEvent) + Send + Sync + 'static) {
        self.events.watch(Arc::new(listener));
    }

    async fn mcp_for(&self, ctx: &Ctx) -> McpManager {
        let cell = self.mcp.lock().unwrap_or_else(|e| e.into_inner()).entry(ctx.cwd.clone()).or_default().clone();
        cell.get_or_init(|| async {
            let mut options = opencode_mcp::McpOptions::new(ctx.cwd.clone(), std::env::vars().collect(), self.paths.data.clone());
            options.default_timeout = ctx.info.experimental.mcp_timeout;
            options.version = VERSION.to_string();
            McpManager::connect(ctx.info.mcp.clone(), options).await
        })
        .await
        .clone()
    }

    // The MCP servers of an instance directory: connected from its config on first use, then shared by every prompt.
    pub async fn mcp(&self, directory: &Path) -> Result<McpManager, Error> {
        let ctx = self.instance(&plain_directory(directory.to_path_buf()), None).await?;
        Ok(self.mcp_for(&ctx).await)
    }

    // Closes a directory's MCP servers, language servers and snapshot upkeep; the next use starts them again from the
    // config as it is then.
    pub async fn dispose(&self, directory: &Path) {
        let key = plain_directory(directory.to_path_buf());
        let cell = self.mcp.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
        if let Some(manager) = cell.as_ref().and_then(|cell| cell.get()) {
            manager.shutdown().await;
        }
        let workspace = self.workspaces.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
        if let Some(workspace) = workspace {
            workspace.dispose().await;
        }
        self.dispose_plugins(&key).await;
        self.forget_overrides(Some(&key));
        // The next instance fetches the org config again, as a new instance does in the original.
        *self.console.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    // Closes every MCP server and language server (and kills local server processes).
    pub async fn shutdown(&self) {
        let cells: Vec<_> = self.mcp.lock().unwrap_or_else(|e| e.into_inner()).drain().map(|(_, cell)| cell).collect();
        for cell in cells {
            if let Some(manager) = cell.get() {
                manager.shutdown().await;
            }
        }
        let workspaces: Vec<_> = self.workspaces.lock().unwrap_or_else(|e| e.into_inner()).drain().map(|(_, w)| w).collect();
        for workspace in workspaces {
            workspace.dispose().await;
        }
        self.shutdown_plugins().await;
        self.forget_overrides(None);
        self.stop_catalog_refresh();
        // Rust build: background shells started by this engine end with it.
        opencode_tools::bash::background::kill_owned(&self.paths.data);
    }

    // Every slash command of a directory, MCP prompts included (their templates fetched).
    pub async fn commands(&self, directory: &Path) -> Result<Vec<Command>, Error> {
        let ctx = self.instance(&plain_directory(directory.to_path_buf()), None).await?;
        let manager = self.mcp_for(&ctx).await;
        let prompts = mcp::prompt_commands(&manager).await;
        let templates = futures_util::future::join_all(prompts.iter().map(|prompt| mcp::template(&manager, prompt))).await;
        let extra = prompts.into_iter().zip(templates).map(|(prompt, template)| Command { template, ..prompt.command }).collect();
        Ok(ctx.commands(extra).into_values().collect())
    }

    // The directory's providers as the loop uses them: catalog, config, auth, built-in discovery and plugin hooks.
    pub async fn providers(&self, directory: &Path) -> Result<Vec<Provider>, Error> {
        let ctx = self.instance(&plain_directory(directory.to_path_buf()), None).await?;
        Ok(ctx.providers.clone())
    }

    // `Provider.defaultModel` for a directory, as (provider id, model id).
    pub fn default_model(&self, directory: &Path) -> Result<Option<(String, String)>, Error> {
        let ctx = self.context(&plain_directory(directory.to_path_buf()))?;
        Ok(self.default_model_in(&ctx))
    }

    // The project a directory belongs to; its row is created on first use. Like `Project.fromDirectory`, the
    // directory's worktree root is remembered among the project's directories.
    pub fn project(&self, directory: &Path) -> Result<Project, Error> {
        let directory = plain_directory(directory.to_path_buf());
        let resolved = identity::resolve(&directory);
        if let Some(project) = self.store.get_project(&resolved.id)? {
            remember_directory(&self.store, &project.id, &resolved.directory);
            return Ok(project);
        }
        let project = self.new_project(&directory, resolved.id)?;
        remember_directory(&self.store, &project.id, &resolved.directory);
        Ok(project)
    }

    fn new_project(&self, directory: &Path, project_id: String) -> Result<Project, Error> {
        let worktree = config::find_worktree(directory);
        let now = id::now_ms();
        let project = Project {
            id: project_id,
            worktree: worktree.as_ref().map_or_else(|| "/".to_string(), |w| w.to_string_lossy().into_owned()),
            vcs: worktree.as_ref().map(|_| Vcs::Git),
            name: None,
            icon: None,
            commands: None,
            time: ProjectTime { created: now, updated: now, initialized: None },
            sandboxes: Vec::new(),
        };
        self.store.upsert_project(&project)?;
        Ok(project)
    }

    // Creates a session row (and the project row on first use) the way `Session.create` does.
    pub fn create_session(&self, directory: &Path, input: CreateSession) -> Result<Session, Error> {
        let directory = plain_directory(directory.to_path_buf());
        let project = self.project(&directory)?;
        let root = config::find_worktree(&directory).unwrap_or_else(|| PathBuf::from("/"));
        let session = Session::new(NewSession {
            id: input.id,
            project_id: project.id,
            directory: directory.to_string_lossy().into_owned(),
            version: VERSION.to_string(),
            path: Some(agent::relative(&root, &directory).replace('\\', "/")),
            parent_id: input.parent_id,
            workspace_id: input.workspace_id,
            title: input.title,
            agent: input.agent,
            model: input.model,
            metadata: input.metadata,
            permission: input.permission,
        })
        .map_err(|e| Error::Config(e.to_string()))?;
        self.store.create_session(&session)?;
        Ok(session)
    }

    fn base(&self, directory: &Path) -> Result<Base, Error> {
        let worktree = config::find_worktree(directory);
        let opts = LoadOptions { cwd: directory.to_path_buf(), worktree: worktree.clone() };
        let mut loaded = self.load_config(&opts)?;
        // A running plugin host has had its say on this config (the `config` hook).
        let plugins = self.loaded_plugins(directory);
        if let Some(changed) = plugins.as_ref().and_then(|p| p.config_for(&loaded.config)) {
            loaded.config = changed;
        }
        Ok(Base { worktree, opts, loaded, plugins })
    }

    // Providers from what is already known (loaded catalog, computed overrides); `instance` computes the rest.
    fn context(&self, directory: &Path) -> Result<Arc<Ctx>, Error> {
        let base = self.base(directory)?;
        let (catalog, generation) = self.cached_catalog();
        let entries = auth::read_all(&self.paths.auth_file());
        let key = registry::key(generation, base.plugins.as_ref(), &base.loaded.config, &entries);
        let overrides = self.cached_overrides(directory, key).unwrap_or_default();
        let providers = provider::build_with(&catalog, &base.loaded.config, &entries, &overrides);
        self.assemble(directory, base, providers)
    }

    // Pipeline order of the original: the `config` hook already ran, then catalog, `provider.models`, auth loaders.
    async fn context_async(&self, directory: &Path) -> Result<Arc<Ctx>, Error> {
        let base = self.base(directory)?;
        let (catalog, generation) = self.catalog().await;
        let entries = auth::read_all(&self.paths.auth_file());
        let key = registry::key(generation, base.plugins.as_ref(), &base.loaded.config, &entries);
        let overrides = self.instance_overrides(directory, base.plugins.as_ref(), &catalog, &base.loaded.config, &entries, key).await;
        let providers = provider::build_with(&catalog, &base.loaded.config, &entries, &overrides);
        self.assemble(directory, base, providers)
    }

    fn assemble(&self, directory: &Path, base: Base, providers: Vec<Provider>) -> Result<Arc<Ctx>, Error> {
        let cfg = |e: &dyn std::fmt::Display| Error::Config(e.to_string());
        let Base { worktree, opts, loaded, plugins } = base;
        let mut info = loaded.info().map_err(|e| cfg(&e))?;
        let flags = flag::Flags::from_env().map_err(|e| cfg(&e))?;
        if flags.disable_autocompact {
            info.compaction.auto = Some(false);
        }
        if flags.disable_prune {
            info.compaction.prune = Some(false);
        }
        let root = worktree.clone().unwrap_or_else(|| PathBuf::from("/"));
        let skills = skill::Registry::load(&skill::discover(&skill::DiscoverInput {
            home: &self.paths.home,
            cwd: directory,
            worktree: worktree.as_deref(),
            config_dirs: &config::config_dirs(&self.paths, &opts),
            config: &info.skills,
            disable_external: flags.disable_external_skills,
            disable_claude_code: flags.disable_claude_code_skills,
        }));
        let agents = Agents::build(&info, &agent::Context { paths: &self.paths, worktree: &root, skill_dirs: &skills.dirs, reference_dirs: &[] });
        let todos: Arc<dyn opencode_tools::todo::TodoStore> = Arc::new(tools::StoreTodos { store: self.store.clone() });
        let bash = opencode_tools::bash::BashTool::new(opencode_tools::bash::BashConfig { shell: info.shell.clone(), ..Default::default() });
        let shell = bash.shell().to_string();
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(opencode_tools::invalid::InvalidTool),
            Arc::new(bash),
            Arc::new(opencode_tools::read::ReadTool),
            Arc::new(opencode_tools::glob::GlobTool::default()),
            Arc::new(opencode_tools::grep::GrepTool::default()),
            Arc::new(opencode_tools::edit::EditTool),
            Arc::new(opencode_tools::write::WriteTool),
            Arc::new(opencode_tools::webfetch::WebFetchTool),
            Arc::new(opencode_tools::todo::TodoWriteTool::new(todos)),
            Arc::new(opencode_tools::apply_patch::ApplyPatchTool),
        ];
        if flags.experimental_lsp_tool {
            tools.push(Arc::new(opencode_tools::lsp::LspTool));
        }
        let builtins = tools.len();
        // Custom tool files and plugin tools come after the built-ins; a later id replaces an earlier one.
        if let Some(plugins) = &plugins {
            tools.extend(plugins.tools());
        }
        let replaced: std::collections::HashSet<String> = tools[builtins..].iter().map(|t| t.id().to_string()).collect();
        let names = profile::Names::new(!info.original_tools(), &shell, &replaced);
        let workspace = self.workspace_for(directory, worktree.as_deref(), &loaded.config);
        Ok(Arc::new(Ctx {
            instruction: instruction::Context::new(&self.paths, &opts, &flags),
            cwd: directory.to_path_buf(),
            worktree,
            root,
            info,
            flags,
            agents,
            providers,
            skills,
            tools,
            names,
            paths_data: self.paths.data.clone(),
            workspace,
            plugins,
        }))
    }

    // `Provider.defaultModel`
    fn default_model_in(&self, ctx: &Ctx) -> Option<(String, String)> {
        if let Some(m) = ctx.info.model_ref() {
            return Some((m.provider_id, m.model_id));
        }
        let recent = std::fs::read_to_string(self.paths.state.join("model.json")).ok().and_then(|text| {
            let state: Value = serde_json::from_str(&text).ok()?;
            state.get("recent")?.as_array()?.iter().find_map(|entry| {
                let provider_id = entry.get("providerID")?.as_str()?;
                let model_id = entry.get("modelID")?.as_str()?;
                ctx.model(provider_id, model_id).ok()?;
                Some((provider_id.to_string(), model_id.to_string()))
            })
        });
        recent.or_else(|| {
            let provider = ctx.providers.iter().find(|p| ctx.info.provider.is_empty() || ctx.info.provider.contains_key(&p.id))?;
            best_model(&provider.models).map(|m| (provider.id.clone(), m.id.clone()))
        })
    }

    // Session row -> newest user message -> configured default.
    fn current_model(&self, ctx: &Ctx, session: &Session) -> Result<(String, String), Error> {
        if let Some(model) = &session.model {
            return Ok((model.provider_id.clone(), model.id.clone()));
        }
        let rows = self.store.list_messages(&session.id)?;
        if let Some(user) = rows.iter().rev().find_map(|(m, _)| m.as_user()) {
            return Ok((user.model.provider_id.clone(), user.model.model_id.clone()));
        }
        self.default_model_in(ctx).ok_or_else(|| Error::Config("no model configured; pass --model provider/model".into()))
    }

    fn open_session(&self, ctx: &Ctx, session_id: Option<&str>) -> Result<Session, Error> {
        if let Some(id) = session_id {
            return self.store.get_session(id)?.ok_or_else(|| Error::Config(format!("Session not found: {id}")));
        }
        self.create_session(&ctx.cwd, CreateSession::default())
    }

    pub async fn prompt(&self, input: PromptInput, host: Arc<dyn Host>, abort: CancellationToken) -> Result<Session, Error> {
        // Rust build: a queued prompt waits until the session is idle, behind earlier queued ones.
        let _queued = match (&input.session_id, input.delivery) {
            (Some(session_id), Delivery::Queue) => {
                let guard = self.queue_lock(session_id).lock_owned().await;
                tokio::select! {
                    _ = self.idle(session_id) => {}
                    _ = abort.cancelled() => return Err(Error::Aborted),
                }
                Some(guard)
            }
            _ => None,
        };
        let ctx = self.instance(&plain_directory(input.directory), Some(&host)).await?;
        let model = input.model.as_deref().map(parse_model).transpose()?;
        if let Some(name) = &input.agent {
            ctx.agent(name)?;
        }
        // A new session is only created once its first message is known to be runnable.
        if input.session_id.is_none() {
            let agent = match &input.agent {
                Some(name) => ctx.agent(name)?,
                None => ctx.default_agent()?,
            };
            let chosen = model.clone().or_else(|| agent.model.as_ref().map(|m| (m.provider_id.clone(), m.model_id.clone())));
            let (provider_id, model_id) = match chosen.or_else(|| self.default_model_in(&ctx)) {
                Some(model) => model,
                None => return Err(Error::Config("no model configured; pass --model provider/model".into())),
            };
            ctx.model(&provider_id, &model_id)?;
        }
        let session = self.open_session(&ctx, input.session_id.as_deref())?;
        let mut parts = vec![PartInput::Text { text: input.text, synthetic: false }];
        parts.extend(input.files.into_iter().map(PartInput::File));
        let submission = Submission {
            agent: input.agent,
            model,
            variant: input.variant,
            system: input.system,
            tools: input.tools,
            parts,
            no_reply: input.no_reply,
            message_id: input.message_id,
        };
        self.submit(ctx, session, submission, host, abort).await
    }

    // Persists the user message, then runs (or joins) the session loop.
    async fn submit(
        &self,
        ctx: Arc<Ctx>,
        session: Session,
        submission: Submission,
        host: Arc<dyn Host>,
        abort: CancellationToken,
    ) -> Result<Session, Error> {
        self.revert_cleanup(&session)?;
        let no_reply = submission.no_reply;
        let tools = submission.tools.clone();
        self.create_user_message(&ctx, &session, submission).await?;
        let rules: Vec<PermissionRule> = tools
            .into_iter()
            .flatten()
            .map(|(tool, enabled)| PermissionRule {
                permission: tool,
                pattern: "*".into(),
                action: if enabled { PermissionAction::Allow } else { PermissionAction::Deny },
            })
            .collect();
        self.store.patch_session(&session.id, |s| {
            s.time.updated = id::now_ms();
            if !rules.is_empty() {
                s.permission = Some(rules);
            }
        })?;
        if no_reply {
            return self.session(&session.id);
        }
        self.run(ctx, &session.id, host, abort).await
    }

    fn session(&self, session_id: &str) -> Result<Session, Error> {
        self.store.get_session(session_id)?.ok_or_else(|| Error::Config(format!("Session not found: {session_id}")))
    }

    async fn create_user_message(&self, ctx: &Ctx, session: &Session, submission: Submission) -> Result<UserMessage, Error> {
        // What the caller asked for, as the `chat.message` hook sees it.
        let mut requested = JsonMap::new();
        requested.insert("sessionID".into(), session.id.clone().into());
        if let Some(agent) = &submission.agent {
            requested.insert("agent".into(), agent.clone().into());
        }
        if let Some((provider_id, model_id)) = &submission.model {
            requested.insert("model".into(), json!({"providerID": provider_id, "modelID": model_id}));
        }
        if let Some(message_id) = &submission.message_id {
            requested.insert("messageID".into(), message_id.clone().into());
        }
        if let Some(variant) = &submission.variant {
            requested.insert("variant".into(), variant.clone().into());
        }
        let agent = match &submission.agent {
            Some(name) => ctx.agent(name)?,
            None => ctx.default_agent()?,
        };
        let (provider_id, model_id) = match submission.model {
            Some(model) => model,
            None => match &agent.model {
                Some(m) => (m.provider_id.clone(), m.model_id.clone()),
                None => self.current_model(ctx, session)?,
            },
        };
        ctx.model(&provider_id, &model_id)?;
        let user = UserMessage {
            id: id::generate_or(id::Prefix::Message, submission.message_id).map_err(|e| Error::Config(e.to_string()))?,
            session_id: session.id.clone(),
            time: UserTime { created: id::now_ms() },
            format: None,
            summary: None,
            agent: agent.name.clone(),
            model: UserModel { provider_id: provider_id.clone(), model_id: model_id.clone(), variant: submission.variant.clone() },
            system: submission.system,
            tools: submission.tools,
        };
        let stored = session.model.as_ref();
        let stored_variant = stored.and_then(|m| m.variant.as_deref()).filter(|v| *v != "default");
        if session.agent.as_deref() != Some(&agent.name)
            || stored.map(|m| (m.provider_id.as_str(), m.id.as_str())) != Some((&provider_id, &model_id))
            || stored_variant != user.model.variant.as_deref()
        {
            let model = SessionModel {
                id: model_id,
                provider_id,
                variant: Some(user.model.variant.clone().unwrap_or_else(|| "default".into())),
            };
            self.store.patch_session(&session.id, |s| {
                s.agent = Some(agent.name.clone());
                s.model = Some(model);
            })?;
        }

        let mut bodies = Vec::new();
        for part in submission.parts {
            match part {
                PartInput::Text { text, synthetic } => bodies.push(PartBody::Text(TextPart {
                    text,
                    synthetic: synthetic.then_some(true),
                    ignored: None,
                    time: None,
                    metadata: None,
                })),
                PartInput::Subtask(subtask) => bodies.push(PartBody::Subtask(subtask)),
                PartInput::Agent(name) => {
                    let denied = permission::evaluate("task", &name, &[&agent.permission]).action == Action::Deny;
                    let hint = if denied { " . Invoked by user; guaranteed to exist." } else { "" };
                    bodies.push(PartBody::Agent(AgentPart { name: name.clone(), source: None }));
                    bodies.push(synthetic(format!(
                        " Use the above message and context to generate a prompt and call the task tool with subagent: {name}{hint}"
                    )));
                }
                PartInput::File(file) => bodies.extend(self.resolve_file(ctx, session, &user, file).await),
            }
        }
        let mut user = user;
        let mut parts: Vec<Part> = bodies.into_iter().map(|body| Part::new(&session.id, &user.id, body)).collect();
        let name = HookName::ChatMessage;
        if ctx.plugins.as_ref().is_some_and(|p| p.has(name)) {
            let output = json!({"message": Message::User(user.clone()), "parts": parts});
            let mut changed = self.hook(ctx, name, Value::Object(requested), output).await.map_err(Error::Config)?;
            let invalid = |e: serde_json::Error| Error::Config(format!("plugin hook {name} returned an invalid message: {e}"));
            let take = |value: &mut Value, key: &str| value.get_mut(key).map(Value::take).unwrap_or_default();
            match serde_json::from_value::<Message>(take(&mut changed, "message")).map_err(invalid)? {
                Message::User(message) => user = message,
                Message::Assistant(_) => return Err(Error::Config(format!("plugin hook {name} returned an invalid message: not a user message"))),
            }
            parts = serde_json::from_value(take(&mut changed, "parts")).map_err(invalid)?;
        }
        self.store.upsert_message(&Message::User(user.clone()))?;
        for part in &parts {
            self.store.upsert_part(part)?;
        }
        self.claims.clear(&user.id);
        Ok(user)
    }

    // A file attachment becomes what the model would have seen had it read the file itself.
    async fn resolve_file(&self, ctx: &Ctx, session: &Session, user: &UserMessage, file: FilePart) -> Vec<PartBody> {
        if let Some(FilePartSource::Resource { client_name, uri, .. }) = &file.source {
            let manager = self.mcp_for(ctx).await;
            let pieces = mcp::attached(Some(&manager), &file, client_name, uri).await;
            return pieces
                .into_iter()
                .map(|piece| match piece {
                    mcp::Piece::Text(text) => synthetic(text),
                    mcp::Piece::File(file) => PartBody::File(file),
                })
                .collect();
        }
        // The "agent" profile shows these as its Read: `file_path` and numbered lines.
        let agent = ctx.names.adapted("read");
        let key = if agent { "file_path" } else { "filePath" };
        if file.url.starts_with("data:") && file.mime == "text/plain" {
            let call = if agent { json!({"file_path": file.filename}) } else { json!({"filePath": file.filename}) };
            return vec![
                synthetic(format!("Called the Read tool with the following input: {call}")),
                synthetic(decode_data_url(&file.url)),
                PartBody::File(file),
            ];
        }
        let Some((path, query)) = file_url_path(&file.url) else { return vec![PartBody::File(file)] };
        let text = path.to_string_lossy().into_owned();
        let mime = if path.is_dir() { "application/x-directory".to_string() } else { file.mime.clone() };
        let read = |args: Value| async {
            let tool = opencode_tools::read::ReadTool;
            let tctx = ToolContext {
                session_id: session.id.clone(),
                message_id: user.id.clone(),
                call_id: String::new(),
                agent: user.agent.clone(),
                cwd: ctx.cwd.clone(),
                worktree: ctx.root.clone(),
                data_dir: self.paths.data.clone(),
                abort: CancellationToken::new(),
                permissions: Arc::new(AllowAll),
                hooks: Some(self.tool_hooks(ctx, None)),
            };
            tool.execute(args, &tctx).await
        };
        let failed = |error: ToolError| synthetic(format!("Read tool failed to read {text} with the following error: {error}"));
        if mime == "text/plain" {
            let mut args = json!({"filePath": text});
            if let Some(start) = query_number(query, "start") {
                let offset = start.max(1);
                args["offset"] = offset.into();
                if let Some(end) = query_number(query, "end").filter(|end| *end > 0) {
                    args["limit"] = end.saturating_sub(offset - 1).into();
                }
            }
            let shown = if agent { profile::to_exposed("read", args.clone()) } else { args.clone() };
            let mut out = vec![synthetic(format!("Called the Read tool with the following input: {shown}"))];
            match read(args).await {
                Ok(result) => {
                    let text = if agent { opencode_tools::read::numbered(&result.output).unwrap_or(result.output) } else { result.output };
                    out.push(synthetic(text));
                    if result.attachments.is_empty() {
                        out.push(PartBody::File(FilePart { mime, ..file }));
                    } else {
                        out.extend(result.attachments.into_iter().map(|a| {
                            PartBody::File(FilePart { mime: a.mime, url: a.url, filename: file.filename.clone(), source: None })
                        }));
                    }
                }
                Err(error) => out.push(failed(error)),
            }
            return out;
        }
        if mime == "application/x-directory" {
            let args = json!({"filePath": text});
            let shown = if agent { profile::to_exposed("read", args.clone()) } else { args.clone() };
            return match read(args).await {
                Ok(result) => vec![
                    synthetic(format!("Called the Read tool with the following input: {shown}")),
                    synthetic(result.output),
                    PartBody::File(FilePart { mime, ..file }),
                ],
                Err(error) => vec![failed(error)],
            };
        }
        let bytes = std::fs::read(&path).unwrap_or_default();
        vec![
            synthetic(format!("Called the Read tool with the following input: {{\"{key}\":\"{text}\"}}")),
            PartBody::File(FilePart { url: format!("data:{mime};base64,{}", base64(&bytes)), mime, ..file }),
        ]
    }

    // `resolvePromptParts`: the template text plus one part per `@reference`.
    fn resolve_prompt_parts(&self, ctx: &Ctx, template: &str) -> Vec<PartInput> {
        let mut parts = vec![PartInput::Text { text: template.to_string(), synthetic: false }];
        let mut seen: Vec<String> = Vec::new();
        for name in command::file_references(template) {
            if name.is_empty() || seen.contains(&name) {
                continue;
            }
            seen.push(name.clone());
            let path = match name.strip_prefix("~/") {
                Some(rest) => self.paths.home.join(rest),
                None => opencode_tools::util::resolve(&ctx.root, &name),
            };
            match std::fs::metadata(&path) {
                Ok(meta) => parts.push(PartInput::File(FilePart {
                    mime: if meta.is_dir() { "application/x-directory" } else { "text/plain" }.to_string(),
                    filename: Some(name),
                    url: path_to_file_url(&path),
                    source: None,
                })),
                Err(_) => {
                    if let Some(agent) = ctx.agents.get(&name) {
                        parts.push(PartInput::Agent(agent.name.clone()));
                    }
                }
            }
        }
        parts
    }

    // Runs a slash command: template expansion, shell placeholders, `@file` references, then a prompt.
    pub async fn command(&self, input: CommandInput, host: Arc<dyn Host>, abort: CancellationToken) -> Result<Session, Error> {
        let ctx = self.instance(&plain_directory(input.directory.clone()), Some(&host)).await?;
        let manager = self.mcp_for(&ctx).await;
        let prompts = mcp::prompt_commands(&manager).await;
        let commands = ctx.commands(prompts.iter().map(|prompt| prompt.command.clone()).collect());
        if input.command == "goal" && commands.get("goal").is_some_and(|cmd| *cmd == goal::command()) {
            return self.goal_command(ctx, input, host, abort).await;
        }
        let Some(cmd) = commands.get(&input.command) else {
            let available: Vec<&str> = commands.keys().map(String::as_str).collect();
            let hint = if available.is_empty() { String::new() } else { format!(" Available commands: {}", available.join(", ")) };
            return Err(Error::Config(format!("Command not found: \"{}\".{hint}", input.command)));
        };
        let session = self.open_session(&ctx, input.session_id.as_deref())?;
        let agent_name = cmd.agent.clone().or_else(|| input.agent.clone());

        let fetched = match prompts.iter().find(|prompt| cmd.source == Some(command::Source::Mcp) && prompt.command.name == cmd.name) {
            Some(prompt) => Some(mcp::template(&manager, prompt).await),
            None => None,
        };
        let expanded = command::expand(fetched.as_deref().unwrap_or(&cmd.template), &input.arguments);
        let shell = opencode_tools::shell::acceptable(ctx.info.shell.as_deref());
        let outputs = futures_util::future::join_all(
            expanded.shell.iter().map(|item| opencode_tools::bash::shell_stdout(&shell, &item.command, &ctx.cwd)),
        )
        .await;
        let template = expanded.finish(&outputs);

        let input_model = input.model.as_deref().map(parse_model).transpose()?;
        let task_model = match (&cmd.model, cmd.agent.as_deref().and_then(|name| ctx.agents.get(name)).and_then(|a| a.model.as_ref())) {
            (Some(model), _) => parse_model(model)?,
            (None, Some(model)) => (model.provider_id.clone(), model.model_id.clone()),
            (None, None) => match &input_model {
                Some(model) => model.clone(),
                None => self.current_model(&ctx, &session)?,
            },
        };
        ctx.model(&task_model.0, &task_model.1)?;
        let agent = match &agent_name {
            Some(name) => ctx.agent(name)?,
            None => ctx.default_agent()?,
        };

        let template_parts = self.resolve_prompt_parts(&ctx, &template);
        let subtask = (agent.mode == AgentMode::Subagent && cmd.subtask != Some(false)) || cmd.subtask == Some(true);
        let mut submission = if subtask {
            let prompt = template_parts
                .iter()
                .find_map(|p| match p {
                    PartInput::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            let user_agent = match &input.agent {
                Some(name) => name.clone(),
                None => ctx.default_agent()?.name.clone(),
            };
            let user_model = match input_model {
                Some(model) => model,
                None => self.current_model(&ctx, &session)?,
            };
            Submission {
                agent: Some(user_agent),
                model: Some(user_model),
                parts: vec![PartInput::Subtask(SubtaskPart {
                    prompt,
                    description: cmd.description.clone().unwrap_or_default(),
                    agent: agent.name.clone(),
                    model: Some(SubtaskModel { provider_id: task_model.0, model_id: task_model.1 }),
                    command: Some(input.command.clone()),
                })],
                variant: input.variant,
                message_id: input.message_id,
                ..Default::default()
            }
        } else {
            let attached: Vec<PathBuf> = input.files.iter().filter_map(|f| file_url_path(&f.url).map(|(path, _)| path)).collect();
            let mut parts: Vec<PartInput> = template_parts
                .into_iter()
                .filter(|part| match part {
                    PartInput::File(file) => !file_url_path(&file.url).is_some_and(|(path, _)| attached.contains(&path)),
                    _ => true,
                })
                .collect();
            parts.extend(input.files.into_iter().map(PartInput::File));
            Submission {
                agent: Some(agent.name.clone()),
                model: Some(task_model),
                parts,
                variant: input.variant,
                message_id: input.message_id,
                ..Default::default()
            }
        };
        let name = HookName::CommandExecuteBefore;
        if ctx.plugins.as_ref().is_some_and(|p| p.has(name)) {
            let before = json!({"command": input.command, "sessionID": session.id, "arguments": input.arguments});
            let parts: Vec<Value> = submission.parts.iter().map(part_input_json).collect();
            let mut changed = self.hook(&ctx, name, before, json!({"parts": parts})).await.map_err(Error::Config)?;
            let list = match changed.get_mut("parts").map(Value::take) {
                Some(Value::Array(list)) => list,
                _ => Vec::new(),
            };
            submission.parts = list.into_iter().filter_map(part_input_from).collect();
        }
        self.submit(ctx, session, submission, host, abort).await
    }

    // Manual compaction (`POST /session/:id/summarize`): queue a compaction and run the loop.
    pub async fn summarize(&self, session_id: &str, auto: bool, host: Arc<dyn Host>, abort: CancellationToken) -> Result<Session, Error> {
        let session = self.session(session_id)?;
        let ctx = self.instance(&plain_directory(PathBuf::from(&session.directory)), Some(&host)).await?;
        self.revert_cleanup(&session)?;
        let rows = self.store.list_messages(session_id)?;
        let last = rows.iter().rev().find_map(|(m, _)| m.as_user());
        let agent = match last {
            Some(user) => user.agent.clone(),
            None => ctx.default_agent()?.name.clone(),
        };
        let (provider_id, model_id) = match last {
            Some(user) => (user.model.provider_id.clone(), user.model.model_id.clone()),
            None => self.current_model(&ctx, &session)?,
        };
        self.create_compaction(session_id, &agent, UserModel { provider_id, model_id, variant: None }, auto, None)?;
        self.run(ctx, session_id, host, abort).await
    }

    fn create_compaction(&self, session_id: &str, agent: &str, model: UserModel, auto: bool, overflow: Option<bool>) -> Result<(), Error> {
        let user = UserMessage {
            id: id::message(),
            session_id: session_id.to_string(),
            time: UserTime { created: id::now_ms() },
            format: None,
            summary: None,
            agent: agent.to_string(),
            model,
            system: None,
            tools: None,
        };
        self.store.upsert_message(&Message::User(user.clone()))?;
        self.store.upsert_part(&Part::new(session_id, &user.id, PartBody::Compaction(CompactionPart { auto, overflow, tail_start_id: None })))?;
        Ok(())
    }

    // Cancels the running loop of a session (and its subagents). False when it was idle.
    pub fn abort(&self, session_id: &str) -> bool {
        let running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        running.get(session_id).map(|run| run.cancel.cancel()).is_some()
    }

    pub fn status(&self, session_id: &str) -> Status {
        let running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        running.get(session_id).map_or(Status::Idle, |run| run.status.clone())
    }

    // Sessions with a running loop.
    pub fn busy(&self) -> Vec<String> {
        self.running.lock().unwrap_or_else(|e| e.into_inner()).keys().cloned().collect()
    }

    fn assert_idle(&self, session_id: &str) -> Result<(), Error> {
        match self.status(session_id) {
            Status::Idle => Ok(()),
            _ => Err(Error::Busy(session_id.to_string())),
        }
    }

    // Waits for background work (title generation) started by earlier prompts.
    pub async fn settle(&self) {
        loop {
            let handles: Vec<_> = std::mem::take(&mut *self.background.lock().unwrap_or_else(|e| e.into_inner()));
            if handles.is_empty() {
                return;
            }
            for handle in handles {
                let _ = handle.await;
            }
        }
    }

    // Replaces the session's permission overrides; they are evaluated after the agent's rules.
    pub fn set_permission(&self, session_id: &str, rules: Option<Vec<PermissionRule>>) -> Result<Session, Error> {
        Ok(self.store.patch_session(session_id, |s| s.permission = rules)?)
    }

    // The rules granted by "always" replies, in grant order.
    pub fn approvals(&self) -> Vec<Rule> {
        self.approvals.rules()
    }

    // Takes back an "always" approval (a saved permission removed through the API); true when one was granted.
    pub fn revoke_approval(&self, permission: &str, pattern: &str) -> bool {
        self.approvals.revoke(permission, pattern)
    }

    pub fn todos(&self, session_id: &str) -> Result<Vec<Todo>, Error> {
        Ok(self.store.get_todos(session_id)?)
    }

    // Reverts everything from a message (or part) on: files are restored now, messages are removed by the next prompt.
    // Blocks on git; `revert_files` is the async form.
    pub fn revert(&self, session_id: &str, message_id: &str, part_id: Option<&str>) -> Result<Session, Error> {
        workspace::block_on(self.revert_files(session_id, message_id, part_id))?
    }

    // Blocks on git; `unrevert_files` is the async form.
    pub fn unrevert(&self, session_id: &str) -> Result<Session, Error> {
        workspace::block_on(self.unrevert_files(session_id))?
    }

    // `SessionRevert.cleanup`: deletes what a pending revert hides and clears the revert; the loop runs it before new
    // input is added.
    pub fn revert_cleanup(&self, session: &Session) -> Result<(), Error> {
        let Some(revert) = &session.revert else { return Ok(()) };
        let rows = self.store.list_messages(&session.id)?;
        if let Some(index) = rows.iter().position(|(m, _)| m.id() == revert.message_id) {
            let skip = usize::from(revert.part_id.is_some());
            for (message, _) in &rows[index + skip..] {
                self.store.delete_message(message.id())?;
            }
            if let Some(part_id) = &revert.part_id
                && let Some(at) = rows[index].1.iter().position(|p| &p.id == part_id)
            {
                for part in &rows[index].1[at..] {
                    self.store.delete_part(&part.id)?;
                }
            }
        }
        self.store.patch_session(&session.id, |s| s.revert = None)?;
        Ok(())
    }

    // Copies a session's history (up to, not including, `message_id`) into a new session.
    pub fn fork(&self, session_id: &str, message_id: Option<&str>) -> Result<Session, Error> {
        let original = self.session(session_id)?;
        let session = Session::new(NewSession {
            project_id: original.project_id.clone(),
            directory: original.directory.clone(),
            version: VERSION.to_string(),
            path: original.path.clone(),
            workspace_id: original.workspace_id.clone(),
            title: Some(forked_title(&original.title)),
            metadata: original.metadata.clone(),
            ..Default::default()
        })
        .map_err(|e| Error::Config(e.to_string()))?;
        self.store.create_session(&session)?;
        let rows = self.store.list_messages(session_id)?;
        let end = message_id.and_then(|id| rows.iter().position(|(m, _)| m.id() == id)).unwrap_or(rows.len());
        let mut ids: HashMap<String, String> = HashMap::new();
        for (message, parts) in &rows[..end] {
            let new_id = id::message();
            ids.insert(message.id().to_string(), new_id.clone());
            let cloned = match message.clone() {
                Message::User(mut user) => {
                    user.id = new_id.clone();
                    user.session_id = session.id.clone();
                    Message::User(user)
                }
                Message::Assistant(mut assistant) => {
                    assistant.id = new_id.clone();
                    assistant.session_id = session.id.clone();
                    if let Some(parent) = ids.get(&assistant.parent_id) {
                        assistant.parent_id = parent.clone();
                    }
                    Message::Assistant(assistant)
                }
            };
            self.store.upsert_message(&cloned)?;
            for part in parts {
                let mut body = part.body.clone();
                if let PartBody::Compaction(c) = &mut body {
                    c.tail_start_id = c.tail_start_id.as_ref().and_then(|id| ids.get(id).cloned());
                }
                self.store.upsert_part(&Part::new(&session.id, &new_id, body))?;
            }
        }
        self.session(&session.id)
    }

    fn rows(&self, session_id: &str) -> Result<Vec<Row>, Error> {
        Ok(history::filter_compacted(self.store.list_messages(session_id)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fork_titles_count_up() {
        assert_eq!(forked_title("Fix bug"), "Fix bug (fork #1)");
        assert_eq!(forked_title("Fix bug (fork #9)"), "Fix bug (fork #10)");
        assert_eq!(forked_title("Fix (fork #x)"), "Fix (fork #x) (fork #1)");
    }

    #[test]
    fn file_urls_round_trip() {
        let path = if cfg!(windows) { PathBuf::from(r"C:\Users\me\a b#1.txt") } else { PathBuf::from("/home/me/a b#1.txt") };
        let url = path_to_file_url(&path);
        assert!(url.ends_with("/a%20b%231.txt") && url.starts_with("file:///"), "{url}");
        assert_eq!(file_url_path(&format!("{url}?start=3&end=9")), Some((path, "start=3&end=9")));
        assert_eq!((query_number("start=3&end=9", "end"), query_number("start=3", "end")), (Some(9), None));
        assert_eq!(decode_data_url("data:text/plain;base64,aGk="), "hi");
        assert_eq!(decode_data_url("data:text/plain,a%20b"), "a b");
        assert!(file_url_path("https://example.com/x").is_none());
    }
}
