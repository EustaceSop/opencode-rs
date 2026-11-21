// Bridge to the in-process engine: every call returns at once and answers with `Event`s from tokio threads.
use super::attention::{Found, Kind, Signal};
use super::models::{self, Candidate};
use super::prompt::Attachment;
use super::question::QuestionView;
use super::state::{
    AgentItem, Catalog, ChildUpdate, CommandItem, ConnectedProvider, Event, LoginStep, ModelGroup, ModelItem, NEW_SESSION_TITLE, Outcome,
    PermissionView, ProviderEntry, ProviderSource, RunStatus, SessionItem, Usage,
};
use super::transcript::{Row, before_revert, first_user_text, rows_from_store, tool_view};
use opencode_core::agent::{self, Agents};
use opencode_core::command::Source;
use opencode_core::config::{self, AgentMode, LoadOptions};
use opencode_core::paths::Paths;
use opencode_core::{auth, flag};
use opencode_llm::provider;
use opencode_session::{CommandInput, Engine, Host, PromptInput, QuestionRequest, Reply, Status, Update};
use opencode_storage::{
    AssistantMessage, AssistantTime, FilePart, GLOBAL_PROJECT_ID, JsonMap, Message, MessagePath, NewSession, Part, PartBody, PartTime, Project,
    ProjectTime, ReasoningPart, Session, Store, TextPart, Todo, TokenCache, Tokens, ToolPart, ToolState, UserMessage, UserModel, UserTime, Vcs, id,
    identity,
};
use opencode_tools::PermissionRequest;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

// Sessions still carrying the generated title get one derived from their first prompt, up to this many.
const DERIVED_TITLES: usize = 40;

const DEMO_PATCH: &str = "Index: src/greet.rs\n===================================================================\n--- src/greet.rs\n+++ src/greet.rs\n@@ -1,3 +1,3 @@\n-pub fn greet() -> String {\n-    \"Hello, world!\".to_string()\n+pub fn greet(name: &str) -> String {\n+    format!(\"Hello, {name}!\")\n }\n";
const DEMO_REPLY: &str = "I updated **`greet`** to take a name:\n\n```rust\npub fn greet(name: &str) -> String {\n    format!(\"Hello, {name}!\")\n}\n```\n\n- callers now pass the name\n- the test run below still fails";

// Project files offered by the `@` popup, at most this many.
const FILE_LIMIT: usize = 20_000;

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Reply>>>>;
type Answers = Option<Vec<Vec<String>>>;
type PendingQuestions = Arc<Mutex<HashMap<u64, oneshot::Sender<Answers>>>>;

#[derive(Debug, Clone, Default, PartialEq)]
pub enum Work {
    // `text` is the prompt.
    #[default]
    Prompt,
    // A slash command; `text` holds its arguments.
    Command(String),
    // Summarise the session; `text` is unused.
    Compact,
}

#[derive(Debug, Clone, Default)]
pub struct PromptRequest {
    pub directory: PathBuf,
    pub session_id: String,
    // The session does not exist yet and is created under `session_id` first.
    pub create: bool,
    pub work: Work,
    pub text: String,
    pub files: Vec<Attachment>,
    pub model: Option<String>,
    pub agent: Option<String>,
    // Reasoning variant of `model`; None runs the model's default.
    pub variant: Option<String>,
}

// Login flows the Rust CLI implements (`opencode-cli` `providers.rs`), driven here without a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginFlow {
    // GitHub.com device flow (`plugin/github-copilot/copilot.ts`).
    Copilot,
    // ChatGPT Pro/Plus: PKCE with a local callback server, or the device code (`plugin/openai/codex.ts`).
    ChatgptBrowser,
    ChatgptHeadless,
}

impl LoginFlow {
    // The method labels `opencode providers login` lists; the original's plugins report them untranslated too.
    pub fn label(self) -> &'static str {
        match self {
            Self::Copilot => "Login with GitHub Copilot",
            Self::ChatgptBrowser => "ChatGPT Pro/Plus (browser)",
            Self::ChatgptHeadless => "ChatGPT Pro/Plus (headless)",
        }
    }

    // The flows the CLI offers for a provider before its API key (`providers.rs` `methods_of`).
    pub fn of(provider_id: &str) -> &'static [Self] {
        match provider_id {
            opencode_llm::copilot::PROVIDER_ID => &[Self::Copilot],
            opencode_llm::codex::PROVIDER_ID => &[Self::ChatgptBrowser, Self::ChatgptHeadless],
            _ => &[],
        }
    }
}

pub struct Backend {
    runtime: Option<tokio::runtime::Runtime>,
    engine: Arc<Engine>,
    events: UnboundedSender<Event>,
    pending: Pending,
    questions: PendingQuestions,
    runs: Arc<Mutex<HashMap<String, CancellationToken>>>,
    projects: Arc<Mutex<HashMap<PathBuf, String>>>,
    next_permission: Arc<AtomicU64>,
    // The provider login in progress, if any.
    login: Mutex<Option<CancellationToken>>,
}

impl Drop for Backend {
    fn drop(&mut self) {
        for token in self.runs.lock().unwrap().values() {
            token.cancel();
        }
        self.cancel_login();
        // Rust build: background shells started by this engine end with the app.
        opencode_tools::bash::background::kill_owned(&self.engine.paths.data);
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

// Paths compare equal across `\\?\` prefixes, separators and (on Windows) case.
fn same_directory(a: &str, b: &Path) -> bool {
    let normal = |text: &str| {
        let text = text.strip_prefix(r"\\?\").unwrap_or(text).replace('\\', "/");
        let text = text.trim_end_matches('/').to_string();
        if cfg!(windows) { text.to_lowercase() } else { text }
    };
    normal(a) == normal(&b.to_string_lossy())
}

fn project_id(projects: &Mutex<HashMap<PathBuf, String>>, directory: &Path) -> String {
    if let Some(id) = projects.lock().unwrap().get(directory) {
        return id.clone();
    }
    let id = identity::resolve(directory).id;
    projects.lock().unwrap().insert(directory.to_path_buf(), id.clone());
    id
}

fn list_sessions(store: &Store, directory: &Path, project: &str) -> Result<Vec<SessionItem>, String> {
    let sessions = store.list_sessions(project).map_err(|e| e.to_string())?;
    let mut derived = 0;
    let items = sessions
        .into_iter()
        .filter(|s| s.time.archived.is_none())
        .filter(|s| project != GLOBAL_PROJECT_ID || same_directory(&s.directory, directory))
        .map(|session| {
            let title = if !session.has_default_title() {
                session.title.clone()
            } else if derived < DERIVED_TITLES && !session.is_child() {
                derived += 1;
                store.list_messages(&session.id).ok().and_then(|m| first_user_text(&m)).unwrap_or_else(|| NEW_SESSION_TITLE.into())
            } else {
                NEW_SESSION_TITLE.into()
            };
            session_item(&session, title)
        })
        .collect();
    Ok(items)
}

fn session_item(session: &Session, title: String) -> SessionItem {
    SessionItem { id: session.id.as_str().into(), title: title.into(), updated: session.time.updated, parent: session.parent_id.as_deref().map(Arc::from) }
}

// Token and cost totals of the listed messages; the newest step that reported tokens is what the context window holds.
fn usage(messages: &[(Message, Vec<Part>)]) -> Usage {
    let count = |value: f64| value.max(0.0) as u64;
    let mut usage = Usage::default();
    for (message, _) in messages {
        let Message::Assistant(assistant) = message else { continue };
        let tokens = &assistant.tokens;
        usage.cost += assistant.cost;
        usage.input += count(tokens.input);
        usage.output += count(tokens.output);
        usage.reasoning += count(tokens.reasoning);
        usage.cache_read += count(tokens.cache.read);
        usage.cache_write += count(tokens.cache.write);
        let step = count(tokens.input + tokens.output + tokens.reasoning + tokens.cache.read + tokens.cache.write);
        if step > 0 {
            usage.context = step;
        }
    }
    usage
}

// Everything the transcript and its docks show for a stored session.
struct Detail {
    rows: Vec<Row>,
    model: Option<String>,
    agent: Option<String>,
    usage: Usage,
    hidden: usize,
    todos: Vec<Todo>,
}

fn detail(store: &Store, session_id: &str) -> Result<Detail, String> {
    let messages = store.list_messages(session_id).map_err(|e| e.to_string())?;
    let revert = store.get_session(session_id).ok().flatten().and_then(|s| s.revert).map(|r| r.message_id);
    let shown = before_revert(&messages, revert.as_deref());
    let last = shown.iter().rev().find_map(|(message, _)| message.as_user());
    Ok(Detail {
        rows: rows_from_store(shown),
        model: last.map(|u| format!("{}/{}", u.model.provider_id, u.model.model_id)),
        agent: last.map(|u| u.agent.clone()),
        usage: usage(shown),
        hidden: messages[shown.len()..].iter().filter(|(m, _)| m.as_user().is_some()).count(),
        todos: store.get_todos(session_id).unwrap_or_default(),
    })
}

// What the transcript shows changed in the store: rows, usage and what a revert hides.
fn send_stored(events: &UnboundedSender<Event>, store: &Store, session_id: &str) {
    if let Ok(detail) = detail(store, session_id) {
        let session_id = session_id.to_string();
        let _ = events.send(Event::Snapshot { session_id: session_id.clone(), rows: detail.rows });
        let _ = events.send(Event::Usage { session_id: session_id.clone(), usage: detail.usage });
        let _ = events.send(Event::Revert { session_id, hidden: detail.hidden });
    }
}

fn git_files(directory: &Path) -> Option<Vec<String>> {
    let mut command = std::process::Command::new("git");
    command.arg("-C").arg(directory).args(["ls-files", "--cached", "--others", "--exclude-standard"]);
    command.stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // CREATE_NO_WINDOW: a GUI process must not flash a console.
        command.creation_flags(0x0800_0000);
    }
    let output = command.output().ok().filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&output.stdout).lines().filter(|l| !l.is_empty()).take(FILE_LIMIT).map(str::to_string).collect())
}

fn walk_files(directory: &Path) -> Vec<String> {
    const SKIPPED: [&str; 6] = ["node_modules", "target", "dist", "build", "__pycache__", "vendor"];
    let mut files = Vec::new();
    let mut pending = vec![(directory.to_path_buf(), 0usize)];
    while let Some((folder, depth)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&folder) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(kind) = entry.file_type() else { continue };
            if name.starts_with('.') || kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                if depth < 12 && !SKIPPED.contains(&name.as_str()) {
                    pending.push((entry.path(), depth + 1));
                }
            } else if let Ok(relative) = entry.path().strip_prefix(directory) {
                files.push(relative.to_string_lossy().replace('\\', "/"));
                if files.len() >= FILE_LIMIT {
                    return files;
                }
            }
        }
    }
    files
}

pub fn list_files(directory: &Path) -> Vec<String> {
    let mut files = git_files(directory).unwrap_or_else(|| walk_files(directory));
    files.sort();
    files
}

fn recent_model(paths: &Paths, providers: &[provider::Provider]) -> Option<String> {
    let text = std::fs::read_to_string(paths.state.join("model.json")).ok()?;
    let state: Value = serde_json::from_str(&text).ok()?;
    state.get("recent")?.as_array()?.iter().find_map(|entry| {
        let provider_id = entry.get("providerID")?.as_str()?;
        let model_id = entry.get("modelID")?.as_str()?;
        let provider = providers.iter().find(|p| p.id == provider_id)?;
        provider.models.iter().find(|m| m.id == model_id)?;
        Some(format!("{provider_id}/{model_id}"))
    })
}

// Mirrors the engine's own resolution so the header shows what a prompt will actually use.
pub fn load_catalog(paths: &Paths, directory: &Path) -> Catalog {
    let worktree = config::find_worktree(directory);
    let opts = LoadOptions { cwd: directory.to_path_buf(), worktree: worktree.clone() };
    let loaded = match config::load_with(paths, &opts, &flag::process_env) {
        Ok(loaded) => loaded,
        Err(error) => return Catalog { warning: Some(error.to_string()), ..Default::default() },
    };
    let info = match loaded.info() {
        Ok(info) => info,
        Err(error) => return Catalog { warning: Some(error.to_string()), ..Default::default() },
    };
    let root = worktree.unwrap_or_else(|| PathBuf::from("/"));
    let agents = Agents::build(&info, &agent::Context { paths, worktree: &root, skill_dirs: &[], reference_dirs: &[] });
    let database = provider::load_catalog(paths);
    let stored = auth::read_all(&paths.auth_file());
    let providers = provider::build(&database, &loaded.config, &stored);

    let default = agents.default_info().ok();
    let exists = |spec: &String| provider::parse_model(spec).is_some_and(|(p, m)| providers.iter().any(|x| x.id == p && x.models.iter().any(|y| y.id == m)));
    let default_model = default
        .and_then(|a| a.model.clone())
        .or_else(|| info.model_ref())
        .map(|m| format!("{}/{}", m.provider_id, m.model_id))
        .filter(exists)
        .or_else(|| recent_model(paths, &providers));

    let now = id::now_ms() as f64;
    let mut models: Vec<ModelGroup> = providers
        .iter()
        .map(|p| {
            let listed: Vec<&provider::Model> = p.models.iter().filter(|m| m.status != "deprecated").collect();
            let free = |m: &provider::Model| p.id == "opencode" && m.cost.base.input == 0.0;
            let candidates: Vec<Candidate> = listed.iter().map(|m| Candidate { family: &m.family, release: &m.release_date, free: free(m) }).collect();
            let shown = models::default_visible(&candidates, now);
            let mut models: Vec<ModelItem> = listed
                .iter()
                .zip(shown)
                .map(|(m, shown)| ModelItem {
                    spec: format!("{}/{}", p.id, m.id).into(),
                    name: m.name.replace("(latest)", "").trim().into(),
                    context: m.limit_context,
                    provider_id: p.id.as_str().into(),
                    provider: p.name.as_str().into(),
                    free: free(m),
                    latest: m.name.contains("(latest)"),
                    shown,
                    variants: m.variants.keys().map(|v| Arc::from(v.as_str())).collect(),
                })
                .collect();
            models.sort_by(|a, b| a.name.cmp(&b.name));
            ModelGroup { provider: p.name.as_str().into(), id: p.id.as_str().into(), models }
        })
        .filter(|g| !g.models.is_empty())
        .collect();
    models.sort_by(|a, b| models::popular_rank(&a.id).cmp(&models::popular_rank(&b.id)).then_with(|| a.provider.cmp(&b.provider)));

    // `settings-providers.tsx`: OpenCode's free tier is always there and is not listed as connected.
    let own = auth::read_file(&paths.auth_file());
    let connected = providers
        .iter()
        .filter(|p| p.id != "opencode" || p.models.iter().any(|m| m.cost.base.input > 0.0))
        .map(|p| {
            let configured = info.provider.get(&p.id);
            let custom = configured.is_some_and(|c| c.npm.as_deref() == Some("@ai-sdk/openai-compatible") && !c.models.is_empty());
            let env = p.env.iter().any(|name| std::env::var(name).is_ok_and(|value| !value.is_empty()));
            // Assumption: a stored credential outranks the environment, which outranks the config.
            let source = if stored.iter().any(|(id, _)| *id == p.id) {
                ProviderSource::Api
            } else if env {
                ProviderSource::Env
            } else if custom {
                ProviderSource::Custom
            } else if configured.is_some() {
                ProviderSource::Config
            } else {
                ProviderSource::Other
            };
            let removable = own.iter().any(|(id, _)| *id == p.id);
            ConnectedProvider { id: p.id.as_str().into(), name: p.name.as_str().into(), source, removable }
        })
        .collect();
    // What `opencode providers login` offers: the catalog, minus providers the config turns off.
    let enabled = info.enabled_providers.as_ref();
    let catalog_providers = database
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(id, _)| enabled.is_none_or(|list| list.contains(id)) && !info.disabled_providers.contains(id))
        .map(|(id, entry)| {
            let name = entry.get("name").and_then(Value::as_str).filter(|name| !name.is_empty()).unwrap_or(id);
            ProviderEntry { id: id.as_str().into(), name: name.into() }
        })
        .collect();

    // An empty catalog is explained by the view; only load failures are reported here.
    let warning = None;
    Catalog {
        connected,
        providers: catalog_providers,
        models,
        agents: agents
            .list()
            .into_iter()
            .filter(|a| a.mode != AgentMode::Subagent && a.hidden != Some(true))
            .map(|a| AgentItem { name: a.name.as_str().into(), description: a.description.clone().unwrap_or_default().into() })
            .collect(),
        default_model,
        default_agent: default.map(|a| a.name.clone()),
        warning,
    }
}

// Same rows the engine's own session bootstrap writes, under an id the UI already shows.
fn create_session(store: &Store, request: &PromptRequest) -> Result<(), String> {
    let resolved = identity::resolve(&request.directory);
    let worktree = config::find_worktree(&request.directory);
    if store.get_project(&resolved.id).map_err(|e| e.to_string())?.is_none() {
        let now = id::now_ms();
        store
            .upsert_project(&Project {
                id: resolved.id.clone(),
                worktree: worktree.as_ref().map_or_else(|| "/".to_string(), |w| w.to_string_lossy().into_owned()),
                vcs: worktree.as_ref().map(|_| Vcs::Git),
                name: None,
                icon: None,
                commands: None,
                time: ProjectTime { created: now, updated: now, initialized: None },
                sandboxes: Vec::new(),
            })
            .map_err(|e| e.to_string())?;
    }
    let session = Session::new(NewSession {
        id: Some(request.session_id.clone()),
        project_id: resolved.id,
        directory: request.directory.to_string_lossy().into_owned(),
        version: opencode_session::VERSION.to_string(),
        agent: request.agent.clone(),
        ..Default::default()
    })
    .map_err(|e| e.to_string())?;
    store.create_session(&session).map_err(|e| e.to_string())
}

fn run_status(status: Status) -> RunStatus {
    match status {
        Status::Idle => RunStatus::Idle,
        Status::Busy => RunStatus::Busy,
        Status::Retry { attempt, message, next } => RunStatus::Retry { attempt, message, next },
    }
}

fn command_item(command: &opencode_core::command::Command) -> CommandItem {
    let badge = match command.source {
        Some(Source::Mcp) => "prompt.slash.badge.mcp",
        Some(Source::Skill) => "prompt.slash.badge.skill",
        Some(Source::Command) | None => "prompt.slash.badge.custom",
    };
    CommandItem { name: command.name.as_str().into(), description: command.description.clone().unwrap_or_default().into(), badge }
}

fn permission_view(id: u64, session_id: &str, request: &PermissionRequest) -> PermissionView {
    let text = |key: &str| request.metadata.get(key).and_then(Value::as_str).filter(|s| !s.trim().is_empty()).map(Arc::from);
    PermissionView {
        id,
        session_id: session_id.into(),
        permission: request.permission.as_str().into(),
        patterns: request.patterns.clone(),
        detail: text("command").or_else(|| text("filepath")).or_else(|| text("url")),
        diff: text("diff"),
    }
}

struct RunHost {
    session_id: String,
    directory: PathBuf,
    store: Store,
    events: UnboundedSender<Event>,
    pending: Pending,
    questions: PendingQuestions,
    next_permission: Arc<AtomicU64>,
    abort: CancellationToken,
    // Subagent tool calls already counted.
    child_tools: Mutex<HashSet<String>>,
}

impl RunHost {
    fn send(&self, event: Event) {
        let _ = self.events.send(event);
    }

    // The session is read back from the store the way the original's `lookup` asks the server for it.
    fn attention(&self, kind: Kind, session_id: &str) {
        let session = self.store.get_session(session_id).ok().flatten().map(|s| Found { title: s.title, child: s.parent_id.is_some() });
        self.send(Event::Attention(Signal { kind, directory: self.directory.clone(), session_id: Some(session_id.to_string()), session }));
    }

    // Everything a subagent session emits is condensed into the activity line of its `task` card.
    fn child(&self, child_id: String, update: Update) {
        let update = match update {
            Update::Text(delta) => ChildUpdate::Text(delta),
            Update::Tool(part) => {
                let PartBody::Tool(tool) = &part.body else { return };
                let view = tool_view(&tool.tool, &tool.state);
                let started = self.child_tools.lock().unwrap().insert(part.id.clone());
                let label = if view.title.is_empty() { tool.tool.clone() } else { format!("{} {}", tool.tool, view.title) };
                ChildUpdate::Tool { label, started }
            }
            Update::Status { status, .. } => ChildUpdate::Running(status != Status::Idle),
            Update::Title { title, .. } => ChildUpdate::Title(title),
            Update::Child { session_id, update } => return self.child(session_id, *update),
            _ => return,
        };
        self.send(Event::Child { child_id, update });
    }
}

#[async_trait::async_trait]
impl Host for RunHost {
    fn update(&self, update: Update) {
        let session_id = self.session_id.clone();
        let event = match update {
            Update::Text(delta) => Event::Text { session_id, delta },
            Update::Reasoning(delta) => Event::Reasoning { session_id, delta },
            Update::Tool(part) => Event::Tool { session_id, part },
            Update::Retry { attempt, message, delay_ms } => Event::Retry { session_id, attempt, message, delay_ms },
            Update::Title { session_id, title } => Event::Title { session_id, title },
            Update::Status { session_id, status } => {
                let idle = status == Status::Idle;
                self.send(Event::Status { session_id: session_id.clone(), status: run_status(status) });
                if idle {
                    self.attention(Kind::Idle, &session_id);
                }
                return;
            }
            Update::Todos { session_id, todos } => Event::Todos { session_id, todos },
            Update::Error { session_id, message } => Event::Error { session_id, message },
            // `SessionError` repeats `Error` for the transcript; it also stands for aborts, which the original announces.
            Update::SessionError { session_id, .. } => return self.attention(Kind::Error, &session_id),
            Update::Child { session_id, update } => return self.child(session_id, *update),
            // The store changed under the stream; a snapshot replaces what was streamed.
            Update::StepFinish { .. } | Update::Compacted { .. } => {
                let Ok(messages) = self.store.list_messages(&self.session_id) else { return };
                self.send(Event::Usage { session_id: session_id.clone(), usage: usage(&messages) });
                Event::Snapshot { session_id, rows: rows_from_store(&messages) }
            }
            // `Delta` repeats `Text`/`Reasoning`.
            _ => return,
        };
        self.send(event);
    }

    async fn permission(&self, request: &PermissionRequest) -> Reply {
        let id = self.next_permission.fetch_add(1, Ordering::Relaxed);
        let (reply, answer) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, reply);
        self.send(Event::Permission(permission_view(id, &self.session_id, request)));
        self.attention(Kind::Permission(id), &self.session_id);
        let reply = tokio::select! {
            answer = answer => answer.unwrap_or(Reply::Reject),
            _ = self.abort.cancelled() => Reply::Reject,
        };
        self.pending.lock().unwrap().remove(&id);
        self.send(Event::PermissionResolved(id));
        reply
    }

    // Shown under the session the user is looking at, whichever subagent asked.
    async fn question(&self, request: &QuestionRequest) -> Answers {
        let id = self.next_permission.fetch_add(1, Ordering::Relaxed);
        let (reply, answer) = oneshot::channel();
        self.questions.lock().unwrap().insert(id, reply);
        self.send(Event::Question(QuestionView { id, session_id: self.session_id.as_str().into(), questions: request.questions.clone() }));
        self.attention(Kind::Question(id), &self.session_id);
        let answers = tokio::select! {
            answer = answer => answer.unwrap_or(None),
            _ = self.abort.cancelled() => None,
        };
        self.questions.lock().unwrap().remove(&id);
        self.send(Event::QuestionResolved(id));
        answers
    }
}

// Injected stores mean tests or smoke runs: never fetch the models catalog from the network there.
pub(crate) fn offline(engine: Engine) -> Engine {
    let source = opencode_llm::catalog::Source { disabled: true, ..opencode_llm::catalog::Source::from_env(&engine.paths) };
    engine.set_catalog_source(source);
    engine
}

impl Backend {
    pub fn new(paths: Paths) -> Result<(Self, UnboundedReceiver<Event>), String> {
        Ok(Self::with_engine(Engine::new(paths).map_err(|e| e.to_string())?))
    }

    pub fn with_engine(engine: Engine) -> (Self, UnboundedReceiver<Event>) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("opencode-engine")
            .enable_all()
            .build()
            .expect("tokio runtime");
        let (events, receiver) = unbounded_channel();
        let backend = Self {
            runtime: Some(runtime),
            engine: {
                let engine = Arc::new(engine);
                opencode_server::attach_plugin_server(&engine);
                engine
            },
            events,
            pending: Pending::default(),
            questions: PendingQuestions::default(),
            runs: Arc::default(),
            projects: Arc::default(),
            next_permission: Arc::new(AtomicU64::new(1)),
            login: Mutex::default(),
        };
        (backend, receiver)
    }

    pub fn state_dir(&self) -> &Path {
        &self.engine.paths.state
    }

    // Rust build: Windows without any bash and no answer recorded yet (`opencode setup bash` asks the same).
    pub fn offer_bash(&self) -> bool {
        opencode_tools::mingit::should_offer(&self.engine.paths.state, &self.engine.paths.cache)
    }

    // Records the answer; an accept downloads the portable bash and reports where it went.
    pub fn setup_bash(&self, accept: bool) {
        use opencode_tools::mingit::{self, Consent};
        let paths = &self.engine.paths;
        let _ = mingit::set_consent(&paths.state, Some(if accept { Consent::Accepted } else { Consent::Declined }));
        if !accept {
            return;
        }
        let (root, events) = (mingit::root_in(&paths.cache), self.events.clone());
        self.runtime().spawn(async move {
            let event = match mingit::Installer::new(root).install(&|_, _| {}).await {
                Ok(outcome) => Event::Failed { what: "desktop.rs.setup.bash.ready", error: outcome.installed.shell.display().to_string() },
                Err(error) => Event::Failed { what: "desktop.rs.setup.bash.failed", error: error.to_string() },
            };
            let _ = events.send(event);
        });
    }

    fn runtime(&self) -> &tokio::runtime::Runtime {
        self.runtime.as_ref().expect("runtime lives as long as the backend")
    }

    // Reads models and providers again, after a login changed what is connected.
    pub fn reload_catalog(&self, directory: PathBuf) {
        let (engine, events) = (self.engine.clone(), self.events.clone());
        self.runtime().spawn_blocking(move || {
            let catalog = load_catalog(&engine.paths, &directory);
            let _ = events.send(Event::Catalog { directory, catalog });
        });
    }

    // An API key is stored in auth.json the way `opencode providers login` stores it.
    pub fn connect_key(&self, directory: PathBuf, provider_id: String, key: String) {
        let (engine, events) = (self.engine.clone(), self.events.clone());
        self.runtime().spawn_blocking(move || {
            let saved = auth::set(&engine.paths.auth_file(), &provider_id, auth::Info::Api { key, metadata: None });
            let step = saved.as_ref().map_or_else(|e| LoginStep::Failed(e.to_string()), |_| LoginStep::Done);
            let _ = events.send(Event::Login { provider_id, step });
            if saved.is_ok() {
                let catalog = load_catalog(&engine.paths, &directory);
                let _ = events.send(Event::Catalog { directory, catalog });
            }
        });
    }

    // `opencode providers logout`: removes the provider's entry from auth.json.
    pub fn disconnect(&self, directory: PathBuf, provider_id: String) {
        self.edit("desktop.rs.error.disconnect", move |engine, events| {
            auth::remove(&engine.paths.auth_file(), &provider_id).map_err(|e| e.to_string())?;
            let catalog = load_catalog(&engine.paths, &directory);
            let _ = events.send(Event::Catalog { directory, catalog });
            Ok(())
        });
    }

    // Runs one login flow of the CLI; progress arrives as `Event::Login`. A new login or `cancel_login` ends it.
    pub fn login(&self, directory: PathBuf, provider_id: String, flow: LoginFlow) {
        self.cancel_login();
        let token = CancellationToken::new();
        *self.login.lock().unwrap() = Some(token.clone());
        let (engine, events) = (self.engine.clone(), self.events.clone());
        self.runtime().spawn(async move {
            let send = |step: LoginStep| drop(events.send(Event::Login { provider_id: provider_id.clone(), step }));
            let auth_file = engine.paths.auth_file();
            let result = tokio::select! {
                result = run_login(flow, &auth_file, &send) => result,
                _ = token.cancelled() => return,
            };
            match result {
                Ok(()) => {
                    send(LoginStep::Done);
                    let catalog = load_catalog(&engine.paths, &directory);
                    let _ = events.send(Event::Catalog { directory, catalog });
                }
                Err(error) => send(LoginStep::Failed(error)),
            }
        });
    }

    pub fn cancel_login(&self) {
        if let Some(token) = self.login.lock().unwrap().take() {
            token.cancel();
        }
    }

    pub fn load_project(&self, directory: PathBuf) {
        self.refresh_sessions(directory.clone());
        self.reload_catalog(directory.clone());
        let events = self.events.clone();
        self.runtime().spawn_blocking({
            let (directory, events) = (directory.clone(), events.clone());
            move || {
                let files = list_files(&directory);
                let _ = events.send(Event::Files { directory, files });
            }
        });
        // Listing commands connects the project's MCP servers, so it gets a task of its own.
        let engine = self.engine.clone();
        self.runtime().spawn(async move {
            let commands = engine.commands(&directory).await.map(|list| list.iter().map(command_item).collect()).unwrap_or_default();
            let _ = events.send(Event::Commands { directory, commands });
        });
    }

    pub fn refresh_sessions(&self, directory: PathBuf) {
        let (engine, events, projects) = (self.engine.clone(), self.events.clone(), self.projects.clone());
        self.runtime().spawn_blocking(move || {
            let project = project_id(&projects, &directory);
            let event = match list_sessions(&engine.store, &directory, &project) {
                Ok(sessions) => Event::Sessions { directory, sessions },
                Err(error) => Event::Failed { what: "desktop.rs.error.listSessions", error },
            };
            let _ = events.send(event);
        });
    }

    pub fn open_session(&self, session_id: String) {
        let (engine, events) = (self.engine.clone(), self.events.clone());
        self.runtime().spawn_blocking(move || match detail(&engine.store, &session_id) {
            Ok(detail) => {
                let id = || session_id.clone();
                let _ = events.send(Event::Todos { session_id: id(), todos: detail.todos });
                let _ = events.send(Event::Usage { session_id: id(), usage: detail.usage });
                let _ = events.send(Event::Revert { session_id: id(), hidden: detail.hidden });
                let _ = events.send(Event::Opened { session_id, rows: detail.rows, model: detail.model, agent: detail.agent });
            }
            Err(error) => drop(events.send(Event::Failed { what: "desktop.rs.error.openSession", error })),
        });
    }

    fn host(&self, session_id: &str, directory: &Path, abort: CancellationToken) -> Arc<RunHost> {
        Arc::new(RunHost {
            session_id: session_id.to_string(),
            directory: directory.to_path_buf(),
            store: self.engine.store.clone(),
            events: self.events.clone(),
            pending: self.pending.clone(),
            questions: self.questions.clone(),
            next_permission: self.next_permission.clone(),
            abort,
            child_tools: Mutex::default(),
        })
    }

    pub fn prompt(&self, request: PromptRequest) {
        // Rust build: a prompt sent while the session runs joins (steers) that run and shares its stop token.
        let abort = self.runs.lock().unwrap().entry(request.session_id.clone()).or_default().clone();
        let (engine, events, runs, projects) = (self.engine.clone(), self.events.clone(), self.runs.clone(), self.projects.clone());
        let host = self.host(&request.session_id, &request.directory, abort.clone());
        self.runtime().spawn(async move {
            let session_id = request.session_id.clone();
            let directory = request.directory.clone();
            let outcome = match run(&engine, request, host, abort).await {
                Ok(()) => Outcome::Done,
                Err(opencode_session::Error::Aborted) => Outcome::Aborted,
                Err(error) => Outcome::Failed(error.to_string()),
            };
            runs.lock().unwrap().remove(&session_id);
            let messages = engine.store.list_messages(&session_id).ok();
            if let Some(messages) = &messages {
                let _ = events.send(Event::Usage { session_id: session_id.clone(), usage: usage(messages) });
            }
            let rows = messages.map(|messages| rows_from_store(&messages));
            let _ = events.send(Event::Finished { session_id, rows, outcome });
            let project = project_id(&projects, &directory);
            if let Ok(sessions) = list_sessions(&engine.store, &directory, &project) {
                let _ = events.send(Event::Sessions { directory, sessions });
            }
        });
    }

    // Session edits run off the UI thread and report failures as a notice.
    fn edit(&self, what: &'static str, work: impl FnOnce(&Engine, &UnboundedSender<Event>) -> Result<(), String> + Send + 'static) {
        let (engine, events) = (self.engine.clone(), self.events.clone());
        self.runtime().spawn_blocking(move || {
            if let Err(error) = work(&engine, &events) {
                let _ = events.send(Event::Failed { what, error });
            }
        });
    }

    pub fn rename(&self, session_id: String, title: String) {
        self.edit("desktop.rs.error.rename", move |engine, events| {
            let session = engine.store.patch_session(&session_id, |s| s.title = title).map_err(|e| e.to_string())?;
            let _ = events.send(Event::Title { session_id, title: session.title });
            Ok(())
        });
    }

    pub fn delete(&self, session_id: String) {
        self.cancel(&session_id);
        self.edit("desktop.rs.error.delete", move |engine, events| {
            engine.abort(&session_id);
            for child in engine.store.list_children(&session_id).map_err(|e| e.to_string())? {
                engine.store.delete_session(&child.id).map_err(|e| e.to_string())?;
            }
            // Rust build: its background shells end with it.
            opencode_tools::bash::background::kill_session(&session_id);
            engine.store.delete_session(&session_id).map_err(|e| e.to_string())?;
            let _ = events.send(Event::Deleted { session_id });
            Ok(())
        });
    }

    // Copies the history before `message_id` (all of it when None) into a new session.
    pub fn fork(&self, directory: PathBuf, session_id: String, message_id: Option<String>) {
        self.edit("desktop.rs.error.fork", move |engine, events| {
            let session = engine.fork(&session_id, message_id.as_deref()).map_err(|e| e.to_string())?;
            let title = session.title.clone();
            let _ = events.send(Event::Forked { directory, session: session_item(&session, title) });
            Ok(())
        });
    }

    // Hides `message_id` and everything after it until the next prompt deletes it or `unrevert` restores it.
    pub fn revert(&self, session_id: String, message_id: String) {
        self.edit("desktop.rs.error.revert", move |engine, events| {
            engine.revert(&session_id, &message_id, None).map_err(|e| e.to_string())?;
            send_stored(events, &engine.store, &session_id);
            Ok(())
        });
    }

    pub fn unrevert(&self, session_id: String) {
        self.edit("desktop.rs.error.unrevert", move |engine, events| {
            engine.unrevert(&session_id).map_err(|e| e.to_string())?;
            send_stored(events, &engine.store, &session_id);
            Ok(())
        });
    }

    // Writes one finished exchange that exercises every row kind; `--smoke` renders it from a throwaway store.
    pub fn seed_demo(&self, directory: &Path) -> Result<String, String> {
        let store = &self.engine.store;
        let session_id = id::session();
        let request = PromptRequest {
            directory: directory.to_path_buf(),
            session_id: session_id.clone(),
            create: true,
            agent: Some("build".into()),
            ..Default::default()
        };
        create_session(store, &request)?;
        let now = id::now_ms();
        let todo = |content: &str, status: &str| Todo { content: content.into(), status: status.into(), priority: "medium".into() };
        let todos = [todo("Make greet take a name", "completed"), todo("Update the callers", "in_progress"), todo("Fix the failing test", "pending")];
        store.set_todos(&session_id, &todos).map_err(|e| e.to_string())?;
        let user = UserMessage {
            id: id::message(),
            session_id: session_id.clone(),
            time: UserTime { created: now },
            format: None,
            summary: None,
            agent: "build".into(),
            model: UserModel { provider_id: "demo".into(), model_id: "demo".into(), variant: None },
            system: None,
            tools: None,
        };
        let assistant = AssistantMessage {
            id: id::message(),
            session_id: session_id.clone(),
            time: AssistantTime { created: now, completed: Some(now) },
            error: None,
            parent_id: user.id.clone(),
            model_id: "demo".into(),
            provider_id: "demo".into(),
            mode: "build".into(),
            agent: "build".into(),
            path: MessagePath::default(),
            summary: None,
            cost: 0.0123,
            tokens: Tokens { total: None, input: 4_200.0, output: 380.0, reasoning: 120.0, cache: TokenCache { read: 0.0, write: 0.0 } },
            structured: None,
            variant: None,
            finish: Some("stop".into()),
        };
        let input: JsonMap = [("filePath".to_string(), Value::from("src/greet.rs"))].into_iter().collect();
        let metadata: JsonMap = [("diff".to_string(), Value::from(DEMO_PATCH))].into_iter().collect();
        let edit = ToolState::pending(input.clone(), "").to_running(input, now).to_completed("Edit applied successfully.", "src/greet.rs", metadata, None, now + 42);
        let command: JsonMap = [("command".to_string(), Value::from("cargo test"))].into_iter().collect();
        let failed = ToolState::pending(command.clone(), "").to_running(command, now).to_error("error: test failed, to rerun pass `--lib`", None, now + 1_300);
        let text = |text: &str| PartBody::Text(TextPart { text: text.into(), ..Default::default() });
        let tool = |name: &str, state: ToolState| PartBody::Tool(ToolPart { call_id: id::part(), tool: name.into(), state, metadata: None });
        let reasoning = PartBody::Reasoning(ReasoningPart {
            text: "The greeting is hard-coded; make it take a name.".into(),
            metadata: None,
            time: PartTime { start: now, end: Some(now) },
        });

        let save = |result: opencode_storage::store::Result<()>| result.map_err(|e| e.to_string());
        save(store.upsert_message(&Message::User(user.clone())))?;
        save(store.upsert_part(&Part::new(&session_id, &user.id, text("Make `greet` take a name."))))?;
        save(store.upsert_message(&Message::Assistant(assistant.clone())))?;
        for body in [reasoning, text(DEMO_REPLY), tool("edit", edit), tool("bash", failed)] {
            save(store.upsert_part(&Part::new(&session_id, &assistant.id, body)))?;
        }
        Ok(session_id)
    }

    pub fn cancel(&self, session_id: &str) {
        if let Some(token) = self.runs.lock().unwrap().get(session_id) {
            token.cancel();
        }
        // Rust build: also a run started without this client (a background shell's notice, a `/goal` round).
        self.engine.abort(session_id);
    }

    // Registers a pending request the way a running prompt would, for tests that answer it through the UI.
    #[cfg(test)]
    pub fn expect_reply(&self, permission_id: u64) -> oneshot::Receiver<Reply> {
        let (reply, answer) = oneshot::channel();
        self.pending.lock().unwrap().insert(permission_id, reply);
        answer
    }

    pub fn reply(&self, permission_id: u64, reply: Reply) {
        if let Some(sender) = self.pending.lock().unwrap().remove(&permission_id) {
            let _ = sender.send(reply);
        }
    }

    #[cfg(test)]
    pub fn expect_answer(&self, question_id: u64) -> oneshot::Receiver<Answers> {
        let (reply, answer) = oneshot::channel();
        self.questions.lock().unwrap().insert(question_id, reply);
        answer
    }

    // One list of labels (or typed text) per question; None dismisses the question.
    pub fn answer(&self, question_id: u64, answers: Answers) {
        if let Some(sender) = self.questions.lock().unwrap().remove(&question_id) {
            let _ = sender.send(answers);
        }
    }
}

// The CLI's `copilot_login`, `chatgpt_browser` and `chatgpt_headless` without the terminal: `waiting` shows where
// to sign in. An empty error means the provider refused without saying why.
async fn run_login(flow: LoginFlow, auth_file: &Path, waiting: &impl Fn(LoginStep)) -> Result<(), String> {
    use opencode_llm::{codex, copilot};
    let http = reqwest::Client::new();
    let failed = |e: opencode_llm::Error| e.to_string();
    let wait = |ms: u64| tokio::time::sleep(std::time::Duration::from_millis(ms));
    let (provider_id, info) = match flow {
        LoginFlow::ChatgptBrowser => {
            let login = codex::login_start(codex::ISSUER).await.map_err(failed)?;
            waiting(LoginStep::Waiting { url: login.url.clone(), code: None });
            (codex::PROVIDER_ID, login.wait(&http).await.map_err(failed)?)
        }
        LoginFlow::ChatgptHeadless => {
            let login = codex::device_start(&http, codex::ISSUER).await.map_err(failed)?;
            waiting(LoginStep::Waiting { url: login.url.clone(), code: Some(login.user_code.clone()) });
            loop {
                match codex::device_poll(&http, &login).await.map_err(failed)? {
                    copilot::Poll::Success(info) => break (codex::PROVIDER_ID, info),
                    copilot::Poll::Pending { wait_ms } => wait(wait_ms).await,
                    copilot::Poll::Failed => return Err(String::new()),
                }
            }
        }
        LoginFlow::Copilot => {
            let device = copilot::device_start(&http, None).await.map_err(failed)?;
            waiting(LoginStep::Waiting { url: device.verification_uri.clone(), code: Some(device.user_code.clone()) });
            loop {
                match copilot::device_poll(&http, &device).await.map_err(failed)? {
                    copilot::Poll::Success(info) => break (copilot::PROVIDER_ID, info),
                    copilot::Poll::Pending { wait_ms } => wait(wait_ms).await,
                    copilot::Poll::Failed => return Err(String::new()),
                }
            }
        }
    };
    auth::set(auth_file, provider_id, info).map_err(|e| e.to_string())
}

async fn run(engine: &Engine, request: PromptRequest, host: Arc<RunHost>, abort: CancellationToken) -> Result<(), opencode_session::Error> {
    if request.create {
        create_session(&engine.store, &request).map_err(opencode_session::Error::Config)?;
    }
    let files: Vec<FilePart> = request
        .files
        .iter()
        .map(|a| FilePart { mime: a.mime.to_string(), filename: Some(a.name.to_string()), url: a.url.to_string(), source: None })
        .collect();
    match request.work {
        Work::Prompt => {
            #[allow(clippy::needless_update)]
            let input = PromptInput {
                directory: request.directory,
                session_id: Some(request.session_id),
                text: request.text,
                model: request.model,
                agent: request.agent,
                variant: request.variant,
                files,
                ..Default::default()
            };
            engine.prompt(input, host, abort).await.map(|_| ())
        }
        Work::Command(command) => {
            #[allow(clippy::needless_update)]
            let input = CommandInput {
                directory: request.directory,
                session_id: Some(request.session_id),
                command,
                arguments: request.text,
                model: request.model,
                agent: request.agent,
                variant: request.variant,
                files,
                ..Default::default()
            };
            engine.command(input, host, abort).await.map(|_| ())
        }
        Work::Compact => engine.summarize(&request.session_id, false, host, abort).await.map(|_| ()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{Duration, Instant};

    struct Fixture {
        _root: tempfile::TempDir,
        project: PathBuf,
        backend: Backend,
        events: UnboundedReceiver<Event>,
    }

    fn fixture() -> Fixture {
        fixture_with(json!({
            "model": "local/alpha",
            "provider": { "local": {
                "name": "Local",
                "npm": "@ai-sdk/openai-compatible",
                "options": { "baseURL": "http://127.0.0.1:9/v1", "apiKey": "test" },
                "models": { "beta": { "name": "Beta" }, "alpha": { "name": "Alpha" } }
            } }
        }))
    }

    fn fixture_with(config: Value) -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let dir = |name: &str| {
            let path = root.path().join(name);
            std::fs::create_dir_all(&path).unwrap();
            path
        };
        let paths = Paths { home: dir("home"), data: dir("data"), cache: dir("cache"), config: dir("config"), state: dir("state"), tmp: dir("tmp") };
        let project = dir("project");
        std::fs::write(project.join("opencode.json"), config.to_string()).unwrap();
        let (backend, events) = Backend::with_engine(crate::model::backend::offline(Engine::with_store(paths, Store::open_in_memory().unwrap())));
        Fixture { _root: root, project, backend, events }
    }

    impl Fixture {
        fn next<T>(&mut self, mut pick: impl FnMut(Event) -> Option<T>) -> T {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                match self.events.try_recv() {
                    Ok(event) => {
                        if let Some(found) = pick(event) {
                            return found;
                        }
                    }
                    Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                    Err(_) => panic!("timed out waiting for a backend event"),
                }
            }
        }

        fn store(&self) -> &Store {
            &self.backend.engine.store
        }

        fn seed(&self, session_id: &str, text: &str) {
            let request = PromptRequest {
                directory: self.project.clone(),
                session_id: session_id.into(),
                create: true,
                agent: Some("build".into()),
                ..Default::default()
            };
            create_session(self.store(), &request).unwrap();
            let user = UserMessage {
                id: id::message(),
                session_id: session_id.into(),
                time: UserTime { created: id::now_ms() },
                format: None,
                summary: None,
                agent: "plan".into(),
                model: UserModel { provider_id: "local".into(), model_id: "beta".into(), variant: None },
                system: None,
                tools: None,
            };
            self.store().upsert_message(&Message::User(user.clone())).unwrap();
            let body = PartBody::Text(TextPart { text: text.into(), ..Default::default() });
            self.store().upsert_part(&Part::new(session_id, &user.id, body)).unwrap();
        }
    }

    #[test]
    fn loads_sessions_and_catalog_for_a_project() {
        let mut fx = fixture();
        let first = id::session();
        fx.seed(&first, "Explain the build\nin detail");
        fx.backend.load_project(fx.project.clone());

        let sessions = fx.next(|e| match e {
            Event::Sessions { sessions, .. } => Some(sessions),
            _ => None,
        });
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id.as_ref(), first);
        assert_eq!(sessions[0].title.as_ref(), "Explain the build");

        fx.backend.load_project(fx.project.clone());
        let catalog = fx.next(|e| match e {
            Event::Catalog { catalog, .. } => Some(catalog),
            _ => None,
        });
        let local = catalog.models.iter().find(|g| g.provider.as_ref() == "Local").expect("configured provider");
        let names: Vec<&str> = local.models.iter().map(|m| m.name.as_ref()).collect();
        assert_eq!(names, ["Alpha", "Beta"]);
        assert_eq!(local.models[0].spec.as_ref(), "local/alpha");
        assert_eq!(catalog.default_model.as_deref(), Some("local/alpha"));
        assert_eq!(catalog.default_agent.as_deref(), Some("build"));
        assert!(catalog.agents.iter().any(|a| a.name.as_ref() == "plan"));
        assert!(!catalog.agents.iter().any(|a| a.name.as_ref() == "general"));
    }

    // An API key goes to auth.json as `opencode providers login` writes it; disconnecting removes it again.
    #[test]
    fn connecting_and_disconnecting_a_provider_edits_auth_json() {
        let mut fx = fixture();
        let catalog = |fx: &mut Fixture| {
            fx.next(|e| match e {
                Event::Catalog { catalog, .. } => Some(catalog),
                _ => None,
            })
        };
        fx.backend.reload_catalog(fx.project.clone());
        let before = catalog(&mut fx);
        let local = before.connected.iter().find(|p| p.id.as_ref() == "local").expect("configured provider");
        // An openai-compatible entry with its own models is a custom provider (`isConfigCustom`).
        assert_eq!((local.source, local.removable), (ProviderSource::Custom, false));
        let alpha = before.model("local/alpha").unwrap();
        assert_eq!((alpha.provider_id.as_ref(), alpha.provider.as_ref(), alpha.shown), ("local", "Local", true), "undated models are listed");

        fx.backend.connect_key(fx.project.clone(), "local".into(), "sk-test".into());
        let step = fx.next(|e| match e {
            Event::Login { provider_id, step } if provider_id == "local" => Some(step),
            _ => None,
        });
        assert_eq!(step, LoginStep::Done);
        let file = fx.backend.engine.paths.auth_file();
        let stored: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(stored["local"], json!({"type": "api", "key": "sk-test"}));
        let after = catalog(&mut fx);
        let local = after.connected.iter().find(|p| p.id.as_ref() == "local").unwrap();
        assert_eq!((local.source, local.removable), (ProviderSource::Api, true));

        fx.backend.disconnect(fx.project.clone(), "local".into());
        let removed = catalog(&mut fx);
        assert!(!removed.connected.iter().any(|p| p.id.as_ref() == "local" && p.removable));
        let stored: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert!(stored.get("local").is_none());
    }

    #[test]
    fn opening_a_session_returns_rows_and_its_last_model() {
        let mut fx = fixture();
        let session = id::session();
        fx.seed(&session, "hello there");
        fx.backend.open_session(session.clone());
        let (rows, model, agent) = fx.next(|e| match e {
            Event::Opened { rows, model, agent, .. } => Some((rows, model, agent)),
            _ => None,
        });
        assert_eq!(rows.len(), 1);
        assert_eq!((model.as_deref(), agent.as_deref()), (Some("local/beta"), Some("plan")));
    }

    #[test]
    fn a_prompt_that_cannot_start_reports_failure_and_keeps_the_new_session() {
        let mut fx = fixture();
        let session = id::session();
        fx.backend.prompt(PromptRequest {
            directory: fx.project.clone(),
            session_id: session.clone(),
            create: true,
            text: "hi".into(),
            model: Some("missing/model".into()),
            ..Default::default()
        });
        let outcome = fx.next(|e| match e {
            Event::Finished { session_id, outcome, .. } if session_id == session => Some(outcome),
            _ => None,
        });
        assert!(matches!(&outcome, Outcome::Failed(message) if message.contains("missing")), "{outcome:?}");
        assert!(fx.store().get_session(&session).unwrap().is_some());
        assert!(fx.backend.runs.lock().unwrap().is_empty());
    }

    #[test]
    fn permission_requests_round_trip_and_cancel_rejects() {
        let mut fx = fixture();
        let abort = CancellationToken::new();
        let host = fx.backend.host("ses_1", &fx.project,abort.clone());
        let request = PermissionRequest {
            permission: "edit".into(),
            patterns: vec!["src/a.rs".into()],
            always: vec!["*".into()],
            metadata: json!({"filepath": "src/a.rs", "diff": "--- a\n+++ b\n"}),
        };

        let handle = fx.backend.runtime().handle().clone();
        let spawner = handle.clone();
        let ask = move |host: Arc<RunHost>, request: PermissionRequest| spawner.spawn(async move { host.permission(&request).await });
        let answer = ask(host.clone(), request.clone());
        let view = fx.next(|e| match e {
            Event::Permission(view) => Some(view),
            _ => None,
        });
        assert_eq!((view.permission.as_ref(), view.detail.as_deref()), ("edit", Some("src/a.rs")));
        assert_eq!(view.diff.as_deref(), Some("--- a\n+++ b\n"));
        fx.backend.reply(view.id, Reply::Always);
        assert_eq!(handle.block_on(answer).unwrap(), Reply::Always);
        let resolved = fx.next(|e| match e {
            Event::PermissionResolved(id) => Some(id),
            _ => None,
        });
        assert_eq!(resolved, view.id);

        let answer = ask(host, request);
        fx.next(|e| matches!(e, Event::Permission(_)).then_some(()));
        abort.cancel();
        assert_eq!(handle.block_on(answer).unwrap(), Reply::Reject);
        assert!(fx.backend.pending.lock().unwrap().is_empty());
    }

    fn sse(chunks: &[Value]) -> String {
        let mut out: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
        out.push_str("data: [DONE]\n\n");
        out
    }

    // An OpenAI-compatible endpoint on a local port: tool-bearing requests consume the script, title requests get a fixed reply.
    fn mock_provider(script: Vec<String>) -> u16 {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let script = Arc::new(Mutex::new(std::collections::VecDeque::from(script)));
        std::thread::spawn(move || {
            for socket in listener.incoming() {
                let (Ok(mut socket), script) = (socket, script.clone()) else { continue };
                std::thread::spawn(move || {
                    let mut buffer = Vec::new();
                    let body = loop {
                        let mut chunk = [0u8; 8192];
                        let Ok(read) = socket.read(&mut chunk) else { return };
                        if read == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                        let Some(split) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                        let head = String::from_utf8_lossy(&buffer[..split]).to_lowercase();
                        let length: usize = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
                        if buffer.len() >= split + 4 + length {
                            break String::from_utf8_lossy(&buffer[split + 4..]).into_owned();
                        }
                    };
                    let scripted = if body.contains("\"tools\"") { script.lock().unwrap().pop_front() } else { None };
                    // A reply marked `STALL:` is sent without its end and the connection is held open.
                    if let Some(partial) = scripted.as_deref().and_then(|s| s.strip_prefix("STALL:")) {
                        let partial = partial.replace("data: [DONE]\n\n", "");
                        let _ = socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{partial}").as_bytes());
                        let _ = socket.flush();
                        std::thread::sleep(Duration::from_secs(30));
                        return;
                    }
                    let reply = scripted.unwrap_or_else(|| {
                        sse(&[
                            json!({"choices": [{"index": 0, "delta": {"content": "Mock title"}}]}),
                            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
                        ])
                    });
                    let response = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{reply}", reply.len());
                    let _ = socket.write_all(response.as_bytes());
                });
            }
        });
        port
    }

    // The whole bridge against the real engine: stream, tool call, permission round trip, snapshots, completion.
    #[test]
    fn a_prompt_streams_asks_permission_and_finishes() {
        use crate::model::transcript::{RowKind, ToolStatus};
        let write = json!({"filePath": "note.txt", "content": "hello\n"}).to_string();
        let port = mock_provider(vec![
            sse(&[
                json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "write", "arguments": write}}]}}]}),
                json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}], "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}}),
            ]),
            sse(&[
                json!({"choices": [{"index": 0, "delta": {"content": "All "}}]}),
                json!({"choices": [{"index": 0, "delta": {"content": "done"}}]}),
                json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 20, "completion_tokens": 2, "total_tokens": 22}}),
            ]),
        ]);
        let mut fx = fixture_with(json!({
            "model": "mock/m1",
            "permission": {"edit": "ask"},
            "provider": {"mock": {"options": {"baseURL": format!("http://127.0.0.1:{port}/v1"), "apiKey": "k"}, "models": {"m1": {"name": "Mock"}}}}
        }));
        let session = id::session();
        fx.backend.prompt(PromptRequest {
            directory: fx.project.clone(),
            session_id: session.clone(),
            create: true,
            text: "write the note".into(),
            ..Default::default()
        });

        let mut seen: Vec<String> = Vec::new();
        let mut streamed = String::new();
        let (rows, outcome) = loop {
            match fx.next(Some) {
                Event::Tool { session_id, part } => {
                    assert_eq!(session_id, session);
                    let PartBody::Tool(tool) = &part.body else { panic!("tool part expected") };
                    seen.push(format!("tool:{}", tool.state.status()));
                }
                Event::Permission(view) => {
                    assert_eq!((view.session_id.as_ref(), view.permission.as_ref()), (session.as_str(), "edit"));
                    assert!(view.diff.is_some(), "the write preview travels with the request");
                    assert!(!fx.project.join("note.txt").exists(), "nothing is written before the answer");
                    seen.push("permission".into());
                    fx.backend.reply(view.id, Reply::Once);
                }
                Event::PermissionResolved(_) => seen.push("resolved".into()),
                Event::Snapshot { .. } => seen.push("snapshot".into()),
                Event::Text { delta, .. } => {
                    streamed.push_str(&delta);
                    if seen.last().map(String::as_str) != Some("text") {
                        seen.push("text".into());
                    }
                }
                Event::Finished { session_id, rows, outcome } => {
                    assert_eq!(session_id, session);
                    break (rows.expect("final snapshot"), outcome);
                }
                _ => {}
            }
        };
        assert_eq!(outcome, Outcome::Done);
        assert_eq!(streamed, "All done");
        seen.dedup();
        assert_eq!(seen, ["tool:running", "permission", "resolved", "tool:completed", "snapshot", "text", "snapshot"]);
        assert_eq!(std::fs::read_to_string(fx.project.join("note.txt")).unwrap(), "hello\n");

        let kinds: Vec<String> = rows
            .iter()
            .map(|row| match &row.kind {
                RowKind::User { text, .. } => format!("user:{text}"),
                RowKind::Tool(view) => format!("{}:{}", view.tool, view.status.label()),
                RowKind::Text { markdown, .. } => format!("text:{markdown}"),
                _ => "other".into(),
            })
            .collect();
        assert_eq!(kinds, ["user:write the note", "write:completed", "text:All done"]);
        assert!(rows.iter().any(|row| matches!(&row.kind, RowKind::Tool(view) if view.status == ToolStatus::Completed)));

        let sessions = fx.next(|e| match e {
            Event::Sessions { sessions, .. } => Some(sessions),
            _ => None,
        });
        assert_eq!(sessions[0].id.as_ref(), session);
        assert!(fx.backend.runs.lock().unwrap().is_empty());
    }

    #[test]
    fn the_demo_session_covers_every_row_kind() {
        use crate::model::transcript::RowKind;
        let mut fx = fixture();
        let session = fx.backend.seed_demo(&fx.project).unwrap();
        fx.backend.open_session(session);
        let rows = fx.next(|e| match e {
            Event::Opened { rows, .. } => Some(rows),
            _ => None,
        });
        let kinds: Vec<&str> = rows
            .iter()
            .map(|row| match &row.kind {
                RowKind::User { .. } => "user",
                RowKind::Reasoning { .. } => "reasoning",
                RowKind::Text { .. } => "text",
                RowKind::Tool(view) => view.status.label(),
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["user", "reasoning", "text", "completed", "error"]);
    }

    // Stop during a running prompt: the stream is cut, the run ends as aborted and the streamed text stays on screen.
    #[test]
    fn stopping_a_running_prompt_aborts_and_keeps_the_streamed_text() {
        use crate::model::state::AppState;
        use crate::model::transcript::RowKind;
        let partial = sse(&[json!({"choices": [{"index": 0, "delta": {"content": "Partial answer"}}]})]);
        let port = mock_provider(vec![format!("STALL:{partial}")]);
        let mut fx = fixture_with(json!({
            "model": "mock/m1",
            "provider": {"mock": {"options": {"baseURL": format!("http://127.0.0.1:{port}/v1"), "apiKey": "k"}, "models": {"m1": {"name": "Mock"}}}}
        }));
        let session = id::session();
        let mut state = AppState::new(fx.project.clone());
        state.begin_prompt(&session, "explain", vec![], 1);
        fx.backend.prompt(PromptRequest {
            directory: fx.project.clone(),
            session_id: session.clone(),
            create: true,
            text: "explain".into(),
            ..Default::default()
        });

        let started = Instant::now();
        let mut stopped = false;
        let outcome = loop {
            let event = fx.next(Some);
            if matches!(&event, Event::Text { .. }) && !stopped {
                stopped = true;
                assert!(state.current_running());
                fx.backend.cancel(&session);
            }
            let finished = match &event {
                Event::Finished { outcome, .. } => Some(outcome.clone()),
                _ => None,
            };
            state.apply(event);
            if let Some(outcome) = finished {
                break outcome;
            }
        };
        assert!(stopped, "text streamed before the run ended");
        assert_eq!(outcome, Outcome::Aborted);
        assert!(started.elapsed() < Duration::from_secs(15), "cancel must not wait for the provider");
        assert!(!state.current_running());
        assert!(fx.backend.runs.lock().unwrap().is_empty());
        let texts: Vec<String> = state
            .visible()
            .iter()
            .filter_map(|row| match &row.kind {
                RowKind::Text { markdown, streaming } => Some(format!("{markdown}:{streaming}")),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["Partial answer:false"], "exactly one copy of the partial text remains");
    }

    // A slash command goes through `Engine::command`: its template is expanded and sent as the prompt.
    #[test]
    fn a_custom_command_expands_its_template_and_runs() {
        use crate::model::transcript::RowKind;
        let port = mock_provider(vec![sse(&[
            json!({"choices": [{"index": 0, "delta": {"content": "Shipped"}}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 9, "completion_tokens": 1, "total_tokens": 10}}),
        ])]);
        let mut fx = fixture_with(json!({
            "model": "mock/m1",
            "command": {"deploy": {"template": "Deploy $ARGUMENTS", "description": "Ship it"}},
            "provider": {"mock": {"options": {"baseURL": format!("http://127.0.0.1:{port}/v1"), "apiKey": "k"}, "models": {"m1": {"name": "Mock"}}}}
        }));
        let session = id::session();
        fx.backend.prompt(PromptRequest {
            directory: fx.project.clone(),
            session_id: session.clone(),
            create: true,
            work: Work::Command("deploy".into()),
            text: "to staging".into(),
            ..Default::default()
        });
        let (rows, outcome) = fx.next(|e| match e {
            Event::Finished { rows, outcome, .. } => Some((rows.expect("final snapshot"), outcome)),
            _ => None,
        });
        assert_eq!(outcome, Outcome::Done);
        let shown: Vec<String> = rows
            .iter()
            .filter_map(|row| match &row.kind {
                RowKind::User { text, .. } => Some(format!("user:{text}")),
                RowKind::Text { markdown, .. } => Some(format!("text:{markdown}")),
                _ => None,
            })
            .collect();
        assert_eq!(shown, ["user:Deploy to staging", "text:Shipped"]);
    }

    fn add_user(fx: &Fixture, session_id: &str, text: &str) -> String {
        let user = UserMessage {
            id: id::message(),
            session_id: session_id.into(),
            time: UserTime { created: id::now_ms() },
            format: None,
            summary: None,
            agent: "build".into(),
            model: UserModel { provider_id: "local".into(), model_id: "beta".into(), variant: None },
            system: None,
            tools: None,
        };
        fx.store().upsert_message(&Message::User(user.clone())).unwrap();
        let body = PartBody::Text(TextPart { text: text.into(), ..Default::default() });
        fx.store().upsert_part(&Part::new(session_id, &user.id, body)).unwrap();
        user.id
    }

    #[test]
    fn sessions_can_be_renamed_forked_reverted_and_deleted() {
        let mut fx = fixture();
        let session = id::session();
        fx.seed(&session, "first question");
        let second = add_user(&fx, &session, "second question");

        fx.backend.rename(session.clone(), "Renamed".into());
        let title = fx.next(|e| match e {
            Event::Title { session_id, title } if session_id == session => Some(title),
            _ => None,
        });
        assert_eq!(title, "Renamed");
        assert_eq!(fx.store().get_session(&session).unwrap().unwrap().title, "Renamed");

        fx.backend.revert(session.clone(), second.clone());
        let rows = fx.next(|e| match e {
            Event::Snapshot { rows, .. } => Some(rows),
            _ => None,
        });
        assert_eq!(rows.len(), 1, "the reverted message is hidden");
        let hidden = fx.next(|e| match e {
            Event::Revert { hidden, .. } => Some(hidden),
            _ => None,
        });
        assert_eq!(hidden, 1);
        // Reopening shows the same cut.
        fx.backend.open_session(session.clone());
        let hidden = fx.next(|e| match e {
            Event::Revert { hidden, .. } => Some(hidden),
            _ => None,
        });
        let rows = fx.next(|e| match e {
            Event::Opened { rows, .. } => Some(rows),
            _ => None,
        });
        assert_eq!((hidden, rows.len()), (1, 1));

        fx.backend.unrevert(session.clone());
        let rows = fx.next(|e| match e {
            Event::Snapshot { rows, .. } => Some(rows),
            _ => None,
        });
        let hidden = fx.next(|e| match e {
            Event::Revert { hidden, .. } => Some(hidden),
            _ => None,
        });
        assert_eq!((rows.len(), hidden), (2, 0));

        // A fork before the second message carries only the first.
        fx.backend.fork(fx.project.clone(), session.clone(), Some(second));
        let fork = fx.next(|e| match e {
            Event::Forked { session, .. } => Some(session),
            _ => None,
        });
        assert_eq!(fork.title.as_ref(), "Renamed (fork #1)");
        assert_eq!(fork.parent, None);
        assert_eq!(fx.store().list_messages(&fork.id).unwrap().len(), 1);

        fx.backend.delete(session.clone());
        let deleted = fx.next(|e| match e {
            Event::Deleted { session_id } => Some(session_id),
            _ => None,
        });
        assert_eq!(deleted, session);
        assert!(fx.store().get_session(&session).unwrap().is_none());
        assert!(fx.store().get_session(&fork.id).unwrap().is_some());

        fx.backend.rename("ses_missing".into(), "x".into());
        let what = fx.next(|e| match e {
            Event::Failed { what, .. } => Some(what),
            _ => None,
        });
        assert_eq!(what, "desktop.rs.error.rename");
    }

    #[test]
    fn subagent_sessions_are_listed_under_their_parent_and_report_usage() {
        let mut fx = fixture();
        let parent = fx.backend.seed_demo(&fx.project).unwrap();
        let child = fx
            .backend
            .engine
            .create_session(&fx.project, opencode_session::CreateSession { parent_id: Some(parent.clone()), title: Some("Explore".into()), ..Default::default() })
            .unwrap();
        fx.backend.refresh_sessions(fx.project.clone());
        let sessions = fx.next(|e| match e {
            Event::Sessions { sessions, .. } => Some(sessions),
            _ => None,
        });
        let listed = sessions.iter().find(|s| s.id.as_ref() == child.id).expect("children are listed");
        assert_eq!((listed.parent.as_deref(), listed.title.as_ref()), (Some(parent.as_str()), "Explore"));

        fx.backend.open_session(parent.clone());
        let todos = fx.next(|e| match e {
            Event::Todos { todos, .. } => Some(todos),
            _ => None,
        });
        assert_eq!(todos.len(), 3);
        let usage = fx.next(|e| match e {
            Event::Usage { usage, .. } => Some(usage),
            _ => None,
        });
        assert_eq!((usage.input, usage.output, usage.reasoning, usage.context), (4_200, 380, 120, 4_700));
        assert!((usage.cost - 0.0123).abs() < 1e-9);
    }

    #[test]
    fn project_files_and_commands_are_listed() {
        let mut fx = fixture_with(json!({
            "model": "local/alpha",
            "command": {"deploy": {"template": "Deploy $ARGUMENTS", "description": "Ship it"}},
            "provider": {"local": {"npm": "@ai-sdk/openai-compatible", "options": {"baseURL": "http://127.0.0.1:9/v1", "apiKey": "k"}, "models": {"alpha": {}}}}
        }));
        for (path, text) in [("src/main.rs", "fn main() {}"), ("docs/guide.md", "# Guide"), (".cache/skip.bin", "x"), ("node_modules/pkg/index.js", "x")] {
            let file = fx.project.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, text).unwrap();
        }
        let mut walked = walk_files(&fx.project);
        walked.sort();
        assert_eq!(walked, list_files(&fx.project), "outside a repository the walk is the listing");
        fx.backend.load_project(fx.project.clone());
        let mut files = None;
        let mut commands = None;
        while files.is_none() || commands.is_none() {
            match fx.next(Some) {
                Event::Files { files: listed, .. } => files = Some(listed),
                Event::Commands { commands: listed, .. } => commands = Some(listed),
                _ => {}
            }
        }
        assert_eq!(files.unwrap(), ["docs/guide.md", "opencode.json", "src/main.rs"]);
        let commands = commands.unwrap();
        let deploy = commands.iter().find(|c| c.name.as_ref() == "deploy").expect("the configured command");
        assert_eq!((deploy.description.as_ref(), deploy.badge), ("Ship it", "prompt.slash.badge.custom"));
    }

    #[test]
    fn questions_round_trip_and_cancel_dismisses() {
        use opencode_session::{QuestionInfo, QuestionOption};
        let mut fx = fixture();
        let abort = CancellationToken::new();
        let host = fx.backend.host("ses_1", &fx.project,abort.clone());
        let request = QuestionRequest {
            id: "que_1".into(),
            session_id: "ses_child".into(),
            questions: vec![QuestionInfo {
                question: "Which one?".into(),
                header: "Pick".into(),
                options: vec![QuestionOption { label: "A".into(), description: "".into() }],
                multiple: None,
                custom: None,
            }],
            tool: None,
        };
        let handle = fx.backend.runtime().handle().clone();
        let spawner = handle.clone();
        let ask = move |host: Arc<RunHost>, request: QuestionRequest| spawner.spawn(async move { host.question(&request).await });

        let answer = ask(host.clone(), request.clone());
        let view = fx.next(|e| match e {
            Event::Question(view) => Some(view),
            _ => None,
        });
        assert_eq!((view.session_id.as_ref(), view.questions.len()), ("ses_1", 1), "shown under the session on screen");
        fx.backend.answer(view.id, Some(vec![vec!["A".into()]]));
        assert_eq!(handle.block_on(answer).unwrap(), Some(vec![vec!["A".to_string()]]));
        assert_eq!(fx.next(|e| if let Event::QuestionResolved(id) = e { Some(id) } else { None }), view.id);

        let answer = ask(host, request);
        fx.next(|e| matches!(e, Event::Question(_)).then_some(()));
        abort.cancel();
        assert_eq!(handle.block_on(answer).unwrap(), None);
        assert!(fx.backend.questions.lock().unwrap().is_empty());
    }

    #[test]
    fn engine_updates_map_to_events() {
        let mut fx = fixture();
        let host = fx.backend.host("ses_1", &fx.project,CancellationToken::new());
        let child = |update: Update| Update::Child { session_id: "ses_child".into(), update: Box::new(update) };
        let input: JsonMap = [("pattern".to_string(), Value::from("parse_"))].into_iter().collect();
        let running = ToolState::pending(input.clone(), "").to_running(input, 1);
        let part = Part::new("ses_child", "msg_c", PartBody::Tool(ToolPart { call_id: "c1".into(), tool: "grep".into(), state: running, metadata: None }));

        host.update(Update::Status { session_id: "ses_1".into(), status: Status::Retry { attempt: 2, message: "busy".into(), next: 5 } });
        host.update(Update::Todos { session_id: "ses_1".into(), todos: vec![] });
        host.update(Update::Error { session_id: "ses_1".into(), message: "HTTP 500".into() });
        host.update(child(Update::Text("looking".into())));
        host.update(child(Update::Tool(Box::new(part.clone()))));
        host.update(child(Update::Tool(Box::new(part))));
        host.update(child(Update::Status { session_id: "ses_child".into(), status: Status::Busy }));
        host.update(child(Update::Title { session_id: "ses_child".into(), title: "Explore".into() }));
        host.update(child(Update::Child { session_id: "ses_grandchild".into(), update: Box::new(Update::Text("deep".into())) }));
        host.update(child(Update::Reasoning("ignored".into())));
        drop(host);

        let mut seen = Vec::new();
        while let Ok(event) = fx.events.try_recv() {
            seen.push(match event {
                Event::Status { status: RunStatus::Retry { attempt, .. }, .. } => format!("retry:{attempt}"),
                Event::Todos { todos, .. } => format!("todos:{}", todos.len()),
                Event::Error { message, .. } => format!("error:{message}"),
                Event::Child { child_id, update } => match update {
                    ChildUpdate::Text(text) => format!("{child_id}:text:{text}"),
                    ChildUpdate::Tool { label, started } => format!("{child_id}:tool:{label}:{started}"),
                    ChildUpdate::Running(running) => format!("{child_id}:running:{running}"),
                    ChildUpdate::Title(title) => format!("{child_id}:title:{title}"),
                },
                other => format!("{other:?}"),
            });
        }
        assert_eq!(
            seen,
            [
                "retry:2",
                "todos:0",
                "error:HTTP 500",
                "ses_child:text:looking",
                "ses_child:tool:grep parse_:true",
                "ses_child:tool:grep parse_:false",
                "ses_child:running:true",
                "ses_child:title:Explore",
                "ses_grandchild:text:deep",
            ]
        );
    }

    #[test]
    fn idle_errors_and_requests_raise_attention_with_the_stored_session() {
        let mut fx = fixture();
        fx.seed("ses_1", "Fix the parser");
        fx.seed("ses_sub", "look around");
        fx.store().patch_session("ses_sub", |s| s.parent_id = Some("ses_1".into())).unwrap();
        let title = fx.store().get_session("ses_1").unwrap().unwrap().title;
        let host = fx.backend.host("ses_1", &fx.project, CancellationToken::new());
        let sub = fx.backend.host("ses_sub", &fx.project, CancellationToken::new());
        let error = opencode_storage::AssistantError::UnknownError { message: "boom".into(), r#ref: None };
        host.update(Update::Status { session_id: "ses_1".into(), status: Status::Busy });
        host.update(Update::SessionError { session_id: "ses_1".into(), error: error.clone() });
        host.update(Update::Status { session_id: "ses_1".into(), status: Status::Idle });
        // A subagent reached through its parent's run is the parent's business, as in the original.
        host.update(Update::Child { session_id: "ses_sub".into(), update: Box::new(Update::Status { session_id: "ses_sub".into(), status: Status::Idle }) });
        sub.update(Update::Status { session_id: "ses_sub".into(), status: Status::Idle });
        host.update(Update::Status { session_id: "ses_gone".into(), status: Status::Idle });

        let handle = fx.backend.runtime().handle().clone();
        let request = PermissionRequest { permission: "bash".into(), patterns: vec!["ls".into()], always: vec![], metadata: json!({}) };
        let asking = host.clone();
        let answer = handle.spawn(async move { asking.permission(&request).await });
        let mut signals = Vec::new();
        let id = fx.next(|e| match e {
            Event::Permission(view) => Some(view.id),
            Event::Attention(signal) => {
                signals.push(signal);
                None
            }
            _ => None,
        });
        fx.backend.reply(id, Reply::Once);
        handle.block_on(answer).unwrap();
        drop((host, sub));

        while let Ok(event) = fx.events.try_recv() {
            if let Event::Attention(signal) = event {
                signals.push(signal);
            }
        }
        let found = |title: &str, child| Some(Found { title: title.into(), child });
        let signal = |kind, session: &str, found| Signal { kind, directory: fx.project.clone(), session_id: Some(session.into()), session: found };
        // The permission was asked after the status updates; it arrives last.
        assert_eq!(
            signals,
            [
                signal(Kind::Error, "ses_1", found(&title, false)),
                signal(Kind::Idle, "ses_1", found(&title, false)),
                signal(Kind::Idle, "ses_sub", found(&fx.store().get_session("ses_sub").unwrap().unwrap().title, true)),
                signal(Kind::Idle, "ses_gone", None),
                signal(Kind::Permission(id), "ses_1", found(&title, false)),
            ]
        );
    }

    #[test]
    fn directories_compare_across_spellings() {
        if cfg!(windows) {
            assert!(same_directory(r"\\?\C:\Work\App\", Path::new(r"c:\work\app")));
        }
        assert!(same_directory("/work/app/", Path::new("/work/app")));
        assert!(!same_directory("/work/app2", Path::new("/work/app")));
    }
}
