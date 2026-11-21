// Where the TUI's sessions live: the engine in this process, or an opencode server over HTTP. Both report what
// happens as `Action`s on the driver's channel; the reducer cannot tell them apart.
use crate::app::{Action, Child, ChildTool, PermissionAsk, ToastKind};
use crate::question::QuestionAsk;
use opencode_client::{Client, CommandBody, CreateSession as RemoteCreate, Event, EventStream, ModelRef, PermissionReply, PromptBody, PromptPart, SessionPatch, SessionQuery, SessionStatus, ShellBody};
use opencode_session::{CommandInput, Engine, Error, Host, PermissionAnswer, PromptInput, QuestionInfo, QuestionOption, QuestionRequest, Reply, ShellInput, Status, Update};
use opencode_storage::{FilePart, Message, NewSession, Part, PartBody, Project, ProjectTime, Session, Todo, Vcs, id, identity};
use opencode_tools::PermissionRequest;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

// A session as the driver shows it, with what surrounds it.
pub struct Loaded {
    pub info: Session,
    pub messages: Vec<(Message, Vec<Part>)>,
    pub todos: Vec<Todo>,
    pub children: Vec<Child>,
    pub status: Status,
}

impl Loaded {
    pub(crate) fn actions(self, open: bool, settled: bool) -> Vec<Action> {
        let session_id = self.info.id.clone();
        vec![
            Action::SessionLoaded { info: Box::new(self.info), messages: self.messages, todos: self.todos, open, settled },
            Action::ChildrenLoaded { session_id, children: self.children, status: self.status },
        ]
    }
}

// One model turn (or shell command, or compaction) the user started.
#[derive(Debug, Clone, PartialEq)]
pub enum Turn {
    Prompt { session_id: String, text: String, files: Vec<FilePart>, agent: String, model: Option<String>, variant: Option<String> },
    // Rust build: a prompt that waits until the session is idle (opencode 2.x queue delivery).
    Queued { session_id: String, text: String, files: Vec<FilePart>, agent: String, model: Option<String>, variant: Option<String> },
    Shell { session_id: String, command: String, agent: String, model: Option<String> },
    Command { session_id: String, command: String, arguments: String, agent: String, model: Option<String> },
    Compact { session_id: String, model: Option<String> },
}

impl Turn {
    pub fn session_id(&self) -> &str {
        match self {
            Turn::Prompt { session_id, .. }
            | Turn::Queued { session_id, .. }
            | Turn::Shell { session_id, .. }
            | Turn::Command { session_id, .. }
            | Turn::Compact { session_id, .. } => session_id,
        }
    }
}

#[async_trait::async_trait]
pub trait Backend: Send + Sync {
    // Root sessions of the directory, most recently updated first.
    async fn sessions(&self) -> anyhow::Result<Vec<Session>>;
    async fn create_session(&self) -> anyhow::Result<Session>;
    async fn load(&self, session_id: &str) -> anyhow::Result<Loaded>;
    async fn delete_session(&self, session_id: &str) -> anyhow::Result<()>;
    async fn rename_session(&self, session_id: &str, title: &str) -> anyhow::Result<()>;
    async fn fork(&self, session_id: &str, message_id: Option<&str>) -> anyhow::Result<Session>;
    // Runs a turn to its end; an abort is not an error.
    async fn run(&self, turn: Turn, cancel: CancellationToken) -> Result<(), String>;
    async fn revert(&self, session_id: &str, message_id: &str) -> Result<(), String>;
    async fn unrevert(&self, session_id: &str) -> Result<(), String>;
    fn abort(&self, session_id: &str);
    fn reply_permission(&self, id: u64, answer: PermissionAnswer);
    fn reply_question(&self, id: u64, answers: Option<Vec<Vec<String>>>);
    // The UI is going away: whatever still waits on the user is released.
    fn release(&self);
    // Rust build: `/btw`: an answer from the session's context that is not added to it (v2 `session.generate`).
    async fn aside(&self, _session_id: &str, _prompt: &str) -> anyhow::Result<String> {
        anyhow::bail!("/btw is not available with a remote server")
    }
    // Rust build: `/reload`: drops the directory's instance so config, agents and commands are read again.
    async fn reload(&self) -> anyhow::Result<crate::catalog::Catalog> {
        anyhow::bail!("/reload is not available with a remote server")
    }
    // Rust build: `/stats`.
    async fn stats(&self) -> anyhow::Result<String> {
        anyhow::bail!("/stats is not available with a remote server")
    }
}

// A subagent session row: its tool calls and how long it ran, first prompt to last answer.
pub fn child(info: Session, messages: &[(Message, Vec<Part>)], status: Status) -> Child {
    let tools = messages.iter().flat_map(|(_, parts)| parts.iter().filter_map(ChildTool::from_part)).collect();
    let started = messages.iter().find_map(|(m, _)| m.as_user().map(|u| u.time.created));
    let completed = messages.iter().rev().find_map(|(m, _)| match m {
        Message::Assistant(a) => Some(a.time.completed),
        Message::User(_) => None,
    });
    let duration = match (started, completed.flatten()) {
        (Some(started), Some(completed)) => completed.saturating_sub(started),
        _ => 0,
    };
    Child { status, info, tools, duration }
}

fn newest_roots(sessions: Vec<Session>) -> Vec<Session> {
    let mut list: Vec<Session> = sessions.into_iter().filter(|s| s.parent_id.is_none()).collect();
    list.sort_by_key(|session| std::cmp::Reverse(session.time.updated));
    list
}

type Waiting = Arc<Mutex<HashMap<u64, oneshot::Sender<PermissionAnswer>>>>;
type Asking = Arc<Mutex<HashMap<u64, oneshot::Sender<Option<Vec<Vec<String>>>>>>>;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

// The engine's host: updates become actions, permission and question requests wait for the user's answer.
pub struct TuiHost {
    tx: UnboundedSender<Action>,
    waiting: Waiting,
    asking: Asking,
    next: AtomicU64,
}

impl TuiHost {
    async fn decide(&self, session_id: &str, request: &PermissionRequest) -> PermissionAnswer {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (reply, answer) = oneshot::channel();
        lock(&self.waiting).insert(id, reply);
        if self.tx.send(Action::PermissionAsked(PermissionAsk { id, session_id: session_id.to_string(), request: request.clone() })).is_err() {
            return PermissionAnswer::Reject;
        }
        answer.await.unwrap_or(PermissionAnswer::Reject)
    }
}

#[async_trait::async_trait]
impl Host for TuiHost {
    fn update(&self, update: Update) {
        let _ = self.tx.send(Action::Engine(update));
    }

    async fn permission(&self, request: &PermissionRequest) -> Reply {
        match self.decide("", request).await {
            PermissionAnswer::Once => Reply::Once,
            PermissionAnswer::Always => Reply::Always,
            _ => Reply::Reject,
        }
    }

    async fn ask(&self, ask: &opencode_session::PermissionAsk) -> PermissionAnswer {
        self.decide(&ask.session_id, &ask.request).await
    }

    async fn question(&self, request: &QuestionRequest) -> Option<Vec<Vec<String>>> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (reply, answer) = oneshot::channel();
        lock(&self.asking).insert(id, reply);
        self.tx.send(Action::QuestionAsked(QuestionAsk { id, request: request.clone() })).ok()?;
        answer.await.ok().flatten()
    }
}

// The engine in this process, on the store it was given.
pub struct LocalBackend {
    pub engine: Arc<Engine>,
    pub host: Arc<TuiHost>,
    directory: PathBuf,
    worktree: Option<PathBuf>,
    project: OnceLock<String>,
}

impl LocalBackend {
    pub fn new(engine: Engine, tx: UnboundedSender<Action>, directory: PathBuf, worktree: Option<PathBuf>) -> Self {
        let engine = Arc::new(engine);
        opencode_server::attach_plugin_server(&engine);
        let host = Arc::new(TuiHost { tx, waiting: Arc::default(), asking: Arc::default(), next: AtomicU64::new(1) });
        Self { engine, host, directory, worktree, project: OnceLock::new() }
    }

    fn project(&self) -> &str {
        self.project.get_or_init(|| identity::resolve(&self.directory).id)
    }

    fn children(&self, info: &Session) -> Vec<Child> {
        let store = &self.engine.store;
        let nested = info.parent_id.is_none();
        let mut queue = vec![info.parent_id.clone().unwrap_or_else(|| info.id.clone())];
        let mut out = Vec::new();
        while let Some(parent) = queue.pop() {
            for entry in store.list_children(&parent).unwrap_or_default() {
                let messages = store.list_messages(&entry.id).unwrap_or_default();
                if nested {
                    queue.push(entry.id.clone());
                }
                let status = self.engine.status(&entry.id);
                out.push(child(entry, &messages, status));
            }
        }
        out
    }
}

fn outcome<T>(result: Result<T, Error>) -> Result<(), String> {
    match result {
        Ok(_) | Err(Error::Aborted) => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

#[async_trait::async_trait]
impl Backend for LocalBackend {
    async fn sessions(&self) -> anyhow::Result<Vec<Session>> {
        Ok(newest_roots(self.engine.store.list_sessions(self.project()).unwrap_or_default()))
    }

    async fn create_session(&self) -> anyhow::Result<Session> {
        let project_id = self.project().to_string();
        let store = &self.engine.store;
        if store.get_project(&project_id)?.is_none() {
            let now = id::now_ms();
            store.upsert_project(&Project {
                id: project_id.clone(),
                worktree: self.worktree.as_ref().map_or_else(|| "/".to_string(), |w| w.to_string_lossy().into_owned()),
                vcs: self.worktree.as_ref().map(|_| Vcs::Git),
                name: None,
                icon: None,
                commands: None,
                time: ProjectTime { created: now, updated: now, initialized: None },
                sandboxes: Vec::new(),
            })?;
        }
        let session = Session::new(NewSession {
            project_id,
            directory: self.directory.to_string_lossy().into_owned(),
            version: opencode_session::VERSION.to_string(),
            ..Default::default()
        })?;
        store.create_session(&session)?;
        Ok(session)
    }

    async fn load(&self, session_id: &str) -> anyhow::Result<Loaded> {
        let store = &self.engine.store;
        let info = store.get_session(session_id)?.ok_or_else(|| anyhow::anyhow!("Session not found: {session_id}"))?;
        let children = self.children(&info);
        Ok(Loaded { messages: store.list_messages(session_id)?, todos: store.get_todos(session_id)?, children, status: self.engine.status(session_id), info })
    }

    async fn delete_session(&self, session_id: &str) -> anyhow::Result<()> {
        self.engine.abort(session_id);
        // Rust build: its background shells end with it.
        opencode_tools::bash::background::kill_session(session_id);
        self.engine.store.delete_session(session_id)?;
        Ok(())
    }

    async fn rename_session(&self, session_id: &str, title: &str) -> anyhow::Result<()> {
        self.engine.store.patch_session(session_id, |s| s.title = title.to_string())?;
        Ok(())
    }

    async fn fork(&self, session_id: &str, message_id: Option<&str>) -> anyhow::Result<Session> {
        Ok(self.engine.fork(session_id, message_id)?)
    }

    async fn run(&self, turn: Turn, cancel: CancellationToken) -> Result<(), String> {
        let host = self.host.clone() as Arc<dyn Host>;
        let directory = self.directory.clone();
        match turn {
            Turn::Prompt { session_id, text, files, agent, model, variant } => {
                let input = PromptInput { directory, session_id: Some(session_id), text, model, agent: Some(agent), files, variant, ..Default::default() };
                outcome(self.engine.prompt(input, host, cancel).await)
            }
            Turn::Queued { session_id, text, files, agent, model, variant } => {
                let input = PromptInput {
                    directory,
                    session_id: Some(session_id),
                    text,
                    model,
                    agent: Some(agent),
                    files,
                    variant,
                    delivery: opencode_session::Delivery::Queue,
                    ..Default::default()
                };
                outcome(self.engine.prompt(input, host, cancel).await)
            }
            Turn::Shell { session_id, command, agent, model } => outcome(self.engine.shell(ShellInput { session_id, agent, model, command, message_id: None }, host, cancel).await),
            Turn::Command { session_id, command, arguments, agent, model } => {
                let input = CommandInput { directory, session_id: Some(session_id), command, arguments, agent: Some(agent), model, ..Default::default() };
                outcome(self.engine.command(input, host, cancel).await)
            }
            Turn::Compact { session_id, .. } => outcome(self.engine.summarize(&session_id, false, host, cancel).await),
        }
    }

    async fn revert(&self, session_id: &str, message_id: &str) -> Result<(), String> {
        self.engine.revert_files(session_id, message_id, None).await.map(drop).map_err(|error| error.to_string())
    }

    async fn unrevert(&self, session_id: &str) -> Result<(), String> {
        self.engine.unrevert_files(session_id).await.map(drop).map_err(|error| error.to_string())
    }

    fn abort(&self, session_id: &str) {
        self.engine.abort(session_id);
    }

    async fn aside(&self, session_id: &str, prompt: &str) -> anyhow::Result<String> {
        Ok(self.engine.generate_in_session(&self.directory, session_id, prompt).await?)
    }

    async fn reload(&self) -> anyhow::Result<crate::catalog::Catalog> {
        self.engine.dispose(&self.directory).await;
        Ok(crate::catalog::load(&self.engine.paths, &self.directory))
    }

    async fn stats(&self) -> anyhow::Result<String> {
        crate::stats::render(&self.engine.store)
    }

    fn reply_permission(&self, id: u64, answer: PermissionAnswer) {
        if let Some(waiter) = lock(&self.host.waiting).remove(&id) {
            let _ = waiter.send(answer);
        }
    }

    fn reply_question(&self, id: u64, answers: Option<Vec<Vec<String>>>) {
        if let Some(waiter) = lock(&self.host.asking).remove(&id) {
            let _ = waiter.send(answers);
        }
    }

    fn release(&self) {
        lock(&self.host.waiting).clear();
        lock(&self.host.asking).clear();
    }
}

// What the event pump and the turn runners share.
struct Shared {
    tx: UnboundedSender<Action>,
    next: AtomicU64,
    // UI id -> server request id, and back.
    permissions: Mutex<HashMap<u64, String>>,
    questions: Mutex<HashMap<u64, String>>,
    // How many times each session went idle, so a finished request can wait for the stream to catch up.
    idle: Mutex<HashMap<String, u64>>,
    idle_changed: tokio::sync::Notify,
    // The last `session.error` message per session, for the toast of a failed request.
    errors: Mutex<HashMap<String, String>>,
}

impl Shared {
    fn idle_count(&self, session_id: &str) -> u64 {
        lock(&self.idle).get(session_id).copied().unwrap_or(0)
    }

    async fn wait_idle(&self, session_id: &str, seen: u64, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let changed = self.idle_changed.notified();
            if self.idle_count(session_id) > seen {
                return;
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                return;
            }
        }
    }

    fn send(&self, actions: Vec<Action>) {
        for action in actions {
            let _ = self.tx.send(action);
        }
    }
}

// An opencode server over HTTP: requests through the client, everything else from its `/event` stream.
pub struct RemoteBackend {
    pub client: Client,
    shared: Arc<Shared>,
    pump: tokio::task::JoinHandle<()>,
}

impl Drop for RemoteBackend {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

fn status(status: SessionStatus) -> Status {
    match status {
        SessionStatus::Idle => Status::Idle,
        SessionStatus::Busy => Status::Busy,
        SessionStatus::Retry { attempt, message, next } => Status::Retry { attempt, message, next },
    }
}

fn model_ref(model: Option<String>) -> Option<ModelRef> {
    model.as_deref().and_then(ModelRef::parse)
}

fn messages(list: Vec<opencode_storage::MessageWithParts>) -> Vec<(Message, Vec<Part>)> {
    list.into_iter().map(|m| (m.info, m.parts)).collect()
}

async fn remote_children(client: &Client, info: &Session, statuses: &indexmap::IndexMap<String, SessionStatus>) -> Vec<Child> {
    let nested = info.parent_id.is_none();
    let mut queue = vec![info.parent_id.clone().unwrap_or_else(|| info.id.clone())];
    let mut out = Vec::new();
    while let Some(parent) = queue.pop() {
        for entry in client.children(&parent).await.unwrap_or_default() {
            let list = messages(client.messages(&entry.id, None).await.unwrap_or_default());
            if nested {
                queue.push(entry.id.clone());
            }
            let state = statuses.get(&entry.id).cloned().map_or(Status::Idle, status);
            out.push(child(entry, &list, state));
        }
    }
    out
}

async fn remote_load(client: &Client, session_id: &str) -> anyhow::Result<Loaded> {
    let (info, list, todos, statuses) = tokio::join!(client.session(session_id), client.messages(session_id, None), client.todos(session_id), client.status());
    let info = info?;
    let statuses = statuses.unwrap_or_default();
    let children = remote_children(client, &info, &statuses).await;
    let state = statuses.get(session_id).cloned().map_or(Status::Idle, status);
    Ok(Loaded { info, messages: messages(list?), todos: todos.unwrap_or_default(), children, status: state })
}

impl RemoteBackend {
    // Subscribes before anything else is sent, so no event of a later request is missed.
    pub async fn connect(client: Client, tx: UnboundedSender<Action>) -> anyhow::Result<Self> {
        let events = client.events().await?;
        let shared = Arc::new(Shared {
            tx,
            next: AtomicU64::new(1),
            permissions: Mutex::default(),
            questions: Mutex::default(),
            idle: Mutex::default(),
            idle_changed: tokio::sync::Notify::new(),
            errors: Mutex::default(),
        });
        let pump = tokio::spawn(pump(events, client.clone(), shared.clone()));
        Ok(Self { client, shared, pump })
    }

    fn spawn_call<F>(&self, call: F)
    where
        F: std::future::Future<Output = opencode_client::Result<bool>> + Send + 'static,
    {
        let tx = self.shared.tx.clone();
        tokio::spawn(async move {
            if let Err(error) = call.await {
                let _ = tx.send(Action::Toast(error.to_string(), ToastKind::Error));
            }
        });
    }
}

#[async_trait::async_trait]
impl Backend for RemoteBackend {
    async fn sessions(&self) -> anyhow::Result<Vec<Session>> {
        Ok(newest_roots(self.client.sessions(&SessionQuery { roots: Some(true), ..Default::default() }).await?))
    }

    async fn create_session(&self) -> anyhow::Result<Session> {
        Ok(self.client.create_session(&RemoteCreate::default()).await?)
    }

    async fn load(&self, session_id: &str) -> anyhow::Result<Loaded> {
        remote_load(&self.client, session_id).await
    }

    async fn delete_session(&self, session_id: &str) -> anyhow::Result<()> {
        self.client.delete_session(session_id).await?;
        Ok(())
    }

    async fn rename_session(&self, session_id: &str, title: &str) -> anyhow::Result<()> {
        self.client.update_session(session_id, &SessionPatch { title: Some(title.to_string()) }).await?;
        Ok(())
    }

    async fn fork(&self, session_id: &str, message_id: Option<&str>) -> anyhow::Result<Session> {
        Ok(self.client.fork(session_id, message_id).await?)
    }

    // The request answers when the turn is over; the stream's last events (the final parts, idle) can still be
    // in flight, so the run ends once the session's idle has come through too.
    async fn run(&self, turn: Turn, _cancel: CancellationToken) -> Result<(), String> {
        let session_id = turn.session_id().to_string();
        let seen = self.shared.idle_count(&session_id);
        lock(&self.shared.errors).remove(&session_id);
        let client = &self.client;
        let result = match turn {
            // Rust build: the 1.x HTTP API has no queue delivery; a queued prompt is sent as a plain one.
            Turn::Prompt { session_id, text, files, agent, model, variant } | Turn::Queued { session_id, text, files, agent, model, variant } => {
                let mut parts = vec![PromptPart::Text { text }];
                parts.extend(files.into_iter().map(PromptPart::File));
                let body = PromptBody { model: model_ref(model), agent: Some(agent), variant, parts, ..Default::default() };
                client.prompt(&session_id, &body).await.map(drop)
            }
            Turn::Shell { session_id, command, agent, model } => client.shell(&session_id, &ShellBody { agent, model: model_ref(model), command, message_id: None }).await.map(drop),
            Turn::Command { session_id, command, arguments, agent, model } => {
                client.command(&session_id, &CommandBody { agent: Some(agent), model, arguments, command, ..Default::default() }).await.map(drop)
            }
            Turn::Compact { session_id, model } => match model_ref(model) {
                Some(model) => client.summarize(&session_id, &model.provider_id, &model.model_id, false).await.map(drop),
                None => return Err("No model selected".to_string()),
            },
        };
        self.shared.wait_idle(&session_id, seen, Duration::from_secs(2)).await;
        result.map_err(|error| lock(&self.shared.errors).remove(&session_id).unwrap_or_else(|| error.to_string()))
    }

    async fn revert(&self, session_id: &str, message_id: &str) -> Result<(), String> {
        self.client.revert(session_id, message_id, None).await.map(drop).map_err(|error| error.to_string())
    }

    async fn unrevert(&self, session_id: &str) -> Result<(), String> {
        self.client.unrevert(session_id).await.map(drop).map_err(|error| error.to_string())
    }

    fn abort(&self, session_id: &str) {
        let (client, id) = (self.client.clone(), session_id.to_string());
        self.spawn_call(async move { client.abort(&id).await });
    }

    fn reply_permission(&self, id: u64, answer: PermissionAnswer) {
        let Some(request_id) = lock(&self.shared.permissions).remove(&id) else { return };
        let client = self.client.clone();
        self.spawn_call(async move {
            match answer {
                PermissionAnswer::Once => client.reply_permission(&request_id, PermissionReply::Once, None).await,
                PermissionAnswer::Always => client.reply_permission(&request_id, PermissionReply::Always, None).await,
                PermissionAnswer::Reject => client.reply_permission(&request_id, PermissionReply::Reject, None).await,
                PermissionAnswer::Corrected(message) => client.reply_permission(&request_id, PermissionReply::Reject, Some(&message)).await,
            }
        });
    }

    fn reply_question(&self, id: u64, answers: Option<Vec<Vec<String>>>) {
        let Some(request_id) = lock(&self.shared.questions).remove(&id) else { return };
        let client = self.client.clone();
        self.spawn_call(async move {
            match answers {
                Some(answers) => client.reply_question(&request_id, &answers).await,
                None => client.reject_question(&request_id).await,
            }
        });
    }

    // Requests stay open on the server for other clients; nothing to release here.
    fn release(&self) {}
}

// Turns server events into the reducer's actions.
#[derive(Default)]
struct Translator {
    connected: bool,
    // Part id -> "text" | "reasoning", for the deltas that follow.
    kinds: HashMap<String, &'static str>,
    // Session id -> parent id; subagent updates are wrapped the way the engine wraps them.
    parents: HashMap<String, Option<String>>,
    titles: HashMap<String, String>,
}

impl Translator {
    fn wrap(&self, session_id: &str, update: Update) -> Action {
        match self.parents.get(session_id) {
            Some(Some(_)) => Action::Engine(Update::Child { session_id: session_id.to_string(), update: Box::new(update) }),
            _ => Action::Engine(update),
        }
    }

    fn permission(&self, shared: &Shared, request: opencode_client::PermissionRequest) -> Option<Action> {
        let mut known = lock(&shared.permissions);
        if known.values().any(|id| *id == request.id) {
            return None;
        }
        let id = shared.next.fetch_add(1, Ordering::SeqCst);
        known.insert(id, request.id);
        let metadata = if request.metadata.is_object() { request.metadata } else { Value::Object(Default::default()) };
        let request_info = PermissionRequest { permission: request.permission, patterns: request.patterns, always: request.always, metadata };
        Some(Action::PermissionAsked(PermissionAsk { id, session_id: request.session_id, request: request_info }))
    }

    fn question(&self, shared: &Shared, request: opencode_client::QuestionRequest) -> Option<Action> {
        let mut known = lock(&shared.questions);
        if known.values().any(|id| *id == request.id) {
            return None;
        }
        let id = shared.next.fetch_add(1, Ordering::SeqCst);
        known.insert(id, request.id.clone());
        let questions = request
            .questions
            .into_iter()
            .map(|q| QuestionInfo {
                question: q.question,
                header: q.header,
                options: q.options.into_iter().map(|o| QuestionOption { label: o.label, description: o.description }).collect(),
                multiple: q.multiple,
                custom: q.custom,
            })
            .collect();
        let tool = request.tool.map(|tool| (tool.message_id, tool.call_id));
        Some(Action::QuestionAsked(QuestionAsk { id, request: QuestionRequest { id: request.id, session_id: request.session_id, questions, tool } }))
    }

    fn answered(map: &Mutex<HashMap<u64, String>>, request_id: &str) -> Option<Action> {
        let mut known = lock(map);
        let id = known.iter().find(|(_, other)| *other == request_id).map(|(id, _)| *id)?;
        known.remove(&id);
        Some(Action::Answered(id))
    }

    fn translate(&mut self, shared: &Shared, event: Event) -> Vec<Action> {
        match event {
            Event::ServerConnected if self.connected => vec![Action::Resync],
            Event::ServerConnected => {
                self.connected = true;
                Vec::new()
            }
            Event::SessionCreated(info) => {
                self.parents.insert(info.id.clone(), info.parent_id.clone());
                self.titles.insert(info.id.clone(), info.title.clone());
                Vec::new()
            }
            Event::SessionUpdated(info) => {
                self.parents.insert(info.id.clone(), info.parent_id.clone());
                if self.titles.get(&info.id) == Some(&info.title) {
                    return Vec::new();
                }
                self.titles.insert(info.id.clone(), info.title.clone());
                vec![self.wrap(&info.id, Update::Title { session_id: info.id.clone(), title: info.title.clone() })]
            }
            Event::SessionDeleted(info) => vec![Action::SessionDeleted(info.id)],
            Event::SessionStatus { session_id, status: state } => vec![self.wrap(&session_id, Update::Status { session_id: session_id.clone(), status: status(state) })],
            Event::SessionCompacted { session_id } => vec![self.wrap(&session_id, Update::Compacted { session_id: session_id.clone() })],
            Event::TodoUpdated { session_id, todos } => vec![self.wrap(&session_id, Update::Todos { session_id: session_id.clone(), todos })],
            Event::SessionError { session_id: Some(session_id), error: Some(error) } => {
                let name = error.get("name").and_then(Value::as_str).unwrap_or_default();
                if name == "MessageAbortedError" {
                    return Vec::new();
                }
                let message = error.pointer("/data/message").and_then(Value::as_str).filter(|m| !m.is_empty()).unwrap_or(name).to_string();
                lock(&shared.errors).insert(session_id.clone(), message.clone());
                vec![self.wrap(&session_id, Update::Error { session_id: session_id.clone(), message })]
            }
            Event::PartUpdated(part) => {
                let session_id = part.session_id.clone();
                match &part.body {
                    PartBody::Text(_) => drop(self.kinds.insert(part.id.clone(), "text")),
                    PartBody::Reasoning(_) => drop(self.kinds.insert(part.id.clone(), "reasoning")),
                    PartBody::Tool(_) => return vec![self.wrap(&session_id, Update::Tool(part))],
                    PartBody::StepFinish(step) => return vec![self.wrap(&session_id, Update::StepFinish { reason: step.reason.clone(), tokens: step.tokens })],
                    _ => {}
                }
                Vec::new()
            }
            Event::PartDelta { session_id, message_id, part_id, field, delta } if field == "text" => {
                let marker = match self.kinds.get(&part_id) {
                    Some(&"reasoning") => Update::Reasoning(delta.clone()),
                    _ => Update::Text(delta.clone()),
                };
                vec![self.wrap(&session_id, marker), self.wrap(&session_id, Update::Delta { session_id: session_id.clone(), message_id, part_id, field: "text", delta })]
            }
            Event::PermissionAsked(request) => self.permission(shared, request).into_iter().collect(),
            Event::PermissionReplied { request_id, .. } => Self::answered(&shared.permissions, &request_id).into_iter().collect(),
            Event::QuestionAsked(request) => self.question(shared, request).into_iter().collect(),
            Event::QuestionReplied { request_id, .. } | Event::QuestionRejected { request_id, .. } => Self::answered(&shared.questions, &request_id).into_iter().collect(),
            Event::TuiToastShow { message, variant, .. } => {
                let kind = match variant.as_str() {
                    "error" => ToastKind::Error,
                    "warning" => ToastKind::Warning,
                    _ => ToastKind::Info,
                };
                vec![Action::Toast(message, kind)]
            }
            _ => Vec::new(),
        }
    }
}

// Requests already waiting when the stream (re)connects are shown too.
async fn pending(client: &Client, shared: &Shared, translator: &Translator) {
    let (permissions, questions) = tokio::join!(client.permissions(), client.questions());
    let mut actions: Vec<Action> = permissions.unwrap_or_default().into_iter().filter_map(|request| translator.permission(shared, request)).collect();
    actions.extend(questions.unwrap_or_default().into_iter().filter_map(|request| translator.question(shared, request)));
    shared.send(actions);
}

async fn pump(mut events: EventStream, client: Client, shared: Arc<Shared>) {
    let mut translator = Translator::default();
    while let Some(frame) = events.next().await {
        let idle = match &frame.event {
            Event::SessionStatus { session_id, status: SessionStatus::Idle } => Some(session_id.clone()),
            _ => None,
        };
        let connected = matches!(frame.event, Event::ServerConnected);
        // `tui.session.select` from the server's control routes opens that session.
        if let Event::TuiSessionSelect { session_id } = &frame.event {
            let (client, tx, id) = (client.clone(), shared.tx.clone(), session_id.clone());
            tokio::spawn(async move {
                let actions = match remote_load(&client, &id).await {
                    Ok(loaded) => loaded.actions(true, true),
                    Err(error) => vec![Action::Toast(error.to_string(), ToastKind::Error)],
                };
                for action in actions {
                    let _ = tx.send(action);
                }
            });
        }
        let actions = translator.translate(&shared, frame.event);
        shared.send(actions);
        if connected {
            pending(&client, &shared, &translator).await;
        }
        if let Some(session_id) = idle {
            *lock(&shared.idle).entry(session_id).or_default() += 1;
            shared.idle_changed.notify_waiters();
        }
    }
}
