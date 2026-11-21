// Transcript rows -> elements. Pure presentation: all state arrives through `RowContext`.
use crate::model::i18n::I18n;
use crate::model::state::ChildActivity;
use crate::model::time;
use crate::model::transcript::{Row, RowKind, ToolStatus, ToolView};
use gpui_kit::component::alert::Alert;
use gpui_kit::component::bubble::{Bubble, BubbleVariant};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::diff::{Diff, DiffState};
use gpui_kit::component::message::{Message, MessageAlignment, MessageContent};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::text::TextView;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, ClickEvent, ClipboardItem, ElementId, Entity, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, TestSupportExt as _, Window, div, px,
};
use std::hash::{Hash as _, Hasher as _};
use std::rc::Rc;
use std::sync::Arc;

const BLOCK_LINES: usize = 60;

pub type Callback = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

pub struct RowContext {
    pub expanded: bool,
    pub diff: Option<Entity<DiffState>>,
    pub toggle: Callback,
    pub i18n: Rc<I18n>,
    // What the subagent of a `task` call is doing, and how to open its session.
    pub child: Option<ChildActivity>,
    pub open_child: Option<Callback>,
    // Offered on stored user messages while the session is idle.
    pub fork: Option<Callback>,
    pub revert: Option<Callback>,
}

fn shared(text: &Arc<str>) -> SharedString {
    SharedString::from(text.clone())
}

fn element_id(id: &Arc<str>, suffix: &'static str) -> ElementId {
    ElementId::Name(format!("{id}{suffix}").into())
}

// The original's tool titles (`message-part.tsx` `getToolInfo`); unknown tools keep their name.
pub fn tool_label(tool: &str, i18n: &I18n) -> SharedString {
    let key = match tool {
        "bash" | "shell" => "ui.tool.shell",
        "read" => "ui.tool.read",
        "list" => "ui.tool.list",
        "glob" => "ui.tool.glob",
        "grep" => "ui.tool.grep",
        "task" => "ui.tool.task",
        "webfetch" => "ui.tool.webfetch",
        "websearch" => "ui.tool.websearch",
        "edit" => "ui.messagePart.title.edit",
        "write" => "ui.messagePart.title.write",
        "patch" | "apply_patch" => "ui.tool.patch",
        "todowrite" | "todoread" => "ui.tool.todos",
        "question" => "ui.tool.questions",
        "skill" => "ui.tool.skill",
        other => return SharedString::from(other.to_string()),
    };
    i18n.t(key).into()
}

// The diff view scrolls on its own and needs a definite height: one line per hunk row plus a header per file.
pub fn diff_height(patch: &str) -> gpui_kit::Pixels {
    let files = patch.lines().filter(|line| line.starts_with("+++ ")).count().max(1);
    let rows = patch
        .lines()
        .filter(|line| !line.starts_with("+++ ") && !line.starts_with("--- "))
        .filter(|line| matches!(line.as_bytes().first(), Some(b'+' | b'-' | b' ' | b'@')))
        .count();
    px((rows as f32 * 20.0 + files as f32 * 36.0 + 12.0).clamp(72.0, 380.0))
}

pub fn diff_view(state: &Entity<DiffState>, patch: &str) -> impl IntoElement {
    Diff::new(state).w_full().h(diff_height(patch))
}

pub fn copy_button(id: ElementId, text: SharedString, i18n: &I18n) -> Button {
    Button::new(id).ghost().xsmall().icon(IconName::Copy).tooltip(SharedString::from(i18n.t("ui.message.copy"))).on_click(move |_, _, cx| {
        cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
    })
}

fn code_block(label: SharedString, text: &str, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let total = text.lines().count();
    let mut shown: String = text.lines().take(BLOCK_LINES).collect::<Vec<_>>().join("\n");
    if total > BLOCK_LINES {
        shown.push_str(&format!("\n… +{}", total - BLOCK_LINES));
    }
    v_flex()
        .w_full()
        .gap_1()
        .child(div().text_xs().text_color(theme.muted_foreground).child(label))
        .child(
            div()
                .w_full()
                .min_w_0()
                .overflow_hidden()
                .rounded(theme.radius)
                .bg(theme.muted)
                .px_2()
                .py_1p5()
                .font_family(theme.mono_font_family.clone())
                .text_xs()
                .child(SharedString::from(shown)),
        )
}

fn status_icon(id: &Arc<str>, status: ToolStatus, cx: &App) -> AnyElement {
    let theme = cx.theme();
    match status {
        ToolStatus::Pending => Icon::new(IconName::Loader).small().text_color(theme.muted_foreground).into_any_element(),
        // Rust build: a breathing clay dot instead of a spinner.
        ToolStatus::Running => div().size_4().flex().items_center().justify_center().child(super::motion::pulse(element_id(id, "-pulse"), 8.0, cx)).into_any_element(),
        ToolStatus::Completed => Icon::new(IconName::CircleCheck).small().text_color(theme.success).into_any_element(),
        ToolStatus::Error => Icon::new(IconName::CircleX).small().text_color(theme.danger).into_any_element(),
    }
}

fn chevron(expanded: bool, cx: &App) -> Icon {
    Icon::new(if expanded { IconName::ChevronDown } else { IconName::ChevronRight }).small().text_color(cx.theme().muted_foreground)
}

fn child_line(id: &Arc<str>, ctx: &RowContext, cx: &App) -> Option<AnyElement> {
    let theme = cx.theme();
    let open = ctx.open_child.clone()?;
    let activity = ctx.child.clone().unwrap_or_default();
    let summary = match activity.tools {
        0 => activity.last.to_string(),
        tools if activity.last.is_empty() => ctx.i18n.tf("desktop.rs.child.tools", &[("count", &tools.to_string())]),
        tools => format!("{} · {}", ctx.i18n.tf("desktop.rs.child.tools", &[("count", &tools.to_string())]), activity.last),
    };
    Some(
        h_flex()
            .w_full()
            .gap_2()
            .px_3()
            .py_1()
            .border_t_1()
            .border_color(theme.border)
            .text_xs()
            .text_color(theme.muted_foreground)
            .when(activity.running, |this| this.child(super::motion::pulse(element_id(id, "-child-pulse"), 6.0, cx)))
            .child(div().flex_1().min_w_0().truncate().child(SharedString::from(summary)))
            .child(
                Button::new(element_id(id, "-child"))
                    .ghost()
                    .xsmall()
                    .icon(IconName::ExternalLink)
                    .label(SharedString::from(ctx.i18n.t("notification.action.goToSession")))
                    .on_click(move |event, window, cx| open(event, window, cx)),
            )
            .into_any_element(),
    )
}

fn tool_card(id: &Arc<str>, view: &ToolView, ctx: RowContext, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let toggle = ctx.toggle.clone();
    let t = |key: &str| SharedString::from(ctx.i18n.t(key));
    let header = h_flex()
        .id(element_id(id, "-header"))
        .test_support()
        .w_full()
        .gap_2()
        .px_3()
        .py_1p5()
        .cursor_pointer()
        .hover(|this| this.bg(theme.muted))
        .on_click(move |event, window, cx| toggle(event, window, cx))
        .child(status_icon(id, view.status, cx))
        .child(div().flex_shrink_0().text_sm().font_weight(FontWeight::MEDIUM).child(tool_label(&view.tool, &ctx.i18n)))
        .child(div().flex_1().min_w_0().truncate().text_sm().text_color(theme.muted_foreground).child(shared(&view.title)))
        .when_some(view.duration_ms, |this, ms| {
            this.child(div().flex_shrink_0().text_xs().text_color(theme.muted_foreground).child(time::duration(ms)))
        })
        .child(chevron(ctx.expanded, cx));

    v_flex()
        .w_full()
        .min_w_0()
        .overflow_hidden()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .child(header)
        .children(child_line(id, &ctx, cx))
        .when(ctx.expanded, |this| {
            // Rust build: the opened details ease in.
            this.child(super::motion::enter(element_id(id, "-open"), 4.0,
                v_flex()
                    .w_full()
                    .gap_3()
                    .p_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .when(!view.input.is_empty(), |this| this.child(code_block(t("desktop.rs.tool.input"), &view.input, cx)))
                    .when_some(ctx.diff.as_ref().zip(view.diff.as_ref()), |this, (state, patch)| this.child(diff_view(state, patch)))
                    .when(!view.output.is_empty(), |this| {
                        let label = if view.status == ToolStatus::Error { "ui.messagePart.diagnostic.error" } else { "desktop.rs.tool.output" };
                        this.child(code_block(t(label), &view.output, cx))
                    })
                    .when(view.input.is_empty() && view.output.is_empty() && view.diff.is_none(), |this| {
                        this.child(div().text_xs().text_color(theme.muted_foreground).child(t("desktop.rs.tool.empty")))
                    }),
            ))
        })
        .into_any_element()
}

fn reasoning(id: &Arc<str>, text: &Arc<str>, streaming: bool, ctx: RowContext, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let toggle = ctx.toggle.clone();
    let label = ctx.i18n.t(if streaming { "ui.sessionTurn.status.thinking" } else { "desktop.rs.reasoning.done" });
    v_flex()
        .w_full()
        .gap_1()
        .child(
            h_flex()
                .id(element_id(id, "-header"))
                .test_support()
                .gap_1()
                .cursor_pointer()
                .text_sm()
                .text_color(theme.muted_foreground)
                .on_click(move |event, window, cx| toggle(event, window, cx))
                .child(chevron(ctx.expanded, cx))
                // Rust build: "Thinking" shimmers while it lasts.
                .child(if streaming { super::motion::shimmer(element_id(id, "-shimmer"), label) } else { SharedString::from(label).into_any_element() }),
        )
        .when(ctx.expanded, |this| {
            this.child(
                div()
                    .w_full()
                    .pl_3()
                    .border_l_2()
                    .border_color(theme.border)
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(TextView::markdown(element_id(id, "-body"), shared(text)).selectable(true)),
            )
        })
        .into_any_element()
}

fn user(id: &Arc<str>, text: &Arc<str>, files: &[Arc<str>], ctx: RowContext, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let stored = id.starts_with("msg");
    let action = |suffix: &'static str, icon: IconName, tooltip: &str, run: Callback| {
        Button::new(element_id(id, suffix))
            .ghost()
            .xsmall()
            .icon(icon)
            .tooltip(SharedString::from(ctx.i18n.t(tooltip)))
            .on_click(move |event, window, cx| run(event, window, cx))
    };
    v_flex()
        .w_full()
        .items_end()
        .gap_1()
        .when(!files.is_empty(), |this| {
            this.child(h_flex().gap_1().flex_wrap().justify_end().children(files.iter().map(|name| Tag::secondary().small().child(shared(name)))))
        })
        .when(!text.is_empty(), |this| {
            this.child(
                Message::new()
                    .alignment(MessageAlignment::End)
                    .content(MessageContent::new().bubble(Bubble::new().with_variant(BubbleVariant::Muted).child(shared(text)))),
            )
        })
        .when(stored, |this| {
            this.child(
                h_flex()
                    .gap_0p5()
                    .text_color(theme.muted_foreground)
                    .when_some(ctx.revert.clone(), |this, run| this.child(action("-revert", IconName::Undo2, "ui.message.revertMessage", run)))
                    .when_some(ctx.fork.clone(), |this, run| this.child(action("-fork", IconName::Replace, "ui.message.forkMessage", run)))
                    .child(copy_button(element_id(id, "-copy"), shared(text), &ctx.i18n)),
            )
        })
        .into_any_element()
}

fn code_copy_id(code: &str) -> ElementId {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    code.hash(&mut hasher);
    ElementId::Name(format!("code-copy-{:x}", hasher.finish()).into())
}

fn assistant_text(id: &Arc<str>, markdown: &Arc<str>, streaming: bool, ctx: RowContext) -> AnyElement {
    let view = TextView::markdown(element_id(id, ""), shared(markdown))
        .selectable(true)
        .stream_fade(streaming)
        .code_block_actions(|block, _, _| {
            let code = block.code();
            Clipboard::new(code_copy_id(&code)).value(code)
        });
    v_flex()
        .w_full()
        .min_w_0()
        .gap_1()
        .child(view)
        .when(!streaming, |this| this.child(h_flex().child(copy_button(element_id(id, "-copy"), shared(markdown), &ctx.i18n))))
        .into_any_element()
}

fn divider(label: SharedString, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let line = || div().flex_1().h(px(1.)).bg(theme.border);
    h_flex().w_full().gap_3().text_xs().text_color(theme.muted_foreground).child(line()).child(label).child(line()).into_any_element()
}

// `SessionRetry`: the countdown is dropped once it reaches zero, while the attempt is in flight.
pub fn retry_text(attempt: u32, message: &str, remaining_ms: u64, i18n: &I18n) -> String {
    let retrying = i18n.t("ui.sessionTurn.retry.retrying");
    let attempt = i18n.tf("ui.sessionTurn.retry.attempt", &[("attempt", &attempt.to_string())]);
    match remaining_ms.div_ceil(1000) {
        0 => format!("{message} — {retrying}, {attempt}"),
        seconds => format!("{message} — {retrying}, {attempt}, {}", i18n.tf("ui.sessionTurn.retry.inSeconds", &[("seconds", &seconds.to_string())])),
    }
}

pub fn render_row(row: &Row, ctx: RowContext, cx: &App) -> AnyElement {
    let theme = cx.theme();
    match &row.kind {
        RowKind::User { text, files } => user(&row.id, text, files, ctx, cx),
        RowKind::Text { markdown, streaming } => assistant_text(&row.id, markdown, *streaming, ctx),
        RowKind::Reasoning { text, streaming } => reasoning(&row.id, text, *streaming, ctx, cx),
        RowKind::Tool(view) => tool_card(&row.id, view, ctx, cx),
        RowKind::Error { message } => Alert::error(element_id(&row.id, ""), shared(message)).into_any_element(),
        RowKind::Retry { attempt, message, until_ms } => h_flex()
            .gap_2()
            .text_sm()
            .text_color(theme.muted_foreground)
            .child(Spinner::new().small())
            .child(retry_text(*attempt, message, until_ms.saturating_sub(opencode_storage::id::now_ms()), &ctx.i18n))
            .into_any_element(),
        RowKind::Working { label } => h_flex()
            .gap_2()
            .items_center()
            .text_sm()
            .text_color(theme.muted_foreground)
            .child(super::motion::pulse("working-pulse", 8.0, cx))
            .child(super::motion::shimmer("working-label", format!("{}…", ctx.i18n.t(label))))
            .into_any_element(),
        RowKind::Compaction => divider(ctx.i18n.t("ui.messagePart.compaction").into(), cx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_text_counts_down_then_drops_the_delay() {
        let en = I18n::default();
        let waiting = retry_text(2, "busy", 2_500, &en);
        let now = retry_text(2, "busy", 0, &en);
        assert!(waiting.starts_with("busy — ") && waiting.contains('3'), "{waiting}");
        assert!(now.starts_with("busy — ") && !now.contains('3') && now.len() < waiting.len(), "{now}");
    }
}
