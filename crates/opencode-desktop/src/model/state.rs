// Application state and the reducer for backend events. No GPUI, no I/O.
use super::attention::Signal;
use super::question::QuestionView;
use super::transcript::{Live, Row, RowKind};
use opencode_storage::{Part, Todo};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const NEW_SESSION_TITLE: &str = "New session";

#[derive(Debug, Clone, PartialEq)]
pub struct SessionItem {
    pub id: Arc<str>,
    pub title: Arc<str>,
    pub updated: u64,
    // Set for subagent sessions.
    pub parent: Option<Arc<str>>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelItem {
    // "provider/model", the form the engine accepts.
    pub spec: Arc<str>,
    // Without the "(latest)" marker, which `latest` records.
    pub name: Arc<str>,
    // Context window in tokens; 0 when the catalog does not say.
    pub context: u64,
    pub provider_id: Arc<str>,
    pub provider: Arc<str>,
    // OpenCode's own models without an input price (`model.tag.free`).
    pub free: bool,
    pub latest: bool,
    // Listed in the picker unless the user says otherwise (`models.tsx` `visible`).
    pub shown: bool,
    // Names of the model's reasoning variants.
    pub variants: Vec<Arc<str>>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelGroup {
    pub provider: Arc<str>,
    pub id: Arc<str>,
    pub models: Vec<ModelItem>,
}

// Where a connected provider's credentials come from (`settings-providers.tsx` `type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderSource {
    Api,
    Env,
    Config,
    Custom,
    Other,
}

impl ProviderSource {
    pub fn key(self) -> &'static str {
        match self {
            Self::Api => "provider.connect.method.apiKey",
            Self::Env => "settings.providers.tag.environment",
            Self::Config => "settings.providers.tag.config",
            Self::Custom => "settings.providers.tag.custom",
            Self::Other => "settings.providers.tag.other",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConnectedProvider {
    pub id: Arc<str>,
    pub name: Arc<str>,
    pub source: ProviderSource,
    // Its credential is an entry of auth.json this client may remove.
    pub removable: bool,
}

// A provider of the models catalog, connected or not.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderEntry {
    pub id: Arc<str>,
    pub name: Arc<str>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentItem {
    pub name: Arc<str>,
    pub description: Arc<str>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog {
    pub models: Vec<ModelGroup>,
    pub agents: Vec<AgentItem>,
    pub default_model: Option<String>,
    pub default_agent: Option<String>,
    // Config or registry problems worth showing instead of an empty picker.
    pub warning: Option<String>,
    pub connected: Vec<ConnectedProvider>,
    // Every provider of the catalog that may be connected, by name.
    pub providers: Vec<ProviderEntry>,
}

impl Catalog {
    pub fn model(&self, spec: &str) -> Option<&ModelItem> {
        self.models.iter().flat_map(|g| &g.models).find(|m| m.spec.as_ref() == spec)
    }

    pub fn provider_name(&self, id: &str) -> Option<Arc<str>> {
        self.providers.iter().find(|p| p.id.as_ref() == id).map(|p| p.name.clone()).or_else(|| self.models.iter().find(|g| g.id.as_ref() == id).map(|g| g.provider.clone()))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CommandItem {
    pub name: Arc<str>,
    pub description: Arc<str>,
    // i18n key of the origin badge (custom / mcp / skill).
    pub badge: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PermissionView {
    pub id: u64,
    pub session_id: Arc<str>,
    pub permission: Arc<str>,
    pub patterns: Vec<String>,
    pub detail: Option<Arc<str>>,
    pub diff: Option<Arc<str>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Done,
    Aborted,
    Failed(String),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum RunStatus {
    #[default]
    Idle,
    Busy,
    Retry {
        attempt: u32,
        message: String,
        next: u64,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    pub cost: f64,
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    // Tokens the last finished step occupied in the context window.
    pub context: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.reasoning + self.cache_read + self.cache_write
    }
}

// What a subagent session is doing, shown inside the parent's `task` card.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChildActivity {
    pub title: Option<Arc<str>>,
    pub last: Arc<str>,
    pub tools: usize,
    pub running: bool,
    text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChildUpdate {
    Text(String),
    // `started` is true once per tool call.
    Tool { label: String, started: bool },
    Running(bool),
    Title(String),
}

#[derive(Debug, Clone)]
pub enum Event {
    Sessions { directory: PathBuf, sessions: Vec<SessionItem> },
    Catalog { directory: PathBuf, catalog: Catalog },
    Commands { directory: PathBuf, commands: Vec<CommandItem> },
    Files { directory: PathBuf, files: Vec<String> },
    Opened { session_id: String, rows: Vec<Row>, model: Option<String>, agent: Option<String> },
    // The store now holds everything streamed so far.
    Snapshot { session_id: String, rows: Vec<Row> },
    Text { session_id: String, delta: String },
    Reasoning { session_id: String, delta: String },
    Tool { session_id: String, part: Box<Part> },
    Retry { session_id: String, attempt: u32, message: String, delay_ms: u64 },
    Title { session_id: String, title: String },
    Status { session_id: String, status: RunStatus },
    Todos { session_id: String, todos: Vec<Todo> },
    Usage { session_id: String, usage: Usage },
    // A failure recorded on the session while its prompt is still running.
    Error { session_id: String, message: String },
    Child { child_id: String, update: ChildUpdate },
    // How many user messages a pending revert hides; 0 clears it.
    Revert { session_id: String, hidden: usize },
    Forked { directory: PathBuf, session: SessionItem },
    Deleted { session_id: String },
    Permission(PermissionView),
    PermissionResolved(u64),
    Question(QuestionView),
    QuestionResolved(u64),
    Finished { session_id: String, rows: Option<Vec<Row>>, outcome: Outcome },
    // `what` is the i18n key of a message with an `{{error}}` placeholder.
    Failed { what: &'static str, error: String },
    // Something the user may want a sound or a system notification for; the view decides.
    Attention(Signal),
    // Progress of a provider login started from the connect dialog.
    Login { provider_id: String, step: LoginStep },
}

#[derive(Debug, Clone, PartialEq)]
pub enum LoginStep {
    // Open `url`, and enter `code` there when there is one.
    Waiting { url: String, code: Option<String> },
    Done,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub key: &'static str,
    pub error: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Run {
    // Rows as of the last snapshot; `live` holds what arrived since.
    base: Vec<Row>,
    live: Live,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SidebarRow {
    pub session: SessionItem,
    pub depth: usize,
}

#[derive(Debug, Clone, Default)]
pub struct AppState {
    pub directory: PathBuf,
    pub sessions: Vec<SessionItem>,
    // None is an unsent draft.
    pub current: Option<String>,
    pub loading: bool,
    pub catalog: Catalog,
    pub commands: Vec<CommandItem>,
    // Project files relative to the directory, `/`-separated.
    pub files: Arc<Vec<String>>,
    pub model: Option<String>,
    pub agent: Option<String>,
    pub permissions: Vec<PermissionView>,
    pub questions: Vec<QuestionView>,
    // Case-insensitive session title filter of the sidebar.
    pub filter: String,
    // App-level problem (project could not be loaded, ...), shown until dismissed.
    pub notice: Option<Notice>,
    rows: Vec<Row>,
    runs: HashMap<String, Run>,
    errors: HashMap<String, String>,
    todos: HashMap<String, Vec<Todo>>,
    usage: HashMap<String, Usage>,
    status: HashMap<String, RunStatus>,
    reverts: HashMap<String, usize>,
    children: HashMap<String, ChildActivity>,
}

pub fn project_name(directory: &Path) -> String {
    directory.file_name().map_or_else(|| directory.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned())
}

fn tail(text: &str, max: usize) -> &str {
    let line = text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    match line.char_indices().rev().nth(max) {
        Some((cut, _)) => &line[cut..],
        None => line,
    }
}

impl AppState {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory, ..Default::default() }
    }

    pub fn project_name(&self) -> String {
        project_name(&self.directory)
    }

    pub fn window_title(&self) -> String {
        format!("{} — opencode", self.project_name())
    }

    // Runs of the old project keep going; their events are dropped by session id once it is gone from view.
    pub fn open_project(&mut self, directory: PathBuf) {
        let runs = std::mem::take(&mut self.runs);
        *self = Self::new(directory);
        self.runs = runs;
    }

    pub fn select(&mut self, session_id: &str) {
        if self.current.as_deref() == Some(session_id) {
            return;
        }
        self.current = Some(session_id.to_string());
        self.rows.clear();
        self.loading = !self.runs.contains_key(session_id);
    }

    pub fn new_session(&mut self) {
        self.current = None;
        self.rows.clear();
        self.loading = false;
    }

    // `session_id` is the current session, or a fresh id when the draft is being sent.
    pub fn begin_prompt(&mut self, session_id: &str, text: &str, files: Vec<Arc<str>>, now_ms: u64) {
        // Rust build: a prompt sent while the session runs steers that run; it joins the live transcript.
        if let Some(run) = self.runs.get_mut(session_id) {
            run.live.user(text, files);
            return;
        }
        if self.current.is_none() {
            self.current = Some(session_id.to_string());
            self.sessions.insert(0, SessionItem { id: session_id.into(), title: first_line(text).into(), updated: now_ms, parent: None });
        }
        self.errors.remove(session_id);
        self.reverts.remove(session_id);
        self.loading = false;
        let mut run = Run { base: std::mem::take(&mut self.rows), live: Live::default() };
        run.live.user(text, files);
        self.runs.insert(session_id.to_string(), run);
        self.status.insert(session_id.to_string(), RunStatus::Busy);
    }

    pub fn is_running(&self, session_id: &str) -> bool {
        self.runs.contains_key(session_id)
    }

    pub fn current_running(&self) -> bool {
        self.current.as_deref().is_some_and(|id| self.is_running(id))
    }

    fn session(&self, id: &str) -> Option<&SessionItem> {
        self.sessions.iter().find(|s| s.id.as_ref() == id)
    }

    pub fn current_session(&self) -> Option<&SessionItem> {
        self.session(self.current.as_deref()?)
    }

    pub fn current_title(&self) -> Arc<str> {
        self.current_session().map_or_else(|| NEW_SESSION_TITLE.into(), |s| s.title.clone())
    }

    // The parent of the current session when it is a subagent session.
    pub fn current_parent(&self) -> Option<Arc<str>> {
        self.current_session()?.parent.clone()
    }

    pub fn effective_model(&self) -> Option<&str> {
        self.model.as_deref().or(self.catalog.default_model.as_deref())
    }

    pub fn effective_agent(&self) -> Option<&str> {
        self.agent.as_deref().or(self.catalog.default_agent.as_deref())
    }

    pub fn model_label(&self) -> Option<Arc<str>> {
        let spec = self.effective_model()?;
        Some(self.catalog.model(spec).map_or_else(|| spec.into(), |m| m.name.clone()))
    }

    // What the composer's model chip reads: "Provider / Model".
    pub fn chip_label(&self) -> Option<String> {
        let spec = self.effective_model()?;
        Some(self.catalog.model(spec).map_or_else(|| spec.to_string(), |m| super::models::chip_label(&m.provider_id, &m.provider, &m.name)))
    }

    // The agent `delta` places after the effective one, wrapping around.
    pub fn cycle_agent(&mut self, delta: isize) -> Option<Arc<str>> {
        let count = self.catalog.agents.len() as isize;
        if count == 0 {
            return None;
        }
        let at = self.effective_agent().and_then(|name| self.catalog.agents.iter().position(|a| a.name.as_ref() == name)).unwrap_or(0) as isize;
        let next = self.catalog.agents[(at + delta).rem_euclid(count) as usize].name.clone();
        self.agent = Some(next.to_string());
        Some(next)
    }

    pub fn current_permission(&self) -> Option<&PermissionView> {
        let current = self.current.as_deref()?;
        self.permissions.iter().find(|p| p.session_id.as_ref() == current)
    }

    pub fn current_question(&self) -> Option<&QuestionView> {
        let current = self.current.as_deref()?;
        self.questions.iter().find(|q| q.session_id.as_ref() == current)
    }

    // Waiting on the user: a permission or a question.
    pub fn waiting(&self, session_id: &str) -> bool {
        self.permissions.iter().any(|p| p.session_id.as_ref() == session_id) || self.questions.iter().any(|q| q.session_id.as_ref() == session_id)
    }

    pub fn current_todos(&self) -> &[Todo] {
        self.current.as_deref().and_then(|id| self.todos.get(id)).map_or(&[], Vec::as_slice)
    }

    pub fn current_usage(&self) -> Option<Usage> {
        self.usage.get(self.current.as_deref()?).copied().filter(|u| u.total() > 0 || u.cost > 0.0)
    }

    // Share of the model's context window the session occupies, in percent.
    pub fn context_percent(&self) -> Option<u32> {
        let usage = self.current_usage()?;
        let limit = self.catalog.model(self.effective_model()?)?.context;
        (limit > 0 && usage.context > 0).then(|| (usage.context as f64 * 100.0 / limit as f64).round().min(100.0) as u32)
    }

    pub fn current_status(&self) -> RunStatus {
        self.current.as_deref().and_then(|id| self.status.get(id)).cloned().unwrap_or_default()
    }

    pub fn current_revert(&self) -> usize {
        self.current.as_deref().and_then(|id| self.reverts.get(id)).copied().unwrap_or(0)
    }

    pub fn child(&self, id: &str) -> Option<&ChildActivity> {
        self.children.get(id)
    }

    // Sessions as the sidebar lists them: filtered roots in listing order, each followed by its subagent sessions.
    pub fn sidebar(&self) -> Vec<SidebarRow> {
        let needle = self.filter.trim().to_lowercase();
        let hit = |s: &SessionItem| needle.is_empty() || s.title.to_lowercase().contains(&needle);
        let mut by_parent: HashMap<&str, Vec<&SessionItem>> = HashMap::new();
        for child in &self.sessions {
            if let Some(parent) = child.parent.as_deref() {
                by_parent.entry(parent).or_default().push(child);
            }
        }
        let mut rows = Vec::new();
        for root in self.sessions.iter().filter(|s| s.parent.is_none()) {
            let children = by_parent.remove(root.id.as_ref()).unwrap_or_default();
            let root_hit = hit(root);
            if !root_hit && !children.iter().any(|c| hit(c)) {
                continue;
            }
            rows.push(SidebarRow { session: root.clone(), depth: 0 });
            rows.extend(children.into_iter().filter(|c| root_hit || hit(c)).map(|c| SidebarRow { session: c.clone(), depth: 1 }));
        }
        rows
    }

    // The session `delta` rows away from the current one in the sidebar.
    pub fn neighbor(&self, delta: isize) -> Option<Arc<str>> {
        let rows = self.sidebar();
        if rows.is_empty() {
            return None;
        }
        let at = self.current.as_deref().and_then(|id| rows.iter().position(|r| r.session.id.as_ref() == id));
        let next = match at {
            Some(at) => (at as isize + delta).clamp(0, rows.len() as isize - 1) as usize,
            None if delta > 0 => 0,
            None => rows.len() - 1,
        };
        (Some(next) != at).then(|| rows[next].session.id.clone())
    }

    // The newest user message of the current session, for `/undo`.
    pub fn last_user_message(&self) -> Option<Arc<str>> {
        self.rows.iter().rev().find(|row| matches!(row.kind, RowKind::User { .. }) && row.id.starts_with("msg")).map(|row| row.id.clone())
    }

    // The rows the transcript shows for the current session.
    pub fn visible(&self) -> Vec<Row> {
        let Some(current) = self.current.as_deref() else {
            return Vec::new();
        };
        let mut rows = match self.runs.get(current) {
            Some(run) => {
                let mut rows = run.base.clone();
                rows.extend(run.live.rows());
                rows
            }
            None => self.rows.clone(),
        };
        if let Some(error) = self.errors.get(current) {
            rows.push(Row::error("session-error", error));
        }
        rows
    }

    fn drop_session(&mut self, session_id: &str) {
        self.sessions.retain(|s| s.id.as_ref() != session_id && s.parent.as_deref() != Some(session_id));
        self.permissions.retain(|p| p.session_id.as_ref() != session_id);
        self.questions.retain(|q| q.session_id.as_ref() != session_id);
        for map in [&mut self.errors as &mut dyn Forget, &mut self.todos, &mut self.usage, &mut self.status, &mut self.reverts, &mut self.runs] {
            map.forget(session_id);
        }
        if self.current.as_deref() == Some(session_id) {
            self.new_session();
        }
    }

    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Sessions { directory, mut sessions } => {
                if directory == self.directory {
                    // A listing taken before a just-started session reached the store must not drop it.
                    let unlisted = |item: &&SessionItem| self.runs.contains_key(item.id.as_ref()) && !sessions.iter().any(|s| s.id == item.id);
                    let running: Vec<SessionItem> = self.sessions.iter().filter(unlisted).cloned().collect();
                    sessions.splice(0..0, running);
                    self.sessions = sessions;
                }
            }
            Event::Catalog { directory, catalog } => {
                if directory == self.directory {
                    let known = |spec: &Option<String>| spec.as_deref().is_some_and(|s| catalog.model(s).is_some());
                    if !known(&self.model) {
                        self.model = None;
                    }
                    if !self.agent.as_deref().is_some_and(|a| catalog.agents.iter().any(|i| i.name.as_ref() == a)) {
                        self.agent = None;
                    }
                    self.catalog = catalog;
                }
            }
            Event::Commands { directory, commands } => {
                if directory == self.directory {
                    self.commands = commands;
                }
            }
            Event::Files { directory, files } => {
                if directory == self.directory {
                    self.files = Arc::new(files);
                }
            }
            Event::Opened { session_id, rows, model, agent } => {
                if self.current.as_deref() != Some(session_id.as_str()) {
                    return;
                }
                self.loading = false;
                if !self.runs.contains_key(&session_id) {
                    self.rows = rows;
                }
                if let Some(model) = model.filter(|m| self.catalog.model(m).is_some()) {
                    self.model = Some(model);
                }
                if let Some(agent) = agent.filter(|a| self.catalog.agents.iter().any(|i| i.name.as_ref() == a)) {
                    self.agent = Some(agent);
                }
            }
            Event::Snapshot { session_id, rows } => match self.runs.get_mut(&session_id) {
                Some(run) => {
                    run.base = rows;
                    run.live.settle();
                }
                None if self.current.as_deref() == Some(session_id.as_str()) => self.rows = rows,
                None => {}
            },
            Event::Text { session_id, delta } => {
                if let Some(run) = self.runs.get_mut(&session_id) {
                    run.live.text(&delta);
                }
            }
            Event::Reasoning { session_id, delta } => {
                if let Some(run) = self.runs.get_mut(&session_id) {
                    run.live.reasoning(&delta);
                }
            }
            Event::Tool { session_id, part } => {
                if let Some(run) = self.runs.get_mut(&session_id) {
                    run.live.tool(part);
                }
            }
            Event::Retry { session_id, attempt, message, delay_ms } => {
                if let Some(run) = self.runs.get_mut(&session_id) {
                    run.live.retry(attempt, &message, delay_ms);
                }
            }
            Event::Title { session_id, title } => {
                if let Some(session) = self.sessions.iter_mut().find(|s| s.id.as_ref() == session_id) {
                    session.title = title.into();
                }
            }
            Event::Status { session_id, status } => {
                self.status.insert(session_id, status);
            }
            Event::Todos { session_id, todos } => {
                self.todos.insert(session_id, todos);
            }
            Event::Usage { session_id, usage } => {
                self.usage.insert(session_id, usage);
            }
            Event::Error { session_id, message } => {
                if self.runs.contains_key(&session_id) {
                    self.errors.insert(session_id, message);
                }
            }
            Event::Child { child_id, update } => {
                let child = self.children.entry(child_id).or_default();
                match update {
                    ChildUpdate::Text(delta) => {
                        child.text.push_str(&delta);
                        child.last = tail(&child.text, 160).into();
                    }
                    ChildUpdate::Tool { label, started } => {
                        child.tools += usize::from(started);
                        child.text.clear();
                        child.last = label.into();
                    }
                    ChildUpdate::Running(running) => child.running = running,
                    ChildUpdate::Title(title) => child.title = Some(title.into()),
                }
            }
            Event::Revert { session_id, hidden } => {
                if hidden == 0 {
                    self.reverts.remove(&session_id);
                } else {
                    self.reverts.insert(session_id, hidden);
                }
            }
            Event::Forked { directory, session } => {
                if directory == self.directory && self.session(&session.id).is_none() {
                    self.sessions.insert(0, session);
                }
            }
            Event::Deleted { session_id } => self.drop_session(&session_id),
            Event::Permission(request) => self.permissions.push(request),
            Event::PermissionResolved(id) => self.permissions.retain(|p| p.id != id),
            Event::Question(request) => self.questions.push(request),
            Event::QuestionResolved(id) => self.questions.retain(|q| q.id != id),
            Event::Finished { session_id, rows, outcome } => {
                self.permissions.retain(|p| p.session_id.as_ref() != session_id);
                self.questions.retain(|q| q.session_id.as_ref() != session_id);
                self.status.remove(&session_id);
                let Some(run) = self.runs.remove(&session_id) else {
                    return;
                };
                let mut stored = rows.unwrap_or(run.base);
                // The engine may already have recorded the failure on the assistant message.
                let recorded = matches!(stored.last(), Some(Row { kind: RowKind::Error { .. }, .. }));
                let partial = run.live.unsaved(&stored);
                stored.extend(partial);
                match outcome {
                    _ if recorded => drop(self.errors.remove(&session_id)),
                    Outcome::Failed(message) => drop(self.errors.insert(session_id.clone(), message)),
                    Outcome::Done | Outcome::Aborted => {}
                }
                if self.current.as_deref() == Some(session_id.as_str()) {
                    self.rows = stored;
                }
            }
            Event::Failed { what, error } => {
                self.loading = false;
                self.notice = Some(Notice { key: what, error });
            }
            Event::Attention(_) | Event::Login { .. } => {}
        }
    }
}

trait Forget {
    fn forget(&mut self, key: &str);
}

impl<V> Forget for HashMap<String, V> {
    fn forget(&mut self, key: &str) {
        self.remove(key);
    }
}

fn first_line(text: &str) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or(NEW_SESSION_TITLE).trim();
    match line.char_indices().nth(80) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_row(id: &str, text: &str) -> Row {
        Row { id: id.into(), kind: RowKind::Text { markdown: text.into(), streaming: false } }
    }

    fn item(id: &str, title: &str, parent: Option<&str>) -> SessionItem {
        SessionItem { id: id.into(), title: title.into(), updated: 1, parent: parent.map(Arc::from) }
    }

    fn kinds(state: &AppState) -> Vec<String> {
        state
            .visible()
            .iter()
            .map(|row| match &row.kind {
                RowKind::User { text, .. } => format!("user:{text}"),
                RowKind::Text { markdown, streaming } => format!("text:{markdown}:{streaming}"),
                RowKind::Reasoning { text, .. } => format!("reasoning:{text}"),
                RowKind::Tool(view) => format!("tool:{}", view.tool),
                RowKind::Error { message } => format!("error:{message}"),
                RowKind::Retry { attempt, .. } => format!("retry:{attempt}"),
                RowKind::Compaction => "compaction".to_string(),
                RowKind::Working { label } => format!("working:{label}"),
            })
            .collect()
    }

    fn catalog() -> Catalog {
        Catalog {
            models: vec![ModelGroup {
                provider: "Acme".into(),
                id: "acme".into(),
                models: vec![
                    ModelItem { spec: "acme/large".into(), name: "Large".into(), context: 200_000, ..Default::default() },
                    ModelItem { spec: "acme/small".into(), name: "Small".into(), context: 0, ..Default::default() },
                ],
            }],
            agents: vec![
                AgentItem { name: "build".into(), description: "".into() },
                AgentItem { name: "plan".into(), description: "".into() },
            ],
            default_model: Some("acme/large".into()),
            default_agent: Some("build".into()),
            ..Default::default()
        }
    }

    fn finished(session: &str, rows: Option<Vec<Row>>, outcome: Outcome) -> Event {
        Event::Finished { session_id: session.into(), rows, outcome }
    }

    #[test]
    fn a_draft_prompt_creates_and_streams_a_session() {
        let mut state = AppState::new("/work/app".into());
        assert_eq!(state.current_title().as_ref(), NEW_SESSION_TITLE);
        assert!(state.visible().is_empty());

        state.begin_prompt("ses_1", "Fix the bug\nplease", vec!["a.png".into()], 42);
        assert_eq!(state.current.as_deref(), Some("ses_1"));
        assert_eq!(state.sessions[0].title.as_ref(), "Fix the bug");
        assert!(state.current_running());
        assert_eq!(state.current_status(), RunStatus::Busy);
        assert_eq!(kinds(&state), ["user:Fix the bug\nplease"]);
        assert!(matches!(&state.visible()[0].kind, RowKind::User { files, .. } if files.len() == 1));

        let delta = |d: &str| Event::Text { session_id: "ses_1".into(), delta: d.into() };
        state.apply(Event::Reasoning { session_id: "ses_1".into(), delta: "hm".into() });
        state.apply(delta("Look"));
        state.apply(delta("ing"));
        assert_eq!(kinds(&state), ["user:Fix the bug\nplease", "reasoning:hm", "text:Looking:true"]);

        // The step was persisted: the snapshot replaces the streamed tail.
        let stored = vec![text_row("msg_u", "Fix the bug\nplease"), text_row("prt_1", "Looking")];
        state.apply(Event::Snapshot { session_id: "ses_1".into(), rows: stored.clone() });
        assert_eq!(state.visible(), stored);
        assert!(state.current_running());

        state.apply(delta("Done"));
        assert_eq!(state.visible().len(), 3);
        let mut done = stored.clone();
        done.push(text_row("prt_2", "Done"));
        state.apply(finished("ses_1", Some(done.clone()), Outcome::Done));
        assert!(!state.current_running());
        assert_eq!(state.current_status(), RunStatus::Idle);
        assert_eq!(state.visible(), done);
    }

    #[test]
    fn failures_keep_partial_text_and_show_the_error_until_the_next_prompt() {
        let mut state = AppState::new("/work/app".into());
        state.begin_prompt("ses_1", "hi", vec![], 1);
        state.apply(Event::Retry { session_id: "ses_1".into(), attempt: 1, message: "overloaded".into(), delay_ms: 2000 });
        assert_eq!(kinds(&state), ["user:hi", "retry:1"]);
        state.apply(Event::Text { session_id: "ses_1".into(), delta: "partial ans".into() });
        state.apply(finished("ses_1", Some(vec![text_row("msg_u", "hi")]), Outcome::Failed("HTTP 500".into())));
        assert_eq!(kinds(&state), ["text:hi:false", "text:partial ans:false", "error:HTTP 500"]);

        state.begin_prompt("ses_1", "again", vec![], 2);
        assert!(!kinds(&state).iter().any(|k| k.starts_with("error:")));

        state.apply(finished("ses_1", None, Outcome::Aborted));
        assert!(!state.current_running());
        assert!(!kinds(&state).iter().any(|k| k.starts_with("error:")));
    }

    #[test]
    fn streamed_text_survives_an_abort_without_a_snapshot() {
        let mut state = AppState::new("/work/app".into());
        state.begin_prompt("ses_1", "explain", vec![], 1);
        state.apply(Event::Snapshot { session_id: "ses_1".into(), rows: vec![text_row("msg_u", "explain")] });
        state.apply(Event::Reasoning { session_id: "ses_1".into(), delta: "thinking it over".into() });
        state.apply(Event::Text { session_id: "ses_1".into(), delta: "The answer is".into() });
        // The store holds the user message only; the aborted step was never written.
        state.apply(finished("ses_1", Some(vec![text_row("msg_u", "explain")]), Outcome::Aborted));
        assert_eq!(kinds(&state), ["text:explain:false", "reasoning:thinking it over", "text:The answer is:false"]);
        assert!(!state.current_running());
    }

    #[test]
    fn recorded_errors_arrive_live_and_are_not_repeated() {
        let mut state = AppState::new("/work/app".into());
        state.apply(Event::Error { session_id: "ses_1".into(), message: "ignored: nothing runs".into() });
        state.begin_prompt("ses_1", "hi", vec![], 1);
        state.apply(Event::Title { session_id: "ses_1".into(), title: "Greeting".into() });
        state.apply(Event::Title { session_id: "ses_gone".into(), title: "ignored".into() });
        assert_eq!(state.current_title().as_ref(), "Greeting");

        state.apply(Event::Error { session_id: "ses_1".into(), message: "HTTP 500".into() });
        assert_eq!(kinds(&state), ["user:hi", "error:HTTP 500"]);
        let stored = vec![text_row("msg_u", "hi"), Row::error("msg_a-error", "HTTP 500")];
        state.apply(finished("ses_1", Some(stored), Outcome::Failed("HTTP 500".into())));
        assert_eq!(kinds(&state), ["text:hi:false", "error:HTTP 500"]);
    }

    // Rust build: a prompt sent while the session runs steers it and shows up in the same live transcript.
    #[test]
    fn a_prompt_sent_while_running_joins_the_live_run() {
        let mut state = AppState::new("/work/app".into());
        state.begin_prompt("ses_1", "first", vec![], 1);
        state.apply(Event::Text { session_id: "ses_1".into(), delta: "working".into() });
        state.begin_prompt("ses_1", "also check the tests", vec![], 2);
        state.apply(Event::Text { session_id: "ses_1".into(), delta: "ok".into() });
        assert_eq!(kinds(&state), ["user:first", "text:working:false", "user:also check the tests", "text:ok:true"]);
        assert_eq!(state.sessions.len(), 1);
        state.apply(finished("ses_1", None, Outcome::Done));
        assert!(!state.current_running());
    }

    #[test]
    fn background_runs_survive_switching_sessions() {
        let mut state = AppState::new("/work/app".into());
        state.sessions = vec![item("ses_2", "Other", None)];
        state.begin_prompt("ses_1", "long task", vec![], 5);
        state.apply(Event::Text { session_id: "ses_1".into(), delta: "working".into() });

        state.select("ses_2");
        assert!(state.loading);
        assert!(state.visible().is_empty());
        // A late answer for a session that is no longer shown is dropped.
        state.apply(Event::Opened { session_id: "ses_1".into(), rows: vec![text_row("x", "stale")], model: None, agent: None });
        assert!(state.visible().is_empty());
        state.apply(Event::Opened { session_id: "ses_2".into(), rows: vec![text_row("prt_9", "old")], model: None, agent: None });
        assert!(!state.loading);
        assert_eq!(kinds(&state), ["text:old:false"]);
        assert_eq!(state.current_title().as_ref(), "Other");

        state.apply(Event::Text { session_id: "ses_1".into(), delta: " hard".into() });
        assert_eq!(kinds(&state), ["text:old:false"]);
        assert!(state.is_running("ses_1"));

        // A stale listing keeps the running session's placeholder.
        state.apply(Event::Sessions { directory: "/work/app".into(), sessions: vec![item("ses_2", "Other", None)] });
        let listed: Vec<&str> = state.sessions.iter().map(|s| s.id.as_ref()).collect();
        assert_eq!(listed, ["ses_1", "ses_2"]);

        state.select("ses_1");
        assert!(!state.loading);
        assert_eq!(kinds(&state), ["user:long task", "text:working hard:true"]);
        // The store read for a running session must not clobber the live view.
        state.apply(Event::Opened { session_id: "ses_1".into(), rows: vec![], model: None, agent: None });
        assert_eq!(kinds(&state).len(), 2);
    }

    #[test]
    fn permissions_and_questions_are_scoped_to_their_session() {
        let mut state = AppState::new("/work/app".into());
        state.begin_prompt("ses_1", "edit", vec![], 1);
        let request = |id, session: &str| PermissionView {
            id,
            session_id: session.into(),
            permission: "edit".into(),
            patterns: vec!["src/a.rs".into()],
            detail: None,
            diff: None,
        };
        state.apply(Event::Permission(request(1, "ses_1")));
        state.apply(Event::Permission(request(2, "ses_1")));
        state.apply(Event::Permission(request(3, "ses_other")));
        assert_eq!(state.current_permission().map(|p| p.id), Some(1));
        assert!(state.waiting("ses_other"));

        let question = |id, session: &str| QuestionView { id, session_id: session.into(), questions: vec![] };
        state.apply(Event::Question(question(7, "ses_1")));
        state.apply(Event::Question(question(8, "ses_other")));
        assert_eq!(state.current_question().map(|q| q.id), Some(7));
        state.apply(Event::QuestionResolved(7));
        assert_eq!(state.current_question(), None);
        state.apply(Event::Question(question(9, "ses_1")));

        state.apply(Event::PermissionResolved(1));
        assert_eq!(state.current_permission().map(|p| p.id), Some(2));
        state.apply(finished("ses_1", None, Outcome::Aborted));
        assert_eq!((state.current_permission(), state.current_question()), (None, None));
        assert!(!state.waiting("ses_1") && state.waiting("ses_other"));
    }

    #[test]
    fn catalog_and_selection() {
        let mut state = AppState::new("/work/app".into());
        assert_eq!(state.model_label(), None);
        state.model = Some("gone/model".into());
        state.agent = Some("ghost".into());
        state.apply(Event::Catalog { directory: "/elsewhere".into(), catalog: catalog() });
        assert!(state.catalog.models.is_empty());
        assert_eq!(state.cycle_agent(1), None);

        state.apply(Event::Catalog { directory: "/work/app".into(), catalog: catalog() });
        assert_eq!((state.model.as_deref(), state.agent.as_deref()), (None, None));
        assert_eq!(state.effective_model(), Some("acme/large"));
        assert_eq!(state.effective_agent(), Some("build"));
        assert_eq!(state.model_label().as_deref(), Some("Large"));

        state.select("ses_1");
        state.apply(Event::Opened {
            session_id: "ses_1".into(),
            rows: vec![],
            model: Some("acme/small".into()),
            agent: Some("nope".into()),
        });
        assert_eq!(state.model_label().as_deref(), Some("Small"));
        assert_eq!(state.effective_agent(), Some("build"));

        assert_eq!(state.cycle_agent(1).as_deref(), Some("plan"));
        assert_eq!(state.cycle_agent(1).as_deref(), Some("build"));
        assert_eq!(state.cycle_agent(-1).as_deref(), Some("plan"));
        assert_eq!(state.agent.as_deref(), Some("plan"));
    }

    #[test]
    fn projects_and_notices() {
        let mut state = AppState::new("/work/app".into());
        assert_eq!(state.window_title(), "app — opencode");
        state.apply(Event::Sessions { directory: "/work/other".into(), sessions: vec![item("ses_x", "x", None)] });
        assert!(state.sessions.is_empty());
        state.select("ses_1");
        state.apply(Event::Failed { what: "desktop.rs.error.openSession", error: "database is locked".into() });
        assert!(!state.loading);
        assert_eq!(state.notice, Some(Notice { key: "desktop.rs.error.openSession", error: "database is locked".into() }));

        let command = CommandItem { name: "review".into(), description: "".into(), badge: "prompt.slash.badge.custom" };
        state.apply(Event::Commands { directory: "/work/other".into(), commands: vec![command.clone()] });
        state.apply(Event::Files { directory: "/work/other".into(), files: vec!["a.rs".into()] });
        assert!(state.commands.is_empty() && state.files.is_empty());
        state.apply(Event::Commands { directory: "/work/app".into(), commands: vec![command] });
        state.apply(Event::Files { directory: "/work/app".into(), files: vec!["a.rs".into()] });
        assert_eq!((state.commands.len(), state.files.len()), (1, 1));

        state.open_project("/work/other".into());
        assert_eq!(state.project_name(), "other");
        assert_eq!((state.current.as_deref(), state.notice.clone()), (None, None));
        assert!(state.commands.is_empty());
    }

    #[test]
    fn the_sidebar_nests_subagent_sessions_and_filters_by_title() {
        let mut state = AppState::new("/work/app".into());
        state.sessions = vec![
            item("ses_c1", "Explore the parser", Some("ses_a")),
            item("ses_a", "Refactor parser", None),
            item("ses_b", "Write docs", None),
            item("ses_orphan", "Lost child", Some("ses_gone")),
            item("ses_c2", "Run the tests", Some("ses_a")),
        ];
        let listed = |state: &AppState| -> Vec<(String, usize)> { state.sidebar().iter().map(|r| (r.session.id.to_string(), r.depth)).collect() };
        let row = |id: &str, depth| (id.to_string(), depth);
        assert_eq!(listed(&state), [row("ses_a", 0), row("ses_c1", 1), row("ses_c2", 1), row("ses_b", 0)]);

        state.filter = " DOCS ".into();
        assert_eq!(listed(&state), [row("ses_b", 0)]);
        // A matching child keeps its parent for context; a matching parent keeps all children.
        state.filter = "tests".into();
        assert_eq!(listed(&state), [row("ses_a", 0), row("ses_c2", 1)]);
        state.filter = "refactor".into();
        assert_eq!(listed(&state).len(), 3);
        state.filter = "nothing".into();
        assert!(state.sidebar().is_empty());
        assert_eq!(state.neighbor(1), None);

        state.filter.clear();
        assert_eq!(state.neighbor(1).as_deref(), Some("ses_a"));
        assert_eq!(state.neighbor(-1).as_deref(), Some("ses_b"));
        state.select("ses_c1");
        assert_eq!(state.current_parent().as_deref(), Some("ses_a"));
        assert_eq!(state.neighbor(1).as_deref(), Some("ses_c2"));
        assert_eq!(state.neighbor(-1).as_deref(), Some("ses_a"));
        state.select("ses_b");
        assert_eq!(state.neighbor(1), None);
        assert_eq!(state.current_parent(), None);
    }

    #[test]
    fn todos_usage_status_and_reverts_follow_the_current_session() {
        let mut state = AppState::new("/work/app".into());
        state.apply(Event::Catalog { directory: "/work/app".into(), catalog: catalog() });
        assert!(state.current_todos().is_empty());
        assert_eq!((state.current_usage(), state.context_percent(), state.current_revert()), (None, None, 0));

        let todo = |content: &str, status: &str| Todo { content: content.into(), status: status.into(), priority: "medium".into() };
        state.apply(Event::Todos { session_id: "ses_1".into(), todos: vec![todo("a", "completed"), todo("b", "pending")] });
        state.apply(Event::Usage { session_id: "ses_1".into(), usage: Usage { cost: 0.25, input: 900, output: 100, context: 50_000, ..Default::default() } });
        state.apply(Event::Usage { session_id: "ses_2".into(), usage: Usage::default() });
        state.apply(Event::Status { session_id: "ses_1".into(), status: RunStatus::Retry { attempt: 2, message: "busy".into(), next: 9 } });
        state.apply(Event::Revert { session_id: "ses_1".into(), hidden: 2 });
        assert!(state.current_todos().is_empty());

        state.select("ses_1");
        assert_eq!(state.current_todos().len(), 2);
        assert_eq!(state.current_usage().map(|u| u.total()), Some(1000));
        assert_eq!(state.context_percent(), Some(25));
        assert!(matches!(state.current_status(), RunStatus::Retry { attempt: 2, .. }));
        assert_eq!(state.current_revert(), 2);

        // The other model has no known context window.
        state.model = Some("acme/small".into());
        assert_eq!(state.context_percent(), None);
        state.select("ses_2");
        assert_eq!(state.current_usage(), None);

        state.select("ses_1");
        state.apply(Event::Revert { session_id: "ses_1".into(), hidden: 0 });
        assert_eq!(state.current_revert(), 0);
        state.apply(Event::Revert { session_id: "ses_1".into(), hidden: 1 });
        state.begin_prompt("ses_1", "next", vec![], 3);
        assert_eq!(state.current_revert(), 0, "a new prompt discards what was reverted");
    }

    #[test]
    fn subagent_activity_accumulates() {
        let mut state = AppState::new("/work/app".into());
        assert_eq!(state.child("ses_child"), None);
        let update = |update| Event::Child { child_id: "ses_child".into(), update };
        state.apply(update(ChildUpdate::Running(true)));
        state.apply(update(ChildUpdate::Title("Explore (@explore subagent)".into())));
        state.apply(update(ChildUpdate::Tool { label: "grep parse_".into(), started: true }));
        state.apply(update(ChildUpdate::Tool { label: "grep parse_".into(), started: false }));
        state.apply(update(ChildUpdate::Tool { label: "read src/parser.rs".into(), started: true }));
        let child = state.child("ses_child").unwrap();
        assert_eq!((child.tools, child.last.as_ref(), child.running), (2, "read src/parser.rs", true));
        assert_eq!(child.title.as_deref(), Some("Explore (@explore subagent)"));

        state.apply(update(ChildUpdate::Text("The parser lives in\n".into())));
        state.apply(update(ChildUpdate::Text("src/parser.rs\n\n".into())));
        state.apply(update(ChildUpdate::Running(false)));
        let child = state.child("ses_child").unwrap();
        assert_eq!((child.last.as_ref(), child.running), ("src/parser.rs", false));
        assert_eq!(tail(&"x".repeat(500), 160).len(), 161);
    }

    #[test]
    fn forks_appear_and_deleted_sessions_disappear_with_their_children() {
        let mut state = AppState::new("/work/app".into());
        state.sessions = vec![item("ses_a", "A", None), item("ses_c", "child", Some("ses_a")), item("ses_b", "B", None)];
        state.apply(Event::Forked { directory: "/work/app".into(), session: item("ses_f", "A (fork #1)", None) });
        state.apply(Event::Forked { directory: "/work/app".into(), session: item("ses_f", "A (fork #1)", None) });
        state.apply(Event::Forked { directory: "/elsewhere".into(), session: item("ses_x", "x", None) });
        assert_eq!(state.sessions.iter().map(|s| s.id.as_ref()).collect::<Vec<_>>(), ["ses_f", "ses_a", "ses_c", "ses_b"]);

        state.select("ses_a");
        state.apply(Event::Opened { session_id: "ses_a".into(), rows: vec![text_row("msg_1", "hello")], model: None, agent: None });
        state.apply(Event::Todos { session_id: "ses_a".into(), todos: vec![] });
        state.apply(Event::Deleted { session_id: "ses_b".into() });
        assert_eq!(state.current.as_deref(), Some("ses_a"));
        state.apply(Event::Deleted { session_id: "ses_a".into() });
        assert_eq!(state.current, None);
        assert!(state.visible().is_empty());
        assert_eq!(state.sessions.iter().map(|s| s.id.as_ref()).collect::<Vec<_>>(), ["ses_f"]);
    }

    #[test]
    fn undo_targets_the_newest_stored_user_message() {
        let mut state = AppState::new("/work/app".into());
        assert_eq!(state.last_user_message(), None);
        state.select("ses_1");
        let user = |id: &str| Row { id: id.into(), kind: RowKind::User { text: "q".into(), files: vec![] } };
        let rows = vec![user("msg_1"), text_row("prt_1", "a"), user("msg_2"), text_row("prt_2", "b")];
        state.apply(Event::Opened { session_id: "ses_1".into(), rows, model: None, agent: None });
        assert_eq!(state.last_user_message().as_deref(), Some("msg_2"));
    }
}
