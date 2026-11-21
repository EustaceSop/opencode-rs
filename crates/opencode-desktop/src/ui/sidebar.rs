// The session list: search, subagent sessions nested under their parents.
use super::app::{AppView, SIDEBAR_WIDTH, now_ms};
use crate::model::keymap::Command;
use crate::model::time;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::input::Input;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, ElementId, InteractiveElement as _, IntoElement, ParentElement as _, SharedString, StatefulInteractiveElement as _,
    Styled as _, TestSupportExt as _, div, px, uniform_list,
};
use std::ops::Range;

impl AppView {
    fn session_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let now = now_ms();
        let theme = cx.theme();
        let rows = self.sidebar.clone();
        range
            .filter_map(|index| {
                let row = rows.get(index)?;
                let session = &row.session;
                let id = session.id.clone();
                let selected = self.state.current.as_deref() == Some(id.as_ref());
                let running = self.state.is_running(&id) || self.state.child(&id).is_some_and(|child| child.running);
                let waiting = self.state.waiting(&id);
                let age = match time::relative(now, session.updated) {
                    age if age == "now" => self.t("common.time.justNow"),
                    age => age.into(),
                };
                let element = h_flex()
                    .id(ElementId::Name(format!("session-{id}").into()))
                    .test_support()
                    .w_full()
                    .h(px(30.))
                    .px_2()
                    .gap_2()
                    .rounded(theme.radius)
                    .cursor_pointer()
                    .text_sm()
                    .text_color(theme.sidebar_foreground)
                    .when(row.depth > 0, |this| this.pl_6().text_color(theme.muted_foreground))
                    .when(selected, |this| this.bg(theme.sidebar_accent).text_color(theme.sidebar_accent_foreground))
                    .when(!selected, |this| this.hover(|this| this.bg(theme.sidebar_accent.opacity(0.6))))
                    .on_click(cx.listener(move |this, _, window, cx| this.open_session(&id, window, cx)))
                    .child(div().flex_1().min_w_0().truncate().child(SharedString::from(session.title.clone())))
                    .when(waiting, |this| this.child(div().size_2().rounded_full().bg(theme.warning)))
                    .when(running && !waiting, |this| this.child(super::motion::pulse(SharedString::from(format!("side-pulse-{}", session.id)), 6.0, cx)))
                    .when(!running && row.depth == 0, |this| this.child(div().flex_shrink_0().text_xs().text_color(theme.muted_foreground).child(age)));
                Some(div().w_full().px_2().child(element).into_any_element())
            })
            .collect()
    }

    pub(super) fn render_sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let count = self.sidebar.len();
        let empty: Option<SharedString> = match (count, self.state.filter.trim()) {
            (0, "") => Some(self.t("home.sessions.empty")),
            (0, query) => Some(self.i18n.tf("home.sessions.search.noResults", &[("query", query)]).into()),
            _ => None,
        };
        v_flex()
            .id("sidebar")
            .test_support()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_shrink_0()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                h_flex()
                    .p_2()
                    .gap_1()
                    .child(
                        h_flex().flex_1().min_w_0().child(
                            Button::new("open-folder")
                                .ghost()
                                .small()
                                .icon(IconName::FolderOpen)
                                .label(self.state.project_name())
                                .tooltip(SharedString::from(self.state.directory.to_string_lossy().into_owned()))
                                .on_click(cx.listener(|this, _, window, cx| this.run(Command::OpenProject, window, cx))),
                        ),
                    )
                    .child(
                        Button::new("new-session")
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .tooltip(self.t("command.session.new"))
                            .on_click(cx.listener(|this, _, window, cx| this.new_session(window, cx))),
                    ),
            )
            .child(div().px_2().pb_2().child(Input::new(&self.search).focus_ring(false).small().cleanable(true).prefix(Icon::new(IconName::Search).small())))
            .child(div().px_4().pb_1().text_xs().text_color(theme.muted_foreground).child(self.t("home.sessions.search.sessions")))
            .when_some(empty, |this, text| this.child(div().px_4().py_2().text_sm().text_color(theme.muted_foreground).child(text)))
            .child(
                uniform_list("sessions", count, cx.processor(|this, range: Range<usize>, _, cx| this.session_rows(range, cx)))
                    .flex_1()
                    .min_h_0(),
            )
            .into_any_element()
    }
}
