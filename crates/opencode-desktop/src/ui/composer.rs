// The prompt input: slash/mention popup, attachments, history, drafts, sending.
use super::app::{AppView, now_ms};
use crate::model::backend::{PromptRequest, Work};
use crate::model::keymap::Command;
use crate::model::prompt::{self, Builtin, Trigger};
use crate::model::transcript::RowKind;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::combobox::Combobox;
use gpui_kit::component::input::{self, Textarea};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, ClipboardEntry, ClipboardItem, Context, ElementId, InteractiveElement as _, IntoElement, ParentElement as _,
    PathPromptOptions, SharedString, StatefulInteractiveElement as _, Styled as _, TestSupportExt as _, Window, div, px,
};
use std::path::PathBuf;
use std::sync::Arc;

const POPUP_LIMIT: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupKind {
    Slash,
    // Byte offset of the `@` being completed.
    Mention { start: usize },
}

#[derive(Debug, Clone, PartialEq)]
pub struct PopupItem {
    // Command name or project-relative path.
    pub value: Arc<str>,
    pub detail: SharedString,
    pub badge: Option<SharedString>,
    pub builtin: Option<Builtin>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Popup {
    pub kind: PopupKind,
    pub items: Vec<PopupItem>,
    pub selected: usize,
}

impl AppView {
    fn composer_text(&self, cx: &App) -> String {
        self.composer.read(cx).value().to_string()
    }

    // Programmatic edits put the cursor at the end and never open the popup.
    pub(super) fn set_composer(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let end = text.len();
        self.composer.update(cx, |composer, cx| {
            composer.set_value(text.to_string(), window, cx);
            composer.set_selected_range(end..end, cx);
        });
        self.popup = None;
        cx.notify();
    }

    pub(super) fn save_draft(&mut self, cx: &mut Context<Self>) {
        let directory = self.state.directory.clone();
        if directory.as_os_str().is_empty() {
            return;
        }
        let text = self.composer_text(cx);
        let session = self.state.current.clone();
        if self.persisted.update_project(&directory, |project| project.set_draft(session.as_deref(), &text)) {
            self.saver.save(&self.persisted);
        }
    }

    pub(super) fn load_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.persisted.project(&self.state.directory).draft(self.state.current.as_deref()).to_string();
        self.history.reset();
        self.set_composer(&draft, window, cx);
    }

    fn build_popup(&self, text: &str, cursor: usize) -> Option<Popup> {
        match prompt::trigger(text, cursor)? {
            Trigger::Slash { query } => {
                // An alias from another agent that the query names is shown beside the command it reaches.
                let typed = query.trim().to_lowercase();
                let builtins = Builtin::ALL.into_iter().map(|b| {
                    let named = (!typed.is_empty() && !b.name().starts_with(&typed)).then(|| b.aliases().iter().find(|a| a.starts_with(&typed))).flatten();
                    let detail = match named {
                        Some(alias) => SharedString::from(format!("{} · /{alias}", self.t(b.title()))),
                        None => self.t(b.title()),
                    };
                    PopupItem { value: b.name().into(), detail, badge: None, builtin: Some(b) }
                });
                let custom = self.state.commands.iter().filter(|c| Builtin::find(&c.name).is_none()).map(|c| PopupItem {
                    value: c.name.clone(),
                    detail: SharedString::from(c.description.clone()),
                    badge: Some(self.t(c.badge)),
                    builtin: None,
                });
                let all: Vec<PopupItem> = builtins.chain(custom).collect();
                let mut ranked = prompt::rank(&query, all.iter().map(|item| item.value.as_ref()), POPUP_LIMIT);
                // Aliases match by prefix only, after the commands' own names.
                for (index, item) in all.iter().enumerate() {
                    let aliased = !typed.is_empty() && item.builtin.is_some_and(|b| b.aliases().iter().any(|a| a.starts_with(&typed)));
                    if aliased && !ranked.contains(&index) && ranked.len() < POPUP_LIMIT {
                        ranked.push(index);
                    }
                }
                Some(Popup { kind: PopupKind::Slash, items: ranked.into_iter().map(|index| all[index].clone()).collect(), selected: 0 })
            }
            Trigger::Mention { start, query } => {
                let files = self.state.files.clone();
                let items = prompt::rank_paths(&query, &files, POPUP_LIMIT)
                    .into_iter()
                    .map(|index| {
                        let path = &files[index];
                        let folder = path.rsplit_once('/').map_or("", |(folder, _)| folder);
                        PopupItem { value: path.as_str().into(), detail: SharedString::from(folder.to_string()), badge: None, builtin: None }
                    })
                    .collect();
                Some(Popup { kind: PopupKind::Mention { start }, items, selected: 0 })
            }
        }
    }

    pub(super) fn composer_changed(&mut self, cx: &mut Context<Self>) {
        let (text, cursor) = {
            let composer = self.composer.read(cx);
            (composer.value().to_string(), composer.cursor())
        };
        self.history.reset();
        self.popup = if self.state.current_parent().is_some() { None } else { self.build_popup(&text, cursor) };
        cx.notify();
    }

    // Up/down: the popup first, then prompt history while the prompt is empty or already showing an entry.
    fn arrow(&mut self, up: bool, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if let Some(popup) = &mut self.popup {
            let count = popup.items.len();
            if count > 0 {
                popup.selected = if up { (popup.selected + count - 1) % count } else { (popup.selected + 1) % count };
                cx.notify();
            }
            return true;
        }
        let text = self.composer_text(cx);
        if !(text.is_empty() || self.history.browsing()) {
            return false;
        }
        let entry = if up { self.history.up(&text) } else { self.history.down() };
        match entry {
            Some(entry) => {
                self.set_composer(&entry, window, cx);
                true
            }
            None => self.history.browsing(),
        }
    }

    pub(super) fn accept_popup(&mut self, index: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(popup) = self.popup.take() else { return };
        let Some(item) = popup.items.get(index.unwrap_or(popup.selected)).cloned() else {
            cx.notify();
            return;
        };
        match (popup.kind, item.builtin) {
            (PopupKind::Slash, Some(builtin)) => {
                self.set_composer("", window, cx);
                self.run_builtin(builtin, window, cx);
            }
            (PopupKind::Slash, None) => self.set_composer(&format!("/{} ", item.value), window, cx),
            (PopupKind::Mention { start }, _) => {
                let (text, cursor) = {
                    let composer = self.composer.read(cx);
                    (composer.value().to_string(), composer.cursor().min(composer.value().len()))
                };
                let mention = format!("@{} ", item.value);
                let rest = text.get(cursor..).unwrap_or("");
                let replaced = format!("{}{mention}{rest}", text.get(..start).unwrap_or(""));
                let caret = start + mention.len();
                self.composer.update(cx, |composer, cx| {
                    composer.set_value(replaced, window, cx);
                    composer.set_selected_range(caret..caret, cx);
                });
                let path = self.state.directory.join(item.value.as_ref());
                prompt::attach(&mut self.attachments, [prompt::file_attachment(&path)]);
            }
        }
        cx.notify();
    }

    pub(super) fn attach_paths(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        let files = paths.iter().filter(|path| path.is_file()).map(|path| prompt::file_attachment(path));
        prompt::attach(&mut self.attachments, files);
        cx.notify();
    }

    // Images and copied files become attachments; anything else is pasted as text.
    fn paste(&mut self, item: &ClipboardItem, cx: &mut Context<Self>) -> bool {
        let before = self.attachments.len();
        for entry in item.entries() {
            match entry {
                ClipboardEntry::Image(image) => {
                    let mime = image.format.mime_type();
                    let name = format!("pasted-{}.{}", self.attachments.len() + 1, mime.rsplit('/').next().unwrap_or("png"));
                    prompt::attach(&mut self.attachments, [prompt::image_attachment(&name, mime, &image.bytes)]);
                }
                ClipboardEntry::ExternalPaths(paths) => self.attach_paths(paths.paths(), cx),
                ClipboardEntry::String(_) => {}
            }
        }
        cx.notify();
        self.attachments.len() > before
    }

    pub(super) fn choose_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = Some(self.t("prompt.action.attachFile"));
        let picked = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: true, prompt });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = picked.await {
                let _ = this.update(cx, |this, cx| this.attach_paths(&paths, cx));
            }
        })
        .detach();
    }

    fn can_send(&self) -> bool {
        !self.state.loading && !self.state.current_running() && self.state.current_parent().is_none()
    }

    pub(super) fn run_builtin(&mut self, builtin: Builtin, window: &mut Window, cx: &mut Context<Self>) {
        let session = self.state.current.clone().filter(|_| self.can_send());
        match (builtin, session) {
            (Builtin::New, _) => self.new_session(window, cx),
            (Builtin::Model, _) => self.run(Command::ChooseModel, window, cx),
            (Builtin::Agent, _) => self.cycle_agent(1, window, cx),
            (Builtin::Undo, Some(_)) => {
                if let Some(message) = self.state.last_user_message() {
                    self.revert_to(&message, window, cx);
                }
            }
            (Builtin::Redo, Some(session)) => self.backend.unrevert(session),
            (Builtin::Fork, Some(session)) => self.backend.fork(self.state.directory.clone(), session, None),
            (Builtin::Compact, Some(_)) => self.start(Work::Compact, String::new(), "/compact".to_string(), window, cx),
            // The rest need an idle, existing session.
            (_, None) => {}
        }
    }

    // Hides the message and what followed it, and offers its text for editing again.
    pub(super) fn revert_to(&mut self, message_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.state.current.clone().filter(|_| self.can_send()) else { return };
        let text = self.rows.iter().find(|row| row.id.as_ref() == message_id).and_then(|row| match &row.kind {
            RowKind::User { text, .. } => Some(text.to_string()),
            _ => None,
        });
        if let Some(text) = text.filter(|_| self.composer_text(cx).trim().is_empty()) {
            self.set_composer(&text, window, cx);
        }
        self.backend.revert(session, message_id.to_string());
    }

    pub(super) fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.composer_text(cx).trim().to_string();
        // Rust build: while the session runs, a prompt or command steers the run (opencode 2.x); builtins still wait.
        if self.state.loading || self.state.current_parent().is_some() {
            return;
        }
        if let Some((name, arguments)) = prompt::parse_slash(&typed) {
            if let Some(builtin) = Builtin::find(name) {
                self.history.push(&typed);
                self.set_composer("", window, cx);
                return self.run_builtin(builtin, window, cx);
            }
            if self.state.commands.iter().any(|c| c.name.as_ref() == name) {
                return self.start(Work::Command(name.to_string()), arguments.to_string(), typed.clone(), window, cx);
            }
        }
        if typed.is_empty() && self.attachments.is_empty() {
            return;
        }
        self.start(Work::Prompt, typed.clone(), typed, window, cx);
    }

    // `shown` is what the transcript and the history record; `text` is what the engine receives.
    fn start(&mut self, work: Work, text: String, shown: String, window: &mut Window, cx: &mut Context<Self>) {
        let (session_id, create) = match &self.state.current {
            Some(id) => (id.clone(), false),
            None => (opencode_storage::id::session(), true),
        };
        let files = std::mem::take(&mut self.attachments);
        self.state.begin_prompt(&session_id, &shown, files.iter().map(|f| f.name.clone()).collect(), now_ms());
        self.backend.prompt(PromptRequest {
            directory: self.state.directory.clone(),
            session_id,
            create,
            work,
            text,
            files,
            model: self.state.effective_model().map(str::to_string),
            agent: self.state.effective_agent().map(str::to_string),
            variant: self.current_variant(),
        });
        self.history.push(&shown);
        self.persisted.history = self.history.entries().to_vec();
        self.set_composer("", window, cx);
        // The draft that was just sent, under the new-session slot or the session's own.
        let directory = self.state.directory.clone();
        let current = self.state.current.clone();
        self.persisted.update_project(&directory, |project| {
            project.set_draft(None, "");
            project.set_draft(current.as_deref(), "");
        });
        self.saver.save(&self.persisted);
        self.remember();
        self.sync(cx);
        self.scroller.update(cx, |scroller, cx| scroller.scroll_to_end(cx));
    }

    pub(super) fn render_popup(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let popup = self.popup.as_ref()?;
        let theme = cx.theme();
        let empty = match popup.kind {
            PopupKind::Slash => "prompt.popover.emptyCommands",
            PopupKind::Mention { .. } => "prompt.popover.emptyResults",
        };
        let prefix = if popup.kind == PopupKind::Slash { "/" } else { "" };
        let rows = popup.items.iter().enumerate().map(|(index, item)| {
            let selected = index == popup.selected;
            h_flex()
                .id(ElementId::Name(format!("popup-{index}").into()))
                .test_support()
                .w_full()
                .h(px(28.))
                .px_2()
                .gap_2()
                .rounded(theme.radius)
                .cursor_pointer()
                .text_sm()
                .when(selected, |this| this.bg(theme.accent).text_color(theme.accent_foreground))
                .when(!selected, |this| this.hover(|this| this.bg(theme.accent.opacity(0.5))))
                .on_click(cx.listener(move |this, _, window, cx| this.accept_popup(Some(index), window, cx)))
                .child(div().flex_shrink_0().child(SharedString::from(format!("{prefix}{}", item.value.rsplit('/').next().unwrap_or(&item.value)))))
                .child(div().flex_1().min_w_0().truncate().text_xs().text_color(theme.muted_foreground).child(item.detail.clone()))
                .when_some(item.badge.clone(), |this, badge| this.child(Tag::secondary().small().child(badge)))
        });
        Some(
            v_flex()
                .id("prompt-popup")
                .test_support()
                .w_full()
                .p_1()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.border)
                .bg(theme.popover)
                .text_color(theme.popover_foreground)
                .when(popup.items.is_empty(), |this| {
                    this.child(div().px_2().py_1().text_sm().text_color(theme.muted_foreground).child(self.t(empty)))
                })
                .children(rows)
                .into_any_element(),
        )
    }

    fn render_attachments(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.attachments.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let chips = self.attachments.iter().enumerate().map(|(index, attachment)| {
            h_flex()
                .gap_1()
                .pl_2()
                .rounded(theme.radius)
                .bg(theme.muted)
                .text_xs()
                .child(SharedString::from(attachment.name.clone()))
                .child(
                    Button::new(ElementId::Name(format!("attachment-{index}-remove").into()))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .tooltip(self.t("common.delete"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if index < this.attachments.len() {
                                this.attachments.remove(index);
                            }
                            cx.notify();
                        })),
                )
        });
        Some(h_flex().id("attachments").test_support().px_2().pt_2().gap_1().flex_wrap().children(chips).into_any_element())
    }

    pub(super) fn render_composer(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let frame = v_flex().w_full().rounded(theme.radius_lg).border_1().border_color(theme.input).bg(theme.background);
        if self.state.current_parent().is_some() {
            return frame
                .p_3()
                .gap_2()
                .items_center()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(self.t("session.child.promptDisabled"))
                .child(Button::new("back-to-parent").small().outline().label(self.t("session.child.backToParent")).on_click(cx.listener(
                    |this, _, window, cx| {
                        if let Some(parent) = this.state.current_parent() {
                            this.open_session(&parent, window, cx);
                        }
                    },
                )))
                .into_any_element();
        }
        let running = self.state.current_running();
        let empty = self.composer_text(cx).trim().is_empty() && self.attachments.is_empty();
        // Rust build: with something typed while the session runs, a send button steers the run beside the stop button.
        let steer = (running && !empty).then(|| {
            Button::new("steer")
                .small()
                .primary()
                .icon(IconName::ArrowUp)
                .tooltip(self.t("desktop.rs.prompt.steer"))
                .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))
        });
        let action = if running {
            Button::new("stop")
                .small()
                .danger()
                .icon(IconName::Square)
                .tooltip(self.t("prompt.action.stop"))
                .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
        } else {
            Button::new("send")
                .small()
                .primary()
                .icon(IconName::ArrowUp)
                .tooltip(self.t("prompt.action.send"))
                .disabled(empty || self.state.loading)
                .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))
        };
        let view = cx.weak_entity();
        let textarea = Textarea::new(&self.composer)
            .appearance(false)
            .on_paste(move |item, _, cx| view.update(cx, |this, cx| this.paste(item, cx)).unwrap_or(false));
        let input = div()
            .px_1()
            .pt_1()
            .capture_action(cx.listener(|this, _: &input::MoveUp, window, cx| {
                if this.arrow(true, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &input::MoveDown, window, cx| {
                if this.arrow(false, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, enter: &input::Enter, window, cx| {
                if !enter.shift && this.popup.is_some() {
                    this.accept_popup(None, window, cx);
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &input::IndentInline, window, cx| {
                if this.popup.is_some() {
                    this.accept_popup(None, window, cx);
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &input::Escape, _, cx| {
                if this.popup.take().is_some() {
                    cx.notify();
                    cx.stop_propagation();
                } else if this.state.current_running() {
                    this.stop(cx);
                    cx.stop_propagation();
                }
            }))
            .child(textarea);
        let (model, variant) = (self.render_model_picker(cx), self.render_variant_picker());
        frame
            .children(self.render_attachments(cx))
            .child(input)
            .child(
                h_flex()
                    .p_2()
                    .gap_2()
                    .child(
                        Button::new("attach")
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .tooltip(self.t("prompt.action.attachFile"))
                            .on_click(cx.listener(|this, _, window, cx| this.choose_files(window, cx))),
                    )
                    .child(div().id("agent-picker").test_support().w(px(132.)).flex_shrink_0().child(
                        Combobox::new(&self.agents).small().w_full().placeholder(self.t("command.category.agent")).search_placeholder(self.t("common.search.placeholder")),
                    ))
                    .child(model)
                    .children(variant)
                    .child(div().flex_1())
                    .when(self.persisted.settings.auto_accept, |this| {
                        this.child(Tag::warning().small().child(self.t("toast.permissions.autoaccept.on.title")))
                    })
                    .children(steer)
                    .child(action),
            )
            .into_any_element()
    }
}
