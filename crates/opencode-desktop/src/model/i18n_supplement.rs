// English-only strings of this client that have no key in the original dictionaries.
pub static SUPPLEMENT: &[(&str, &str)] = &[
    ("desktop.rs.catalog.noProviders", "No providers are configured. Add credentials with `opencode providers` or a provider entry in opencode.json."),
    ("desktop.rs.child.tools", "{{count}} tool calls"),
    ("desktop.rs.error.delete", "Could not delete the session: {{error}}"),
    ("desktop.rs.error.disconnect", "Could not disconnect the provider: {{error}}"),
    ("desktop.rs.error.modelState", "Could not save the model favorites: {{error}}"),
    ("desktop.rs.settings.about.build", "Rust build. It shares its configuration and provider credentials with opencode."),
    ("desktop.rs.error.fork", "Could not fork the session: {{error}}"),
    ("desktop.rs.error.listSessions", "Could not list sessions: {{error}}"),
    ("desktop.rs.error.openProject", "Could not open the project, not a directory: {{error}}"),
    ("desktop.rs.error.openSession", "Could not open the session: {{error}}"),
    ("desktop.rs.error.rename", "Could not rename the session: {{error}}"),
    ("desktop.rs.error.revert", "Could not revert the message: {{error}}"),
    ("desktop.rs.error.unrevert", "Could not restore the messages: {{error}}"),
    ("desktop.rs.fatal", "opencode could not start: {{error}}"),
    ("desktop.rs.prompt.history.next", "Next prompt in history"),
    ("desktop.rs.prompt.history.previous", "Previous prompt in history"),
    ("desktop.rs.prompt.newline", "New line"),
    ("desktop.rs.prompt.steer", "Send now: the running task reads it at its next step"),
    ("desktop.rs.reasoning.done", "Thought"),
    ("desktop.rs.settings.fontSize", "Font size"),
    ("desktop.rs.settings.shortcutsReadOnly", "Shortcuts cannot be changed in this version."),
    ("desktop.rs.settings.useCurrent", "Use current"),
    ("desktop.rs.setup.bash.title", "Portable bash"),
    ("desktop.rs.setup.bash.accept", "Accept"),
    ("desktop.rs.setup.bash.later", "Not now"),
    ("desktop.rs.setup.bash.never", "Never"),
    ("desktop.rs.setup.bash.download", "Download portable bash"),
    ("desktop.rs.setup.bash.started", "Downloading the portable bash (~38 MB) in the background."),
    ("desktop.rs.setup.bash.ready", "Portable bash ready: {{error}}"),
    ("desktop.rs.setup.bash.failed", "Could not download the portable bash: {{error}}"),
    ("desktop.rs.tool.empty", "No output yet"),
    ("desktop.rs.tool.input", "Input"),
    ("desktop.rs.tool.output", "Output"),
];

#[cfg(test)]
mod tests {
    use super::super::i18n::I18n;
    use super::SUPPLEMENT;
    use std::collections::HashSet;

    #[test]
    fn keys_are_unique_and_absent_from_original_dictionaries() {
        let english = I18n::default();
        let mut seen = HashSet::new();
        for (key, _) in SUPPLEMENT {
            assert!(seen.insert(*key), "duplicate supplement key {key}");
            assert!(!english.has(key), "{key} already exists in the original dictionary");
        }
    }

    // Every key the views and tables name must resolve to text, in the dictionaries or here.
    #[test]
    fn every_referenced_key_resolves() {
        let english = I18n::default();
        let sources = [
            include_str!("../ui/app.rs"),
            include_str!("../ui/composer.rs"),
            include_str!("../ui/docks.rs"),
            include_str!("../ui/menu.rs"),
            include_str!("../ui/models.rs"),
            include_str!("../ui/overlay.rs"),
            include_str!("../ui/providers.rs"),
            include_str!("../ui/settings.rs"),
            include_str!("../ui/rows.rs"),
            include_str!("../ui/sidebar.rs"),
            include_str!("../main.rs"),
            include_str!("backend.rs"),
            include_str!("state.rs"),
            include_str!("prompt.rs"),
            include_str!("attention.rs"),
            include_str!("menu.rs"),
        ];
        // These tables mix i18n keys with command and setting ids, so they are read through their accessors.
        use super::super::keymap::{GROUPS, KEYBINDS, PROMPT_KEYS};
        use super::super::settings::{Scheme, Toggle};
        let toggles = Toggle::GENERAL.into_iter().chain(Toggle::NOTIFICATIONS).chain(Toggle::SOUNDS);
        let listed = KEYBINDS
            .iter()
            .map(|bind| bind.title)
            .chain(GROUPS)
            .chain(PROMPT_KEYS.iter().map(|(_, title)| *title))
            .chain(Scheme::ALL.map(Scheme::key))
            .chain(toggles.flat_map(|toggle| [Some(toggle.title()), toggle.description()]).flatten());
        for key in listed {
            assert_ne!(&*english.t(key), key, "no text for {key}");
        }
        let prefixes = [
            "desktop.rs.", "ui.", "common.", "command.", "settings.", "session.", "prompt.", "home.", "context.", "toast.", "notification.", "theme.", "dialog.",
            "sidebar.", "provider.", "model.tag.",
        ];
        let mut checked = 0;
        for source in sources {
            for literal in source.split('"').skip(1).step_by(2) {
                let looks_like_key = prefixes.iter().any(|p| literal.starts_with(p))
                    && literal.contains('.')
                    && literal.chars().all(|c| c.is_ascii_alphanumeric() || c == '.')
                    && !literal.ends_with('.');
                if looks_like_key && literal != "session.revertDock.summary" && !literal.ends_with(".rs") {
                    assert_ne!(&*english.t(literal), literal, "no text for {literal}");
                    checked += 1;
                }
            }
        }
        assert!(checked > 120, "only {checked} keys were found; the scan is broken");
        assert!(english.has("session.revertDock.summary.one") && english.has("session.revertDock.summary.other"));
    }
}
