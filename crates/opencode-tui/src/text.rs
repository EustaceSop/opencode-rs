use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

pub fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| width(&s.content)).sum()
}

// `Locale.truncate` by display columns.
pub fn truncate(text: &str, max: usize) -> String {
    if width(text) <= max {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > max {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out
}

pub fn truncate_left(text: &str, max: usize) -> String {
    if width(text) <= max {
        return text.to_string();
    }
    let mut kept: Vec<char> = Vec::new();
    let mut used = 0;
    for c in text.chars().rev() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > max {
            break;
        }
        used += w;
        kept.push(c);
    }
    std::iter::once('…').chain(kept.into_iter().rev()).collect()
}

// Ends a wrapped row, dropping the whitespace left at the break.
fn wrap_row(lines: &mut Vec<Vec<Span<'static>>>, line: &mut Vec<Span<'static>>) {
    while let Some(last) = line.last_mut() {
        let kept = last.content.trim_end().len();
        if kept > 0 {
            last.content.to_mut().truncate(kept);
            break;
        }
        line.pop();
    }
    lines.push(std::mem::take(line));
}

fn push(line: &mut Vec<Span<'static>>, text: &str, style: Style) {
    match line.last_mut() {
        Some(last) if last.style == style => last.content.to_mut().push_str(text),
        _ => line.push(Span::styled(text.to_string(), style)),
    }
}

// Never at the start of a row / never at its end (line breaking rules for CJK text).
const CLOSING: &str = "，。、；：？！）」』】〉》〕｝…‥・ー々〜’”,.;:!?)]}%";
const OPENING: &str = "（「『【〈《〔｛‘“([{";

// Break opportunities inside a run without spaces: around wide (CJK) characters, which are not separated by spaces,
// except before closing and after opening punctuation.
fn pieces(token: &str) -> Vec<&str> {
    let wide = |c: char| c.width() == Some(2);
    let mut out = Vec::new();
    let mut start = 0;
    let mut previous: Option<char> = None;
    for (index, c) in token.char_indices() {
        if let Some(p) = previous
            && (wide(p) || wide(c))
            && !CLOSING.contains(c)
            && !OPENING.contains(p)
        {
            out.push(&token[start..index]);
            start = index;
        }
        previous = Some(c);
    }
    out.push(&token[start..]);
    out
}

// Word-wraps styled text. Newlines inside spans force a break; whitespace at a wrap point is dropped.
pub fn wrap_spans(spans: &[Span<'static>], max: usize) -> Vec<Vec<Span<'static>>> {
    let max = max.max(1);
    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut line: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    let mut wrapped = false;
    for span in spans {
        for (index, segment) in span.content.split('\n').enumerate() {
            if index > 0 {
                lines.push(std::mem::take(&mut line));
                used = 0;
                wrapped = false;
            }
            let mut rest = segment;
            while !rest.is_empty() {
                let space = rest.starts_with(char::is_whitespace);
                let end = rest.find(|c: char| c.is_whitespace() != space).unwrap_or(rest.len());
                let (token, tail) = rest.split_at(end);
                rest = tail;
                let w = width(token);
                if space {
                    if used == 0 && wrapped {
                        continue;
                    }
                    if used + w > max {
                        wrap_row(&mut lines, &mut line);
                        used = 0;
                        wrapped = true;
                        continue;
                    }
                    push(&mut line, token, span.style);
                    used += w;
                    continue;
                }
                for piece in pieces(token) {
                    let w = width(piece);
                    if used + w > max && used > 0 {
                        wrap_row(&mut lines, &mut line);
                        used = 0;
                        wrapped = true;
                    }
                    if w <= max {
                        push(&mut line, piece, span.style);
                        used += w;
                        continue;
                    }
                    let mut chunk = String::new();
                    for c in piece.chars() {
                        let cw = c.width().unwrap_or(0);
                        if used + cw > max && used > 0 {
                            push(&mut line, &chunk, span.style);
                            chunk.clear();
                            lines.push(std::mem::take(&mut line));
                            used = 0;
                            wrapped = true;
                        }
                        chunk.push(c);
                        used += cw;
                    }
                    push(&mut line, &chunk, span.style);
                }
            }
        }
    }
    lines.push(line);
    lines
}

pub fn wrap_plain(text: &str, max: usize, style: Style) -> Vec<Vec<Span<'static>>> {
    wrap_spans(&[Span::styled(text.to_string(), style)], max)
}

// Prefixes a wrapped line and pads it to `total` columns so a background fills the row.
pub fn padded(prefix: Vec<Span<'static>>, body: Vec<Span<'static>>, total: usize, fill: Style) -> Line<'static> {
    let mut spans = prefix;
    spans.extend(body);
    let used = spans_width(&spans);
    if used < total {
        spans.push(Span::styled(" ".repeat(total - used), fill));
    }
    Line::from(spans)
}

pub fn line_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

// `Locale.titlecase`
pub fn titlecase(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut boundary = true;
    for c in text.chars() {
        let word = c.is_alphanumeric() || c == '_';
        if word && boundary {
            out.extend(c.to_uppercase());
        } else {
            out.push(c);
        }
        boundary = !word;
    }
    out
}

// A timestamp in the viewer's time zone, as the original's `Date` methods show it; `format` is a chrono pattern.
pub fn local_time(ms: u64, format: &str) -> String {
    use chrono::TimeZone as _;
    chrono::Local.timestamp_millis_opt(ms as i64).single().map(|date| date.format(format).to_string()).unwrap_or_default()
}

// `Locale.duration`
pub fn duration(ms: u64) -> String {
    match ms {
        0..1000 => format!("{ms}ms"),
        1000..60_000 => format!("{:.1}s", ms as f64 / 1000.0),
        60_000..3_600_000 => format!("{}m {}s", ms / 60_000, ms % 60_000 / 1000),
        3_600_000..86_400_000 => format!("{}h {}m", ms / 3_600_000, ms % 3_600_000 / 60_000),
        _ => format!("{}d {}h", ms / 86_400_000, ms % 86_400_000 / 3_600_000),
    }
}

// `toLocaleString()` grouping for en-US.
pub fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: Vec<Vec<Span<'static>>>) -> Vec<String> {
        lines.iter().map(|l| l.iter().map(|s| s.content.as_ref()).collect()).collect()
    }

    #[test]
    fn wraps_words_and_long_tokens() {
        assert_eq!(texts(wrap_plain("the quick brown fox", 10, Style::default())), vec!["the quick", "brown fox"]);
        assert_eq!(texts(wrap_plain("abcdefghij", 4, Style::default())), vec!["abcd", "efgh", "ij"]);
        assert_eq!(texts(wrap_plain("a\n\n  b", 10, Style::default())), vec!["a", "", "  b"]);
        assert_eq!(texts(wrap_plain("", 10, Style::default())), vec![""]);
        assert_eq!(texts(wrap_plain("漢字漢字", 4, Style::default())), vec!["漢字", "漢字"]);
    }

    #[test]
    fn wraps_cjk_between_characters_but_not_before_closing_punctuation() {
        let plain = |text: &str, max| texts(wrap_plain(text, max, Style::default()));
        assert_eq!(plain("中文句子沒有空白也能換行", 10), vec!["中文句子沒", "有空白也能", "換行"]);
        assert_eq!(plain("一二三四，五六", 8), vec!["一二三", "四，五六"]);
        assert_eq!(plain("一二（三四）", 6), vec!["一二", "（三", "四）"]);
        assert_eq!(plain("only Copy的型別", 10), vec!["only Copy", "的型別"]);
        assert_eq!(plain("https://example.com/path", 30), vec!["https://example.com/path"]);
        let spans = [Span::raw("實作 "), Span::styled("Copy", Style::default().add_modifier(ratatui::style::Modifier::BOLD)), Span::raw("的型別才是複製")];
        assert_eq!(texts(wrap_spans(&spans, 12)), vec!["實作 Copy的", "型別才是複製"]);
    }

    #[test]
    fn formats_like_locale() {
        assert_eq!(titlecase("build"), "Build");
        assert_eq!(titlecase("plan mode"), "Plan Mode");
        assert_eq!(duration(950), "950ms");
        assert_eq!(duration(12_345), "12.3s");
        assert_eq!(duration(125_000), "2m 5s");
        assert_eq!(grouped(1_234_567), "1,234,567");
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate_left("hello world", 6), "…world");
        let now = chrono::Local::now();
        assert_eq!(local_time(now.timestamp_millis() as u64, "%Y-%m-%d %H:%M"), now.format("%Y-%m-%d %H:%M").to_string());
    }
}
