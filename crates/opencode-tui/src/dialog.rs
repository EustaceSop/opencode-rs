use crate::app::App;
use crate::catalog::Catalog;
use crate::editor::Editor;
use crate::fuzzy;
use crate::keybind::{self, Keymap};
use crate::state::{ModelKey, ModelState};
use opencode_storage::{Message, Part, PartBody, Session};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectKind {
    Sessions,
    Models,
    Agents,
    Themes,
    Commands,
    Help,
    Timeline,
    Fork,
    // Actions on the message whose id is the dialog's context.
    MessageActions,
    Stash,
    Variants,
    // Rust build: the one-time offer of a portable bash on Windows (`opencode_tools::mingit::OFFER`).
    SetupBash,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub title: String,
    pub description: String,
    pub category: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct SelectDialog {
    pub kind: SelectKind,
    pub title: String,
    pub items: Vec<Item>,
    pub query: String,
    // Indexes into `items`, in display order.
    pub filtered: Vec<usize>,
    // Position within `filtered`.
    pub selected: usize,
    // Value of the entry that is active outside the dialog.
    pub current: Option<String>,
    // Sessions: the entry armed for deletion by a first press.
    pub confirm_delete: Option<String>,
    pub context: Option<String>,
}

impl SelectDialog {
    pub fn new(kind: SelectKind, title: &str, items: Vec<Item>, current: Option<String>) -> Self {
        let mut dialog = Self { kind, title: title.to_string(), items, query: String::new(), filtered: Vec::new(), selected: 0, current, confirm_delete: None, context: None };
        dialog.refilter();
        if let Some(position) = dialog.current.as_ref().and_then(|c| dialog.filtered.iter().position(|&i| &dialog.items[i].value == c)) {
            dialog.selected = position;
        }
        dialog
    }

    pub fn refilter(&mut self) {
        self.filtered = fuzzy::filter(&self.query, &self.items, |item| format!("{} {}", item.title, item.category));
        self.selected = 0;
        self.confirm_delete = None;
    }

    pub fn set_query(&mut self, query: String) {
        self.query = query;
        self.refilter();
    }

    pub fn selected_item(&self) -> Option<&Item> {
        self.filtered.get(self.selected).map(|&i| &self.items[i])
    }

    pub fn move_by(&mut self, delta: i64) {
        let len = self.filtered.len() as i64;
        if len == 0 {
            return;
        }
        // Single steps wrap around; page jumps clamp.
        let next = self.selected as i64 + delta;
        self.selected = if delta.abs() == 1 { next.rem_euclid(len) } else { next.clamp(0, len - 1) } as usize;
        self.confirm_delete = None;
    }

    pub fn move_to(&mut self, last: bool) {
        self.selected = if last { self.filtered.len().saturating_sub(1) } else { 0 };
        self.confirm_delete = None;
    }

    pub fn remove_value(&mut self, value: &str) {
        self.items.retain(|item| item.value != value);
        let keep = self.selected;
        self.refilter();
        self.selected = keep.min(self.filtered.len().saturating_sub(1));
    }

    // Categories only structure the unfiltered list.
    pub fn show_categories(&self) -> bool {
        self.query.trim().is_empty()
    }
}

pub const EXPORT_FIELDS: usize = 5;

// `DialogExportOptions`: field 0 is the filename, 1..=4 the checkboxes in `flags` order.
#[derive(Debug, Clone)]
pub struct ExportDialog {
    pub filename: Editor,
    // thinking, tool details, assistant metadata, open without saving
    pub flags: [bool; 4],
    pub active: usize,
}

pub const EXPORT_LABELS: [&str; 4] = ["Include thinking", "Include tool details", "Include assistant metadata", "Open without saving"];

// Rust build: read-only text with a title, e.g. the `/btw` answer and `/stats`.
#[derive(Debug, Clone, PartialEq)]
pub struct TextDialog {
    pub title: String,
    pub subtitle: String,
    pub body: String,
    pub markdown: bool,
    pub scroll: usize,
}

#[derive(Debug, Clone)]
pub enum Dialog {
    Select(SelectDialog),
    Rename { session_id: String, editor: Editor },
    Export(ExportDialog),
    Text(TextDialog),
}

// (config key, title, category, slash names) for every command this port runs.
pub const COMMANDS: &[(&str, &str, &str, &[&str])] = &[
    ("session_list", "Switch session", "Session", &["sessions", "resume", "continue"]),
    ("session_new", "New session", "Session", &["new", "clear"]),
    ("session_rename", "Rename session", "Session", &["rename"]),
    ("session_timeline", "Jump to message", "Session", &["timeline"]),
    ("session_fork", "Fork session", "Session", &["fork"]),
    ("session_compact", "Compact session", "Session", &["compact"]),
    // Rust build: opencode 2.x `/btw` (takes the question as its argument).
    ("session_aside", "Ask a side question", "Session", &["btw"]),
    ("messages_undo", "Undo previous message", "Session", &["undo"]),
    ("messages_redo", "Redo", "Session", &["redo"]),
    ("sidebar_toggle", "Show sidebar", "Session", &[]),
    ("messages_toggle_conceal", "Disable code concealment", "Session", &[]),
    ("session_toggle_timestamps", "Show timestamps", "Session", &["timestamps", "toggle-timestamps"]),
    // Rust build: opencode 2.x gives `/thinking` to the variant picker; `/toggle-thinking` stays here.
    ("display_thinking", "Expand thinking", "Session", &["toggle-thinking"]),
    ("tool_details", "Hide tool details", "Session", &[]),
    ("scrollbar_toggle", "Toggle session scrollbar", "Session", &[]),
    ("session_toggle_generic_tool_output", "Show generic tool output", "Session", &[]),
    ("messages_copy", "Copy last assistant message", "Session", &[]),
    ("session_copy", "Copy session transcript", "Session", &["copy"]),
    ("session_export", "Export session transcript", "Session", &["export"]),
    ("editor_open", "Open editor", "Session", &["editor"]),
    ("prompt_stash", "Stash prompt", "Prompt", &[]),
    ("prompt_stash_pop", "Stash pop", "Prompt", &[]),
    ("prompt_stash_list", "Stash list", "Prompt", &[]),
    ("model_list", "Switch model", "Agent", &["models", "mo"]),
    ("agent_list", "Switch agent", "Agent", &["agents"]),
    ("variant_cycle", "Variant cycle", "Agent", &[]),
    ("variant_list", "Switch model variant", "Agent", &["variants", "thinking", "effort"]),
    ("theme_list", "Switch theme", "System", &["themes"]),
    ("theme_switch_mode", "Switch to light mode", "System", &[]),
    ("theme_mode_lock", "Lock theme mode", "System", &[]),
    ("terminal_title_toggle", "Disable terminal title", "System", &[]),
    ("app_toggle_animations", "Disable animations", "System", &[]),
    ("app_toggle_paste_summary", "Disable paste summary", "System", &[]),
    // Rust build: the desktop app's sounds (`sound.rs`).
    ("sounds_toggle", "Disable sounds", "System", &["sounds"]),
    ("permission_mode", "Enable auto-approve permissions", "System", &[]),
    ("tips_toggle", "Toggle tips", "System", &[]),
    ("help_show", "Help", "System", &["help"]),
    // Rust build: opencode 2.x `/reload` and `/stats`.
    ("config_reload", "Reload configuration", "System", &["reload"]),
    ("stats_show", "Usage statistics", "System", &["stats"]),
    ("docs_open", "Open docs", "System", &[]),
    ("app_exit", "Exit the app", "System", &["exit", "quit", "q"]),
];

// Rust build: names other coding agents use for the same commands (Claude Code, Codex, Gemini CLI), so muscle memory
// finds them: typing one lists the command, with the alias shown beside it.
pub const ALIASES: &[(&str, &[&str])] = &[
    ("messages_undo", &["rewind"]),
    ("session_list", &["history", "chat"]),
    ("session_new", &["reset"]),
    ("session_compact", &["compress", "summarize"]),
    ("model_list", &["model"]),
    ("agent_list", &["agent", "mode"]),
    ("variant_list", &["reasoning"]),
    ("theme_list", &["theme"]),
    ("stats_show", &["cost", "usage"]),
    ("permission_mode", &["permissions", "approvals", "yolo"]),
    ("app_exit", &["bye"]),
];

pub fn aliases(key: &str) -> &'static [&'static str] {
    ALIASES.iter().find(|(k, _)| *k == key).map_or(&[], |(_, names)| names)
}

// Titles the original computes from the current state.
pub fn command_title(app: &App, key: &str) -> Option<&'static str> {
    let pick = |on: bool, yes: &'static str, no: &'static str| Some(if on { yes } else { no });
    match key {
        "sidebar_toggle" => pick(app.sidebar_visible(), "Hide sidebar", "Show sidebar"),
        "messages_toggle_conceal" => pick(app.conceal, "Disable code concealment", "Enable code concealment"),
        "session_toggle_timestamps" => pick(app.show_timestamps(), "Hide timestamps", "Show timestamps"),
        "display_thinking" => pick(app.show_thinking, "Collapse thinking", "Expand thinking"),
        "tool_details" => pick(app.show_details, "Hide tool details", "Show tool details"),
        "session_toggle_generic_tool_output" => pick(app.kv.get_bool("generic_tool_output_visibility", false), "Hide generic tool output", "Show generic tool output"),
        "theme_switch_mode" => pick(app.mode == crate::theme::Mode::Dark, "Switch to light mode", "Switch to dark mode"),
        "theme_mode_lock" => pick(app.kv.get_str("theme_mode_lock").is_some(), "Unlock theme mode", "Lock theme mode"),
        "terminal_title_toggle" => pick(app.terminal_title_enabled(), "Disable terminal title", "Enable terminal title"),
        "app_toggle_animations" => pick(app.animations(), "Disable animations", "Enable animations"),
        "app_toggle_paste_summary" => pick(app.paste_summary(), "Disable paste summary", "Enable paste summary"),
        "sounds_toggle" => pick(app.sounds_on(), "Disable sounds", "Enable sounds"),
        "permission_mode" => pick(app.auto_permissions, "Disable auto-approve permissions", "Enable auto-approve permissions"),
        _ => None,
    }
}

pub fn slash_command(name: &str) -> Option<&'static str> {
    COMMANDS.iter().find(|(key, _, _, slashes)| slashes.contains(&name) || aliases(key).contains(&name)).map(|(key, ..)| *key)
}

// `Date.toDateString()`, in local time.
fn day(ms: u64) -> String {
    crate::text::local_time(ms, "%a %b %d %Y")
}

// Accept downloads now, Not now asks again next start, Never records the refusal.
pub fn setup_bash() -> SelectDialog {
    let item = |title: &str, description: &str, value: &str| Item { title: title.into(), description: description.into(), category: String::new(), value: value.into() };
    let items = vec![item("Accept", "download now", "accept"), item("Not now", "ask next time", "later"), item("Never", "do not ask again", "never")];
    SelectDialog::new(SelectKind::SetupBash, "Portable bash", items, None)
}

// Pinned sessions first, in pin order, then the rest by day.
pub fn sessions(list: &[Session], pinned: &[String], current: Option<&str>, now_ms: u64) -> SelectDialog {
    let today = day(now_ms);
    let item = |session: &Session, category: Option<&str>| {
        let date = day(session.time.updated);
        Item {
            title: session.title.clone(),
            description: crate::text::local_time(session.time.updated, "%H:%M"),
            category: category.map_or_else(|| if date == today { "Today".to_string() } else { date }, str::to_string),
            value: session.id.clone(),
        }
    };
    let mut items: Vec<Item> = pinned.iter().filter_map(|id| list.iter().find(|session| session.id == *id)).map(|session| item(session, Some("Pinned"))).collect();
    items.extend(list.iter().filter(|session| !pinned.contains(&session.id)).map(|session| item(session, None)));
    SelectDialog::new(SelectKind::Sessions, "Sessions", items, current.map(str::to_string))
}

pub fn models(catalog: &Catalog, state: &ModelState, current: Option<&ModelKey>) -> SelectDialog {
    let mut items = Vec::new();
    let mut pinned = |keys: &[ModelKey], category: &str| {
        for key in keys {
            if let Some((provider, model)) = catalog.model(key) {
                items.push(Item { title: model.name.clone(), description: provider.name.clone(), category: category.to_string(), value: key.spec() });
            }
        }
    };
    pinned(&state.favorite, "Favorites");
    pinned(&state.recent, "Recent");
    for provider in &catalog.providers {
        for model in &provider.models {
            let key = ModelKey::new(&provider.id, &model.id);
            let star = if state.favorite.contains(&key) { "★" } else { "" };
            items.push(Item { title: model.name.clone(), description: star.to_string(), category: provider.name.clone(), value: key.spec() });
        }
    }
    SelectDialog::new(SelectKind::Models, "Select model", items, current.map(ModelKey::spec))
}

pub fn agents(catalog: &Catalog, current: &str) -> SelectDialog {
    let items = catalog
        .primary_agents()
        .into_iter()
        .map(|agent| Item {
            title: agent.name.clone(),
            description: agent.description.clone().unwrap_or_default(),
            category: String::new(),
            value: agent.name.clone(),
        })
        .collect();
    SelectDialog::new(SelectKind::Agents, "Select agent", items, Some(current.to_string()))
}

pub fn themes(names: &[String], current: &str) -> SelectDialog {
    let items = names.iter().map(|name| Item { title: name.clone(), description: String::new(), category: String::new(), value: name.clone() }).collect();
    SelectDialog::new(SelectKind::Themes, "Themes", items, Some(current.to_string()))
}

fn user_text(parts: &[Part]) -> Option<&str> {
    parts.iter().find_map(|part| match &part.body {
        PartBody::Text(text) if text.synthetic != Some(true) && text.ignored != Some(true) => Some(text.text.as_str()),
        _ => None,
    })
}

// User messages, newest first. Times are UTC `HH:MM`; the original formats them in the process locale.
fn message_items(messages: &[(Message, Vec<Part>)]) -> Vec<Item> {
    let mut items: Vec<Item> = messages
        .iter()
        .filter_map(|(message, parts)| {
            let user = message.as_user()?;
            Some(Item { title: user_text(parts)?.replace('\n', " "), description: crate::text::local_time(user.time.created, "%H:%M"), category: String::new(), value: user.id.clone() })
        })
        .collect();
    items.reverse();
    items
}

pub fn timeline(messages: &[(Message, Vec<Part>)]) -> SelectDialog {
    SelectDialog::new(SelectKind::Timeline, "Timeline", message_items(messages), None)
}

pub fn fork(messages: &[(Message, Vec<Part>)]) -> SelectDialog {
    let mut items = vec![Item { title: "Full session".into(), description: String::new(), category: String::new(), value: String::new() }];
    items.extend(message_items(messages));
    SelectDialog::new(SelectKind::Fork, "Fork session", items, None)
}

pub fn message_actions(message_id: &str) -> SelectDialog {
    let item = |title: &str, value: &str, description: &str| Item { title: title.into(), description: description.into(), category: String::new(), value: value.into() };
    let items = vec![
        item("Revert", "session.revert", "undo messages and file changes"),
        item("Copy", "message.copy", "message text to clipboard"),
        item("Fork", "session.fork", "create a new session"),
    ];
    let mut dialog = SelectDialog::new(SelectKind::MessageActions, "Message Actions", items, None);
    dialog.context = Some(message_id.to_string());
    dialog
}

// `getRelativeTime`; older entries show the local date and time.
fn relative_time(timestamp: u64, now_ms: u64) -> String {
    let seconds = now_ms.saturating_sub(timestamp) / 1000;
    let (minutes, hours, days) = (seconds / 60, seconds / 3600, seconds / 86_400);
    match () {
        _ if seconds < 60 => "just now".into(),
        _ if minutes < 60 => format!("{minutes}m ago"),
        _ if hours < 24 => format!("{hours}h ago"),
        _ if days < 7 => format!("{days}d ago"),
        _ => crate::text::local_time(timestamp, "%Y-%m-%d %H:%M"),
    }
}

// Most recent first; the value is the entry's index in the stash.
pub fn stash(entries: &[Value], now_ms: u64) -> SelectDialog {
    let items = entries
        .iter()
        .enumerate()
        .rev()
        .map(|(index, entry)| {
            let input = entry.get("input").and_then(Value::as_str).unwrap_or_default();
            let lines = input.matches('\n').count() + 1;
            let age = relative_time(entry.get("timestamp").and_then(Value::as_u64).unwrap_or(0), now_ms);
            Item {
                title: crate::text::truncate(input.split('\n').next().unwrap_or_default().trim(), 50),
                description: if lines > 1 { format!("{age}  ~{lines} lines") } else { age },
                category: String::new(),
                value: index.to_string(),
            }
        })
        .collect();
    SelectDialog::new(SelectKind::Stash, "Stash", items, None)
}

pub fn variants(list: &[String], selected: Option<&str>) -> SelectDialog {
    let mut items = vec![Item { title: "Default".into(), description: String::new(), category: String::new(), value: "default".into() }];
    items.extend(list.iter().map(|name| Item { title: name.clone(), description: String::new(), category: String::new(), value: name.clone() }));
    SelectDialog::new(SelectKind::Variants, "Select variant", items, selected.map(str::to_string))
}

pub fn commands(app: &App) -> SelectDialog {
    let (keymap, in_session) = (&app.keymap, app.session().is_some());
    let child = app.session().is_some_and(|view| view.is_child());
    let items = COMMANDS
        .iter()
        .filter(|(key, ..)| in_session || !crate::app::SESSION_ONLY.contains(key))
        .filter(|(key, ..)| !in_session || *key != "tips_toggle")
        .filter(|(key, ..)| !child || !(key.starts_with("prompt_") || *key == "editor_open"))
        .filter(|(key, ..)| match *key {
            "prompt_stash" => !app.editor.is_empty(),
            "prompt_stash_pop" | "prompt_stash_list" => !app.stash.entries.is_empty(),
            "variant_list" | "variant_cycle" => !app.variants().is_empty(),
            _ => true,
        })
        .map(|(key, title, category, slashes)| Item {
            title: command_title(app, key).unwrap_or(title).to_string(),
            description: match (keymap.display(key), slashes.first()) {
                (shortcut, _) if !shortcut.is_empty() => shortcut,
                (_, Some(slash)) => format!("/{slash}"),
                _ => String::new(),
            },
            category: category.to_string(),
            value: key.to_string(),
        })
        .collect();
    SelectDialog::new(SelectKind::Commands, "Commands", items, None)
}

pub fn help(keymap: &Keymap) -> SelectDialog {
    let items = keybind::definitions()
        .iter()
        .filter(|def| !keymap.sequences(&def.name).is_empty())
        .map(|def| Item { title: def.description.clone(), description: keymap.display_all(&def.name), category: String::new(), value: def.name.clone() })
        .collect();
    SelectDialog::new(SelectKind::Help, "Help", items, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;

    #[test]
    fn model_picker_groups_and_filters() {
        let catalog = catalog::fixture();
        let mut state = ModelState::default();
        state.push_recent(&ModelKey::new("openai", "gpt-5"));
        state.push_recent(&ModelKey::new("gone", "model"));
        let mut dialog = models(&catalog, &state, Some(&ModelKey::new("anthropic", "claude-haiku-4-5")));
        let categories: Vec<&str> = dialog.filtered.iter().map(|&i| dialog.items[i].category.as_str()).collect();
        assert_eq!(categories, vec!["Recent", "Anthropic", "Anthropic", "OpenAI"]);
        assert_eq!(dialog.selected_item().unwrap().value, "anthropic/claude-haiku-4-5");
        dialog.set_query("haiku".into());
        assert_eq!(dialog.filtered.len(), 1);
        assert_eq!(dialog.selected_item().unwrap().title, "Claude Haiku 4.5");
        dialog.set_query("zzz".into());
        assert!(dialog.selected_item().is_none());
    }

    #[test]
    fn selection_wraps_and_clamps() {
        let names: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let mut dialog = themes(&names, "b");
        assert_eq!(dialog.selected, 1);
        dialog.move_by(1);
        dialog.move_by(1);
        assert_eq!(dialog.selected, 0);
        dialog.move_by(-1);
        assert_eq!(dialog.selected, 2);
        dialog.move_by(-10);
        assert_eq!(dialog.selected, 0);
        dialog.move_to(true);
        assert_eq!(dialog.selected, 2);
    }

    #[test]
    fn slash_names_resolve() {
        assert_eq!(slash_command("resume"), Some("session_list"));
        assert_eq!((slash_command("rewind"), slash_command("model"), slash_command("cost")), (Some("messages_undo"), Some("model_list"), Some("stats_show")));
        // Every alias points at a command this port has, and none shadows another command's own name.
        for (key, names) in ALIASES {
            assert!(COMMANDS.iter().any(|(k, ..)| k == key), "{key}");
            for name in *names {
                assert!(COMMANDS.iter().all(|(_, _, _, slashes)| !slashes.contains(name)), "{name}");
            }
        }
        assert_eq!(slash_command("q"), Some("app_exit"));
        assert_eq!(slash_command("nope"), None);
    }
}
