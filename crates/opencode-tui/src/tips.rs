// Static tips are read from the original's source; the tips it computes from live keybinds are rebuilt in `all`.
const SOURCE: &str = include_str!("../../../../opencode-ts/packages/tui/src/feature-plugins/home/tips-view.tsx");

use crate::keybind::Keymap;

fn highlight(text: &str) -> String {
    format!("{{highlight}}{text}{{/highlight}}")
}

fn press(shortcut: String, text: &str) -> Option<String> {
    (!shortcut.is_empty()).then(|| format!("Press {} {text}", highlight(&shortcut)))
}

fn command(slash: &str, shortcut: String, text: &str) -> Option<String> {
    let keys = if shortcut.is_empty() { highlight(slash) } else { format!("{} or {}", highlight(slash), highlight(&shortcut)) };
    Some(format!("Use {keys} {text}"))
}

pub fn fixed() -> Vec<String> {
    let start = SOURCE.find("const TIPS: Tip[] = [").unwrap_or(0);
    let body = &SOURCE[start..];
    let body = &body[..body.find("\n]\n").unwrap_or(body.len())];
    body.lines()
        .filter_map(|line| {
            let line = line.trim().strip_suffix(',')?;
            let quote = line.chars().next().filter(|c| matches!(c, '"' | '\''))?;
            let inner = line.strip_prefix(quote)?.strip_suffix(quote)?;
            Some(inner.replace("\\'", "'").replace("\\\"", "\""))
        })
        // Rust build: `upgrade` and `github install` are not implemented, so their tips are left out.
        .filter(|tip| !tip.contains("opencode upgrade") && !tip.contains("opencode github install"))
        .collect()
}

// Every tip the original computes from live keybinds. `key` is its `useCommandShortcut` (the first binding), `keys`
// its `configShortcut` (all bindings). `suspend` selects the platform tip: terminal suspend, or input undo on win32.
pub fn all(keymap: &Keymap, suspend: bool) -> Vec<String> {
    let key = |name: &str| keymap.display(name);
    let keys = |name: &str| keymap.display_all(name);
    let family: Vec<String> = ["session_parent", "session_child_first", "session_child_cycle_reverse", "session_child_cycle"].iter().map(|name| keys(name)).filter(|k| !k.is_empty()).collect();
    let (page_up, page_down) = (keys("messages_page_up"), keys("messages_page_down"));
    let (quick_first, quick_last) = (key("session_quick_switch_1"), key("session_quick_switch_9"));
    let palette = key("command_list");
    let mut tips: Vec<String> = [
        press(key("agent_cycle"), "to cycle between Build and Plan agents"),
        press(key("input_paste"), "to paste images from your clipboard into the prompt"),
        command("/editor", key("editor_open"), "to compose messages in your external editor"),
        command("/models", key("model_list"), "to switch between available AI models"),
        command("/themes", key("theme_list"), &format!("to switch between {} built-in themes", crate::theme::BUILTIN.len())),
        command("/new", key("session_new"), "to start a fresh conversation session"),
        command("/sessions", key("session_list"), "to list, pin, and continue sessions"),
        press(keys("session_pin_toggle"), "in the session list to pin one at the top"),
        (!quick_first.is_empty() && !quick_last.is_empty()).then(|| format!("Use {} through {} to switch pinned sessions", highlight(&quick_first), highlight(&quick_last))),
        command("/export", keys("session_export"), "to save the conversation as Markdown"),
        press(keys("messages_copy"), "to copy the assistant's last message to clipboard"),
        press(palette.clone(), "to see all available actions and commands"),
        Some(format!("The leader key is {}; combine with other keys for quick actions", highlight(&keys("leader")))),
        press(key("model_cycle_recent"), "to quickly switch between recently used models"),
        press(keys("sidebar_toggle"), "in a session to show or hide the sidebar panel"),
        (!page_up.is_empty() && !page_down.is_empty()).then(|| format!("Use {}/{} to navigate through conversation history", highlight(&page_up), highlight(&page_down))),
        press(keys("messages_first"), "to jump to the beginning of the conversation"),
        press(keys("messages_last"), "to jump to the most recent message"),
        press(key("input_newline"), "to add newlines in your prompt"),
        press(key("input_clear"), "when typing to clear the input field"),
        press(keys("session_interrupt"), "to stop the AI mid-response"),
        (!family.is_empty()).then(|| format!("Use {} for parent/child sessions", family.iter().map(|k| highlight(k)).collect::<Vec<_>>().join(" / "))),
        command("/timeline", keys("session_timeline"), "to jump to specific messages"),
        press(keys("messages_toggle_conceal"), "to toggle code block visibility in messages"),
        command("/status", key("status_view"), "to see system status info"),
        Some(if palette.is_empty() {
            "Toggle username display in chat via the command palette".to_string()
        } else {
            format!("Toggle username display in chat via the command palette ({})", highlight(&palette))
        }),
        command("/help", key("help_show"), "to show the help dialog"),
        if suspend { press(key("terminal_suspend"), "to suspend the terminal and return to your shell") } else { press(key("input_undo"), "to undo changes in your prompt") },
    ]
    .into_iter()
    .flatten()
    .collect();
    tips.extend(fixed());
    tips
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_static_tips_from_the_original() {
        let tips = fixed();
        assert!(tips.len() > 60, "{}", tips.len());
        assert!(tips.contains(&"Run {highlight}/compact{/highlight} to summarize long sessions near context limits".to_string()));
        assert!(tips.contains(&"Use {highlight}\"theme\": \"system\"{/highlight} to match your terminal's colors".to_string()));
        assert!(tips.iter().all(|tip| !tip.contains("shortcuts") && !tip.contains("opencode upgrade") && !tip.contains("github install")));
        assert!(tips.iter().any(|tip| tip.contains("in GitHub issues/PRs")));
        let (unix, _) = Keymap::new(&serde_json::Map::new(), 2000, true);
        let dynamic = all(&unix, true);
        for expected in [
            "Use {highlight}/models{/highlight} or {highlight}ctrl+x m{/highlight} to switch between available AI models",
            "Press {highlight}ctrl+v{/highlight} to paste images from your clipboard into the prompt",
            "Use {highlight}/editor{/highlight} or {highlight}ctrl+x e{/highlight} to compose messages in your external editor",
            "Press {highlight}ctrl+f{/highlight} in the session list to pin one at the top",
            "Use {highlight}ctrl+x 1{/highlight} through {highlight}ctrl+x 9{/highlight} to switch pinned sessions",
            "Use {highlight}pgup, ctrl+alt+b{/highlight}/{highlight}pgdn, ctrl+alt+f{/highlight} to navigate through conversation history",
            "Use {highlight}up{/highlight} / {highlight}ctrl+x down{/highlight} / {highlight}left{/highlight} / {highlight}right{/highlight} for parent/child sessions",
            "Use {highlight}/timeline{/highlight} or {highlight}ctrl+x g{/highlight} to jump to specific messages",
            "Toggle username display in chat via the command palette ({highlight}ctrl+p{/highlight})",
            "Use {highlight}/help{/highlight} to show the help dialog",
            "Press {highlight}ctrl+z{/highlight} to suspend the terminal and return to your shell",
            "The leader key is {highlight}ctrl+x{/highlight}; combine with other keys for quick actions",
        ] {
            assert!(dynamic.contains(&expected.to_string()), "missing {expected}\n{dynamic:#?}");
        }
        assert_eq!(dynamic.len(), fixed().len() + 28);
        let (windows, _) = Keymap::new(&serde_json::Map::new(), 2000, false);
        let dynamic = all(&windows, false);
        assert!(dynamic.contains(&"Press {highlight}ctrl+z{/highlight} to undo changes in your prompt".to_string()));
        let unbound = serde_json::json!({"command_list": "none", "messages_page_up": "none", "session_parent": "none"});
        let (sparse, _) = Keymap::new(unbound.as_object().unwrap(), 2000, true);
        let dynamic = all(&sparse, true);
        assert!(dynamic.contains(&"Toggle username display in chat via the command palette".to_string()));
        assert!(dynamic.iter().all(|tip| !tip.contains("conversation history")));
        assert!(dynamic.contains(&"Use {highlight}ctrl+x down{/highlight} / {highlight}left{/highlight} / {highlight}right{/highlight} for parent/child sessions".to_string()));
    }
}
