use crate::catalog::{AgentInfo, Catalog};
use crate::config::{MaxWidth, TuiConfig};
use crate::dialog::{self, Dialog, SelectKind};
use crate::editor::Editor;
use crate::fuzzy;
use crate::keybind::{self, KeyStroke, Keymap, Part};
use crate::question::{QuestionAsk, QuestionState};
use crate::stash::{self, Frecency, Stash};
use crate::state::{History, HistoryWrite, Kv, ModelKey, ModelState, Pins};
use crate::terminal::{Caps, ClipboardContent, LocalAttachment, TerminalColors};
use crate::theme::{DEFAULT_THEME, KV_THEME, Mode, Registry, Rgba, Theme};
use crate::transcript::PartCache;
use opencode_session::{Reply, Status, Update};
use opencode_storage::{FilePart, Message, Part as StoredPart, PartBody, Session, Todo, ToolState};
use opencode_tools::PermissionRequest;
use ratatui::crossterm::event::{KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const INTERRUPT_WINDOW: Duration = Duration::from_secs(5);
// Rust build: the popup scrolls (10 rows shown), so it holds every command and up to 100 files.
const AUTOCOMPLETE_LIMIT: usize = 100;
pub const AUTOCOMPLETE_ROWS: usize = 10;
pub const SIDEBAR_WIDTH: u16 = 42;

#[derive(Debug, Clone, PartialEq)]
pub enum Live {
    Text(String),
    Reasoning(String),
    Tool(Box<StoredPart>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChildTool {
    pub id: String,
    pub tool: String,
    pub status: &'static str,
    pub title: Option<String>,
}

impl ChildTool {
    pub fn from_part(part: &StoredPart) -> Option<Self> {
        let PartBody::Tool(tool) = &part.body else { return None };
        let title = match &tool.state {
            ToolState::Running(running) => running.title.clone().filter(|t| !t.is_empty()),
            ToolState::Completed(done) => Some(done.title.clone()).filter(|t| !t.is_empty()),
            _ => None,
        };
        Some(Self { id: part.id.clone(), tool: tool.tool.clone(), status: tool.state.status(), title })
    }
}

// A subagent session of the one on screen (or, in a child view, one of its siblings).
#[derive(Debug, Clone, PartialEq)]
pub struct Child {
    pub info: Session,
    pub tools: Vec<ChildTool>,
    pub status: Status,
    // First user message to last completed answer, 0 while unknown.
    pub duration: u64,
}

#[derive(Debug, Clone)]
pub struct SessionView {
    pub info: Session,
    pub messages: Vec<(Message, Vec<StoredPart>)>,
    // Output of the running step that the store has not been re-read for yet.
    pub live: Vec<Live>,
    // Stored part ids whose text is being streamed into `live`.
    pub live_parts: Vec<String>,
    // Submitted text and the user-message count at submit time.
    pub pending_user: Option<(String, usize)>,
    pub status: Status,
    // Lines scrolled up from the bottom; 0 follows new output.
    pub scroll: usize,
    pub todos: Vec<Todo>,
    pub children: Vec<Child>,
}

impl SessionView {
    pub fn new(info: Session, messages: Vec<(Message, Vec<StoredPart>)>, todos: Vec<Todo>) -> Self {
        Self { info, messages, live: Vec::new(), live_parts: Vec::new(), pending_user: None, status: Status::Idle, scroll: 0, todos, children: Vec::new() }
    }

    pub fn busy(&self) -> bool {
        self.status != Status::Idle
    }

    pub fn is_child(&self) -> bool {
        self.info.parent_id.is_some()
    }

    pub fn child(&self, session_id: &str) -> Option<&Child> {
        self.children.iter().find(|child| child.info.id == session_id)
    }

    pub(crate) fn user_count(&self) -> usize {
        self.messages.iter().filter(|(m, _)| matches!(m, Message::User(_))).count()
    }

    // Messages hidden by an undo are still stored until the next prompt.
    pub fn visible_messages(&self) -> impl Iterator<Item = &(Message, Vec<StoredPart>)> {
        let cut = self.info.revert.as_ref().map(|r| r.message_id.clone());
        self.messages.iter().filter(move |(m, _)| cut.as_deref().is_none_or(|cut| m.id() < cut))
    }
}

#[derive(Debug, Clone)]
pub enum Route {
    Home,
    Session(Box<SessionView>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct PermissionAsk {
    pub id: u64,
    // The asking session; empty when the host did not say.
    pub session_id: String,
    pub request: PermissionRequest,
}

// A screen region the mouse can act on, recorded by the renderer.
#[derive(Debug, Clone, PartialEq)]
pub enum HitTarget {
    // Toggles the full output of a tool part.
    Tool(String),
    // Opens a subagent session.
    Session(String),
    // Selects (and submits) a dialog row by its position in the filtered list.
    DialogRow(usize),
    // Drags the transcript scrollbar.
    Scrollbar,
    Command(&'static str),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub target: HitTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhichKeyLayout {
    Dock,
    Overlay,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WhichKey {
    pub pinned: bool,
    pub layout: WhichKeyLayout,
    pub pending_preview: bool,
    pub group: usize,
    pub scroll: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EditorPurpose {
    Prompt,
    // Transcript opened for review; written to the file afterwards when one is set.
    Export { filename: Option<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionStage {
    Ask,
    Always,
    // Typing feedback for a rejected subagent request.
    Reject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub message: String,
    pub kind: ToastKind,
    pub until: Instant,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AcAction {
    // Runs a built-in command (config key).
    Builtin(&'static str),
    // Completes a config/markdown command name.
    Command(String),
    File(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct AcItem {
    pub label: String,
    pub description: String,
    pub action: AcAction,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Autocomplete {
    // Byte offset of the `/` or `@` trigger.
    pub start: usize,
    pub items: Vec<AcItem>,
    pub selected: usize,
}

#[derive(Debug)]
pub enum Action {
    Key(KeyEvent),
    Paste(String),
    Mouse(MouseEvent),
    Resize(u16, u16),
    Tick(Instant, u64),
    Engine(Update),
    PermissionAsked(PermissionAsk),
    QuestionAsked(QuestionAsk),
    PromptFinished { session_id: String, error: Option<String> },
    // `open` switches to the session; otherwise it refreshes the one on screen. `settled` drops live output.
    SessionLoaded { info: Box<Session>, messages: Vec<(Message, Vec<StoredPart>)>, todos: Vec<Todo>, open: bool, settled: bool },
    // Subagent sessions around the one on screen, with the engine's status of that session.
    ChildrenLoaded { session_id: String, children: Vec<Child>, status: Status },
    SessionsListed(Vec<Session>),
    SessionDeleted(String),
    FilesLoaded(Vec<String>),
    Toast(String, ToastKind),
    // A revert or restore of the session's messages and files finished.
    SessionChanged { session_id: String, error: Option<String> },
    // Replaces the prompt (`PromptInfo`: `{input, parts}`).
    PromptSet(Value),
    Clipboard(Option<ClipboardContent>),
    // A pasted path that turned out to be an image, PDF or SVG file.
    Attachment(LocalAttachment),
    EditorClosed { purpose: EditorPurpose, content: Option<String> },
    Focus(bool),
    Terminal(TerminalColors),
    // The runtime created a session for a prompt sent from the home screen; `then` sends that prompt.
    SessionStarted { info: Box<Session>, text: String, then: Box<Effect> },
    // A permission or question was answered elsewhere (another client of the same server).
    Answered(u64),
    // The event stream reconnected; what is on screen may be stale.
    Resync,
    // Submits the prompt as it stands (`--prompt` once the startup session is open).
    Submit,
    // Rust build: the `/btw` answer, or why there is none.
    Aside { question: String, answer: Result<String, String> },
    // Rust build: `/reload` finished; the catalog as it now reads.
    Reloaded(Result<Box<crate::catalog::Catalog>, String>),
    // Rust build: `/stats`, rendered.
    Stats(Result<String, String>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Quit,
    Submit { session_id: Option<String>, text: String, files: Vec<FilePart>, agent: String, model: Option<String>, variant: Option<String> },
    Command { session_id: Option<String>, command: String, arguments: String, agent: String, model: Option<String> },
    Shell { session_id: Option<String>, command: String, agent: String, model: Option<String> },
    QuestionReply { id: u64, answers: Option<Vec<Vec<String>>> },
    Fork { session_id: String, message_id: Option<String>, prompt: Option<Value> },
    ReadClipboard,
    OpenEditor { purpose: EditorPurpose, text: String },
    WriteFile { filename: String, text: String },
    Stash(stash::Write),
    Frecency(stash::Write),
    Suspend,
    Compact { session_id: String },
    Abort { session_id: String },
    Undo { session_id: String, message_id: String },
    Redo { session_id: String },
    PermissionReply { id: u64, reply: Reply },
    // Reject with a message for the model.
    PermissionFeedback { id: u64, message: String },
    OpenSession(String),
    Reload { session_id: String, settled: bool },
    ListSessions,
    DeleteSession(String),
    RenameSession { session_id: String, title: String },
    SaveKv,
    SaveModels,
    SavePins,
    // Opens the session in quick slot 1..=9, if that slot is filled.
    QuickSwitch(usize),
    OpenUrl(String),
    History(HistoryWrite),
    Copy(String),
    // Rust build: the answer to the portable-bash offer (true downloads it, false never asks again).
    SetupBash { accept: bool },
    // Rust build: a prompt for a busy session that waits until the agent is done (opencode 2.x queue delivery).
    Queue { session_id: String, text: String, files: Vec<FilePart>, agent: String, model: Option<String>, variant: Option<String> },
    // Rust build: `/btw`: a one-off answer from the session's context, not added to the conversation.
    Aside { session_id: String, question: String },
    // Rust build: `/reload`.
    ReloadConfig,
    // Rust build: `/stats`.
    Stats,
}

pub struct Init {
    pub directory: PathBuf,
    pub home: PathBuf,
    pub config: TuiConfig,
    pub themes: Registry,
    pub kv: Kv,
    pub models: ModelState,
    pub history: History,
    pub catalog: Catalog,
    pub model: Option<String>,
    pub agent: Option<String>,
    pub prompt: Option<String>,
    pub tips: Vec<String>,
    pub branch: Option<String>,
    pub stash: Stash,
    pub frecency: Frecency,
    pub pins: Pins,
    pub caps: Caps,
    // `experimental.disable_paste_summary` from the instance config.
    pub disable_paste_summary: bool,
}

pub struct App {
    pub size: (u16, u16),
    pub route: Route,
    pub editor: Editor,
    // Raw prompt parts (`PromptInfo.parts`) for the `@file` mentions in the editor.
    pub attachments: Vec<Value>,
    pub autocomplete: Option<Autocomplete>,
    pub(crate) autocomplete_dismissed: bool,
    pub interrupt: u8,
    interrupt_at: Option<Instant>,
    pub dialog: Option<Dialog>,
    pub keymap: Keymap,
    pub pending: Vec<Part>,
    pending_at: Option<Instant>,
    pub themes: Registry,
    pub theme_name: String,
    pub theme: Theme,
    pub mode: Mode,
    // Theme to restore when the picker is dismissed.
    pub(crate) theme_before_preview: Option<String>,
    pub questions: VecDeque<QuestionAsk>,
    pub question: QuestionState,
    pub shell_mode: bool,
    // Rust build: the prompt being submitted waits for the agent to finish (`prompt_queue`) instead of steering.
    queueing: bool,
    pub stash: Stash,
    pub frecency: Frecency,
    pub pins: Pins,
    // Sessions behind the open session list.
    listed: Vec<Session>,
    pub caps: Caps,
    pub disable_paste_summary: bool,
    // The user opened the sidebar on a narrow terminal.
    pub sidebar_open: bool,
    pub conceal: bool,
    pub which_key: WhichKey,
    pub focused: bool,
    pub permission_fullscreen: bool,
    // Tool parts whose output the user expanded.
    pub expanded: HashSet<String>,
    // Written by the renderer.
    pub hits: RefCell<Vec<Hit>>,
    // First transcript line of every visible message, written by the renderer.
    pub anchors: RefCell<Vec<(String, bool, usize)>>,
    // Whether a text/reasoning delta is next, from the untagged update that precedes each tagged one.
    stream_reasoning: bool,
    // Live text of sessions that are streaming off screen: (entries, stored part ids).
    streams: HashMap<String, (Vec<Live>, Vec<String>)>,
    dragging_scrollbar: bool,
    pub terminal: TerminalColors,
    pub config: TuiConfig,
    pub kv: Kv,
    pub models: ModelState,
    pub history: History,
    pub catalog: Catalog,
    pub agent: String,
    pub model_by_agent: HashMap<String, ModelKey>,
    cli_model: Option<ModelKey>,
    pub files: Vec<String>,
    pub toast: Option<Toast>,
    pub directory: PathBuf,
    pub home: PathBuf,
    pub branch: Option<String>,
    pub tip: Option<String>,
    tips: Vec<String>,
    pub show_thinking: bool,
    pub show_details: bool,
    pub permissions: VecDeque<PermissionAsk>,
    // `--auto`: every permission request is approved once without asking.
    pub auto_permissions: bool,
    pub permission_choice: usize,
    pub permission_stage: PermissionStage,
    pub permission_feedback: Editor,
    // Sessions with a loop in flight.
    pub running: Vec<String>,
    pub now: Instant,
    pub now_ms: u64,
    pub frame: u64,
    pub quit: bool,
    // Rust build: set when a reply settles, so the runtime can repaint from scratch where the console leaves stale
    // wide characters behind (Windows console host).
    pub repaint: bool,
    // Rust build: drawing on the Windows console host, whose fonts lack some symbols (`⎿`).
    pub console_host: bool,
    // Rust build: the terminal draws ambiguous-width characters two cells wide; frames get narrow stand-ins.
    pub narrow_glyphs: bool,
    // Rust build: sounds to play, drained by the runtime (`sound.rs`).
    pub chimes: Vec<crate::sound::Chime>,
    // Sessions the user stopped; their end makes no sound.
    interrupted: std::collections::HashSet<String>,
    // Written by the renderer: (message viewport height, transcript line count).
    pub viewport: Cell<(usize, usize)>,
    pub part_cache: RefCell<PartCache>,
}

fn file_url(path: &Path) -> String {
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

// Keys an engaged text field consumes, in the order they are tried.
const INPUT: &[&str] = &[
    "input_submit",
    "input_newline",
    "input_clear",
    "input_move_left",
    "input_move_right",
    "input_move_up",
    "input_move_down",
    "input_line_home",
    "input_line_end",
    "input_visual_line_home",
    "input_visual_line_end",
    "input_buffer_home",
    "input_buffer_end",
    "input_delete_line",
    "input_delete_to_line_end",
    "input_delete_to_line_start",
    "input_backspace",
    "input_delete",
    "input_undo",
    "input_redo",
    "input_word_forward",
    "input_word_backward",
    "input_delete_word_forward",
    "input_delete_word_backward",
    "input_select_left",
    "input_select_right",
    "input_select_up",
    "input_select_down",
    "input_select_line_home",
    "input_select_line_end",
    "input_select_visual_line_home",
    "input_select_visual_line_end",
    "input_select_buffer_home",
    "input_select_buffer_end",
    "input_select_word_forward",
    "input_select_word_backward",
    "input_select_all",
    "prompt_queue",
    "prompt_submit",
];

pub(crate) const WHICH_KEY_PANEL: &[&str] = &[
    "which_key_group_previous",
    "which_key_group_next",
    "which_key_scroll_up",
    "which_key_scroll_down",
    "which_key_page_up",
    "which_key_page_down",
    "which_key_home",
    "which_key_end",
];

// Commands that need a session on screen.
pub(crate) const SESSION_ONLY: &[&str] = &[
    "session_rename",
    "session_compact",
    "session_export",
    "session_copy",
    "session_timeline",
    "session_fork",
    "session_toggle_timestamps",
    "session_toggle_generic_tool_output",
    "session_child_first",
    "sidebar_toggle",
    "scrollbar_toggle",
    "messages_undo",
    "messages_redo",
    "messages_copy",
    "messages_next",
    "messages_previous",
    "messages_last_user",
    "messages_toggle_conceal",
    "display_thinking",
    "tool_details",
];

// `appGlobalBindingCommands`: registered without a mode, so they fire under dialogs and prompts as well.
const APP_GLOBAL: &[&str] = &[
    "session_list",
    "session_new",
    "session_quick_switch_1",
    "session_quick_switch_2",
    "session_quick_switch_3",
    "session_quick_switch_4",
    "session_quick_switch_5",
    "session_quick_switch_6",
    "session_quick_switch_7",
    "session_quick_switch_8",
    "session_quick_switch_9",
];

const GLOBAL: &[&str] = &[
    "command_list",
    "help_show",
    "theme_list",
    "theme_switch_mode",
    "theme_mode_lock",
    "editor_open",
    "sidebar_toggle",
    "scrollbar_toggle",
    "session_export",
    "session_copy",
    "session_timeline",
    "session_fork",
    "session_toggle_timestamps",
    "session_toggle_generic_tool_output",
    "session_child_first",
    "variant_cycle",
    "variant_list",
    "messages_next",
    "messages_previous",
    "messages_last_user",
    "messages_toggle_conceal",
    "prompt_stash",
    "prompt_stash_pop",
    "prompt_stash_list",
    "terminal_suspend",
    "terminal_title_toggle",
    "app_toggle_paste_summary",
    "app_toggle_animations",
    "docs_open",
    "session_quick_switch_1",
    "session_quick_switch_2",
    "session_quick_switch_3",
    "session_quick_switch_4",
    "session_quick_switch_5",
    "session_quick_switch_6",
    "session_quick_switch_7",
    "session_quick_switch_8",
    "session_quick_switch_9",
    "session_new",
    "session_list",
    "session_rename",
    "session_compact",
    "model_list",
    "model_cycle_recent",
    "model_cycle_recent_reverse",
    "model_cycle_favorite",
    "model_cycle_favorite_reverse",
    "agent_list",
    "agent_cycle",
    "agent_cycle_reverse",
    "messages_page_up",
    "messages_page_down",
    "messages_line_up",
    "messages_line_down",
    "messages_half_page_up",
    "messages_half_page_down",
    "messages_first",
    "messages_last",
    "messages_copy",
    "messages_undo",
    "messages_redo",
    "display_thinking",
    "tool_details",
    "tips_toggle",
    "app_exit",
];

const SCROLL: &[&str] = &[
    "messages_page_up",
    "messages_page_down",
    "messages_line_up",
    "messages_line_down",
    "messages_half_page_up",
    "messages_half_page_down",
    "messages_first",
    "messages_last",
];

pub fn implemented_keybinds() -> Vec<&'static str> {
    let mut all: Vec<&'static str> = INPUT.iter().chain(GLOBAL).chain(WHICH_KEY_PANEL).copied().collect();
    all.extend([
        "leader",
        "session_interrupt",
        "session_delete",
        "session_child_cycle",
        "session_child_cycle_reverse",
        "session_parent",
        "stash_delete",
        "session_pin_toggle",
        "input_paste",
        "permission.prompt.fullscreen",
        "which_key_toggle",
        "which_key_layout_toggle",
        "which_key_pending_toggle",
        "model_favorite_toggle",
        "history_previous",
        "history_next",
        "dialog.select.prev",
        "dialog.select.next",
        "dialog.select.page_up",
        "dialog.select.page_down",
        "dialog.select.home",
        "dialog.select.end",
        "dialog.select.submit",
        "dialog.prompt.submit",
        "prompt.autocomplete.prev",
        "prompt.autocomplete.next",
        "prompt.autocomplete.hide",
        "prompt.autocomplete.select",
        "prompt.autocomplete.complete",
    ]);
    all
}

impl App {
    pub fn new(init: Init) -> Self {
        let terminal_suspend = !cfg!(windows);
        let (keymap, _) = Keymap::new(&init.config.keybinds, init.config.leader_timeout, terminal_suspend);
        let mode = match init.kv.get_str("theme_mode_lock") {
            Some("light") => Mode::Light,
            _ => Mode::Dark,
        };
        let wanted = init.config.theme.clone().or_else(|| init.kv.get_str(KV_THEME).map(str::to_string)).unwrap_or_else(|| DEFAULT_THEME.to_string());
        let theme_name = if init.themes.has(&wanted) { wanted } else { DEFAULT_THEME.to_string() };
        let theme = init.themes.resolve(&theme_name, mode);
        let show_thinking = match init.kv.get_str("thinking_mode") {
            Some(mode) => mode == "show",
            None => init.kv.get_bool("thinking_visibility", false),
        };
        let tips_hidden = init.kv.get_bool("tips_hidden", false);
        let mut app = Self {
            size: (80, 24),
            route: Route::Home,
            editor: Editor::default(),
            attachments: Vec::new(),
            autocomplete: None,
            autocomplete_dismissed: false,
            interrupt: 0,
            interrupt_at: None,
            dialog: None,
            keymap,
            pending: Vec::new(),
            pending_at: None,
            themes: init.themes,
            theme_name,
            theme,
            mode,
            theme_before_preview: None,
            questions: VecDeque::new(),
            question: QuestionState::default(),
            shell_mode: false,
            queueing: false,
            stash: init.stash,
            frecency: init.frecency,
            pins: init.pins,
            listed: Vec::new(),
            caps: init.caps,
            disable_paste_summary: init.disable_paste_summary,
            sidebar_open: false,
            conceal: true,
            which_key: WhichKey {
                pinned: false,
                layout: if init.kv.get_str("which_key_layout") == Some("overlay") { WhichKeyLayout::Overlay } else { WhichKeyLayout::Dock },
                pending_preview: init.kv.get_bool("which_key_pending_preview", false),
                group: 0,
                scroll: 0,
            },
            focused: true,
            permission_fullscreen: false,
            expanded: HashSet::new(),
            hits: RefCell::new(Vec::new()),
            anchors: RefCell::new(Vec::new()),
            stream_reasoning: false,
            streams: HashMap::new(),
            dragging_scrollbar: false,
            terminal: TerminalColors::default(),
            show_details: init.kv.get_bool("tool_details_visibility", true),
            show_thinking,
            tip: (!tips_hidden).then(|| init.tips.first().cloned()).flatten(),
            tips: init.tips,
            config: init.config,
            kv: init.kv,
            models: init.models,
            history: init.history,
            agent: String::new(),
            model_by_agent: HashMap::new(),
            cli_model: init.model.as_deref().and_then(ModelKey::parse),
            catalog: init.catalog,
            files: Vec::new(),
            toast: None,
            directory: init.directory,
            home: init.home,
            branch: init.branch,
            permissions: VecDeque::new(),
            auto_permissions: false,
            permission_choice: 0,
            permission_stage: PermissionStage::Ask,
            permission_feedback: Editor::default(),
            running: Vec::new(),
            now: Instant::now(),
            now_ms: 0,
            frame: 0,
            quit: false,
            repaint: false,
            console_host: false,
            narrow_glyphs: false,
            chimes: Vec::new(),
            interrupted: Default::default(),
            viewport: Cell::new((0, 0)),
            part_cache: RefCell::new(PartCache::default()),
        };
        let agents = app.catalog.primary_agents();
        app.agent = init
            .agent
            .filter(|name| agents.iter().any(|a| &a.name == name))
            .or_else(|| agents.first().map(|a| a.name.clone()))
            .unwrap_or_else(|| "build".to_string());
        if let Some(error) = app.catalog.error.clone() {
            app.show_toast(error, ToastKind::Error);
        }
        if let Some(prompt) = init.prompt {
            app.editor.set(&prompt);
        }
        app
    }

    pub fn session(&self) -> Option<&SessionView> {
        match &self.route {
            Route::Session(view) => Some(view),
            Route::Home => None,
        }
    }

    pub fn show_timestamps(&self) -> bool {
        self.kv.get_str("timestamps") == Some("show")
    }

    pub fn show_scrollbar(&self) -> bool {
        self.kv.get_bool("scrollbar_visible", false)
    }

    pub fn animations(&self) -> bool {
        self.kv.get_bool("animations_enabled", true)
    }

    pub fn paste_summary(&self) -> bool {
        self.kv.get_bool("paste_summary_enabled", !self.disable_paste_summary)
    }

    pub fn terminal_title_enabled(&self) -> bool {
        self.kv.get_bool("terminal_title_enabled", true)
    }

    // `sidebarVisible`: never in a subagent view; otherwise opened by hand or automatically on wide terminals.
    pub fn sidebar_visible(&self) -> bool {
        match self.session() {
            Some(view) if !view.is_child() => self.sidebar_open || (self.kv.get_str("sidebar") != Some("hide") && self.size.0 > 120),
            _ => false,
        }
    }

    // The permission request the session on screen has to answer, if any. Subagent views never prompt.
    pub fn permission(&self) -> Option<&PermissionAsk> {
        let view = self.session().filter(|view| !view.is_child())?;
        self.permissions.iter().find(|ask| self.concerns(view, &ask.session_id))
    }

    pub fn question_ask(&self) -> Option<&QuestionAsk> {
        if self.permission().is_some() {
            return None;
        }
        let view = self.session().filter(|view| !view.is_child())?;
        self.questions.iter().find(|ask| self.concerns(view, &ask.request.session_id))
    }

    fn concerns(&self, view: &SessionView, session_id: &str) -> bool {
        session_id.is_empty() || session_id == view.info.id || view.child(session_id).is_some() || (self.running.len() == 1 && self.running[0] == view.info.id)
    }

    pub(crate) fn session_mut(&mut self) -> Option<&mut SessionView> {
        match &mut self.route {
            Route::Session(view) => Some(view),
            Route::Home => None,
        }
    }

    // Rust build: asks once whether to download a portable bash (Windows without one).
    // Rust build: tool calls drawn as `● Name(args)` with the result under `⎿`, unless kv.json has
    // `"rs.style": "opencode"`.
    // Rust build: TUI sounds, on unless kv.json has `"rs.sounds": false`.
    pub fn sounds_on(&self) -> bool {
        self.kv.get_bool("rs.sounds", true)
    }

    fn chime(&mut self, chime: crate::sound::Chime) {
        if self.sounds_on() {
            self.chimes.push(chime);
        }
    }

    pub fn claude_look(&self) -> bool {
        self.kv.get_str("rs.style") != Some("opencode")
    }

    pub fn offer_bash(&mut self) {
        self.dialog = Some(Dialog::Select(dialog::setup_bash()));
    }

    pub fn show_toast(&mut self, message: impl Into<String>, kind: ToastKind) {
        self.toast = Some(Toast { message: message.into(), kind, until: self.now + Duration::from_secs(if kind == ToastKind::Error { 6 } else { 3 }) });
    }

    pub fn current_agent(&self) -> Option<&AgentInfo> {
        self.catalog.agents.iter().find(|a| a.name == self.agent)
    }

    // `local.agent.color`
    pub fn agent_color(&self, name: &str) -> Rgba {
        let t = &self.theme;
        let palette = [t.secondary, t.accent, t.success, t.warning, t.primary, t.error, t.info];
        let visible: Vec<&AgentInfo> = self.catalog.agents.iter().filter(|a| !a.hidden).collect();
        let Some(index) = visible.iter().position(|a| a.name == name) else { return palette[0] };
        match visible[index].color.as_deref() {
            Some(hex) if hex.starts_with('#') => Rgba::from_hex(hex).unwrap_or(palette[0]),
            Some(named) => t.named(named).unwrap_or(palette[index % palette.len()]),
            None => palette[index % palette.len()],
        }
    }

    // Per-agent choice, then the agent's configured model, then --model, config, recents, first provider.
    pub fn current_model(&self) -> Option<ModelKey> {
        let valid = |key: &ModelKey| self.catalog.is_valid(key);
        self.model_by_agent
            .get(&self.agent)
            .filter(|k| valid(k))
            .or_else(|| self.current_agent().and_then(|a| a.model.as_ref()).filter(|k| valid(k)))
            .or_else(|| self.cli_model.as_ref().filter(|k| valid(k)))
            .or_else(|| self.catalog.config_model.as_ref().filter(|k| valid(k)))
            .or_else(|| self.models.recent.iter().find(|k| valid(k)))
            .cloned()
            .or_else(|| self.catalog.first_model())
    }

    // (model name, provider name) for the prompt status row.
    pub fn model_label(&self) -> (String, String) {
        match self.current_model() {
            Some(key) => match self.catalog.model(&key) {
                Some((provider, model)) => (model.name.clone(), provider.name.clone()),
                None => (key.model_id, key.provider_id),
            },
            None => ("No provider selected".into(), "Connect a provider".into()),
        }
    }

    pub fn leader_active(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn prompt_max_width(&self) -> usize {
        match self.config.prompt_max_width {
            MaxWidth::Auto => 75.max(usize::from(self.size.0) * 7 / 10),
            MaxWidth::Fixed(n) => usize::from(n),
        }
    }

    // Columns available to prompt text.
    pub fn prompt_text_width(&self) -> usize {
        let outer = usize::from(self.size.0).saturating_sub(4 + if self.sidebar_visible() { usize::from(SIDEBAR_WIDTH) } else { 0 });
        let boxed = if self.session().is_some() { outer } else { outer.min(self.prompt_max_width()) };
        // "┃  " and padding, or "> " and a margin in the Claude look.
        boxed.saturating_sub(if self.claude_look() { 3 } else { 5 }).max(1)
    }

    pub fn prompt_max_rows(&self) -> usize {
        self.config.prompt_max_height.map_or_else(|| 6.max(usize::from(self.size.1) / 3), usize::from)
    }

    pub fn update(&mut self, action: Action) -> Vec<Effect> {
        match action {
            Action::Key(event) => {
                if event.kind == KeyEventKind::Release {
                    return Vec::new();
                }
                self.key(event)
            }
            Action::Paste(text) => {
                self.paste(&text);
                Vec::new()
            }
            Action::Clipboard(content) => {
                match content {
                    Some(ClipboardContent::Text(text)) => self.paste(&text),
                    Some(ClipboardContent::Image(data)) if self.prompt_focused() => self.paste_attachment("clipboard", "clipboard", "image/png", &data),
                    _ => {}
                }
                Vec::new()
            }
            Action::Attachment(attachment) => {
                if self.prompt_focused() {
                    match attachment {
                        LocalAttachment::Svg { filename, content } => self.paste_text(&content, &format!("[SVG: {filename}]")),
                        LocalAttachment::Binary { filename, path, mime, base64 } => self.paste_attachment(&filename, &path, mime, &base64),
                    }
                }
                Vec::new()
            }
            Action::Mouse(event) => self.mouse(event),
            Action::QuestionAsked(ask) => {
                self.questions.push_back(ask);
                Vec::new()
            }
            Action::ChildrenLoaded { session_id, children, status } => {
                if let Some(view) = self.session_mut().filter(|view| view.info.id == session_id) {
                    view.children = children;
                    // A session opened while its loop runs elsewhere (a subagent) shows as busy.
                    if !view.busy() && status != Status::Idle {
                        view.status = status;
                    }
                }
                Vec::new()
            }
            Action::PromptSet(prompt) => {
                self.restore(&prompt);
                Vec::new()
            }
            Action::SessionChanged { session_id, error } => {
                if let Some(error) = error {
                    self.show_toast(error, ToastKind::Error);
                }
                match self.session().filter(|view| view.info.id == session_id) {
                    Some(_) => vec![Effect::Reload { session_id, settled: true }],
                    None => Vec::new(),
                }
            }
            Action::EditorClosed { purpose, content } => self.editor_closed(purpose, content),
            Action::Focus(focused) => {
                self.focused = focused;
                Vec::new()
            }
            Action::Terminal(colors) => {
                self.set_terminal(colors);
                Vec::new()
            }
            Action::Resize(width, height) => {
                self.size = (width, height);
                Vec::new()
            }
            Action::Tick(now, now_ms) => {
                self.now = now;
                self.now_ms = now_ms;
                self.frame = self.frame.wrapping_add(1);
                if self.pending_at.is_some_and(|at| now.duration_since(at) >= Duration::from_millis(self.keymap.leader_timeout_ms)) {
                    self.pending.clear();
                    self.pending_at = None;
                }
                if self.interrupt_at.is_some_and(|at| now.duration_since(at) >= INTERRUPT_WINDOW) {
                    self.interrupt = 0;
                    self.interrupt_at = None;
                }
                if self.toast.as_ref().is_some_and(|toast| now >= toast.until) {
                    self.toast = None;
                }
                Vec::new()
            }
            Action::Engine(update) => self.engine(update),
            // `sync.tsx`: in auto mode a request is answered "once" as it arrives and never shown.
            Action::PermissionAsked(ask) if self.auto_permissions => vec![Effect::PermissionReply { id: ask.id, reply: Reply::Once }],
            Action::PermissionAsked(ask) => {
                self.permissions.push_back(ask);
                self.chime(crate::sound::Chime::Permission);
                Vec::new()
            }
            Action::SessionStarted { info, text, then } => {
                self.started(*info, text);
                vec![*then]
            }
            Action::Answered(id) => {
                if self.permission().is_some_and(|ask| ask.id == id) {
                    self.permission_choice = 0;
                    self.permission_stage = PermissionStage::Ask;
                    self.permission_fullscreen = false;
                    self.permission_feedback = Editor::default();
                }
                if self.question_ask().is_some_and(|ask| ask.id == id) {
                    self.question = QuestionState::default();
                }
                self.permissions.retain(|ask| ask.id != id);
                self.questions.retain(|ask| ask.id != id);
                Vec::new()
            }
            Action::Resync => match self.session() {
                Some(view) => vec![Effect::Reload { session_id: view.info.id.clone(), settled: true }],
                None => Vec::new(),
            },
            Action::Submit => self.submit(),
            Action::PromptFinished { session_id, error } => {
                self.running.retain(|id| *id != session_id);
                // An interruption the user asked for stays silent, however the run reports it.
                let stopped = self.interrupted.remove(&session_id);
                match &error {
                    _ if stopped => {}
                    Some(text) if text.to_lowercase().contains("abort") => {}
                    Some(_) => self.chime(crate::sound::Chime::Error),
                    None => self.chime(crate::sound::Chime::Done),
                }
                if let Some(error) = error {
                    self.show_toast(error, ToastKind::Error);
                }
                self.interrupt = 0;
                match self.session_mut().filter(|view| view.info.id == session_id) {
                    Some(view) => {
                        view.status = Status::Idle;
                        vec![Effect::Reload { session_id, settled: true }]
                    }
                    None => Vec::new(),
                }
            }
            Action::SessionLoaded { info, messages, todos, open, settled } => {
                self.repaint |= settled;
                self.session_loaded(*info, messages, todos, open, settled);
                Vec::new()
            }
            Action::SessionsListed(list) => {
                let current = self.session().map(|view| view.info.id.clone());
                self.dialog = Some(Dialog::Select(dialog::sessions(&list, &self.pins.pinned, current.as_deref(), self.now_ms)));
                self.listed = list;
                Vec::new()
            }
            Action::SessionDeleted(id) => {
                if let Some(Dialog::Select(select)) = &mut self.dialog
                    && select.kind == SelectKind::Sessions
                {
                    select.remove_value(&id);
                }
                self.listed.retain(|session| session.id != id);
                if self.session().is_some_and(|view| view.info.id == id) {
                    self.route = Route::Home;
                }
                // A deleted session leaves the pinned list.
                if self.pins.pinned.contains(&id) {
                    self.pins.toggle(&id);
                    return vec![Effect::SavePins];
                }
                Vec::new()
            }
            Action::FilesLoaded(files) => {
                self.files = files;
                self.refresh_autocomplete();
                Vec::new()
            }
            Action::Toast(message, kind) => {
                self.show_toast(message, kind);
                Vec::new()
            }
            Action::Aside { question, answer } => {
                match answer {
                    Ok(answer) => {
                        let subtitle = question.split_whitespace().collect::<Vec<_>>().join(" ");
                        self.dialog = Some(Dialog::Text(dialog::TextDialog { title: "/btw".into(), subtitle, body: answer.trim().to_string(), markdown: true, scroll: 0 }));
                        self.toast = None;
                    }
                    Err(error) => self.show_toast(error, ToastKind::Error),
                }
                Vec::new()
            }
            Action::Reloaded(result) => {
                match result {
                    Ok(catalog) => {
                        self.catalog = *catalog;
                        self.show_toast("Configuration reloaded", ToastKind::Info);
                    }
                    Err(error) => self.show_toast(error, ToastKind::Error),
                }
                Vec::new()
            }
            Action::Stats(result) => {
                match result {
                    Ok(body) => self.dialog = Some(Dialog::Text(dialog::TextDialog { title: "opencode / stats".into(), subtitle: String::new(), body, markdown: false, scroll: 0 })),
                    Err(error) => self.show_toast(format!("Could not load stats: {error}"), ToastKind::Error),
                }
                Vec::new()
            }
        }
    }

    fn session_loaded(&mut self, info: Session, messages: Vec<(Message, Vec<StoredPart>)>, todos: Vec<Todo>, open: bool, settled: bool) {
        if open {
            if let Some(agent) = info.agent.as_ref().filter(|name| self.catalog.primary_agents().iter().any(|a| &a.name == *name)) {
                self.agent = agent.clone();
            }
            if let Some(model) = info.model.as_ref().map(|m| ModelKey::new(&m.provider_id, &m.id)).filter(|k| self.catalog.is_valid(k)) {
                self.model_by_agent.insert(self.agent.clone(), model);
            }
            let mut view = SessionView::new(info, messages, todos);
            if self.running.contains(&view.info.id) {
                view.status = Status::Busy;
            }
            // Leaving a session mid-stream parks its live text; coming back picks it up again.
            if let Route::Session(old) = &mut self.route
                && !old.live_parts.is_empty()
            {
                let text: Vec<Live> = old.live.drain(..).filter(|live| !matches!(live, Live::Tool(_))).collect();
                self.streams.insert(old.info.id.clone(), (text, std::mem::take(&mut old.live_parts)));
            }
            if let Some((live, parts)) = self.streams.remove(&view.info.id) {
                view.live = live;
                view.live_parts = parts;
            }
            self.route = Route::Session(Box::new(view));
            self.dialog = None;
            return;
        }
        let Some(view) = self.session_mut().filter(|view| view.info.id == info.id) else { return };
        view.info = info;
        view.messages = messages;
        view.todos = todos;
        if settled {
            view.live.clear();
            view.live_parts.clear();
        } else {
            // Tool parts the store now holds are no longer live.
            let stored: Vec<&str> = view.messages.iter().flat_map(|(_, parts)| parts.iter().map(|p| p.id.as_str())).collect();
            view.live.retain(|live| !matches!(live, Live::Tool(part) if stored.contains(&part.id.as_str())));
        }
        if view.pending_user.as_ref().is_some_and(|(_, before)| view.user_count() > *before) {
            view.pending_user = None;
        }
    }

    fn engine(&mut self, update: Update) -> Vec<Effect> {
        // A subagent's updates arrive wrapped once per nesting level; the innermost session is the source.
        let mut source: Option<String> = None;
        let mut update = update;
        while let Update::Child { session_id, update: inner } = update {
            source = Some(session_id);
            update = *inner;
        }
        match update {
            Update::Text(_) => self.stream_reasoning = false,
            Update::Reasoning(_) => self.stream_reasoning = true,
            _ => {}
        }
        let reasoning = self.stream_reasoning;
        let running = self.running.clone();
        let on_screen = self.session().map(|view| view.info.id.clone());
        // Text of sessions that are not on screen is kept until their step ends, so opening one mid-stream
        // (a running subagent, say) shows what it has said so far.
        match &update {
            Update::Delta { session_id, part_id, delta, .. } if on_screen.as_ref() != Some(session_id) => {
                let (live, parts) = self.streams.entry(session_id.clone()).or_default();
                append_live(live, parts, part_id, delta, reasoning);
                return Vec::new();
            }
            Update::StepFinish { .. } => match &source {
                Some(source) => drop(self.streams.remove(source)),
                None => self.streams.retain(|session_id, _| !running.contains(session_id)),
            },
            Update::Status { session_id, status: Status::Idle } => drop(self.streams.remove(session_id)),
            _ => {}
        }
        let Some(view) = self.session_mut() else { return Vec::new() };
        let id = view.info.id.clone();
        let mut effects = Vec::new();
        let reload_pending = |view: &SessionView, effects: &mut Vec<Effect>| {
            if view.pending_user.is_some() {
                effects.push(Effect::Reload { session_id: view.info.id.clone(), settled: false });
            }
        };
        match update {
            Update::Delta { session_id, part_id, delta, .. } if session_id == id => {
                reload_pending(view, &mut effects);
                append_live(&mut view.live, &mut view.live_parts, &part_id, &delta, reasoning);
            }
            Update::Tool(part) if part.session_id == id => {
                reload_pending(view, &mut effects);
                let stored = view.messages.iter_mut().flat_map(|(_, parts)| parts.iter_mut()).find(|p| p.id == part.id);
                if let Some(slot) = stored {
                    *slot = *part;
                } else if let Some(slot) = view.live.iter_mut().find(|live| matches!(live, Live::Tool(p) if p.id == part.id)) {
                    *slot = Live::Tool(part);
                } else {
                    view.live.push(Live::Tool(part));
                }
            }
            Update::Tool(part) => match view.children.iter_mut().find(|child| child.info.id == part.session_id) {
                Some(child) => {
                    if let Some(tool) = ChildTool::from_part(&part) {
                        match child.tools.iter_mut().find(|t| t.id == tool.id) {
                            Some(slot) => *slot = tool,
                            None => child.tools.push(tool),
                        }
                    }
                }
                // A subagent session this view has not listed yet.
                None if source.is_some() => effects.push(Effect::Reload { session_id: id, settled: false }),
                None => {}
            },
            Update::StepFinish { .. } => {
                let mine = match &source {
                    Some(source) => *source == id,
                    None => running.contains(&id),
                };
                if mine {
                    effects.push(Effect::Reload { session_id: id, settled: true });
                }
            }
            Update::Status { session_id, status } if session_id == id => {
                let idle = status == Status::Idle;
                view.status = status;
                if idle {
                    effects.push(Effect::Reload { session_id: id, settled: true });
                }
            }
            Update::Status { session_id, status } => {
                let known = view.children.iter_mut().find(|child| child.info.id == session_id).map(|child| child.status = status.clone()).is_some();
                // A finished subagent has its duration in the store; an unknown one has to be listed.
                if source.is_some() && (!known || status == Status::Idle) {
                    effects.push(Effect::Reload { session_id: id, settled: false });
                }
            }
            Update::Title { session_id, title } if session_id == id => view.info.title = title,
            Update::Title { session_id, title } => {
                if let Some(child) = view.children.iter_mut().find(|child| child.info.id == session_id) {
                    child.info.title = title;
                }
            }
            Update::Compacted { session_id } if session_id == id => effects.push(Effect::Reload { session_id: id, settled: true }),
            Update::Todos { session_id, todos } if session_id == id => view.todos = todos,
            Update::Error { session_id, message } if session_id == id => self.show_toast(message, ToastKind::Error),
            _ => {}
        }
        effects
    }

    fn hit(&self, column: u16, row: u16) -> Option<HitTarget> {
        let hits = self.hits.borrow();
        hits.iter().rev().find(|hit| column >= hit.x && column < hit.x + hit.width && row >= hit.y && row < hit.y + hit.height).map(|hit| hit.target.clone())
    }

    // Moves the transcript so the scrollbar thumb sits under `row`.
    fn drag_scrollbar(&mut self, row: u16) {
        let Some((top, track)) = self.hits.borrow().iter().find(|hit| hit.target == HitTarget::Scrollbar).map(|hit| (hit.y, hit.height)) else { return };
        let (height, total) = self.viewport.get();
        let max = total.saturating_sub(height);
        let at = usize::from(row.saturating_sub(top)).min(usize::from(track.saturating_sub(1)));
        let span = usize::from(track.saturating_sub(1)).max(1);
        if let Some(view) = self.session_mut() {
            view.scroll = max - (max * at / span).min(max);
        }
    }

    fn mouse(&mut self, event: MouseEvent) -> Vec<Effect> {
        let step = self.config.scroll_speed.round().max(1.0) as i64;
        let delta = match event.kind {
            MouseEventKind::ScrollUp => step,
            MouseEventKind::ScrollDown => -step,
            MouseEventKind::Down(MouseButton::Left) => return self.click(event.column, event.row),
            MouseEventKind::Drag(MouseButton::Left) if self.dragging_scrollbar => {
                self.drag_scrollbar(event.row);
                return Vec::new();
            }
            MouseEventKind::Up(_) => {
                self.dragging_scrollbar = false;
                return Vec::new();
            }
            _ => return Vec::new(),
        };
        match &mut self.dialog {
            Some(Dialog::Select(select)) => {
                select.move_by(-delta.signum());
                self.preview_theme();
            }
            Some(_) => {}
            None => self.scroll_by(delta),
        }
        Vec::new()
    }

    fn click(&mut self, column: u16, row: u16) -> Vec<Effect> {
        let Some(target) = self.hit(column, row) else { return Vec::new() };
        match target {
            HitTarget::DialogRow(position) => {
                let Some(Dialog::Select(select)) = &mut self.dialog else { return Vec::new() };
                // First click moves the selection, a click on the selected row submits it.
                if select.selected == position {
                    return self.dialog_submit();
                }
                select.selected = position.min(select.filtered.len().saturating_sub(1));
                select.confirm_delete = None;
                self.preview_theme();
            }
            _ if self.dialog.is_some() => {}
            HitTarget::Tool(id) => {
                if !self.expanded.remove(&id) {
                    self.expanded.insert(id);
                }
                self.part_cache.borrow_mut().clear();
            }
            HitTarget::Session(id) => return vec![Effect::OpenSession(id)],
            HitTarget::Scrollbar => {
                self.dragging_scrollbar = true;
                self.drag_scrollbar(row);
            }
            HitTarget::Command(name) => return self.run(name),
        }
        Vec::new()
    }

    // Positive scrolls toward older output.
    pub(crate) fn scroll_by(&mut self, delta: i64) {
        let (height, total) = self.viewport.get();
        let max = total.saturating_sub(height) as i64;
        if let Some(view) = self.session_mut() {
            view.scroll = (view.scroll as i64 + delta).clamp(0, max) as usize;
        }
    }

    fn key(&mut self, event: KeyEvent) -> Vec<Effect> {
        let Some(stroke) = keybind::from_event(&event) else { return Vec::new() };
        if !self.pending.is_empty() {
            if stroke.name == "escape" && !stroke.ctrl {
                self.pending.clear();
                self.pending_at = None;
                return Vec::new();
            }
            if stroke.name == "backspace" {
                self.pending.pop();
                return Vec::new();
            }
            let mut sequence = std::mem::take(&mut self.pending);
            self.pending_at = None;
            sequence.push(Part::Key(stroke));
            return self.dispatch(&sequence, None);
        }
        if self.keymap.is_leader(&stroke) {
            self.pending = vec![Part::Leader];
            self.pending_at = Some(self.now);
            return Vec::new();
        }
        self.dispatch(&[Part::Key(stroke)], Some(event))
    }

    // Whether keys reach the prompt editor.
    pub fn prompt_focused(&self) -> bool {
        self.dialog.is_none() && self.permission().is_none() && self.question_ask().is_none() && !self.session().is_some_and(SessionView::is_child)
    }

    fn global_command(&self, sequence: &[Part]) -> Option<&'static str> {
        let in_session = self.session().is_some();
        GLOBAL.iter().copied().find(|name| self.keymap.is(name, sequence) && (in_session || !SESSION_ONLY.contains(name)) && (!in_session || *name != "tips_toggle"))
    }

    fn dispatch(&mut self, sequence: &[Part], event: Option<KeyEvent>) -> Vec<Effect> {
        // The which-key layer sits above everything else, dialogs included.
        if let Some(effects) = self.which_key_key(sequence) {
            return effects;
        }
        if self.dialog.is_some() {
            // Session list, new session and quick switch are bound outside every mode, dialogs included. Keys
            // that type into the dialog stay with it.
            if event.as_ref().and_then(keybind::printable).is_none()
                && let Some(name) = self.keymap.find(APP_GLOBAL, sequence)
            {
                self.close_dialog();
                return self.run(name);
            }
            return self.dialog_key(sequence, event);
        }
        if self.permission().is_some() {
            // Leader chords never belong to the prompt itself.
            if sequence.len() > 1
                && let Some(effects) = self.permission_fallback(sequence)
            {
                return effects;
            }
            return self.permission_key(sequence, event);
        }
        if self.question_ask().is_some() {
            return self.question_key(sequence, event);
        }
        if self.session().is_some_and(SessionView::is_child) {
            return self.child_key(sequence);
        }
        let keymap = self.keymap.clone();
        if self.shell_mode {
            let leave = matches!(sequence, [Part::Key(stroke)] if !stroke.ctrl && !stroke.alt && (stroke.name == "escape" || (stroke.name == "backspace" && self.editor.cursor == 0)));
            if leave {
                self.shell_mode = false;
                return Vec::new();
            }
        }
        if self.autocomplete.is_some()
            && let Some(effects) = self.autocomplete_key(sequence)
        {
            return effects;
        }
        // The popup pushes the `autocomplete` mode: base-mode bindings (app, session and prompt commands, exit,
        // interrupt, history) are off while it shows; the input's own bindings and the mode-less ones stay.
        let base = self.autocomplete.is_none();
        if base && !self.shell_mode && self.editor.cursor == 0 && matches!(event.as_ref().and_then(keybind::printable), Some('!')) {
            self.shell_mode = true;
            return Vec::new();
        }
        let empty = self.editor.is_empty();
        if base && empty && keymap.is("app_exit", sequence) {
            return self.run("app_exit");
        }
        if base && self.session().is_some_and(SessionView::busy) && keymap.is("session_interrupt", sequence) {
            return self.run("session_interrupt");
        }
        if keymap.is("input_paste", sequence) {
            return vec![Effect::ReadClipboard];
        }
        if base && empty && let Some(name) = keymap.find(&["messages_first", "messages_last"], sequence) {
            return self.run(name);
        }
        let width = self.prompt_text_width();
        let (row, _) = self.editor.cursor_position(width);
        let last_row = self.editor.rows(width).len().saturating_sub(1);
        if base && keymap.is("history_previous", sequence) && row == 0 {
            if let Some(entry) = self.history.step(-1, &self.editor.text.clone()) {
                self.restore(&entry);
                self.editor.buffer_home();
                return Vec::new();
            }
        } else if base && keymap.is("history_next", sequence) && row == last_row && let Some(entry) = self.history.step(1, &self.editor.text.clone()) {
            self.restore(&entry);
            return Vec::new();
        }
        if let Some(name) = keymap.find(INPUT, sequence) {
            return self.input(name);
        }
        if let Some(name) = keymap.find(&SCROLL[..6], sequence).or_else(|| keymap.find(APP_GLOBAL, sequence)) {
            return self.run(name);
        }
        if base && let Some(name) = self.global_command(sequence) {
            return self.run(name);
        }
        if let Some(c) = event.as_ref().and_then(keybind::printable) {
            self.editor.insert(c.encode_utf8(&mut [0; 4]));
            self.edited();
        }
        Vec::new()
    }

    pub(crate) fn restore(&mut self, entry: &Value) {
        self.editor.set(entry.get("input").and_then(Value::as_str).unwrap_or_default());
        self.attachments = entry.get("parts").and_then(Value::as_array).cloned().unwrap_or_default();
        self.shell_mode = entry.get("mode").and_then(Value::as_str) == Some("shell");
        self.autocomplete = None;
        self.autocomplete_dismissed = true;
    }

    pub(crate) fn edited(&mut self) {
        self.autocomplete_dismissed = false;
        self.interrupt = 0;
        self.refresh_autocomplete();
    }

    fn input(&mut self, name: &str) -> Vec<Effect> {
        let width = self.prompt_text_width();
        let editor = &mut self.editor;
        if let Some(motion) = name.strip_prefix("input_select_") {
            if motion == "all" {
                editor.select_all();
                return Vec::new();
            }
            editor.extend();
            match motion {
                "left" => editor.move_left(),
                "right" => editor.move_right(),
                "up" => {
                    if !editor.move_vertical(-1, width) {
                        editor.buffer_home();
                    }
                }
                "down" => {
                    if !editor.move_vertical(1, width) {
                        editor.buffer_end();
                    }
                }
                "line_home" => editor.line_home(),
                "line_end" => editor.line_end_move(),
                "visual_line_home" => editor.visual_line_home(width),
                "visual_line_end" => editor.visual_line_end(width),
                "buffer_home" => editor.buffer_home(),
                "buffer_end" => editor.buffer_end(),
                "word_forward" => editor.word_forward(),
                "word_backward" => editor.word_backward(),
                _ => {}
            }
            return Vec::new();
        }
        match name {
            "input_submit" | "prompt_submit" => return self.submit(),
            "prompt_queue" => {
                self.queueing = true;
                let effects = self.submit();
                self.queueing = false;
                return effects;
            }
            "input_newline" => editor.insert("\n"),
            "input_clear" => {
                // `clearPrompt`: a draft worth keeping goes to the history first.
                let keep = self.editor.text.trim().chars().count() >= 20 || !self.attachments.is_empty();
                let effects = if keep { self.record_history() } else { Vec::new() };
                self.editor.clear();
                self.attachments.clear();
                self.history.index = 0;
                self.edited();
                return effects;
            }
            "input_move_left" => editor.move_left(),
            "input_move_right" => editor.move_right(),
            "input_move_up" => {
                if !editor.move_vertical(-1, width) {
                    editor.buffer_home();
                }
            }
            "input_move_down" => {
                if !editor.move_vertical(1, width) {
                    editor.buffer_end();
                }
            }
            "input_line_home" => editor.line_home(),
            "input_line_end" => editor.line_end_move(),
            "input_visual_line_home" => editor.visual_line_home(width),
            "input_visual_line_end" => editor.visual_line_end(width),
            "input_buffer_home" => editor.buffer_home(),
            "input_buffer_end" => editor.buffer_end(),
            "input_delete_line" => editor.delete_line(),
            "input_delete_to_line_end" => editor.delete_to_line_end(),
            "input_delete_to_line_start" => editor.delete_to_line_start(),
            "input_backspace" => editor.backspace(),
            "input_delete" => editor.delete(),
            "input_undo" => editor.undo(),
            "input_redo" => editor.redo(),
            "input_word_forward" => editor.word_forward(),
            "input_word_backward" => editor.word_backward(),
            "input_delete_word_forward" => editor.delete_word_forward(),
            "input_delete_word_backward" => editor.delete_word_backward(),
            _ => {}
        }
        self.editor.anchor = None;
        self.edited();
        Vec::new()
    }

    fn slash_items(&self, query: &str) -> Vec<AcItem> {
        let mut items: Vec<(String, AcItem)> = Vec::new();
        let typed = query.trim().to_lowercase();
        for (key, title, _, slashes) in dialog::COMMANDS {
            let Some(primary) = slashes.first() else { continue };
            let aliases = dialog::aliases(key);
            let haystack = slashes.join(" ");
            // An alias from another agent that the typed text names is shown, so the match makes sense.
            let named = (!typed.is_empty() && !slashes.iter().any(|s| s.starts_with(&typed))).then(|| aliases.iter().find(|a| a.starts_with(&typed))).flatten();
            let description = match named {
                Some(alias) => format!("{title} · /{alias}"),
                None => title.to_string(),
            };
            items.push((haystack, AcItem { label: format!("/{primary}"), description, action: AcAction::Builtin(key) }));
        }
        for command in &self.catalog.commands {
            let item = AcItem {
                label: format!("/{}", command.name),
                description: command.description.clone().unwrap_or_default(),
                action: AcAction::Command(command.name.clone()),
            };
            items.push((command.name.clone(), item));
        }
        let mut order = fuzzy::filter(query, &items, |(haystack, _)| haystack.clone());
        // Rust build: another agent's name for a command matches by prefix, after the commands' own names.
        for (index, (_, item)) in items.iter().enumerate() {
            let aliased = !typed.is_empty()
                && matches!(item.action, AcAction::Builtin(key) if dialog::aliases(key).iter().any(|a| a.starts_with(&typed)));
            if aliased && !order.contains(&index) {
                order.push(index);
            }
        }
        order.into_iter().map(|i| items[i].1.clone()).collect()
    }

    fn file_items(&self, query: &str) -> Vec<AcItem> {
        let mut order = fuzzy::filter(query, &self.files, Clone::clone);
        // Recently and often attached files first; the original gets this ordering from its file search service.
        if !self.frecency.data.is_empty() {
            let score = |index: &usize| self.frecency.score(&self.directory.join(&self.files[*index]).to_string_lossy(), self.now_ms);
            order.sort_by(|a, b| score(b).partial_cmp(&score(a)).unwrap_or(std::cmp::Ordering::Equal));
        }
        order
            .into_iter()
            .take(AUTOCOMPLETE_LIMIT)
            .map(|i| AcItem { label: self.files[i].clone(), description: String::new(), action: AcAction::File(self.files[i].clone()) })
            .collect()
    }

    pub fn refresh_autocomplete(&mut self) {
        if self.autocomplete_dismissed {
            self.autocomplete = None;
            return;
        }
        let selected = self.autocomplete.as_ref().map_or(0, |ac| ac.selected);
        let (start, token) = self.editor.token_before_cursor();
        let next = if start == 0 && token.starts_with('/') {
            Some((start, self.slash_items(&token[1..])))
        } else {
            token.strip_prefix('@').map(|query| (start, self.file_items(query)))
        };
        self.autocomplete = next.filter(|(_, items)| !items.is_empty()).map(|(start, items)| Autocomplete {
            start,
            selected: selected.min(items.len() - 1),
            items,
        });
    }

    fn autocomplete_key(&mut self, sequence: &[Part]) -> Option<Vec<Effect>> {
        let keymap = self.keymap.clone();
        let ac = self.autocomplete.as_mut()?;
        let len = ac.items.len();
        if keymap.is("prompt.autocomplete.prev", sequence) {
            ac.selected = (ac.selected + len - 1) % len;
        } else if keymap.is("prompt.autocomplete.next", sequence) {
            ac.selected = (ac.selected + 1) % len;
        } else if keymap.is("prompt.autocomplete.hide", sequence) {
            self.autocomplete = None;
            self.autocomplete_dismissed = true;
        } else if keymap.is("prompt.autocomplete.select", sequence) || keymap.is("prompt.autocomplete.complete", sequence) {
            let item = ac.items[ac.selected].clone();
            let start = ac.start;
            return Some(self.complete(start, item));
        } else {
            return None;
        }
        Some(Vec::new())
    }

    fn complete(&mut self, start: usize, item: AcItem) -> Vec<Effect> {
        self.autocomplete = None;
        match item.action {
            AcAction::Builtin(key) => {
                self.editor.clear();
                self.attachments.clear();
                self.run(key)
            }
            AcAction::Command(name) => {
                self.editor.set(&format!("/{name} "));
                self.autocomplete_dismissed = true;
                Vec::new()
            }
            AcAction::File(path) => {
                let mention = format!("@{path}");
                self.editor.replace_range(start, self.editor.cursor, &format!("{mention} "));
                let absolute = self.directory.join(&path);
                self.attachments.push(json!({
                    "type": "file",
                    "mime": "text/plain",
                    "filename": path,
                    "url": file_url(&absolute),
                    "source": {
                        "type": "file",
                        "path": absolute.to_string_lossy(),
                        "text": { "start": start, "end": start + mention.len(), "value": mention },
                    },
                }));
                self.autocomplete_dismissed = true;
                vec![Effect::Frecency(self.frecency.update(&absolute.to_string_lossy(), self.now_ms))]
            }
        }
    }

    pub fn submit(&mut self) -> Vec<Effect> {
        if self.editor.text.trim().is_empty() {
            return Vec::new();
        }
        if matches!(self.editor.text.trim(), "exit" | "quit" | ":q") {
            return self.run("app_exit");
        }
        // `expandPastedTextPlaceholders`: summaries become the text they stand for.
        let mut expanded = self.editor.text.clone();
        for part in self.attachments.iter().filter(|part| part.get("type").and_then(Value::as_str) == Some("text")) {
            if let (Some(label), Some(full)) = (part.pointer("/source/text/value").and_then(Value::as_str), part.get("text").and_then(Value::as_str)) {
                expanded = expanded.replacen(label, full, 1);
            }
        }
        let text = expanded.trim().to_string();
        let session_id = self.session().map(|view| view.info.id.clone());
        let model = self.current_model().map(|key| key.spec());
        if self.shell_mode {
            let mut effects = self.record_history();
            self.shell_mode = false;
            self.begin(format!("!{text}"));
            effects.push(Effect::Shell { session_id, command: text, agent: self.agent.clone(), model });
            return effects;
        }
        if let Some(rest) = text.strip_prefix('/') {
            let (name, arguments) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            if name == "btw" && !self.catalog.commands.iter().any(|c| c.name == name) {
                return self.aside(arguments.trim());
            }
            if let Some(key) = dialog::slash_command(name).filter(|_| arguments.trim().is_empty()) {
                self.editor.clear();
                self.attachments.clear();
                return self.run(key);
            }
            if self.catalog.commands.iter().any(|c| c.name == name) {
                let mut effects = self.record_history();
                self.begin(text.clone());
                effects.push(Effect::Command { session_id, command: name.to_string(), arguments: arguments.trim().to_string(), agent: self.agent.clone(), model });
                return effects;
            }
        }
        if self.catalog.providers.is_empty() {
            self.show_toast("Connect a provider to send prompts", ToastKind::Warning);
            return Vec::new();
        }
        // Mentions deleted from the text no longer attach their file.
        let files: Vec<FilePart> = self
            .attachments
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("file"))
            .filter(|part| part.pointer("/source/text/value").and_then(Value::as_str).is_some_and(|mention| self.editor.text.contains(mention)))
            .filter_map(|part| serde_json::from_value(part.clone()).ok())
            .collect();
        let mut effects = self.record_history();
        if let Some(key) = self.current_model() {
            self.models.push_recent(&key);
            effects.push(Effect::SaveModels);
        }
        let variant = self.current_variant();
        // Rust build: while the agent works, Enter steers it (the engine reads the prompt at its next step);
        // `prompt_queue` holds the prompt until the agent is done.
        if let Some(session_id) = session_id.clone().filter(|_| self.queueing && self.session().is_some_and(SessionView::busy)) {
            self.editor.clear();
            self.attachments.clear();
            self.autocomplete = None;
            self.show_toast("Queued: sent when the agent is done", ToastKind::Info);
            effects.push(Effect::Queue { session_id, text, files, agent: self.agent.clone(), model, variant });
            return effects;
        }
        self.begin(text.clone());
        effects.push(Effect::Submit { session_id, text, files, agent: self.agent.clone(), model, variant });
        effects
    }

    // Rust build: `/btw <question>` (opencode 2.x `feature-plugins/prompt/btw.tsx`).
    fn aside(&mut self, question: &str) -> Vec<Effect> {
        let Some(session_id) = self.session().map(|view| view.info.id.clone()) else {
            self.show_toast("Open a session first", ToastKind::Warning);
            return Vec::new();
        };
        if question.is_empty() {
            self.show_toast("Usage: /btw <question>", ToastKind::Info);
            return Vec::new();
        }
        let mut effects = self.record_history();
        self.editor.clear();
        self.attachments.clear();
        self.autocomplete = None;
        self.show_toast("/btw: thinking…", ToastKind::Info);
        effects.push(Effect::Aside { session_id, question: question.to_string() });
        effects
    }

    fn record_history(&mut self) -> Vec<Effect> {
        let entry = json!({ "input": self.editor.text, "parts": self.attachments, "mode": if self.shell_mode { "shell" } else { "normal" } });
        match self.history.append(entry) {
            HistoryWrite::None => Vec::new(),
            write => vec![Effect::History(write)],
        }
    }

    fn mark_running(&mut self, id: String) {
        if !self.running.contains(&id) {
            self.running.push(id);
        }
    }

    // Shows the prompt as sent while the engine persists it.
    fn begin(&mut self, text: String) {
        self.editor.clear();
        self.attachments.clear();
        self.autocomplete = None;
        if let Some(view) = self.session_mut() {
            view.pending_user = Some((text, view.user_count()));
            view.status = Status::Busy;
            view.scroll = 0;
            let id = view.info.id.clone();
            self.mark_running(id);
        }
    }

    // The runtime created a session for a prompt sent from the home screen.
    pub fn started(&mut self, info: Session, text: String) {
        let mut view = SessionView::new(info, Vec::new(), Vec::new());
        view.pending_user = Some((text, 0));
        view.status = Status::Busy;
        self.mark_running(view.info.id.clone());
        self.route = Route::Session(Box::new(view));
    }

    fn cycle_agent(&mut self, direction: i64) {
        let agents: Vec<String> = self.catalog.primary_agents().iter().map(|a| a.name.clone()).collect();
        if agents.is_empty() {
            return;
        }
        let at = agents.iter().position(|name| *name == self.agent).unwrap_or(0) as i64;
        self.agent = agents[(at + direction).rem_euclid(agents.len() as i64) as usize].clone();
    }

    fn cycle_model(&mut self, list: &[ModelKey], direction: i64, favorites: bool) -> Vec<Effect> {
        let valid: Vec<ModelKey> = list.iter().filter(|k| self.catalog.is_valid(k)).cloned().collect();
        if valid.is_empty() {
            if favorites {
                self.show_toast("Add a favorite model to use this shortcut", ToastKind::Info);
            }
            return Vec::new();
        }
        let current = self.current_model();
        let next = match current.and_then(|c| valid.iter().position(|k| *k == c)) {
            Some(at) => (at as i64 + direction).rem_euclid(valid.len() as i64) as usize,
            None if favorites => {
                if direction > 0 {
                    0
                } else {
                    valid.len() - 1
                }
            }
            None => return Vec::new(),
        };
        self.model_by_agent.insert(self.agent.clone(), valid[next].clone());
        if favorites {
            self.models.push_recent(&valid[next]);
            return vec![Effect::SaveModels];
        }
        Vec::new()
    }

    // Runs a command by its keybind config key.
    pub fn run(&mut self, name: &str) -> Vec<Effect> {
        let (height, total) = self.viewport.get();
        let page = height.max(1) as i64;
        match name {
            "app_exit" => {
                self.quit = true;
                return vec![Effect::Quit];
            }
            "command_list" => self.dialog = Some(Dialog::Select(dialog::commands(self))),
            "help_show" => self.dialog = Some(Dialog::Select(dialog::help(&self.keymap))),
            "theme_list" => {
                self.theme_before_preview = Some(self.theme_name.clone());
                self.dialog = Some(Dialog::Select(dialog::themes(&self.themes.names(), &self.theme_name)));
            }
            "model_list" => self.dialog = Some(Dialog::Select(dialog::models(&self.catalog, &self.models, self.current_model().as_ref()))),
            "agent_list" => self.dialog = Some(Dialog::Select(dialog::agents(&self.catalog, &self.agent))),
            "session_list" => return vec![Effect::ListSessions],
            "session_new" => {
                self.route = Route::Home;
                self.dialog = None;
            }
            "session_rename" => match self.session() {
                Some(view) => self.dialog = Some(Dialog::Rename { session_id: view.info.id.clone(), editor: Editor::with_text(&view.info.title) }),
                None => self.show_toast("No session to rename", ToastKind::Info),
            },
            "session_aside" => return self.aside(""),
            "config_reload" => {
                self.show_toast("Reloading configuration…", ToastKind::Info);
                return vec![Effect::ReloadConfig];
            }
            "stats_show" => return vec![Effect::Stats],
            "session_compact" => {
                if let Some(view) = self.session_mut().filter(|view| !view.busy()) {
                    view.status = Status::Busy;
                    let session_id = view.info.id.clone();
                    self.mark_running(session_id.clone());
                    return vec![Effect::Compact { session_id }];
                }
            }
            "session_interrupt" => {
                let Some(session_id) = self.session().filter(|view| view.busy()).map(|view| view.info.id.clone()) else { return Vec::new() };
                self.interrupt += 1;
                self.interrupt_at = Some(self.now);
                if self.interrupt >= 2 {
                    self.interrupt = 0;
                    self.interrupt_at = None;
                    self.interrupted.insert(session_id.clone());
                    return vec![Effect::Abort { session_id }];
                }
            }
            "messages_undo" => {
                let Some(view) = self.session().filter(|view| !view.busy()) else { return Vec::new() };
                let last = view.visible_messages().filter(|(m, _)| matches!(m, Message::User(_))).last();
                let Some((message, parts)) = last else { return Vec::new() };
                let text: Vec<&str> = parts
                    .iter()
                    .filter_map(|p| match &p.body {
                        PartBody::Text(t) if t.synthetic != Some(true) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect();
                let effect = Effect::Undo { session_id: view.info.id.clone(), message_id: message.id().to_string() };
                self.editor.set(&text.join("\n\n"));
                return vec![effect];
            }
            "messages_redo" => {
                if let Some(view) = self.session().filter(|view| !view.busy() && view.info.revert.is_some()) {
                    return vec![Effect::Redo { session_id: view.info.id.clone() }];
                }
            }
            "messages_copy" => {
                let Some(view) = self.session() else { return Vec::new() };
                let Some((_, parts)) = view.visible_messages().filter(|(m, _)| matches!(m, Message::Assistant(_))).last() else {
                    self.show_toast("No assistant messages found", ToastKind::Error);
                    return Vec::new();
                };
                let texts: Vec<&str> = parts
                    .iter()
                    .filter_map(|p| match &p.body {
                        PartBody::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect();
                if texts.is_empty() {
                    self.show_toast("No text parts found in last assistant message", ToastKind::Error);
                    return Vec::new();
                }
                let text = texts.join("\n").trim().to_string();
                if text.is_empty() {
                    self.show_toast("No text content found in last assistant message", ToastKind::Error);
                    return Vec::new();
                }
                self.show_toast("Message copied to clipboard!", ToastKind::Info);
                return vec![Effect::Copy(text)];
            }
            "model_cycle_recent" | "model_cycle_recent_reverse" => {
                let list = self.models.recent.clone();
                return self.cycle_model(&list, if name.ends_with("reverse") { -1 } else { 1 }, false);
            }
            "model_cycle_favorite" | "model_cycle_favorite_reverse" => {
                let list = self.models.favorite.clone();
                return self.cycle_model(&list, if name.ends_with("reverse") { -1 } else { 1 }, true);
            }
            "agent_cycle" => self.cycle_agent(1),
            "agent_cycle_reverse" => self.cycle_agent(-1),
            "messages_page_up" => self.scroll_by(page),
            "messages_page_down" => self.scroll_by(-page),
            "messages_half_page_up" => self.scroll_by((page / 2).max(1)),
            "messages_half_page_down" => self.scroll_by(-(page / 2).max(1)),
            "messages_line_up" => self.scroll_by(1),
            "messages_line_down" => self.scroll_by(-1),
            "messages_first" => self.scroll_by(total as i64),
            "messages_last" => self.scroll_by(-(total as i64)),
            "display_thinking" => {
                self.show_thinking = !self.show_thinking;
                self.kv.set("thinking_mode", json!(if self.show_thinking { "show" } else { "hide" }));
                return vec![Effect::SaveKv];
            }
            "tool_details" => {
                self.show_details = !self.show_details;
                self.kv.set("tool_details_visibility", json!(self.show_details));
                return vec![Effect::SaveKv];
            }
            "tips_toggle" if self.session().is_none() => {
                let hidden = !self.kv.get_bool("tips_hidden", false);
                self.kv.set("tips_hidden", json!(hidden));
                self.tip = if hidden { None } else { self.tips.first().cloned() };
                return vec![Effect::SaveKv];
            }
            other => return self.run_extra(other),
        }
        Vec::new()
    }

    pub(crate) fn set_theme(&mut self, name: &str) {
        if self.theme_name != name {
            self.theme_name = name.to_string();
            self.theme = self.themes.resolve(name, self.mode);
            self.part_cache.borrow_mut().clear();
        }
    }

    // Live preview: the theme under the cursor, or the original when the query is empty.
    fn preview_theme(&mut self) {
        let Some(Dialog::Select(select)) = &self.dialog else { return };
        if select.kind != SelectKind::Themes {
            return;
        }
        let name = select.selected_item().map(|item| item.value.clone()).or_else(|| self.theme_before_preview.clone());
        if let Some(name) = name {
            self.set_theme(&name);
        }
    }

    fn close_dialog(&mut self) {
        if let Some(original) = self.theme_before_preview.take() {
            self.set_theme(&original);
        }
        self.dialog = None;
    }

    fn dialog_key(&mut self, sequence: &[Part], event: Option<KeyEvent>) -> Vec<Effect> {
        let keymap = self.keymap.clone();
        let stroke = match sequence {
            [Part::Key(stroke)] => Some(stroke.clone()),
            _ => None,
        };
        let is = |name: &str, ctrl: bool| stroke.as_ref().is_some_and(|s| s.name == name && s.ctrl == ctrl && !s.alt);
        if is("escape", false) || is("c", true) {
            self.close_dialog();
            return Vec::new();
        }
        match self.dialog.as_mut() {
            Some(Dialog::Rename { session_id, editor }) => {
                if keymap.is("dialog.prompt.submit", sequence) {
                    let title = editor.text.trim().to_string();
                    let session_id = session_id.clone();
                    self.dialog = None;
                    if title.is_empty() {
                        return Vec::new();
                    }
                    if let Some(view) = self.session_mut().filter(|view| view.info.id == session_id) {
                        view.info.title = title.clone();
                    }
                    return vec![Effect::RenameSession { session_id, title }];
                }
                edit_field(editor, &keymap, sequence, event.as_ref());
                Vec::new()
            }
            Some(Dialog::Export(_)) => self.export_key(sequence, event),
            Some(Dialog::Text(text)) => {
                let name = stroke.as_ref().filter(|s| !s.ctrl && !s.alt).map(|s| s.name.as_str()).unwrap_or_default();
                let last = text.body.lines().count().saturating_sub(1);
                match name {
                    "up" | "k" => text.scroll = text.scroll.saturating_sub(1),
                    "down" | "j" => text.scroll = (text.scroll + 1).min(last),
                    "pageup" => text.scroll = text.scroll.saturating_sub(10),
                    "pagedown" => text.scroll = (text.scroll + 10).min(last),
                    "home" => text.scroll = 0,
                    "end" => text.scroll = last,
                    "c" => {
                        let body = text.body.clone();
                        self.show_toast("Copied to clipboard", ToastKind::Info);
                        return vec![Effect::Copy(body)];
                    }
                    "return" => self.dialog = None,
                    _ => {}
                }
                Vec::new()
            }
            Some(Dialog::Select(select)) => {
                let page = 10;
                if keymap.is("dialog.select.submit", sequence) {
                    return self.dialog_submit();
                } else if keymap.is("dialog.select.prev", sequence) {
                    select.move_by(-1);
                } else if keymap.is("dialog.select.next", sequence) {
                    select.move_by(1);
                } else if keymap.is("dialog.select.page_up", sequence) {
                    select.move_by(-page);
                } else if keymap.is("dialog.select.page_down", sequence) {
                    select.move_by(page);
                } else if keymap.is("dialog.select.home", sequence) {
                    select.move_to(false);
                } else if keymap.is("dialog.select.end", sequence) {
                    select.move_to(true);
                } else if select.kind == SelectKind::Sessions && keymap.is("session_delete", sequence) {
                    let Some(id) = select.selected_item().map(|item| item.value.clone()) else { return Vec::new() };
                    if select.confirm_delete.as_deref() == Some(id.as_str()) {
                        select.confirm_delete = None;
                        return vec![Effect::DeleteSession(id)];
                    }
                    select.confirm_delete = Some(id);
                    return Vec::new();
                } else if select.kind == SelectKind::Sessions && keymap.is("session_pin_toggle", sequence) {
                    let Some(id) = select.selected_item().map(|item| item.value.clone()) else { return Vec::new() };
                    let (query, current) = (select.query.clone(), select.current.clone());
                    self.pins.toggle(&id);
                    let mut next = dialog::sessions(&self.listed, &self.pins.pinned, current.as_deref(), self.now_ms);
                    next.set_query(query);
                    // The cursor follows the session to its new place.
                    next.selected = next.filtered.iter().position(|&index| next.items[index].value == id).unwrap_or(0);
                    self.dialog = Some(Dialog::Select(next));
                    return vec![Effect::SavePins];
                } else if select.kind == SelectKind::Sessions && keymap.is("session_rename", sequence) {
                    if let Some(item) = select.selected_item() {
                        self.dialog = Some(Dialog::Rename { session_id: item.value.clone(), editor: Editor::with_text(&item.title) });
                    }
                    return Vec::new();
                } else if select.kind == SelectKind::Models && keymap.is("model_favorite_toggle", sequence) {
                    let Some(key) = select.selected_item().and_then(|item| ModelKey::parse(&item.value)) else { return Vec::new() };
                    let (query, position) = (select.query.clone(), select.selected);
                    self.models.toggle_favorite(&key);
                    let mut next = dialog::models(&self.catalog, &self.models, self.current_model().as_ref());
                    next.set_query(query);
                    next.selected = position.min(next.filtered.len().saturating_sub(1));
                    self.dialog = Some(Dialog::Select(next));
                    return vec![Effect::SaveModels];
                } else if select.kind == SelectKind::Stash && keymap.is("stash_delete", sequence) {
                    let Some(value) = select.selected_item().map(|item| item.value.clone()) else { return Vec::new() };
                    if select.confirm_delete.as_deref() != Some(value.as_str()) {
                        select.confirm_delete = Some(value);
                        return Vec::new();
                    }
                    self.stash.remove(value.parse().unwrap_or(usize::MAX));
                    self.dialog = (!self.stash.entries.is_empty()).then(|| Dialog::Select(dialog::stash(&self.stash.entries, self.now_ms)));
                    return vec![Effect::Stash(stash::Write::Rewrite)];
                } else if keymap.is("input_backspace", sequence) {
                    let mut query = select.query.clone();
                    query.pop();
                    select.set_query(query);
                } else if keymap.is("input_delete_word_backward", sequence) || keymap.is("input_delete_to_line_start", sequence) {
                    select.set_query(String::new());
                } else if let Some(c) = event.as_ref().and_then(keybind::printable) {
                    let mut query = select.query.clone();
                    query.push(c);
                    select.set_query(query);
                } else {
                    return Vec::new();
                }
                self.preview_theme();
                Vec::new()
            }
            None => Vec::new(),
        }
    }

    fn dialog_submit(&mut self) -> Vec<Effect> {
        let Some(Dialog::Select(select)) = &self.dialog else { return Vec::new() };
        let kind = select.kind;
        let context = select.context.clone();
        let Some(item) = select.selected_item().cloned() else { return Vec::new() };
        match kind {
            SelectKind::Timeline | SelectKind::Fork | SelectKind::MessageActions | SelectKind::Stash | SelectKind::Variants => self.dialog_submit_extra(kind, context, item),
            SelectKind::Sessions => {
                self.dialog = None;
                vec![Effect::OpenSession(item.value)]
            }
            SelectKind::Models => {
                self.dialog = None;
                let Some(key) = ModelKey::parse(&item.value) else { return Vec::new() };
                if !self.catalog.is_valid(&key) {
                    self.show_toast(format!("Model {} is not valid", item.value), ToastKind::Warning);
                    return Vec::new();
                }
                self.model_by_agent.insert(self.agent.clone(), key.clone());
                self.models.push_recent(&key);
                vec![Effect::SaveModels]
            }
            SelectKind::Agents => {
                self.dialog = None;
                self.agent = item.value;
                Vec::new()
            }
            SelectKind::Themes => {
                self.theme_before_preview = None;
                self.dialog = None;
                self.set_theme(&item.value);
                self.kv.set(KV_THEME, json!(item.value));
                vec![Effect::SaveKv]
            }
            SelectKind::Commands => {
                self.dialog = None;
                self.run(&item.value)
            }
            SelectKind::Help => {
                self.dialog = None;
                Vec::new()
            }
            SelectKind::SetupBash => {
                self.dialog = None;
                match item.value.as_str() {
                    "accept" => vec![Effect::SetupBash { accept: true }],
                    "never" => vec![Effect::SetupBash { accept: false }],
                    _ => Vec::new(),
                }
            }
        }
    }

    pub fn permission_options(&self) -> &'static [&'static str] {
        match self.permission_stage {
            PermissionStage::Ask => &["Allow once", "Allow always", "Reject"],
            PermissionStage::Always => &["Confirm", "Cancel"],
            PermissionStage::Reject => &[],
        }
    }

    fn permission_key(&mut self, sequence: &[Part], event: Option<KeyEvent>) -> Vec<Effect> {
        let keymap = self.keymap.clone();
        if let Some(name) = keymap.find(SCROLL, sequence) {
            return self.run(name);
        }
        let Some((id, from_child)) = self.permission().map(|ask| (ask.id, self.session().is_some_and(|view| view.child(&ask.session_id).is_some()))) else { return Vec::new() };
        let done = |app: &mut Self| {
            app.permissions.retain(|ask| ask.id != id);
            app.permission_choice = 0;
            app.permission_stage = PermissionStage::Ask;
            app.permission_fullscreen = false;
            app.permission_feedback = Editor::default();
        };
        let plain = |name: &str| matches!(sequence, [Part::Key(stroke)] if stroke.name == name && !stroke.ctrl && !stroke.alt);
        // A subagent's request can be rejected with instructions for the model.
        if self.permission_stage == PermissionStage::Reject {
            if plain("escape") {
                self.permission_stage = PermissionStage::Ask;
            } else if plain("return") {
                let message = self.permission_feedback.text.trim().to_string();
                done(self);
                return vec![if message.is_empty() { Effect::PermissionReply { id, reply: Reply::Reject } } else { Effect::PermissionFeedback { id, message } }];
            } else {
                edit_field(&mut self.permission_feedback, &keymap, sequence, event.as_ref());
            }
            return Vec::new();
        }
        if keymap.is("app_exit", sequence) && !keymap.is("input_delete", sequence) {
            return self.run("app_exit");
        }
        if keymap.is("permission.prompt.fullscreen", sequence) && self.permission_stage == PermissionStage::Ask {
            self.permission_fullscreen = !self.permission_fullscreen;
            return Vec::new();
        }
        let [Part::Key(KeyStroke { name, ctrl: false, alt: false, .. })] = sequence else { return Vec::new() };
        let count = self.permission_options().len();
        let reply = |app: &mut Self, reply: Reply| {
            done(app);
            vec![Effect::PermissionReply { id, reply }]
        };
        let reject = |app: &mut Self| {
            if from_child {
                app.permission_stage = PermissionStage::Reject;
                return Vec::new();
            }
            reply(app, Reply::Reject)
        };
        match (name.as_str(), self.permission_stage) {
            ("left" | "h", _) => self.permission_choice = (self.permission_choice + count - 1) % count,
            ("right" | "l" | "tab", _) => self.permission_choice = (self.permission_choice + 1) % count,
            ("return", PermissionStage::Ask) => match self.permission_choice {
                0 => return reply(self, Reply::Once),
                1 => {
                    self.permission_stage = PermissionStage::Always;
                    self.permission_choice = 0;
                }
                _ => return reject(self),
            },
            ("return", PermissionStage::Always) => {
                if self.permission_choice == 0 {
                    return reply(self, Reply::Always);
                }
                self.permission_stage = PermissionStage::Ask;
                self.permission_choice = 0;
            }
            ("escape", PermissionStage::Ask) => return reject(self),
            ("escape", PermissionStage::Always) => {
                self.permission_stage = PermissionStage::Ask;
                self.permission_choice = 0;
            }
            _ => {}
        }
        Vec::new()
    }

    // The permission prompt stays in the base mode, so app and session commands keep working beside it.
    fn permission_fallback(&mut self, sequence: &[Part]) -> Option<Vec<Effect>> {
        if self.permission_stage == PermissionStage::Reject {
            return None;
        }
        let name = self.global_command(sequence).filter(|name| !name.starts_with("prompt_") && *name != "editor_open")?;
        Some(self.run(name))
    }

    fn question_key(&mut self, sequence: &[Part], event: Option<KeyEvent>) -> Vec<Effect> {
        use crate::question::Outcome;
        let keymap = self.keymap.clone();
        let Some(ask) = self.question_ask().cloned() else { return Vec::new() };
        let plain = |name: &str| matches!(sequence, [Part::Key(stroke)] if stroke.name == name && !stroke.ctrl && !stroke.alt);
        let outcome = if self.question.editing.is_some() {
            if plain("escape") {
                self.question.editing = None;
                Outcome::Pending
            } else if plain("return") {
                self.question.submit_edit(&ask.request.questions)
            } else if keymap.is("input_clear", sequence) {
                self.question.clear_edit();
                Outcome::Pending
            } else {
                if let Some(editor) = &mut self.question.editing {
                    edit_field(editor, &keymap, sequence, event.as_ref());
                }
                Outcome::Pending
            }
        } else if let Some(name) = keymap.find(SCROLL, sequence).or_else(|| keymap.find(APP_GLOBAL, sequence)) {
            // Mode-less bindings work under the `question` mode too.
            return self.run(name);
        } else if keymap.is("app_exit", sequence) {
            Outcome::Reject
        } else if let [Part::Key(stroke)] = sequence {
            self.question.key(&ask.request, stroke)
        } else {
            Outcome::Pending
        };
        let answers = match outcome {
            Outcome::Pending => return Vec::new(),
            Outcome::Reply(answers) => Some(answers),
            Outcome::Reject => None,
        };
        self.questions.retain(|other| other.id != ask.id);
        self.question = QuestionState::default();
        vec![Effect::QuestionReply { id: ask.id, answers }]
    }

    // A subagent view has no prompt: it scrolls, walks between sessions and runs global commands.
    fn child_key(&mut self, sequence: &[Part]) -> Vec<Effect> {
        let keymap = self.keymap.clone();
        if let Some(name) = keymap.find(&["session_parent", "session_child_cycle", "session_child_cycle_reverse", "app_exit"], sequence) {
            return self.run(name);
        }
        if let Some(name) = keymap.find(SCROLL, sequence) {
            return self.run(name);
        }
        match self.global_command(sequence) {
            Some(name) if !name.starts_with("prompt_") && name != "editor_open" => self.run(name),
            _ => Vec::new(),
        }
    }

    pub fn which_key_visible(&self) -> bool {
        self.which_key.pinned || (self.which_key.layout == WhichKeyLayout::Overlay && self.which_key.pending_preview && !self.pending.is_empty())
    }

    fn which_key_key(&mut self, sequence: &[Part]) -> Option<Vec<Effect>> {
        let keymap = self.keymap.clone();
        if keymap.is("which_key_toggle", sequence) {
            self.which_key.pinned = !self.which_key.pinned;
            self.which_key.scroll = 0;
            return Some(Vec::new());
        }
        if keymap.is("which_key_layout_toggle", sequence) {
            self.which_key.layout = if self.which_key.layout == WhichKeyLayout::Dock { WhichKeyLayout::Overlay } else { WhichKeyLayout::Dock };
            self.kv.set("which_key_layout", json!(if self.which_key.layout == WhichKeyLayout::Dock { "dock" } else { "overlay" }));
            return Some(vec![Effect::SaveKv]);
        }
        if keymap.is("which_key_pending_toggle", sequence) {
            self.which_key.pending_preview = !self.which_key.pending_preview;
            self.kv.set("which_key_pending_preview", json!(self.which_key.pending_preview));
            return Some(vec![Effect::SaveKv]);
        }
        if !self.which_key.pinned {
            return None;
        }
        let groups = crate::panels::which_key_groups(self).len().max(1);
        let state = &mut self.which_key;
        match keymap.find(WHICH_KEY_PANEL, sequence)? {
            "which_key_group_previous" => (state.group, state.scroll) = ((state.group + groups - 1) % groups, 0),
            "which_key_group_next" => (state.group, state.scroll) = ((state.group + 1) % groups, 0),
            "which_key_scroll_up" => state.scroll = state.scroll.saturating_sub(1),
            "which_key_scroll_down" => state.scroll += 1,
            "which_key_page_up" => state.scroll = state.scroll.saturating_sub(8),
            "which_key_page_down" => state.scroll += 8,
            "which_key_home" => state.scroll = 0,
            _ => state.scroll = usize::MAX / 2,
        }
        Some(Vec::new())
    }

    // `pasteInputText`: long pastes collapse to a `[Pasted ~N lines]` placeholder that expands on submit.
    pub(crate) fn paste(&mut self, raw: &str) {
        let text = raw.replace("\r\n", "\n").replace('\r', "\n");
        match &mut self.dialog {
            Some(Dialog::Select(select)) => {
                let query = format!("{}{}", select.query, text.replace('\n', " "));
                select.set_query(query);
                self.preview_theme();
                return;
            }
            Some(Dialog::Rename { editor, .. }) => return editor.insert(&text.replace('\n', " ")),
            Some(Dialog::Export(export)) => return export.filename.insert(&text.replace('\n', " ")),
            Some(Dialog::Text(_)) => return,
            None => {}
        }
        if self.question_ask().is_some() {
            if let Some(editor) = &mut self.question.editing {
                editor.insert(&text.replace('\n', " "));
            }
            return;
        }
        if !self.prompt_focused() {
            return;
        }
        let trimmed = text.trim();
        let lines = trimmed.matches('\n').count() + 1;
        if (lines >= 3 || trimmed.chars().count() > 150) && self.paste_summary() {
            self.paste_text(trimmed, &format!("[Pasted ~{lines} lines]"));
        } else {
            self.editor.insert(&text);
            self.edited();
        }
    }

    // `pasteText`: `label` stands in the prompt for `text` until the prompt is sent.
    fn paste_text(&mut self, text: &str, label: &str) {
        let start = self.editor.cursor;
        self.editor.insert(&format!("{label} "));
        self.attachments.push(json!({
            "type": "text",
            "text": text,
            "source": { "text": { "start": start, "end": start + label.len(), "value": label } },
        }));
        self.edited();
    }

    // `pasteAttachment`: an image or PDF shown as `[Image N]` and sent as a data URL.
    pub(crate) fn paste_attachment(&mut self, filename: &str, path: &str, mime: &str, base64: &str) {
        let pdf = mime == "application/pdf";
        let count = self
            .attachments
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("file"))
            .filter(|part| part.get("mime").and_then(Value::as_str).is_some_and(|m| if pdf { m == "application/pdf" } else { m.starts_with("image/") }))
            .count();
        let label = if pdf { format!("[PDF {}]", count + 1) } else { format!("[Image {}]", count + 1) };
        let start = self.editor.cursor;
        self.editor.insert(&format!("{label} "));
        self.attachments.push(json!({
            "type": "file",
            "mime": mime,
            "filename": filename,
            "url": format!("data:{mime};base64,{base64}"),
            "source": { "type": "file", "path": path, "text": { "start": start, "end": start + label.len(), "value": label } },
        }));
        self.edited();
    }
}

// Adds a streamed chunk: a new part starts a new entry, further chunks of it extend the last one.
fn append_live(live: &mut Vec<Live>, parts: &mut Vec<String>, part_id: &str, delta: &str, reasoning: bool) {
    let fresh = parts.last().map(String::as_str) != Some(part_id);
    if fresh {
        parts.push(part_id.to_string());
    }
    match (live.last_mut(), reasoning) {
        (Some(Live::Text(text)), false) if !fresh => text.push_str(delta),
        (Some(Live::Reasoning(text)), true) if !fresh => text.push_str(delta),
        (_, false) => live.push(Live::Text(delta.to_string())),
        (_, true) => live.push(Live::Reasoning(delta.to_string())),
    }
}

// Keys of a single-line text field inside a dialog or prompt panel. True when the key was consumed.
pub(crate) fn edit_field(editor: &mut Editor, keymap: &Keymap, sequence: &[Part], event: Option<&KeyEvent>) -> bool {
    match keymap.find(INPUT, sequence) {
        Some("input_backspace") => editor.backspace(),
        Some("input_delete") => editor.delete(),
        Some("input_move_left") => editor.move_left(),
        Some("input_move_right") => editor.move_right(),
        Some("input_line_home" | "input_buffer_home") => editor.line_home(),
        Some("input_line_end" | "input_buffer_end") => editor.line_end_move(),
        Some("input_delete_word_backward") => editor.delete_word_backward(),
        Some("input_delete_word_forward") => editor.delete_word_forward(),
        Some("input_delete_to_line_start") => editor.delete_to_line_start(),
        Some("input_delete_to_line_end") => editor.delete_to_line_end(),
        Some("input_word_forward") => editor.word_forward(),
        Some("input_word_backward") => editor.word_backward(),
        Some("input_undo") => editor.undo(),
        Some("input_redo") => editor.redo(),
        Some(_) => return false,
        None => match event.and_then(keybind::printable) {
            Some(c) => editor.insert(c.encode_utf8(&mut [0; 4])),
            None => return false,
        },
    }
    true
}

#[cfg(test)]
pub mod testing {
    use super::*;
    use crate::catalog;
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    pub fn init(state: &Path) -> Init {
        Init {
            directory: state.join("project"),
            home: state.to_path_buf(),
            config: TuiConfig::default(),
            themes: Registry::builtin(),
            kv: Kv::load(state),
            models: ModelState::load(state),
            history: History::load(state),
            catalog: catalog::fixture(),
            model: None,
            agent: None,
            prompt: None,
            tips: vec!["Run {highlight}/compact{/highlight} to summarize long sessions near context limits".into()],
            branch: None,
            stash: Stash::load(state),
            frecency: Frecency::load(state),
            pins: Pins::load(state),
            caps: Caps::default(),
            disable_paste_summary: false,
        }
    }

    pub fn app() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let app = App::new(init(dir.path()));
        (dir, app)
    }

    pub fn key(code: KeyCode, modifiers: KeyModifiers) -> Action {
        Action::Key(KeyEvent::new(code, modifiers))
    }

    pub fn press(app: &mut App, code: KeyCode) -> Vec<Effect> {
        app.update(key(code, KeyModifiers::NONE))
    }

    pub fn ctrl(app: &mut App, c: char) -> Vec<Effect> {
        app.update(key(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    pub fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    pub fn session(id: &str, title: &str) -> Session {
        let mut session = Session::new(opencode_storage::NewSession {
            project_id: "global".into(),
            directory: "/project".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            title: Some(title.into()),
            ..Default::default()
        })
        .unwrap();
        if !id.is_empty() {
            session.id = id.into();
        }
        session
    }

    pub fn open(app: &mut App, info: Session, messages: Vec<(Message, Vec<StoredPart>)>) {
        app.update(Action::SessionLoaded { info: Box::new(info), messages, todos: Vec::new(), open: true, settled: true });
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    #[test]
    fn typing_and_submitting_from_home() {
        let (_dir, mut app) = app();
        type_text(&mut app, "hello");
        app.update(key(KeyCode::Char('j'), KeyModifiers::CONTROL));
        type_text(&mut app, "world");
        assert_eq!(app.editor.text, "hello\nworld");
        let effects = press(&mut app, KeyCode::Enter);
        assert_eq!(effects.len(), 3);
        assert!(matches!(&effects[0], Effect::History(HistoryWrite::Append(entry)) if entry["input"] == "hello\nworld"));
        assert_eq!(effects[1], Effect::SaveModels);
        assert_eq!(
            effects[2],
            Effect::Submit { session_id: None, text: "hello\nworld".into(), files: vec![], agent: "build".into(), model: Some("anthropic/claude-sonnet-4-5".into()), variant: None }
        );
        assert!(app.editor.is_empty());
        assert_eq!(app.models.recent, vec![ModelKey::new("anthropic", "claude-sonnet-4-5")]);
    }

    #[test]
    fn leader_sequences_and_timeout() {
        let (_dir, mut app) = app();
        ctrl(&mut app, 'x');
        assert!(app.leader_active());
        press(&mut app, KeyCode::Char('m'));
        assert!(!app.leader_active());
        assert!(matches!(&app.dialog, Some(Dialog::Select(d)) if d.kind == SelectKind::Models));
        press(&mut app, KeyCode::Esc);
        assert!(app.dialog.is_none());

        ctrl(&mut app, 'x');
        let later = app.now + Duration::from_millis(2100);
        app.update(Action::Tick(later, 0));
        assert!(!app.leader_active());
        press(&mut app, KeyCode::Char('m'));
        assert_eq!(app.editor.text, "m");

        ctrl(&mut app, 'x');
        press(&mut app, KeyCode::Esc);
        assert!(!app.leader_active());
        assert_eq!(ctrl(&mut app, 'x'), vec![]);
        assert_eq!(press(&mut app, KeyCode::Char('q')), vec![Effect::Quit]);
    }

    #[test]
    fn exit_keys_only_fire_on_an_empty_prompt() {
        let (_dir, mut app) = app();
        type_text(&mut app, "ab");
        assert_eq!(ctrl(&mut app, 'c'), vec![]);
        assert!(app.editor.is_empty());
        assert_eq!(ctrl(&mut app, 'c'), vec![Effect::Quit]);
        let (_dir, mut app) = self::app();
        type_text(&mut app, "ab");
        app.editor.buffer_home();
        ctrl(&mut app, 'd');
        assert_eq!(app.editor.text, "b");
        type_text(&mut app, "exit");
        app.editor.set("quit");
        assert_eq!(press(&mut app, KeyCode::Enter), vec![Effect::Quit]);
    }

    #[test]
    fn agent_cycle_and_model_picker_write_recents() {
        let (_dir, mut app) = app();
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.agent, "plan");
        app.update(key(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(app.agent, "build");
        ctrl(&mut app, 'x');
        press(&mut app, KeyCode::Char('m'));
        type_text(&mut app, "gpt");
        let effects = press(&mut app, KeyCode::Enter);
        assert_eq!(effects, vec![Effect::SaveModels]);
        assert_eq!(app.current_model(), Some(ModelKey::new("openai", "gpt-5")));
        assert_eq!(app.model_label(), ("GPT-5".to_string(), "OpenAI".to_string()));
        assert_eq!(app.models.recent[0], ModelKey::new("openai", "gpt-5"));
        // The choice belongs to the agent it was made for.
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.current_model(), Some(ModelKey::new("openai", "gpt-5")));
        app.models.recent.clear();
        assert_eq!(app.current_model(), Some(ModelKey::new("anthropic", "claude-sonnet-4-5")));
    }

    #[test]
    fn theme_picker_previews_and_reverts() {
        let (_dir, mut app) = app();
        let original = app.theme.clone();
        ctrl(&mut app, 'x');
        press(&mut app, KeyCode::Char('t'));
        type_text(&mut app, "dracula");
        assert_eq!(app.theme_name, "dracula");
        assert_ne!(app.theme, original);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.theme_name, "clay");
        assert_eq!(app.theme, original);

        ctrl(&mut app, 'x');
        press(&mut app, KeyCode::Char('t'));
        type_text(&mut app, "nord");
        assert_eq!(press(&mut app, KeyCode::Enter), vec![Effect::SaveKv]);
        assert_eq!(app.theme_name, "nord");
        assert_eq!((app.kv.get_str(KV_THEME), app.kv.get_str("theme")), (Some("nord"), None));
    }

    #[test]
    fn slash_and_file_completion() {
        let (_dir, mut app) = app();
        app.update(Action::FilesLoaded(vec!["src/main.rs".into(), "src/lib.rs".into(), "README.md".into()]));
        type_text(&mut app, "/mod");
        let ac = app.autocomplete.clone().unwrap();
        assert_eq!(ac.items[0].label, "/models");
        press(&mut app, KeyCode::Enter);
        assert!(matches!(&app.dialog, Some(Dialog::Select(d)) if d.kind == SelectKind::Models));
        assert!(app.editor.is_empty());
        press(&mut app, KeyCode::Esc);

        type_text(&mut app, "/in");
        assert_eq!(app.autocomplete.as_ref().unwrap().items[0].action, AcAction::Command("init".into()));
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.editor.text, "/init ");
        type_text(&mut app, "now");
        let effects = press(&mut app, KeyCode::Enter);
        assert!(effects.contains(&Effect::Command {
            session_id: None,
            command: "init".into(),
            arguments: "now".into(),
            agent: "build".into(),
            model: Some("anthropic/claude-sonnet-4-5".into())
        }));

        type_text(&mut app, "see @main");
        assert_eq!(app.autocomplete.as_ref().unwrap().items[0].label, "src/main.rs");
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.editor.text, "see @src/main.rs ");
        assert!(app.autocomplete.is_none());
        let effects = press(&mut app, KeyCode::Enter);
        let Some(Effect::Submit { files, text, .. }) = effects.last() else { panic!("no submit: {effects:?}") };
        assert_eq!(text, "see @src/main.rs");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename.as_deref(), Some("src/main.rs"));
        assert!(files[0].url.starts_with("file:///") && files[0].url.ends_with("/project/src/main.rs"));

        type_text(&mut app, "/the");
        press(&mut app, KeyCode::Esc);
        assert!(app.autocomplete.is_none());
        assert_eq!(app.editor.text, "/the");
    }

    #[test]
    fn history_recall_with_up_and_down() {
        let (_dir, mut app) = app();
        for text in ["first", "second"] {
            type_text(&mut app, text);
            press(&mut app, KeyCode::Enter);
        }
        press(&mut app, KeyCode::Up);
        assert_eq!(app.editor.text, "second");
        press(&mut app, KeyCode::Up);
        assert_eq!(app.editor.text, "first");
        press(&mut app, KeyCode::Down);
        assert_eq!(app.editor.text, "second");
        press(&mut app, KeyCode::Down);
        assert_eq!(app.editor.text, "");
    }

    #[test]
    fn streaming_updates_build_the_live_turn() {
        let (_dir, mut app) = app();
        open(&mut app, session("ses_1", "Demo"), Vec::new());
        type_text(&mut app, "do it");
        let effects = press(&mut app, KeyCode::Enter);
        assert!(matches!(effects.last(), Some(Effect::Submit { session_id: Some(id), .. }) if id == "ses_1"));
        let view = app.session().unwrap();
        assert!(view.busy());
        assert_eq!(view.pending_user.as_ref().unwrap().0, "do it");

        // Every chunk arrives twice: untagged, then tagged with its session and part.
        let chunk = |app: &mut App, session: &str, part: &str, reasoning: bool, text: &str| {
            let first = app.update(Action::Engine(if reasoning { Update::Reasoning(text.into()) } else { Update::Text(text.into()) }));
            assert_eq!(first, vec![]);
            app.update(Action::Engine(Update::Delta { session_id: session.into(), message_id: "msg_a".into(), part_id: part.into(), field: "text", delta: text.into() }))
        };
        let reload = chunk(&mut app, "ses_1", "prt_1", false, "Hel");
        assert_eq!(reload, vec![Effect::Reload { session_id: "ses_1".into(), settled: false }]);
        chunk(&mut app, "ses_1", "prt_1", false, "lo");
        chunk(&mut app, "ses_1", "prt_2", true, "hm");
        chunk(&mut app, "ses_1", "prt_3", false, "Bye");
        // Chunks of another session never show here, however many loops run.
        chunk(&mut app, "ses_other", "prt_9", false, "nope");
        let view = app.session().unwrap();
        assert_eq!(view.live, vec![Live::Text("Hello".into()), Live::Reasoning("hm".into()), Live::Text("Bye".into())]);
        assert_eq!(view.live_parts, vec!["prt_1".to_string(), "prt_2".to_string(), "prt_3".to_string()]);
        let done = app.update(Action::Engine(Update::Status { session_id: "ses_1".into(), status: Status::Idle }));
        assert_eq!(done, vec![Effect::Reload { session_id: "ses_1".into(), settled: true }]);
        app.update(Action::SessionLoaded { info: Box::new(session("ses_1", "Demo")), messages: Vec::new(), todos: Vec::new(), open: false, settled: true });
        let view = app.session().unwrap();
        assert!(view.live.is_empty() && view.live_parts.is_empty());
    }

    fn child(id: &str, parent: &str) -> Child {
        let mut info = session(id, "Find things (@general subagent)");
        info.parent_id = Some(parent.into());
        Child { info, tools: Vec::new(), status: Status::Idle, duration: 0 }
    }

    #[test]
    fn subagent_updates_feed_the_task_row_and_the_child_view() {
        use crate::transcript::testing::tool_part;
        let (_dir, mut app) = app();
        open(&mut app, session("ses_1", "Demo"), Vec::new());
        app.running = vec!["ses_1".into()];
        let wrap = |update: Update| Action::Engine(Update::Child { session_id: "ses_c".into(), update: Box::new(update) });
        let mut tool = tool_part("msg_c", "c1", "read", json!({"filePath": "/x"}), "", json!({}));
        tool.session_id = "ses_c".into();
        // An unknown subagent session makes the view list its children.
        let first = app.update(wrap(Update::Status { session_id: "ses_c".into(), status: Status::Busy }));
        assert_eq!(first, vec![Effect::Reload { session_id: "ses_1".into(), settled: false }]);
        app.update(Action::ChildrenLoaded { session_id: "ses_1".into(), children: vec![child("ses_c", "ses_1")], status: Status::Busy });
        assert_eq!(app.update(wrap(Update::Tool(Box::new(tool.clone())))), vec![]);
        assert_eq!(app.update(wrap(Update::Status { session_id: "ses_c".into(), status: Status::Busy })), vec![]);
        app.update(wrap(Update::Title { session_id: "ses_c".into(), title: "Renamed".into() }));
        let view = app.session().unwrap();
        assert_eq!(view.children[0].tools, vec![ChildTool { id: tool.id.clone(), tool: "read".into(), status: "completed", title: None }]);
        assert_eq!((view.children[0].status.clone(), view.children[0].info.title.as_str()), (Status::Busy, "Renamed"));
        // The child's own text and step ends do not touch the parent's transcript.
        app.update(wrap(Update::Text("child text".into())));
        app.update(wrap(Update::Delta { session_id: "ses_c".into(), message_id: "m".into(), part_id: "p".into(), field: "text", delta: "child text".into() }));
        assert_eq!(app.update(wrap(Update::StepFinish { reason: "stop".into(), tokens: Default::default() })), vec![]);
        assert!(app.session().unwrap().live.is_empty());
        // A nested subagent reports through two wrappers; the innermost session is the source.
        let nested = Action::Engine(Update::Child { session_id: "ses_c".into(), update: Box::new(Update::Child { session_id: "ses_d".into(), update: Box::new(Update::Status { session_id: "ses_d".into(), status: Status::Busy }) }) });
        assert_eq!(app.update(nested), vec![Effect::Reload { session_id: "ses_1".into(), settled: false }]);

        // Inside the child view the same updates are its own.
        open(&mut app, child("ses_c", "ses_1").info, Vec::new());
        app.update(wrap(Update::Text("child text".into())));
        app.update(wrap(Update::Delta { session_id: "ses_c".into(), message_id: "m".into(), part_id: "p".into(), field: "text", delta: "child text".into() }));
        assert_eq!(app.session().unwrap().live, vec![Live::Text("child text".into())]);
        assert_eq!(app.update(wrap(Update::StepFinish { reason: "stop".into(), tokens: Default::default() })), vec![Effect::Reload { session_id: "ses_c".into(), settled: true }]);
        // The parent's step end is not the child's.
        assert_eq!(app.update(Action::Engine(Update::StepFinish { reason: "stop".into(), tokens: Default::default() })), vec![]);
    }

    #[test]
    fn engine_updates_set_title_todos_status_and_errors() {
        let (_dir, mut app) = app();
        open(&mut app, session("ses_1", "Demo"), Vec::new());
        app.update(Action::Engine(Update::Title { session_id: "ses_1".into(), title: "Named".into() }));
        app.update(Action::Engine(Update::Todos { session_id: "ses_1".into(), todos: vec![Todo { content: "ship".into(), status: "pending".into(), priority: "high".into() }] }));
        app.update(Action::Engine(Update::Status { session_id: "ses_1".into(), status: Status::Retry { attempt: 2, message: "overloaded".into(), next: 5 } }));
        let view = app.session().unwrap();
        assert_eq!((view.info.title.as_str(), view.todos.len(), view.busy()), ("Named", 1, true));
        assert_eq!(app.update(Action::Engine(Update::Compacted { session_id: "ses_1".into() })), vec![Effect::Reload { session_id: "ses_1".into(), settled: true }]);
        app.update(Action::Engine(Update::Error { session_id: "ses_1".into(), message: "boom".into() }));
        assert_eq!(app.toast.as_ref().map(|t| (t.message.as_str(), t.kind)), Some(("boom", ToastKind::Error)));
        // Other sessions' updates are ignored.
        app.update(Action::Engine(Update::Title { session_id: "ses_2".into(), title: "Other".into() }));
        app.update(Action::Engine(Update::Error { session_id: "ses_2".into(), message: "elsewhere".into() }));
        assert_eq!(app.session().unwrap().info.title, "Named");
        assert_eq!(app.toast.as_ref().unwrap().message, "boom");
    }

    #[test]
    fn question_prompt_takes_the_keys_and_replies() {
        use crate::question::testing::{info, request};
        let (_dir, mut app) = app();
        open(&mut app, session("ses_1", "Demo"), Vec::new());
        app.update(Action::QuestionAsked(QuestionAsk { id: 5, request: request(vec![info("Color", &["Red", "Blue"], false)]) }));
        assert!(app.question_ask().is_some() && !app.prompt_focused());
        type_text(&mut app, "zz");
        assert!(app.editor.is_empty());
        press(&mut app, KeyCode::Down);
        assert_eq!(press(&mut app, KeyCode::Enter), vec![Effect::QuestionReply { id: 5, answers: Some(vec![vec!["Blue".into()]]) }]);
        assert!(app.questions.is_empty() && app.prompt_focused());

        // Custom answer: typed text goes to the answer field, ctrl+c clears it and then leaves it.
        app.update(Action::QuestionAsked(QuestionAsk { id: 6, request: request(vec![info("Color", &["Red"], false)]) }));
        press(&mut app, KeyCode::Char('2'));
        type_text(&mut app, "Teal");
        app.update(Action::Paste("ish".into()));
        assert_eq!(app.question.editing.as_ref().unwrap().text, "Tealish");
        assert_eq!(ctrl(&mut app, 'c'), vec![]);
        assert_eq!(app.question.editing.as_ref().unwrap().text, "");
        type_text(&mut app, "Green");
        assert_eq!(press(&mut app, KeyCode::Enter), vec![Effect::QuestionReply { id: 6, answers: Some(vec![vec!["Green".into()]]) }]);

        // Escape and the exit key both dismiss; a permission request is answered before a question.
        app.update(Action::QuestionAsked(QuestionAsk { id: 7, request: request(vec![info("Color", &["Red"], true)]) }));
        app.update(Action::QuestionAsked(QuestionAsk { id: 8, request: request(vec![info("Size", &["S"], false)]) }));
        app.update(Action::PermissionAsked(ask(9)));
        assert_eq!(press(&mut app, KeyCode::Enter), vec![Effect::PermissionReply { id: 9, reply: Reply::Once }]);
        assert_eq!(press(&mut app, KeyCode::Esc), vec![Effect::QuestionReply { id: 7, answers: None }]);
        assert_eq!(ctrl(&mut app, 'c'), vec![Effect::QuestionReply { id: 8, answers: None }]);
    }

    #[test]
    fn replies_errors_and_permissions_chime_unless_silenced() {
        use crate::sound::Chime;
        let (_dir, mut app) = app();
        let finished = |app: &mut App, error: Option<&str>| {
            app.update(Action::PromptFinished { session_id: "ses_1".into(), error: error.map(str::to_string) });
            std::mem::take(&mut app.chimes)
        };
        assert_eq!(finished(&mut app, None), [Chime::Done]);
        assert_eq!(finished(&mut app, Some("HTTP 500")), [Chime::Error]);
        assert_eq!(finished(&mut app, Some("The operation was aborted.")), []);
        app.update(Action::PermissionAsked(ask(1)));
        assert_eq!(std::mem::take(&mut app.chimes), [Chime::Permission]);
        // Stopped with Esc Esc: the run may still end without an error, and stays silent.
        app.interrupted.insert("ses_1".into());
        assert_eq!(finished(&mut app, None), []);
        assert_eq!(finished(&mut app, None), [Chime::Done]);
        app.run("sounds_toggle");
        assert!(!app.sounds_on());
        assert_eq!(finished(&mut app, None), []);
    }

    #[test]
    fn subagent_permission_can_be_rejected_with_feedback() {
        let (_dir, mut app) = app();
        open(&mut app, session("ses_1", "Demo"), Vec::new());
        app.update(Action::ChildrenLoaded { session_id: "ses_1".into(), children: vec![child("ses_c", "ses_1")], status: Status::Idle });
        let from_child = |id| PermissionAsk { session_id: "ses_c".into(), ..ask(id) };
        app.update(Action::PermissionAsked(from_child(1)));
        assert_eq!(press(&mut app, KeyCode::Esc), vec![]);
        assert_eq!(app.permission_stage, PermissionStage::Reject);
        type_text(&mut app, "use b.txt");
        assert_eq!(press(&mut app, KeyCode::Enter), vec![Effect::PermissionFeedback { id: 1, message: "use b.txt".into() }]);
        assert_eq!(app.permission_stage, PermissionStage::Ask);
        app.update(Action::PermissionAsked(from_child(2)));
        press(&mut app, KeyCode::Left);
        press(&mut app, KeyCode::Enter);
        assert_eq!(press(&mut app, KeyCode::Esc), vec![]);
        assert_eq!(app.permission_stage, PermissionStage::Ask);
        press(&mut app, KeyCode::Enter);
        assert_eq!(press(&mut app, KeyCode::Enter), vec![Effect::PermissionReply { id: 2, reply: Reply::Reject }]);
        // A request of a session that is not on screen waits.
        app.running = vec!["ses_1".into(), "ses_9".into()];
        app.update(Action::PermissionAsked(PermissionAsk { session_id: "ses_9".into(), ..ask(3) }));
        assert!(app.permission().is_none() && app.prompt_focused());
        ctrl(&mut app, 'f');
        app.update(Action::PermissionAsked(ask(4)));
        ctrl(&mut app, 'f');
        assert!(app.permission_fullscreen);
    }

    #[test]
    fn shell_mode_runs_the_prompt_as_a_command() {
        let (_dir, mut app) = app();
        open(&mut app, session("ses_1", "Demo"), Vec::new());
        press(&mut app, KeyCode::Char('!'));
        assert!(app.shell_mode && app.editor.is_empty());
        press(&mut app, KeyCode::Char('!'));
        assert_eq!(app.editor.text, "!");
        press(&mut app, KeyCode::Backspace);
        assert!(app.shell_mode);
        press(&mut app, KeyCode::Backspace);
        assert!(!app.shell_mode);
        press(&mut app, KeyCode::Char('!'));
        press(&mut app, KeyCode::Esc);
        assert!(!app.shell_mode);
        type_text(&mut app, "a!");
        assert_eq!((app.editor.text.as_str(), app.shell_mode), ("a!", false));
        app.editor.clear();
        type_text(&mut app, "!ls -la");
        let effects = press(&mut app, KeyCode::Enter);
        assert!(matches!(&effects[0], Effect::History(HistoryWrite::Append(entry)) if entry["mode"] == "shell" && entry["input"] == "ls -la"));
        assert_eq!(effects[1], Effect::Shell { session_id: Some("ses_1".into()), command: "ls -la".into(), agent: "build".into(), model: Some("anthropic/claude-sonnet-4-5".into()) });
        let view = app.session().unwrap();
        assert!(!app.shell_mode && view.busy() && view.pending_user.as_ref().unwrap().0 == "!ls -la");
        // Recalling the entry restores the mode.
        app.update(Action::PromptFinished { session_id: "ses_1".into(), error: None });
        press(&mut app, KeyCode::Up);
        assert_eq!((app.editor.text.as_str(), app.shell_mode), ("ls -la", true));
    }

    #[test]
    fn pastes_are_summarised_and_expanded_on_submit() {
        let (_dir, mut app) = app();
        app.update(Action::Paste("short text".into()));
        assert_eq!(app.editor.text, "short text");
        app.editor.clear();
        type_text(&mut app, "see ");
        app.update(Action::Paste("one\r\ntwo\rthree\n".into()));
        assert_eq!(app.editor.text, "see [Pasted ~3 lines] ");
        assert_eq!(app.attachments[0], json!({"type": "text", "text": "one\ntwo\nthree", "source": {"text": {"start": 4, "end": 21, "value": "[Pasted ~3 lines]"}}}));
        app.update(Action::Clipboard(Some(ClipboardContent::Image("aGk=".into()))));
        assert_eq!(app.editor.text, "see [Pasted ~3 lines] [Image 1] ");
        let effects = press(&mut app, KeyCode::Enter);
        let Some(Effect::Submit { text, files, .. }) = effects.last() else { panic!("no submit: {effects:?}") };
        assert_eq!(text, "see one\ntwo\nthree [Image 1]");
        assert_eq!((files.len(), files[0].mime.as_str(), files[0].url.as_str()), (1, "image/png", "data:image/png;base64,aGk="));
        assert!(matches!(&effects[0], Effect::History(HistoryWrite::Append(entry)) if entry["input"] == "see [Pasted ~3 lines] [Image 1] " && entry["mode"] == "normal"));

        // ctrl+v asks the runtime for the clipboard; with summaries off the text is inserted as is.
        assert_eq!(ctrl(&mut app, 'v'), vec![Effect::ReadClipboard]);
        app.run("app_toggle_paste_summary");
        let long = "x".repeat(200);
        app.update(Action::Clipboard(Some(ClipboardContent::Text(long.clone()))));
        assert_eq!(app.editor.text, long);
        // A long draft cleared with ctrl+c is kept in the history.
        let cleared = ctrl(&mut app, 'c');
        assert!(matches!(cleared.as_slice(), [Effect::History(HistoryWrite::Append(entry))] if entry["input"] == long.as_str()));
    }

    #[test]
    fn selection_keys_extend_and_replace() {
        let (_dir, mut app) = app();
        type_text(&mut app, "hello world");
        app.update(key(KeyCode::Left, KeyModifiers::SHIFT));
        app.update(key(KeyCode::Left, KeyModifiers::SHIFT));
        assert_eq!(app.editor.selected_text(), Some("ld"));
        app.update(key(KeyCode::Left, KeyModifiers::ALT | KeyModifiers::SHIFT));
        assert_eq!(app.editor.selected_text(), Some("world"));
        app.update(key(KeyCode::Home, KeyModifiers::SHIFT));
        assert_eq!(app.editor.selected_text(), Some("hello world"));
        press(&mut app, KeyCode::Right);
        assert_eq!(app.editor.selection(), None);
        app.update(key(KeyCode::End, KeyModifiers::SHIFT));
        app.update(key(KeyCode::Char('e'), KeyModifiers::CONTROL | KeyModifiers::SHIFT));
        assert_eq!(app.editor.selected_text(), Some("ello world"));
        type_text(&mut app, "i");
        assert_eq!(app.editor.text, "hi");
        app.update(key(KeyCode::Char('a'), KeyModifiers::SUPER));
        press(&mut app, KeyCode::Backspace);
        assert!(app.editor.is_empty());
    }

    #[test]
    fn which_key_panel_and_mouse_targets() {
        let (_dir, mut app) = app();
        assert!(!app.which_key_visible());
        app.update(key(KeyCode::Char('k'), KeyModifiers::CONTROL | KeyModifiers::ALT));
        assert!(app.which_key_visible());
        let groups = crate::panels::which_key_groups(&app);
        assert!(groups.iter().any(|(label, entries)| label == "Session" && entries.contains(&("ctrl+x n".to_string(), "Create a new session".to_string()))), "{groups:?}");
        app.update(key(KeyCode::Right, KeyModifiers::CONTROL | KeyModifiers::ALT));
        assert_eq!(app.which_key.group, 1);
        app.update(key(KeyCode::Down, KeyModifiers::CONTROL | KeyModifiers::ALT));
        assert_eq!(app.which_key.scroll, 1);
        assert_eq!(app.update(key(KeyCode::Char('k'), KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT)), vec![Effect::SaveKv]);
        assert_eq!(app.kv.get_str("which_key_layout"), Some("overlay"));
        app.update(key(KeyCode::Char('k'), KeyModifiers::CONTROL | KeyModifiers::ALT));
        assert!(!app.which_key_visible());
        // With the preview on, a pending leader shows only its continuations.
        app.update(key(KeyCode::Char('p'), KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT));
        ctrl(&mut app, 'x');
        assert!(app.which_key_visible());
        let pending = crate::panels::which_key_groups(&app);
        assert!(pending.iter().flat_map(|(_, entries)| entries).all(|(keys, _)| !keys.contains("ctrl+x")));
        assert!(pending.iter().flat_map(|(_, entries)| entries).any(|(keys, description)| keys == "m" && description == "List available models"));
        press(&mut app, KeyCode::Esc);

        // Clicks act on what the renderer recorded under the pointer.
        open(&mut app, session("ses_1", "Demo"), Vec::new());
        let click = |app: &mut App, x, y| app.update(Action::Mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x, row: y, modifiers: KeyModifiers::NONE }));
        *app.hits.borrow_mut() = vec![
            Hit { x: 0, y: 0, width: 10, height: 2, target: HitTarget::Tool("prt_1".into()) },
            Hit { x: 0, y: 5, width: 10, height: 1, target: HitTarget::Session("ses_c".into()) },
        ];
        assert_eq!(click(&mut app, 3, 1), vec![]);
        assert!(app.expanded.contains("prt_1"));
        click(&mut app, 3, 0);
        assert!(app.expanded.is_empty());
        assert_eq!(click(&mut app, 0, 5), vec![Effect::OpenSession("ses_c".into())]);
        assert_eq!(click(&mut app, 50, 50), vec![]);
        app.update(Action::SessionsListed(vec![session("ses_a", "Alpha"), session("ses_b", "Beta")]));
        *app.hits.borrow_mut() = vec![Hit { x: 0, y: 0, width: 10, height: 1, target: HitTarget::DialogRow(1) }];
        assert_eq!(click(&mut app, 1, 0), vec![]);
        assert_eq!(click(&mut app, 1, 0), vec![Effect::OpenSession("ses_b".into())]);
    }

    #[test]
    fn escape_twice_interrupts_a_busy_session() {
        let (_dir, mut app) = app();
        open(&mut app, session("ses_1", "Demo"), Vec::new());
        assert_eq!(press(&mut app, KeyCode::Esc), vec![]);
        assert_eq!(app.interrupt, 0);
        type_text(&mut app, "go");
        press(&mut app, KeyCode::Enter);
        assert_eq!(press(&mut app, KeyCode::Esc), vec![]);
        assert_eq!(app.interrupt, 1);
        let later = app.now + INTERRUPT_WINDOW;
        app.update(Action::Tick(later, 0));
        assert_eq!(app.interrupt, 0);
        press(&mut app, KeyCode::Esc);
        assert_eq!(press(&mut app, KeyCode::Esc), vec![Effect::Abort { session_id: "ses_1".into() }]);
        let finished = app.update(Action::PromptFinished { session_id: "ses_1".into(), error: None });
        assert_eq!(finished, vec![Effect::Reload { session_id: "ses_1".into(), settled: true }]);
        assert!(!app.session().unwrap().busy());
    }

    fn ask(id: u64) -> PermissionAsk {
        PermissionAsk {
            id,
            session_id: String::new(),
            request: PermissionRequest { permission: "bash".into(), patterns: vec!["rm *".into()], always: vec!["rm *".into()], metadata: json!({"command": "rm -rf build"}) },
        }
    }

    #[test]
    fn permission_prompt_replies() {
        let (_dir, mut app) = app();
        open(&mut app, session("ses_1", "Demo"), Vec::new());
        for id in 1..=3 {
            app.update(Action::PermissionAsked(ask(id)));
        }
        type_text(&mut app, "x");
        assert!(app.editor.is_empty());
        assert_eq!(press(&mut app, KeyCode::Enter), vec![Effect::PermissionReply { id: 1, reply: Reply::Once }]);
        press(&mut app, KeyCode::Right);
        assert_eq!(press(&mut app, KeyCode::Enter), vec![]);
        assert_eq!(app.permission_stage, PermissionStage::Always);
        assert_eq!(press(&mut app, KeyCode::Enter), vec![Effect::PermissionReply { id: 2, reply: Reply::Always }]);
        press(&mut app, KeyCode::Left);
        assert_eq!(app.permission_choice, 2);
        assert_eq!(press(&mut app, KeyCode::Enter), vec![Effect::PermissionReply { id: 3, reply: Reply::Reject }]);
        assert!(app.permissions.is_empty());
        app.update(Action::PermissionAsked(ask(4)));
        assert_eq!(press(&mut app, KeyCode::Esc), vec![Effect::PermissionReply { id: 4, reply: Reply::Reject }]);
    }

    #[test]
    fn session_list_delete_needs_two_presses_and_rename_edits() {
        let (_dir, mut app) = app();
        assert_eq!(app.update(key(KeyCode::Char('l'), KeyModifiers::NONE)), vec![]);
        app.editor.clear();
        ctrl(&mut app, 'x');
        assert_eq!(press(&mut app, KeyCode::Char('l')), vec![Effect::ListSessions]);
        app.update(Action::SessionsListed(vec![session("ses_a", "Alpha"), session("ses_b", "Beta")]));
        type_text(&mut app, "bet");
        assert_eq!(ctrl(&mut app, 'd'), vec![]);
        assert_eq!(ctrl(&mut app, 'd'), vec![Effect::DeleteSession("ses_b".into())]);
        app.update(Action::SessionDeleted("ses_b".into()));
        let Some(Dialog::Select(select)) = &app.dialog else { panic!("dialog closed") };
        assert_eq!(select.items.len(), 1);

        ctrl(&mut app, 'u');
        ctrl(&mut app, 'r');
        assert!(matches!(&app.dialog, Some(Dialog::Rename { editor, .. }) if editor.text == "Alpha"));
        type_text(&mut app, "!");
        assert_eq!(press(&mut app, KeyCode::Enter), vec![Effect::RenameSession { session_id: "ses_a".into(), title: "Alpha!".into() }]);

        ctrl(&mut app, 'x');
        press(&mut app, KeyCode::Char('l'));
        app.update(Action::SessionsListed(vec![session("ses_a", "Alpha")]));
        assert_eq!(press(&mut app, KeyCode::Enter), vec![Effect::OpenSession("ses_a".into())]);
    }

    #[test]
    fn sessions_can_be_pinned_and_quick_switched() {
        let (_dir, mut app) = app();
        ctrl(&mut app, 'x');
        press(&mut app, KeyCode::Char('l'));
        app.update(Action::SessionsListed(vec![session("ses_a", "Alpha"), session("ses_b", "Beta")]));
        press(&mut app, KeyCode::Down);
        assert_eq!(ctrl(&mut app, 'f'), vec![Effect::SavePins]);
        assert_eq!(app.pins.pinned, vec!["ses_b".to_string()]);
        let Some(Dialog::Select(select)) = &app.dialog else { panic!("dialog closed") };
        assert_eq!(select.items.iter().map(|i| (i.title.as_str(), i.category.as_str())).collect::<Vec<_>>()[0], ("Beta", "Pinned"));
        assert_eq!(select.selected_item().unwrap().value, "ses_b");
        // Deleting a pinned session unpins it.
        assert_eq!(app.update(Action::SessionDeleted("ses_b".into())), vec![Effect::SavePins]);
        assert!(app.pins.pinned.is_empty());
        assert_eq!(ctrl(&mut app, 'f'), vec![Effect::SavePins]);
        assert_eq!(app.pins.pinned, vec!["ses_a".to_string()]);
        press(&mut app, KeyCode::Esc);
        ctrl(&mut app, 'x');
        assert_eq!(press(&mut app, KeyCode::Char('3')), vec![Effect::QuickSwitch(3)]);
        assert_eq!(app.run("docs_open"), vec![Effect::OpenUrl("https://opencode.ai/docs".into())]);
    }

    #[test]
    fn keybind_overrides_apply() {
        let dir = tempfile::tempdir().unwrap();
        let mut init = init(dir.path());
        init.config = TuiConfig::from_value(&json!({"keybinds": {"model_list": "f5", "leader": "ctrl+o", "session_new": "<leader>z"}}));
        let mut app = App::new(init);
        press(&mut app, KeyCode::F(5));
        assert!(matches!(&app.dialog, Some(Dialog::Select(d)) if d.kind == SelectKind::Models));
        press(&mut app, KeyCode::Esc);
        open(&mut app, session("ses_1", "Demo"), Vec::new());
        ctrl(&mut app, 'o');
        press(&mut app, KeyCode::Char('z'));
        assert!(app.session().is_none());
    }

    #[test]
    fn implemented_keybinds_exist_in_the_table() {
        let known: Vec<&str> = keybind::definitions().iter().map(|d| d.name.as_str()).collect();
        for name in implemented_keybinds() {
            assert!(known.contains(&name), "{name} is not a keybind config key");
        }
    }

    // Rust build: Enter steers a busy session; `<leader>return` queues the prompt until the agent is done.
    #[test]
    fn a_busy_session_is_steered_or_queued() {
        let (_dir, mut app) = app();
        open(&mut app, session("ses_1", "Demo"), Vec::new());
        type_text(&mut app, "first");
        press(&mut app, KeyCode::Enter);
        assert!(app.session().unwrap().busy());
        type_text(&mut app, "also check tests");
        let steered = press(&mut app, KeyCode::Enter);
        assert!(matches!(steered.last(), Some(Effect::Submit { text, .. }) if text == "also check tests"), "{steered:?}");
        type_text(&mut app, "then deploy");
        ctrl(&mut app, 'x');
        let queued = press(&mut app, KeyCode::Enter);
        assert!(matches!(queued.last(), Some(Effect::Queue { session_id, text, .. }) if session_id == "ses_1" && text == "then deploy"), "{queued:?}");
        assert!(app.editor.is_empty());
        assert_eq!(app.session().unwrap().pending_user.as_ref().unwrap().0, "also check tests");
        // Idle, the queue key just sends.
        app.update(Action::PromptFinished { session_id: "ses_1".into(), error: None });
        type_text(&mut app, "now");
        ctrl(&mut app, 'x');
        assert!(matches!(press(&mut app, KeyCode::Enter).last(), Some(Effect::Submit { text, .. }) if text == "now"));
    }

    // Rust build: opencode 2.x slash commands.
    #[test]
    fn v2_slash_commands() {
        let (_dir, mut app) = app();
        type_text(&mut app, "/btw what is this?");
        assert!(press(&mut app, KeyCode::Enter).is_empty());
        assert_eq!(app.toast.as_ref().unwrap().message, "Open a session first");
        open(&mut app, session("ses_1", "Demo"), Vec::new());
        app.editor.set("/btw what is   this?");
        let effects = app.submit();
        assert_eq!(effects.last(), Some(&Effect::Aside { session_id: "ses_1".into(), question: "what is   this?".into() }));
        app.update(Action::Aside { question: "what is   this?".into(), answer: Ok(" A **demo**. ".into()) });
        let Some(Dialog::Text(text)) = &app.dialog else { panic!("no answer dialog") };
        assert_eq!((text.title.as_str(), text.subtitle.as_str(), text.body.as_str(), text.markdown), ("/btw", "what is this?", "A **demo**.", true));
        assert_eq!(press(&mut app, KeyCode::Char('c')), vec![Effect::Copy("A **demo**.".into())]);
        press(&mut app, KeyCode::Esc);
        assert!(app.dialog.is_none());
        assert!(app.session().unwrap().messages.is_empty(), "the answer is not part of the conversation");

        app.editor.set("/thinking");
        app.submit();
        assert!(app.toast.as_ref().is_some_and(|t| t.message.starts_with("No variants available")));
        app.editor.set("/effort");
        app.submit();
        app.editor.set("/reload");
        assert_eq!(app.submit(), vec![Effect::ReloadConfig]);
        app.update(Action::Reloaded(Err("nope".into())));
        assert_eq!(app.toast.as_ref().unwrap().message, "nope");
        app.editor.set("/stats");
        assert_eq!(app.submit(), vec![Effect::Stats]);
        app.update(Action::Stats(Ok("1.5K TOKENS".into())));
        assert!(matches!(&app.dialog, Some(Dialog::Text(text)) if text.body == "1.5K TOKENS" && !text.markdown));
        for (alias, key) in [("resume", "session_list"), ("continue", "session_list"), ("clear", "session_new"), ("q", "app_exit"), ("toggle-thinking", "display_thinking")] {
            assert_eq!(dialog::slash_command(alias), Some(key), "{alias}");
        }
    }
}
