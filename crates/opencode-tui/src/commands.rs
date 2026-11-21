// Session-level commands of the reducer: navigation between subagent sessions, timeline/fork/export dialogs,
// stash, variants, toggles kept in `kv.json`, and the terminal-derived theme.
use crate::app::{App, EditorPurpose, Effect, ToastKind, edit_field};
use crate::dialog::{self, Dialog, EXPORT_FIELDS, ExportDialog, Item, SelectKind};
use crate::editor::Editor;
use crate::export;
use crate::keybind::Part;
use crate::stash;
use crate::terminal::{self, TerminalColors};
use crate::theme::Mode;
use opencode_storage::{Part as StoredPart, PartBody};
use ratatui::crossterm::event::KeyEvent;
use serde_json::{Value, json};

// `{input, parts}` of a stored user message: its typed text and file attachments.
fn prompt_of(parts: &[StoredPart]) -> Value {
    let mut input = String::new();
    let mut files = Vec::new();
    for part in parts {
        match &part.body {
            PartBody::Text(text) if text.synthetic != Some(true) => input.push_str(&text.text),
            PartBody::File(file) => {
                let mut value = serde_json::to_value(file).unwrap_or_default();
                if let Some(map) = value.as_object_mut() {
                    map.insert("type".into(), json!("file"));
                }
                files.push(value);
            }
            _ => {}
        }
    }
    json!({ "input": input, "parts": files })
}

impl App {
    // Variants of the current model.
    pub fn variants(&self) -> Vec<String> {
        self.current_model().and_then(|key| self.catalog.model(&key).map(|(_, model)| model.variants.clone())).unwrap_or_default()
    }

    // `local.model.variant.current`
    pub fn current_variant(&self) -> Option<String> {
        let key = self.current_model()?.spec();
        let selected = self.models.variant.get(&key)?.as_str()?.to_string();
        self.variants().contains(&selected).then_some(selected)
    }

    fn set_variant(&mut self, value: Option<&str>) -> Vec<Effect> {
        let Some(key) = self.current_model() else { return Vec::new() };
        self.models.variant.insert(key.spec(), json!(value.unwrap_or("default")));
        vec![Effect::SaveModels]
    }

    fn toggle_kv(&mut self, key: &str, default: bool) -> Vec<Effect> {
        let next = !self.kv.get_bool(key, default);
        self.kv.set(key, json!(next));
        self.part_cache.borrow_mut().clear();
        vec![Effect::SaveKv]
    }

    fn apply_mode(&mut self, mode: Mode) {
        self.mode = mode;
        if let Some(system) = terminal::system_theme(&self.terminal, mode) {
            self.themes.insert("system", system);
        }
        self.theme = self.themes.resolve(&self.theme_name, mode);
        self.part_cache.borrow_mut().clear();
    }

    // Colours reported by the terminal: sets the mode unless it is locked, and builds the `system` theme.
    pub(crate) fn set_terminal(&mut self, colors: TerminalColors) {
        self.terminal = colors;
        let locked = self.kv.get_str("theme_mode_lock").is_some();
        let mode = if locked { self.mode } else { terminal::mode(&self.terminal).unwrap_or(self.mode) };
        self.apply_mode(mode);
        // `system` was asked for before the palette was known.
        let wanted = self.config.theme.clone().or_else(|| self.kv.get_str(crate::theme::KV_THEME).map(str::to_string));
        if wanted.as_deref() == Some("system") && self.themes.has("system") {
            self.theme_name = "system".into();
            self.theme = self.themes.resolve("system", self.mode);
        }
    }

    fn open_child(&self, direction: i64) -> Vec<Effect> {
        let Some(view) = self.session().filter(|view| view.is_child()) else { return Vec::new() };
        // `children()` is ordered by id.
        let mut ids: Vec<&str> = view.children.iter().filter(|child| child.info.parent_id == view.info.parent_id).map(|child| child.info.id.as_str()).collect();
        ids.sort_unstable();
        if ids.len() <= 1 {
            return Vec::new();
        }
        let at = ids.iter().position(|id| *id == view.info.id).map_or(-1, |i| i as i64) - direction;
        let next = if at >= ids.len() as i64 { 0 } else if at < 0 { ids.len() - 1 } else { at as usize };
        vec![Effect::OpenSession(ids[next].to_string())]
    }

    // First line of the nearest message above or below the top of the viewport.
    fn scroll_to_message(&mut self, pick: impl Fn(&[(String, bool, usize)], usize) -> Option<usize>) {
        let (height, total) = self.viewport.get();
        let Some(scroll) = self.session().map(|view| view.scroll) else { return };
        let top = total.saturating_sub(height + scroll.min(total.saturating_sub(height)));
        let target = pick(&self.anchors.borrow(), top);
        let max = total.saturating_sub(height);
        if let (Some(line), Some(view)) = (target, self.session_mut()) {
            view.scroll = max.saturating_sub(line.saturating_sub(1).min(max));
        }
    }

    pub(crate) fn run_extra(&mut self, name: &str) -> Vec<Effect> {
        let in_session = self.session().is_some();
        match name {
            "sidebar_toggle" if in_session => {
                let visible = self.sidebar_visible();
                self.kv.set("sidebar", json!(if visible { "hide" } else { "auto" }));
                self.sidebar_open = !visible;
                return vec![Effect::SaveKv];
            }
            "scrollbar_toggle" => return self.toggle_kv("scrollbar_visible", false),
            "session_toggle_generic_tool_output" => return self.toggle_kv("generic_tool_output_visibility", false),
            "app_toggle_animations" => return self.toggle_kv("animations_enabled", true),
            "sounds_toggle" => return self.toggle_kv("rs.sounds", true),
            "terminal_title_toggle" => return self.toggle_kv("terminal_title_enabled", true),
            "app_toggle_paste_summary" => {
                let next = !self.paste_summary();
                self.kv.set("paste_summary_enabled", json!(next));
                return vec![Effect::SaveKv];
            }
            "session_toggle_timestamps" => {
                let next = if self.show_timestamps() { "hide" } else { "show" };
                self.kv.set("timestamps", json!(next));
                return vec![Effect::SaveKv];
            }
            "permission_mode" => self.auto_permissions = !self.auto_permissions,
            "messages_toggle_conceal" => {
                self.conceal = !self.conceal;
                self.part_cache.borrow_mut().clear();
            }
            "theme_switch_mode" => {
                let next = if self.mode == Mode::Dark { Mode::Light } else { Mode::Dark };
                self.apply_mode(next);
                // `setMode` only persists a mode that is locked.
                if self.kv.get_str("theme_mode_lock").is_some() {
                    self.kv.set("theme_mode", json!(next.key()));
                    return vec![Effect::SaveKv];
                }
            }
            "theme_mode_lock" => {
                if self.kv.get_str("theme_mode_lock").is_some() {
                    self.kv.remove("theme_mode_lock");
                    self.kv.remove("theme_mode");
                    let mode = terminal::mode(&self.terminal).unwrap_or(self.mode);
                    self.apply_mode(mode);
                } else {
                    self.kv.set("theme_mode_lock", json!(self.mode.key()));
                }
                return vec![Effect::SaveKv];
            }
            "terminal_suspend" => return vec![Effect::Suspend],
            "docs_open" => return vec![Effect::OpenUrl("https://opencode.ai/docs".into())],
            _ if name.starts_with("session_quick_switch_") => {
                let slot = name.rsplit('_').next().and_then(|n| n.parse().ok()).unwrap_or(0);
                return vec![Effect::QuickSwitch(slot)];
            }
            "variant_cycle" => {
                let list = self.variants();
                if list.is_empty() {
                    return Vec::new();
                }
                let next = match self.current_variant().and_then(|current| list.iter().position(|v| *v == current)) {
                    None => Some(list[0].clone()),
                    Some(index) => list.get(index + 1).cloned(),
                };
                return self.set_variant(next.as_deref());
            }
            "variant_list" => {
                let list = self.variants();
                if list.is_empty() {
                    self.show_toast("No variants available: The current model does not support any variants.", ToastKind::Info);
                    return Vec::new();
                }
                let key = self.current_model().map(|key| key.spec()).unwrap_or_default();
                let selected = self.models.variant.get(&key).and_then(Value::as_str).map(str::to_string);
                self.dialog = Some(Dialog::Select(dialog::variants(&list, selected.as_deref())));
            }
            "editor_open" => {
                // Pasted summaries are edited as the text they stand for.
                let mut text = self.editor.text.clone();
                for part in self.attachments.iter().filter(|part| part.get("type").and_then(Value::as_str) == Some("text")) {
                    if let (Some(label), Some(full)) = (part.pointer("/source/text/value").and_then(Value::as_str), part.get("text").and_then(Value::as_str)) {
                        text = text.replacen(label, full, 1);
                    }
                }
                return vec![Effect::OpenEditor { purpose: EditorPurpose::Prompt, text }];
            }
            "prompt_stash" => {
                if self.editor.is_empty() {
                    return Vec::new();
                }
                let write = self.stash.push(&self.editor.text, &self.attachments, self.now_ms);
                self.editor.clear();
                self.attachments.clear();
                self.edited();
                return vec![Effect::Stash(write)];
            }
            "prompt_stash_pop" => {
                let Some(entry) = self.stash.pop() else { return Vec::new() };
                self.restore(&entry);
                return vec![Effect::Stash(stash::Write::Rewrite)];
            }
            "prompt_stash_list" => {
                if !self.stash.entries.is_empty() {
                    self.dialog = Some(Dialog::Select(dialog::stash(&self.stash.entries, self.now_ms)));
                }
            }
            "session_timeline" => {
                if let Some(view) = self.session() {
                    self.dialog = Some(Dialog::Select(dialog::timeline(&view.messages)));
                }
            }
            "session_fork" => {
                if let Some(view) = self.session() {
                    self.dialog = Some(Dialog::Select(dialog::fork(&view.messages)));
                }
            }
            "session_copy" => {
                let Some(view) = self.session() else { return Vec::new() };
                let options = export::Options { thinking: true, tool_details: self.show_details, assistant_metadata: self.kv.get_bool("assistant_metadata_visibility", true) };
                let text = export::format_transcript(&view.info, &view.messages, &self.catalog, options);
                self.show_toast("Session transcript copied to clipboard!", ToastKind::Info);
                return vec![Effect::Copy(text)];
            }
            "session_export" => {
                let Some(view) = self.session() else { return Vec::new() };
                let filename = format!("session-{}.md", view.info.id.chars().take(8).collect::<String>());
                let flags = [true, self.show_details, self.kv.get_bool("assistant_metadata_visibility", true), false];
                self.dialog = Some(Dialog::Export(ExportDialog { filename: Editor::with_text(&filename), flags, active: 0 }));
            }
            "session_child_first" => {
                // Subagent sessions of this one, in id order.
                let Some(view) = self.session() else { return Vec::new() };
                let root = view.info.parent_id.clone().unwrap_or_else(|| view.info.id.clone());
                let first = view.children.iter().filter(|child| child.info.parent_id.as_deref() == Some(root.as_str())).map(|child| child.info.id.clone()).min();
                return first.map(|id| vec![Effect::OpenSession(id)]).unwrap_or_default();
            }
            "session_parent" => {
                let parent = self.session().and_then(|view| view.info.parent_id.clone());
                return parent.map(|id| vec![Effect::OpenSession(id)]).unwrap_or_default();
            }
            "session_child_cycle" => return self.open_child(1),
            "session_child_cycle_reverse" => return self.open_child(-1),
            "messages_next" => self.scroll_to_message(|anchors, top| anchors.iter().map(|(_, _, line)| *line).find(|line| *line > top + 1)),
            "messages_previous" => self.scroll_to_message(|anchors, top| anchors.iter().map(|(_, _, line)| *line).rev().find(|line| *line < top)),
            "messages_last_user" => self.scroll_to_message(|anchors, _| anchors.iter().rev().find(|(_, user, _)| *user).map(|(_, _, line)| *line)),
            _ => {}
        }
        Vec::new()
    }

    pub(crate) fn dialog_submit_extra(&mut self, kind: SelectKind, context: Option<String>, item: Item) -> Vec<Effect> {
        self.dialog = None;
        let Some(session_id) = self.session().map(|view| view.info.id.clone()) else {
            // Stash and variants also work on the home screen.
            return match kind {
                SelectKind::Stash => self.take_stash(&item.value),
                SelectKind::Variants => self.set_variant((item.value != "default").then_some(item.value.as_str())),
                _ => Vec::new(),
            };
        };
        let message = |app: &App, id: &str| app.session().and_then(|view| view.messages.iter().find(|(m, _)| m.id() == id).map(|(_, parts)| parts.clone()));
        match kind {
            SelectKind::Timeline => self.dialog = Some(Dialog::Select(dialog::message_actions(&item.value))),
            SelectKind::Fork if item.value.is_empty() => return vec![Effect::Fork { session_id, message_id: None, prompt: None }],
            SelectKind::Fork => {
                let prompt = message(self, &item.value).map(|parts| prompt_of(&parts));
                return vec![Effect::Fork { session_id, message_id: Some(item.value), prompt }];
            }
            SelectKind::MessageActions => {
                let Some(message_id) = context else { return Vec::new() };
                let Some(parts) = message(self, &message_id) else { return Vec::new() };
                let prompt = prompt_of(&parts);
                match item.value.as_str() {
                    "session.revert" => {
                        if self.session().is_some_and(|view| view.busy()) {
                            return Vec::new();
                        }
                        self.restore(&prompt);
                        return vec![Effect::Undo { session_id, message_id }];
                    }
                    "message.copy" => return vec![Effect::Copy(prompt["input"].as_str().unwrap_or_default().to_string())],
                    _ => return vec![Effect::Fork { session_id, message_id: Some(message_id), prompt: Some(prompt) }],
                }
            }
            SelectKind::Stash => return self.take_stash(&item.value),
            SelectKind::Variants => return self.set_variant((item.value != "default").then_some(item.value.as_str())),
            _ => {}
        }
        Vec::new()
    }

    fn take_stash(&mut self, index: &str) -> Vec<Effect> {
        let Some(entry) = index.parse().ok().and_then(|index| self.stash.remove(index)) else { return Vec::new() };
        self.restore(&entry);
        vec![Effect::Stash(stash::Write::Rewrite)]
    }

    pub(crate) fn export_key(&mut self, sequence: &[Part], event: Option<KeyEvent>) -> Vec<Effect> {
        let keymap = self.keymap.clone();
        let Some(Dialog::Export(export)) = &mut self.dialog else { return Vec::new() };
        let plain = |name: &str| matches!(sequence, [Part::Key(stroke)] if stroke.name == name && !stroke.ctrl && !stroke.alt);
        if plain("tab") {
            let back = matches!(sequence, [Part::Key(stroke)] if stroke.shift);
            export.active = (export.active + if back { EXPORT_FIELDS - 1 } else { 1 }) % EXPORT_FIELDS;
            return Vec::new();
        }
        if plain("space") && export.active > 0 {
            export.flags[export.active - 1] = !export.flags[export.active - 1];
            return Vec::new();
        }
        if !plain("return") {
            if export.active == 0 {
                edit_field(&mut export.filename, &keymap, sequence, event.as_ref());
            }
            return Vec::new();
        }
        let (filename, [thinking, tool_details, assistant_metadata, open_only]) = (export.filename.text.trim().to_string(), export.flags);
        self.dialog = None;
        let Some(view) = self.session() else { return Vec::new() };
        let text = export::format_transcript(&view.info, &view.messages, &self.catalog, export::Options { thinking, tool_details, assistant_metadata });
        if open_only {
            return vec![Effect::OpenEditor { purpose: EditorPurpose::Export { filename: None }, text }];
        }
        if filename.is_empty() {
            self.show_toast("Failed to export session", ToastKind::Error);
            return Vec::new();
        }
        // Written first, then offered to the editor; an edited copy replaces it.
        vec![Effect::WriteFile { filename: filename.clone(), text: text.clone() }, Effect::OpenEditor { purpose: EditorPurpose::Export { filename: Some(filename) }, text }]
    }

    pub(crate) fn editor_closed(&mut self, purpose: EditorPurpose, content: Option<String>) -> Vec<Effect> {
        match purpose {
            EditorPurpose::Prompt => {
                let Some(content) = content else { return Vec::new() };
                let text = terminal::normalize_prompt_content(&content).to_string();
                // Attachments whose placeholder the user deleted are dropped; pasted summaries were expanded.
                self.attachments.retain(|part| {
                    part.get("type").and_then(Value::as_str) != Some("text") && part.pointer("/source/text/value").and_then(Value::as_str).is_some_and(|label| text.contains(label))
                });
                self.editor.set(&text);
                self.edited();
                Vec::new()
            }
            EditorPurpose::Export { filename } => {
                let mut effects = Vec::new();
                if let Some(filename) = filename {
                    if let Some(text) = content {
                        effects.push(Effect::WriteFile { filename: filename.clone(), text });
                    }
                    self.show_toast(format!("Session exported to {filename}"), ToastKind::Info);
                }
                effects
            }
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::testing::{app, ctrl, open, press, session, type_text};
    use crate::app::{Action, Child};
    use crate::transcript::testing::conversation;
    use opencode_session::Status;
    use ratatui::crossterm::event::KeyCode;

    fn leader(app: &mut App, c: char) -> Vec<Effect> {
        ctrl(app, 'x');
        press(app, KeyCode::Char(c))
    }

    #[test]
    fn timeline_offers_revert_copy_and_fork() {
        let (_dir, mut app) = app();
        open(&mut app, session("ses_1", "Demo"), conversation());
        leader(&mut app, 'g');
        assert!(matches!(&app.dialog, Some(Dialog::Select(d)) if d.kind == SelectKind::Timeline && d.items[0].title == "list the files"));
        press(&mut app, KeyCode::Enter);
        let Some(Dialog::Select(actions)) = &app.dialog else { panic!("no actions") };
        assert_eq!(actions.items.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(), vec!["Revert", "Copy", "Fork"]);
        press(&mut app, KeyCode::Down);
        assert_eq!(press(&mut app, KeyCode::Enter), vec![Effect::Copy("list the files".into())]);

        leader(&mut app, 'g');
        press(&mut app, KeyCode::Enter);
        assert_eq!(press(&mut app, KeyCode::Enter), vec![Effect::Undo { session_id: "ses_1".into(), message_id: "msg_1".into() }]);
        assert_eq!(app.editor.text, "list the files");

        app.editor.clear();
        type_text(&mut app, "/fork");
        press(&mut app, KeyCode::Enter);
        let Some(Dialog::Select(fork)) = &app.dialog else { panic!("no fork dialog") };
        assert_eq!(fork.items.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(), vec!["Full session", "list the files"]);
        assert_eq!(press(&mut app, KeyCode::Enter), vec![Effect::Fork { session_id: "ses_1".into(), message_id: None, prompt: None }]);
        app.run("session_fork");
        press(&mut app, KeyCode::Down);
        let effects = press(&mut app, KeyCode::Enter);
        assert_eq!(effects, vec![Effect::Fork { session_id: "ses_1".into(), message_id: Some("msg_1".into()), prompt: Some(json!({"input": "list the files", "parts": []})) }]);
    }

    #[test]
    fn export_dialog_writes_then_opens_the_editor() {
        let (_dir, mut app) = app();
        open(&mut app, session("ses_abcdefgh123", "Demo"), conversation());
        leader(&mut app, 'x');
        let Some(Dialog::Export(export)) = &app.dialog else { panic!("no export dialog") };
        assert_eq!((export.filename.text.as_str(), export.flags), ("session-ses_abcd.md", [true, true, true, false]));
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Tab);
        type_text(&mut app, "x");
        let effects = press(&mut app, KeyCode::Enter);
        let [Effect::WriteFile { filename, text }, Effect::OpenEditor { purpose, .. }] = effects.as_slice() else { panic!("{effects:?}") };
        assert_eq!(filename, "session-ses_abcd.mdx");
        assert!(text.starts_with("# Demo\n\n**Session ID:** ses_abcdefgh123") && text.contains("## User\n\nlist the files"));
        assert_eq!(*purpose, EditorPurpose::Export { filename: Some("session-ses_abcd.mdx".into()) });
        let done = app.update(Action::EditorClosed { purpose: purpose.clone(), content: Some("edited".into()) });
        assert_eq!(done, vec![Effect::WriteFile { filename: "session-ses_abcd.mdx".into(), text: "edited".into() }]);
        assert_eq!(app.toast.as_ref().unwrap().message, "Session exported to session-ses_abcd.mdx");

        app.run("session_export");
        for _ in 0..4 {
            press(&mut app, KeyCode::Tab);
        }
        press(&mut app, KeyCode::Char(' '));
        let effects = press(&mut app, KeyCode::Enter);
        assert!(matches!(effects.as_slice(), [Effect::OpenEditor { purpose: EditorPurpose::Export { filename: None }, .. }]), "{effects:?}");
        let copied = app.run("session_copy");
        assert!(matches!(copied.as_slice(), [Effect::Copy(text)] if text.contains("**Tool: bash**")));
    }

    #[test]
    fn stash_push_pop_list_and_delete() {
        let (_dir, mut app) = app();
        assert_eq!(app.run("prompt_stash"), vec![]);
        type_text(&mut app, "first");
        let effects = app.run("prompt_stash");
        assert!(matches!(effects.as_slice(), [Effect::Stash(stash::Write::Append(entry))] if entry["input"] == "first"));
        assert!(app.editor.is_empty());
        type_text(&mut app, "second\nmore");
        app.editor.set("second\nmore");
        app.run("prompt_stash");
        app.run("prompt_stash_list");
        let Some(Dialog::Select(list)) = &app.dialog else { panic!("no stash dialog") };
        assert_eq!(list.items.iter().map(|i| (i.title.as_str(), i.description.as_str())).collect::<Vec<_>>(), vec![("second", "just now  ~2 lines"), ("first", "just now")]);
        assert_eq!(ctrl(&mut app, 'd'), vec![]);
        assert_eq!(ctrl(&mut app, 'd'), vec![Effect::Stash(stash::Write::Rewrite)]);
        assert_eq!(app.stash.entries.len(), 1);
        assert_eq!(press(&mut app, KeyCode::Enter), vec![Effect::Stash(stash::Write::Rewrite)]);
        assert_eq!(app.editor.text, "first");
        assert!(app.stash.entries.is_empty() && app.dialog.is_none());
        app.editor.clear();
        type_text(&mut app, "again");
        app.run("prompt_stash");
        assert_eq!(app.run("prompt_stash_pop"), vec![Effect::Stash(stash::Write::Rewrite)]);
        assert_eq!(app.editor.text, "again");
    }

    #[test]
    fn variants_cycle_through_the_models_list_and_back_to_default() {
        let (_dir, mut app) = app();
        assert_eq!(app.run("variant_cycle"), vec![]);
        app.run("variant_list");
        assert!(app.toast.as_ref().unwrap().message.starts_with("No variants available"));
        app.catalog.providers[0].models[0].variants = vec!["high".into(), "max".into()];
        assert_eq!(app.run("variant_cycle"), vec![Effect::SaveModels]);
        assert_eq!(app.current_variant().as_deref(), Some("high"));
        app.run("variant_cycle");
        assert_eq!(app.current_variant().as_deref(), Some("max"));
        app.run("variant_cycle");
        assert_eq!(app.current_variant(), None);
        assert_eq!(app.models.variant["anthropic/claude-sonnet-4-5"], "default");
        ctrl(&mut app, 't');
        type_text(&mut app, "go");
        let effects = press(&mut app, KeyCode::Enter);
        assert!(matches!(effects.last(), Some(Effect::Submit { variant: Some(v), .. }) if v == "high"), "{effects:?}");
        app.run("variant_list");
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.current_variant().as_deref(), Some("max"));
    }

    #[test]
    fn toggles_persist_under_the_original_keys() {
        let (_dir, mut app) = app();
        open(&mut app, session("ses_1", "Demo"), conversation());
        for (command, key, expected) in [
            ("scrollbar_toggle", "scrollbar_visible", json!(true)),
            ("session_toggle_generic_tool_output", "generic_tool_output_visibility", json!(true)),
            ("app_toggle_animations", "animations_enabled", json!(false)),
            ("terminal_title_toggle", "terminal_title_enabled", json!(false)),
            ("app_toggle_paste_summary", "paste_summary_enabled", json!(false)),
            ("session_toggle_timestamps", "timestamps", json!("show")),
            ("sidebar_toggle", "sidebar", json!("auto")),
            ("tool_details", "tool_details_visibility", json!(false)),
            ("display_thinking", "thinking_mode", json!("show")),
            ("theme_mode_lock", "theme_mode_lock", json!("dark")),
        ] {
            assert_eq!(app.run(command), vec![Effect::SaveKv], "{command}");
            assert_eq!(app.kv.map[key], expected, "{command}");
        }
        assert!(app.sidebar_visible() && app.show_timestamps() && app.show_scrollbar());
        app.run("sidebar_toggle");
        assert_eq!((app.kv.map["sidebar"].clone(), app.sidebar_visible()), (json!("hide"), false));
        assert_eq!(app.run("theme_switch_mode"), vec![Effect::SaveKv]);
        assert_eq!((app.mode, app.kv.map["theme_mode"].clone()), (Mode::Light, json!("light")));
        app.run("theme_mode_lock");
        assert!(!app.kv.map.contains_key("theme_mode_lock") && !app.kv.map.contains_key("theme_mode"));
        assert_eq!(app.run("theme_switch_mode"), vec![]);
        assert!(app.conceal);
        leader(&mut app, 'h');
        assert!(!app.conceal);
    }

    #[test]
    fn terminal_colours_set_the_mode_and_the_system_theme() {
        let (_dir, mut app) = app();
        let white = crate::theme::Rgba::rgb(250, 250, 250);
        let colors = TerminalColors { background: Some(white), foreground: Some(crate::theme::Rgba::rgb(10, 10, 10)), palette: vec![Some(white); 16] };
        app.kv.set(crate::theme::KV_THEME, json!("system"));
        app.update(Action::Terminal(colors.clone()));
        assert_eq!((app.mode, app.theme_name.as_str()), (Mode::Light, "system"));
        assert_eq!(app.theme.text, crate::theme::Rgba::rgb(10, 10, 10));
        // A locked mode ignores what the terminal reports.
        let (_dir, mut app) = self::app();
        app.run("theme_mode_lock");
        app.update(Action::Terminal(colors));
        assert_eq!(app.mode, Mode::Dark);
    }

    #[test]
    fn child_sessions_are_walked_in_id_order() {
        let (_dir, mut app) = app();
        let child = |id: &str| {
            let mut info = session(id, "Explore (@general subagent)");
            info.parent_id = Some("ses_1".into());
            Child { info, tools: Vec::new(), status: Status::Idle, duration: 0 }
        };
        let children = vec![child("ses_c"), child("ses_a"), child("ses_b")];
        open(&mut app, session("ses_1", "Parent"), conversation());
        app.update(Action::ChildrenLoaded { session_id: "ses_1".into(), children: children.clone(), status: Status::Idle });
        assert_eq!(press(&mut app, KeyCode::Right), vec![]);
        app.editor.clear();
        ctrl(&mut app, 'x');
        assert_eq!(press(&mut app, KeyCode::Down), vec![Effect::OpenSession("ses_a".into())]);

        open(&mut app, children[1].info.clone(), Vec::new());
        app.update(Action::ChildrenLoaded { session_id: "ses_a".into(), children, status: Status::Busy });
        assert!(app.session().unwrap().busy() && !app.prompt_focused());
        type_text(&mut app, "ignored");
        assert!(app.editor.is_empty());
        // `moveChild(1)` steps to the previous id and wraps.
        assert_eq!(press(&mut app, KeyCode::Right), vec![Effect::OpenSession("ses_c".into())]);
        assert_eq!(press(&mut app, KeyCode::Left), vec![Effect::OpenSession("ses_b".into())]);
        assert_eq!(press(&mut app, KeyCode::Up), vec![Effect::OpenSession("ses_1".into())]);
    }
}
