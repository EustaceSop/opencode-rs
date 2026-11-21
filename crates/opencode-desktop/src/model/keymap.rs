// The original app's keybind table (`docs/spec/06-clients.md` §4.10), limited to what this client implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Command {
    NewSession,
    OpenSettings,
    Shortcuts,
    ToggleSidebar,
    OpenProject,
    FocusInput,
    FocusSearch,
    PreviousSession,
    NextSession,
    ChooseModel,
    CycleAgent,
    CycleAgentReverse,
    AutoAccept,
    AttachFiles,
    CycleTheme,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Keybind {
    // Command id of the original; `desktop.rs.*` ids exist only here.
    pub id: &'static str,
    // Original grammar: `mod+shift+s`, alternatives separated by commas.
    pub combo: &'static str,
    pub title: &'static str,
    pub group: &'static str,
    pub command: Command,
}

const GENERAL: &str = "settings.shortcuts.group.general";
const SESSION: &str = "settings.shortcuts.group.session";
const NAVIGATION: &str = "settings.shortcuts.group.navigation";
const MODEL: &str = "settings.shortcuts.group.modelAndAgent";
const PROMPT: &str = "settings.shortcuts.group.prompt";

pub const GROUPS: [&str; 5] = [GENERAL, SESSION, NAVIGATION, MODEL, PROMPT];

const fn bind(id: &'static str, combo: &'static str, title: &'static str, group: &'static str, command: Command) -> Keybind {
    Keybind { id, combo, title, group, command }
}

pub const KEYBINDS: &[Keybind] = &[
    bind("settings.open", "mod+comma", "command.settings.open", GENERAL, Command::OpenSettings),
    bind("desktop.rs.shortcuts", "f1", "settings.shortcuts.title", GENERAL, Command::Shortcuts),
    bind("sidebar.toggle", "mod+b", "command.sidebar.toggle", GENERAL, Command::ToggleSidebar),
    bind("project.open", "mod+o", "command.project.open", GENERAL, Command::OpenProject),
    bind("theme.cycle", "mod+shift+t", "command.theme.cycle", GENERAL, Command::CycleTheme),
    bind("session.new", "mod+shift+s,mod+t,mod+n", "command.session.new", SESSION, Command::NewSession),
    bind("permissions.autoaccept", "mod+shift+a", "command.permissions.autoaccept.enable", SESSION, Command::AutoAccept),
    bind("session.previous", "alt+arrowup", "command.session.previous", NAVIGATION, Command::PreviousSession),
    bind("session.next", "alt+arrowdown", "command.session.next", NAVIGATION, Command::NextSession),
    bind("home.sessions.search.focus", "mod+f", "home.sessions.search.placeholder", NAVIGATION, Command::FocusSearch),
    bind("model.choose", "mod+'", "command.model.choose", MODEL, Command::ChooseModel),
    bind("agent.cycle", "mod+.", "command.agent.cycle", MODEL, Command::CycleAgent),
    bind("agent.cycle.reverse", "shift+mod+.", "command.agent.cycle.reverse", MODEL, Command::CycleAgentReverse),
    bind("input.focus", "ctrl+l", "command.input.focus", PROMPT, Command::FocusInput),
    bind("file.attach", "mod+u", "prompt.action.attachFile", PROMPT, Command::AttachFiles),
];

// Keys the prompt handles itself (`prompt-input.tsx:1268-1416`), listed in the shortcuts view only.
pub const PROMPT_KEYS: &[(&str, &str)] = &[
    ("enter", "prompt.action.send"),
    ("shift+enter", "desktop.rs.prompt.newline"),
    ("escape", "prompt.action.stop"),
    ("arrowup", "desktop.rs.prompt.history.previous"),
    ("arrowdown", "desktop.rs.prompt.history.next"),
];

fn key_name(key: &str) -> &str {
    match key {
        "comma" => ",",
        "plus" => "+",
        "arrowup" => "up",
        "arrowdown" => "down",
        "arrowleft" => "left",
        "arrowright" => "right",
        "esc" => "escape",
        other => other,
    }
}

// One GPUI keystroke per alternative, e.g. `mod+shift+s,mod+t` -> ["ctrl-shift-s", "ctrl-t"].
pub fn keystrokes(combo: &str, mac: bool) -> Vec<String> {
    combo
        .split(',')
        .filter(|alternative| !alternative.trim().is_empty())
        .map(|alternative| {
            let (mut modifiers, mut key) = (Vec::new(), "");
            for part in alternative.trim().split('+') {
                match part.trim().to_lowercase().as_str() {
                    "mod" => modifiers.push(if mac { "cmd" } else { "ctrl" }),
                    "ctrl" | "control" => modifiers.push("ctrl"),
                    "meta" | "cmd" | "command" => modifiers.push("cmd"),
                    "alt" | "option" => modifiers.push("alt"),
                    "shift" => modifiers.push("shift"),
                    _ => key = part.trim(),
                }
            }
            // GPUI wants a fixed modifier order.
            let ordered: Vec<&str> = ["ctrl", "alt", "shift", "cmd"].into_iter().filter(|m| modifiers.contains(m)).collect();
            let key = key_name(&key.to_lowercase()).to_string();
            if ordered.is_empty() { key } else { format!("{}-{key}", ordered.join("-")) }
        })
        .collect()
}

// How a combo reads in the shortcuts view: `Ctrl+Shift+S`.
pub fn display(combo: &str, mac: bool) -> String {
    let first = combo.split(',').next().unwrap_or("").trim();
    first
        .split('+')
        .map(|part| match part.trim().to_lowercase().as_str() {
            "mod" => if mac { "Cmd" } else { "Ctrl" }.to_string(),
            "ctrl" | "control" => "Ctrl".to_string(),
            "meta" | "cmd" | "command" => "Cmd".to_string(),
            "alt" | "option" => "Alt".to_string(),
            "shift" => "Shift".to_string(),
            "comma" => ",".to_string(),
            "arrowup" => "↑".to_string(),
            "arrowdown" => "↓".to_string(),
            "enter" => "Enter".to_string(),
            "escape" | "esc" => "Esc".to_string(),
            key => key.to_uppercase(),
        })
        .collect::<Vec<_>>()
        .join("+")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn combos_convert_to_gpui_keystrokes() {
        assert_eq!(keystrokes("mod+shift+s,mod+t,mod+n", false), ["ctrl-shift-s", "ctrl-t", "ctrl-n"]);
        assert_eq!(keystrokes("mod+comma", true), ["cmd-,"]);
        assert_eq!(keystrokes("shift+mod+.", false), ["ctrl-shift-."]);
        assert_eq!(keystrokes("alt+arrowup", false), ["alt-up"]);
        assert_eq!(keystrokes("ctrl+l", true), ["ctrl-l"]);
        assert_eq!(keystrokes("f1", false), ["f1"]);
        assert_eq!(keystrokes("mod+'", false), ["ctrl-'"]);
        assert_eq!(display("mod+shift+s,mod+t", false), "Ctrl+Shift+S");
        assert_eq!(display("mod+comma", true), "Cmd+,");
        assert_eq!(display("alt+arrowdown", false), "Alt+↓");
    }

    #[test]
    fn the_table_has_no_duplicate_ids_or_colliding_keys() {
        let mut ids = HashSet::new();
        let mut keys = HashSet::new();
        for bind in KEYBINDS {
            assert!(ids.insert(bind.id), "duplicate id {}", bind.id);
            assert!(GROUPS.contains(&bind.group));
            for stroke in keystrokes(bind.combo, false) {
                assert!(keys.insert(stroke.clone()), "{stroke} is bound twice");
            }
        }
        let commands: HashSet<Command> = KEYBINDS.iter().map(|b| b.command).collect();
        assert_eq!(commands.len(), KEYBINDS.len(), "one binding row per command");
    }
}
