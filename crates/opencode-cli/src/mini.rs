// `opencode --mini` / `opencode attach --mini` (`cli/cmd/run/*`): the transcript goes to the terminal's own
// scrollback as it happens, and a small footer at the bottom holds the prompt, the status line and the permission
// and question prompts (`run/types.ts`). It drives the same session backends as the full TUI, in-process or remote.
//
// Ported: the split-footer layout; user (`› `), assistant, reasoning (`Thinking: `), tool and error rows; prompts
// queued while a turn runs; the permission machine of `permission.shared.ts` (once / always with confirmation /
// reject with feedback); question prompts; esc twice to interrupt; ctrl+c to clear, twice to exit; `/exit`,
// `/quit`, `:q`, `/new`; prompt history; replay of a resumed session (`--no-replay`, `--replay-limit`); the entry
// and exit banners as plain text.
// Left out: the command palette with its model, variant, skill and queued-prompt menus, `/editor`, `/skills`,
// subagent tabs and background subagents, shell mode, markdown/code/diff rendering in scrollback, the logo art,
// themes and the TUI keybind config (keys are fixed), `--demo`.
use crate::ui::fail;
use anyhow::Result;
use opencode_core::paths::Paths;
use opencode_session::{Engine, PermissionAnswer, Status, Update};
use opencode_storage::{Message, Part, PartBody, Session, ToolState};
use opencode_tui::app::{Action, Effect, PermissionAsk, ToastKind};
use opencode_tui::backend::{Backend, LocalBackend, RemoteBackend, Turn};
use opencode_tui::catalog::{self, Catalog};
use opencode_tui::editor::Editor;
use opencode_tui::question::QuestionAsk;
use opencode_tui::state::ModelKey;
use opencode_tui::text;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;

pub const FOOTER_HEIGHT: u16 = 6;
// `SPLASH_TITLE_LIMIT`, `SPLASH_TITLE_FALLBACK`
const TITLE_LIMIT: usize = 50;
const TITLE_FALLBACK: &str = "Untitled session";

#[derive(Debug, Clone)]
pub struct Options {
    // Local mode: the project directory; attach mode: a directory on the server, passed through.
    pub directory: Option<String>,
    pub attach: Option<String>,
    pub password: Option<String>,
    pub username: Option<String>,
    pub resume: bool,
    pub session: Option<String>,
    pub fork: bool,
    pub model: Option<String>,
    pub agent: Option<String>,
    pub prompt: Option<String>,
    pub replay: bool,
    pub replay_limit: Option<f64>,
    pub demo: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { directory: None, attach: None, password: None, username: None, resume: false, session: None, fork: false, model: None, agent: None, prompt: None, replay: true, replay_limit: None, demo: false }
    }
}

// `EntryKind`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    System,
    User,
    Assistant,
    Reasoning,
    Tool,
    Error,
}

// One line committed to scrollback; `gap` puts a blank line before it.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub kind: Kind,
    pub text: String,
    pub gap: bool,
}

impl Row {
    fn new(kind: Kind, text: impl Into<String>, gap: bool) -> Self {
        Self { kind, text: text.into(), gap }
    }
}

// What the footer asks the runtime to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Prompt(String),
    Abort,
    Permission(u64, PermissionAnswer),
    Question(u64, Option<Vec<Vec<String>>>),
    New,
}

// `PermissionStage` / `PermissionOption` of `permission.shared.ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Permission,
    Always,
    Reject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    Once,
    Always,
    Reject,
    Confirm,
    Cancel,
}

fn choices(stage: Stage) -> &'static [Choice] {
    match stage {
        Stage::Permission => &[Choice::Once, Choice::Always, Choice::Reject],
        Stage::Always => &[Choice::Confirm, Choice::Cancel],
        Stage::Reject => &[],
    }
}

// `permissionLabel`
fn label(choice: Choice) -> &'static str {
    match choice {
        Choice::Once => "Allow once",
        Choice::Always => "Allow always",
        Choice::Reject => "Reject",
        Choice::Confirm => "Confirm",
        Choice::Cancel => "Cancel",
    }
}

struct Stream {
    part: String,
    reasoning: bool,
    text: String,
    lines: usize,
}

// Footer state and the scrollback rows waiting to be written.
pub struct Mini {
    pub session_id: Option<String>,
    pub title: Option<String>,
    titled: bool,
    pub agent: String,
    pub model: Option<String>,
    model_label: String,
    cwd: PathBuf,
    home: PathBuf,
    thinking: bool,
    pub editor: Editor,
    history: Vec<String>,
    history_at: Option<usize>,
    draft: String,
    pub running: bool,
    started_at: Option<Instant>,
    pub queued: VecDeque<String>,
    interrupt: bool,
    exiting: bool,
    pub permissions: VecDeque<PermissionAsk>,
    stage: Stage,
    choice: Choice,
    feedback: Editor,
    pub questions: VecDeque<QuestionAsk>,
    question_at: usize,
    question_choice: usize,
    answers: Vec<Vec<String>>,
    reasoning_next: bool,
    streams: Vec<Stream>,
    printed: HashSet<String>,
    pub rows: Vec<Row>,
    pub quit: bool,
}

fn is_exit(text: &str) -> bool {
    matches!(text.trim().to_lowercase().as_str(), "/exit" | "/quit" | ":q")
}

fn is_new(text: &str) -> bool {
    text.trim().eq_ignore_ascii_case("/new")
}

// `formatModelLabel` without a variant: model name, then provider name.
pub fn model_label(catalog: &Catalog, model: Option<&str>) -> String {
    let Some(key) = model.and_then(ModelKey::parse) else { return "Model default".to_string() };
    match catalog.model(&key) {
        Some((provider, model)) => format!("{} · {}", model.name, provider.name),
        None => format!("{} · {}", key.model_id, key.provider_id),
    }
}

impl Mini {
    pub fn new(agent: String, model: Option<String>, model_label: String, cwd: PathBuf, home: PathBuf) -> Self {
        Self {
            session_id: None,
            title: None,
            titled: false,
            agent,
            model,
            model_label,
            cwd,
            home,
            thinking: true,
            editor: Editor::default(),
            history: Vec::new(),
            history_at: None,
            draft: String::new(),
            running: false,
            started_at: None,
            queued: VecDeque::new(),
            interrupt: false,
            exiting: false,
            permissions: VecDeque::new(),
            stage: Stage::Permission,
            choice: Choice::Once,
            feedback: Editor::default(),
            questions: VecDeque::new(),
            question_at: 0,
            question_choice: 0,
            answers: Vec::new(),
            reasoning_next: false,
            streams: Vec::new(),
            printed: HashSet::new(),
            rows: Vec::new(),
            quit: false,
        }
    }

    pub fn open(&mut self, info: &Session) {
        self.session_id = Some(info.id.clone());
        self.title = Some(info.title.clone());
        self.titled = !info.has_default_title();
    }

    fn on_screen(&self, session_id: &str) -> bool {
        self.session_id.as_deref() == Some(session_id)
    }

    fn push(&mut self, kind: Kind, text: &str, gap: bool) {
        // The engine reports a failed turn both as a session error and as the run's result.
        if kind == Kind::Error && self.rows.last().is_some_and(|row| row.kind == Kind::Error && row.text == text) {
            return;
        }
        for (index, line) in text.split('\n').enumerate() {
            self.rows.push(Row::new(kind, line, gap && index == 0));
        }
    }

    // Complete lines of a streamed part go to scrollback at once; the rest waits for the part to end.
    fn append(&mut self, part: &str, delta: &str) {
        let reasoning = self.reasoning_next;
        if reasoning && !self.thinking {
            return;
        }
        if self.streams.last().is_some_and(|stream| stream.part != part) {
            self.flush();
        }
        if self.streams.is_empty() {
            self.streams.push(Stream { part: part.to_string(), reasoning, text: String::new(), lines: 0 });
        }
        let stream = self.streams.last_mut().expect("a stream");
        stream.text.push_str(delta);
        let mut out = Vec::new();
        while let Some(at) = stream.text.find('\n') {
            let line: String = stream.text.drain(..=at).collect();
            out.push((line.trim_end_matches(['\n', '\r']).to_string(), stream.lines == 0, stream.reasoning));
            stream.lines += 1;
        }
        for (line, first, reasoning) in out {
            self.stream_row(&line, first, reasoning);
        }
    }

    fn stream_row(&mut self, line: &str, first: bool, reasoning: bool) {
        if first && line.trim().is_empty() {
            return;
        }
        match (reasoning, first) {
            (true, true) => self.rows.push(Row::new(Kind::Reasoning, format!("Thinking: {line}"), true)),
            (true, false) => self.rows.push(Row::new(Kind::Reasoning, line, false)),
            (false, _) => self.rows.push(Row::new(Kind::Assistant, line, first)),
        }
    }

    fn flush(&mut self) {
        for stream in std::mem::take(&mut self.streams) {
            let rest = stream.text.trim_end();
            if !rest.is_empty() {
                let first = stream.lines == 0;
                for (index, line) in rest.split('\n').enumerate() {
                    self.stream_row(line, first && index == 0, stream.reasoning);
                }
            }
        }
    }

    fn tool(&mut self, part: &Part) {
        let PartBody::Tool(tool) = &part.body else { return };
        let info = crate::run::tool_inline(&self.cwd, tool);
        let description = info.description.as_ref().map_or_else(String::new, |d| format!(" {d}"));
        match &tool.state {
            ToolState::Completed(_) if self.printed.insert(part.id.clone()) => {
                self.flush();
                self.push(Kind::Tool, &format!("{} {}{description}", info.icon, info.title), info.block);
                if let Some(body) = info.body.as_ref().filter(|body| info.block && !body.trim().is_empty()) {
                    self.push(Kind::Tool, body, false);
                }
            }
            ToolState::Error(state) if self.printed.insert(part.id.clone()) => {
                self.flush();
                self.push(Kind::Tool, &format!("✗ {} failed{description}", info.title), false);
                self.push(Kind::Error, &state.error, false);
            }
            ToolState::Running(_) if tool.tool == "task" && self.printed.insert(format!("{}:running", part.id)) => {
                self.flush();
                self.push(Kind::Tool, &format!("{} {}{description}", info.icon, info.title), false);
            }
            _ => {}
        }
    }

    fn update(&mut self, update: Update) {
        // Subagent output stays in its own session; this port has no subagent tabs to show it in.
        if matches!(update, Update::Child { .. }) {
            return;
        }
        match update {
            Update::Text(_) => self.reasoning_next = false,
            Update::Reasoning(_) => self.reasoning_next = true,
            Update::Delta { session_id, part_id, delta, .. } if self.on_screen(&session_id) => self.append(&part_id, &delta),
            Update::Tool(part) if self.on_screen(&part.session_id) => self.tool(&part),
            Update::StepFinish { .. } => self.flush(),
            Update::Status { session_id, status: Status::Idle } if self.on_screen(&session_id) => self.flush(),
            Update::Title { session_id, title } if self.on_screen(&session_id) => {
                self.title = Some(title);
                self.titled = true;
            }
            Update::Error { session_id, message } if self.on_screen(&session_id) => {
                self.flush();
                self.push(Kind::Error, &message, true);
            }
            _ => {}
        }
    }

    pub fn action(&mut self, action: Action) -> Vec<Command> {
        match action {
            Action::Engine(update) => self.update(update),
            Action::PermissionAsked(ask) => self.permissions.push_back(ask),
            Action::QuestionAsked(ask) => self.questions.push_back(ask),
            Action::Answered(id) => {
                if self.permissions.front().is_some_and(|ask| ask.id == id) {
                    self.reset_permission();
                }
                if self.questions.front().is_some_and(|ask| ask.id == id) {
                    self.reset_question();
                }
                self.permissions.retain(|ask| ask.id != id);
                self.questions.retain(|ask| ask.id != id);
            }
            Action::SessionStarted { info, .. } => self.open(&info),
            Action::PromptFinished { session_id, error } if session_id.is_empty() || self.on_screen(&session_id) => {
                self.flush();
                if let Some(error) = error {
                    self.push(Kind::Error, &error, true);
                }
                self.running = false;
                self.started_at = None;
                self.interrupt = false;
                if let Some(next) = self.queued.pop_front() {
                    return self.begin(next);
                }
            }
            Action::Toast(message, ToastKind::Error) => self.push(Kind::Error, &message, true),
            _ => {}
        }
        Vec::new()
    }

    fn begin(&mut self, text: String) -> Vec<Command> {
        self.push(Kind::User, &format!("› {text}"), true);
        self.running = true;
        self.started_at = Some(Instant::now());
        vec![Command::Prompt(text)]
    }

    // A prompt typed while a turn runs waits for it.
    pub fn submit(&mut self) -> Vec<Command> {
        let text = self.editor.text.trim().to_string();
        if text.is_empty() {
            return Vec::new();
        }
        if self.history.last() != Some(&text) {
            self.history.push(text.clone());
        }
        self.history_at = None;
        self.editor.clear();
        if is_exit(&text) {
            self.quit = true;
            return Vec::new();
        }
        if is_new(&text) {
            return vec![Command::New];
        }
        if self.running {
            self.queued.push_back(text);
            return Vec::new();
        }
        self.begin(text)
    }

    fn history(&mut self, direction: i64) {
        if self.history.is_empty() {
            return;
        }
        let next = match (self.history_at, direction) {
            (None, -1) => {
                self.draft = self.editor.text.clone();
                Some(self.history.len() - 1)
            }
            (None, _) => return,
            (Some(at), -1) => Some(at.saturating_sub(1)),
            (Some(at), _) if at + 1 < self.history.len() => Some(at + 1),
            (Some(_), _) => None,
        };
        self.history_at = next;
        let text = next.map_or_else(|| self.draft.clone(), |at| self.history[at].clone());
        self.editor.set(&text);
    }

    fn reset_permission(&mut self) {
        self.stage = Stage::Permission;
        self.choice = Choice::Once;
        self.feedback = Editor::default();
    }

    fn reset_question(&mut self) {
        self.question_at = 0;
        self.question_choice = 0;
        self.answers.clear();
    }

    fn answer_permission(&mut self, answer: PermissionAnswer) -> Vec<Command> {
        let Some(ask) = self.permissions.pop_front() else { return Vec::new() };
        self.reset_permission();
        vec![Command::Permission(ask.id, answer)]
    }

    // `permissionRun`
    fn run_choice(&mut self, choice: Choice) -> Vec<Command> {
        match (self.stage, choice) {
            (Stage::Permission, Choice::Always) => {
                self.stage = Stage::Always;
                self.choice = Choice::Confirm;
            }
            (Stage::Permission, Choice::Reject) => {
                self.stage = Stage::Reject;
                self.choice = Choice::Reject;
            }
            (Stage::Permission, _) => return self.answer_permission(PermissionAnswer::Once),
            (Stage::Always, Choice::Cancel) => {
                self.stage = Stage::Permission;
                self.choice = Choice::Always;
            }
            (Stage::Always, _) => return self.answer_permission(PermissionAnswer::Always),
            (Stage::Reject, _) => {}
        }
        Vec::new()
    }

    fn permission_key(&mut self, key: KeyEvent) -> Vec<Command> {
        if self.stage == Stage::Reject {
            match key.code {
                // `permissionCancel`
                KeyCode::Esc => {
                    self.stage = Stage::Permission;
                    self.choice = Choice::Reject;
                }
                // `permissionReject`: the trimmed message, if any, goes back to the model.
                KeyCode::Enter => {
                    let message = self.feedback.text.trim().to_string();
                    return self.answer_permission(if message.is_empty() { PermissionAnswer::Reject } else { PermissionAnswer::Corrected(message) });
                }
                KeyCode::Char(c) => self.feedback.insert(&c.to_string()),
                KeyCode::Backspace => self.feedback.backspace(),
                KeyCode::Delete => self.feedback.delete(),
                KeyCode::Left => self.feedback.move_left(),
                KeyCode::Right => self.feedback.move_right(),
                _ => {}
            }
            return Vec::new();
        }
        let list = choices(self.stage);
        let at = list.iter().position(|choice| *choice == self.choice).unwrap_or(0);
        match key.code {
            KeyCode::Left => self.choice = list[(at + list.len() - 1) % list.len()],
            KeyCode::Right | KeyCode::Tab => self.choice = list[(at + 1) % list.len()],
            KeyCode::Enter => return self.run_choice(self.choice),
            // `permissionEscape`
            KeyCode::Esc if self.stage == Stage::Always => {
                self.stage = Stage::Permission;
                self.choice = Choice::Always;
            }
            KeyCode::Esc => {
                self.stage = Stage::Reject;
                self.choice = Choice::Reject;
            }
            _ => {}
        }
        Vec::new()
    }

    // Questions of one request are answered in turn, each by picking an option.
    fn question_key(&mut self, key: KeyEvent) -> Vec<Command> {
        let Some(ask) = self.questions.front() else { return Vec::new() };
        let (id, total) = (ask.id, ask.request.questions.len());
        let options: Vec<String> = ask.request.questions.get(self.question_at).map(|q| q.options.iter().map(|o| o.label.clone()).collect()).unwrap_or_default();
        let count = options.len().max(1);
        let pick = match key.code {
            KeyCode::Up => {
                self.question_choice = (self.question_choice + count - 1) % count;
                None
            }
            KeyCode::Down | KeyCode::Tab => {
                self.question_choice = (self.question_choice + 1) % count;
                None
            }
            KeyCode::Char(c) if c.to_digit(10).is_some_and(|d| d >= 1 && (d as usize) <= options.len()) => Some(c.to_digit(10).unwrap_or(1) as usize - 1),
            KeyCode::Enter => Some(self.question_choice),
            KeyCode::Esc => {
                self.questions.pop_front();
                self.reset_question();
                return vec![Command::Question(id, None)];
            }
            _ => None,
        };
        let Some(pick) = pick else { return Vec::new() };
        self.answers.push(options.get(pick).cloned().into_iter().collect());
        self.question_at += 1;
        self.question_choice = 0;
        if self.question_at < total {
            return Vec::new();
        }
        self.questions.pop_front();
        let answers = std::mem::take(&mut self.answers);
        self.reset_question();
        vec![Command::Question(id, Some(answers))]
    }

    pub fn key(&mut self, key: KeyEvent) -> Vec<Command> {
        if key.kind == KeyEventKind::Release {
            return Vec::new();
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            // `prompt.clear`: a typed prompt is cleared first; on an empty one a second press exits.
            if !self.editor.is_empty() && self.permissions.is_empty() && self.questions.is_empty() {
                self.editor.clear();
                return Vec::new();
            }
            if self.exiting {
                self.quit = true;
            }
            self.exiting = true;
            return Vec::new();
        }
        self.exiting = false;
        if !self.permissions.is_empty() {
            return self.permission_key(key);
        }
        if !self.questions.is_empty() {
            return self.question_key(key);
        }
        if key.code != KeyCode::Esc {
            self.interrupt = false;
        }
        match key.code {
            KeyCode::Enter if key.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) => self.editor.insert("\n"),
            KeyCode::Enter => return self.submit(),
            // `session.interrupt`: the first press arms, the second aborts.
            KeyCode::Esc if self.running => {
                if self.interrupt {
                    self.interrupt = false;
                    return vec![Command::Abort];
                }
                self.interrupt = true;
            }
            KeyCode::Char(c) if !ctrl => self.editor.insert(&c.to_string()),
            KeyCode::Char('u') => self.editor.delete_to_line_start(),
            KeyCode::Char('w') => self.editor.delete_word_backward(),
            KeyCode::Backspace => self.editor.backspace(),
            KeyCode::Delete => self.editor.delete(),
            KeyCode::Left => self.editor.move_left(),
            KeyCode::Right => self.editor.move_right(),
            KeyCode::Home => self.editor.line_home(),
            KeyCode::End => self.editor.line_end_move(),
            KeyCode::Up => self.history(-1),
            KeyCode::Down => self.history(1),
            _ => {}
        }
        Vec::new()
    }

    pub fn paste(&mut self, text: &str) {
        if self.permissions.is_empty() && self.questions.is_empty() {
            self.editor.insert(&text.replace("\r\n", "\n"));
        }
    }

    // A resumed session's history, newest `limit` messages, written before the footer takes over.
    pub fn replay(&mut self, messages: &[(Message, Vec<Part>)], limit: Option<usize>) {
        let start = limit.map_or(0, |limit| messages.len().saturating_sub(limit));
        for (message, parts) in &messages[start..] {
            match message {
                Message::User(_) => {
                    let text: Vec<&str> = parts.iter().filter_map(|part| match &part.body {
                        PartBody::Text(text) if !text.synthetic.unwrap_or(false) => Some(text.text.as_str()),
                        _ => None,
                    }).collect();
                    let text = text.join("\n");
                    if !text.trim().is_empty() {
                        self.push(Kind::User, &format!("› {}", text.trim()), true);
                    }
                }
                Message::Assistant(_) => {
                    for part in parts {
                        match &part.body {
                            PartBody::Text(text) if !text.text.trim().is_empty() => self.push(Kind::Assistant, text.text.trim(), true),
                            PartBody::Reasoning(reasoning) if self.thinking && !reasoning.text.trim().is_empty() => self.push(Kind::Reasoning, &format!("Thinking: {}", reasoning.text.trim()), true),
                            PartBody::Tool(_) => self.tool(part),
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    // The exit banner (`exitSplash`): the session and how to come back to it.
    pub fn farewell(&mut self) {
        let Some(id) = self.session_id.clone() else { return };
        let title = match (&self.title, self.titled) {
            (Some(title), true) => Some(title.clone()),
            _ => self.history.iter().find(|item| !item.trim().is_empty() && !is_exit(item)).cloned(),
        };
        if let Some(title) = title {
            let title = if title.chars().count() > TITLE_LIMIT { format!("{}…", title.chars().take(TITLE_LIMIT - 1).collect::<String>()) } else { title };
            self.rows.push(Row::new(Kind::System, format!("Session  {}", if title.is_empty() { TITLE_FALLBACK.to_string() } else { title }), true));
        }
        self.rows.push(Row::new(Kind::System, format!("Continue opencode --mini -s {id}"), false));
    }

    fn status(&self) -> String {
        if self.exiting {
            return "Press ctrl+c again to exit".to_string();
        }
        let mut parts = Vec::new();
        if self.running {
            parts.push(if self.interrupt { "esc again to interrupt".to_string() } else { "esc interrupt".to_string() });
            if let Some(started) = self.started_at {
                parts.push(text::duration(started.elapsed().as_millis() as u64));
            }
        }
        if !self.queued.is_empty() {
            parts.push(format!("{} queued", self.queued.len()));
        }
        parts.join(" · ")
    }

    // The footer as plain styled lines: the prompt, or the permission or question in front.
    pub fn footer(&self, width: u16) -> (Vec<Line<'static>>, Option<(u16, u16)>) {
        let width = usize::from(width.max(10));
        let muted = Style::default().fg(Color::DarkGray);
        let rule = |color: Color| Line::from(Span::styled("─".repeat(width), Style::default().fg(color)));
        if let Some(ask) = self.permissions.front() {
            let (icon, title, info) = permission_info(&ask.request, &self.cwd, &self.home);
            let mut lines = vec![rule(Color::Yellow)];
            let heading = match self.stage {
                Stage::Permission => "Permission required",
                Stage::Always => "Always allow",
                Stage::Reject => "Reject permission",
            };
            lines.push(Line::from(vec![Span::styled("△ ", Style::default().fg(Color::Yellow)), Span::styled(heading, Style::default().add_modifier(Modifier::BOLD))]));
            lines.push(Line::from(vec![Span::styled(format!("{icon} "), muted), Span::raw(text::truncate(&title, width.saturating_sub(2)))]));
            let detail: Vec<String> = match self.stage {
                Stage::Always if ask.request.always.len() == 1 && ask.request.always[0] == "*" => vec![format!("This will allow {} until OpenCode is restarted.", ask.request.permission)],
                Stage::Always => std::iter::once("This will allow the following patterns until OpenCode is restarted.".to_string()).chain(ask.request.always.iter().map(|p| format!("- {p}"))).collect(),
                _ => info,
            };
            let mut cursor = None;
            if self.stage == Stage::Reject {
                let typed = if self.feedback.is_empty() { Span::styled("Tell OpenCode what to do differently", muted) } else { Span::raw(self.feedback.text.clone()) };
                cursor = Some((2 + text::width(&self.feedback.text[..self.feedback.cursor]) as u16, lines.len() as u16));
                lines.push(Line::from(vec![Span::raw("› "), typed]));
            } else {
                lines.extend(detail.iter().take(2).map(|line| Line::from(Span::styled(text::truncate(line, width), muted))));
            }
            while lines.len() < usize::from(FOOTER_HEIGHT) - 1 {
                lines.push(Line::default());
            }
            let mut options = Vec::new();
            for choice in choices(self.stage) {
                let style = if *choice == self.choice { Style::default().fg(Color::Black).bg(Color::Yellow) } else { muted };
                options.push(Span::styled(format!(" {} ", label(*choice)), style));
                options.push(Span::raw(" "));
            }
            lines.truncate(usize::from(FOOTER_HEIGHT) - 1);
            lines.push(Line::from(options));
            return (lines, cursor);
        }
        if let Some(ask) = self.questions.front() {
            let mut lines = vec![rule(Color::Cyan)];
            if let Some(question) = ask.request.questions.get(self.question_at) {
                let total = ask.request.questions.len();
                let count = if total > 1 { format!(" ({}/{total})", self.question_at + 1) } else { String::new() };
                lines.push(Line::from(vec![Span::styled(format!("{}{count}", question.header), Style::default().add_modifier(Modifier::BOLD))]));
                lines.push(Line::from(text::truncate(&question.question, width)));
                let room = usize::from(FOOTER_HEIGHT) - lines.len();
                let first = self.question_choice.saturating_sub(room.saturating_sub(1));
                for (index, option) in question.options.iter().enumerate().skip(first).take(room) {
                    let selected = index == self.question_choice;
                    let marker = if selected { "› " } else { "  " };
                    let style = if selected { Style::default().fg(Color::Cyan) } else { Style::default() };
                    let mut spans = vec![Span::styled(format!("{marker}{}. {}", index + 1, option.label), style)];
                    if !option.description.is_empty() {
                        spans.push(Span::styled(format!("  {}", option.description), muted));
                    }
                    lines.push(Line::from(spans));
                }
            }
            return (lines, None);
        }
        let mut lines = vec![rule(Color::DarkGray)];
        let rows = self.editor.rows(width.saturating_sub(2).max(1));
        let (row, column) = self.editor.cursor_position(width.saturating_sub(2).max(1));
        let room = usize::from(FOOTER_HEIGHT) - 3;
        let first = row.saturating_sub(room - 1);
        for (index, span) in rows.iter().enumerate().skip(first).take(room) {
            let lead = if index == 0 { "› " } else { "  " };
            lines.push(Line::from(vec![Span::styled(lead, Style::default().fg(Color::Cyan)), Span::raw(self.editor.text[span.start..span.end].to_string())]));
        }
        let cursor = Some((2 + column as u16, (1 + row - first) as u16));
        while lines.len() < usize::from(FOOTER_HEIGHT) - 1 {
            lines.push(Line::default());
        }
        let (mode, color) = if self.exiting { ("EXIT", Color::Red) } else { ("BUILD", Color::Cyan) };
        let mut status = vec![Span::styled(mode, Style::default().fg(color).add_modifier(Modifier::BOLD)), Span::styled(" · ", muted), Span::raw(self.model_label.clone())];
        let state = self.status();
        if !state.is_empty() {
            status.push(Span::styled(format!("  {state}"), if self.interrupt || self.exiting { Style::default().fg(color) } else { muted }));
        }
        lines.push(Line::from(status));
        (lines, cursor)
    }

    pub fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        let (lines, cursor) = self.footer(area.width);
        frame.render_widget(Paragraph::new(lines), area);
        if let Some((x, y)) = cursor.filter(|(x, y)| *x < area.width && *y < area.height) {
            frame.set_cursor_position((area.x + x, area.y + y));
        }
    }
}

fn dict(value: &Value) -> serde_json::Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

// `permissionInfo` / `toolPermissionInfo`: icon, title and detail lines of a request. `input` is the metadata with
// its `input` object merged over it.
fn permission_info(request: &opencode_tools::PermissionRequest, cwd: &Path, home: &Path) -> (&'static str, String, Vec<String>) {
    let meta = dict(&request.metadata);
    let mut input = meta.clone();
    input.extend(dict(meta.get("input").unwrap_or(&Value::Null)));
    let text = |key: &str| input.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
    let first = request.patterns.first().cloned().unwrap_or_default();
    // `toolPath(file, {home: true})`: relative inside the directory, `~` under the home directory, else absolute.
    let path = |file: &str| {
        let absolute = opencode_tools::util::resolve(cwd, file);
        if file.is_empty() || absolute.starts_with(cwd) || home.as_os_str().is_empty() || !absolute.starts_with(home) {
            return crate::run::tool_path(cwd, file);
        }
        crate::ui::shorten(&absolute, home).replace('\\', "/")
    };
    let or = |value: String| if value.is_empty() { first.clone() } else { value };
    match request.permission.as_str() {
        "edit" => {
            let file = or(if text("filePath").is_empty() { text("filepath") } else { text("filePath") });
            ("→", format!("Edit {}", path(&file)), Vec::new())
        }
        "read" => {
            let file = or(text("filePath"));
            ("→", format!("Read {}", path(&file)), if file.is_empty() { Vec::new() } else { vec![format!("Path: {}", path(&file))] })
        }
        "glob" | "grep" => {
            let pattern = or(text("pattern"));
            let name = if request.permission == "glob" { "Glob" } else { "Grep" };
            ("✱", format!("{name} \"{pattern}\""), if pattern.is_empty() { Vec::new() } else { vec![format!("Pattern: {pattern}")] })
        }
        "list" => {
            let dir = or(text("path"));
            ("→", format!("List {}", path(&dir)), if dir.is_empty() { Vec::new() } else { vec![format!("Path: {}", path(&dir))] })
        }
        "bash" => {
            let command = text("command");
            ("#", "Shell command".to_string(), if command.is_empty() { request.patterns.iter().map(|p| format!("- {p}")).collect() } else { vec![format!("$ {command}")] })
        }
        "task" => {
            let kind = Some(text("subagent_type")).filter(|kind| !kind.is_empty()).unwrap_or_else(|| "general".to_string());
            let description = text("description");
            ("#", format!("{} Task", text::titlecase(&kind)), if description.is_empty() { Vec::new() } else { vec![format!("◉ {description}")] })
        }
        "webfetch" => {
            let url = text("url");
            ("%", format!("WebFetch {url}"), if url.is_empty() { Vec::new() } else { vec![format!("URL: {url}")] })
        }
        "external_directory" => {
            let raw = [text("parentDir"), text("filepath"), first.clone()].into_iter().find(|value| !value.is_empty()).unwrap_or_default();
            let dir = match raw.find('*') {
                Some(at) => raw[..at].trim_end_matches(['/', '\\']).to_string(),
                None => raw,
            };
            ("←", format!("Access external directory {}", path(&dir)), request.patterns.iter().map(|p| format!("- {p}")).collect())
        }
        "doom_loop" => ("⟳", "Continue after repeated failures".to_string(), vec!["This keeps the session running despite repeated failures.".to_string()]),
        other => ("⚙", format!("Call tool {other}"), vec![format!("Tool: {other}")]),
    }
}

// Scrollback lines for rows, wrapped to the terminal width.
pub fn row_lines(rows: &[Row], width: u16, written: &mut bool) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut out = Vec::new();
    for row in rows {
        if row.gap && *written {
            out.push(Line::default());
        }
        *written = true;
        let style = match row.kind {
            Kind::System => Style::default().add_modifier(Modifier::BOLD),
            Kind::User => Style::default().fg(Color::Cyan),
            Kind::Assistant => Style::default(),
            Kind::Reasoning => Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
            Kind::Tool => Style::default().fg(Color::Gray),
            Kind::Error => Style::default().fg(Color::Red),
        };
        let mut line = String::new();
        let mut used = 0;
        for c in row.text.chars() {
            let w = text::width(c.encode_utf8(&mut [0; 4]));
            if used + w > width && !line.is_empty() {
                out.push(Line::from(Span::styled(std::mem::take(&mut line), style)));
                used = 0;
            }
            line.push(c);
            used += w;
        }
        out.push(Line::from(Span::styled(line, style)));
    }
    out
}

// Executes what the footer asks for against a session backend; turn results come back as actions.
pub struct Runtime {
    pub mini: Mini,
    backend: Arc<dyn Backend>,
    tx: UnboundedSender<Action>,
}

impl Runtime {
    pub fn new(mini: Mini, backend: Arc<dyn Backend>, tx: UnboundedSender<Action>) -> Self {
        Self { mini, backend, tx }
    }

    pub fn handle(&mut self, action: Action) {
        let commands = self.mini.action(action);
        self.execute(commands);
    }

    pub fn key(&mut self, key: KeyEvent) {
        let commands = self.mini.key(key);
        self.execute(commands);
    }

    pub fn execute(&mut self, commands: Vec<Command>) {
        for command in commands {
            match command {
                Command::Prompt(text) => self.prompt(text),
                Command::Abort => {
                    if let Some(id) = &self.mini.session_id {
                        self.backend.abort(id);
                    }
                }
                Command::Permission(id, answer) => self.backend.reply_permission(id, answer),
                Command::Question(id, answers) => self.backend.reply_question(id, answers),
                // `/new`: the next prompt starts a fresh session.
                Command::New => {
                    self.mini.session_id = None;
                    self.mini.title = None;
                    self.mini.titled = false;
                }
            }
        }
    }

    // A session is created on the first prompt; its `SessionStarted` is queued ahead of every event of the turn.
    fn prompt(&mut self, text: String) {
        let (backend, tx) = (self.backend.clone(), self.tx.clone());
        let (session, agent, model) = (self.mini.session_id.clone(), self.mini.agent.clone(), self.mini.model.clone());
        tokio::spawn(async move {
            let session_id = match session {
                Some(id) => id,
                None => match backend.create_session().await {
                    Ok(info) => {
                        let id = info.id.clone();
                        let _ = tx.send(Action::SessionStarted { info: Box::new(info), text: String::new(), then: Box::new(Effect::ListSessions) });
                        id
                    }
                    Err(error) => {
                        let _ = tx.send(Action::PromptFinished { session_id: String::new(), error: Some(error.to_string()) });
                        return;
                    }
                },
            };
            let turn = Turn::Prompt { session_id: session_id.clone(), text, files: Vec::new(), agent, model, variant: None };
            let error = backend.run(turn, CancellationToken::new()).await.err();
            let _ = tx.send(Action::PromptFinished { session_id, error });
        });
    }

    pub fn release(&self) {
        self.backend.release();
    }
}

fn controlling_terminal() -> bool {
    if std::io::stdin().is_terminal() {
        return true;
    }
    std::fs::File::open(if cfg!(windows) { "CONIN$" } else { "/dev/tty" }).is_ok()
}

async fn open(backend: &dyn Backend, session_id: &str) -> Result<opencode_tui::backend::Loaded> {
    backend.load(session_id).await.map_err(|_| fail("Session not found"))
}

pub async fn run(paths: &Paths, options: Options) -> Result<i32> {
    if options.replay_limit.is_some_and(|limit| limit.fract() != 0.0 || limit <= 0.0) {
        return Err(fail("--replay-limit must be a positive integer"));
    }
    if !std::io::stdout().is_terminal() {
        return Err(fail("--mini requires a TTY stdout"));
    }
    if !controlling_terminal() {
        return Err(fail("--mini requires a controlling terminal for input"));
    }
    if options.demo {
        return Err(crate::unimplemented("--mini --demo"));
    }
    let mut initial = options.prompt.clone().unwrap_or_default();
    if !std::io::stdin().is_terminal() {
        let mut piped = String::new();
        let _ = std::io::stdin().read_to_string(&mut piped);
        if !piped.is_empty() {
            initial = if initial.is_empty() { piped } else { format!("{initial}\n{piped}") };
        }
    }
    if options.fork && !options.resume && options.session.is_none() {
        return Err(fail("--fork requires --continue or --session"));
    }

    let (tx, rx) = unbounded_channel();
    let (backend, catalog, cwd): (Arc<dyn Backend>, Catalog, PathBuf) = match &options.attach {
        Some(url) => {
            let credentials = opencode_client::Credentials::from_env(options.password.clone(), options.username.clone());
            let client = opencode_client::Client::new(url).with_directory(options.directory.clone()).with_credentials(credentials);
            let (catalog, path) = tokio::join!(catalog::remote(&client), client.path());
            let cwd = options.directory.clone().or_else(|| path.ok().map(|path| path.directory)).map(PathBuf::from).unwrap_or_default();
            (Arc::new(RemoteBackend::connect(client, tx.clone()).await?), catalog, cwd)
        }
        None => {
            let dir = options.directory.clone().map(PathBuf::from).unwrap_or(std::env::current_dir()?);
            if std::env::set_current_dir(&dir).is_err() {
                return Err(fail(format!("Failed to change directory to {}", dir.display())));
            }
            let dir = std::env::current_dir()?;
            let catalog = catalog::load(paths, &dir);
            let backend = LocalBackend::new(Engine::new(paths.clone())?, tx.clone(), dir.clone(), catalog.worktree.clone());
            (Arc::new(backend), catalog, dir)
        }
    };

    let target = match &options.session {
        Some(id) => Some(id.clone()),
        None if options.resume => backend.sessions().await?.into_iter().next().map(|session| session.id),
        None => None,
    };
    let loaded = match target {
        Some(id) if options.fork => Some(open(&*backend, &backend.fork(&id, None).await.map_err(|_| fail("Session not found"))?.id).await?),
        Some(id) => Some(open(&*backend, &id).await?),
        // Attach mode starts its session up front; locally it is created with the first prompt.
        None if options.attach.is_some() => Some(open(&*backend, &backend.create_session().await?.id).await?),
        None => None,
    };
    let model = options.model.clone().or_else(|| catalog.config_model.clone().or_else(|| catalog.first_model()).map(|key| key.spec()));
    let label = model_label(&catalog, model.as_deref());
    let mut mini = Mini::new(options.agent.clone().unwrap_or_else(|| "build".to_string()), model, label, cwd, paths.home.clone());
    mini.rows.push(Row::new(Kind::System, "OpenCode", false));
    if let Some(loaded) = &loaded {
        mini.open(&loaded.info);
        let resume = (options.session.is_some() || options.resume) && !options.fork;
        if resume && options.replay {
            mini.replay(&loaded.messages, options.replay_limit.map(|limit| limit as usize));
        }
    }
    let mut runtime = Runtime::new(mini, backend, tx);
    if !initial.trim().is_empty() {
        runtime.mini.editor.set(initial.trim());
        let commands = runtime.mini.submit();
        runtime.execute(commands);
    }
    interact(&mut runtime, rx).await?;
    Ok(0)
}

struct Raw;

impl Drop for Raw {
    fn drop(&mut self) {
        let _ = ratatui::crossterm::terminal::disable_raw_mode();
        let _ = ratatui::crossterm::execute!(std::io::stdout(), ratatui::crossterm::event::DisableBracketedPaste, ratatui::crossterm::cursor::Show);
    }
}

fn write_rows<B: ratatui::backend::Backend>(terminal: &mut ratatui::Terminal<B>, rows: &[Row], written: &mut bool) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let width = terminal.size().map_err(|error| anyhow::anyhow!("{error:?}"))?.width;
    let lines = row_lines(rows, width, written);
    terminal
        .insert_before(lines.len() as u16, |buffer| {
            for (y, line) in lines.iter().enumerate() {
                buffer.set_line(0, y as u16, line, width);
            }
        })
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    Ok(())
}

// The split-footer terminal: rows are pushed above an inline viewport that holds the footer.
async fn interact(runtime: &mut Runtime, mut rx: UnboundedReceiver<Action>) -> Result<()> {
    ratatui::crossterm::terminal::enable_raw_mode()?;
    let _raw = Raw;
    let _ = ratatui::crossterm::execute!(std::io::stdout(), ratatui::crossterm::event::EnableBracketedPaste);
    let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
    let mut terminal = ratatui::Terminal::with_options(backend, ratatui::TerminalOptions { viewport: ratatui::Viewport::Inline(FOOTER_HEIGHT) })?;
    let (keys_tx, mut keys) = unbounded_channel();
    let stop = Arc::new(AtomicBool::new(false));
    let reader_stop = stop.clone();
    let reader = std::thread::spawn(move || {
        while !reader_stop.load(Ordering::Relaxed) {
            if !event::poll(Duration::from_millis(50)).unwrap_or(false) {
                continue;
            }
            match event::read() {
                Ok(event) => {
                    if keys_tx.send(event).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    let mut ticker = tokio::time::interval(Duration::from_millis(250));
    let mut written = false;
    let result: Result<()> = loop {
        let rows = std::mem::take(&mut runtime.mini.rows);
        if let Err(error) = write_rows(&mut terminal, &rows, &mut written) {
            break Err(error);
        }
        if runtime.mini.quit {
            break Ok(());
        }
        if let Err(error) = terminal.draw(|frame| runtime.mini.render(frame)) {
            break Err(error.into());
        }
        tokio::select! {
            action = rx.recv() => match action {
                Some(action) => runtime.handle(action),
                None => break Ok(()),
            },
            input = keys.recv() => match input {
                Some(Event::Key(key)) => runtime.key(key),
                Some(Event::Paste(text)) => runtime.mini.paste(&text),
                Some(_) => {
                    let _ = terminal.autoresize();
                }
                None => break Ok(()),
            },
            _ = ticker.tick() => {}
        }
    };
    stop.store(true, Ordering::Relaxed);
    let _ = reader.join();
    runtime.release();
    runtime.mini.farewell();
    let rows = std::mem::take(&mut runtime.mini.rows);
    let _ = terminal.draw(|frame| frame.render_widget(ratatui::widgets::Clear, frame.area()));
    let _ = write_rows(&mut terminal, &rows, &mut written);
    let _ = terminal.draw(|frame| frame.render_widget(ratatui::widgets::Clear, frame.area()));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencode_session::{QuestionInfo, QuestionOption, QuestionRequest};
    use opencode_tools::PermissionRequest;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use serde_json::json;

    fn mini() -> Mini {
        let mut mini = Mini::new("build".into(), Some("mock/m1".into()), "Mock · mock".into(), PathBuf::from("/work"), PathBuf::from("/home/u"));
        mini.session_id = Some("ses_1".into());
        mini
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_text(mini: &mut Mini, text: &str) -> Vec<Command> {
        text.chars().flat_map(|c| mini.key(key(KeyCode::Char(c)))).collect()
    }

    fn texts(mini: &Mini) -> Vec<String> {
        mini.rows.iter().map(|row| row.text.clone()).collect()
    }

    fn footer_text(mini: &Mini) -> String {
        let mut terminal = Terminal::new(TestBackend::new(60, FOOTER_HEIGHT)).unwrap();
        terminal.draw(|frame| mini.render(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height).map(|y| (0..buffer.area.width).map(|x| buffer[(x, y)].symbol()).collect::<String>().trim_end().to_string()).collect::<Vec<_>>().join("\n")
    }

    fn delta(part: &str, text: &str) -> Action {
        Action::Engine(Update::Delta { session_id: "ses_1".into(), message_id: "msg_1".into(), part_id: part.into(), field: "text", delta: text.into() })
    }

    #[test]
    fn prompts_stream_into_scrollback_line_by_line() {
        let mut mini = mini();
        assert_eq!(type_text(&mut mini, "hello"), vec![]);
        assert_eq!(mini.key(key(KeyCode::Enter)), vec![Command::Prompt("hello".into())]);
        assert!(mini.running && mini.editor.is_empty());
        mini.action(Action::Engine(Update::Reasoning(String::new())));
        mini.action(delta("prt_r", "let me see"));
        mini.action(Action::Engine(Update::Text(String::new())));
        mini.action(delta("prt_t", "Hel"));
        mini.action(delta("prt_t", "lo\nwor"));
        assert_eq!(texts(&mini), ["› hello", "Thinking: let me see", "Hello"]);
        mini.action(delta("prt_t", "ld"));
        // A subagent's text is not this session's.
        mini.action(Action::Engine(Update::Child { session_id: "ses_c".into(), update: Box::new(Update::Text("x".into())) }));
        mini.action(Action::Engine(Update::StepFinish { reason: "stop".into(), tokens: Default::default() }));
        assert_eq!(texts(&mini), ["› hello", "Thinking: let me see", "Hello", "world"]);
        assert_eq!(mini.rows.iter().map(|row| row.gap).collect::<Vec<_>>(), [true, true, true, false]);

        // Typed while running: queued, then sent when the turn ends.
        type_text(&mut mini, "next");
        assert_eq!(mini.key(key(KeyCode::Enter)), vec![]);
        assert!(footer_text(&mini).contains("BUILD · Mock · mock  esc interrupt"));
        assert!(footer_text(&mini).contains("1 queued"));
        assert_eq!(mini.action(Action::PromptFinished { session_id: "ses_1".into(), error: Some("boom".into()) }), vec![Command::Prompt("next".into())]);
        assert_eq!(texts(&mini)[4..], ["boom", "› next"]);

        // esc twice interrupts; ctrl+c clears, then twice exits.
        assert_eq!(mini.key(key(KeyCode::Esc)), vec![]);
        assert!(footer_text(&mini).contains("esc again to interrupt"));
        assert_eq!(mini.key(key(KeyCode::Esc)), vec![Command::Abort]);
        mini.key(key(KeyCode::Up));
        assert_eq!(mini.editor.text, "next");
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        mini.key(ctrl_c);
        assert!(mini.editor.is_empty() && !mini.quit);
        mini.key(ctrl_c);
        assert!(footer_text(&mini).contains("EXIT") && footer_text(&mini).contains("Press ctrl+c again to exit"));
        mini.key(ctrl_c);
        assert!(mini.quit);

        let mut other = super::tests::mini();
        type_text(&mut other, "/new");
        assert_eq!(other.key(key(KeyCode::Enter)), vec![Command::New]);
        type_text(&mut other, ":q");
        other.key(key(KeyCode::Enter));
        assert!(other.quit);
    }

    #[test]
    fn permissions_follow_the_original_state_machine() {
        let mut mini = mini();
        let request = PermissionRequest { permission: "bash".into(), patterns: vec!["ls".into()], always: vec!["ls *".into()], metadata: json!({"command": "ls -la"}) };
        mini.action(Action::PermissionAsked(PermissionAsk { id: 7, session_id: "ses_1".into(), request: request.clone() }));
        let shown = footer_text(&mini);
        for needle in ["△ Permission required", "# Shell command", "$ ls -la", "Allow once", "Allow always", "Reject"] {
            assert!(shown.contains(needle), "missing {needle:?}\n{shown}");
        }
        mini.key(key(KeyCode::Right));
        assert_eq!(mini.key(key(KeyCode::Enter)), vec![]);
        let always = footer_text(&mini);
        assert!(always.contains("Always allow") && always.contains("- ls *") && always.contains("Confirm"), "{always}");
        assert_eq!(mini.key(key(KeyCode::Enter)), vec![Command::Permission(7, PermissionAnswer::Always)]);
        assert!(mini.permissions.is_empty());

        mini.action(Action::PermissionAsked(PermissionAsk { id: 8, session_id: "ses_1".into(), request: request.clone() }));
        mini.key(key(KeyCode::Esc));
        assert!(footer_text(&mini).contains("Tell OpenCode what to do differently"));
        type_text(&mut mini, " use dir ");
        assert_eq!(mini.key(key(KeyCode::Enter)), vec![Command::Permission(8, PermissionAnswer::Corrected("use dir".into()))]);
        mini.action(Action::PermissionAsked(PermissionAsk { id: 9, session_id: "ses_1".into(), request }));
        mini.key(key(KeyCode::Esc));
        assert_eq!(mini.key(key(KeyCode::Enter)), vec![Command::Permission(9, PermissionAnswer::Reject)]);

        let question = QuestionRequest {
            id: "que_1".into(),
            session_id: "ses_1".into(),
            questions: vec![QuestionInfo { question: "Which color?".into(), header: "Pick".into(), options: vec![QuestionOption { label: "Red".into(), description: "warm".into() }, QuestionOption { label: "Blue".into(), description: "cold".into() }], multiple: None, custom: None }],
            tool: None,
        };
        mini.action(Action::QuestionAsked(QuestionAsk { id: 10, request: question.clone() }));
        let asked = footer_text(&mini);
        assert!(asked.contains("Which color?") && asked.contains("› 1. Red  warm") && asked.contains("2. Blue"), "{asked}");
        mini.key(key(KeyCode::Down));
        assert_eq!(mini.key(key(KeyCode::Enter)), vec![Command::Question(10, Some(vec![vec!["Blue".into()]]))]);
        mini.action(Action::QuestionAsked(QuestionAsk { id: 11, request: question }));
        mini.action(Action::Answered(11));
        assert!(mini.questions.is_empty());
    }

    // Scripted OpenAI-style replies in order; title requests are answered on the side.
    async fn provider(responses: Vec<Vec<Value>>) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let script = Arc::new(std::sync::Mutex::new(VecDeque::from(responses)));
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let script = script.clone();
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let body = loop {
                        let mut chunk = [0u8; 8192];
                        let n = socket.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..n]);
                        let Some(split) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                        let head = String::from_utf8_lossy(&buffer[..split]).to_lowercase();
                        let length: usize = head.lines().find_map(|l| l.strip_prefix("content-length:")).map_or(0, |v| v.trim().parse().unwrap_or(0));
                        if buffer.len() >= split + 4 + length {
                            break String::from_utf8_lossy(&buffer[split + 4..]).into_owned();
                        }
                    };
                    let stop = json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}});
                    let chunks = if body.contains("You are a title generator") {
                        vec![json!({"choices": [{"index": 0, "delta": {"content": "Mini title"}}]}), stop]
                    } else {
                        let next = script.lock().unwrap().pop_front();
                        match next {
                            Some(chunks) => chunks,
                            None => return,
                        }
                    };
                    let mut reply: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
                    reply.push_str("data: [DONE]\n\n");
                    let head = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\ncontent-length: {}\r\n\r\n", reply.len());
                    let _ = socket.write_all(format!("{head}{reply}").as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        port
    }

    async fn pump(runtime: &mut Runtime, rx: &mut UnboundedReceiver<Action>, what: &str, done: impl Fn(&Mini) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while !done(&runtime.mini) {
            let action = tokio::time::timeout_at(deadline, rx.recv()).await.unwrap_or_else(|_| panic!("timed out waiting for {what}")).expect("channel closed");
            runtime.handle(action);
        }
    }

    #[tokio::test]
    async fn turns_and_permissions_run_against_a_server() {
        let text = |content: &str| vec![json!({"choices": [{"index": 0, "delta": {"content": content}}]}), json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}})];
        let call = json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "c1", "type": "function", "function": {"name": "write", "arguments": json!({"filePath": "a.txt", "content": "mini"}).to_string()}}]}}]});
        let calls = json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}], "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}});
        let port = provider(vec![text("Hello\nthere."), vec![call, calls], text("Written.")]).await;
        let root = tempfile::tempdir().unwrap();
        let base = PathBuf::from(root.path().canonicalize().unwrap().to_string_lossy().trim_start_matches(r"\\?\"));
        let project = base.join("project");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        let config = json!({"model": "mock/m1", "permission": {"edit": "ask"}, "provider": {"mock": {"options": {"baseURL": format!("http://127.0.0.1:{port}/v1"), "apiKey": "k"}, "models": {"m1": {"name": "Mock"}}}}});
        std::fs::write(project.join("opencode.json"), config.to_string()).unwrap();
        let paths = Paths { home: base.join("home"), data: base.join("data"), cache: base.join("cache"), config: base.join("config"), state: base.join("state"), tmp: base.join("tmp") };
        let source = opencode_llm::catalog::Source { disabled: true, ..opencode_llm::catalog::Source::from_env(&paths) };
        let engine = Engine::with_store(paths, opencode_storage::Store::open_in_memory().unwrap());
        engine.set_catalog_source(source);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let options = opencode_server::ServerOptions { directory: Some(project.to_string_lossy().into_owned()), ..Default::default() };
        let server = opencode_server::serve_on(listener, options, Arc::new(engine)).await.unwrap();

        let client = opencode_client::Client::new(&server.url()).with_directory(Some(project.to_string_lossy().into_owned()));
        let catalog = catalog::remote(&client).await;
        let (tx, mut rx) = unbounded_channel();
        let backend = Arc::new(RemoteBackend::connect(client, tx.clone()).await.unwrap());
        let mini = Mini::new("build".into(), None, model_label(&catalog, catalog.config_model.as_ref().map(ModelKey::spec).as_deref()), project.clone(), base.join("home"));
        let mut runtime = Runtime::new(mini, backend, tx);
        assert!(footer_text(&runtime.mini).contains("BUILD · Mock · mock"), "{}", footer_text(&runtime.mini));

        type_text(&mut runtime.mini, "hi");
        runtime.key(key(KeyCode::Enter));
        pump(&mut runtime, &mut rx, "the first turn", |mini| !mini.running).await;
        assert!(runtime.mini.session_id.is_some());
        assert_eq!(texts(&runtime.mini), ["› hi", "Hello", "there."]);

        runtime.mini.rows.clear();
        type_text(&mut runtime.mini, "write it");
        runtime.key(key(KeyCode::Enter));
        pump(&mut runtime, &mut rx, "the permission", |mini| !mini.permissions.is_empty()).await;
        assert!(footer_text(&runtime.mini).contains("→ Edit a.txt"), "{}", footer_text(&runtime.mini));
        runtime.key(key(KeyCode::Enter));
        pump(&mut runtime, &mut rx, "the second turn", |mini| !mini.running).await;
        assert_eq!(std::fs::read_to_string(project.join("a.txt")).unwrap(), "mini");
        let rows = texts(&runtime.mini);
        assert_eq!(rows.first().map(String::as_str), Some("› write it"));
        assert!(rows.iter().any(|row| row == "← Write a.txt") && rows.last().is_some_and(|row| row == "Written."), "{rows:?}");
        runtime.mini.rows.clear();
        runtime.mini.farewell();
        let banner = texts(&runtime.mini);
        assert!(banner[1].starts_with("Continue opencode --mini -s ses"), "{banner:?}");
        server.shutdown().await;
    }

    #[test]
    fn rows_wrap_and_the_exit_banner_names_the_session() {
        let rows = vec![Row::new(Kind::User, "› abcdefgh", true), Row::new(Kind::Assistant, "xy", true)];
        let mut written = false;
        let lines = row_lines(&rows, 6, &mut written);
        let plain: Vec<String> = lines.iter().map(|line| line.spans.iter().map(|s| s.content.to_string()).collect()).collect();
        assert_eq!(plain, ["› abcd", "efgh", "", "xy"]);
        let mut mini = mini();
        mini.title = Some("New session - 2026".into());
        type_text(&mut mini, "fix the bug");
        mini.key(key(KeyCode::Enter));
        mini.rows.clear();
        mini.farewell();
        assert_eq!(texts(&mini), ["Session  fix the bug", "Continue opencode --mini -s ses_1"]);
    }
}
