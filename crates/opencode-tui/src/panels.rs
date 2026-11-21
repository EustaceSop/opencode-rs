// Panels around the transcript: question prompt, permission rejection, sidebar, subagent footer, which-key,
// scrollbar and the export dialog body.
use crate::app::{App, Hit, HitTarget, SIDEBAR_WIDTH, SessionView, WhichKeyLayout, implemented_keybinds};
use crate::dialog::{COMMANDS, EXPORT_LABELS, ExportDialog};
use crate::keybind;
use crate::question::QuestionState;
use crate::text::{self, grouped, padded, spans_width, titlecase, truncate, truncate_left, wrap_plain};
use crate::theme::{Rgba, tint};
use crate::view::usage;
use opencode_storage::Message;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

type Spans = Vec<Span<'static>>;

fn fg(color: Rgba) -> Style {
    Style::default().fg(color.color())
}

// Rows on the panel background behind a coloured left border.
fn framed(app: &App, border: Rgba, width: usize, rows: Vec<Spans>) -> Vec<Line<'static>> {
    let fill = Style::default().bg(app.theme.background_panel.color());
    rows.into_iter()
        .map(|content| {
            let content = content.into_iter().map(|span| Span::styled(span.content, fill.patch(span.style))).collect();
            padded(vec![Span::styled("┃", fg(border).bg(app.theme.background_panel.color())), Span::styled("  ", fill)], content, width, fill)
        })
        .collect()
}

// The question prompt and, while a custom answer is typed, the cursor as (column, row) inside it.
pub fn question_lines(app: &App, width: usize) -> (Vec<Line<'static>>, Option<(u16, u16)>) {
    let theme = &app.theme;
    let Some(ask) = app.question_ask() else { return (Vec::new(), None) };
    let questions = ask.request.questions.as_slice();
    let state = &app.question;
    let inner = width.saturating_sub(6);
    let (muted, plain) = (fg(theme.text_muted), fg(theme.text));
    let single = QuestionState::single(questions);
    let confirm = state.confirm(questions);
    let multi = state.multi(questions);
    let mut rows: Vec<Spans> = vec![Vec::new()];
    let mut cursor = None;
    if !single {
        let mut tabs: Spans = Vec::new();
        for (index, question) in questions.iter().enumerate() {
            let answered = state.answers.get(index).is_some_and(|a| !a.is_empty());
            let style = if index == state.tab {
                Style::default().bg(theme.accent.color()).fg(theme.selected_foreground(Some(theme.accent)).color())
            } else if answered {
                plain
            } else {
                muted
            };
            tabs.push(Span::styled(format!(" {} ", question.header), style));
            tabs.push(Span::raw(" "));
        }
        let style = if confirm { Style::default().bg(theme.accent.color()).fg(theme.selected_foreground(Some(theme.accent)).color()) } else { muted };
        tabs.push(Span::styled(" Confirm ", style));
        rows.push(tabs);
        rows.push(Vec::new());
    }
    if confirm {
        rows.push(vec![Span::styled("Review", plain)]);
        rows.push(Vec::new());
        for (index, question) in questions.iter().enumerate() {
            let value = state.answers.get(index).map(|a| a.join(", ")).unwrap_or_default();
            let shown = if value.is_empty() { Span::styled("(not answered)", fg(theme.error)) } else { Span::styled(value, plain) };
            rows.push(vec![Span::styled(format!("{}: ", question.header), muted), shown]);
        }
    } else if let Some(question) = questions.get(state.tab) {
        let suffix = if multi { " (select all that apply)" } else { "" };
        rows.extend(wrap_plain(&format!("{}{suffix}", question.question), inner, plain));
        rows.push(Vec::new());
        let option = |index: usize, label: &str, picked: bool| -> Spans {
            let active = index == state.selected;
            let back = if active { Style::default().bg(theme.background_element.color()) } else { Style::default() };
            let number = if active { tint(theme.text_muted, theme.secondary, 0.6) } else { theme.text_muted };
            let color = if active { theme.secondary } else if picked { theme.success } else { theme.text };
            let label = if multi { format!("[{}] {label}", if picked { "✓" } else { " " }) } else { label.to_string() };
            let mut spans = vec![Span::styled(format!("{}. ", index + 1), back.fg(number.color())), Span::styled(truncate(&label, inner.saturating_sub(6)), back.fg(color.color()))];
            if !multi && picked {
                spans.push(Span::styled(" ✓", fg(theme.success)));
            }
            spans
        };
        for (index, item) in question.options.iter().enumerate() {
            rows.push(option(index, &item.label, state.picked(&item.label)));
            for line in wrap_plain(&item.description, inner.saturating_sub(3), muted) {
                let mut spans = vec![Span::raw("   ")];
                spans.extend(line);
                rows.push(spans);
            }
        }
        if state.custom_allowed(questions) {
            rows.push(option(question.options.len(), "Type your own answer", state.custom_picked()));
            match &state.editing {
                Some(editor) => {
                    let shown = if editor.is_empty() { Span::styled("Type your own answer", muted) } else { Span::styled(editor.text.clone(), plain) };
                    cursor = Some((6 + text::width(&editor.text[..editor.cursor]) as u16, rows.len() as u16));
                    rows.push(vec![Span::raw("   "), shown]);
                }
                None if !state.input().is_empty() => rows.push(vec![Span::raw("   "), Span::styled(state.input().to_string(), muted)]),
                None => {}
            }
        }
    }
    rows.push(Vec::new());
    let mut hints: Spans = Vec::new();
    let mut hint = |key: &str, label: &str| {
        hints.push(Span::styled(format!("{key} "), plain));
        hints.push(Span::styled(format!("{label}  "), muted));
    };
    if !single {
        hint("⇆", "tab");
    }
    if !confirm {
        hint("↑↓", "select");
    }
    hint("enter", if confirm { "submit" } else if multi { "toggle" } else if single { "submit" } else { "confirm" });
    hint("esc", "dismiss");
    rows.push(hints);
    rows.push(Vec::new());
    (framed(app, theme.accent, width, rows), cursor)
}

// `RejectPrompt`
pub fn reject_lines(app: &App, width: usize) -> (Vec<Line<'static>>, Option<(u16, u16)>) {
    let theme = &app.theme;
    let editor = &app.permission_feedback;
    let rows: Vec<Spans> = vec![
        Vec::new(),
        vec![Span::styled("△ ", fg(theme.error)), Span::styled("Reject permission", fg(theme.text))],
        Vec::new(),
        vec![Span::styled("Tell OpenCode what to do differently", fg(theme.text_muted))],
        Vec::new(),
        vec![Span::styled(editor.text.clone(), fg(theme.text))],
        Vec::new(),
        vec![Span::styled("enter ", fg(theme.text)), Span::styled("confirm  ", fg(theme.text_muted)), Span::styled("esc ", fg(theme.text)), Span::styled("cancel", fg(theme.text_muted))],
        Vec::new(),
    ];
    let cursor = (3 + text::width(&editor.text[..editor.cursor]) as u16, 5);
    (framed(app, theme.error, width, rows), Some(cursor))
}

// `SubagentFooter`
pub fn subagent_lines(app: &App, view: &SessionView, width: usize) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let label = view
        .info
        .title
        .split('@')
        .nth(1)
        .and_then(|rest| rest.split(" subagent").next().filter(|_| rest.contains(" subagent")))
        .filter(|name| !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_'))
        .map_or_else(|| "Subagent".to_string(), titlecase);
    let mut siblings: Vec<&crate::app::Child> = view.children.iter().filter(|child| child.info.parent_id == view.info.parent_id).collect();
    siblings.sort_by_key(|child| child.info.time.created);
    let mut left = vec![Span::styled(label, fg(theme.text).add_modifier(Modifier::BOLD))];
    if let Some(index) = siblings.iter().position(|child| child.info.id == view.info.id) {
        left.push(Span::styled(format!(" ({} of {})", index + 1, siblings.len()), fg(theme.text_muted)));
    }
    if let Some(usage) = usage(app, view) {
        left.push(Span::styled(format!(" {usage}"), fg(theme.text_muted)));
    }
    let mut right: Spans = Vec::new();
    for (name, command) in [("Parent", "session_parent"), ("Prev", "session_child_cycle_reverse"), ("Next", "session_child_cycle")] {
        right.push(Span::styled(format!("{name} "), fg(theme.text)));
        right.push(Span::styled(format!("{}  ", app.keymap.display(command)), fg(theme.text_muted)));
    }
    let gap = width.saturating_sub(4 + spans_width(&left) + spans_width(&right));
    left.push(Span::raw(" ".repeat(gap)));
    left.extend(right);
    framed(app, theme.border, width, vec![Vec::new(), left, Vec::new()])
}

// `Sidebar` with the built-in context, MCP, todo and modified-files sections.
pub fn sidebar(frame: &mut Frame, app: &App, view: &SessionView, area: Rect) {
    let theme = &app.theme;
    let width = usize::from(area.width.saturating_sub(4));
    let (bold, muted) = (fg(theme.text).add_modifier(Modifier::BOLD), fg(theme.text_muted));
    let mut lines: Vec<Line<'static>> = vec![Line::default()];
    lines.extend(wrap_plain(&view.info.title, width, bold).into_iter().map(Line::from));
    lines.push(Line::default());

    let last = view.messages.iter().rev().find_map(|(m, _)| match m {
        Message::Assistant(a) if a.tokens.output > 0.0 => Some(a),
        _ => None,
    });
    let tokens = last.map_or(0.0, |a| a.tokens.input + a.tokens.output + a.tokens.reasoning + a.tokens.cache.read + a.tokens.cache.write);
    let limit = last.and_then(|a| app.catalog.model(&crate::state::ModelKey::new(&a.provider_id, &a.model_id))).map(|(_, m)| m.context).filter(|c| *c > 0);
    let percent = limit.map_or(0.0, |limit| (tokens / limit as f64 * 100.0).round());
    lines.push(Line::from(Span::styled("Context", bold)));
    lines.push(Line::from(Span::styled(format!("{} tokens", grouped(tokens as u64)), muted)));
    lines.push(Line::from(Span::styled(format!("{percent}% used"), muted)));
    lines.push(Line::from(Span::styled(format!("${:.2} spent", view.info.cost.unwrap_or(0.0)), muted)));

    // Connection state lives in the engine; this lists what the config declares.
    if !app.catalog.mcp.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled("MCP", bold)));
        for (name, enabled) in &app.catalog.mcp {
            lines.push(Line::from(vec![Span::styled("• ", muted), Span::styled(truncate(name, width.saturating_sub(13)), fg(theme.text)), Span::styled(if *enabled { " Configured" } else { " Disabled" }, muted)]));
        }
    }
    if view.todos.iter().any(|todo| todo.status != "completed") {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled("Todo", bold)));
        for todo in &view.todos {
            let style = if todo.status == "in_progress" { fg(theme.warning) } else { muted };
            let mark = match todo.status.as_str() {
                "completed" => "✓",
                "in_progress" => "•",
                _ => " ",
            };
            for (index, body) in wrap_plain(&todo.content, width.saturating_sub(4), style).into_iter().enumerate() {
                let mut spans = vec![Span::styled(if index == 0 { format!("[{mark}] ") } else { "    ".to_string() }, style)];
                spans.extend(body);
                lines.push(Line::from(spans));
            }
        }
    }
    let diffs = view.info.summary.as_ref().and_then(|s| s.diffs.as_ref()).filter(|d| !d.is_empty());
    if let Some(diffs) = diffs {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled("Modified Files", bold)));
        for diff in diffs {
            let mut counts: Spans = Vec::new();
            if diff.additions > 0.0 {
                counts.push(Span::styled(format!(" +{}", diff.additions), fg(theme.diff_added)));
            }
            if diff.deletions > 0.0 {
                counts.push(Span::styled(format!(" -{}", diff.deletions), fg(theme.diff_removed)));
            }
            let room = width.saturating_sub(spans_width(&counts)).max(2);
            let name = truncate_left(diff.file.as_deref().unwrap_or_default(), room);
            let gap = width.saturating_sub(text::width(&name) + spans_width(&counts));
            let mut spans = vec![Span::styled(name, muted), Span::raw(" ".repeat(gap))];
            spans.extend(counts);
            lines.push(Line::from(spans));
        }
    }
    frame.render_widget(Clear, area);
    frame.buffer_mut().set_style(area, Style::default().bg(theme.background_panel.color()).fg(theme.text.color()));
    let body = Rect::new(area.x + 2, area.y, area.width.saturating_sub(4), area.height.saturating_sub(2));
    frame.render_widget(Paragraph::new(lines), body);
    if area.height >= 4 {
        let footer = Line::from(vec![
            Span::styled("• ", fg(theme.success)),
            Span::styled("Open", muted.add_modifier(Modifier::BOLD)),
            Span::styled("Code ", bold),
            Span::styled(opencode_session::VERSION, muted),
        ]);
        frame.render_widget(Paragraph::new(footer), Rect::new(area.x + 2, area.y + area.height - 2, area.width.saturating_sub(4), 1));
    }
}

pub fn sidebar_rect(app: &App, area: Rect) -> Option<Rect> {
    let width = SIDEBAR_WIDTH.min(area.width.saturating_sub(20));
    (app.sidebar_visible() && width >= 20).then(|| Rect::new(area.x + area.width - width, area.y, width, area.height))
}

// One-column scrollbar for the transcript; registers its track for dragging.
pub fn scrollbar(frame: &mut Frame, app: &App, area: Rect, scroll: usize, total: usize) {
    let height = usize::from(area.height);
    if height == 0 || total <= height {
        return;
    }
    let theme = &app.theme;
    let thumb = (height * height / total).clamp(1, height);
    let max = total - height;
    let top = (height - thumb) * (max - scroll.min(max)) / max;
    let x = area.x + area.width - 1;
    for row in 0..height {
        let on = row >= top && row < top + thumb;
        let style = if on { fg(theme.border_active) } else { fg(theme.border_subtle) };
        frame.buffer_mut().set_string(x, area.y + row as u16, if on { "█" } else { "│" }, style);
    }
    app.hits.borrow_mut().push(Hit { x, y: area.y, width: 1, height: area.height, target: HitTarget::Scrollbar });
}

fn group_of(name: &str) -> &'static str {
    if let Some((_, _, category, _)) = COMMANDS.iter().find(|(key, ..)| *key == name) {
        return category;
    }
    match name {
        _ if name.starts_with("input_") || name.starts_with("history_") => "Input",
        _ if name.starts_with("messages_") || name.starts_with("session_") => "Session",
        _ if name.starts_with("model_") || name.starts_with("agent_") => "Agent",
        _ if name.starts_with("dialog.") || name.starts_with("prompt.") || name.starts_with("permission.") || name == "stash_delete" => "Dialog",
        _ if name.starts_with("which_key_") => "Which-key",
        _ => "System",
    }
}

// Reachable bindings by group: (group, [(keys still to press, description)]). With a pending sequence only its
// continuations are listed.
pub fn which_key_groups(app: &App) -> Vec<(String, Vec<(String, String)>)> {
    let implemented = implemented_keybinds();
    let mut groups: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for def in keybind::definitions() {
        if def.name == keybind::LEADER || !implemented.contains(&def.name.as_str()) {
            continue;
        }
        let Some(rest) = app.keymap.sequences(&def.name).iter().find_map(|sequence| sequence.strip_prefix(app.pending.as_slice()).filter(|rest| !rest.is_empty())) else { continue };
        let entry = (app.keymap.format(rest), def.description.clone());
        let group = group_of(&def.name);
        match groups.iter_mut().find(|(label, _)| label == group) {
            Some((_, entries)) => entries.push(entry),
            None => groups.push((group.to_string(), vec![entry])),
        }
    }
    groups
}

pub fn which_key_height(app: &App, area: Rect) -> u16 {
    if !app.which_key_visible() {
        return 0;
    }
    ((f64::from(area.height) * 0.3).floor() as u16).clamp(8, 16).min(area.height.saturating_sub(6))
}

// `WhichKeyPanel`: tabs per group when pinned, every continuation at once while a sequence is pending.
pub fn which_key(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    if area.height < 4 {
        return;
    }
    let pending = !app.pending.is_empty();
    let groups = which_key_groups(app);
    let width = usize::from(area.width.saturating_sub(2)).max(1);
    let columns = ((width + 4) / 32).max(1);
    let column_width = ((width.saturating_sub((columns - 1) * 4)) / columns).clamp(1, 44);
    let (accent, muted, plain) = (fg(theme.accent).add_modifier(Modifier::BOLD), fg(theme.text_muted), fg(theme.text));
    let mut lines: Vec<Line<'static>> = vec![Line::default()];
    let entries: Vec<(String, String)> = if pending {
        groups.iter().flat_map(|(_, entries)| entries.clone()).collect()
    } else {
        let at = app.which_key.group.min(groups.len().saturating_sub(1));
        let mut tabs: Spans = vec![Span::raw(" ")];
        for (index, (label, _)) in groups.iter().enumerate() {
            let style = if index == at { Style::default().bg(theme.primary.color()).fg(theme.selected_foreground(Some(theme.primary)).color()) } else { muted };
            tabs.push(Span::styled(format!(" {label} "), style));
            tabs.push(Span::raw(" "));
        }
        lines.push(Line::from(tabs));
        lines.push(Line::default());
        groups.get(at).map(|(_, entries)| entries.clone()).unwrap_or_default()
    };
    let footer = usize::from(!pending) * 2;
    let rows = usize::from(area.height).saturating_sub(lines.len() + footer).max(1);
    if entries.is_empty() {
        lines.push(Line::from(Span::styled(" No reachable bindings", muted)));
    }
    let first = app.which_key.scroll.min(entries.len().div_ceil(columns).saturating_sub(rows)) * columns;
    for row in 0..rows {
        let mut spans: Spans = vec![Span::raw(" ")];
        for column in 0..columns {
            let Some((keys, description)) = entries.get(first + row * columns + column) else { break };
            let keys = truncate(keys, column_width / 2);
            let room = column_width.saturating_sub(text::width(&keys) + 1);
            let description = truncate(description, room);
            let gap = column_width.saturating_sub(text::width(&keys) + text::width(&description));
            spans.push(Span::styled(keys, accent));
            spans.push(Span::raw(" ".repeat(gap)));
            spans.push(Span::styled(description, plain));
            spans.push(Span::raw("    "));
        }
        lines.push(Line::from(spans));
    }
    if !pending {
        lines.truncate(usize::from(area.height).saturating_sub(1));
        while lines.len() + 1 < usize::from(area.height) {
            lines.push(Line::default());
        }
        let next = if app.which_key.layout == WhichKeyLayout::Dock { "overlay" } else { "dock" };
        let left = vec![Span::styled(" toggle ", plain), Span::styled(app.keymap.display("which_key_toggle"), muted)];
        let right = vec![Span::styled(format!("{next} "), plain), Span::styled(format!("{} ", app.keymap.display("which_key_layout_toggle")), muted)];
        let gap = usize::from(area.width).saturating_sub(spans_width(&left) + spans_width(&right));
        let mut spans = left;
        spans.push(Span::raw(" ".repeat(gap)));
        spans.extend(right);
        lines.push(Line::from(spans));
    }
    frame.render_widget(Clear, area);
    frame.buffer_mut().set_style(area, Style::default().bg(theme.background_panel.color()).fg(theme.text.color()));
    frame.render_widget(Paragraph::new(lines), area);
}

// Body of `DialogExportOptions` and the cursor inside the filename field.
pub fn export_lines(app: &App, export: &ExportDialog, width: usize) -> (Vec<Line<'static>>, Option<(u16, u16)>) {
    let theme = &app.theme;
    let (muted, plain) = (fg(theme.text_muted), fg(theme.text));
    let mut lines = vec![Line::from(vec![Span::raw("  "), Span::styled("Filename:", plain)])];
    let name = if export.filename.is_empty() { Span::styled("Enter filename", muted) } else { Span::styled(truncate(&export.filename.text, width.saturating_sub(4)), plain) };
    lines.push(Line::from(vec![Span::raw("  "), name]));
    lines.push(Line::default());
    for (index, label) in EXPORT_LABELS.iter().enumerate() {
        let active = export.active == index + 1;
        let back = if active { Style::default().bg(theme.background_element.color()) } else { Style::default() };
        let mark = if export.flags[index] { "[x]" } else { "[ ]" };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{mark} "), back.fg(if active { theme.primary } else { theme.text_muted }.color())),
            Span::styled(label.to_string(), back.fg(if active { theme.primary } else { theme.text }.color())),
        ]));
    }
    lines.push(Line::default());
    let hint = if export.active == 0 {
        vec![Span::styled("  Press ", muted), Span::styled("return", plain), Span::styled(" to confirm, ", muted), Span::styled("tab", plain), Span::styled(" for options", muted)]
    } else {
        vec![Span::styled("  Press ", muted), Span::styled("space", plain), Span::styled(" to toggle, ", muted), Span::styled("return", plain), Span::styled(" to confirm", muted)]
    };
    lines.push(Line::from(hint));
    lines.push(Line::default());
    let cursor = (export.active == 0).then(|| (2 + text::width(&export.filename.text[..export.filename.cursor]) as u16, 1));
    (lines, cursor)
}
