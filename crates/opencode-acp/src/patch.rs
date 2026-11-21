// jsdiff 8.0.2 `applyPatch(source, uniDiff)` with its defaults (fuzzFactor 0, autoConvertLineEndings); None where it
// returns false or throws.
struct Hunk {
    old_start: i64,
    old_lines: i64,
    lines: Vec<String>,
}

fn file_header(line: &str) -> bool {
    let rest = line.strip_prefix("---").or_else(|| line.strip_prefix("+++"));
    rest.is_some_and(|rest| rest.starts_with(char::is_whitespace))
}

fn index_boundary(line: &str) -> bool {
    line.starts_with("Index:") && line[6..].starts_with(char::is_whitespace)
        || line.starts_with("diff") && line[4..].starts_with(char::is_whitespace)
        || file_header(line)
        || line.starts_with("===================================================================")
}

// `@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@`
fn hunk_header(line: &str) -> Option<(i64, Option<i64>, i64, Option<i64>)> {
    let at = line.find("@@ -")?;
    let rest = &line[at + 4..];
    let range = |text: &str| -> Option<(i64, Option<i64>, usize)> {
        let digits = text.bytes().take_while(u8::is_ascii_digit).count();
        let start = text[..digits].parse().ok()?;
        let after = &text[digits..];
        let Some(count) = after.strip_prefix(',') else { return Some((start, None, digits)) };
        let more = count.bytes().take_while(u8::is_ascii_digit).count();
        if more == 0 {
            return Some((start, None, digits));
        }
        Some((start, Some(count[..more].parse().ok()?), digits + 1 + more))
    };
    let (old_start, old_lines, used) = range(rest)?;
    let rest = rest[used..].strip_prefix(" +")?;
    let (new_start, new_lines, used) = range(rest)?;
    rest[used..].strip_prefix(" @@")?;
    Some((old_start, old_lines, new_start, new_lines))
}

fn parse(patch: &str) -> Option<Vec<Vec<Hunk>>> {
    let lines: Vec<&str> = patch.split('\n').collect();
    let mut i = 0;
    let mut indexes = Vec::new();
    while i < lines.len() {
        while i < lines.len() {
            let line = lines[i];
            let starts = ["---", "+++", "@@"].iter().any(|p| line.strip_prefix(p).is_some_and(|r| r.starts_with(char::is_whitespace)));
            if starts {
                break;
            }
            i += 1;
        }
        for _ in 0..2 {
            if i < lines.len() && file_header(lines[i]) {
                i += 1;
            }
        }
        let mut hunks = Vec::new();
        while i < lines.len() {
            let line = lines[i];
            if index_boundary(line) {
                break;
            }
            if line.starts_with("@@") {
                let (old_start, old_lines, _, new_lines) = hunk_header(line)?;
                i += 1;
                let mut hunk = Hunk { old_start, old_lines: old_lines.unwrap_or(1), lines: Vec::new() };
                let mut new_lines = new_lines.unwrap_or(1);
                if hunk.old_lines == 0 {
                    hunk.old_start += 1;
                }
                let (mut added, mut removed) = (0, 0);
                while i < lines.len() && (removed < hunk.old_lines || added < new_lines || lines[i].starts_with('\\')) {
                    let current = lines[i];
                    let operation = if current.is_empty() && i != lines.len() - 1 { Some(' ') } else { current.chars().next() };
                    match operation {
                        Some('+') => added += 1,
                        Some('-') => removed += 1,
                        Some(' ') => {
                            added += 1;
                            removed += 1;
                        }
                        Some('\\') => {}
                        _ => return None,
                    }
                    hunk.lines.push(current.to_string());
                    i += 1;
                }
                if added == 0 && new_lines == 1 {
                    new_lines = 0;
                }
                if removed == 0 && hunk.old_lines == 1 {
                    hunk.old_lines = 0;
                }
                if added != new_lines || removed != hunk.old_lines {
                    return None;
                }
                hunks.push(hunk);
            } else if !line.is_empty() {
                return None;
            } else {
                i += 1;
            }
        }
        indexes.push(hunks);
    }
    Some(indexes)
}

fn only_win_endings(text: &str) -> bool {
    let bytes = text.as_bytes();
    text.contains("\r\n") && !text.starts_with('\n') && !bytes.windows(2).any(|w| w[0] != b'\r' && w[1] == b'\n')
}

fn only_unix_endings(text: &str) -> bool {
    !text.contains("\r\n") && text.contains('\n')
}

// A line `unixToWin` leaves alone.
fn win_line(lines: &[String], i: usize) -> bool {
    let line = &lines[i];
    line.starts_with('\\') || line.ends_with('\r') || lines.get(i + 1).is_some_and(|next| next.starts_with('\\'))
}

fn convert_endings(source: &str, hunks: &mut [Hunk]) {
    let is_unix = !hunks.iter().flat_map(|h| &h.lines).any(|line| !line.starts_with('\\') && line.ends_with('\r'));
    let is_win = hunks.iter().flat_map(|h| &h.lines).any(|line| line.ends_with('\r'))
        && hunks.iter().all(|h| (0..h.lines.len()).all(|i| win_line(&h.lines, i)));
    if only_win_endings(source) && is_unix {
        for hunk in hunks.iter_mut() {
            let keep: Vec<bool> = (0..hunk.lines.len()).map(|i| win_line(&hunk.lines, i)).collect();
            for (line, keep) in hunk.lines.iter_mut().zip(keep) {
                if !keep {
                    line.push('\r');
                }
            }
        }
    } else if only_unix_endings(source) && is_win {
        for line in hunks.iter_mut().flat_map(|h| h.lines.iter_mut()) {
            if line.ends_with('\r') {
                line.pop();
            }
        }
    }
}

fn line_at<'a>(lines: &[&'a str], at: i64) -> Option<&'a str> {
    usize::try_from(at).ok().and_then(|at| lines.get(at).copied())
}

// `applyHunk` with maxErrors 0: the patched lines and the last old line consumed.
fn fit(lines: &[&str], hunk: &[String], mut at: i64) -> Option<(Vec<String>, i64)> {
    let mut patched = Vec::new();
    let mut trailing = 0i64;
    for line in hunk {
        let (operation, content) = match line.chars().next() {
            Some(first) => (first, &line[first.len_utf8()..]),
            None => (' ', line.as_str()),
        };
        match operation {
            '-' => {
                if line_at(lines, at) != Some(content) {
                    return None;
                }
                at += 1;
                trailing = 0;
            }
            '+' => {
                patched.push(content.to_string());
                trailing = 0;
            }
            ' ' => {
                trailing += 1;
                if line_at(lines, at) != Some(content) {
                    return None;
                }
                patched.push(content.to_string());
                at += 1;
            }
            _ => {}
        }
    }
    patched.truncate(patched.len().saturating_sub(trailing as usize));
    Some((patched, at - trailing - 1))
}

// `distanceIterator(start, min, max)`: start, then alternately one further forward and backward.
fn positions(start: i64, min: i64, max: i64) -> Vec<i64> {
    let mut out = vec![start];
    let mut offset = 1;
    loop {
        let forward = start + offset <= max;
        let backward = min <= start - offset;
        if !forward && !backward {
            return out;
        }
        if forward {
            out.push(start + offset);
        }
        if backward {
            out.push(start - offset);
        }
        offset += 1;
    }
}

pub fn apply(source: &str, patch: &str) -> Option<String> {
    let mut indexes = parse(patch)?;
    if indexes.len() > 1 {
        return None;
    }
    let mut hunks = indexes.pop().unwrap_or_default();
    convert_endings(source, &mut hunks);
    let mut lines: Vec<&str> = source.split('\n').collect();
    if hunks.is_empty() {
        return Some(source.to_string());
    }

    let (mut remove_eofnl, mut add_eofnl) = (false, false);
    let mut previous = "";
    for line in &hunks[hunks.len() - 1].lines {
        if line.starts_with('\\') {
            if previous.starts_with('+') {
                remove_eofnl = true;
            } else if previous.starts_with('-') {
                add_eofnl = true;
            }
        }
        previous = line;
    }
    let ends_empty = lines.last() == Some(&"");
    if remove_eofnl {
        if add_eofnl {
            if ends_empty {
                return None;
            }
        } else if ends_empty {
            lines.pop();
        } else {
            return None;
        }
    } else if add_eofnl {
        if ends_empty {
            return None;
        }
        lines.push("");
    }

    let mut result: Vec<String> = Vec::new();
    let mut min_line = 0i64;
    let mut offset = 0i64;
    for hunk in &hunks {
        let max_line = lines.len() as i64 - hunk.old_lines;
        let start = hunk.old_start + offset - 1;
        let (at, (patched, last)) = positions(start, min_line, max_line).into_iter().find_map(|at| fit(&lines, &hunk.lines, at).map(|fitted| (at, fitted)))?;
        for i in min_line..at {
            result.push(line_at(&lines, i).unwrap_or_default().to_string());
        }
        result.extend(patched);
        min_line = last + 1;
        offset = at + 1 - hunk.old_start;
    }
    for i in min_line.max(0)..lines.len() as i64 {
        result.push(line_at(&lines, i).unwrap_or_default().to_string());
    }
    Some(result.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::apply;

    const EDIT: &str = "Index: /p/a.txt\n===================================================================\n--- /p/a.txt\n+++ /p/a.txt\n@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n";

    #[test]
    fn applies_unified_diffs_like_jsdiff() {
        assert_eq!(apply("one\ntwo\nthree\n", EDIT).as_deref(), Some("one\nTWO\nthree\n"));
        assert_eq!(apply("zero\none\ntwo\nthree\n", EDIT).as_deref(), Some("zero\none\nTWO\nthree\n"));
        assert_eq!(apply("one\nfour\nthree\n", EDIT), None);
        let create = "Index: f\n===================================================================\n--- f\n+++ f\n@@ -0,0 +1,1 @@\n+hello\n";
        assert_eq!(apply("", create).as_deref(), Some("hello\n"));
        let no_eol = "--- f\n+++ f\n@@ -1,1 +1,1 @@\n-a\n\\ No newline at end of file\n+b\n\\ No newline at end of file\n";
        assert_eq!(apply("a", no_eol).as_deref(), Some("b"));
        assert_eq!(apply("one\r\ntwo\r\nthree\r\n", EDIT).as_deref(), Some("one\r\nTWO\r\nthree\r\n"));
        assert_eq!(apply("x", "garbage line").as_deref(), Some("x"));
        assert_eq!(apply("same", "").as_deref(), Some("same"));
        assert_eq!(apply("a\n", "@@ -1,1 +1,1 @@\n?a\n"), None);
        assert_eq!(apply("a\n", "--- f\n+++ f\n@@ -1 +1 @@\n-a\n+b\n--- g\n+++ g\n@@ -1 +1 @@\n-a\n+b\n"), None);
    }
}
