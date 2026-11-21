// What sits between the transcript and the prompt: reverted messages, todos, permission and question requests.
use super::app::{AppView, permission_key};
use super::rows::diff_view;
use crate::model::question::{QuestionView, allows_custom};
use crate::model::state::PermissionView;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::input::Input;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, ElementId, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, TestSupportExt as _, div,
};
use opencode_session::Reply;

impl AppView {
    pub(super) fn render_revert(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let hidden = self.state.current_revert();
        let session = self.state.current.clone().filter(|_| hidden > 0)?;
        let theme = cx.theme();
        let summary = self.i18n.tn("session.revertDock.summary", hidden as u64, &[]);
        Some(
            h_flex()
                .id("revert-dock")
                .test_support()
                .w_full()
                .gap_2()
                .px_3()
                .py_2()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.border)
                .text_sm()
                .child(Icon::new(IconName::Undo2).small().text_color(theme.muted_foreground))
                .child(div().flex_1().min_w_0().truncate().child(SharedString::from(summary)))
                .child(
                    Button::new("revert-restore")
                        .small()
                        .outline()
                        .label(self.t("session.revertDock.restore"))
                        .disabled(self.state.current_running())
                        .on_click(cx.listener(move |this, _, _, _| this.backend.unrevert(session.clone()))),
                )
                .into_any_element(),
        )
    }

    // Shown while something is left to do, like the original's todo dock.
    pub(super) fn render_todos(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let todos = self.state.current_todos();
        let done = todos.iter().filter(|todo| todo.status == "completed").count();
        let open = todos.iter().filter(|todo| !matches!(todo.status.as_str(), "completed" | "cancelled")).count();
        if open == 0 {
            return None;
        }
        let theme = cx.theme();
        let running = self.state.current_running();
        let progress = self.i18n.tf("session.todo.progress", &[("done", &done.to_string()), ("total", &todos.len().to_string())]);
        let collapsed = self.todos_collapsed;
        let items = todos.iter().map(|todo| {
            let icon = match todo.status.as_str() {
                "completed" => Icon::new(IconName::CircleCheck).small().text_color(theme.success).into_any_element(),
                "in_progress" if running => Spinner::new().small().into_any_element(),
                "in_progress" => Icon::new(IconName::Loader).small().text_color(theme.foreground).into_any_element(),
                "cancelled" => Icon::new(IconName::Close).small().text_color(theme.muted_foreground).into_any_element(),
                _ => Icon::new(IconName::Square).small().text_color(theme.muted_foreground).into_any_element(),
            };
            let settled = matches!(todo.status.as_str(), "completed" | "cancelled");
            h_flex()
                .w_full()
                .gap_2()
                .items_start()
                .text_sm()
                .when(settled, |this| this.text_color(theme.muted_foreground))
                .child(div().pt_0p5().child(icon))
                .child(div().flex_1().min_w_0().child(SharedString::from(todo.content.clone())))
        });
        Some(
            v_flex()
                .id("todo-dock")
                .test_support()
                .w_full()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.border)
                .child(
                    h_flex()
                        .id("todo-toggle")
                        .test_support()
                        .w_full()
                        .gap_2()
                        .px_3()
                        .py_1p5()
                        .cursor_pointer()
                        .text_sm()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.todos_collapsed = !this.todos_collapsed;
                            cx.notify();
                        }))
                        .child(div().font_weight(FontWeight::MEDIUM).child(self.t("session.todo.title")))
                        .child(div().flex_1().min_w_0().truncate().text_xs().text_color(theme.muted_foreground).child(SharedString::from(progress)))
                        .child(Icon::new(if collapsed { IconName::ChevronRight } else { IconName::ChevronDown }).small().text_color(theme.muted_foreground)),
                )
                .when(!collapsed, |this| this.child(v_flex().w_full().gap_1().px_3().pb_2().children(items)))
                .into_any_element(),
        )
    }

    // A question outranks a permission: the model is blocked on it either way, and only one card fits.
    pub(super) fn render_request(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if let Some(question) = self.state.current_question().cloned() {
            return Some(self.render_question(&question, cx));
        }
        self.state.current_permission().cloned().map(|request| self.render_permission(&request, cx))
    }

    fn render_permission(&self, request: &PermissionView, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let id = request.id;
        let reply = |element: &'static str, label: &str, reply: Reply, cx: &Context<Self>| {
            Button::new(element).small().label(self.t(label)).on_click(cx.listener(move |this, _, _, cx| {
                this.backend.reply(id, reply);
                cx.notify();
            }))
        };
        let diff = self.diffs.get(&permission_key(id)).zip(request.diff.as_ref());
        v_flex()
            .id("permission-card")
            .test_support()
            .w_full()
            .gap_2()
            .p_3()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.warning)
            .bg(theme.background)
            .child(
                h_flex()
                    .gap_2()
                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(self.t("notification.permission.title")))
                    .child(Tag::warning().small().child(SharedString::from(request.permission.clone()))),
            )
            .when_some(request.detail.as_ref(), |this, detail| {
                this.child(
                    div()
                        .w_full()
                        .rounded(theme.radius)
                        .bg(theme.muted)
                        .px_2()
                        .py_1()
                        .font_family(theme.mono_font_family.clone())
                        .text_xs()
                        .child(SharedString::from(detail.clone())),
                )
            })
            .when(request.detail.is_none() && !request.patterns.is_empty(), |this| {
                this.child(div().text_xs().text_color(theme.muted_foreground).child(request.patterns.join(", ")))
            })
            .when_some(diff, |this, (state, patch)| this.child(diff_view(state, patch)))
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(reply("permission-reject", "ui.permission.deny", Reply::Reject, cx).ghost())
                    .child(reply("permission-always", "ui.permission.allowAlways", Reply::Always, cx).outline())
                    .child(reply("permission-once", "ui.permission.allowOnce", Reply::Once, cx).primary()),
            )
            .into_any_element()
    }

    pub(super) fn toggle_answer(&mut self, question: usize, label: &str, cx: &mut Context<Self>) {
        if let (Some(view), Some(form)) = (self.state.current_question().cloned(), self.question.as_mut()) {
            form.toggle(&view, question, label);
            cx.notify();
        }
    }

    // Sends the chosen labels and typed answers; `dismiss` rejects the question instead.
    pub(super) fn answer_question(&mut self, dismiss: bool, cx: &mut Context<Self>) {
        let (Some(view), Some(form)) = (self.state.current_question().cloned(), self.question.as_mut()) else {
            return;
        };
        if dismiss {
            self.backend.answer(view.id, None);
        } else {
            for (index, input) in self.question_inputs.iter().enumerate() {
                form.set_custom(index, &input.read(cx).value());
            }
            if !form.complete(&view) {
                cx.notify();
                return;
            }
            self.backend.answer(view.id, Some(form.answers(&view)));
        }
        cx.notify();
    }

    fn render_question(&self, view: &QuestionView, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let form = self.question.clone().unwrap_or_default();
        let typed = |index: usize| self.question_inputs.get(index).is_some_and(|input| !input.read(cx).value().trim().is_empty());
        let answered = view.questions.iter().enumerate().filter(|(index, info)| {
            typed(*index) && allows_custom(info) || info.options.iter().any(|option| form.is_selected(*index, &option.label))
        });
        let answered = answered.count();
        let questions = view.questions.iter().enumerate().map(|(index, info)| {
            let hint = if info.multiple == Some(true) { "ui.question.multiHint" } else { "ui.question.singleHint" };
            let options = info.options.iter().enumerate().map(|(position, option)| {
                let selected = form.is_selected(index, &option.label);
                let label = option.label.clone();
                h_flex()
                    .id(ElementId::Name(format!("question-{index}-{position}").into()))
                    .test_support()
                    .w_full()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(if selected { theme.ring } else { theme.border })
                    .cursor_pointer()
                    .text_sm()
                    .when(selected, |this| this.bg(theme.accent))
                    .hover(|this| this.bg(theme.accent))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_answer(index, &label, cx)))
                    .child(div().w_4().flex_shrink_0().when(selected, |this| this.child(Icon::new(IconName::Check).small())))
                    .child(div().flex_shrink_0().font_weight(FontWeight::MEDIUM).child(SharedString::from(option.label.clone())))
                    .child(div().flex_1().min_w_0().truncate().text_xs().text_color(theme.muted_foreground).child(SharedString::from(option.description.clone())))
            });
            v_flex()
                .w_full()
                .gap_1()
                .when(!info.header.is_empty(), |this| {
                    this.child(div().text_xs().text_color(theme.muted_foreground).child(SharedString::from(info.header.clone())))
                })
                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(SharedString::from(info.question.clone())))
                .child(div().text_xs().text_color(theme.muted_foreground).child(self.t(hint)))
                .children(options)
                .when_some(self.question_inputs.get(index).filter(|_| allows_custom(info)), |this, input| this.child(Input::new(input).focus_ring(false).small()))
        });
        v_flex()
            .id("question-card")
            .test_support()
            .w_full()
            .gap_3()
            .p_3()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.ring)
            .bg(theme.background)
            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(self.t("notification.question.title")))
            .children(questions)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(SharedString::from(self.i18n.tf("ui.question.subtitle.answered", &[("count", &answered.to_string())]))),
                    )
                    .child(
                        Button::new("question-dismiss")
                            .small()
                            .ghost()
                            .label(self.t("ui.common.dismiss"))
                            .on_click(cx.listener(|this, _, _, cx| this.answer_question(true, cx))),
                    )
                    .child(
                        Button::new("question-submit")
                            .small()
                            .primary()
                            .label(self.t("ui.common.submit"))
                            .disabled(answered < view.questions.len())
                            .on_click(cx.listener(|this, _, _, cx| this.answer_question(false, cx))),
                    ),
            )
            .into_any_element()
    }
}
