//! Lenient legacy patch parser (`opencode/src/patch/index.ts`). Not the strict `core/src/patch.ts`.

use crate::util::{bom_split, is_js_whitespace, js_trim, js_trim_end};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct UpdateFileChunk {
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
    pub change_context: Option<String>,
    pub is_end_of_file: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Hunk {
    Add {
        path: String,
        contents: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_path: Option<String>,
        chunks: Vec<UpdateFileChunk>,
    },
}

const BEGIN_MARKER: &str = "*** Begin Patch";
const END_MARKER: &str = "*** End Patch";

/// `/^(?:cat\s+)?<<['"]?(\w+)['"]?\s*\n([\s\S]*?)\n\1\s*$/`
fn strip_heredoc(input: &str) -> &str {
    let mut rest = input;
    if let Some(after) = rest.strip_prefix("cat") {
        let trimmed = after.trim_start_matches(is_js_whitespace);
        if trimmed.len() == after.len() {
            return input;
        }
        rest = trimmed;
    }
    let Some(mut rest) = rest.strip_prefix("<<") else {
        return input;
    };
    rest = rest.strip_prefix(['\'', '"']).unwrap_or(rest);
    let tag_len = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    if tag_len == 0 {
        return input;
    }
    let (tag, mut rest) = rest.split_at(tag_len);
    rest = rest.strip_prefix(['\'', '"']).unwrap_or(rest);
    // `\s*\n`: the header ends at the last newline of the whitespace run that precedes the closing tag.
    let ws_len = rest.len() - rest.trim_start_matches(is_js_whitespace).len();
    let tail = js_trim_end(rest);
    let closing = format!("\n{tag}");
    for (idx, _) in rest.match_indices(&closing) {
        if idx + closing.len() != tail.len() {
            continue;
        }
        let Some(newline) = rest[..ws_len.min(idx)].rfind('\n') else {
            continue;
        };
        return &rest[newline + 1..idx];
    }
    input
}

fn parse_header(lines: &[&str], idx: usize) -> Option<(String, Option<String>, usize)> {
    let line = lines[idx];
    if let Some(rest) = line.strip_prefix("*** Add File:") {
        let path = js_trim(rest);
        return (!path.is_empty()).then(|| (path.to_string(), None, idx + 1));
    }
    if let Some(rest) = line.strip_prefix("*** Delete File:") {
        let path = js_trim(rest);
        return (!path.is_empty()).then(|| (path.to_string(), None, idx + 1));
    }
    if let Some(rest) = line.strip_prefix("*** Update File:") {
        let path = js_trim(rest);
        let mut next = idx + 1;
        let mut move_path = None;
        if let Some(target) = lines.get(next).and_then(|l| l.strip_prefix("*** Move to:")) {
            move_path = Some(js_trim(target).to_string());
            next += 1;
        }
        return (!path.is_empty()).then(|| (path.to_string(), move_path, next));
    }
    None
}

fn parse_update_chunks(lines: &[&str], start: usize) -> (Vec<UpdateFileChunk>, usize) {
    let mut chunks = Vec::new();
    let mut i = start;
    while i < lines.len() && !lines[i].starts_with("***") {
        if !lines[i].starts_with("@@") {
            i += 1;
            continue;
        }
        let context = js_trim(&lines[i][2..]);
        i += 1;
        let mut chunk = UpdateFileChunk {
            change_context: (!context.is_empty()).then(|| context.to_string()),
            ..Default::default()
        };
        // `*** End of File` never reaches its equality check: the loop stops on any `***` line first.
        while i < lines.len() && !lines[i].starts_with("@@") && !lines[i].starts_with("***") {
            let line = lines[i];
            if let Some(content) = line.strip_prefix(' ') {
                chunk.old_lines.push(content.to_string());
                chunk.new_lines.push(content.to_string());
            } else if let Some(content) = line.strip_prefix('-') {
                chunk.old_lines.push(content.to_string());
            } else if let Some(content) = line.strip_prefix('+') {
                chunk.new_lines.push(content.to_string());
            }
            i += 1;
        }
        chunks.push(chunk);
    }
    (chunks, i)
}

fn parse_add_content(lines: &[&str], start: usize) -> (String, usize) {
    let mut content = String::new();
    let mut i = start;
    while i < lines.len() && !lines[i].starts_with("***") {
        if let Some(rest) = lines[i].strip_prefix('+') {
            content.push_str(rest);
            content.push('\n');
        }
        i += 1;
    }
    if content.ends_with('\n') {
        content.pop();
    }
    (content, i)
}

pub fn parse_patch(patch_text: &str) -> Result<Vec<Hunk>, String> {
    let cleaned = strip_heredoc(js_trim(patch_text));
    let lines: Vec<&str> = cleaned.split('\n').collect();
    let begin = lines.iter().position(|l| js_trim(l) == BEGIN_MARKER);
    let end = lines.iter().position(|l| js_trim(l) == END_MARKER);
    let (Some(begin), Some(end)) = (begin, end) else {
        return Err("Invalid patch format: missing Begin/End markers".to_string());
    };
    if begin >= end {
        return Err("Invalid patch format: missing Begin/End markers".to_string());
    }

    let mut hunks = Vec::new();
    let mut i = begin + 1;
    while i < end {
        let Some((path, move_path, next)) = parse_header(&lines, i) else {
            i += 1;
            continue;
        };
        if lines[i].starts_with("*** Add File:") {
            let (contents, after) = parse_add_content(&lines, next);
            hunks.push(Hunk::Add { path, contents });
            i = after;
        } else if lines[i].starts_with("*** Delete File:") {
            hunks.push(Hunk::Delete { path });
            i = next;
        } else {
            let (chunks, after) = parse_update_chunks(&lines, next);
            hunks.push(Hunk::Update {
                path,
                move_path: move_path.filter(|m| !m.is_empty()),
                chunks,
            });
            i = after;
        }
    }
    Ok(hunks)
}

fn normalize_unicode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' => out.push('\''),
            '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' => out.push('"'),
            '\u{2010}'..='\u{2015}' => out.push('-'),
            '\u{2026}' => out.push_str("..."),
            '\u{a0}' => out.push(' '),
            other => out.push(other),
        }
    }
    out
}

type Comparator = dyn Fn(&str, &str) -> bool;

fn try_match(
    lines: &[String],
    pattern: &[String],
    start: usize,
    eof: bool,
    compare: &Comparator,
) -> Option<usize> {
    if pattern.len() > lines.len() {
        return None;
    }
    let matches_at = |at: usize| {
        pattern
            .iter()
            .enumerate()
            .all(|(j, p)| compare(&lines[at + j], p))
    };
    let from_end = lines.len() - pattern.len();
    if eof && from_end >= start && matches_at(from_end) {
        return Some(from_end);
    }
    (start..=from_end).find(|i| matches_at(*i))
}

fn seek_sequence(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.is_empty() {
        return None;
    }
    let passes: [&Comparator; 4] = [
        &|a, b| a == b,
        &|a, b| js_trim_end(a) == js_trim_end(b),
        &|a, b| js_trim(a) == js_trim(b),
        &|a, b| normalize_unicode(js_trim(a)) == normalize_unicode(js_trim(b)),
    ];
    passes
        .iter()
        .find_map(|compare| try_match(lines, pattern, start, eof, compare))
}

fn compute_replacements(
    lines: &[String],
    file_path: &str,
    chunks: &[UpdateFileChunk],
) -> Result<Vec<(usize, usize, Vec<String>)>, String> {
    let mut replacements = Vec::new();
    let mut line_index = 0usize;
    for chunk in chunks {
        if let Some(context) = &chunk.change_context {
            let Some(idx) = seek_sequence(lines, std::slice::from_ref(context), line_index, false)
            else {
                return Err(format!("Failed to find context '{context}' in {file_path}"));
            };
            line_index = idx + 1;
        }

        if chunk.old_lines.is_empty() {
            let insertion = if lines.last().is_some_and(|l| l.is_empty()) {
                lines.len() - 1
            } else {
                lines.len()
            };
            replacements.push((insertion, 0, chunk.new_lines.clone()));
            continue;
        }

        let mut pattern: &[String] = &chunk.old_lines;
        let mut new_slice: &[String] = &chunk.new_lines;
        let mut found = seek_sequence(lines, pattern, line_index, chunk.is_end_of_file);
        if found.is_none() && pattern.last().is_some_and(|l| l.is_empty()) {
            pattern = &pattern[..pattern.len() - 1];
            if new_slice.last().is_some_and(|l| l.is_empty()) {
                new_slice = &new_slice[..new_slice.len() - 1];
            }
            found = seek_sequence(lines, pattern, line_index, chunk.is_end_of_file);
        }
        match found {
            Some(idx) => {
                replacements.push((idx, pattern.len(), new_slice.to_vec()));
                line_index = idx + pattern.len();
            }
            None => {
                return Err(format!(
                    "Failed to find expected lines in {file_path}:\n{}",
                    chunk.old_lines.join("\n")
                ));
            }
        }
    }
    replacements.sort_by_key(|r| r.0);
    Ok(replacements)
}

/// `deriveNewContentsFromChunks`: returns `(content, bom)`.
pub fn derive_new_contents(
    file_path: &str,
    chunks: &[UpdateFileChunk],
    original: &str,
) -> Result<(String, bool), String> {
    let (bom, text) = bom_split(original);
    let mut lines: Vec<String> = text.split('\n').map(String::from).collect();
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let replacements = compute_replacements(&lines, file_path, chunks)?;
    for (start, old_len, segment) in replacements.into_iter().rev() {
        let end = (start + old_len).min(lines.len());
        lines.splice(start.min(end)..end, segment);
    }
    if lines.last().is_none_or(|l| !l.is_empty()) {
        lines.push(String::new());
    }
    let joined = lines.join("\n");
    let (next_bom, content) = bom_split(&joined);
    Ok((content.to_string(), bom || next_bom))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(old: &[&str], new: &[&str], ctx: Option<&str>) -> UpdateFileChunk {
        UpdateFileChunk {
            old_lines: old.iter().map(|s| s.to_string()).collect(),
            new_lines: new.iter().map(|s| s.to_string()).collect(),
            change_context: ctx.map(String::from),
            is_end_of_file: false,
        }
    }

    #[test]
    fn parses_all_hunk_kinds() {
        let patch = "*** Begin Patch\n*** Add File: a.txt\n+one\n+two\nignored\n*** Delete File: b.txt\n*** Update File: c.txt\n*** Move to: d.txt\nskipped before chunk\n@@ fn main\n ctx\n-old\n+new\n\n@@\n+tail\n*** End of File\n*** End Patch\ntrailing";
        let hunks = parse_patch(patch).unwrap();
        assert_eq!(
            hunks,
            vec![
                Hunk::Add {
                    path: "a.txt".into(),
                    contents: "one\ntwo".into()
                },
                Hunk::Delete {
                    path: "b.txt".into()
                },
                Hunk::Update {
                    path: "c.txt".into(),
                    move_path: Some("d.txt".into()),
                    chunks: vec![
                        chunk(&["ctx", "old"], &["ctx", "new"], Some("fn main")),
                        chunk(&[], &["tail"], None),
                    ],
                },
            ]
        );
    }

    #[test]
    fn missing_markers_and_empty_paths() {
        assert_eq!(
            parse_patch("nothing").unwrap_err(),
            "Invalid patch format: missing Begin/End markers"
        );
        assert_eq!(
            parse_patch("*** End Patch\n*** Begin Patch").unwrap_err(),
            "Invalid patch format: missing Begin/End markers"
        );
        assert!(
            parse_patch("*** Begin Patch\n*** Add File:   \n+x\n*** End Patch")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn heredoc_wrapper_is_stripped() {
        let inner = "*** Begin Patch\n*** Delete File: x\n*** End Patch";
        for wrapped in [
            format!("cat <<'EOF'\n{inner}\nEOF"),
            format!("<<EOF\n{inner}\nEOF"),
            format!("  <<\"PATCH\"  \n{inner}\nPATCH\n"),
        ] {
            assert_eq!(
                parse_patch(&wrapped).unwrap(),
                vec![Hunk::Delete { path: "x".into() }]
            );
        }
        assert_eq!(strip_heredoc("<<EOF\nbody\nNOPE"), "<<EOF\nbody\nNOPE");
        assert_eq!(strip_heredoc("<<EOF\nEOF"), "<<EOF\nEOF");
        assert_eq!(strip_heredoc("<<EOF\n\nEOF"), "");
    }

    #[test]
    fn derive_replaces_and_keeps_trailing_newline() {
        let (out, bom) = derive_new_contents(
            "f",
            &[chunk(&["b"], &["B", "B2"], None)],
            "\u{feff}a\nb\nc\n",
        )
        .unwrap();
        assert_eq!(out, "a\nB\nB2\nc\n");
        assert!(bom);
        let (out, _) = derive_new_contents("f", &[chunk(&["c"], &["C"], None)], "a\nb\nc").unwrap();
        assert_eq!(out, "a\nb\nC\n");
    }

    #[test]
    fn derive_context_and_pure_addition() {
        let chunks = [chunk(&["x"], &["y"], Some("second"))];
        let (out, _) = derive_new_contents("f", &chunks, "x\nsecond\nx\n").unwrap();
        assert_eq!(out, "x\nsecond\ny\n");
        let (out, _) = derive_new_contents(
            "f",
            &[chunk(&[], &["end"], Some("second"))],
            "x\nsecond\nx\n",
        )
        .unwrap();
        assert_eq!(out, "x\nsecond\nx\nend\n");
    }

    #[test]
    fn derive_fuzzy_passes_and_errors() {
        let (out, _) = derive_new_contents(
            "f",
            &[chunk(&["  say \u{201c}hi\u{201d}  "], &["bye"], None)],
            "say \"hi\"\n",
        )
        .unwrap();
        assert_eq!(out, "bye\n");
        let (out, _) =
            derive_new_contents("f", &[chunk(&["a", ""], &["A", ""], None)], "a\n").unwrap();
        assert_eq!(out, "A\n");
        assert_eq!(
            derive_new_contents("f.txt", &[chunk(&["nope", "x"], &[], None)], "a\n").unwrap_err(),
            "Failed to find expected lines in f.txt:\nnope\nx"
        );
        assert_eq!(
            derive_new_contents("f.txt", &[chunk(&["a"], &[], Some("zzz"))], "a\n").unwrap_err(),
            "Failed to find context 'zzz' in f.txt"
        );
    }
}
