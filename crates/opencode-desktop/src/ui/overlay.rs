// Modal panels drawn over the window (dialogs, rename and delete confirmation) and the contents of the Preferences,
// Appearance, Notifications and Shortcuts settings pages.
use super::app::{AppView, Overlay, SettingsTab};
use crate::model::attention;
use crate::model::keymap;
use crate::model::settings::{Scheme, Toggle};
use crate::model::state::Notice;
use crate::model::theme;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::combobox::Combobox;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::input::Input;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Animation, AnimationExt as _, AnyElement, Context, ElementId, FontWeight, InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, TestSupportExt as _, Window, div, ease_out_quint, px,
};

impl AppView {
    pub(super) fn open_overlay(&mut self, overlay: Overlay, window: &mut Window, cx: &mut Context<Self>) {
        match &overlay {
            Overlay::Rename(id) => {
                let title = self.state.sessions.iter().find(|s| s.id.as_ref() == id).map(|s| s.title.to_string()).unwrap_or_default();
                self.rename.update(cx, |input, cx| {
                    input.set_value(title, window, cx);
                    input.focus(window, cx);
                });
            }
            Overlay::ManageModels => self.model_filter.update(cx, |input, cx| {
                input.set_value("", window, cx);
                input.focus(window, cx);
            }),
            Overlay::Connect(connect) => {
                let (provider, back) = (connect.provider.clone(), connect.back);
                return self.open_connect(provider, back, window, cx);
            }
            _ => self.focus.focus(window, cx),
        }
        self.overlay = Some(overlay);
        cx.notify();
    }

    // A dialog opened from a settings page returns to that page.
    pub(super) fn close_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(closed) = self.overlay.take() else { return };
        if let Overlay::Connect(connect) = &closed {
            self.backend.cancel_login();
            if let Some(tab) = connect.back {
                self.overlay = Some(Overlay::Settings(tab));
                self.focus.focus(window, cx);
                cx.notify();
                return;
            }
        }
        if self.preview.take().is_some() {
            self.apply_theme(window, cx);
        }
        self.composer.update(cx, |composer, cx| composer.focus(window, cx));
        cx.notify();
    }

    // The settings page shown, also behind a dialog opened from it.
    pub(super) fn settings_tab(&self) -> Option<SettingsTab> {
        match self.overlay.as_ref()? {
            Overlay::Settings(tab) => Some(*tab),
            Overlay::Connect(connect) => connect.back,
            _ => None,
        }
    }

    pub(super) fn confirm_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Overlay::Rename(id)) = self.overlay.clone() else { return };
        let title = self.rename.read(cx).value().trim().to_string();
        if !title.is_empty() {
            self.backend.rename(id, title);
        }
        self.close_overlay(window, cx);
    }

    fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Overlay::Delete(id)) = self.overlay.clone() else { return };
        let directory = self.state.directory.clone();
        self.persisted.update_project(&directory, |project| {
            project.set_draft(Some(&id), "");
            if project.last_session.as_deref() == Some(id.as_str()) {
                project.last_session = None;
            }
        });
        self.saver.save(&self.persisted);
        self.backend.delete(id);
        self.close_overlay(window, cx);
    }

    // Hovering a theme shows it; leaving the row it came from puts the saved one back.
    fn preview_theme(&mut self, id: &str, hovered: bool, window: &mut Window, cx: &mut Context<Self>) {
        let next = match (hovered, self.preview.as_deref()) {
            (true, current) if current != Some(id) => Some(id.to_string()),
            (false, Some(current)) if current == id => None,
            _ => return,
        };
        self.preview = next;
        self.apply_theme(window, cx);
    }

    pub(super) fn choose_theme(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.persisted.settings.theme = id.to_string();
        self.preview = None;
        self.apply_theme(window, cx);
        self.save_settings(cx);
    }

    pub(super) fn choose_scheme(&mut self, scheme: Scheme, window: &mut Window, cx: &mut Context<Self>) {
        self.persisted.settings.scheme = scheme;
        self.apply_theme(window, cx);
        self.save_settings(cx);
    }

    pub(super) fn step_font(&mut self, delta: f32, window: &mut Window, cx: &mut Context<Self>) {
        self.persisted.settings.step_font_size(delta);
        self.apply_theme(window, cx);
        self.save_settings(cx);
    }

    pub(super) fn set_toggle(&mut self, toggle: Toggle, value: bool, cx: &mut Context<Self>) {
        if toggle == Toggle::AutoAccept {
            if self.persisted.settings.auto_accept != value {
                self.toggle_auto_accept(cx);
            }
            return;
        }
        self.persisted.settings.set(toggle, value);
        self.save_settings(cx);
    }

    fn section(&self, title: &str, cx: &Context<Self>) -> gpui_kit::Div {
        v_flex().w_full().gap_2().child(div().text_xs().font_weight(FontWeight::MEDIUM).text_color(cx.theme().muted_foreground).child(self.t(title)))
    }

    fn setting_row(&self, title: SharedString, description: Option<SharedString>, control: impl IntoElement, cx: &Context<Self>) -> gpui_kit::Div {
        h_flex()
            .w_full()
            .gap_3()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().child(title))
                    .when_some(description, |this, text| this.child(div().text_xs().text_color(cx.theme().muted_foreground).child(text))),
            )
            .child(div().flex_shrink_0().child(control))
    }

    fn toggle_row(&self, toggle: Toggle, cx: &Context<Self>) -> gpui_kit::Div {
        let description = toggle.description().map(|key| self.t(key));
        let control = Switch::new(toggle.id())
            .checked(self.persisted.settings.get(toggle))
            .on_click(cx.listener(move |this, checked: &bool, _, cx| this.set_toggle(toggle, *checked, cx)));
        self.setting_row(self.t(toggle.title()), description, control, cx)
    }

    fn sound_row(&self, toggle: Toggle, cx: &Context<Self>) -> gpui_kit::Div {
        let description = toggle.description().map(|key| self.t(key));
        let picker = self.sounds.iter().find(|(t, _)| *t == toggle).map(|(_, picker)| picker);
        let control =
            div().w(px(200.)).children(picker.map(|picker| Combobox::new(picker).small().w_full().search_placeholder(self.t("common.search.placeholder"))));
        self.setting_row(self.t(toggle.title()), description, control, cx)
    }

    // Picking a sound turns it on and plays it once; "None" turns it off (`settings-general.tsx:239-249`).
    pub(super) fn choose_sound(&mut self, toggle: Toggle, value: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(value) = value else {
            // Clicking the chosen entry again cleared the selection; keep showing the setting.
            let selected = super::app::sound_selection(&self.persisted.settings, toggle);
            if let Some((_, picker)) = self.sounds.iter().find(|(t, _)| *t == toggle) {
                picker.update(cx, |picker, cx| picker.set_selected_values(&[selected], window, cx));
            }
            return;
        };
        let sound = attention::sound(&value);
        self.persisted.settings.choose_sound(toggle, sound.map(|s| s.id));
        self.alerts.preview(sound);
        self.save_settings(cx);
    }

    fn default_row(&self, title: &str, saved: Option<&str>, current: Option<&str>, model: bool, cx: &Context<Self>) -> gpui_kit::Div {
        let (set_id, clear_id) = if model { ("default-model-set", "default-model-clear") } else { ("default-agent-set", "default-agent-clear") };
        let current = current.map(str::to_string);
        let control = h_flex()
            .gap_1()
            .child(Button::new(set_id).small().outline().label(self.t("desktop.rs.settings.useCurrent")).on_click(cx.listener(move |this, _, _, cx| {
                let settings = &mut this.persisted.settings;
                if model { settings.default_model = current.clone() } else { settings.default_agent = current.clone() }
                this.save_settings(cx);
            })))
            .child(Button::new(clear_id).small().ghost().label(self.t("common.clear")).on_click(cx.listener(move |this, _, _, cx| {
                let settings = &mut this.persisted.settings;
                if model { settings.default_model = None } else { settings.default_agent = None }
                this.save_settings(cx);
            })));
        let shown = saved.map_or_else(|| self.t("common.default"), |value| SharedString::from(value.to_string()));
        self.setting_row(self.t(title), Some(shown), control, cx)
    }

    // `settings/appearance/appearance.tsx`: scheme, theme and the text size.
    pub(super) fn render_appearance(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let settings = &self.persisted.settings;
        let schemes = h_flex().gap_1().children(Scheme::ALL.map(|scheme| {
            let id = ElementId::Name(format!("scheme-{}", scheme.id()).into());
            let button = Button::new(id).small().label(self.t(scheme.key())).on_click(cx.listener(move |this, _, window, cx| this.choose_scheme(scheme, window, cx)));
            if settings.scheme == scheme { button.primary() } else { button.outline() }
        }));
        let saved_theme = settings.theme().id.as_str();
        let themes = theme::themes().iter().map(|palette| {
            let (hover_id, click_id) = (palette.id.clone(), palette.id.clone());
            let selected = palette.id == saved_theme;
            h_flex()
                .id(ElementId::Name(format!("theme-{}", palette.id).into()))
                .test_support()
                .w_full()
                .h(px(26.))
                .px_2()
                .gap_2()
                .rounded(theme.radius)
                .cursor_pointer()
                .text_sm()
                .when(selected, |this| this.bg(theme.accent))
                .hover(|this| this.bg(theme.accent))
                .on_hover(cx.listener(move |this, hovered: &bool, window, cx| this.preview_theme(&hover_id, *hovered, window, cx)))
                .on_click(cx.listener(move |this, _, window, cx| this.choose_theme(&click_id, window, cx)))
                .child(div().flex_1().min_w_0().truncate().child(SharedString::from(palette.name.clone())))
                .when(selected, |this| this.child(Icon::new(IconName::Check).small()))
        });
        let font = h_flex()
            .gap_1()
            .child(Button::new("font-smaller").small().outline().icon(IconName::Minus).on_click(cx.listener(|this, _, window, cx| this.step_font(-1.0, window, cx))))
            .child(div().w_12().text_sm().text_center().child(format!("{} px", settings.font_size())))
            .child(Button::new("font-larger").small().outline().icon(IconName::Plus).on_click(cx.listener(|this, _, window, cx| this.step_font(1.0, window, cx))));
        v_flex()
            .id("settings-appearance").test_support()
            .w_full()
            .gap_5()
            .child(
                self.section("settings.general.section.appearance", cx)
                    .child(self.setting_row(
                        self.t("settings.general.row.colorScheme.title"),
                        Some(self.t("settings.general.row.colorScheme.description")),
                        schemes,
                        cx,
                    ))
                    .child(self.setting_row(self.t("desktop.rs.settings.fontSize"), None, font, cx))
                    .child(self.setting_row(self.t("settings.general.row.theme.title"), Some(self.t("settings.general.row.theme.description")), div(), cx))
                    .child(
                        v_flex()
                            .id("theme-list")
                            .w_full()
                            .h(px(260.))
                            .p_1()
                            .rounded(theme.radius)
                            .border_1()
                            .border_color(theme.border)
                            .overflow_y_scroll()
                            .children(themes),
                    ),
            )
            .into_any_element()
    }

    // `settings/general/general.tsx` ("Preferences"): language, behaviour and defaults.
    pub(super) fn render_general(&self, cx: &Context<Self>) -> AnyElement {
        let settings = &self.persisted.settings;
        let language = div().w(px(200.)).child(Combobox::new(&self.languages).small().w_full().search_placeholder(self.t("common.search.placeholder")));
        v_flex()
            .id("settings-general").test_support()
            .w_full()
            .gap_5()
            .child(
                self.section("settings.tab.general", cx)
                    .child(self.setting_row(self.t("settings.general.row.language.title"), Some(self.t("settings.general.row.language.description")), language, cx))
                    .children(Toggle::GENERAL.map(|toggle| self.toggle_row(toggle, cx)))
                    .child(self.default_row("command.category.model", settings.default_model.as_deref(), self.state.effective_model(), true, cx))
                    .child(self.default_row("command.category.agent", settings.default_agent.as_deref(), self.state.effective_agent(), false, cx))
                    .when(cfg!(windows), |this| {
                        let download = Button::new("setup-bash-download")
                            .small()
                            .outline()
                            .label(self.t("desktop.rs.setup.bash.download"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.backend.setup_bash(true);
                                this.state.notice = Some(Notice { key: "desktop.rs.setup.bash.started", error: String::new() });
                                cx.notify();
                            }));
                        this.child(self.setting_row(self.t("desktop.rs.setup.bash.title"), Some(SharedString::from(opencode_tools::mingit::OFFER)), download, cx))
                    }),
            )
            .into_any_element()
    }

    // `settings/notifications/notifications.tsx`: system notifications and sounds.
    pub(super) fn render_notifications(&self, cx: &Context<Self>) -> AnyElement {
        v_flex()
            .id("settings-notifications").test_support()
            .w_full()
            .gap_5()
            .child(self.section("settings.general.section.notifications", cx).children(Toggle::NOTIFICATIONS.map(|toggle| self.toggle_row(toggle, cx))))
            .child(self.section("settings.general.section.sounds", cx).children(Toggle::SOUNDS.map(|toggle| self.sound_row(toggle, cx))))
            .into_any_element()
    }

    pub(super) fn render_shortcuts(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let mac = cfg!(target_os = "macos");
        let key = |combo: &str| {
            div()
                .flex_shrink_0()
                .px_1p5()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.border)
                .bg(theme.muted)
                .font_family(theme.mono_font_family.clone())
                .text_xs()
                .child(keymap::display(combo, mac))
        };
        let row = |title: SharedString, combo: &str| h_flex().w_full().gap_3().text_sm().child(div().flex_1().min_w_0().truncate().child(title)).child(key(combo));
        let groups = keymap::GROUPS.map(|group| {
            let binds = keymap::KEYBINDS.iter().filter(|bind| bind.group == group).map(|bind| row(self.t(bind.title), bind.combo));
            let prompt_keys = keymap::PROMPT_KEYS.iter().filter(|_| group == "settings.shortcuts.group.prompt").map(|(combo, title)| row(self.t(title), combo));
            self.section(group, cx).children(binds).children(prompt_keys)
        });
        v_flex()
            .id("settings-shortcuts")
            .test_support()
            .w_full()
            .gap_5()
            .child(div().text_xs().text_color(theme.muted_foreground).child(self.t("desktop.rs.settings.shortcutsReadOnly")))
            .children(groups)
            .into_any_element()
    }

    // Rust build: the portable-bash answer. Accept downloads in the background, Never records the refusal.
    pub(super) fn answer_bash(&mut self, accept: Option<bool>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(accept) = accept {
            self.backend.setup_bash(accept);
            if accept {
                self.state.notice = Some(Notice { key: "desktop.rs.setup.bash.started", error: String::new() });
            }
        }
        self.close_overlay(window, cx);
    }

    fn render_setup_bash(&self, cx: &Context<Self>) -> AnyElement {
        let button = |id: &'static str, key: &str, accept: Option<bool>| {
            Button::new(id).small().label(self.t(key)).on_click(cx.listener(move |this, _, window, cx| this.answer_bash(accept, window, cx)))
        };
        v_flex()
            .id("setup-bash")
            .test_support()
            .w(px(440.))
            .gap_3()
            .p_4()
            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(self.t("desktop.rs.setup.bash.title")))
            .child(div().text_sm().child(SharedString::from(opencode_tools::mingit::OFFER)))
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(button("setup-bash-never", "desktop.rs.setup.bash.never", Some(false)).ghost())
                    .child(button("setup-bash-later", "desktop.rs.setup.bash.later", None).ghost())
                    .child(button("setup-bash-accept", "desktop.rs.setup.bash.accept", Some(true)).primary()),
            )
            .into_any_element()
    }

    fn render_dialog(&self, title: SharedString, body: AnyElement, confirm: Button, cx: &Context<Self>) -> AnyElement {
        v_flex()
            .w(px(420.))
            .gap_3()
            .p_4()
            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(title))
            .child(body)
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("overlay-cancel")
                            .small()
                            .ghost()
                            .label(self.t("common.cancel"))
                            .on_click(cx.listener(|this, _, window, cx| this.close_overlay(window, cx))),
                    )
                    .child(confirm),
            )
            .into_any_element()
    }

    pub(super) fn render_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let panel = match self.overlay.as_ref()? {
            // The settings screen is a page of its own (`render_settings_page`).
            Overlay::Settings(_) => return None,
            Overlay::ManageModels => self.render_manage_models(cx),
            Overlay::Connect(connect) => self.render_connect(connect, cx),
            Overlay::SetupBash => self.render_setup_bash(cx),
            Overlay::Rename(_) => self.render_dialog(
                self.t("common.rename"),
                Input::new(&self.rename).focus_ring(false).into_any_element(),
                Button::new("rename-save")
                    .small()
                    .primary()
                    .label(self.t("common.save"))
                    .on_click(cx.listener(|this, _, window, cx| this.confirm_rename(window, cx))),
                cx,
            ),
            Overlay::Delete(id) => {
                let name = self.state.sessions.iter().find(|s| s.id.as_ref() == id).map(|s| s.title.to_string()).unwrap_or_default();
                self.render_dialog(
                    self.t("session.delete.title"),
                    div().text_sm().child(SharedString::from(self.i18n.tf("session.delete.confirm", &[("name", &name)]))).into_any_element(),
                    Button::new("delete-confirm")
                        .small()
                        .danger()
                        .label(self.t("session.delete.button"))
                        .on_click(cx.listener(|this, _, window, cx| this.confirm_delete(window, cx))),
                    cx,
                )
            }
        };
        let theme = cx.theme();
        Some(
            div()
                .id("overlay")
                .test_support()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme.overlay)
                .occlude()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    // Rust build: the panel rises into place as the backdrop fades in.
                    div()
                        .relative()
                        .max_h_full()
                        .rounded(theme.radius_lg)
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.popover)
                        .text_color(theme.popover_foreground)
                        .shadow_lg()
                        .overflow_hidden()
                        .child(panel)
                        .with_animation("overlay-panel", Animation::new(std::time::Duration::from_millis(220)).with_easing(ease_out_quint()), |this, t| {
                            this.opacity(t).top(px((1.0 - t) * 10.0))
                        }),
                )
                .with_animation("overlay-backdrop", Animation::new(std::time::Duration::from_millis(180)), |this, t| this.opacity(t))
                .into_any_element(),
        )
    }
}
