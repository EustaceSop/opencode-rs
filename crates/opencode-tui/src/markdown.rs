use crate::text::{spans_width, width, wrap_spans};
use crate::theme::{Rgba, Theme};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::str::FromStr;
use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Color as SynColor, FontStyle, ScopeSelectors, StyleModifier, Theme as SynTheme, ThemeItem, ThemeSettings};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn syn_color(c: Rgba) -> SynColor {
    SynColor { r: c.r, g: c.g, b: c.b, a: 255 }
}

// Maps the theme's `syntax*` colors onto TextMate scopes.
fn syn_theme(theme: &Theme) -> SynTheme {
    let item = |scopes: &str, color: Rgba, font: Option<FontStyle>| ThemeItem {
        scope: ScopeSelectors::from_str(scopes).unwrap_or_default(),
        style: StyleModifier { foreground: Some(syn_color(color)), background: None, font_style: font },
    };
    SynTheme {
        name: None,
        author: None,
        settings: ThemeSettings { foreground: Some(syn_color(theme.markdown_code_block)), ..Default::default() },
        scopes: vec![
            item("variable, support.variable", theme.syntax_variable, None),
            item("punctuation, meta.brace", theme.syntax_punctuation, None),
            item("keyword, storage, storage.modifier", theme.syntax_keyword, None),
            item("keyword.operator", theme.syntax_operator, None),
            item("entity.name.function, support.function, meta.function-call, variable.function", theme.syntax_function, None),
            item("entity.name.type, entity.name.class, support.type, support.class, entity.other.inherited-class", theme.syntax_type, None),
            item("string, punctuation.definition.string", theme.syntax_string, None),
            item("constant.numeric, constant.language, constant.character", theme.syntax_number, None),
            item("comment, punctuation.definition.comment", theme.syntax_comment, Some(FontStyle::ITALIC)),
        ],
    }
}

fn syntax_for(token: &str) -> Option<&'static SyntaxReference> {
    let set = syntaxes();
    let token = token.trim().to_ascii_lowercase();
    let mapped = match token.as_str() {
        "ts" | "tsx" | "typescript" | "jsx" | "mjs" | "cjs" | "mts" | "cts" => "js",
        "shell" | "zsh" | "fish" | "console" => "bash",
        "jsonc" | "json5" => "json",
        "yml" => "yaml",
        other => other,
    };
    if mapped.is_empty() {
        return None;
    }
    set.find_syntax_by_token(mapped)
}

// One vector of styled spans per source line; unknown languages come back unstyled.
pub fn highlight(code: &str, language: &str, theme: &Theme) -> Vec<Vec<Span<'static>>> {
    let base = Style::default().fg(theme.markdown_code_block.color());
    let Some(syntax) = syntax_for(language) else {
        return code.lines().map(|line| vec![Span::styled(line.to_string(), base)]).collect();
    };
    let syn = syn_theme(theme);
    let mut lines = HighlightLines::new(syntax, &syn);
    let mut out = Vec::new();
    for line in LinesWithEndings::from(code) {
        let spans = match lines.highlight_line(line, syntaxes()) {
            Ok(ranges) => ranges
                .into_iter()
                .map(|(style, text)| {
                    let mut out = Style::default().fg(ratatui::style::Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b));
                    if style.font_style.contains(FontStyle::ITALIC) {
                        out = out.add_modifier(Modifier::ITALIC);
                    }
                    Span::styled(text.trim_end_matches(['\n', '\r']).to_string(), out)
                })
                .filter(|span| !span.content.is_empty())
                .collect(),
            Err(_) => vec![Span::styled(line.trim_end_matches(['\n', '\r']).to_string(), base)],
        };
        out.push(spans);
    }
    out
}

pub fn language_for_path(path: &str) -> String {
    std::path::Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or_default().to_string()
}

struct Table {
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    cell: String,
}

struct Renderer<'a> {
    theme: &'a Theme,
    width: usize,
    out: Vec<Line<'static>>,
    inline: Vec<Span<'static>>,
    styles: Vec<Style>,
    // One entry per open list: the next ordinal for ordered lists.
    lists: Vec<Option<u64>>,
    // Marker width of each open list item.
    indents: Vec<usize>,
    marker: Option<Span<'static>>,
    quote: usize,
    code: Option<(String, String)>,
    table: Option<Table>,
    links: Vec<(String, usize)>,
}

impl Renderer<'_> {
    fn style(&self) -> Style {
        self.styles.last().copied().unwrap_or_default()
    }

    fn gap(&mut self) {
        if self.lists.is_empty() && self.out.last().is_some_and(|line| !line.spans.is_empty()) {
            self.out.push(Line::default());
        }
    }

    fn prefix(&mut self, first: bool) -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        if self.quote > 0 {
            spans.push(Span::styled("│ ".repeat(self.quote), Style::default().fg(self.theme.markdown_block_quote.color())));
        }
        let marker = if first { self.marker.take() } else { None };
        let indent: usize = match &marker {
            Some(_) => self.indents.iter().rev().skip(1).sum(),
            None => self.indents.iter().sum(),
        };
        if indent > 0 {
            spans.push(Span::raw(" ".repeat(indent)));
        }
        spans.extend(marker);
        spans
    }

    fn flush(&mut self) {
        if self.inline.is_empty() {
            return;
        }
        let inline = std::mem::take(&mut self.inline);
        let reserved = self.quote * 2 + self.indents.iter().sum::<usize>();
        for (index, body) in wrap_spans(&inline, self.width.saturating_sub(reserved)).into_iter().enumerate() {
            let mut spans = self.prefix(index == 0);
            spans.extend(body);
            self.out.push(Line::from(spans));
        }
    }

    fn code_block(&mut self, language: &str, code: &str) {
        let reserved = self.quote * 2 + self.indents.iter().sum::<usize>() + 2;
        for line in highlight(code.trim_end_matches('\n'), language, self.theme) {
            for body in wrap_spans(&line, self.width.saturating_sub(reserved)) {
                let mut spans = self.prefix(false);
                spans.push(Span::raw("  "));
                spans.extend(body);
                self.out.push(Line::from(spans));
            }
        }
    }

    fn table(&mut self, table: Table) {
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        let widths: Vec<usize> = (0..columns).map(|c| table.rows.iter().filter_map(|r| r.get(c)).map(|cell| width(cell)).max().unwrap_or(0)).collect();
        let border = Style::default().fg(self.theme.border.color());
        let text = Style::default().fg(self.theme.markdown_text.color());
        let rule = |left: &str, mid: &str, right: &str| {
            let cells: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
            Line::from(Span::styled(format!("{left}{}{right}", cells.join(mid)), border))
        };
        self.out.push(rule("┌", "┬", "┐"));
        for (index, row) in table.rows.iter().enumerate() {
            let mut spans = vec![Span::styled("│", border)];
            for (column, w) in widths.iter().enumerate() {
                let cell = row.get(column).map(String::as_str).unwrap_or_default();
                let style = if index == 0 { text.add_modifier(Modifier::BOLD) } else { text };
                spans.push(Span::styled(format!(" {cell}{} ", " ".repeat(w - width(cell))), style));
                spans.push(Span::styled("│", border));
            }
            self.out.push(Line::from(spans));
            if index + 1 < table.rows.len() {
                self.out.push(rule("├", "┼", "┤"));
            }
        }
        self.out.push(rule("└", "┴", "┘"));
    }

    fn text(&mut self, text: &str) {
        if let Some((_, code)) = &mut self.code {
            code.push_str(text);
        } else if let Some(table) = &mut self.table {
            table.cell.push_str(text);
        } else {
            self.inline.push(Span::styled(text.to_string(), self.style()));
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        let theme = self.theme;
        let base = self.style();
        match tag {
            Tag::Paragraph => self.gap(),
            Tag::Heading { .. } => {
                self.gap();
                self.styles.push(base.fg(theme.markdown_heading.color()).add_modifier(Modifier::BOLD));
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.gap();
                self.quote += 1;
                self.styles.push(base.fg(theme.markdown_block_quote.color()));
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                self.gap();
                let language = match kind {
                    CodeBlockKind::Fenced(info) => info.split([' ', ',']).next().unwrap_or_default().to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((language, String::new()));
            }
            Tag::List(start) => {
                self.flush();
                self.gap();
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush();
                let (label, color) = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        *n += 1;
                        (format!("{}. ", *n - 1), theme.markdown_list_enumeration)
                    }
                    _ => ("- ".to_string(), theme.markdown_list_item),
                };
                self.indents.push(width(&label));
                self.marker = Some(Span::styled(label, Style::default().fg(color.color())));
            }
            Tag::Emphasis => self.styles.push(base.fg(theme.markdown_emph.color()).add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.styles.push(base.fg(theme.markdown_strong.color()).add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self.styles.push(base.add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.links.push((dest_url.to_string(), self.inline.len()));
                self.styles.push(base.fg(theme.markdown_link_text.color()).add_modifier(Modifier::UNDERLINED));
            }
            Tag::Image { .. } => {
                self.inline.push(Span::styled("[image: ", Style::default().fg(theme.markdown_image.color())));
                self.styles.push(base.fg(theme.markdown_image_text.color()));
            }
            Tag::Table(_) => {
                self.flush();
                self.gap();
                self.table = Some(Table { rows: Vec::new(), row: Vec::new(), cell: String::new() });
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush(),
            TagEnd::Heading(_) => {
                self.flush();
                self.styles.pop();
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote = self.quote.saturating_sub(1);
                self.styles.pop();
            }
            TagEnd::CodeBlock => {
                if let Some((language, code)) = self.code.take() {
                    self.code_block(&language, &code);
                }
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
            }
            TagEnd::Item => {
                self.flush();
                self.indents.pop();
                self.marker = None;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                if let Some((url, from)) = self.links.pop() {
                    let label: String = self.inline[from.min(self.inline.len())..].iter().map(|s| s.content.as_ref()).collect();
                    if !url.is_empty() && label != url {
                        self.inline.push(Span::styled(format!(" ({url})"), Style::default().fg(self.theme.markdown_link.color())));
                    }
                }
            }
            TagEnd::Image => {
                self.styles.pop();
                self.inline.push(Span::styled("]", Style::default().fg(self.theme.markdown_image.color())));
            }
            TagEnd::TableCell => {
                if let Some(table) = &mut self.table {
                    let cell = std::mem::take(&mut table.cell);
                    table.row.push(cell);
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.table(table);
                }
            }
            _ => {}
        }
    }
}

pub fn render(source: &str, max_width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut renderer = Renderer {
        theme,
        width: max_width.max(8),
        out: Vec::new(),
        inline: Vec::new(),
        styles: vec![Style::default().fg(theme.markdown_text.color())],
        lists: Vec::new(),
        indents: Vec::new(),
        marker: None,
        quote: 0,
        code: None,
        table: None,
        links: Vec::new(),
    };
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(source, options) {
        match event {
            Event::Start(tag) => renderer.start(tag),
            Event::End(tag) => renderer.end(tag),
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => renderer.text(&text),
            Event::Code(text) => {
                if let Some(table) = &mut renderer.table {
                    table.cell.push_str(&text);
                } else {
                    renderer.inline.push(Span::styled(text.to_string(), Style::default().fg(theme.markdown_code.color())));
                }
            }
            Event::SoftBreak => renderer.text(" "),
            Event::HardBreak => renderer.text("\n"),
            Event::TaskListMarker(done) => renderer.text(if done { "[x] " } else { "[ ] " }),
            Event::Rule => {
                renderer.flush();
                renderer.gap();
                let rule = "─".repeat(renderer.width.saturating_sub(renderer.quote * 2));
                let mut spans = renderer.prefix(false);
                spans.push(Span::styled(rule, Style::default().fg(theme.markdown_horizontal_rule.color())));
                renderer.out.push(Line::from(spans));
            }
            _ => {}
        }
    }
    renderer.flush();
    // An unterminated fence while streaming still shows its code.
    if let Some((language, code)) = renderer.code.take() {
        renderer.code_block(&language, &code);
    }
    while renderer.out.last().is_some_and(|line| spans_width(&line.spans) == 0) {
        renderer.out.pop();
    }
    renderer.out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::line_text;
    use crate::theme::{Mode, Registry};

    fn theme() -> Theme {
        Registry::builtin().resolve("opencode", Mode::Dark)
    }

    fn texts(source: &str, width: usize) -> Vec<String> {
        render(source, width, &theme()).iter().map(line_text).collect()
    }

    #[test]
    fn renders_blocks() {
        let out = texts("# Title\n\nSome *emphasis* and `code`.\n\n- one\n- two\n  1. nested\n\n> quoted\n\n---\n", 40);
        assert_eq!(
            out,
            vec![
                "Title".to_string(),
                String::new(),
                "Some emphasis and code.".into(),
                String::new(),
                "- one".into(),
                "- two".into(),
                "  1. nested".into(),
                String::new(),
                "│ quoted".into(),
                String::new(),
                "─".repeat(40),
            ]
        );
    }

    #[test]
    fn styles_inline_markup() {
        let theme = theme();
        let lines = render("**bold** `x` [site](https://example.com)", 60, &theme);
        let spans = &lines[0].spans;
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[2].style.fg, Some(theme.markdown_code.color()));
        assert_eq!(line_text(&lines[0]), "bold x site (https://example.com)");
    }

    #[test]
    fn highlights_fenced_code() {
        let theme = theme();
        let lines = render("```rust\nfn main() { let x = 1; }\n```", 60, &theme);
        assert_eq!(line_text(&lines[0]), "  fn main() { let x = 1; }");
        let keyword = lines[0].spans.iter().find(|s| s.content.as_ref() == "fn").unwrap();
        assert_eq!(keyword.style.fg, Some(theme.syntax_keyword.color()));
        let number = lines[0].spans.iter().find(|s| s.content.as_ref() == "1").unwrap();
        assert_eq!(number.style.fg, Some(theme.syntax_number.color()));
        // Streaming: the fence has not been closed yet.
        assert_eq!(texts("```\nplain", 20), vec!["  plain"]);
    }

    #[test]
    fn wraps_list_items_under_their_marker() {
        assert_eq!(texts("1. alpha beta gamma delta", 14), vec!["1. alpha beta", "   gamma delta"]);
    }

    #[test]
    fn renders_tables() {
        let out = texts("| a | bb |\n|---|----|\n| 1 | 2 |", 40);
        assert_eq!(out, vec!["┌───┬────┐", "│ a │ bb │", "├───┼────┤", "│ 1 │ 2  │", "└───┴────┘"]);
    }
}
