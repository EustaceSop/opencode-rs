use crate::app::{App, Hit, HitTarget, PermissionStage, Route, SessionView, ToastKind, WhichKeyLayout};
use crate::dialog::{Dialog, SelectDialog, SelectKind};
use crate::panels;
use crate::state::ModelKey;
use crate::text::{self, padded, spans_width, titlecase, truncate, truncate_left, wrap_plain};
use crate::theme::{Rgba, Theme, tint};
use crate::transcript::{self, diff_rows, format_path};
use opencode_session::Status;
use opencode_storage::Message;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use serde_json::Value;

pub const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const LOGO_LEFT: [&str; 4] = ["                   ", "█▀▀█ █▀▀█ █▀▀█ █▀▀▄", "█__█ █__█ █^^^ █__█", "▀▀▀▀ █▀▀▀ ▀▀▀▀ ▀~~▀"];
const LOGO_RIGHT: [&str; 4] = ["             ▄     ", "█▀▀▀ █▀▀█ █▀▀█ █▀▀█", "█___ █__█ █__█ █^^^", "▀▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀"];
const PLACEHOLDERS: [&str; 3] = ["Fix a TODO in the codebase", "What is the tech stack of this project?", "Fix broken tests"];

fn fg(color: Rgba) -> Style {
    Style::default().fg(color.color())
}

fn paint(frame: &mut Frame, area: Rect, lines: Vec<Line<'static>>) {
    frame.render_widget(Paragraph::new(lines), area);
}

fn logo_line(source: &str, color: Rgba, bold: bool, theme: &Theme) -> Vec<Span<'static>> {
    let shadow = tint(theme.background, color, 0.25);
    let base = if bold { fg(color).add_modifier(Modifier::BOLD) } else { fg(color) };
    source
        .chars()
        .map(|c| match c {
            '_' => Span::styled(" ", base.bg(shadow.color())),
            '^' => Span::styled("▀", base.bg(shadow.color())),
            '~' => Span::styled("▀", fg(shadow)),
            ',' => Span::styled("▄", fg(shadow)),
            other => Span::styled(other.to_string(), base),
        })
        .collect()
}

fn logo(theme: &Theme) -> Vec<Line<'static>> {
    LOGO_LEFT
        .iter()
        .zip(LOGO_RIGHT)
        .map(|(left, right)| {
            let mut spans = logo_line(left, theme.text_muted, false, theme);
            spans.push(Span::raw(" "));
            spans.extend(logo_line(right, theme.text, true, theme));
            Line::from(spans)
        })
        .collect()
}

// `{highlight}…{/highlight}` segments render in the text color, the rest muted.
fn tip_spans(tip: &str, theme: &Theme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = tip;
    while let Some(start) = rest.find("{highlight}") {
        let after = &rest[start + 11..];
        let Some(end) = after.find("{/highlight}") else { break };
        if start > 0 {
            spans.push(Span::styled(rest[..start].to_string(), fg(theme.text_muted)));
        }
        spans.push(Span::styled(after[..end].to_string(), fg(theme.text)));
        rest = &after[end + 12..];
    }
    if !rest.is_empty() {
        spans.push(Span::styled(rest.to_string(), fg(theme.text_muted)));
    }
    spans
}

pub fn directory_label(app: &App) -> String {
    let shown = format_path(app, &app.directory.to_string_lossy());
    let shown = if shown == "." { app.directory.to_string_lossy().replace('\\', "/") } else { shown };
    let home = app.home.to_string_lossy().replace('\\', "/");
    let shown = match shown.strip_prefix(&home).filter(|_| !home.is_empty()) {
        Some(rest) => format!("~{rest}"),
        None => shown,
    };
    match &app.branch {
        Some(branch) => format!("{shown}:{branch}"),
        None => shown,
    }
}

// `Locale.number`
fn compact(n: f64) -> String {
    if n >= 1_000_000.0 {
        format!("{:.1}M", n / 1_000_000.0)
    } else if n >= 1000.0 {
        format!("{:.1}K", n / 1000.0)
    } else {
        format!("{n}")
    }
}

// Context use of the last answered turn plus the session cost, as the prompt shows it.
pub fn usage(app: &App, view: &SessionView) -> Option<String> {
    let last = view.messages.iter().rev().find_map(|(m, _)| match m {
        Message::Assistant(a) if a.tokens.output > 0.0 => Some(a),
        _ => None,
    })?;
    let t = &last.tokens;
    let tokens = t.input + t.output + t.reasoning + t.cache.read + t.cache.write;
    if tokens <= 0.0 {
        return None;
    }
    let limit = app.catalog.model(&ModelKey::new(&last.provider_id, &last.model_id)).map(|(_, m)| m.context).filter(|c| *c > 0);
    let mut out = match limit {
        Some(limit) => format!("{} ({}%)", compact(tokens), (tokens / limit as f64 * 100.0).round()),
        None => compact(tokens),
    };
    let cost = view.info.cost.unwrap_or(0.0);
    if cost > 0.0 {
        out.push_str(&format!(" · ${cost:.2}"));
    }
    Some(out)
}

// The usage figures in colour: tokens plain, the share of the context window in yellow, the cost in blue.
fn usage_spans(app: &App, view: &SessionView) -> Option<Vec<Span<'static>>> {
    let theme = &app.theme;
    let text = usage(app, view)?;
    let (tokens, cost) = text.split_once(" · ").map_or((text.as_str(), None), |(t, c)| (t, Some(c)));
    let (count, share) = tokens.split_once(" (").map_or((tokens, None), |(n, s)| (n, Some(s.trim_end_matches(')'))));
    let mut spans = vec![Span::styled(count.to_string(), fg(theme.text))];
    if let Some(share) = share {
        spans.extend([Span::styled(" ctx ", fg(theme.text_muted)), Span::styled(share.to_string(), fg(theme.warning))]);
    }
    if let Some(cost) = cost {
        spans.extend([Span::styled(" · ", fg(theme.text_muted)), Span::styled(cost.to_string(), fg(theme.info))]);
    }
    Some(spans)
}

pub fn prompt_rows(app: &App) -> usize {
    app.editor.rows(app.prompt_text_width()).len().clamp(1, app.prompt_max_rows())
}

pub fn prompt_height(app: &App) -> u16 {
    (prompt_rows(app) + if app.claude_look() { 3 } else { 5 }) as u16
}

// `content` laid out between `│ ` and ` │` on a line exactly `width` columns wide.
fn boxed(content: Vec<Span<'static>>, width: usize, border: Style) -> Line<'static> {
    let gap = width.saturating_sub(spans_width(&content) + 4);
    let mut spans = vec![Span::styled("│ ", border)];
    spans.extend(content);
    spans.push(Span::raw(" ".repeat(gap)));
    spans.push(Span::styled(" │", border));
    Line::from(spans)
}

fn rule(left: &str, right: &str, width: usize, border: Style) -> Line<'static> {
    Line::from(Span::styled(format!("{left}{}{right}", "─".repeat(width.saturating_sub(2))), border))
}

// Rust build, Claude look: the input in a rounded frame with a `>` marker, the agent and model on the line below.
fn prompt_claude(frame: &mut Frame, app: &App, area: Rect, focused: bool) {
    let theme = &app.theme;
    let width = usize::from(area.width);
    let border = fg(if app.shell_mode { theme.primary } else { theme.border });
    let text_width = app.prompt_text_width();
    let rows = app.editor.rows(text_width);
    let visible = prompt_rows(app);
    let (cursor_row, cursor_col) = app.editor.cursor_position(text_width);
    let first = cursor_row.saturating_sub(visible - 1).min(rows.len().saturating_sub(visible));
    let marker = Span::styled(if app.shell_mode { "! " } else { "> " }, if app.shell_mode { fg(theme.primary) } else { fg(theme.text) }.add_modifier(Modifier::BOLD));
    // Placeholder text sits well below the muted grey, so it reads as a hint rather than content.
    let faint = fg(crate::theme::tint(theme.text_muted, theme.background, 0.45));
    let row = |body: Vec<Span<'static>>| Line::from(body);

    // Open-sided: a rule above and below the input, as wide as the column.
    let mut lines = vec![Line::from(Span::styled("─".repeat(width), border))];
    if app.editor.is_empty() {
        let example = PLACEHOLDERS[(app.now_ms / 1000 / 3600) as usize % PLACEHOLDERS.len()];
        let placeholder = if app.shell_mode { String::new() } else { format!("Ask anything… \"{example}\"") };
        lines.push(row(vec![marker.clone(), Span::styled(truncate(&placeholder, text_width), faint)]));
    } else {
        let selection = app.editor.selection();
        let selected = Style::default().bg(theme.text.color()).fg(theme.background.color());
        for (index, r) in rows.iter().enumerate().skip(first).take(visible) {
            let text = &app.editor.text;
            let lead = if index == 0 { marker.clone() } else { Span::raw("  ") };
            let body = match selection.map(|(start, end)| (start.clamp(r.start, r.end), end.clamp(r.start, r.end))).filter(|(start, end)| start < end) {
                Some((start, end)) => vec![
                    lead,
                    Span::styled(text[r.start..start].to_string(), fg(theme.text)),
                    Span::styled(text[start..end].to_string(), selected),
                    Span::styled(text[end..r.end].to_string(), fg(theme.text)),
                ],
                None => vec![lead, Span::styled(text[r.start..r.end].to_string(), fg(theme.text))],
            };
            lines.push(row(body));
        }
    }
    lines.push(Line::from(Span::styled("─".repeat(width), border)));

    let muted = fg(theme.text_muted);
    let mut left: Vec<Span<'static>> = vec![Span::raw("  ")];
    let view = app.session();
    if app.shell_mode {
        left.extend([Span::styled("shell mode", fg(theme.primary)), Span::styled(" · esc to exit", muted)]);
    } else if let Some(view) = view.filter(|view| view.busy()) {
        let spin = if app.animations() { SPINNER_FRAMES[app.frame as usize % SPINNER_FRAMES.len()] } else { "·" };
        // A light sweeps across the label, one cell per frame.
        let label: Vec<char> = format!("{spin} Working…").chars().collect();
        let head = if app.animations() { (app.frame as usize) % (label.len() + 8) } else { usize::MAX };
        for (index, c) in label.iter().enumerate() {
            let color = match (index + 4).abs_diff(head) {
                0 => crate::theme::tint(theme.primary, theme.text, 0.6),
                1 => crate::theme::tint(theme.primary, theme.text, 0.3),
                _ => theme.primary,
            };
            left.push(Span::styled(c.to_string(), fg(color)));
        }
        if let Status::Retry { attempt, message, next } = &view.status {
            let seconds = next.saturating_sub(app.now_ms) / 1000;
            let wait = if seconds > 0 { format!(" in {seconds}s") } else { String::new() };
            left.push(Span::styled(format!(" {} · retrying{wait}, attempt #{attempt}", truncate(message, 60)), fg(theme.error)));
        }
        let hot = app.interrupt > 0;
        let started = view.messages.iter().rev().find_map(|(m, _)| m.as_user().map(|u| u.time.created));
        let elapsed = started.map(|at| format!("{} · ", text::duration(app.now_ms.saturating_sub(at) / 1000 * 1000))).unwrap_or_default();
        left.push(Span::styled(format!(" ({elapsed}{})", if hot { "esc again to interrupt" } else { "esc to interrupt" }), if hot { fg(theme.primary) } else { muted }));
    } else {
        let (model, provider) = app.model_label();
        left.push(Span::styled(app.agent.clone(), fg(app.agent_color(&app.agent)).add_modifier(Modifier::BOLD)));
        if app.auto_permissions {
            left.push(Span::styled(" ▸▸ auto-accept", fg(theme.error)));
        }
        left.extend([Span::styled(" · ", muted), Span::styled(model, fg(theme.text).add_modifier(Modifier::BOLD)), Span::styled(format!(" {provider}"), faint)]);
        if let Some(variant) = app.current_variant() {
            left.extend([Span::styled(" · ", muted), Span::styled(variant, fg(theme.warning))]);
        }
    }
    let mut right: Vec<Span<'static>> = Vec::new();
    if app.leader_active() {
        right.push(Span::styled(format!("{} …  ", app.keymap.display("leader")), fg(theme.warning)));
    }
    match view.and_then(|view| usage_spans(app, view)) {
        Some(spans) => {
            right.extend(spans);
            right.push(Span::styled(" · ", muted));
        }
        None => right.extend([Span::styled(app.keymap.display("agent_cycle"), fg(theme.text)), Span::styled(" agents · ", muted)]),
    }
    right.extend([Span::styled(app.keymap.display("command_list"), fg(theme.text)), Span::styled(" commands ", muted)]);
    if spans_width(&left) + spans_width(&right) > width {
        right.clear();
    }
    let gap = width.saturating_sub(spans_width(&left) + spans_width(&right));
    left.push(Span::raw(" ".repeat(gap)));
    left.extend(right);
    lines.push(Line::from(left));
    paint(frame, area, lines);

    if focused {
        frame.set_cursor_position(Position::new(area.x + 2 + cursor_col as u16, area.y + 1 + (cursor_row - first) as u16));
    }
    autocomplete(frame, app, area, focused);
}

fn prompt(frame: &mut Frame, app: &App, area: Rect, focused: bool) {
    if app.claude_look() {
        return prompt_claude(frame, app, area, focused);
    }
    let theme = &app.theme;
    let width = usize::from(area.width);
    let fill = Style::default().bg(theme.background_element.color());
    let accent = if app.shell_mode { theme.primary } else { app.agent_color(&app.agent) };
    let edge = || Span::styled("┃", fg(accent).bg(theme.background_element.color()));
    let row = |body: Vec<Span<'static>>| {
        let body = body.into_iter().map(|span| Span::styled(span.content, fill.patch(span.style))).collect();
        padded(vec![edge(), Span::styled("  ", fill)], body, width, fill)
    };
    let text_width = app.prompt_text_width();
    let rows = app.editor.rows(text_width);
    let visible = prompt_rows(app);
    let (cursor_row, cursor_col) = app.editor.cursor_position(text_width);
    let first = cursor_row.saturating_sub(visible - 1).min(rows.len().saturating_sub(visible));

    let mut lines = vec![row(Vec::new())];
    if app.editor.is_empty() {
        let example = PLACEHOLDERS[(app.now_ms / 1000 / 3600) as usize % PLACEHOLDERS.len()];
        // Shell mode has no placeholder unless the host supplies shell examples.
        let placeholder = if app.shell_mode { String::new() } else { format!("Ask anything… \"{example}\"") };
        lines.push(row(vec![Span::styled(truncate(&placeholder, text_width), fg(theme.text_muted))]));
    } else {
        let selection = app.editor.selection();
        let selected = Style::default().bg(theme.text.color()).fg(theme.background_element.color());
        for r in rows.iter().skip(first).take(visible) {
            let text = &app.editor.text;
            match selection.map(|(start, end)| (start.clamp(r.start, r.end), end.clamp(r.start, r.end))).filter(|(start, end)| start < end) {
                Some((start, end)) => lines.push(row(vec![
                    Span::styled(text[r.start..start].to_string(), fg(theme.text)),
                    Span::styled(text[start..end].to_string(), selected),
                    Span::styled(text[end..r.end].to_string(), fg(theme.text)),
                ])),
                None => lines.push(row(vec![Span::styled(text[r.start..r.end].to_string(), fg(theme.text))])),
            }
        }
    }
    lines.push(row(Vec::new()));
    let (model, provider) = app.model_label();
    let leader = app.leader_active();
    let mut status = vec![Span::styled(if app.shell_mode { "Shell".to_string() } else { titlecase(&app.agent) }, fg(accent))];
    if !app.shell_mode && app.auto_permissions {
        status.push(Span::styled(" auto", fg(theme.text_muted)));
    }
    status.extend([
        Span::styled(" · ", fg(theme.text_muted)),
        Span::styled(model, fg(if leader { theme.text_muted } else { theme.text })),
        Span::styled(format!(" {provider}"), fg(theme.text_muted)),
    ]);
    if let Some(variant) = app.current_variant() {
        status.push(Span::styled(" · ", fg(theme.text_muted)));
        status.push(Span::styled(variant, fg(theme.warning).add_modifier(Modifier::BOLD)));
    }
    lines.push(row(status));
    let bar = if theme.background_element.a == 0 { " " } else { "▀" };
    lines.push(Line::from(vec![
        Span::styled(if theme.background_element.a == 0 { " " } else { "╹" }, fg(accent)),
        Span::styled(bar.repeat(width.saturating_sub(1)), fg(theme.background_element)),
    ]));

    let view = app.session();
    let mut left: Vec<Span<'static>> = Vec::new();
    if app.shell_mode {
        left.push(Span::styled(" esc ", fg(theme.text)));
        left.push(Span::styled("exit shell mode", fg(theme.text_muted)));
    } else if let Some(view) = view.filter(|view| view.busy()) {
        if app.animations() {
            left.push(Span::styled(format!(" {} ", SPINNER_FRAMES[app.frame as usize % SPINNER_FRAMES.len()]), fg(accent)));
        } else {
            left.push(Span::styled(" [⋯] ", fg(theme.text_muted)));
        }
        if let Status::Retry { attempt, message, next } = &view.status {
            let seconds = next.saturating_sub(app.now_ms) / 1000;
            let wait = if seconds > 0 { format!("in {seconds}s ") } else { String::new() };
            left.push(Span::styled(format!("{} [retrying {wait}attempt #{attempt}] ", truncate(message, 80)), fg(theme.error)));
        }
        let hot = app.interrupt > 0;
        left.push(Span::styled("esc ", fg(if hot { theme.primary } else { theme.text })));
        left.push(Span::styled(if hot { "again to interrupt" } else { "interrupt" }, fg(if hot { theme.primary } else { theme.text_muted })));
    }
    let mut right: Vec<Span<'static>> = Vec::new();
    if leader {
        right.push(Span::styled(format!("{} …  ", app.keymap.display("leader")), fg(theme.warning)));
    }
    match view.and_then(|view| usage(app, view)) {
        Some(usage) => right.push(Span::styled(usage, fg(theme.text_muted))),
        None => {
            right.push(Span::styled(app.keymap.display("agent_cycle"), fg(theme.text)));
            right.push(Span::styled(" agents", fg(theme.text_muted)));
        }
    }
    right.push(Span::styled(format!("  {}", app.keymap.display("command_list")), fg(theme.text)));
    right.push(Span::styled(" commands", fg(theme.text_muted)));
    let gap = width.saturating_sub(spans_width(&left) + spans_width(&right));
    left.push(Span::raw(" ".repeat(gap)));
    left.extend(right);
    lines.push(Line::from(left));
    paint(frame, area, lines);

    if focused {
        let y = area.y + 1 + (cursor_row - first) as u16;
        frame.set_cursor_position(Position::new(area.x + 3 + cursor_col as u16, y));
    }
    autocomplete(frame, app, area, focused);
}

fn autocomplete(frame: &mut Frame, app: &App, area: Rect, focused: bool) {
    let theme = &app.theme;
    let width = usize::from(area.width);
    if let Some(ac) = app.autocomplete.as_ref().filter(|_| focused) {
        // Rust build: a window of rows that follows the selection through the whole list.
        let rows = ac.items.len().min(crate::app::AUTOCOMPLETE_ROWS).min(usize::from(area.y));
        let first = (ac.selected + 1).saturating_sub(rows);
        let height = rows as u16;
        let popup = Rect::new(area.x, area.y - height, area.width, height);
        let menu = Style::default().bg(theme.background_menu.color());
        let lines: Vec<Line<'static>> = ac
            .items
            .iter()
            .enumerate()
            .skip(first)
            .take(rows)
            .map(|(index, item)| {
                let selected = index == ac.selected;
                let base = if selected { Style::default().bg(theme.primary.color()).fg(theme.selected_foreground(Some(theme.primary)).color()) } else { menu.fg(theme.text.color()) };
                let dim = if selected { base } else { menu.fg(theme.text_muted.color()) };
                let label = truncate_left(&item.label, width.saturating_sub(4));
                let room = width.saturating_sub(text::width(&label) + 4);
                padded(vec![Span::styled(format!(" {label}  "), base)], vec![Span::styled(truncate(&item.description, room), dim)], width, base)
            })
            .collect();
        frame.render_widget(Clear, popup);
        paint(frame, popup, lines);
    }
}

// Rust build, Claude look: a welcome card (name, version, folder, agent and model) above the input, a tip below.
fn home_claude(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let body = Rect::new(area.x + 2, area.y, area.width.saturating_sub(4), area.height);
    let width = (app.prompt_max_width() as u16).min(body.width);
    let inner = usize::from(width).saturating_sub(4);
    let border = fg(theme.primary);
    let muted = fg(theme.text_muted);
    let (model, provider) = app.model_label();
    let version = opencode_session::VERSION;
    let name = "OpenCode";
    let title = vec![
        Span::styled(name, fg(theme.primary).add_modifier(Modifier::BOLD)),
        Span::raw(" ".repeat(inner.saturating_sub(name.len() + version.len()))),
        Span::styled(version, muted),
    ];
    let card = vec![
        rule("╭", "╮", usize::from(width), border),
        boxed(title, usize::from(width), border),
        boxed(Vec::new(), usize::from(width), border),
        boxed(vec![Span::styled(truncate_left(&directory_label(app), inner), muted)], usize::from(width), border),
        boxed(vec![Span::styled(truncate(&format!("{} · {model} {provider}", app.agent), inner), muted)], usize::from(width), border),
        rule("╰", "╯", usize::from(width), border),
    ];
    let tip_lines: Vec<Line<'static>> = match &app.tip {
        Some(tip) => {
            let mut spans = vec![Span::styled("  Tip  ", fg(theme.primary))];
            spans.extend(tip_spans(tip, theme));
            text::wrap_spans(&spans, usize::from(width)).into_iter().map(Line::from).collect()
        }
        None => Vec::new(),
    };
    let prompt_h = prompt_height(app);
    let card_h = if body.height >= prompt_h + 10 { card.len() as u16 + 1 } else { 0 };
    let content = card_h + prompt_h + 1 + tip_lines.len() as u16;
    let top = body.y + body.height.saturating_sub(content) / 2;
    let x = body.x + body.width.saturating_sub(width) / 2;
    if card_h > 0 {
        paint(frame, Rect::new(x, top, width, card_h - 1), card);
    }
    let prompt_area = Rect::new(x, top + card_h, width, prompt_h.min(body.height.saturating_sub(card_h)));
    let tip_y = prompt_area.y + prompt_area.height + 1;
    if tip_y < body.y + body.height {
        let height = (tip_lines.len() as u16).min(body.y + body.height - tip_y);
        paint(frame, Rect::new(x, tip_y, width, height), tip_lines);
    }
    prompt(frame, app, prompt_area, app.dialog.is_none());
}

fn home(frame: &mut Frame, app: &App, area: Rect) {
    if app.claude_look() {
        return home_claude(frame, app, area);
    }
    let theme = &app.theme;
    let footer_height = 3.min(area.height);
    let body = Rect::new(area.x + 2, area.y, area.width.saturating_sub(4), area.height - footer_height);
    let prompt_width = (app.prompt_max_width() as u16).min(body.width);
    let prompt_h = prompt_height(app);
    let tip_lines: Vec<Line<'static>> = match &app.tip {
        Some(tip) => {
            let mut spans = vec![Span::styled("● Tip ", fg(theme.warning))];
            spans.extend(tip_spans(tip, theme));
            text::wrap_spans(&spans, usize::from(prompt_width)).into_iter().map(Line::from).collect()
        }
        None => Vec::new(),
    };
    let logo_lines = logo(theme);
    let logo_h = if body.height >= prompt_h + 6 { logo_lines.len() as u16 + 1 } else { 0 };
    let content = logo_h + prompt_h + 1 + tip_lines.len() as u16;
    let top = body.y + body.height.saturating_sub(content) / 2;
    let center = |w: u16| body.x + body.width.saturating_sub(w) / 2;
    if logo_h > 0 {
        let logo_w = spans_width(&logo_lines[0].spans) as u16;
        paint(frame, Rect::new(center(logo_w), top, logo_w.min(body.width), logo_h - 1), logo_lines);
    }
    let prompt_area = Rect::new(center(prompt_width), top + logo_h, prompt_width, prompt_h.min(body.height.saturating_sub(logo_h)));
    let tip_y = prompt_area.y + prompt_area.height + 1;
    if tip_y < body.y + body.height {
        let height = (tip_lines.len() as u16).min(body.y + body.height - tip_y);
        paint(frame, Rect::new(prompt_area.x, tip_y, prompt_width, height), tip_lines);
    }
    prompt(frame, app, prompt_area, app.dialog.is_none());

    if footer_height == 3 {
        let width = usize::from(area.width.saturating_sub(4));
        let version = opencode_session::VERSION;
        let directory = truncate_left(&directory_label(app), width.saturating_sub(version.len() + 2));
        let gap = width.saturating_sub(text::width(&directory) + version.len());
        let line = Line::from(vec![Span::styled(directory, fg(theme.text_muted)), Span::raw(" ".repeat(gap)), Span::styled(version, fg(theme.text_muted))]);
        paint(frame, Rect::new(area.x + 2, area.y + area.height - 2, area.width.saturating_sub(4), 1), vec![line]);
    }
}

fn permission_lines(app: &App, width: usize, room: usize) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let Some(ask) = app.permission() else { return Vec::new() };
    // Fullscreen gives the diff every row the screen can spare.
    let diff_rows_shown = if app.permission_fullscreen { room.saturating_sub(9).max(14) } else { 14 };
    let request = &ask.request;
    let data = &request.metadata;
    let string = |key: &str| data.get(key).and_then(Value::as_str).unwrap_or_default();
    let inner = width.saturating_sub(4);
    let muted = fg(theme.text_muted);
    let plain = fg(theme.text);
    let mut body: Vec<Vec<Span<'static>>> = Vec::new();
    let (icon, title) = match (app.permission_stage, request.permission.as_str()) {
        (PermissionStage::Always, permission) => {
            body.extend(wrap_plain(&format!("This will allow {permission} until OpenCode is restarted."), inner, muted));
            if !request.always.is_empty() {
                body.extend(wrap_plain(&format!("Patterns: {}", request.always.join(", ")), inner, muted));
            }
            ("△", "Always allow".to_string())
        }
        (_, "edit") => {
            body.extend(diff_rows(string("diff"), inner, theme).into_iter().take(diff_rows_shown));
            ("→", format!("Edit {}", format_path(app, string("filepath"))))
        }
        (_, "bash") => {
            body.extend(wrap_plain(&format!("$ {}", string("command")), inner, plain));
            ("#", "Shell command".to_string())
        }
        (_, "read") => ("→", format!("Read {}", format_path(app, request.patterns.first().map(String::as_str).unwrap_or_default()))),
        (_, "webfetch") => {
            body.extend(wrap_plain(&format!("URL: {}", string("url")), inner, muted));
            ("%", format!("WebFetch {}", string("url")))
        }
        (_, "external_directory") => {
            body.extend(wrap_plain(&format!("Patterns: {}", request.patterns.join(", ")), inner, muted));
            ("←", "Access external directory".to_string())
        }
        (_, permission) => {
            if !request.patterns.is_empty() {
                body.extend(wrap_plain(&format!("Patterns: {}", request.patterns.join(", ")), inner, muted));
            }
            ("⚙", format!("Call tool {permission}"))
        }
    };
    let fill = Style::default().bg(theme.background_panel.color());
    let row = |content: Vec<Span<'static>>| {
        let content = content.into_iter().map(|span| Span::styled(span.content, fill.patch(span.style))).collect();
        padded(vec![Span::styled("┃", fg(theme.warning).bg(theme.background_panel.color())), Span::styled("  ", fill)], content, width, fill)
    };
    let mut lines = vec![row(Vec::new())];
    let waiting = app.permissions.len().saturating_sub(1);
    let mut heading = vec![Span::styled("△ ", fg(theme.warning)), Span::styled("Permission required", plain)];
    if waiting > 0 {
        heading.push(Span::styled(format!("  +{waiting} more"), muted));
    }
    lines.push(row(heading));
    lines.push(row(vec![Span::styled(format!("{icon} "), muted), Span::styled(truncate(&title, inner.saturating_sub(2)), plain)]));
    if !body.is_empty() {
        lines.push(row(Vec::new()));
        lines.extend(body.into_iter().map(&row));
    }
    lines.push(row(Vec::new()));
    let mut options = Vec::new();
    for (index, label) in app.permission_options().iter().enumerate() {
        let style = if index == app.permission_choice {
            Style::default().bg(theme.warning.color()).fg(theme.selected_foreground(Some(theme.warning)).color())
        } else {
            Style::default().bg(theme.background_element.color()).fg(theme.text_muted.color())
        };
        options.push(Span::styled(format!(" {label} "), style));
        options.push(Span::raw(" "));
    }
    lines.push(row(options));
    let mut hints = Vec::new();
    let fullscreen = app.keymap.display("permission.prompt.fullscreen");
    if app.permission_stage == PermissionStage::Ask && !fullscreen.is_empty() {
        hints.push(Span::styled(format!("{fullscreen} "), plain));
        hints.push(Span::styled(if app.permission_fullscreen { "minimize  " } else { "fullscreen  " }, muted));
    }
    hints.extend([Span::styled("⇆ ", plain), Span::styled("select  ", muted), Span::styled("enter ", plain), Span::styled("confirm  ", muted), Span::styled("esc ", plain), Span::styled("reject", muted)]);
    lines.push(row(hints));
    lines.push(row(Vec::new()));
    lines
}

fn session(frame: &mut Frame, app: &App, view: &SessionView, area: Rect) {
    let theme = &app.theme;
    let side = panels::sidebar_rect(app, area);
    let area = match side {
        Some(side) => Rect::new(area.x, area.y, area.width - side.width, area.height),
        None => area,
    };
    let inner = Rect::new(area.x + 2, area.y, area.width.saturating_sub(4), area.height);
    let width = usize::from(inner.width);
    let room = usize::from(inner.height.saturating_sub(3));
    let asking = app.permission().is_some();
    // What sits under the transcript: a permission or question prompt, the subagent footer, or the input.
    let (panel, cursor): (Option<Vec<Line<'static>>>, Option<(u16, u16)>) = if asking && app.permission_stage == PermissionStage::Reject {
        let (lines, cursor) = panels::reject_lines(app, width);
        (Some(lines), cursor)
    } else if asking {
        (Some(permission_lines(app, width, room)), None)
    } else if app.question_ask().is_some() {
        let (lines, cursor) = panels::question_lines(app, width);
        (Some(lines), cursor)
    } else if view.is_child() {
        (Some(panels::subagent_lines(app, view, width)), None)
    } else {
        (None, None)
    };
    let bottom = panel.as_ref().map_or_else(|| prompt_height(app), |lines| lines.len() as u16);
    let bottom = bottom.min(inner.height.saturating_sub(3));
    let header = Rect::new(inner.x, inner.y, inner.width, 1.min(inner.height));
    let footer_y = inner.y + inner.height.saturating_sub(1);
    let bottom_y = footer_y.saturating_sub(bottom);
    let messages = Rect::new(inner.x, inner.y + 2, inner.width, bottom_y.saturating_sub(inner.y + 3));

    let mark = if app.claude_look() { Span::styled("● ", fg(theme.primary)) } else { Span::styled("# ", fg(theme.text_muted)) };
    let title = Line::from(vec![mark, Span::styled(truncate(&view.info.title, width.saturating_sub(2)), fg(theme.text).add_modifier(Modifier::BOLD))]);
    paint(frame, header, vec![title]);

    let bar = app.show_scrollbar();
    let laid = transcript::layout(app, view, width.saturating_sub(usize::from(bar) * 2));
    let height = usize::from(messages.height);
    let total = laid.lines.len();
    app.viewport.set((height, total));
    let scroll = view.scroll.min(total.saturating_sub(height));
    let start = total.saturating_sub(height + scroll);
    {
        let mut hits = app.hits.borrow_mut();
        for (from, to, target) in &laid.regions {
            let (top, end) = ((*from).max(start), (*to).min(start + height));
            if top < end {
                hits.push(Hit { x: messages.x, y: messages.y + (top - start) as u16, width: messages.width, height: (end - top) as u16, target: target.clone() });
            }
        }
    }
    *app.anchors.borrow_mut() = laid.anchors;
    let shown: Vec<Line<'static>> = laid.lines.into_iter().skip(start).take(height).collect();
    paint(frame, messages, shown);
    if bar {
        panels::scrollbar(frame, app, messages, scroll, total);
    }
    if scroll > 0 && messages.height > 0 {
        let label = format!(" ↓ {scroll} more ");
        let w = (text::width(&label) as u16).min(messages.width);
        let badge = Rect::new(messages.x + messages.width.saturating_sub(w + u16::from(bar) * 2), messages.y + messages.height - 1, w, 1);
        paint(frame, badge, vec![Line::from(Span::styled(label, Style::default().bg(theme.background_element.color()).fg(theme.text_muted.color())))]);
    }

    let bottom_area = Rect::new(inner.x, bottom_y, inner.width, bottom);
    match panel {
        Some(lines) => {
            // The newest rows stay visible when the panel is taller than the screen allows.
            let skip = lines.len().saturating_sub(usize::from(bottom));
            paint(frame, bottom_area, lines.into_iter().skip(skip).collect());
            if let Some((x, y)) = cursor.filter(|(_, y)| app.dialog.is_none() && usize::from(*y) >= skip) {
                frame.set_cursor_position(Position::new(bottom_area.x + x.min(bottom_area.width.saturating_sub(1)), bottom_area.y + y - skip as u16));
            }
        }
        None => prompt(frame, app, bottom_area, app.dialog.is_none()),
    }
    if let Some(side) = side {
        panels::sidebar(frame, app, view, side);
    }

    let directory = directory_label(app);
    let mut right: Vec<Span<'static>> = Vec::new();
    if asking {
        right.push(Span::styled(format!("△ {} Permission{}", app.permissions.len(), if app.permissions.len() > 1 { "s" } else { "" }), fg(theme.warning)));
    }
    let open = view.todos.iter().filter(|t| t.status != "completed" && t.status != "cancelled").count();
    if open > 0 {
        right.push(Span::styled(format!("  {open} todo{}", if open == 1 { "" } else { "s" }), fg(theme.text_muted)));
    }
    let room = width.saturating_sub(spans_width(&right) + 1);
    let mut spans = vec![Span::styled(truncate_left(&directory, room), fg(theme.text_muted))];
    let gap = width.saturating_sub(spans_width(&spans) + spans_width(&right));
    spans.push(Span::raw(" ".repeat(gap)));
    spans.extend(right);
    paint(frame, Rect::new(inner.x, footer_y, inner.width, 1.min(inner.height)), vec![Line::from(spans)]);
}

fn dialog_frame(frame: &mut Frame, app: &App, width: u16, lines: Vec<Line<'static>>, cursor: Option<(u16, u16)>) -> Rect {
    let theme = &app.theme;
    let area = frame.area();
    let width = width.min(area.width.saturating_sub(2));
    let height = (lines.len() as u16).min(area.height.saturating_sub(2));
    let rect = Rect::new(area.x + (area.width - width) / 2, area.y + (area.height.saturating_sub(height)) / 4, width, height);
    frame.render_widget(Clear, rect);
    frame.buffer_mut().set_style(rect, Style::default().bg(theme.background_panel.color()).fg(theme.text.color()));
    paint(frame, rect, lines);
    if let Some((x, y)) = cursor {
        frame.set_cursor_position(Position::new(rect.x + x.min(width.saturating_sub(1)), rect.y + y));
    }
    rect
}

fn dialog_title(title: &str, width: usize, theme: &Theme) -> Line<'static> {
    let gap = width.saturating_sub(text::width(title) + 4 + 3);
    Line::from(vec![
        Span::styled(format!("  {title}"), fg(theme.text).add_modifier(Modifier::BOLD)),
        Span::raw(" ".repeat(gap)),
        Span::styled("esc", fg(theme.text_muted)),
    ])
}

fn select_dialog(frame: &mut Frame, app: &App, select: &SelectDialog) {
    let theme = &app.theme;
    let area = frame.area();
    let wide = matches!(select.kind, SelectKind::Help | SelectKind::Sessions);
    let width = usize::from((if wide { 88 } else { 60 }).min(area.width.saturating_sub(2)));
    let muted = fg(theme.text_muted);
    let mut lines = vec![Line::default(), dialog_title(&select.title, width, theme), Line::default()];
    if select.kind == SelectKind::SetupBash {
        for row in text::wrap_plain(opencode_tools::mingit::OFFER, width.saturating_sub(4), fg(theme.text)) {
            lines.push(Line::from([vec![Span::raw("  ")], row].concat()));
        }
    } else {
        let query = if select.query.is_empty() { Span::styled("Search", muted) } else { Span::styled(select.query.clone(), fg(theme.text)) };
        lines.push(Line::from(vec![Span::raw("  "), query]));
    }
    lines.push(Line::default());

    // (line, index into `filtered` for selectable rows)
    let mut rows: Vec<(Line<'static>, Option<usize>)> = Vec::new();
    let mut category = None;
    for (position, &index) in select.filtered.iter().enumerate() {
        let item = &select.items[index];
        if select.show_categories() && !item.category.is_empty() && category != Some(&item.category) {
            if !rows.is_empty() {
                rows.push((Line::default(), None));
            }
            rows.push((Line::from(Span::styled(format!("   {}", item.category), fg(theme.accent).add_modifier(Modifier::BOLD))), None));
            category = Some(&item.category);
        }
        let selected = position == select.selected;
        let deleting = select.confirm_delete.as_deref() == Some(item.value.as_str());
        let base = match (deleting, selected) {
            (true, _) => Style::default().bg(theme.error.color()).fg(theme.selected_foreground(Some(theme.error)).color()),
            (false, true) => Style::default().bg(theme.primary.color()).fg(theme.selected_foreground(Some(theme.primary)).color()),
            (false, false) => fg(theme.text),
        };
        let dim = if selected || deleting { base } else { muted };
        let mark = if select.current.as_deref() == Some(item.value.as_str()) { "● " } else { "  " };
        let delete_key = if select.kind == SelectKind::Stash { "stash_delete" } else { "session_delete" };
        let title = if deleting { format!("Press {} again to confirm", app.keymap.display(delete_key)) } else { item.title.clone() };
        let room = width.saturating_sub(4);
        let detail = truncate(&item.description, room / 2);
        let title = truncate(&title, room.saturating_sub(text::width(&detail) + 2));
        let gap = room.saturating_sub(text::width(&title) + text::width(&detail));
        rows.push((
            padded(vec![Span::styled(format!(" {mark}"), base)], vec![Span::styled(title, base), Span::styled(" ".repeat(gap), base), Span::styled(detail, dim)], width, base),
            Some(position),
        ));
    }
    if rows.is_empty() {
        rows.push((Line::from(Span::styled("  No results found", muted)), None));
    }
    let footer: Vec<(&str, String)> = match select.kind {
        SelectKind::Sessions => {
            vec![("pin/unpin", app.keymap.display("session_pin_toggle")), ("delete", app.keymap.display("session_delete")), ("rename", app.keymap.display("session_rename"))]
        }
        SelectKind::Models => vec![("favorite", app.keymap.display("model_favorite_toggle"))],
        SelectKind::Stash => vec![("delete", app.keymap.display("stash_delete"))],
        _ => Vec::new(),
    };
    let reserved = lines.len() + 1 + if footer.is_empty() { 0 } else { 2 };
    let room = usize::from(area.height.saturating_sub(4)).saturating_sub(reserved).max(1);
    let focus = rows.iter().position(|(_, at)| *at == Some(select.selected)).unwrap_or(0);
    let start = focus.saturating_sub(room / 2).min(rows.len().saturating_sub(room));
    // (line index inside the dialog, position in the filtered list) of every selectable row drawn.
    let mut clickable: Vec<(usize, usize)> = Vec::new();
    for (line, position) in rows.into_iter().skip(start).take(room) {
        if let Some(position) = position {
            clickable.push((lines.len(), position));
        }
        lines.push(line);
    }
    if !footer.is_empty() {
        lines.push(Line::default());
        let mut spans = vec![Span::raw("  ")];
        for (label, shortcut) in footer {
            spans.push(Span::styled(format!("{label} "), fg(theme.text)));
            spans.push(Span::styled(format!("{shortcut}  "), muted));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::default());
    let cursor = (2 + text::width(&select.query) as u16, 3);
    let rect = dialog_frame(frame, app, width as u16, lines, Some(cursor));
    let mut hits = app.hits.borrow_mut();
    for (line, position) in clickable.into_iter().filter(|(line, _)| *line < usize::from(rect.height)) {
        hits.push(Hit { x: rect.x, y: rect.y + line as u16, width: rect.width, height: 1, target: HitTarget::DialogRow(position) });
    }
}

fn export_dialog(frame: &mut Frame, app: &App, export: &crate::dialog::ExportDialog) {
    let width = 60usize.min(usize::from(frame.area().width.saturating_sub(2)));
    let mut lines = vec![Line::default(), dialog_title("Export Options", width, &app.theme), Line::default()];
    let (body, cursor) = panels::export_lines(app, export, width);
    let offset = lines.len() as u16;
    lines.extend(body);
    dialog_frame(frame, app, width as u16, lines, cursor.map(|(x, y)| (x, y + offset)));
}

fn rename_dialog(frame: &mut Frame, app: &App, editor: &crate::editor::Editor) {
    let theme = &app.theme;
    let width = 60usize.min(usize::from(frame.area().width.saturating_sub(2)));
    let column = text::width(&editor.text[..editor.cursor]);
    let lines = vec![
        Line::default(),
        dialog_title("Rename Session", width, theme),
        Line::default(),
        Line::from(vec![Span::raw("  "), Span::styled(editor.text.clone(), fg(theme.text))]),
        Line::default(),
        Line::from(vec![Span::raw("  "), Span::styled("enter ", fg(theme.text)), Span::styled("submit", fg(theme.text_muted))]),
        Line::default(),
    ];
    dialog_frame(frame, app, width as u16, lines, Some((2 + column as u16, 3)));
}

// Rust build: read-only text (the `/btw` answer, `/stats`), scrolled with the arrow keys.
fn text_dialog(frame: &mut Frame, app: &App, dialog: &crate::dialog::TextDialog) {
    let theme = &app.theme;
    let area = frame.area();
    let width = 80usize.min(usize::from(area.width.saturating_sub(2)));
    let muted = fg(theme.text_muted);
    let mut lines = vec![Line::default(), dialog_title(&dialog.title, width, theme)];
    if !dialog.subtitle.is_empty() {
        lines.push(Line::from(vec![Span::raw("  "), Span::styled(truncate(&dialog.subtitle, width.saturating_sub(4)), muted)]));
    }
    lines.push(Line::default());
    let room = width.saturating_sub(4).max(1);
    let body: Vec<Line<'static>> = if dialog.markdown {
        crate::markdown::render(&dialog.body, room, theme)
    } else {
        dialog.body.lines().flat_map(|line| wrap_plain(line, room, fg(theme.text)).into_iter().map(Line::from).collect::<Vec<_>>()).collect()
    };
    let height = usize::from(area.height.saturating_sub(4)).saturating_sub(lines.len() + 3).max(1);
    let start = dialog.scroll.min(body.len().saturating_sub(1));
    for line in body.into_iter().skip(start).take(height) {
        let mut spans = vec![Span::raw("  ")];
        spans.extend(line.spans);
        lines.push(Line::from(spans));
    }
    lines.push(Line::default());
    lines.push(Line::from(vec![Span::raw("  "), Span::styled("c ", fg(theme.text)), Span::styled("copy  ", muted), Span::styled("esc ", fg(theme.text)), Span::styled("close", muted)]));
    lines.push(Line::default());
    dialog_frame(frame, app, width as u16, lines, None);
}

fn toast(frame: &mut Frame, app: &App) {
    let Some(toast) = &app.toast else { return };
    let theme = &app.theme;
    let area = frame.area();
    let width = 60.min(usize::from(area.width.saturating_sub(4)));
    if width < 8 {
        return;
    }
    let color = match toast.kind {
        ToastKind::Info => theme.info,
        ToastKind::Warning => theme.warning,
        ToastKind::Error => theme.error,
    };
    let fill = Style::default().bg(theme.background_panel.color());
    let edge = || Span::styled("┃", fg(color).bg(theme.background_panel.color()));
    let mut lines = Vec::new();
    for body in std::iter::once(Vec::new()).chain(wrap_plain(&toast.message, width - 6, fill.fg(theme.text.color()))).chain(std::iter::once(Vec::new())).take(8) {
        let mut line = padded(vec![edge(), Span::styled("  ", fill)], body, width - 1, fill);
        line.spans.push(edge());
        lines.push(line);
    }
    let rect = Rect::new(area.x + area.width - width as u16 - 2, area.y + 1, width as u16, (lines.len() as u16).min(area.height.saturating_sub(1)));
    frame.render_widget(Clear, rect);
    paint(frame, rect, lines);
}

pub fn render(frame: &mut Frame, app: &App) {
    let full = frame.area();
    app.hits.borrow_mut().clear();
    frame.buffer_mut().set_style(full, Style::default().bg(app.theme.background.color()).fg(app.theme.text.color()));
    if full.width < 10 || full.height < 6 {
        return;
    }
    // A docked which-key panel takes rows from the screen; an overlay covers them.
    let keys = panels::which_key_height(app, full);
    let keys_area = Rect::new(full.x, full.y + full.height - keys, full.width, keys);
    let docked = keys > 0 && app.which_key.layout == WhichKeyLayout::Dock;
    let area = if docked { Rect::new(full.x, full.y, full.width, full.height - keys) } else { full };
    match &app.route {
        Route::Home => home(frame, app, area),
        Route::Session(view) => session(frame, app, view, area),
    }
    if keys > 0 {
        panels::which_key(frame, app, keys_area);
    }
    match &app.dialog {
        Some(Dialog::Select(select)) => select_dialog(frame, app, select),
        Some(Dialog::Rename { editor, .. }) => rename_dialog(frame, app, editor),
        Some(Dialog::Export(export)) => export_dialog(frame, app, export),
        Some(Dialog::Text(text)) => text_dialog(frame, app, text),
        None => {}
    }
    toast(frame, app);
    crate::terminal::adapt(frame.buffer_mut(), app.caps);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::testing::{ctrl, open, press, session, type_text};

    // These tests check the original layout; the Claude look has its own tests at the end.
    fn app() -> (tempfile::TempDir, App) {
        let (dir, mut app) = crate::app::testing::app();
        app.kv.set("rs.style", json!("opencode"));
        (dir, app)
    }
    use crate::app::{Action, Effect, PermissionAsk};
    use crate::transcript::testing::conversation;
    use opencode_session::Reply;
    use opencode_tools::PermissionRequest;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyModifiers, MouseEvent, MouseEventKind};
    use serde_json::json;

    fn draw(app: &App, width: u16, height: u16) -> (Vec<String>, Terminal<TestBackend>) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rows = (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect::<String>().trim_end().to_string())
            .collect();
        (rows, terminal)
    }

    fn screen(app: &mut App, width: u16, height: u16) -> Vec<String> {
        app.update(Action::Resize(width, height));
        draw(app, width, height).0
    }

    fn has(rows: &[String], needle: &str) -> bool {
        rows.iter().any(|row| row.contains(needle))
    }

    // Rust build: the `/btw` answer and `/stats` dialog draw in both looks.
    #[test]
    fn text_dialog_in_both_looks() {
        for look in ["claude", "opencode"] {
            let (_dir, mut app) = crate::app::testing::app();
            app.kv.set("rs.style", json!(look));
            crate::app::testing::open(&mut app, crate::app::testing::session("ses_1", "Demo"), Vec::new());
            app.update(Action::Aside { question: "why?".into(), answer: Ok("Because **reasons**.\n\n- one\n- two".into()) });
            let rows = screen(&mut app, 90, 30);
            let all = rows.join("\n");
            assert!(has(&rows, "/btw") && has(&rows, "why?") && has(&rows, "Because reasons.") && has(&rows, "copy") && has(&rows, "close"), "{look}: {all}");
            app.update(Action::Stats(Ok("Jan – Nov 2026\n\n1.5K TOKENS".into())));
            let rows = screen(&mut app, 90, 30);
            assert!(has(&rows, "opencode / stats") && has(&rows, "1.5K TOKENS"), "{look}: {}", rows.join("\n"));
        }
    }

    #[test]
    fn claude_look_home_session_and_narrow_consoles() {
        let (_dir, mut app) = crate::app::testing::app();
        let rows = screen(&mut app, 100, 30);
        let all = rows.join("\n");
        // A welcome card, then the input between two rules with the agent and model under it.
        assert!(has(&rows, "│ OpenCode") && has(&rows, opencode_session::VERSION) && has(&rows, "│ build · Claude Sonnet 4.5 Anthropic"), "{all}");
        let input = rows.iter().position(|r| r.trim_start().starts_with("> Ask anything… \"")).expect(&all);
        assert!(rows[input - 1].trim().chars().all(|c| c == '─') && rows[input + 1].trim().chars().all(|c| c == '─'), "{all}");
        assert!(has(&rows, "  build · Claude Sonnet 4.5 Anthropic") && has(&rows, "ctrl+p commands"), "{all}");
        assert!(has(&rows, "Tip  Run /compact"), "{all}");
        // The placeholder is fainter than muted text.
        let (_, terminal) = draw(&app, 100, 30);
        let cell = &terminal.backend().buffer()[(rows[input].find('A').unwrap() as u16, input as u16)];
        assert_ne!(cell.fg, app.theme.text_muted.color());
        type_text(&mut app, "hello");
        let (rows, mut terminal) = draw(&app, 100, 30);
        let row = rows.iter().position(|r| r.trim_start().starts_with("> hello")).unwrap();
        let column = rows[row].chars().position(|c| c == '>').unwrap();
        terminal.backend_mut().assert_cursor_position(Position::new(column as u16 + 7, row as u16));

        // A session: the title after a dot, the prompt after `>`, the reply after a dot.
        open(&mut app, session("ses_1", "Listing files"), conversation());
        let rows = screen(&mut app, 100, 40);
        let all = rows.join("\n");
        assert!(has(&rows, "● Listing files") && has(&rows, " > list the files") && has(&rows, " ● Here is the plan:"), "{all}");
        assert!(!has(&rows, "▣"), "{all}");

        // An East Asian console host gets one-cell stand-ins for every ambiguous character.
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
        terminal
            .draw(|frame| {
                render(frame, &app);
                crate::glyphs::narrow_buffer(frame.buffer_mut());
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert!(buffer.content.iter().all(|cell| cell.symbol().chars().all(|c| !crate::glyphs::ambiguous(c))));
    }

    #[test]
    fn home_shows_logo_prompt_tip_and_footer() {
        let (_dir, mut app) = app();
        let rows = screen(&mut app, 100, 30);
        let all = rows.join("\n");
        assert!(has(&rows, "█▀▀█ █▀▀█ █▀▀█ █▀▀▄ █▀▀▀ █▀▀█ █▀▀█ █▀▀█"), "{all}");
        assert!(has(&rows, "Ask anything… \""), "{all}");
        assert!(has(&rows, "Build · Claude Sonnet 4.5 Anthropic"), "{all}");
        assert!(has(&rows, "tab agents  ctrl+p commands"), "{all}");
        assert!(has(&rows, "● Tip Run /compact to summarize long sessions near context limits"), "{all}");
        assert!(rows[28].ends_with(opencode_session::VERSION), "{all}");
        assert!(rows[28].contains("project"), "{all}");

        type_text(&mut app, "hello");
        let (rows, mut terminal) = draw(&app, 100, 30);
        let row = rows.iter().position(|r| r.contains("┃  hello")).unwrap();
        let column = rows[row].chars().position(|c| c == '┃').unwrap();
        terminal.backend_mut().assert_cursor_position(Position::new(column as u16 + 8, row as u16));
    }

    #[test]
    fn portable_bash_offer_shows_the_text_and_three_answers() {
        use crate::app::Effect;
        use crate::app::testing::press;
        use ratatui::crossterm::event::KeyCode;
        let (_dir, mut app) = app();
        app.offer_bash();
        let rows = screen(&mut app, 100, 30);
        let all = rows.join("\n");
        for needle in ["Portable bash", "Accept", "Not now", "Never"] {
            assert!(has(&rows, needle), "{needle}: {all}");
        }
        // The whole offer, wrapped to the dialog's width.
        let flat = all.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(flat.contains(opencode_tools::mingit::OFFER), "{all}");
        assert_eq!(press(&mut app, KeyCode::Enter), [Effect::SetupBash { accept: true }]);
        assert!(app.dialog.is_none());
        app.offer_bash();
        press(&mut app, KeyCode::Down);
        assert!(press(&mut app, KeyCode::Enter).is_empty());
        app.offer_bash();
        press(&mut app, KeyCode::Up);
        assert_eq!(press(&mut app, KeyCode::Enter), [Effect::SetupBash { accept: false }]);
    }

    #[test]
    fn session_renders_messages_prompt_and_usage() {
        let (_dir, mut app) = app();
        app.kv.set("rs.style", json!("opencode"));
        app.directory = "/project".into();
        let mut info = session("ses_1", "Listing files");
        info.cost = Some(0.0123);
        open(&mut app, info, conversation());
        let rows = screen(&mut app, 90, 60);
        let all = rows.join("\n");
        assert!(rows[0].contains("# Listing files"), "{all}");
        for needle in ["┃  list the files", "Here is the plan:", "- run ls", "→ Read src/main.rs [limit=20]", "$ ls -la", "Cargo.toml", "← Edit src/main.rs", "2 +     new();", "[•] ship it", "⚙ mystery [alpha=1, beta=two]", "▣  Build · Claude Sonnet 4.5 · 12.4s"] {
            assert!(has(&rows, needle), "missing {needle:?}\n{all}");
        }
        assert!(has(&rows, "1.2K (1%) · $0.01  ctrl+p commands"), "{all}");
        assert!(rows[59].contains("/project"), "{all}");
    }

    #[test]
    fn scrolling_moves_the_viewport() {
        let (_dir, mut app) = app();
        open(&mut app, session("ses_1", "Demo"), conversation());
        let bottom = screen(&mut app, 80, 20);
        assert!(has(&bottom, "▣  Build"));
        assert!(!has(&bottom, "list the files"));
        app.update(Action::Key(ratatui::crossterm::event::KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)));
        let up = screen(&mut app, 80, 20);
        assert!(!has(&up, "▣  Build"));
        assert!(has(&up, "↓"));
        app.run("messages_first");
        let top = screen(&mut app, 80, 20);
        assert!(has(&top, "list the files"));
        app.update(Action::Mouse(MouseEvent { kind: MouseEventKind::ScrollDown, column: 0, row: 0, modifiers: KeyModifiers::NONE }));
        let (height, total) = app.viewport.get();
        assert_eq!(app.session().unwrap().scroll, total - height - 3);
        app.run("messages_last");
        assert_eq!(app.session().unwrap().scroll, 0);
        app.run("messages_half_page_up");
        assert_eq!(app.session().unwrap().scroll, height / 2);
    }

    #[test]
    fn permission_prompt_replaces_the_input() {
        let (_dir, mut app) = app();
        open(&mut app, session("ses_1", "Demo"), conversation());
        app.update(Action::PermissionAsked(PermissionAsk {
            id: 7,
            session_id: "ses_1".into(),
            request: PermissionRequest { permission: "bash".into(), patterns: vec!["cargo *".into()], always: vec!["cargo *".into()], metadata: json!({"command": "cargo publish"}) },
        }));
        let rows = screen(&mut app, 80, 30);
        let all = rows.join("\n");
        assert!(has(&rows, "△ Permission required"), "{all}");
        assert!(has(&rows, "# Shell command"), "{all}");
        assert!(has(&rows, "$ cargo publish"), "{all}");
        assert!(has(&rows, "Allow once   Allow always   Reject"), "{all}");
        assert!(!has(&rows, "ctrl+p commands"), "{all}");
        assert!(rows[29].contains("△ 1 Permission"), "{all}");
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Enter);
        let rows = screen(&mut app, 80, 30);
        assert!(has(&rows, "This will allow bash until OpenCode is restarted."));
        assert!(has(&rows, "Confirm   Cancel"));
        assert_eq!(press(&mut app, KeyCode::Enter), vec![Effect::PermissionReply { id: 7, reply: Reply::Always }]);
        let rows = screen(&mut app, 80, 30);
        assert!(!has(&rows, "Permission required") && has(&rows, "ctrl+p commands"));
    }

    #[test]
    fn model_picker_renders_groups_and_filters() {
        let (_dir, mut app) = app();
        app.models.push_recent(&ModelKey::new("openai", "gpt-5"));
        ctrl(&mut app, 'x');
        press(&mut app, KeyCode::Char('m'));
        let rows = screen(&mut app, 100, 30);
        let all = rows.join("\n");
        for needle in ["Select model", "Search", "Recent", "Anthropic", "Claude Sonnet 4.5", "Claude Haiku 4.5", "OpenAI", "● GPT-5", "favorite ctrl+f"] {
            assert!(has(&rows, needle), "missing {needle:?}\n{all}");
        }
        type_text(&mut app, "haiku");
        let rows = screen(&mut app, 100, 30);
        assert!(has(&rows, "Claude Haiku 4.5") && !has(&rows, "● GPT-5") && !has(&rows, "Recent"), "{}", rows.join("\n"));
        type_text(&mut app, "zzz");
        assert!(has(&screen(&mut app, 100, 30), "No results found"));
    }

    #[test]
    fn other_dialogs_render() {
        let (_dir, mut app) = app();
        app.run("help_show");
        let rows = screen(&mut app, 100, 30);
        assert!(has(&rows, "Help") && has(&rows, "Exit the application") && has(&rows, "ctrl+c, ctrl+d, ctrl+x q"), "{}", rows.join("\n"));
        press(&mut app, KeyCode::Esc);

        ctrl(&mut app, 'p');
        let rows = screen(&mut app, 100, 30);
        assert!(has(&rows, "Commands") && has(&rows, "Switch model") && has(&rows, "ctrl+x m"), "{}", rows.join("\n"));
        press(&mut app, KeyCode::Esc);

        app.update(Action::SessionsListed(vec![session("ses_a", "Alpha"), session("ses_b", "Beta")]));
        ctrl(&mut app, 'd');
        let rows = screen(&mut app, 100, 30);
        assert!(has(&rows, "Sessions") && has(&rows, "Press ctrl+d again to confirm") && has(&rows, "Beta"), "{}", rows.join("\n"));
        ctrl(&mut app, 'r');
        let rows = screen(&mut app, 100, 30);
        assert!(has(&rows, "Rename Session") && has(&rows, "Alpha"), "{}", rows.join("\n"));
        press(&mut app, KeyCode::Esc);

        app.run("theme_list");
        let before = draw(&app, 100, 30).1.backend().buffer()[(0, 0)].bg;
        type_text(&mut app, "github");
        let rows = screen(&mut app, 100, 30);
        assert!(has(&rows, "Themes") && has(&rows, "github"));
        let after = draw(&app, 100, 30).1.backend().buffer()[(0, 0)].bg;
        assert_ne!(before, after);
    }

    #[test]
    fn autocomplete_popup_and_toast_and_busy_hint() {
        let (_dir, mut app) = app();
        open(&mut app, session("ses_1", "Demo"), Vec::new());
        type_text(&mut app, "/mo");
        let rows = screen(&mut app, 80, 24);
        assert!(has(&rows, "/models  Switch model"), "{}", rows.join("\n"));
        press(&mut app, KeyCode::Esc);
        app.editor.clear();
        type_text(&mut app, "go");
        press(&mut app, KeyCode::Enter);
        let rows = screen(&mut app, 80, 24);
        assert!(has(&rows, "esc interrupt") && has(&rows, "┃  go"), "{}", rows.join("\n"));
        press(&mut app, KeyCode::Esc);
        assert!(has(&screen(&mut app, 80, 24), "esc again to interrupt"));
        app.update(Action::Toast("Something failed".into(), ToastKind::Error));
        assert!(has(&screen(&mut app, 80, 24), "Something failed"));
    }

    #[test]
    fn other_agents_command_names_find_the_same_command() {
        let (_dir, mut app) = crate::app::testing::app();
        let first = |app: &mut App, typed: &str| {
            app.editor.clear();
            type_text(app, typed);
            let item = &app.autocomplete.as_ref().unwrap().items[0];
            (item.label.clone(), item.description.clone())
        };
        assert_eq!(first(&mut app, "/rewind"), ("/undo".to_string(), "Undo previous message · /rewind".to_string()));
        assert_eq!(first(&mut app, "/resum").0, "/sessions");
        assert_eq!(first(&mut app, "/model").0, "/models");
        // A command's own name is not annotated.
        assert_eq!(first(&mut app, "/undo").1, "Undo previous message");
    }

    #[test]
    fn autocomplete_scrolls_past_the_tenth_command() {
        let (_dir, mut app) = crate::app::testing::app();
        type_text(&mut app, "/");
        let total = app.autocomplete.as_ref().unwrap().items.len();
        assert!(total > crate::app::AUTOCOMPLETE_ROWS, "{total}");
        for _ in 0..crate::app::AUTOCOMPLETE_ROWS + 2 {
            press(&mut app, KeyCode::Down);
        }
        let ac = app.autocomplete.as_ref().unwrap();
        let wanted = ac.items[crate::app::AUTOCOMPLETE_ROWS + 2].label.clone();
        let rows = screen(&mut app, 100, 30);
        assert!(has(&rows, &wanted), "{wanted} not on screen:\n{}", rows.join("\n"));
        assert_eq!(rows.iter().filter(|r| r.trim_start().starts_with('/')).count(), crate::app::AUTOCOMPLETE_ROWS);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.autocomplete.as_ref().unwrap().selected, crate::app::AUTOCOMPLETE_ROWS + 1);
    }

    #[test]
    fn question_prompt_replaces_the_input() {
        use crate::question::QuestionAsk;
        use crate::question::testing::{info, request};
        let (_dir, mut app) = app();
        open(&mut app, session("ses_1", "Demo"), conversation());
        app.update(Action::QuestionAsked(QuestionAsk { id: 1, request: request(vec![info("Color", &["Red", "Blue"], false)]) }));
        let rows = screen(&mut app, 80, 40);
        let all = rows.join("\n");
        for needle in ["┃  Color?", "┃  1. Red", "┃     about Red", "┃  2. Blue", "┃  3. Type your own answer", "↑↓ select  enter submit  esc dismiss"] {
            assert!(has(&rows, needle), "missing {needle:?}\n{all}");
        }
        assert!(!has(&rows, "Confirm") && !has(&rows, "ctrl+p commands"), "{all}");
        press(&mut app, KeyCode::Char('3'));
        type_text(&mut app, "Teal");
        let (rows, mut terminal) = draw(&app, 80, 40);
        let row = rows.iter().position(|r| r.contains("┃     Teal")).unwrap_or_else(|| panic!("{}", rows.join("\n")));
        terminal.backend_mut().assert_cursor_position(Position::new(2 + 6 + 4, row as u16));
        press(&mut app, KeyCode::Enter);

        app.update(Action::QuestionAsked(QuestionAsk { id: 2, request: request(vec![info("Color", &["Red", "Blue"], true), info("Size", &["S"], false)]) }));
        press(&mut app, KeyCode::Enter);
        let rows = screen(&mut app, 80, 40);
        let all = rows.join("\n");
        for needle in [" Color   Size   Confirm", "Color? (select all that apply)", "1. [✓] Red", "2. [ ] Blue", "⇆ tab  ↑↓ select  enter toggle  esc dismiss"] {
            assert!(has(&rows, needle), "missing {needle:?}\n{all}");
        }
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Tab);
        let rows = screen(&mut app, 80, 40);
        let all = rows.join("\n");
        for needle in ["┃  Review", "Color: Red", "Size: (not answered)", "⇆ tab  enter submit  esc dismiss"] {
            assert!(has(&rows, needle), "missing {needle:?}\n{all}");
        }
    }

    #[test]
    fn sidebar_shows_context_todos_and_modified_files() {
        use opencode_storage::{FileDiff, SessionSummary, Todo};
        let (_dir, mut app) = app();
        let mut info = session("ses_1", "Listing files");
        info.cost = Some(0.5);
        info.summary = Some(SessionSummary {
            additions: 3.0,
            deletions: 1.0,
            files: 1.0,
            diffs: Some(vec![FileDiff { file: Some("src/main.rs".into()), patch: None, additions: 3.0, deletions: 1.0, status: None }]),
        });
        app.catalog.mcp = vec![("github".into(), true), ("old".into(), false)];
        app.update(Action::SessionLoaded {
            info: Box::new(info),
            messages: conversation(),
            todos: vec![Todo { content: "ship it".into(), status: "in_progress".into(), priority: "high".into() }, Todo { content: "done".into(), status: "completed".into(), priority: "low".into() }],
            open: true,
            settled: true,
        });
        // Narrow terminals hide it until asked; wide ones show it on their own.
        let rows = screen(&mut app, 100, 40);
        assert!(!has(&rows, "Modified Files"));
        let rows = screen(&mut app, 140, 40);
        let all = rows.join("\n");
        let side: Vec<String> = rows.iter().map(|row| row.chars().skip(98).collect::<String>().trim().to_string()).collect();
        for needle in ["Listing files", "Context", "1,234 tokens", "1% used", "$0.50 spent", "MCP", "• github Configured", "• old Disabled", "Todo", "[•] ship it", "[✓] done", "Modified Files", "+3 -1"] {
            assert!(side.iter().any(|row| row.contains(needle)), "missing {needle:?}\n{all}");
        }
        assert!(side.iter().any(|row| row.starts_with("src/main.rs") && row.ends_with("+3 -1")), "{all}");
        assert!(side[38].contains("• OpenCode") && side[38].contains(opencode_session::VERSION), "{all}");
        // The transcript and the prompt keep clear of it.
        assert!(rows.iter().all(|row| !row.chars().skip(98).collect::<String>().contains("list the files")), "{all}");
        assert_eq!(app.prompt_text_width(), 140 - 42 - 4 - 5);
        app.run("sidebar_toggle");
        assert!(!has(&screen(&mut app, 140, 40), "Modified Files"));
        app.run("sidebar_toggle");
        assert!(has(&screen(&mut app, 100, 40), "Modified Files"));
    }

    #[test]
    fn subagent_view_has_a_footer_instead_of_a_prompt() {
        use crate::app::Child;
        let (_dir, mut app) = app();
        let child = |id: &str, created: u64| {
            let mut info = session(id, "Find things (@general subagent)");
            info.parent_id = Some("ses_1".into());
            info.time.created = created;
            Child { info, tools: Vec::new(), status: Status::Idle, duration: 0 }
        };
        let children = vec![child("ses_a", 20), child("ses_b", 10)];
        open(&mut app, children[0].info.clone(), conversation());
        app.update(Action::ChildrenLoaded { session_id: "ses_a".into(), children, status: Status::Idle });
        let rows = screen(&mut app, 100, 30);
        let all = rows.join("\n");
        assert!(has(&rows, "┃  General (2 of 2) 1.2K (1%)"), "{all}");
        assert!(has(&rows, "Parent up  Prev left  Next right"), "{all}");
        assert!(!has(&rows, "ctrl+p commands") && !has(&rows, "Ask anything"), "{all}");
    }

    #[test]
    fn task_rows_follow_the_subagent_and_open_it_on_click() {
        use crate::app::{Child, ChildTool};
        use crate::transcript::testing::{assistant, tool_part, user};
        let (_dir, mut app) = app();
        app.kv.set("rs.style", json!("opencode"));
        let mut task =tool_part("msg_2", "c1", "task", json!({"description": "Find things", "subagent_type": "general"}), "", json!({"sessionId": "ses_c"}));
        let done = task.clone();
        if let opencode_storage::PartBody::Tool(tool) = &mut task.body {
            tool.state = opencode_storage::ToolState::Running(opencode_storage::ToolStateRunning {
                input: tool.state.input().clone(),
                title: None,
                metadata: json!({"sessionId": "ses_c"}).as_object().cloned(),
                time: opencode_storage::ToolTimeRunning { start: 1 },
            });
        }
        let mut info = session("ses_c", "Find things (@general subagent)");
        info.parent_id = Some("ses_1".into());
        let mut child = Child { info, tools: Vec::new(), status: Status::Busy, duration: 0 };
        open(&mut app, session("ses_1", "Demo"), vec![user("msg_1", "delegate"), assistant("msg_2", "msg_1", vec![task])]);
        app.update(Action::ChildrenLoaded { session_id: "ses_1".into(), children: vec![child.clone()], status: Status::Busy });
        let rows = screen(&mut app, 80, 24);
        assert!(has(&rows, "   │ General Task — Find things"), "{}", rows.join("\n"));
        child.tools = vec![
            ChildTool { id: "p1".into(), tool: "read".into(), status: "completed", title: Some("src/main.rs".into()) },
            ChildTool { id: "p2".into(), tool: "grep".into(), status: "running", title: None },
        ];
        app.update(Action::ChildrenLoaded { session_id: "ses_1".into(), children: vec![child.clone()], status: Status::Busy });
        let rows = screen(&mut app, 80, 24);
        assert!(has(&rows, "     ↳ Read src/main.rs"), "{}", rows.join("\n"));
        child.status = Status::Retry { attempt: 2, message: "overloaded".into(), next: 0 };
        app.update(Action::ChildrenLoaded { session_id: "ses_1".into(), children: vec![child.clone()], status: Status::Busy });
        assert!(has(&screen(&mut app, 80, 24), "     ↳ Retrying (attempt 2) · overloaded"));

        child.status = Status::Idle;
        child.duration = 4200;
        open(&mut app, session("ses_1", "Demo"), vec![user("msg_1", "delegate"), assistant("msg_2", "msg_1", vec![done])]);
        app.update(Action::ChildrenLoaded { session_id: "ses_1".into(), children: vec![child], status: Status::Idle });
        let rows = screen(&mut app, 80, 24);
        let all = rows.join("\n");
        assert!(has(&rows, "   ✓ General Task — Find things") && has(&rows, "     ↳ 2 toolcalls · 4.2s"), "{all}");
        let row = rows.iter().position(|r| r.contains("General Task")).unwrap() as u16;
        let click = Action::Mouse(MouseEvent { kind: MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left), column: 10, row: row + 1, modifiers: KeyModifiers::NONE });
        assert_eq!(app.update(click), vec![Effect::OpenSession("ses_c".into())]);
    }

    #[test]
    fn long_tool_output_expands_on_click_and_the_scrollbar_drags() {
        use crate::transcript::testing::{assistant, tool_part, user};
        let (_dir, mut app) = app();
        app.kv.set("rs.style", json!("opencode"));
        let output: String = (1..=30).map(|n| format!("line {n}\n")).collect();
        let bash = tool_part("msg_2", "c1", "bash", json!({"command": "seq 30"}), "", json!({"output": output}));
        open(&mut app, session("ses_1", "Demo"), vec![user("msg_1", "count"), assistant("msg_2", "msg_1", vec![bash])]);
        let rows = screen(&mut app, 80, 40);
        let all = rows.join("\n");
        assert!(has(&rows, "line 10") && !has(&rows, "line 11") && has(&rows, "Click to expand"), "{all}");
        let row = rows.iter().position(|r| r.contains("$ seq 30")).unwrap() as u16;
        let click = |column, row| Action::Mouse(MouseEvent { kind: MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left), column, row, modifiers: KeyModifiers::NONE });
        app.update(click(10, row));
        let rows = screen(&mut app, 80, 40);
        assert!(has(&rows, "line 30") && has(&rows, "Click to collapse"), "{}", rows.join("\n"));

        // The scrollbar only draws when asked for and when there is something to scroll.
        assert!(!has(&screen(&mut app, 80, 20), "█"));
        app.run("scrollbar_toggle");
        let rows = screen(&mut app, 80, 20);
        let thumb: Vec<usize> = rows.iter().enumerate().filter(|(_, r)| r.ends_with('█')).map(|(i, _)| i).collect();
        let (height, _) = app.viewport.get();
        assert!(!thumb.is_empty() && *thumb.last().unwrap() == 2 + height - 1, "{}", rows.join("\n"));
        app.update(click(77, 2));
        assert!(app.session().unwrap().scroll > 0);
        let (height, total) = app.viewport.get();
        assert_eq!(app.session().unwrap().scroll, total - height);
        app.update(Action::Mouse(MouseEvent { kind: MouseEventKind::Drag(ratatui::crossterm::event::MouseButton::Left), column: 77, row: 2 + height as u16 - 1, modifiers: KeyModifiers::NONE }));
        assert_eq!(app.session().unwrap().scroll, 0);
        let rows = screen(&mut app, 80, 20);
        assert!(rows[2].ends_with('│'), "{}", rows.join("\n"));
    }

    #[test]
    fn dialog_rows_take_clicks_and_export_and_which_key_render() {
        let (_dir, mut app) = app();
        open(&mut app, session("ses_1", "Demo"), conversation());
        app.update(Action::SessionsListed(vec![session("ses_a", "Alpha"), session("ses_b", "Beta")]));
        let rows = screen(&mut app, 100, 30);
        let row = rows.iter().position(|r| r.contains("Beta")).unwrap() as u16;
        let column = rows[row as usize].chars().position(|c| c == 'B').unwrap() as u16;
        let click = Action::Mouse(MouseEvent { kind: MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left), column, row, modifiers: KeyModifiers::NONE });
        assert_eq!(app.update(click), vec![]);
        assert!(matches!(&app.dialog, Some(Dialog::Select(select)) if select.selected == 1));
        screen(&mut app, 100, 30);
        let click = Action::Mouse(MouseEvent { kind: MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left), column, row, modifiers: KeyModifiers::NONE });
        assert_eq!(app.update(click), vec![Effect::OpenSession("ses_b".into())]);
        press(&mut app, KeyCode::Esc);

        app.run("session_export");
        let rows = screen(&mut app, 100, 30);
        let all = rows.join("\n");
        for needle in ["Export Options", "Filename:", "session-ses_1.md", "[x] Include thinking", "[x] Include tool details", "[x] Include assistant metadata", "[ ] Open without saving", "Press return to confirm, tab for options"] {
            assert!(has(&rows, needle), "missing {needle:?}\n{all}");
        }
        press(&mut app, KeyCode::Tab);
        assert!(has(&screen(&mut app, 100, 30), "Press space to toggle, return to confirm"));
        press(&mut app, KeyCode::Esc);

        app.update(Action::Key(ratatui::crossterm::event::KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL | KeyModifiers::ALT)));
        let rows = screen(&mut app, 100, 40);
        let all = rows.join("\n");
        for needle in [" System ", " Session ", " Agent ", "ctrl+p", "List available commands", "toggle ctrl+alt+k", "overlay ctrl+alt+shift+k"] {
            assert!(has(&rows, needle), "missing {needle:?}\n{all}");
        }
        // Docked, the panel takes its rows from the session; the footer moves up.
        assert!(rows[39].contains("toggle ctrl+alt+k") && rows[27].contains("project"), "{all}");
        // As an overlay it covers them instead.
        app.update(Action::Key(ratatui::crossterm::event::KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT)));
        let rows = screen(&mut app, 100, 40);
        assert!(rows[39].contains("dock ctrl+alt+shift+k") && !has(&rows, "ctrl+p commands"), "{}", rows.join("\n"));
    }

    #[test]
    fn shell_mode_selection_and_limited_terminals() {
        let (_dir, mut app) = app();
        press(&mut app, KeyCode::Char('!'));
        type_text(&mut app, "ls");
        let rows = screen(&mut app, 100, 30);
        let all = rows.join("\n");
        assert!(has(&rows, "┃  ls") && has(&rows, "Shell · Claude Sonnet 4.5") && has(&rows, "esc exit shell mode"), "{all}");
        press(&mut app, KeyCode::Esc);
        app.update(Action::Key(ratatui::crossterm::event::KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT)));
        let (rows, terminal) = draw(&app, 100, 30);
        let row = rows.iter().position(|r| r.contains("┃  ls")).unwrap();
        let column = rows[row].chars().position(|c| c == '┃').unwrap() as u16 + 3;
        let buffer = terminal.backend().buffer();
        assert_ne!(buffer[(column, row as u16)].bg, buffer[(column + 1, row as u16)].bg, "the selected cell is highlighted");

        // A console without unicode fonts or truecolor gets ASCII borders and palette colours.
        app.caps = crate::terminal::Caps { truecolor: false, unicode: false };
        let (rows, terminal) = draw(&app, 100, 30);
        assert!(has(&rows, "|  ls") && !has(&rows, "┃"), "{}", rows.join("\n"));
        assert!(terminal.backend().buffer().content.iter().all(|cell| !matches!(cell.fg, ratatui::style::Color::Rgb(..)) && !matches!(cell.bg, ratatui::style::Color::Rgb(..))));
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        let (_dir, mut app) = app();
        for (w, h) in [(1, 1), (9, 5), (12, 6), (20, 8), (30, 10)] {
            screen(&mut app, w, h);
        }
        open(&mut app, session("ses_1", "Demo"), conversation());
        app.run("model_list");
        app.update(Action::Toast("x".repeat(300), ToastKind::Info));
        for (w, h) in [(10, 6), (12, 7), (20, 8), (30, 10), (200, 60)] {
            screen(&mut app, w, h);
        }
        // Every panel at once: sidebar, scrollbar, which-key, then each prompt replacement.
        press(&mut app, KeyCode::Esc);
        app.run("sidebar_toggle");
        app.run("scrollbar_toggle");
        app.update(Action::Key(ratatui::crossterm::event::KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL | KeyModifiers::ALT)));
        let sizes = [(10, 6), (14, 7), (24, 9), (43, 12), (60, 14), (130, 20), (300, 90)];
        for (w, h) in sizes {
            screen(&mut app, w, h);
        }
        app.update(Action::QuestionAsked(crate::question::QuestionAsk {
            id: 1,
            request: crate::question::testing::request(vec![crate::question::testing::info("Color", &["Red", "Blue", "Green", "A very long option label that will not fit"], true)]),
        }));
        for (w, h) in sizes {
            screen(&mut app, w, h);
        }
        app.update(Action::PermissionAsked(PermissionAsk {
            id: 2,
            session_id: String::new(),
            request: PermissionRequest { permission: "edit".into(), patterns: vec![], always: vec![], metadata: json!({"filepath": "a.rs", "diff": "@@ -1 +1 @@\n-a\n+b\n".repeat(40)}) },
        }));
        ctrl(&mut app, 'f');
        for (w, h) in sizes {
            screen(&mut app, w, h);
        }
        app.run("session_export");
        for (w, h) in sizes {
            screen(&mut app, w, h);
        }
    }
}
