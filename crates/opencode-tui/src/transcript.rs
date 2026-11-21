use crate::app::{App, Child, HitTarget, Live, SessionView};
use crate::markdown;
use crate::state::ModelKey;
use crate::text::{self, duration, padded, titlecase, truncate, wrap_plain, wrap_spans};
use crate::theme::Theme;
use opencode_storage::{AssistantError, AssistantMessage, Message, Part, PartBody, ReasoningPart, ToolPart, ToolState};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

type Spans = Vec<Span<'static>>;

#[derive(Clone)]
enum Rendered {
    Hidden,
    // Rows that sit flush under a previous inline row.
    Inline(Vec<Line<'static>>),
    Block(Vec<Line<'static>>),
}

// Rendered lines per stored part, keyed by part id and reused while the part and the layout are unchanged.
#[derive(Default)]
pub struct PartCache {
    entries: HashMap<String, (u64, Rendered)>,
}

impl PartCache {
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

fn fingerprint(app: &App, part: &Part, width: usize) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (width, &app.theme_name, app.mode.key(), app.show_thinking, app.show_details, app.conceal).hash(&mut hasher);
    (app.expanded.contains(&part.id), app.kv.get_bool("generic_tool_output_visibility", false), app.claude_look(), app.console_host, app.narrow_glyphs).hash(&mut hasher);
    match &part.body {
        PartBody::Text(t) => t.text.len().hash(&mut hasher),
        PartBody::Reasoning(r) => (r.text.len(), r.time.end).hash(&mut hasher),
        PartBody::Tool(tool) => {
            tool.state.status().hash(&mut hasher);
            // A task row follows its subagent's progress.
            if let Some(child) = subagent(app, tool) {
                (child.tools.len(), child.tools.iter().map(|t| (t.status, t.title.as_deref())).collect::<Vec<_>>(), format!("{:?}", child.status), child.duration).hash(&mut hasher);
            }
            match &tool.state {
                ToolState::Completed(done) => (done.output.len(), done.metadata.len(), done.time.end).hash(&mut hasher),
                ToolState::Error(failed) => failed.error.len().hash(&mut hasher),
                ToolState::Running(running) => {
                    running.metadata.as_ref().map(|m| serde_json::to_string(m).map_or(0, |s| s.len())).hash(&mut hasher);
                    // The breathing dot of a running call changes with the frame.
                    (app.claude_look() && app.animations()).then_some(app.frame % 16).hash(&mut hasher);
                }
                ToolState::Pending(pending) => pending.raw.len().hash(&mut hasher),
            }
        }
        _ => {}
    }
    hasher.finish()
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            if c != '\r' {
                out.push(c);
            }
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

pub fn format_path(app: &App, path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let unified = |p: &str| p.replace('\\', "/");
    let (target, cwd, home) = (unified(path), unified(&app.directory.to_string_lossy()), unified(&app.home.to_string_lossy()));
    if target == cwd {
        return ".".into();
    }
    if let Some(rest) = target.strip_prefix(&format!("{}/", cwd.trim_end_matches('/'))) {
        return rest.to_string();
    }
    if let Some(rest) = target.strip_prefix(&format!("{}/", home.trim_end_matches('/'))).filter(|_| !home.is_empty()) {
        return format!("~/{rest}");
    }
    path.to_string()
}

// `input()`: primitive tool arguments as `[key=value, ...]`.
fn arguments(input: &Map<String, Value>, omit: &[&str]) -> String {
    let parts: Vec<String> = input
        .iter()
        .filter(|(key, _)| !omit.contains(&key.as_str()))
        .filter_map(|(key, value)| match value {
            Value::String(s) => Some(format!("{key}={s}")),
            Value::Number(n) => Some(format!("{key}={n}")),
            Value::Bool(b) => Some(format!("{key}={b}")),
            _ => None,
        })
        .collect();
    if parts.is_empty() { String::new() } else { format!("[{}]", parts.join(", ")) }
}

fn string<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    map.get(key).and_then(Value::as_str)
}

fn block(theme: &Theme, width: usize, border: Option<Style>, title: Option<String>, body: Vec<Spans>) -> Vec<Line<'static>> {
    let fill = Style::default().bg(theme.background_panel.color());
    let edge = || match border {
        Some(style) => Span::styled("┃", style.bg(theme.background_panel.color())),
        None => Span::styled(" ", fill),
    };
    let row = |content: Spans| {
        let content = content.into_iter().map(|span| Span::styled(span.content, fill.patch(span.style))).collect();
        padded(vec![edge(), Span::styled("  ", fill)], content, width, fill)
    };
    let mut lines = vec![row(Vec::new())];
    if let Some(title) = title {
        lines.push(row(vec![Span::styled(format!("   {title}"), Style::default().fg(theme.text_muted.color()))]));
        if !body.is_empty() {
            lines.push(row(Vec::new()));
        }
    }
    lines.extend(body.into_iter().map(row));
    lines.push(row(Vec::new()));
    lines
}

// Unified diff rows with line numbers and added/removed backgrounds.
pub fn diff_rows(diff: &str, width: usize, theme: &Theme) -> Vec<Spans> {
    let mut rows = Vec::new();
    let (mut old, mut new) = (0u64, 0u64);
    for line in diff.lines() {
        if line.starts_with("Index:") || line.starts_with("====") || line.starts_with("--- ") || line.starts_with("+++ ") || line.starts_with('\\') {
            continue;
        }
        if let Some(header) = line.strip_prefix("@@") {
            let mut numbers = header.split_whitespace().filter_map(|token| token.trim_start_matches(['-', '+']).split(',').next()?.parse::<u64>().ok());
            old = numbers.next().unwrap_or(1);
            new = numbers.next().unwrap_or(1);
            continue;
        }
        let (sign, content) = match line.chars().next() {
            Some('+') => ('+', &line[1..]),
            Some('-') => ('-', &line[1..]),
            Some(' ') => (' ', &line[1..]),
            _ => (' ', line),
        };
        let (number, number_bg, bg, sign_color) = match sign {
            '+' => (new, theme.diff_added_line_number_bg, theme.diff_added_bg, theme.diff_highlight_added),
            '-' => (old, theme.diff_removed_line_number_bg, theme.diff_removed_bg, theme.diff_highlight_removed),
            _ => (new, theme.diff_context_bg, theme.diff_context_bg, theme.diff_context),
        };
        match sign {
            '+' => new += 1,
            '-' => old += 1,
            _ => {
                old += 1;
                new += 1;
            }
        }
        let gutter = format!("{number:>4} ");
        let room = width.saturating_sub(text::width(&gutter) + 2);
        let shown = truncate(&content.replace('\t', "    "), room);
        let pad = room.saturating_sub(text::width(&shown));
        rows.push(vec![
            Span::styled(gutter, Style::default().fg(theme.diff_line_number.color()).bg(number_bg.color())),
            Span::styled(format!("{sign} "), Style::default().fg(sign_color.color()).bg(bg.color())),
            Span::styled(format!("{shown}{}", " ".repeat(pad)), Style::default().fg(theme.text.color()).bg(bg.color())),
        ]);
    }
    rows
}

fn todo_rows(todos: Option<&Value>, theme: &Theme, width: usize) -> Vec<Spans> {
    let mut rows = Vec::new();
    for todo in todos.and_then(Value::as_array).into_iter().flatten() {
        let (Some(status), Some(content)) = (todo.get("status").and_then(Value::as_str), todo.get("content").and_then(Value::as_str)) else { continue };
        let style = Style::default().fg(if status == "in_progress" { theme.warning.color() } else { theme.text_muted.color() });
        let mark = match status {
            "completed" => "✓",
            "in_progress" => "•",
            _ => " ",
        };
        for (index, body) in wrap_plain(content, width.saturating_sub(4), style).into_iter().enumerate() {
            let mut spans = vec![Span::styled(if index == 0 { format!("[{mark}] ") } else { "    ".to_string() }, style)];
            spans.extend(body);
            rows.push(spans);
        }
    }
    rows
}

fn numbered_code(code: &str, path: &str, theme: &Theme, width: usize) -> Vec<Spans> {
    let lines = markdown::highlight(code, &markdown::language_for_path(path), theme);
    let digits = lines.len().to_string().len().max(3);
    lines
        .into_iter()
        .enumerate()
        .map(|(index, spans)| {
            let mut row = vec![Span::styled(format!("{:>digits$} ", index + 1), Style::default().fg(theme.text_muted.color()))];
            let mut used = digits + 1;
            for span in spans {
                let room = width.saturating_sub(used);
                if room == 0 {
                    break;
                }
                let shown = truncate(&span.content.replace('\t', "    "), room);
                used += text::width(&shown);
                row.push(Span::styled(shown, span.style));
            }
            row
        })
        .collect()
}

// Rust build, Claude-style tool rows (the default; `rs.style: "opencode"` in kv.json restores the original's):
// a status dot with `Name(arguments)`, and the result indented under an elbow.
#[derive(Clone, Copy, PartialEq)]
enum Outcome {
    Running,
    Done,
    Failed,
    Denied,
}

fn claude(app: &App, outcome: Outcome, name: &str, args: &str, result: Vec<Spans>, width: usize) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let dot = match outcome {
        Outcome::Done => theme.success,
        Outcome::Failed => theme.error,
        // A running call breathes between muted and the accent colour.
        Outcome::Running if app.animations() => {
            let phase = (app.frame % 16) as f64 / 8.0;
            crate::theme::tint(theme.text_muted, theme.primary, if phase > 1.0 { 2.0 - phase } else { phase })
        }
        Outcome::Running | Outcome::Denied => theme.text_muted,
    };
    let mut strong = Style::default().fg(theme.text.color()).add_modifier(Modifier::BOLD);
    let mut quiet = Style::default().fg(theme.text_muted.color());
    if outcome == Outcome::Denied {
        strong = strong.add_modifier(Modifier::CROSSED_OUT);
        quiet = quiet.add_modifier(Modifier::CROSSED_OUT);
    }
    let call = if args.is_empty() { vec![Span::styled(name.to_string(), strong)] } else {
        vec![Span::styled(name.to_string(), strong), Span::styled(format!("({args})"), quiet)]
    };
    let mut lines: Vec<Line<'static>> = wrap_spans(&call, width.saturating_sub(4))
        .into_iter()
        .enumerate()
        .map(|(index, spans)| {
            let lead = if index == 0 { Span::styled("● ", Style::default().fg(dot.color())) } else { Span::raw("  ") };
            Line::from(std::iter::once(Span::raw(" ")).chain(std::iter::once(lead)).chain(spans).collect::<Vec<_>>())
        })
        .collect();
    let elbow = if app.narrow_glyphs { "|" } else if app.console_host { "└" } else { "⎿" };
    for (index, row) in result.into_iter().enumerate() {
        let lead = if index == 0 { format!("   {elbow}  ") } else { "      ".to_string() };
        lines.push(Line::from(std::iter::once(Span::styled(lead, quiet)).chain(row).collect::<Vec<_>>()));
    }
    lines
}

// The shell tool's name as the model sees it in the agent profile: Bash, PowerShell or Cmd.
static SHELL_NAME: std::sync::LazyLock<&'static str> =
    std::sync::LazyLock::new(|| opencode_tools::shell::tool_name(&opencode_tools::shell::preferred(None)));

// "Read path [offset=2]" -> ("Read", "path [offset=2]"); a trailing "(3 matches)" becomes the result line.
fn claude_call(body: &str) -> (String, String, Option<String>) {
    let first = body.lines().next().unwrap_or_default();
    let (name, mut args) = first.split_once(' ').map_or((first.to_string(), String::new()), |(n, a)| (n.to_string(), a.to_string()));
    let mut found = None;
    let counted = args.ends_with(" match)") || args.ends_with(" matches)");
    if counted && let Some(start) = args.rfind(" (") {
        found = Some(format!("Found {}", &args[start + 2..args.len() - 1]));
        args.truncate(start);
    }
    // `arguments()` brackets its list: "[a=1]" alone, or "path [a=1]" after a path.
    if let Some(inner) = args.strip_suffix(']') {
        args = match inner.strip_prefix('[') {
            Some(list) => list.to_string(),
            None => inner.replacen(" [", ", ", 1),
        };
    }
    (name, args, found)
}

fn inline(theme: &Theme, icon: &str, body: String, style: Style, width: usize) -> Vec<Line<'static>> {
    let _ = theme;
    wrap_plain(&body, width.saturating_sub(5), style)
        .into_iter()
        .enumerate()
        .map(|(index, spans)| {
            let mut line = vec![Span::styled(if index == 0 { format!("   {icon} ") } else { "     ".to_string() }, style)];
            line.extend(spans);
            Line::from(line)
        })
        .collect()
}

fn metadata_of(tool: &ToolPart) -> Option<&Map<String, Value>> {
    match &tool.state {
        ToolState::Completed(done) => Some(&done.metadata),
        ToolState::Running(running) => running.metadata.as_ref(),
        ToolState::Error(failed) => failed.metadata.as_ref(),
        ToolState::Pending(_) => None,
    }
}

// The subagent session a `task` call started, when the view knows it.
fn subagent<'a>(app: &'a App, tool: &ToolPart) -> Option<&'a Child> {
    if tool.tool != "task" {
        return None;
    }
    app.session()?.child(metadata_of(tool)?.get("sessionId")?.as_str()?)
}

// What a click on a tool part does, if anything.
pub fn click_target(part: &Part) -> Option<HitTarget> {
    let PartBody::Tool(tool) = &part.body else { return None };
    let metadata = metadata_of(tool);
    match tool.tool.as_str() {
        "task" => Some(HitTarget::Session(metadata?.get("sessionId")?.as_str()?.to_string())),
        "bash" => {
            let output = strip_ansi(metadata?.get("output")?.as_str()?.trim());
            collapse(&output, BASH_LINES, usize::MAX).1.then(|| HitTarget::Tool(part.id.clone()))
        }
        _ => None,
    }
}

const BASH_LINES: usize = 10;

// `collapseToolOutput`
pub fn collapse(output: &str, max_lines: usize, max_chars: usize) -> (String, bool) {
    let lines: Vec<&str> = output.split('\n').collect();
    if lines.len() <= max_lines && output.chars().count() <= max_chars {
        return (output.to_string(), false);
    }
    let preview = lines[..max_lines.min(lines.len())].join("\n");
    if preview.chars().count() > max_chars {
        return (preview.chars().take(max_chars.saturating_sub(1)).collect::<String>() + "…", true);
    }
    (format!("{preview}\n…"), true)
}

fn tool(app: &App, id: &str, tool: &ToolPart, width: usize) -> Rendered {
    let theme = &app.theme;
    let input = tool.state.input();
    let empty = Map::new();
    let (metadata, output) = match &tool.state {
        ToolState::Completed(done) => (&done.metadata, Some(done.output.as_str())),
        ToolState::Running(running) => (running.metadata.as_ref().unwrap_or(&empty), None),
        ToolState::Error(failed) => (failed.metadata.as_ref().unwrap_or(&empty), None),
        ToolState::Pending(_) => (&empty, None),
    };
    if !app.show_details && matches!(tool.state, ToolState::Completed(_)) {
        return Rendered::Hidden;
    }
    let inner = width.saturating_sub(4);
    let path = || format_path(app, string(input, "filePath").unwrap_or_default());
    let error = match &tool.state {
        ToolState::Error(failed) => Some(failed.error.as_str()),
        _ => None,
    };
    let denied = error.is_some_and(|e| ["QuestionRejectedError", "rejected permission", "specified a rule", "user dismissed"].iter().any(|needle| e.contains(needle)));
    let failed = error.is_some() && !denied;
    let claude_style = app.claude_look();
    let outcome = match &tool.state {
        _ if denied => Outcome::Denied,
        _ if failed => Outcome::Failed,
        ToolState::Completed(_) => Outcome::Done,
        _ => Outcome::Running,
    };
    let muted = Style::default().fg(theme.text_muted.color());
    let changes = |diff: &str| {
        let count = |sign: char, header: &str| diff.lines().filter(|l| l.starts_with(sign) && !l.starts_with(header)).count();
        let (added, removed) = (count('+', "+++"), count('-', "---"));
        let plural = |n: usize| if n == 1 { "" } else { "s" };
        vec![Span::styled(format!("Added {added} line{}, removed {removed} line{}", plural(added), plural(removed)), muted)]
    };

    // (icon, pending label, whether enough input arrived to describe the call, description)
    let (icon, pending, complete, body): (&str, &str, bool, String) = match tool.tool.as_str() {
        "bash" => {
            let command = string(input, "command");
            if let Some(shown) = string(metadata, "output") {
                let cleaned = strip_ansi(shown.trim());
                let text_style = Style::default().fg(theme.text.color());
                if claude_style {
                    let mut rows: Vec<Spans> = Vec::new();
                    let expanded = app.expanded.contains(id);
                    let (preview, overflow) = collapse(&cleaned, BASH_LINES, BASH_LINES * inner.max(20));
                    let shown = if expanded { cleaned.as_str() } else { preview.trim_end_matches('…').trim_end() };
                    rows.extend(shown.lines().flat_map(|line| wrap_plain(line, inner.saturating_sub(6), text_style)));
                    if cleaned.is_empty() {
                        rows.push(vec![Span::styled("(No output)", muted)]);
                    }
                    if overflow {
                        let hidden = cleaned.lines().count().saturating_sub(shown.lines().count());
                        let hint = if expanded { "Click to collapse".to_string() } else { format!("… +{hidden} lines (click to expand)") };
                        rows.push(vec![Span::styled(hint, muted)]);
                    }
                    let mut args = command.unwrap_or_default().to_string();
                    if let Some(dir) = string(input, "workdir").map(|w| format_path(app, w)).filter(|w| !w.is_empty() && w != ".") {
                        args.push_str(&format!(" in {dir}"));
                    }
                    return Rendered::Block(claude(app, outcome, &SHELL_NAME, &args, rows, width));
                }
                let mut rows = wrap_plain(&format!("$ {}", command.unwrap_or_default()), inner, text_style);
                if !cleaned.is_empty() {
                    rows.push(Vec::new());
                    let expanded = app.expanded.contains(id);
                    let (preview, overflow) = collapse(&cleaned, BASH_LINES, BASH_LINES * inner.max(20));
                    let shown = if expanded { cleaned.as_str() } else { preview.as_str() };
                    rows.extend(shown.lines().flat_map(|line| wrap_plain(line, inner, text_style)));
                    if overflow {
                        rows.push(Vec::new());
                        rows.push(vec![Span::styled(if expanded { "Click to collapse" } else { "Click to expand" }, Style::default().fg(theme.text_muted.color()))]);
                    }
                }
                let workdir = string(input, "workdir").map(|w| format_path(app, w)).filter(|w| !w.is_empty() && w != ".");
                return Rendered::Block(block(theme, width, None, workdir.map(|w| format!("# Running in {w}")), rows));
            }
            ("$", "Writing command…", command.is_some(), command.unwrap_or_default().to_string())
        }
        "read" => ("→", "Reading file…", string(input, "filePath").is_some(), format!("Read {} {}", path(), arguments(input, &["filePath"])).trim_end().to_string()),
        "glob" | "grep" => {
            let pattern = string(input, "pattern");
            let (label, count) = if tool.tool == "glob" { ("Glob", metadata.get("count")) } else { ("Grep", metadata.get("matches")) };
            let mut body = format!("{label} \"{}\"", pattern.unwrap_or_default());
            if let Some(dir) = string(input, "path") {
                body.push_str(&format!(" in {}", format_path(app, dir)));
            }
            if let Some(n) = count.and_then(Value::as_u64).filter(|n| *n > 0) {
                body.push_str(&format!(" ({n} {})", if n == 1 { "match" } else { "matches" }));
            }
            ("✱", if tool.tool == "glob" { "Finding files…" } else { "Searching content…" }, pattern.is_some(), body)
        }
        "webfetch" => ("%", "Fetching from the web…", string(input, "url").is_some(), format!("WebFetch {}", string(input, "url").unwrap_or_default())),
        "websearch" => ("◈", "Searching web…", string(input, "query").is_some(), format!("WebSearch \"{}\"", string(input, "query").unwrap_or_default())),
        "write" => {
            if metadata.contains_key("diagnostics") {
                let code = string(input, "content").unwrap_or_default();
                let rows = numbered_code(code, string(input, "filePath").unwrap_or_default(), theme, inner);
                if claude_style {
                    let lines = code.lines().count();
                    let summary = vec![Span::styled(format!("Wrote {lines} line{}", if lines == 1 { "" } else { "s" }), muted)];
                    return Rendered::Block(claude(app, outcome, "Write", &path(), std::iter::once(summary).chain(rows).collect(), width));
                }
                return Rendered::Block(block(theme, width, None, Some(format!("# Wrote {}", path())), rows));
            }
            ("←", "Preparing write…", string(input, "filePath").is_some(), format!("Write {}", path()))
        }
        "edit" => {
            if let Some(diff) = string(metadata, "diff") {
                if claude_style {
                    let rows = std::iter::once(changes(diff)).chain(diff_rows(diff, inner.saturating_sub(6), theme)).collect();
                    return Rendered::Block(claude(app, outcome, "Update", &path(), rows, width));
                }
                return Rendered::Block(block(theme, width, None, Some(format!("← Edit {}", path())), diff_rows(diff, inner, theme)));
            }
            let replace_all = input.get("replaceAll").filter(|v| v.is_boolean()).map(|v| format!(" [replaceAll={v}]")).unwrap_or_default();
            ("←", "Preparing edit…", string(input, "filePath").is_some(), format!("Edit {}{replace_all}", path()))
        }
        "apply_patch" => {
            let mut lines = Vec::new();
            for file in metadata.get("files").and_then(Value::as_array).into_iter().flatten() {
                let Some(file) = file.as_object() else { continue };
                let (Some(kind), Some(relative), Some(full), Some(patch)) = (string(file, "type"), string(file, "relativePath"), string(file, "filePath"), string(file, "patch")) else {
                    continue;
                };
                let deletions = file.get("deletions").and_then(Value::as_u64).unwrap_or(0);
                let title = match kind {
                    "delete" => format!("# Deleted {relative}"),
                    "add" => format!("# Created {relative}"),
                    "move" => format!("# Moved {} → {relative}", format_path(app, full)),
                    _ => format!("← Patched {relative}"),
                };
                let rows = if kind == "delete" {
                    vec![vec![Span::styled(format!("-{deletions} line{}", if deletions == 1 { "" } else { "s" }), Style::default().fg(theme.diff_removed.color()))]]
                } else if claude_style {
                    std::iter::once(changes(patch)).chain(diff_rows(patch, inner.saturating_sub(6), theme)).collect()
                } else {
                    diff_rows(patch, inner, theme)
                };
                if !lines.is_empty() {
                    lines.push(Line::default());
                }
                if claude_style {
                    let (name, args) = match kind {
                        "delete" => ("Delete", relative.to_string()),
                        "add" => ("Create", relative.to_string()),
                        "move" => ("Move", format!("{} → {relative}", format_path(app, full))),
                        _ => ("Update", relative.to_string()),
                    };
                    lines.extend(claude(app, outcome, name, &args, rows, width));
                    continue;
                }
                lines.extend(block(theme, width, None, Some(title), rows));
            }
            if !lines.is_empty() {
                return Rendered::Block(lines);
            }
            ("%", "Preparing patch…", false, if failed { "Patch failed".into() } else { "Patch".into() })
        }
        "todowrite" => {
            if metadata.get("todos").and_then(Value::as_array).is_some_and(|t| !t.is_empty()) {
                if claude_style {
                    return Rendered::Block(claude(app, outcome, "Update Todos", "", todo_rows(input.get("todos"), theme, inner.saturating_sub(6)), width));
                }
                return Rendered::Block(block(theme, width, None, Some("# Todos".into()), todo_rows(input.get("todos"), theme, inner)));
            }
            ("⚙", "Updating todos…", false, if failed { "Todo update failed".into() } else { "Updating todos…".into() })
        }
        "skill" => ("→", "Loading skill…", string(input, "name").is_some(), format!("Skill \"{}\"", string(input, "name").unwrap_or_default())),
        "question" => {
            let questions: Vec<&str> = input.get("questions").and_then(Value::as_array).into_iter().flatten().filter_map(|q| q.get("question").and_then(Value::as_str)).collect();
            if let Some(answers) = metadata.get("answers").and_then(Value::as_array) {
                let mut rows: Vec<Spans> = Vec::new();
                for (index, question) in questions.iter().enumerate() {
                    if index > 0 {
                        rows.push(Vec::new());
                    }
                    let picked: Vec<&str> = answers.get(index).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
                    rows.extend(wrap_plain(question, inner, Style::default().fg(theme.text_muted.color())));
                    rows.extend(wrap_plain(&if picked.is_empty() { "(no answer)".to_string() } else { picked.join(", ") }, inner, Style::default().fg(theme.text.color())));
                }
                if claude_style {
                    return Rendered::Block(claude(app, outcome, "Questions", "", rows, width));
                }
                return Rendered::Block(block(theme, width, None, Some("# Questions".into()), rows));
            }
            let count = questions.len();
            ("→", "Asking questions…", count > 0, format!("Asked {count} question{}", if count == 1 { "" } else { "s" }))
        }
        "task" => {
            let description = string(input, "description");
            let background = metadata.get("background").and_then(Value::as_bool) == Some(true);
            let agent = titlecase(string(input, "subagent_type").unwrap_or("General"));
            let mut body = format!("{agent} Task{} — {}", if background { " (background)" } else { "" }, description.unwrap_or_default());
            let child = subagent(app, tool);
            let running = matches!(tool.state, ToolState::Running(_)) || (background && child.is_some_and(|c| c.status != opencode_session::Status::Idle));
            let calls = child.map_or(0, |c| c.tools.len());
            let count = |n: usize| format!("{n} toolcall{}", if n == 1 { "" } else { "s" });
            match child.map(|c| &c.status) {
                Some(opencode_session::Status::Retry { attempt, message, .. }) if running => body.push_str(&format!("\n↳ Retrying (attempt {attempt}) · {}", truncate(message, 80))),
                _ if running && calls > 0 => {
                    let current = child.and_then(|c| c.tools.iter().rev().find(|t| matches!(t.status, "running" | "completed") && t.title.is_some()));
                    match current {
                        Some(current) => body.push_str(&format!("\n↳ {} {}", titlecase(&current.tool), current.title.as_deref().unwrap_or_default())),
                        None => body.push_str(&format!("\n↳ {}", count(calls))),
                    }
                }
                _ => {}
            }
            if !running && matches!(tool.state, ToolState::Completed(_)) {
                let took = duration(child.map_or(0, |c| c.duration));
                body.push_str(&if calls == 0 { format!("\n↳ {took}") } else { format!("\n↳ {} · {took}", count(calls)) });
            }
            // The original animates a spinner in place of the bar while the subagent runs.
            let icon = if matches!(tool.state, ToolState::Completed(_)) { "✓" } else { "│" };
            (icon, "Delegating…", description.is_some(), body)
        }
        name => {
            if let Some(text) = output.map(str::trim).filter(|o| !o.is_empty() && app.kv.get_bool("generic_tool_output_visibility", false)) {
                let style = Style::default().fg(theme.text.color());
                let (preview, _) = collapse(text, 3, 3 * inner.max(20));
                let rows: Vec<Spans> = preview.lines().flat_map(|line| wrap_plain(line, inner, style)).collect();
                if claude_style {
                    return Rendered::Block(claude(app, outcome, name, &arguments(input, &[]), rows, width));
                }
                return Rendered::Block(block(theme, width, None, Some(format!("# {name} {}", arguments(input, &[])).trim_end().to_string()), rows));
            }
            ("⚙", "Writing command…", true, format!("{name} {}", arguments(input, &[])).trim_end().to_string())
        }
    };

    if claude_style {
        if !(complete || failed) {
            return Rendered::Block(claude(app, Outcome::Running, pending, "", Vec::new(), width));
        }
        let (name, args, found) = claude_call(&body);
        let mut rows: Vec<Spans> = found.into_iter().map(|line| vec![Span::styled(line, muted)]).collect();
        rows.extend(body.lines().skip(1).map(|line| vec![Span::styled(line.trim_start_matches("↳ ").to_string(), muted)]));
        if let Some(message) = error.filter(|_| failed) {
            rows.extend(wrap_plain(message.lines().next().unwrap_or_default(), width.saturating_sub(8), Style::default().fg(theme.error.color())));
        }
        return Rendered::Block(claude(app, outcome, &name, &args, rows, width));
    }
    let mut style = Style::default().fg(if failed {
        theme.error.color()
    } else if complete {
        theme.text_muted.color()
    } else {
        theme.text.color()
    });
    if denied {
        style = style.add_modifier(Modifier::CROSSED_OUT);
    }
    let mut lines = if complete || failed { inline(theme, icon, body, style, width) } else { inline(theme, "~", pending.to_string(), style, width) };
    if let Some(message) = error.filter(|_| failed) {
        let first = message.lines().next().unwrap_or_default();
        lines.extend(wrap_plain(first, width.saturating_sub(5), Style::default().fg(theme.error.color())).into_iter().map(|spans| {
            let mut line = vec![Span::raw("     ")];
            line.extend(spans);
            Line::from(line)
        }));
    }
    Rendered::Inline(lines)
}

fn indented(lines: Vec<Line<'static>>, indent: usize) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|line| {
            let mut spans = vec![Span::raw(" ".repeat(indent))];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

fn text_block(app: &App, source: &str, width: usize) -> Rendered {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Rendered::Hidden;
    }
    let lines = if app.conceal {
        markdown::render(trimmed, width.saturating_sub(3), &app.theme)
    } else {
        // Markup stays visible when concealment is off.
        let style = Style::default().fg(app.theme.text.color());
        wrap_plain(trimmed, width.saturating_sub(3), style).into_iter().map(Line::from).collect()
    };
    if !app.claude_look() {
        return Rendered::Block(indented(lines, 3));
    }
    // Rust build, Claude look: a reply opens with a dot, like a tool call.
    let dot = Style::default().fg(app.theme.text.color());
    Rendered::Block(
        lines
            .into_iter()
            .enumerate()
            .map(|(index, line)| {
                let lead = if index == 0 { vec![Span::raw(" "), Span::styled("● ", dot)] } else { vec![Span::raw("   ")] };
                Line::from(lead.into_iter().chain(line.spans).collect::<Vec<_>>())
            })
            .collect(),
    )
}

fn reasoning(app: &App, source: &str, time: Option<(u64, Option<u64>)>, width: usize) -> Rendered {
    let theme = &app.theme;
    let content = source.replace("[REDACTED]", "");
    let content = content.trim();
    if content.is_empty() {
        return Rendered::Hidden;
    }
    let done = time.and_then(|(start, end)| end.map(|end| end.saturating_sub(start)));
    let open = app.show_thinking;
    let mut header_style = Style::default().fg(theme.warning.color());
    if open {
        header_style = header_style.add_modifier(Modifier::DIM);
    }
    let header = match done {
        None => "Thinking".to_string(),
        Some(ms) => format!("{}Thought · {}", if open { "" } else { "+ " }, duration(ms)),
    };
    let mut lines = vec![Line::from(vec![Span::raw("   "), Span::styled(header, header_style)])];
    if open {
        lines.push(Line::default());
        let muted = Style::default().fg(theme.text_muted.color());
        lines.extend(indented(wrap_plain(content, width.saturating_sub(3), muted).into_iter().map(Line::from).collect(), 3));
    }
    Rendered::Block(lines)
}

fn part(app: &App, part: &Part, width: usize) -> Rendered {
    match &part.body {
        PartBody::Text(t) => text_block(app, &t.text, width),
        PartBody::Reasoning(ReasoningPart { text, time, .. }) => reasoning(app, text, Some((time.start, time.end)), width),
        PartBody::Tool(t) => tool(app, &part.id, t, width),
        _ => Rendered::Hidden,
    }
}

fn cached(app: &App, stored: &Part, width: usize) -> Rendered {
    let print = fingerprint(app, stored, width);
    if let Some((existing, rendered)) = app.part_cache.borrow().entries.get(&stored.id)
        && *existing == print
    {
        return rendered.clone();
    }
    let rendered = part(app, stored, width);
    app.part_cache.borrow_mut().entries.insert(stored.id.clone(), (print, rendered.clone()));
    rendered
}

fn user_block(app: &App, agent: &str, body: &str, files: &[(String, bool)], created: Option<u64>, width: usize) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let style = Style::default().fg(theme.text.color());
    let mut rows = wrap_plain(body, width.saturating_sub(4), style);
    // `Locale.todayTimeOrDateTime`, in local time.
    if let Some(created) = created.filter(|_| app.show_timestamps()) {
        let local = crate::text::local_time;
        let today = local(app.now_ms, "%Y-%m-%d") == local(created, "%Y-%m-%d");
        let shown = if today { local(created, "%H:%M") } else { local(created, "%H:%M · %Y-%m-%d") };
        rows.push(vec![Span::styled(shown, Style::default().fg(theme.text_muted.color()))]);
    }
    if !files.is_empty() {
        rows.push(Vec::new());
        for (name, directory) in files {
            rows.push(vec![
                Span::styled(if *directory { " Directory " } else { " File " }, Style::default().bg(theme.secondary.color()).fg(theme.background.color())),
                Span::styled(format!(" {name} "), Style::default().bg(theme.background_element.color()).fg(theme.text_muted.color())),
            ]);
        }
    }
    if app.claude_look() {
        // Rust build, Claude look: the prompt as typed, after a `>` marker, on a soft band.
        let fill = Style::default().bg(theme.background_panel.color());
        let marker = Style::default().fg(theme.text_muted.color());
        return rows
            .into_iter()
            .enumerate()
            .map(|(index, row)| {
                let lead = Span::styled(if index == 0 { " > " } else { "   " }, marker);
                let row = row.into_iter().map(|span| Span::styled(span.content, fill.patch(span.style))).collect();
                padded(vec![Span::styled(lead.content, fill.patch(lead.style))], row, width, fill)
            })
            .collect();
    }
    block(theme, width, Some(Style::default().fg(app.agent_color(agent).color())), None, rows)
}

fn error_message(error: &AssistantError) -> String {
    match error {
        AssistantError::ProviderAuthError { message, .. }
        | AssistantError::UnknownError { message, .. }
        | AssistantError::MessageAbortedError { message }
        | AssistantError::StructuredOutputError { message, .. }
        | AssistantError::ContextOverflowError { message, .. }
        | AssistantError::ContentFilterError { message } => message.clone(),
        AssistantError::MessageOutputLengthError {} => "MessageOutputLengthError".to_string(),
        AssistantError::ApiError(data) => data.message.clone(),
    }
}

fn assistant_footer(app: &App, view: &SessionView, message: &AssistantMessage, last: bool) -> Option<Line<'static>> {
    let theme = &app.theme;
    let aborted = matches!(message.error, Some(AssistantError::MessageAbortedError { .. }));
    let finished = message.finish.as_deref().is_some_and(|f| !matches!(f, "tool-calls" | "unknown"));
    if !(last || finished || aborted) {
        return None;
    }
    let muted = Style::default().fg(theme.text_muted.color());
    let key = ModelKey::new(&message.provider_id, &message.model_id);
    let model = app.catalog.model(&key).map_or_else(|| message.model_id.clone(), |(_, m)| m.name.clone());
    let mark = if aborted { muted } else { Style::default().fg(app.agent_color(&message.agent).color()) };
    let mut spans = if app.claude_look() {
        // Rust build, Claude look: a quiet signature line.
        vec![Span::raw("   "), Span::styled(titlecase(&message.mode), mark), Span::styled(format!(" · {model}"), muted)]
    } else {
        vec![
            Span::raw("   "),
            Span::styled("▣ ", mark),
            Span::raw(" "),
            Span::styled(titlecase(&message.mode), Style::default().fg(theme.text.color())),
            Span::styled(format!(" · {model}"), muted),
        ]
    };
    let started = view.messages.iter().find_map(|(m, _)| m.as_user().filter(|u| u.id == message.parent_id).map(|u| u.time.created));
    if let (true, Some(completed), Some(started)) = (finished, message.time.completed, started)
        && completed > started
    {
        spans.push(Span::styled(format!(" · {}", duration(completed - started)), muted));
    }
    if aborted {
        spans.push(Span::styled(" · interrupted", muted));
    }
    Some(Line::from(spans))
}

struct Builder {
    lines: Vec<Line<'static>>,
    after_inline: bool,
    regions: Vec<(usize, usize, HitTarget)>,
}

impl Builder {
    fn gap(&mut self) {
        if !self.lines.is_empty() {
            self.lines.push(Line::default());
        }
    }

    fn push(&mut self, rendered: Rendered) {
        match rendered {
            Rendered::Hidden => {}
            Rendered::Inline(lines) => {
                if !self.after_inline {
                    self.gap();
                }
                self.lines.extend(lines);
                self.after_inline = true;
            }
            Rendered::Block(lines) => {
                self.gap();
                self.lines.extend(lines);
                self.after_inline = false;
            }
        }
    }

    fn block(&mut self, lines: Vec<Line<'static>>) {
        self.push(Rendered::Block(lines));
    }

    // Pushes a part and remembers which lines a click on it covers.
    fn part(&mut self, stored: &Part, rendered: Rendered) {
        let before = self.lines.len();
        self.push(rendered);
        if self.lines.len() > before
            && let Some(target) = click_target(stored)
        {
            // The separating blank line is not part of the region.
            let start = (before..self.lines.len()).find(|i| !self.lines[*i].spans.is_empty()).unwrap_or(before);
            self.regions.push((start, self.lines.len(), target));
        }
    }
}

#[derive(Default)]
pub struct Transcript {
    pub lines: Vec<Line<'static>>,
    // (message id, is a user message, first line) of every message block.
    pub anchors: Vec<(String, bool, usize)>,
    // Half-open line ranges that react to a click.
    pub regions: Vec<(usize, usize, HitTarget)>,
}

pub fn build(app: &App, view: &SessionView, width: usize) -> Vec<Line<'static>> {
    layout(app, view, width).lines
}

// Every line of the session view at `width` columns.
pub fn layout(app: &App, view: &SessionView, width: usize) -> Transcript {
    let theme = &app.theme;
    let mut out = Builder { lines: Vec::new(), after_inline: false, regions: Vec::new() };
    let mut anchors = Vec::new();
    let visible: Vec<&(Message, Vec<Part>)> = view.visible_messages().collect();
    for (index, (message, parts)) in visible.iter().enumerate() {
        anchors.push((message.id().to_string(), matches!(message, Message::User(_)), out.lines.len() + usize::from(!out.lines.is_empty())));
        match message {
            Message::User(user) => {
                let body: Vec<&str> = parts
                    .iter()
                    .filter_map(|p| match &p.body {
                        PartBody::Text(t) if t.synthetic != Some(true) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect();
                let files: Vec<(String, bool)> = parts
                    .iter()
                    .filter_map(|p| match &p.body {
                        PartBody::File(f) => Some((f.filename.clone().unwrap_or_default(), f.mime == "application/x-directory")),
                        _ => None,
                    })
                    .collect();
                if !body.is_empty() {
                    out.block(user_block(app, &user.agent, &body.join("\n\n"), &files, Some(user.time.created), width));
                }
                if parts.iter().any(|p| matches!(p.body, PartBody::Compaction(_))) {
                    let label = " Compaction ";
                    let side = width.saturating_sub(label.len()) / 2;
                    let rule = format!("{}{label}{}", "─".repeat(side), "─".repeat(width.saturating_sub(side + label.len())));
                    out.block(vec![Line::from(Span::styled(rule, Style::default().fg(theme.border_active.color())))]);
                }
            }
            Message::Assistant(assistant) => {
                for stored in parts {
                    // Text still streaming is drawn from the live copy below.
                    if view.live_parts.contains(&stored.id) {
                        continue;
                    }
                    out.part(stored, cached(app, stored, width));
                }
                if let Some(error) = assistant.error.as_ref().filter(|e| !matches!(e, AssistantError::MessageAbortedError { .. })) {
                    let rows = wrap_plain(&error_message(error), width.saturating_sub(4), Style::default().fg(theme.text_muted.color()));
                    out.block(block(theme, width, Some(Style::default().fg(theme.error.color())), None, rows));
                }
                let last = index + 1 == visible.len() && view.live.is_empty() && view.pending_user.is_none();
                if let Some(footer) = assistant_footer(app, view, assistant, last) {
                    out.block(vec![footer]);
                }
            }
        }
    }
    if let Some((body, _)) = &view.pending_user {
        out.block(user_block(app, &app.agent, body, &[], None, width));
    }
    for live in &view.live {
        match live {
            Live::Text(source) => out.push(text_block(app, source, width)),
            Live::Reasoning(source) => out.push(reasoning(app, source, None, width)),
            Live::Tool(stored) => out.part(stored, part(app, stored, width)),
        }
    }
    if view.info.revert.is_some() {
        let hidden = view.messages.len() - visible.len();
        let hint = app.keymap.display("messages_redo");
        let body = format!("{hidden} message{} reverted · {hint} or /redo to restore", if hidden == 1 { "" } else { "s" });
        let rows = wrap_spans(&[Span::styled(body, Style::default().fg(theme.text_muted.color()))], width.saturating_sub(4));
        out.block(block(theme, width, Some(Style::default().fg(theme.warning.color())), None, rows));
    }
    Transcript { lines: out.lines, anchors, regions: out.regions }
}

#[cfg(test)]
pub mod testing {
    use super::*;
    use opencode_storage::{AssistantTime, MessagePath, TextPart, Tokens, ToolStateCompleted, ToolTimeCompleted, UserMessage, UserModel, UserTime};
    use serde_json::json;

    pub fn user(id: &str, text: &str) -> (Message, Vec<Part>) {
        let message = Message::User(UserMessage {
            id: id.into(),
            session_id: "ses_1".into(),
            time: UserTime { created: 1_000 },
            format: None,
            summary: None,
            agent: "build".into(),
            model: UserModel { provider_id: "anthropic".into(), model_id: "claude-sonnet-4-5".into(), variant: None },
            system: None,
            tools: None,
        });
        (message, vec![text_part(id, text)])
    }

    pub fn text_part(message_id: &str, text: &str) -> Part {
        Part {
            id: format!("prt_{message_id}_{}", text.len()),
            session_id: "ses_1".into(),
            message_id: message_id.into(),
            body: PartBody::Text(TextPart { text: text.into(), ..Default::default() }),
        }
    }

    pub fn tool_part(message_id: &str, call: &str, name: &str, input: Value, output: &str, metadata: Value) -> Part {
        let object = |v: Value| v.as_object().cloned().unwrap_or_default();
        Part {
            id: format!("prt_{message_id}_{call}"),
            session_id: "ses_1".into(),
            message_id: message_id.into(),
            body: PartBody::Tool(ToolPart {
                call_id: call.into(),
                tool: name.into(),
                state: ToolState::Completed(ToolStateCompleted {
                    input: object(input),
                    output: output.into(),
                    title: String::new(),
                    metadata: object(metadata),
                    time: ToolTimeCompleted { start: 1, end: 2, compacted: None },
                    attachments: None,
                }),
                metadata: None,
            }),
        }
    }

    pub fn assistant(id: &str, parent: &str, parts: Vec<Part>) -> (Message, Vec<Part>) {
        let message = Message::Assistant(AssistantMessage {
            id: id.into(),
            session_id: "ses_1".into(),
            time: AssistantTime { created: 1_100, completed: Some(13_400) },
            error: None,
            parent_id: parent.into(),
            model_id: "claude-sonnet-4-5".into(),
            provider_id: "anthropic".into(),
            mode: "build".into(),
            agent: "build".into(),
            path: MessagePath::default(),
            summary: None,
            cost: 0.0,
            tokens: Tokens { input: 1200.0, output: 34.0, ..Default::default() },
            structured: None,
            variant: None,
            finish: Some("stop".into()),
        });
        (message, parts)
    }

    pub fn conversation() -> Vec<(Message, Vec<Part>)> {
        vec![
            user("msg_1", "list the files"),
            assistant(
                "msg_2",
                "msg_1",
                vec![
                    text_part("msg_2", "Here is **the** plan:\n\n- run `ls`"),
                    tool_part("msg_2", "c1", "read", json!({"filePath": "/project/src/main.rs", "limit": 20}), "", json!({})),
                    tool_part("msg_2", "c2", "bash", json!({"command": "ls -la"}), "", json!({"output": "Cargo.toml\nsrc\n", "exit": 0})),
                    tool_part(
                        "msg_2",
                        "c3",
                        "edit",
                        json!({"filePath": "/project/src/main.rs"}),
                        "",
                        json!({"diff": "--- a\n+++ b\n@@ -1,2 +1,2 @@\n fn main() {\n-    old();\n+    new();\n"}),
                    ),
                    tool_part(
                        "msg_2",
                        "c4",
                        "todowrite",
                        json!({"todos": [{"content": "ship it", "status": "in_progress", "priority": "high"}, {"content": "rest", "status": "completed", "priority": "low"}]}),
                        "",
                        json!({"todos": [{"content": "ship it", "status": "in_progress"}]}),
                    ),
                    tool_part("msg_2", "c5", "mystery", json!({"alpha": 1, "beta": "two", "nested": {}}), "out", json!({})),
                ],
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::app::testing::{app, open, session};
    use crate::text::line_text;
    use serde_json::json;

    fn rendered(app: &App, width: usize) -> Vec<String> {
        build(app, app.session().unwrap(), width).iter().map(|l| line_text(l).trim_end().to_string()).collect()
    }

    #[test]
    fn renders_a_conversation_with_every_tool_shape() {
        let (dir, mut app) = app();
        app.kv.set("rs.style", json!("opencode"));
        app.directory = "/project".into();
        open(&mut app, session("ses_1", "Demo"), conversation());
        let lines = rendered(&app, 60);
        let expected = [
            "┃",
            "┃  list the files",
            "┃",
            "",
            "   Here is the plan:",
            "",
            "   - run ls",
            "",
            "   → Read src/main.rs [limit=20]",
            "",
            "",
            "   $ ls -la",
            "",
            "   Cargo.toml",
            "   src",
            "",
            "",
            "",
            "      ← Edit src/main.rs",
            "",
            "      1   fn main() {",
            "      2 -     old();",
            "      2 +     new();",
            "",
            "",
            "",
            "      # Todos",
            "",
            "   [•] ship it",
            "   [✓] rest",
            "",
            "",
            "   ⚙ mystery [alpha=1, beta=two]",
            "",
            "   ▣  Build · Claude Sonnet 4.5 · 12.4s",
        ];
        assert_eq!(lines, expected, "{}", lines.join("\n"));
        drop(dir);
    }

    #[test]
    fn hides_completed_tools_and_toggles_thinking() {
        let (_dir, mut app) = app();
        let mut reasoning = text_part("msg_2", "x");
        reasoning.id = "prt_reason".into();
        reasoning.body = PartBody::Reasoning(ReasoningPart { text: "weighing options".into(), metadata: None, time: opencode_storage::PartTime { start: 0, end: Some(2500) } });
        let mut messages = conversation();
        messages[1].1.insert(0, reasoning);
        open(&mut app, session("ses_1", "Demo"), messages);
        let lines = rendered(&app, 60);
        assert!(lines.contains(&"   + Thought · 2.5s".to_string()));
        assert!(!lines.iter().any(|l| l.contains("weighing options")));
        app.run("display_thinking");
        let lines = rendered(&app, 60);
        assert!(lines.contains(&"   Thought · 2.5s".to_string()));
        assert!(lines.contains(&"   weighing options".to_string()));
        app.run("tool_details");
        let lines = rendered(&app, 60);
        assert!(!lines.iter().any(|l| l.contains("ls -la") || l.contains("mystery")));
    }

    #[test]
    fn claude_style_draws_calls_with_results_under_an_elbow() {
        let (_dir, mut app) = app();
        app.directory = "/project".into();
        open(&mut app, session("ses_1", "Demo"), conversation());
        let flat = |app: &App| rendered(app, 60).iter().map(|line| line.trim().to_string()).collect::<Vec<_>>().join("\n");
        let text = flat(&app);
        for expected in [
            "● Read(src/main.rs, limit=20)",
            &format!("● {}(ls -la)\n⎿  Cargo.toml\nsrc", *SHELL_NAME),
            "● Update(src/main.rs)\n⎿  Added 1 line, removed 1 line",
            "● Update Todos\n⎿  [•] ship it",
            "● mystery(alpha=1, beta=two)",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in\n{text}");
        }
        assert_eq!(claude_call("Grep \"fn\" in src (3 matches)"), ("Grep".into(), "\"fn\" in src".into(), Some("Found 3 matches".into())));
        // The console host's fonts lack the elbow; the original look stays one kv setting away.
        app.console_host = true;
        assert!(flat(&app).contains("└  Cargo.toml"));
        app.kv.set("rs.style", json!("opencode"));
        assert!(!flat(&app).contains("● "));
    }

    #[test]
    fn shows_pending_and_failed_tools_live_text_and_errors() {
        let (_dir, mut app) = app();
        app.kv.set("rs.style", json!("opencode"));
        let mut messages = conversation();
        let mut failed = tool_part("msg_2", "c9", "read", json!({"filePath": "/x/missing.rs"}), "", json!({}));
        if let PartBody::Tool(tool) = &mut failed.body {
            tool.state = tool.state.to_error("File not found: /x/missing.rs", None, 3);
        }
        let mut pending = tool_part("msg_2", "c10", "bash", json!({}), "", json!({}));
        if let PartBody::Tool(tool) = &mut pending.body {
            tool.state = ToolState::pending(Map::new(), "");
        }
        messages[1].1 = vec![failed, pending];
        if let Message::Assistant(assistant) = &mut messages[1].0 {
            assistant.error = Some(AssistantError::UnknownError { message: "provider exploded".into(), r#ref: None });
        }
        open(&mut app, session("ses_1", "Demo"), messages);
        if let crate::app::Route::Session(view) = &mut app.route {
            view.live.push(Live::Text("streaming *now*".into()));
            view.pending_user = Some(("next question".into(), 1));
        }
        let lines = rendered(&app, 60);
        let joined = lines.join("\n");
        assert!(joined.contains("   → Read /x/missing.rs\n     File not found: /x/missing.rs\n   ~ Writing command…"), "{joined}");
        assert!(joined.contains("┃  provider exploded"));
        assert!(joined.contains("┃  next question"));
        assert_eq!(lines.last().unwrap(), "   streaming now");
    }

    #[test]
    fn strips_ansi_and_formats_paths() {
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m \u{1b}]0;title\u{7}ok\r"), "red ok");
        let (_dir, mut app) = app();
        app.directory = "C:\\work\\proj".into();
        app.home = "C:\\Users\\me".into();
        assert_eq!(format_path(&app, "C:\\work\\proj\\src\\a.rs"), "src/a.rs");
        assert_eq!(format_path(&app, "C:\\Users\\me\\notes.md"), "~/notes.md");
        assert_eq!(format_path(&app, "/etc/hosts"), "/etc/hosts");
    }
}
