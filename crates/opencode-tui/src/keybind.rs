use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::OnceLock;

// The original's definition table is parsed straight from its source so names and defaults cannot drift.
const SOURCE: &str = include_str!("../../../../opencode-ts/packages/tui/src/config/keybind.ts");

pub const LEADER: &str = "leader";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct KeyStroke {
    pub name: String,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub sup: bool,
    pub hyper: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Part {
    Leader,
    Key(KeyStroke),
}

pub type Sequence = Vec<Part>;

#[derive(Debug, Clone)]
pub struct Definition {
    pub name: String,
    pub default: String,
    pub description: String,
    pub command: String,
}

fn quoted(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('"') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('"') else { break };
        out.push(&after[..end]);
        rest = &after[end + 1..];
    }
    out
}

fn section<'a>(start: &str, end: &str) -> &'a str {
    let from = SOURCE.find(start).map_or(0, |i| i + start.len());
    let len = SOURCE[from..].find(end).unwrap_or(0);
    &SOURCE[from..from + len]
}

pub fn definitions() -> &'static [Definition] {
    static DEFS: OnceLock<Vec<Definition>> = OnceLock::new();
    DEFS.get_or_init(|| {
        let leader_default = SOURCE
            .lines()
            .find_map(|l| l.strip_prefix("export const LeaderDefault = "))
            .and_then(|l| quoted(l).first().copied())
            .unwrap_or_default();
        let commands: HashMap<&str, &str> = section("export const CommandMap = {", "} satisfies BindingCommandMap")
            .lines()
            .filter_map(|line| {
                let (name, value) = line.trim().split_once(": ")?;
                Some((name.trim_matches('"'), *quoted(value).first()?))
            })
            .collect();
        section("export const Definitions = {", "} satisfies Record<string, Definition>")
            .lines()
            .filter_map(|line| {
                let (name, args) = line.trim().split_once(": keybind(")?;
                let name = name.trim_matches('"');
                let strings = quoted(args);
                let description = strings.last()?.to_string();
                let default = if args.starts_with("LeaderDefault") { leader_default } else { strings.first()? };
                Some(Definition {
                    name: name.to_string(),
                    default: default.to_string(),
                    description,
                    command: commands.get(name).copied().unwrap_or(name).to_string(),
                })
            })
            .chain(RUST_ONLY.iter().map(|(name, default, description, command)| Definition {
                name: name.to_string(),
                default: default.to_string(),
                description: description.to_string(),
                command: command.to_string(),
            }))
            .collect()
    })
}

// Rust build: keybinds of opencode 2.x features this build has; `prompt_queue` is 2.x's own name for its 1.x-style
// config (`packages/tui/src/config/v1/keybind.ts`), the others have no default.
const RUST_ONLY: &[(&str, &str, &str, &str)] = &[
    ("prompt_queue", "<leader>return", "Queue prompt", "prompt.queue"),
    ("session_aside", "none", "Ask a side question", "session.aside"),
    ("config_reload", "none", "Reload configuration", "location.reload"),
    ("stats_show", "none", "Usage statistics", "stats.open"),
];

fn alias(name: &str) -> &str {
    match name {
        "enter" => "return",
        "esc" => "escape",
        "pgdown" => "pagedown",
        "pgup" => "pageup",
        other => other,
    }
}

fn parse_stroke(text: &str) -> Option<KeyStroke> {
    let mut stroke = KeyStroke::default();
    let (mods, key) = match text.strip_suffix('+') {
        Some(mods) if !mods.is_empty() => (mods.trim_end_matches('+'), "+"),
        Some(_) => ("", "+"),
        None => text.rsplit_once('+').unwrap_or(("", text)),
    };
    for modifier in mods.split('+').filter(|m| !m.is_empty()) {
        match modifier.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => stroke.ctrl = true,
            "alt" | "meta" | "option" => stroke.alt = true,
            "shift" => stroke.shift = true,
            "super" => stroke.sup = true,
            "hyper" => stroke.hyper = true,
            _ => return None,
        }
    }
    if key.is_empty() {
        return None;
    }
    let mut chars = key.chars();
    let single = chars.next().filter(|_| chars.next().is_none());
    match single {
        Some(c) if c.is_alphabetic() && c.is_uppercase() => {
            stroke.shift = true;
            stroke.name = c.to_lowercase().collect();
        }
        Some(c) => stroke.name = c.to_string(),
        None => stroke.name = alias(&key.to_ascii_lowercase()).to_string(),
    }
    Some(normalize(stroke))
}

// Shift is only meaningful on letters and named keys; symbols already encode it.
fn normalize(mut stroke: KeyStroke) -> KeyStroke {
    let mut chars = stroke.name.chars();
    if let (Some(c), None) = (chars.next(), chars.next())
        && !c.is_alphabetic()
    {
        stroke.shift = false;
    }
    stroke
}

pub fn parse_sequence(text: &str) -> Option<Sequence> {
    let mut parts = Vec::new();
    let mut rest = text.trim();
    while let Some(after) = rest.strip_prefix("<leader>") {
        parts.push(Part::Leader);
        rest = after.trim_start();
    }
    if !rest.is_empty() {
        parts.push(Part::Key(parse_stroke(rest)?));
    }
    (!parts.is_empty()).then_some(parts)
}

// "a,b" -> alternatives; "none" and "" disable.
pub fn parse_binding(text: &str) -> Vec<Sequence> {
    if text.trim().eq_ignore_ascii_case("none") {
        return Vec::new();
    }
    text.split(',').map(str::trim).filter(|piece| !piece.is_empty()).filter_map(parse_sequence).collect()
}

fn stroke_from_object(map: &Map<String, Value>) -> Option<KeyStroke> {
    let flag = |key: &str| map.get(key).and_then(Value::as_bool).unwrap_or(false);
    let name = map.get("name")?.as_str()?;
    let mut stroke = parse_stroke(name)?;
    stroke.ctrl |= flag("ctrl");
    stroke.alt |= flag("meta");
    stroke.shift |= flag("shift");
    stroke.sup |= flag("super");
    stroke.hyper |= flag("hyper");
    Some(normalize(stroke))
}

// `BindingValueSchema`: false | "none" | string | KeyStroke | {key, ...} | array of those.
pub fn parse_value(value: &Value) -> Option<Vec<Sequence>> {
    match value {
        Value::Bool(false) => Some(Vec::new()),
        Value::String(text) => Some(parse_binding(text)),
        Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                out.extend(parse_value(item)?);
            }
            Some(out)
        }
        Value::Object(map) => match map.get("key") {
            Some(Value::String(text)) => Some(parse_binding(text)),
            Some(Value::Object(stroke)) => Some(vec![vec![Part::Key(stroke_from_object(stroke)?)]]),
            Some(_) => None,
            None => Some(vec![vec![Part::Key(stroke_from_object(map)?)]]),
        },
        _ => None,
    }
}

pub fn from_event(event: &KeyEvent) -> Option<KeyStroke> {
    let mods = event.modifiers;
    let mut stroke = KeyStroke {
        name: String::new(),
        ctrl: mods.contains(KeyModifiers::CONTROL),
        alt: mods.contains(KeyModifiers::ALT),
        shift: mods.contains(KeyModifiers::SHIFT),
        sup: mods.contains(KeyModifiers::SUPER),
        hyper: mods.contains(KeyModifiers::HYPER),
    };
    stroke.name = match event.code {
        KeyCode::Char(' ') => "space".into(),
        KeyCode::Char(c) if c.is_alphabetic() && c.is_uppercase() => {
            stroke.shift = true;
            c.to_lowercase().collect()
        }
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "return".into(),
        KeyCode::Esc => "escape".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => {
            stroke.shift = true;
            "tab".into()
        }
        KeyCode::Delete => "delete".into(),
        KeyCode::Insert => "insert".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::PageUp => "pageup".into(),
        KeyCode::PageDown => "pagedown".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::F(n) => format!("f{n}"),
        _ => return None,
    };
    Some(normalize(stroke))
}

// The character a stroke types into a text field, if any.
pub fn printable(event: &KeyEvent) -> Option<char> {
    let blocked = KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META;
    match event.code {
        KeyCode::Char(c) if !event.modifiers.intersects(blocked) => Some(c),
        _ => None,
    }
}

pub fn format_stroke(stroke: &KeyStroke) -> String {
    let mut out = String::new();
    for (on, label) in [(stroke.ctrl, "ctrl"), (stroke.alt, "alt"), (stroke.sup, "super"), (stroke.hyper, "hyper"), (stroke.shift, "shift")] {
        if on {
            out.push_str(label);
            out.push('+');
        }
    }
    out.push_str(match stroke.name.as_str() {
        "pageup" => "pgup",
        "pagedown" => "pgdn",
        "delete" => "del",
        other => other,
    });
    out
}

#[derive(Debug, Clone)]
pub struct Keymap {
    bindings: HashMap<String, Vec<Sequence>>,
    pub leader_timeout_ms: u64,
}

impl Keymap {
    // `terminal_suspend` false mirrors win32: suspend is disabled and ctrl+z becomes undo.
    pub fn new(overrides: &Map<String, Value>, leader_timeout_ms: u64, terminal_suspend: bool) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let mut bindings = HashMap::new();
        for def in definitions() {
            let mut sequences = parse_binding(&def.default);
            if let Some(value) = overrides.get(&def.name) {
                match parse_value(value) {
                    Some(parsed) => sequences = parsed,
                    None => warnings.push(format!("invalid keybind {}", def.name)),
                }
            } else if !terminal_suspend && def.name == "input_undo" {
                sequences.insert(0, parse_sequence("ctrl+z").unwrap_or_default());
            }
            if !terminal_suspend && def.name == "terminal_suspend" {
                sequences.clear();
            }
            bindings.insert(def.name.clone(), sequences);
        }
        let known: std::collections::HashSet<&str> = definitions().iter().map(|d| d.name.as_str()).collect();
        for key in overrides.keys().filter(|k| !known.contains(k.as_str())) {
            warnings.push(format!("unrecognized keybind {key}"));
        }
        (Self { bindings, leader_timeout_ms }, warnings)
    }

    pub fn defaults() -> Self {
        Self::new(&Map::new(), 2000, !cfg!(windows)).0
    }

    pub fn sequences(&self, name: &str) -> &[Sequence] {
        self.bindings.get(name).map_or(&[], Vec::as_slice)
    }

    pub fn is(&self, name: &str, sequence: &[Part]) -> bool {
        self.sequences(name).iter().any(|s| s.as_slice() == sequence)
    }

    pub fn find<'a>(&self, names: &[&'a str], sequence: &[Part]) -> Option<&'a str> {
        names.iter().copied().find(|name| self.is(name, sequence))
    }

    pub fn is_leader(&self, stroke: &KeyStroke) -> bool {
        self.sequences(LEADER).iter().any(|s| matches!(s.as_slice(), [Part::Key(k)] if k == stroke))
    }

    fn leader_display(&self) -> String {
        match self.sequences(LEADER).first().and_then(|s| s.first()) {
            Some(Part::Key(stroke)) => format_stroke(stroke),
            _ => String::new(),
        }
    }

    pub fn format(&self, sequence: &[Part]) -> String {
        sequence
            .iter()
            .map(|part| match part {
                Part::Leader => self.leader_display(),
                Part::Key(stroke) => format_stroke(stroke),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    // First binding of a config key, formatted for display; empty when unbound.
    pub fn display(&self, name: &str) -> String {
        self.sequences(name).first().map(|s| self.format(s)).unwrap_or_default()
    }

    pub fn display_all(&self, name: &str) -> String {
        self.sequences(name).iter().map(|s| self.format(s)).collect::<Vec<_>>().join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key(text: &str) -> Part {
        Part::Key(parse_stroke(text).unwrap())
    }

    #[test]
    fn table_is_read_from_source() {
        let defs = definitions();
        // 184 from the original's table, 4 Rust-only.
        assert_eq!(defs.len(), 188);
        assert_eq!(defs.iter().find(|d| d.name == "prompt_queue").map(|d| d.default.as_str()), Some("<leader>return"));
        let get = |name: &str| defs.iter().find(|d| d.name == name).unwrap();
        assert_eq!(get("leader").default, "ctrl+x");
        assert_eq!(get("app_exit").default, "ctrl+c,ctrl+d,<leader>q");
        assert_eq!(get("app_exit").command, "app.exit");
        assert_eq!(get("input_paste").default, "ctrl+v");
        assert_eq!(get("dialog.select.prev").command, "dialog.select.prev");
        assert_eq!(get("messages_page_up").command, "session.page.up");
        assert_eq!(get("which_key_end").default, "ctrl+alt+end");
    }

    #[test]
    fn parses_binding_strings() {
        assert_eq!(parse_binding("none"), Vec::<Sequence>::new());
        assert_eq!(parse_binding("ctrl+c,ctrl+d,<leader>q"), vec![vec![key("ctrl+c")], vec![key("ctrl+d")], vec![Part::Leader, key("q")]]);
        assert_eq!(parse_binding("<leader>down"), vec![vec![Part::Leader, key("down")]]);
        assert_eq!(parse_binding("enter,esc,pgup"), vec![vec![key("return")], vec![key("escape")], vec![key("pageup")]]);
        assert_eq!(parse_binding("ctrl+-,super+z").len(), 2);
        let upper = parse_stroke("E").unwrap();
        assert!(upper.shift && upper.name == "e");
        let combo = parse_stroke("ctrl+alt+shift+k").unwrap();
        assert!(combo.ctrl && combo.alt && combo.shift && combo.name == "k");
        assert_eq!(parse_stroke("meta+f").unwrap(), parse_stroke("alt+f").unwrap());
    }

    #[test]
    fn parses_binding_values() {
        assert_eq!(parse_value(&json!(false)), Some(vec![]));
        assert_eq!(parse_value(&json!({"key": "ctrl+v", "preventDefault": false})), Some(vec![vec![key("ctrl+v")]]));
        assert_eq!(parse_value(&json!({"name": "k", "ctrl": true})), Some(vec![vec![key("ctrl+k")]]));
        assert_eq!(parse_value(&json!(["f5", "<leader>z"])), Some(vec![vec![key("f5")], vec![Part::Leader, key("z")]]));
        assert_eq!(parse_value(&json!(3)), None);
    }

    #[test]
    fn overrides_and_platform_adjustment() {
        let overrides = json!({"session_new": "ctrl+n", "model_list": "none", "bogus": "x"});
        let (map, warnings) = Keymap::new(overrides.as_object().unwrap(), 500, false);
        assert!(map.is("session_new", &[key("ctrl+n")]));
        assert!(map.sequences("model_list").is_empty());
        assert!(map.sequences("terminal_suspend").is_empty());
        assert!(map.is("input_undo", &[key("ctrl+z")]));
        assert_eq!(warnings, vec!["unrecognized keybind bogus".to_string()]);
        let (unix, _) = Keymap::new(&Map::new(), 2000, true);
        assert!(unix.is("terminal_suspend", &[key("ctrl+z")]));
        assert!(!unix.is("input_undo", &[key("ctrl+z")]));
    }

    #[test]
    fn events_match_bindings() {
        let map = Keymap::defaults();
        let stroke = from_event(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)).unwrap();
        assert!(map.is_leader(&stroke));
        let tab = from_event(&KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)).unwrap();
        assert!(map.is("agent_cycle_reverse", &[Part::Key(tab)]));
        let question = from_event(&KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT)).unwrap();
        assert!(map.is("diff_help", &[Part::Key(question)]));
        assert_eq!(map.display("session_new"), "ctrl+x n");
        assert_eq!(map.display("messages_page_up"), "pgup");
        assert_eq!(map.display("help_show"), "");
    }
}
