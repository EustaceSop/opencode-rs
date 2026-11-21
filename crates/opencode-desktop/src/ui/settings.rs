// The settings screen: a page with a searchable navigation on the left (`settings/navigation.tsx`, `settings/pages.ts`).
// Pages without anything behind them in this build (extensions, projects, workspaces, pairing, server, experimental)
// are not listed.
use super::app::{AppView, Overlay, SettingsTab};
use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::Button;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::input::Input;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, ElementId, FontWeight, InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, TestSupportExt as _, div, px,
};

// The navigation's groups, in the original's order.
const GROUPS: [&[SettingsTab]; 3] = [
    &[SettingsTab::Providers, SettingsTab::Models],
    &[SettingsTab::General, SettingsTab::Appearance, SettingsTab::Notifications, SettingsTab::Shortcuts],
    &[SettingsTab::About],
];

impl SettingsTab {
    pub fn id(self) -> &'static str {
        match self {
            Self::Providers => "providers",
            Self::Models => "models",
            Self::General => "general",
            Self::Appearance => "appearance",
            Self::Notifications => "notifications",
            Self::Shortcuts => "shortcuts",
            Self::About => "about",
        }
    }

    // `pageLabels`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Providers => "settings.providers.title",
            Self::Models => "settings.models.title",
            Self::General => "settings.tab.preferences",
            Self::Appearance => "settings.general.section.appearance",
            Self::Notifications => "settings.tab.notifications",
            Self::Shortcuts => "settings.shortcuts.title",
            Self::About => "settings.tab.about",
        }
    }

    fn description(self) -> Option<&'static str> {
        match self {
            Self::Providers => Some("settings.providers.description"),
            Self::Models => Some("settings.models.description"),
            Self::General => Some("settings.preferences.description"),
            Self::Appearance => Some("settings.appearance.description"),
            Self::Notifications => Some("settings.notifications.description"),
            Self::Shortcuts => Some("settings.shortcuts.description"),
            Self::About => None,
        }
    }

    // `pageIcons`, with the nearest icons this build bundles.
    fn icon(self) -> Icon {
        match self {
            Self::Providers => Icon::new(Lucide::Plug),
            Self::Models => Icon::new(Lucide::Box),
            Self::General => Icon::new(Lucide::SlidersHorizontal),
            Self::Appearance => Icon::new(IconName::Palette),
            Self::Notifications => Icon::new(IconName::Bell),
            Self::Shortcuts => Icon::new(Lucide::Keyboard),
            Self::About => Icon::new(IconName::Info),
        }
    }

    // Row titles a search also finds the page by.
    fn terms(self) -> &'static [&'static str] {
        match self {
            Self::Providers => &["settings.providers.section.connected", "settings.providers.section.popular", "command.provider.connect"],
            Self::Models => &["dialog.model.manage", "dialog.model.search.placeholder"],
            Self::General => &["settings.general.row.language.title", "command.category.model", "command.category.agent", "settings.tab.general"],
            Self::Appearance => &["settings.general.row.colorScheme.title", "settings.general.row.theme.title", "desktop.rs.settings.fontSize"],
            Self::Notifications => &["settings.general.section.notifications", "settings.general.section.sounds"],
            Self::Shortcuts => &["settings.tab.shortcuts"],
            Self::About => &["app.name.desktop"],
        }
    }
}

impl AppView {
    fn settings_matches(&self, tab: SettingsTab, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        query.is_empty() || std::iter::once(tab.label()).chain(tab.terms().iter().copied()).any(|key| self.i18n.t(key).to_lowercase().contains(&query))
    }

    pub(super) fn open_settings_tab(&mut self, tab: SettingsTab, cx: &mut Context<Self>) {
        self.overlay = Some(Overlay::Settings(tab));
        cx.notify();
    }

    fn render_about(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        v_flex()
            .id("settings-about")
            .test_support()
            .gap_2()
            .text_sm()
            .child(div().text_lg().font_weight(FontWeight::MEDIUM).child(self.t("app.name.desktop")))
            .child(SharedString::from(self.i18n.tf("settings.about.version", &[("version", env!("CARGO_PKG_VERSION"))])))
            .child(div().text_color(theme.muted_foreground).child(self.t("settings.about.description")))
            .child(div().text_color(theme.muted_foreground).child(self.t("settings.about.license")))
            .child(div().text_color(theme.muted_foreground).child(self.t("desktop.rs.settings.about.build")))
            .into_any_element()
    }

    fn render_settings_body(&self, tab: SettingsTab, cx: &mut Context<Self>) -> AnyElement {
        match tab {
            SettingsTab::Providers => self.render_providers_page(cx),
            SettingsTab::Models => v_flex()
                .gap_4()
                .child(Input::new(&self.model_filter).focus_ring(false).small().prefix(Icon::new(IconName::Search).small()).cleanable(true))
                .child(self.render_model_groups("models", cx))
                .into_any_element(),
            SettingsTab::General => self.render_general(cx),
            SettingsTab::Appearance => self.render_appearance(cx),
            SettingsTab::Notifications => self.render_notifications(cx),
            SettingsTab::Shortcuts => self.render_shortcuts(cx),
            SettingsTab::About => self.render_about(cx),
        }
    }

    // Covers the sidebar and the conversation; the title bar stays.
    pub(super) fn render_settings_page(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let tab = self.settings_tab()?;
        let page = self.render_settings_body(tab, cx);
        let theme = cx.theme();
        let query = self.settings_search.read(cx).value().to_string();
        let mut found = false;
        let groups: Vec<_> = GROUPS
            .iter()
            .map(|group| {
                let items: Vec<_> = group
                    .iter()
                    .filter(|page| self.settings_matches(**page, &query))
                    .map(|page| {
                        let page = *page;
                        let selected = page == tab;
                        h_flex()
                            .id(ElementId::Name(format!("settings-tab-{}", page.id()).into()))
                            .test_support()
                            .w_full()
                            .h(px(30.))
                            .px_2()
                            .gap_2()
                            .rounded(theme.radius)
                            .cursor_pointer()
                            .text_sm()
                            .when(selected, |this| this.bg(theme.sidebar_accent).text_color(theme.sidebar_accent_foreground))
                            .when(!selected, |this| this.text_color(theme.sidebar_foreground).hover(|this| this.bg(theme.sidebar_accent)))
                            .on_click(cx.listener(move |this, _, _, cx| this.open_settings_tab(page, cx)))
                            .child(page.icon().small())
                            .child(div().min_w_0().truncate().child(self.t(page.label())))
                    })
                    .collect();
                found |= !items.is_empty();
                v_flex().w_full().gap_0p5().children(items)
            })
            .collect();
        let nav = v_flex()
            .id("settings-nav")
            .w(px(240.))
            .h_full()
            .flex_shrink_0()
            .p_3()
            .gap_3()
            .border_r_1()
            .border_color(theme.sidebar_border)
            .bg(theme.sidebar)
            .child(
                h_flex()
                    .id("settings-back")
                    .test_support()
                    .gap_2()
                    .px_2()
                    .h(px(28.))
                    .rounded(theme.radius)
                    .cursor_pointer()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .hover(|this| this.bg(theme.sidebar_accent))
                    .on_click(cx.listener(|this, _, window, cx| this.close_overlay(window, cx)))
                    .child(Icon::new(IconName::ArrowLeft).small())
                    .child(self.t("settings.backToApp")),
            )
            .child(Input::new(&self.settings_search).focus_ring(false).small().prefix(Icon::new(IconName::Search).small()).cleanable(true))
            .when(!found, |this| {
                this.child(div().px_2().text_sm().text_color(theme.muted_foreground).child(SharedString::from(self.i18n.tf("settings.search.empty", &[("query", query.trim())]))))
            })
            .children(groups);
        let header = v_flex()
            .gap_1()
            .pb_4()
            .child(div().text_lg().font_weight(FontWeight::MEDIUM).child(self.t(tab.label())))
            .when_some(tab.description(), |this, key| this.child(div().text_xs().text_color(theme.muted_foreground).child(self.t(key))))
            .when(tab == SettingsTab::Models, |this| {
                this.child(h_flex().pt_2().child(
                    Button::new("settings-models-connect")
                        .small()
                        .outline()
                        .icon(IconName::Plus)
                        .label(self.t("command.provider.connect"))
                        .on_click(cx.listener(|this, _, window, cx| this.open_connect(None, Some(SettingsTab::Models), window, cx))),
                ))
            });
        let body = v_flex().w_full().max_w(px(720.)).child(header).child(page);
        let content = div()
            .id("settings-content")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .px_8()
            .py_6()
            .child(super::motion::enter(ElementId::Name(format!("settings-page-{}", tab.id()).into()), 6.0, body));
        Some(
            h_flex()
                .id("settings")
                .test_support()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .bg(theme.background)
                .occlude()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(nav)
                .child(content)
                .into_any_element(),
        )
    }
}
