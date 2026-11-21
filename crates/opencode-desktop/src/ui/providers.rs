// Connecting providers (`dialog-connect-provider.tsx`) and the Providers settings page (`settings-providers.tsx`).
// Credentials go where `opencode providers login` puts them: auth.json, through the CLI's own flows.
use super::app::{AppView, Connect, ConnectStep, Overlay, SettingsTab};
use crate::model::backend::LoginFlow;
use crate::model::models::{self, POPULAR};
use crate::model::state::{LoginStep, Notice, ProviderSource};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::input::Input;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, ClipboardItem, Context, ElementId, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, TestSupportExt as _, Window, div, px,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Method {
    Flow(LoginFlow),
    ApiKey,
}

// The CLI's methods: its login flows, then an API key unless the provider signs in only through its flow.
pub(super) fn methods(provider_id: &str) -> Vec<Method> {
    let flows = LoginFlow::of(provider_id).iter().map(|flow| Method::Flow(*flow));
    let key = (provider_id != opencode_llm::copilot::PROVIDER_ID).then_some(Method::ApiKey);
    flows.chain(key).collect()
}

// `dialog-connect-provider.tsx` `note` and taglines.
fn note(id: &str) -> Option<&'static str> {
    match id {
        "opencode" => Some("dialog.provider.opencode.tagline"),
        "opencode-go" => Some("dialog.provider.opencodeGo.tagline"),
        "anthropic" => Some("dialog.provider.anthropic.note"),
        "openai" => Some("dialog.provider.openai.note"),
        _ if id.starts_with("github-copilot") => Some("dialog.provider.copilot.note"),
        _ => None,
    }
}

// `settings-providers.tsx` `PROVIDER_NOTES`.
fn settings_note(id: &str) -> Option<&'static str> {
    match id {
        "opencode" => Some("dialog.provider.opencode.note"),
        "opencode-go" => Some("dialog.provider.opencodeGo.tagline"),
        "anthropic" => Some("dialog.provider.anthropic.note"),
        "openai" => Some("dialog.provider.openai.note"),
        "google" => Some("dialog.provider.google.note"),
        "openrouter" => Some("dialog.provider.openrouter.note"),
        "vercel" => Some("dialog.provider.vercel.note"),
        _ if id.starts_with("github-copilot") => Some("dialog.provider.copilot.note"),
        _ => None,
    }
}

fn recommended(id: &str) -> bool {
    id == "opencode" || id == "opencode-go"
}

fn element_id(prefix: &str, value: impl std::fmt::Display) -> ElementId {
    ElementId::Name(format!("{prefix}-{value}").into())
}

// A provider's initial in a small square; brand icons are not bundled.
fn badge(name: &str, cx: &Context<AppView>) -> impl IntoElement {
    let theme = cx.theme();
    let initial: String = name.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
    div()
        .flex()
        .items_center()
        .justify_center()
        .size(px(20.))
        .flex_shrink_0()
        .rounded(theme.radius)
        .bg(theme.muted)
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .child(initial)
}

impl AppView {
    fn provider_label(&self, id: &str) -> String {
        self.state.catalog.provider_name(id).map_or_else(|| id.to_string(), |name| name.to_string())
    }

    pub(super) fn open_connect(&mut self, provider: Option<String>, back: Option<SettingsTab>, window: &mut Window, cx: &mut Context<Self>) {
        self.backend.cancel_login();
        self.connect_search.update(cx, |input, cx| input.set_value("", window, cx));
        self.overlay = Some(Overlay::Connect(Connect { provider: None, step: ConnectStep::Choose, back }));
        match provider {
            Some(id) => self.select_provider(&id, window, cx),
            None => self.connect_search.update(cx, |input, cx| input.focus(window, cx)),
        }
        cx.notify();
    }

    fn connect_mut(&mut self) -> Option<&mut Connect> {
        match self.overlay.as_mut() {
            Some(Overlay::Connect(connect)) => Some(connect),
            _ => None,
        }
    }

    // A provider with one method starts it at once (`methods().length === 1`).
    pub(super) fn select_provider(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(connect) = self.connect_mut() else { return };
        connect.provider = Some(id.to_string());
        connect.step = ConnectStep::Choose;
        if let [only] = methods(id).as_slice() {
            self.select_method(*only, window, cx);
        }
        cx.notify();
    }

    pub(super) fn select_method(&mut self, method: Method, window: &mut Window, cx: &mut Context<Self>) {
        let directory = self.state.directory.clone();
        let Some(connect) = self.connect_mut() else { return };
        let Some(provider) = connect.provider.clone() else { return };
        match method {
            Method::ApiKey => {
                connect.step = ConnectStep::ApiKey { missing: false };
                self.api_key.update(cx, |input, cx| {
                    input.set_value("", window, cx);
                    input.focus(window, cx);
                });
            }
            Method::Flow(flow) => {
                connect.step = ConnectStep::Pending;
                self.backend.login(directory, provider, flow);
            }
        }
        cx.notify();
    }

    pub(super) fn submit_key(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let key = self.api_key.read(cx).value().trim().to_string();
        let directory = self.state.directory.clone();
        let Some(connect) = self.connect_mut() else { return };
        let (Some(provider), ConnectStep::ApiKey { .. }) = (connect.provider.clone(), &connect.step) else { return };
        if key.is_empty() {
            connect.step = ConnectStep::ApiKey { missing: true };
        } else {
            connect.step = ConnectStep::Pending;
            self.backend.connect_key(directory, provider, key);
        }
        cx.notify();
    }

    // `goBack`: from a method to the provider's methods when it has several, otherwise to the provider list.
    fn connect_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.backend.cancel_login();
        let Some(connect) = self.connect_mut() else { return };
        let several = connect.provider.as_deref().is_some_and(|id| methods(id).len() > 1);
        if several && connect.step != ConnectStep::Choose {
            connect.step = ConnectStep::Choose;
        } else {
            connect.provider = None;
            connect.step = ConnectStep::Choose;
            self.connect_search.update(cx, |input, cx| input.focus(window, cx));
        }
        cx.notify();
    }

    pub(super) fn login_progress(&mut self, provider_id: &str, step: LoginStep, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.provider_label(provider_id);
        let Some(connect) = self.connect_mut().filter(|c| c.provider.as_deref() == Some(provider_id)) else { return };
        match step {
            LoginStep::Waiting { url, code } => connect.step = ConnectStep::Waiting { url, code },
            LoginStep::Failed(error) => connect.step = ConnectStep::Failed(error),
            LoginStep::Done => {
                self.close_overlay(window, cx);
                self.state.notice = Some(Notice { key: "provider.connect.toast.connected.title", error: name });
            }
        }
        cx.notify();
    }

    pub(super) fn disconnect_provider(&mut self, id: &str, cx: &mut Context<Self>) {
        let name = self.provider_label(id);
        self.backend.disconnect(self.state.directory.clone(), id.to_string());
        self.state.notice = Some(Notice { key: "provider.disconnect.toast.disconnected.title", error: name });
        cx.notify();
    }

    fn render_provider_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let query = self.connect_search.read(cx).value().trim().to_lowercase();
        let all: Vec<_> = self
            .state
            .catalog
            .providers
            .iter()
            .filter(|p| query.is_empty() || p.id.to_lowercase().contains(&query) || p.name.to_lowercase().contains(&query))
            .collect();
        let mut popular: Vec<_> = all.iter().filter(|p| POPULAR.contains(&p.id.as_ref())).copied().collect();
        popular.sort_by_key(|p| models::popular_rank(&p.id));
        let mut other: Vec<_> = all.iter().filter(|p| !POPULAR.contains(&p.id.as_ref())).copied().collect();
        other.sort_by(|a, b| a.name.cmp(&b.name));
        let row = |entry: &crate::model::state::ProviderEntry| {
            let id = entry.id.to_string();
            h_flex()
                .id(element_id("connect-provider", &id))
                .test_support()
                .w_full()
                .min_h(px(36.))
                .px_2()
                .gap_2()
                .rounded(theme.radius)
                .cursor_pointer()
                .text_sm()
                .hover(|this| this.bg(theme.accent))
                .on_click(cx.listener({
                    let id = id.clone();
                    move |this, _, window, cx| this.select_provider(&id, window, cx)
                }))
                .child(badge(&entry.name, cx))
                .child(div().flex_shrink_0().font_weight(FontWeight::MEDIUM).child(SharedString::from(entry.name.to_string())))
                .when_some(note(&id), |this, key| this.child(div().min_w_0().truncate().text_color(theme.muted_foreground).child(self.t(key))))
                .when(recommended(&id), |this| this.child(Tag::secondary().small().child(self.t("dialog.provider.tag.recommended"))))
        };
        let group = |title: &str, items: &[&crate::model::state::ProviderEntry]| {
            (!items.is_empty()).then(|| {
                v_flex()
                    .w_full()
                    .child(div().px_2().pb_1().pt_2().text_xs().text_color(theme.muted_foreground).child(self.t(title)))
                    .children(items.iter().map(|entry| row(entry)))
            })
        };
        v_flex()
            .flex_1()
            .min_h_0()
            .gap_2()
            .child(Input::new(&self.connect_search).focus_ring(false).small().prefix(Icon::new(IconName::Search).small()).cleanable(true))
            .child(
                v_flex()
                    .id("connect-providers")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .when(all.is_empty(), |this| this.child(div().px_2().py_2().text_sm().text_color(theme.muted_foreground).child(self.t("dialog.provider.empty"))))
                    .children(group("dialog.provider.group.popular", &popular))
                    .children(group("dialog.provider.group.other", &other)),
            )
            .into_any_element()
    }

    fn render_methods(&self, provider: &str, name: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let rows = methods(provider).into_iter().enumerate().map(|(index, method)| {
            // `methodDetails`: a "(browser)"/"(headless)" suffix becomes a translated hint.
            let (label, hint) = match method {
                Method::ApiKey => (self.t("provider.connect.method.apiKey"), None),
                Method::Flow(flow) => {
                    let label = flow.label();
                    match [(" (browser)", "provider.connect.method.browser"), (" (headless)", "provider.connect.method.headless")]
                        .into_iter()
                        .find_map(|(suffix, key)| label.strip_suffix(suffix).map(|rest| (rest, key)))
                    {
                        Some((rest, key)) => (SharedString::from(rest.to_string()), Some(self.t(key))),
                        None => (SharedString::from(label), None),
                    }
                }
            };
            h_flex()
                .id(element_id("connect-method", index))
                .test_support()
                .w_full()
                .h(px(36.))
                .px_2()
                .gap_2()
                .rounded(theme.radius)
                .cursor_pointer()
                .text_sm()
                .hover(|this| this.bg(theme.accent))
                .on_click(cx.listener(move |this, _, window, cx| this.select_method(method, window, cx)))
                .child(div().font_weight(FontWeight::MEDIUM).child(label))
                .when_some(hint, |this, hint| this.child(div().text_color(theme.muted_foreground).child(hint)))
        });
        v_flex()
            .gap_2()
            .child(div().text_sm().child(SharedString::from(self.i18n.tf("provider.connect.selectMethod", &[("provider", name)]))))
            .children(rows)
            .into_any_element()
    }

    fn render_api_key(&self, provider: &str, name: &str, missing: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let intro = if provider == "opencode" {
            v_flex()
                .gap_2()
                .child(self.t("provider.connect.opencodeZen.line1"))
                .child(self.t("provider.connect.opencodeZen.line2"))
                .child(
                    h_flex()
                        .flex_wrap()
                        .child(self.t("provider.connect.opencodeZen.visit.prefix"))
                        .child(
                            Button::new("connect-zen-link")
                                .link()
                                .small()
                                .label(self.t("provider.connect.opencodeZen.visit.link"))
                                .on_click(|_, _, cx| cx.open_url("https://opencode.ai/zen")),
                        )
                        .child(self.t("provider.connect.opencodeZen.visit.suffix")),
                )
        } else {
            v_flex().child(SharedString::from(self.i18n.tf("provider.connect.apiKey.description", &[("provider", name)])))
        };
        v_flex()
            .gap_4()
            .text_sm()
            .child(intro)
            .child(
                v_flex()
                    .gap_1()
                    .child(div().font_weight(FontWeight::MEDIUM).child(SharedString::from(self.i18n.tf("provider.connect.apiKey.label", &[("provider", name)]))))
                    .child(Input::new(&self.api_key).focus_ring(false).small().mask_toggle())
                    .when(missing, |this| this.child(div().text_xs().text_color(theme.danger).child(self.t("provider.connect.apiKey.required")))),
            )
            .child(
                h_flex().child(
                    Button::new("connect-submit")
                        .small()
                        .primary()
                        .label(self.t("common.continue"))
                        .on_click(cx.listener(|this, _, window, cx| this.submit_key(window, cx))),
                ),
            )
            .into_any_element()
    }

    fn render_waiting(&self, url: &str, code: Option<&str>, name: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let link = url.to_string();
        let copied = code.map(str::to_string);
        v_flex()
            .gap_4()
            .text_sm()
            .child(
                h_flex()
                    .flex_wrap()
                    .child(self.t("provider.connect.oauth.auto.visit.prefix"))
                    .child(
                        Button::new("connect-open-link")
                            .link()
                            .small()
                            .label(self.t("provider.connect.oauth.auto.visit.link"))
                            .on_click(move |_, _, cx| cx.open_url(&link)),
                    )
                    .child(SharedString::from(self.i18n.tf("provider.connect.oauth.auto.visit.suffix", &[("provider", name)]))),
            )
            .when_some(copied, |this, code| {
                this.child(
                    v_flex().gap_1().child(div().font_weight(FontWeight::MEDIUM).child(self.t("provider.connect.oauth.auto.confirmationCode"))).child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .id("connect-code")
                                    .test_support()
                                    .px_2()
                                    .py_1()
                                    .rounded(theme.radius)
                                    .border_1()
                                    .border_color(theme.border)
                                    .bg(theme.muted)
                                    .font_family(theme.mono_font_family.clone())
                                    .child(SharedString::from(code.clone())),
                            )
                            .child(
                                Button::new("connect-copy-code")
                                    .ghost()
                                    .small()
                                    .icon(IconName::Copy)
                                    .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(code.clone()))),
                            ),
                    ),
                )
            })
            .child(h_flex().gap_2().child(Spinner::new()).child(self.t("provider.connect.status.waiting")))
            .into_any_element()
    }

    pub(super) fn render_connect(&self, connect: &Connect, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let name = connect.provider.as_deref().map(|id| self.provider_label(id)).unwrap_or_default();
        let title: SharedString = match &connect.provider {
            None => self.t("command.provider.connect"),
            Some(_) => self.i18n.tf("provider.connect.title", &[("provider", &name)]).into(),
        };
        let body = match (connect.provider.as_deref(), &connect.step) {
            (None, _) => self.render_provider_list(cx),
            (Some(id), ConnectStep::Choose) => self.render_methods(id, &name, cx),
            (Some(id), ConnectStep::ApiKey { missing }) => self.render_api_key(id, &name, *missing, cx),
            (Some(_), ConnectStep::Pending) => {
                h_flex().gap_2().text_sm().child(Spinner::new()).child(self.t("provider.connect.status.inProgress")).into_any_element()
            }
            (Some(_), ConnectStep::Waiting { url, code }) => self.render_waiting(url, code.as_deref(), &name, cx),
            (Some(_), ConnectStep::Failed(error)) => {
                let error = if error.is_empty() { self.t("common.requestFailed").to_string() } else { error.clone() };
                h_flex()
                    .gap_2()
                    .text_sm()
                    .child(Icon::new(IconName::CircleX).small().text_color(theme.danger))
                    .child(SharedString::from(self.i18n.tf("provider.connect.status.failed", &[("error", &error)])))
                    .into_any_element()
            }
        };
        v_flex()
            .id("connect-dialog")
            .test_support()
            .w(px(560.))
            .h(px(512.))
            .max_h_full()
            .gap_3()
            .p_4()
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .when(connect.provider.is_some(), |this| {
                        this.child(
                            Button::new("connect-back")
                                .ghost()
                                .small()
                                .icon(IconName::ArrowLeft)
                                .tooltip(self.t("common.goBack"))
                                .on_click(cx.listener(|this, _, window, cx| this.connect_back(window, cx))),
                        )
                        .child(badge(&name, cx))
                    })
                    .child(div().flex_1().min_w_0().truncate().text_sm().font_weight(FontWeight::MEDIUM).child(title))
                    .child(
                        Button::new("overlay-close")
                            .ghost()
                            .small()
                            .icon(IconName::Close)
                            .tooltip(self.t("common.close"))
                            .on_click(cx.listener(|this, _, window, cx| this.close_overlay(window, cx))),
                    ),
            )
            .child(v_flex().flex_1().min_h_0().child(body))
            .into_any_element()
    }

    // `settings-providers.tsx`: connected providers with their source, then popular ones to connect.
    pub(super) fn render_providers_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let catalog = &self.state.catalog;
        let connected = catalog.connected.iter().map(|provider| {
            let id = provider.id.to_string();
            let action = if provider.removable {
                Button::new(element_id("provider-disconnect", &id))
                    .small()
                    .ghost()
                    .label(self.t("common.disconnect"))
                    .on_click(cx.listener(move |this, _, _, cx| this.disconnect_provider(&id, cx)))
                    .into_any_element()
            } else if provider.source == ProviderSource::Env {
                div().text_xs().text_color(theme.muted_foreground).child(self.t("settings.providers.connected.environmentDescription")).into_any_element()
            } else {
                div().into_any_element()
            };
            h_flex()
                .w_full()
                .min_h(px(48.))
                .gap_3()
                .border_b_1()
                .border_color(theme.border)
                .child(badge(&provider.name, cx))
                .child(div().min_w_0().truncate().text_sm().font_weight(FontWeight::MEDIUM).child(SharedString::from(provider.name.to_string())))
                .child(Tag::secondary().small().child(self.t(provider.source.key())))
                .child(div().flex_1())
                .child(action)
        });
        let mut popular: Vec<_> =
            catalog.providers.iter().filter(|p| POPULAR.contains(&p.id.as_ref()) && !catalog.connected.iter().any(|c| c.id == p.id)).collect();
        popular.sort_by_key(|p| models::popular_rank(&p.id));
        let popular = popular.into_iter().map(|provider| {
            let id = provider.id.to_string();
            h_flex()
                .w_full()
                .min_h(px(56.))
                .gap_3()
                .border_b_1()
                .border_color(theme.border)
                .child(badge(&provider.name, cx))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            h_flex()
                                .gap_2()
                                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(SharedString::from(provider.name.to_string())))
                                .when(recommended(&id), |this| this.child(Tag::secondary().small().child(self.t("dialog.provider.tag.recommended")))),
                        )
                        .when_some(settings_note(&id), |this, key| this.child(div().text_xs().text_color(theme.muted_foreground).child(self.t(key)))),
                )
                .child(
                    Button::new(element_id("provider-connect", &id))
                        .small()
                        .outline()
                        .icon(IconName::Plus)
                        .label(self.t("common.connect"))
                        .on_click(cx.listener(move |this, _, window, cx| this.open_connect(Some(id.clone()), Some(SettingsTab::Providers), window, cx))),
                )
        });
        v_flex()
            .w_full()
            .gap_6()
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).pb_1().child(self.t("settings.providers.section.connected")))
                    .when(catalog.connected.is_empty(), |this| {
                        this.child(div().py_3().text_sm().text_color(theme.muted_foreground).child(self.t("settings.providers.connected.empty")))
                    })
                    .children(connected),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).pb_1().child(self.t("settings.providers.section.popular")))
                    .children(popular)
                    .child(
                        h_flex().pt_3().child(
                            Button::new("provider-view-all")
                                .link()
                                .small()
                                .label(self.t("dialog.provider.viewAll"))
                                .on_click(cx.listener(|this, _, window, cx| this.open_connect(None, Some(SettingsTab::Providers), window, cx))),
                        ),
                    ),
            )
            .into_any_element()
    }
}
