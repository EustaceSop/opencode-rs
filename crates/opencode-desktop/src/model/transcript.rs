// Stored messages/parts and live engine updates -> flat rows the transcript renders.
use opencode_storage::{AssistantError, JsonMap, Message, Part, PartBody, ToolState};
use serde_json::Value;
use std::sync::Arc;

const OUTPUT_LIMIT: usize = 16_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Pending,
    Running,
    Completed,
    Error,
}

impl ToolStatus {
    #[cfg(test)]
    pub fn label(self) -> &'static str {
        match self {
            ToolStatus::Pending => "pending",
            ToolStatus::Running => "running",
            ToolStatus::Completed => "completed",
            ToolStatus::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolView {
    pub tool: Arc<str>,
    pub title: Arc<str>,
    pub status: ToolStatus,
    pub input: Arc<str>,
    pub output: Arc<str>,
    pub diff: Option<Arc<str>>,
    pub duration_ms: Option<u64>,
    // The subagent session a `task` call runs in.
    pub child: Option<Arc<str>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RowKind {
    // `files` are the names of the attachments sent along.
    User { text: Arc<str>, files: Vec<Arc<str>> },
    Text { markdown: Arc<str>, streaming: bool },
    Reasoning { text: Arc<str>, streaming: bool },
    Tool(ToolView),
    Error { message: Arc<str> },
    // `until_ms`: when the next attempt starts (epoch ms), so the view can count down (`SessionRetry`).
    Retry { attempt: u32, message: Arc<str>, until_ms: u64 },
    // The history above was summarised.
    Compaction,
    // Rust build: the last row while a run is in flight; `label` is the i18n key of what it is doing.
    Working { label: &'static str },
}

// Rust build: what a running session is doing, as the original's `ui.sessionTurn.status.*` labels name it.
pub fn working_label(rows: &[Row]) -> &'static str {
    let tool = rows.iter().rev().take_while(|row| !matches!(row.kind, RowKind::User { .. })).find_map(|row| match &row.kind {
        RowKind::Tool(view) => Some(view.tool.as_ref()),
        _ => None,
    });
    match tool {
        Some("bash" | "shell") => "ui.sessionTurn.status.runningCommands",
        Some("edit" | "write" | "patch" | "apply_patch" | "multiedit") => "ui.sessionTurn.status.makingEdits",
        Some("grep" | "glob" | "list" | "codesearch") => "ui.sessionTurn.status.searchingCodebase",
        Some("read") => "ui.sessionTurn.status.gatheringContext",
        Some("webfetch" | "websearch") => "ui.sessionTurn.status.searchingWeb",
        Some("task" | "subagent") => "ui.sessionTurn.status.delegating",
        Some("todowrite") => "ui.sessionTurn.status.planning",
        _ => "ui.sessionTurn.status.thinking",
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: Arc<str>,
    pub kind: RowKind,
}

impl Row {
    pub fn new(id: impl AsRef<str>, kind: RowKind) -> Self {
        Self { id: id.as_ref().into(), kind }
    }

    pub fn error(id: impl AsRef<str>, message: impl AsRef<str>) -> Self {
        Self::new(id, RowKind::Error { message: message.as_ref().into() })
    }
}

fn first_string<'a>(input: &'a JsonMap, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| input.get(*key).and_then(Value::as_str)).filter(|s| !s.is_empty())
}

fn one_line(text: &str, max: usize) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    match line.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_string(),
    }
}

// What a tool call is about, derived from its arguments while no title exists yet.
pub fn tool_summary(tool: &str, input: &JsonMap) -> String {
    let keys: &[&str] = match tool {
        "bash" | "shell" => &["description", "command"],
        "read" | "edit" | "write" => &["filePath", "file_path", "path"],
        "glob" | "grep" => &["pattern"],
        "list" => &["path"],
        "webfetch" => &["url"],
        "websearch" => &["query"],
        "task" => &["description"],
        "skill" => &["name"],
        _ => &["description", "filePath", "path", "pattern", "command", "url", "query", "name"],
    };
    first_string(input, keys).map(|s| one_line(s, 120)).unwrap_or_default()
}

fn clip(text: &str) -> Arc<str> {
    match text.char_indices().nth(OUTPUT_LIMIT) {
        Some((cut, _)) => format!("{}\n… ({} more bytes)", &text[..cut], text.len() - cut).into(),
        None => text.into(),
    }
}

fn pretty_input(tool: &str, input: &JsonMap) -> Arc<str> {
    if input.is_empty() {
        return "".into();
    }
    if matches!(tool, "bash" | "shell")
        && let Some(command) = input.get("command").and_then(Value::as_str)
    {
        return clip(command);
    }
    clip(&serde_json::to_string_pretty(input).unwrap_or_default())
}

fn diff_of(metadata: Option<&JsonMap>) -> Option<Arc<str>> {
    metadata?.get("diff")?.as_str().filter(|d| !d.trim().is_empty()).map(Arc::from)
}

fn child_of(metadata: Option<&JsonMap>) -> Option<Arc<str>> {
    metadata?.get("sessionId")?.as_str().filter(|id| !id.is_empty()).map(Arc::from)
}

pub fn tool_view(tool: &str, state: &ToolState) -> ToolView {
    let child = match state {
        ToolState::Pending(_) => None,
        ToolState::Running(s) => child_of(s.metadata.as_ref()),
        ToolState::Completed(s) => child_of(Some(&s.metadata)),
        ToolState::Error(s) => child_of(s.metadata.as_ref()),
    };
    let input = state.input();
    let summary = tool_summary(tool, input);
    let (status, title, output, diff, duration_ms) = match state {
        ToolState::Pending(_) => (ToolStatus::Pending, summary, String::new(), None, None),
        ToolState::Running(s) => (
            ToolStatus::Running,
            s.title.clone().filter(|t| !t.is_empty()).unwrap_or(summary),
            String::new(),
            diff_of(s.metadata.as_ref()),
            None,
        ),
        ToolState::Completed(s) => (
            ToolStatus::Completed,
            if s.title.is_empty() { summary } else { one_line(&s.title, 160) },
            s.output.clone(),
            diff_of(Some(&s.metadata)),
            Some(s.time.end.saturating_sub(s.time.start)),
        ),
        ToolState::Error(s) => (
            ToolStatus::Error,
            summary,
            s.error.clone(),
            diff_of(s.metadata.as_ref()),
            Some(s.time.end.saturating_sub(s.time.start)),
        ),
    };
    ToolView {
        tool: tool.into(),
        title: title.into(),
        status,
        input: pretty_input(tool, input),
        output: clip(&output),
        diff,
        duration_ms,
        child: child.filter(|_| tool == "task"),
    }
}

pub fn error_message(error: &AssistantError) -> String {
    match error {
        AssistantError::ProviderAuthError { provider_id, message } => format!("{provider_id}: {message}"),
        AssistantError::UnknownError { message, .. }
        | AssistantError::MessageAbortedError { message }
        | AssistantError::StructuredOutputError { message, .. }
        | AssistantError::ContextOverflowError { message, .. }
        | AssistantError::ContentFilterError { message } => message.clone(),
        AssistantError::MessageOutputLengthError {} => "The response exceeded the model's output limit".to_string(),
        AssistantError::ApiError(data) => match data.status_code {
            Some(code) => format!("{} (HTTP {code})", data.message),
            None => data.message.clone(),
        },
    }
}

fn part_row(part: &Part) -> Option<Row> {
    let kind = match &part.body {
        PartBody::Text(t) if !t.text.trim().is_empty() && t.ignored != Some(true) => {
            RowKind::Text { markdown: t.text.as_str().into(), streaming: false }
        }
        PartBody::Reasoning(r) if !r.text.trim().is_empty() => {
            RowKind::Reasoning { text: r.text.as_str().into(), streaming: false }
        }
        PartBody::Tool(t) => RowKind::Tool(tool_view(&t.tool, &t.state)),
        _ => return None,
    };
    Some(Row::new(&part.id, kind))
}

fn file_name(file: &opencode_storage::FilePart) -> Arc<str> {
    match &file.filename {
        Some(name) if !name.is_empty() => name.as_str().into(),
        _ => file.mime.as_str().into(),
    }
}

// The messages a pending revert leaves visible: everything before the reverted message.
pub fn before_revert<'a>(messages: &'a [(Message, Vec<Part>)], revert: Option<&str>) -> &'a [(Message, Vec<Part>)] {
    let cut = revert.and_then(|id| messages.iter().position(|(m, _)| m.id() == id)).unwrap_or(messages.len());
    &messages[..cut]
}

pub fn rows_from_store(messages: &[(Message, Vec<Part>)]) -> Vec<Row> {
    let mut rows = Vec::new();
    for (message, parts) in messages {
        match message {
            Message::User(user) => {
                let text: Vec<&str> = parts
                    .iter()
                    .filter_map(|p| match &p.body {
                        PartBody::Text(t) if t.synthetic != Some(true) && t.ignored != Some(true) && !t.text.is_empty() => {
                            Some(t.text.as_str())
                        }
                        _ => None,
                    })
                    .collect();
                let files: Vec<Arc<str>> = parts
                    .iter()
                    .filter_map(|p| match &p.body {
                        PartBody::File(file) => Some(file_name(file)),
                        _ => None,
                    })
                    .collect();
                if parts.iter().any(|p| matches!(p.body, PartBody::Compaction(_))) {
                    rows.push(Row::new(format!("{}-compaction", user.id), RowKind::Compaction));
                } else if !text.is_empty() || !files.is_empty() {
                    rows.push(Row::new(&user.id, RowKind::User { text: text.join("\n").into(), files }));
                }
            }
            Message::Assistant(assistant) => {
                rows.extend(parts.iter().filter_map(part_row));
                if let Some(error) = &assistant.error {
                    rows.push(Row::error(format!("{}-error", assistant.id), error_message(error)));
                }
            }
        }
    }
    rows
}

// The first user text of a session, used as a title while the stored one is still the default.
pub fn first_user_text(messages: &[(Message, Vec<Part>)]) -> Option<String> {
    messages.iter().find_map(|(message, parts)| {
        message.as_user()?;
        parts.iter().find_map(|p| match &p.body {
            PartBody::Text(t) if t.synthetic != Some(true) && !t.text.trim().is_empty() => Some(one_line(&t.text, 80)),
            _ => None,
        })
    })
}

#[derive(Debug, Clone, PartialEq)]
enum LiveItem {
    User(String, Vec<Arc<str>>),
    Text(String),
    Reasoning(String),
    Tool(Box<Part>),
}

// Everything a running prompt produced since the last store snapshot.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Live {
    // Bumped on every snapshot so row ids of different steps never collide.
    generation: u64,
    items: Vec<LiveItem>,
    retry: Option<(u32, String, u64)>,
}

impl Live {
    pub fn user(&mut self, text: &str, files: Vec<Arc<str>>) {
        self.items.push(LiveItem::User(text.to_string(), files));
    }

    pub fn text(&mut self, delta: &str) {
        self.retry = None;
        match self.items.last_mut() {
            Some(LiveItem::Text(text)) => text.push_str(delta),
            _ => self.items.push(LiveItem::Text(delta.to_string())),
        }
    }

    pub fn reasoning(&mut self, delta: &str) {
        self.retry = None;
        match self.items.last_mut() {
            Some(LiveItem::Reasoning(text)) => text.push_str(delta),
            _ => self.items.push(LiveItem::Reasoning(delta.to_string())),
        }
    }

    pub fn tool(&mut self, part: Box<Part>) {
        self.retry = None;
        let existing = self.items.iter_mut().find(|item| matches!(item, LiveItem::Tool(p) if p.id == part.id));
        match existing {
            Some(item) => *item = LiveItem::Tool(part),
            None => self.items.push(LiveItem::Tool(part)),
        }
    }

    pub fn retry(&mut self, attempt: u32, message: &str, delay_ms: u64) {
        self.retry = Some((attempt, message.to_string(), opencode_storage::id::now_ms() + delay_ms));
    }

    // A store snapshot now covers everything received so far.
    pub fn settle(&mut self) {
        self.generation += 1;
        self.items.clear();
        self.retry = None;
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.retry.is_none()
    }

    pub fn rows(&self) -> Vec<Row> {
        let last = self.items.len().saturating_sub(1);
        let mut rows: Vec<Row> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                let id = format!("live-{}-{index}", self.generation);
                let streaming = index == last;
                let kind = match item {
                    LiveItem::User(text, files) => RowKind::User { text: text.as_str().into(), files: files.clone() },
                    LiveItem::Text(text) if text.trim().is_empty() => return None,
                    LiveItem::Text(text) => RowKind::Text { markdown: text.as_str().into(), streaming },
                    LiveItem::Reasoning(text) if text.trim().is_empty() => return None,
                    LiveItem::Reasoning(text) => RowKind::Reasoning { text: text.as_str().into(), streaming },
                    LiveItem::Tool(part) => return part_row(part),
                };
                Some(Row::new(id, kind))
            })
            .collect();
        if let Some((attempt, message, until_ms)) = &self.retry {
            rows.push(Row::new(
                format!("live-{}-retry", self.generation),
                RowKind::Retry { attempt: *attempt, message: message.as_str().into(), until_ms: *until_ms },
            ));
        }
        rows
    }

    // Streamed text the store never received (the engine only persists finished steps).
    pub fn unsaved(&self, stored: &[Row]) -> Vec<Row> {
        let known = |text: &str| {
            stored.iter().any(|row| match &row.kind {
                RowKind::Text { markdown, .. } => markdown.as_ref() == text,
                RowKind::Reasoning { text: saved, .. } => saved.as_ref() == text,
                _ => false,
            })
        };
        self.items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                let id = format!("partial-{}-{index}", self.generation);
                match item {
                    LiveItem::Text(text) if !text.trim().is_empty() && !known(text) => {
                        Some(Row::new(id, RowKind::Text { markdown: text.as_str().into(), streaming: false }))
                    }
                    LiveItem::Reasoning(text) if !text.trim().is_empty() && !known(text) => {
                        Some(Row::new(id, RowKind::Reasoning { text: text.as_str().into(), streaming: false }))
                    }
                    _ => None,
                }
            })
            .collect()
    }
}

// How to turn the list showing `old` into one showing `new`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListPatch {
    // Rows [keep..old_len) are replaced by `insert` new rows.
    pub keep: usize,
    pub remove: usize,
    pub insert: usize,
    // Kept rows whose content (and so height) changed.
    pub remeasure: Vec<usize>,
}

impl ListPatch {
    pub fn is_noop(&self) -> bool {
        self.remove == 0 && self.insert == 0 && self.remeasure.is_empty()
    }
}

pub fn diff_rows(old: &[Row], new: &[Row]) -> ListPatch {
    let keep = old.iter().zip(new).take_while(|(a, b)| a.id == b.id).count();
    let remeasure = (0..keep).filter(|&i| old[i].kind != new[i].kind).collect();
    ListPatch { keep, remove: old.len() - keep, insert: new.len() - keep, remeasure }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencode_storage::{
        AssistantMessage, AssistantTime, MessagePath, ReasoningPart, TextPart, Tokens, ToolPart, UserMessage, UserModel,
        UserTime,
    };
    use serde_json::json;

    fn object(value: Value) -> JsonMap {
        value.as_object().cloned().unwrap_or_default()
    }

    fn user(id: &str) -> Message {
        Message::User(UserMessage {
            id: id.into(),
            session_id: "ses_1".into(),
            time: UserTime { created: 1 },
            format: None,
            summary: None,
            agent: "build".into(),
            model: UserModel { provider_id: "p".into(), model_id: "m".into(), variant: None },
            system: None,
            tools: None,
        })
    }

    fn assistant(id: &str, error: Option<AssistantError>) -> Message {
        Message::Assistant(AssistantMessage {
            id: id.into(),
            session_id: "ses_1".into(),
            time: AssistantTime { created: 2, completed: None },
            error,
            parent_id: "msg_u".into(),
            model_id: "m".into(),
            provider_id: "p".into(),
            mode: "build".into(),
            agent: "build".into(),
            path: MessagePath::default(),
            summary: None,
            cost: 0.0,
            tokens: Tokens::default(),
            structured: None,
            variant: None,
            finish: None,
        })
    }

    fn part(id: &str, message: &str, body: PartBody) -> Part {
        Part { id: id.into(), session_id: "ses_1".into(), message_id: message.into(), body }
    }

    fn text(text: &str) -> PartBody {
        PartBody::Text(TextPart { text: text.into(), ..Default::default() })
    }

    fn tool(state: ToolState) -> PartBody {
        PartBody::Tool(ToolPart { call_id: "call_1".into(), tool: "edit".into(), state, metadata: None })
    }

    #[test]
    fn stored_messages_become_rows() {
        let running = ToolState::pending(object(json!({"filePath": "src/a.rs"})), "{}").to_running(object(json!({"filePath": "src/a.rs"})), 10);
        let done = running.to_completed("ok", "src/a.rs", object(json!({"diff": "--- a\n+++ b\n"})), None, 25);
        let synthetic = PartBody::Text(TextPart { text: "hidden".into(), synthetic: Some(true), ..Default::default() });
        let messages = vec![
            (user("msg_u"), vec![part("prt_1", "msg_u", text("hello")), part("prt_s", "msg_u", synthetic)]),
            (
                assistant("msg_a", Some(AssistantError::aborted())),
                vec![
                    part("prt_2", "msg_a", PartBody::StepStart(Default::default())),
                    part(
                        "prt_3",
                        "msg_a",
                        PartBody::Reasoning(ReasoningPart {
                            text: "thinking".into(),
                            metadata: None,
                            time: opencode_storage::PartTime { start: 1, end: None },
                        }),
                    ),
                    part("prt_4", "msg_a", text("# Title")),
                    part("prt_5", "msg_a", tool(done)),
                    part("prt_6", "msg_a", text("  ")),
                ],
            ),
        ];
        let rows = rows_from_store(&messages);
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_ref()).collect();
        assert_eq!(ids, ["msg_u", "prt_3", "prt_4", "prt_5", "msg_a-error"]);
        assert_eq!(rows[0].kind, RowKind::User { text: "hello".into(), files: vec![] });
        assert_eq!(rows[2].kind, RowKind::Text { markdown: "# Title".into(), streaming: false });
        let RowKind::Tool(view) = &rows[3].kind else { panic!("tool row") };
        assert_eq!(view.status, ToolStatus::Completed);
        assert_eq!(view.title.as_ref(), "src/a.rs");
        assert_eq!(view.duration_ms, Some(15));
        assert_eq!(view.diff.as_deref(), Some("--- a\n+++ b\n"));
        assert_eq!(rows[4].kind, RowKind::Error { message: "Aborted".into() });
        assert_eq!(first_user_text(&messages).as_deref(), Some("hello"));
    }

    #[test]
    fn tool_views_follow_state() {
        let input = object(json!({"command": "cargo test\n--all", "description": "Run tests"}));
        let pending = ToolState::pending(input.clone(), "");
        let view = tool_view("bash", &pending);
        assert_eq!((view.status, view.title.as_ref()), (ToolStatus::Pending, "Run tests"));
        assert_eq!(view.input.as_ref(), "cargo test\n--all");
        let failed = pending.to_running(input, 5).to_error("exit 1", None, 9);
        let view = tool_view("bash", &failed);
        assert_eq!((view.status, view.output.as_ref(), view.duration_ms), (ToolStatus::Error, "exit 1", Some(4)));
        assert_eq!(tool_summary("grep", &object(json!({"pattern": "fn main"}))), "fn main");
        assert_eq!(tool_summary("mystery", &JsonMap::new()), "");
    }

    #[test]
    fn long_output_is_clipped() {
        let state = ToolState::pending(JsonMap::new(), "").to_completed("x".repeat(OUTPUT_LIMIT + 10), "t", JsonMap::new(), None, 1);
        let view = tool_view("read", &state);
        assert!(view.output.ends_with("… (10 more bytes)"));
    }

    #[test]
    fn live_deltas_accumulate_and_settle() {
        let mut live = Live::default();
        live.user("hi", vec![]);
        live.reasoning("thin");
        live.reasoning("king");
        live.text("Hel");
        live.text("lo");
        let rows = live.rows();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].kind, RowKind::Reasoning { text: "thinking".into(), streaming: false });
        assert_eq!(rows[2].kind, RowKind::Text { markdown: "Hello".into(), streaming: true });

        let pending = ToolState::pending(JsonMap::new(), "");
        live.tool(Box::new(part("prt_t", "msg_a", tool(pending.clone()))));
        live.tool(Box::new(part("prt_t", "msg_a", tool(pending.to_running(JsonMap::new(), 1)))));
        let rows = live.rows();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[3].id.as_ref(), "prt_t");
        assert!(matches!(&rows[3].kind, RowKind::Tool(view) if view.status == ToolStatus::Running));
        assert_eq!(rows[2].kind, RowKind::Text { markdown: "Hello".into(), streaming: false });

        live.retry(2, "overloaded", 500);
        assert!(matches!(live.rows().last().unwrap().kind, RowKind::Retry { attempt: 2, .. }));
        live.text("!");
        assert!(!live.rows().iter().any(|r| matches!(r.kind, RowKind::Retry { .. })));

        let before = live.rows()[0].id.clone();
        live.settle();
        assert!(live.is_empty());
        live.text("next");
        assert_ne!(live.rows()[0].id, before);
    }

    #[test]
    fn unsaved_keeps_only_text_missing_from_the_store() {
        let mut live = Live::default();
        live.user("hi", vec![]);
        live.text("saved");
        live.tool(Box::new(part("prt_t", "msg_a", tool(ToolState::pending(JsonMap::new(), "")))));
        live.text("cut off mid-sen");
        let stored = vec![Row::new("prt_1", RowKind::Text { markdown: "saved".into(), streaming: false })];
        let rows = live.unsaved(&stored);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, RowKind::Text { markdown: "cut off mid-sen".into(), streaming: false });
    }

    #[test]
    fn diff_rows_describes_the_minimal_tail_change() {
        let row = |id: &str, text: &str| Row::new(id, RowKind::Text { markdown: text.into(), streaming: false });
        let old = vec![row("a", "1"), row("b", "2"), row("live-0-0", "x")];
        assert!(diff_rows(&old, &old).is_noop());

        let grown = vec![row("a", "1"), row("b", "2"), row("live-0-0", "xy")];
        assert_eq!(diff_rows(&old, &grown), ListPatch { keep: 3, remove: 0, insert: 0, remeasure: vec![2] });

        let settled = vec![row("a", "1"), row("b", "2"), row("c", "xy"), row("d", "z")];
        assert_eq!(diff_rows(&old, &settled), ListPatch { keep: 2, remove: 1, insert: 2, remeasure: vec![] });

        assert_eq!(diff_rows(&old, &[]), ListPatch { keep: 0, remove: 3, insert: 0, remeasure: vec![] });
    }
}
