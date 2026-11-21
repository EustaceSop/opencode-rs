//! `edit` tool and its fuzzy replacement chain (`opencode/src/tool/edit.ts`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Value, json};

use crate::util::{
    PathKind, assert_external_directory, bom_join, bom_read, bom_split, display, io_err,
    is_js_whitespace, js_trim, leading_whitespace, line_counts, real_path, relative, skip_chars,
    trim_diff, two_files_patch, utf16_len, write_with_dirs,
};
use crate::write::join_path;
use crate::{PermissionRequest, Tool, ToolContext, ToolError, ToolOutput, hooks, parse_args};

const SINGLE_CANDIDATE_SIMILARITY_THRESHOLD: f64 = 0.65;
const MULTIPLE_CANDIDATES_SIMILARITY_THRESHOLD: f64 = 0.65;

const ERR_IDENTICAL: &str = "No changes to apply: oldString and newString are identical.";
const ERR_EMPTY_OLD: &str = "oldString cannot be empty when editing an existing file. Provide the exact text to replace, or use write for an intentional full-file replacement.";
const ERR_DISPROPORTIONATE: &str = "Refusing replacement because the matched span is much larger than oldString. Re-read the file and provide the full exact oldString for the intended replacement.";
const ERR_NOT_FOUND: &str = "Could not find oldString in the file. It must match exactly, including whitespace, indentation, and line endings.";
const ERR_MULTIPLE: &str = "Found multiple matches for oldString. Provide more surrounding context to make the match unique.";

#[derive(Debug, serde::Deserialize)]
struct Params {
    #[serde(rename = "filePath")]
    file_path: String,
    #[serde(rename = "oldString")]
    old_string: String,
    #[serde(rename = "newString")]
    new_string: String,
    #[serde(rename = "replaceAll", default)]
    replace_all: Option<bool>,
}

pub struct EditTool;

fn lock_for(path: PathBuf) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut map = LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    map.entry(path).or_default().clone()
}

fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n")
}

fn convert_to_line_ending(text: String, crlf: bool) -> String {
    if crlf {
        text.replace('\n', "\r\n")
    } else {
        text
    }
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<u16> = a.encode_utf16().collect();
    let b: Vec<u16> = b.encode_utf16().collect();
    if a.is_empty() || b.is_empty() {
        return a.len().max(b.len());
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut row = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            row[j] = (prev[j] + 1).min(row[j - 1] + 1).min(prev[j - 1] + cost);
        }
        prev = row;
    }
    prev[b.len()]
}

/// Substring of `content` covering lines `start..=end` (no trailing newline).
fn line_span<'a>(content: &'a str, lines: &[&str], start: usize, end: usize) -> &'a str {
    let from: usize = lines[..start].iter().map(|l| l.len() + 1).sum();
    let len: usize = lines[start..=end].iter().map(|l| l.len()).sum::<usize>() + (end - start);
    &content[from..from + len]
}

fn simple(_content: &str, find: &str) -> Vec<String> {
    vec![find.to_string()]
}

fn line_trimmed(content: &str, find: &str) -> Vec<String> {
    let original: Vec<&str> = content.split('\n').collect();
    let mut search: Vec<&str> = find.split('\n').collect();
    if search.last() == Some(&"") {
        search.pop();
    }
    let mut out = Vec::new();
    if search.len() > original.len() {
        return out;
    }
    for i in 0..=original.len() - search.len() {
        if (0..search.len()).all(|j| js_trim(original[i + j]) == js_trim(search[j])) {
            if search.is_empty() {
                out.push(String::new());
            } else {
                out.push(line_span(content, &original, i, i + search.len() - 1).to_string());
            }
        }
    }
    out
}

fn block_similarity(
    original: &[&str],
    search: &[&str],
    start: usize,
    end: usize,
    early_exit: bool,
) -> f64 {
    let block = end - start + 1;
    let size = search.len();
    let to_check = (size as isize - 2).min(block as isize - 2);
    if to_check <= 0 {
        return 1.0;
    }
    let mut similarity = 0.0;
    let mut j = 1;
    while j < size - 1 && j < block - 1 {
        let a = js_trim(original[start + j]);
        let b = js_trim(search[j]);
        let max_len = utf16_len(a).max(utf16_len(b));
        j += 1;
        if max_len == 0 {
            continue;
        }
        let term = 1.0 - levenshtein(a, b) as f64 / max_len as f64;
        if early_exit {
            similarity += term / to_check as f64;
            if similarity >= SINGLE_CANDIDATE_SIMILARITY_THRESHOLD {
                break;
            }
        } else {
            similarity += term;
        }
    }
    if early_exit {
        similarity
    } else {
        similarity / to_check as f64
    }
}

fn block_anchor(content: &str, find: &str) -> Vec<String> {
    let original: Vec<&str> = content.split('\n').collect();
    let mut search: Vec<&str> = find.split('\n').collect();
    if search.len() < 3 {
        return vec![];
    }
    if search.last() == Some(&"") {
        search.pop();
    }
    let first = js_trim(search[0]);
    let last = js_trim(search[search.len() - 1]);
    let size = search.len();
    let max_delta = 1usize.max((size as f64 * 0.25).floor() as usize);

    let mut candidates: Vec<(usize, usize)> = Vec::new();
    for i in 0..original.len() {
        if js_trim(original[i]) != first {
            continue;
        }
        for (j, line) in original.iter().enumerate().skip(i + 2) {
            if js_trim(line) == last {
                if (j - i + 1).abs_diff(size) <= max_delta {
                    candidates.push((i, j));
                }
                break;
            }
        }
    }

    match candidates.len() {
        0 => vec![],
        1 => {
            let (start, end) = candidates[0];
            if block_similarity(&original, &search, start, end, true)
                >= SINGLE_CANDIDATE_SIMILARITY_THRESHOLD
            {
                vec![line_span(content, &original, start, end).to_string()]
            } else {
                vec![]
            }
        }
        _ => {
            let mut best: Option<(usize, usize)> = None;
            let mut max = -1.0;
            for (start, end) in candidates {
                let similarity = block_similarity(&original, &search, start, end, false);
                if similarity > max {
                    max = similarity;
                    best = Some((start, end));
                }
            }
            match best {
                Some((start, end)) if max >= MULTIPLE_CANDIDATES_SIMILARITY_THRESHOLD => {
                    vec![line_span(content, &original, start, end).to_string()]
                }
                _ => vec![],
            }
        }
    }
}

fn split_whitespace_js(text: &str) -> Vec<&str> {
    text.split(is_js_whitespace)
        .filter(|w| !w.is_empty())
        .collect()
}

fn normalize_whitespace(text: &str) -> String {
    split_whitespace_js(text).join(" ")
}

/// First match of `words` joined by `\s+` inside `line` (`new RegExp(pattern)` + `line.match`).
fn match_words<'a>(line: &'a str, words: &[&str]) -> Option<&'a str> {
    if words.is_empty() {
        return Some("");
    }
    'start: for (start, _) in line.char_indices() {
        let mut pos = start;
        for (i, word) in words.iter().enumerate() {
            if i > 0 {
                let rest = &line[pos..];
                let gap = rest.len() - rest.trim_start_matches(is_js_whitespace).len();
                if gap == 0 {
                    continue 'start;
                }
                pos += gap;
            }
            if !line[pos..].starts_with(word) {
                continue 'start;
            }
            pos += word.len();
        }
        return Some(&line[start..pos]);
    }
    None
}

fn whitespace_normalized(content: &str, find: &str) -> Vec<String> {
    let normalized_find = normalize_whitespace(find);
    let lines: Vec<&str> = content.split('\n').collect();
    let words = split_whitespace_js(find);
    let mut out = Vec::new();
    for line in &lines {
        let normalized_line = normalize_whitespace(line);
        if normalized_line == normalized_find {
            out.push(line.to_string());
        } else if normalized_line.contains(&normalized_find)
            && let Some(found) = match_words(line, &words)
        {
            out.push(found.to_string());
        }
    }
    let find_lines = find.split('\n').count();
    if find_lines > 1 && find_lines <= lines.len() {
        for i in 0..=lines.len() - find_lines {
            let block = lines[i..i + find_lines].join("\n");
            if normalize_whitespace(&block) == normalized_find {
                out.push(block);
            }
        }
    }
    out
}

fn remove_indentation(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let min = lines
        .iter()
        .filter(|l| !js_trim(l).is_empty())
        .map(|l| leading_whitespace(l))
        .min();
    let Some(min) = min else {
        return text.to_string();
    };
    lines
        .iter()
        .map(|line| {
            if js_trim(line).is_empty() {
                *line
            } else {
                skip_chars(line, min)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn windows<'a>(lines: &'a [&'a str], size: usize) -> impl Iterator<Item = String> + 'a {
    let count = if size <= lines.len() {
        lines.len() - size + 1
    } else {
        0
    };
    (0..count).map(move |i| lines[i..i + size].join("\n"))
}

fn indentation_flexible(content: &str, find: &str) -> Vec<String> {
    let normalized_find = remove_indentation(find);
    let lines: Vec<&str> = content.split('\n').collect();
    let size = find.split('\n').count();
    windows(&lines, size)
        .filter(|block| remove_indentation(block) == normalized_find)
        .collect()
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let replacement = match chars.peek() {
            Some('n') => Some('\n'),
            Some('t') => Some('\t'),
            Some('r') => Some('\r'),
            Some(&q @ ('\'' | '"' | '`' | '\\' | '\n' | '$')) => Some(q),
            _ => None,
        };
        match replacement {
            Some(r) => {
                out.push(r);
                chars.next();
            }
            None => out.push(c),
        }
    }
    out
}

fn escape_normalized(content: &str, find: &str) -> Vec<String> {
    let unescaped_find = unescape(find);
    let mut out = Vec::new();
    if content.contains(&unescaped_find) {
        out.push(unescaped_find.clone());
    }
    let lines: Vec<&str> = content.split('\n').collect();
    let size = unescaped_find.split('\n').count();
    out.extend(windows(&lines, size).filter(|block| unescape(block) == unescaped_find));
    out
}

fn trimmed_boundary(content: &str, find: &str) -> Vec<String> {
    let trimmed = js_trim(find);
    if trimmed == find {
        return vec![];
    }
    let mut out = Vec::new();
    if content.contains(trimmed) {
        out.push(trimmed.to_string());
    }
    let lines: Vec<&str> = content.split('\n').collect();
    let size = find.split('\n').count();
    out.extend(windows(&lines, size).filter(|block| js_trim(block) == trimmed));
    out
}

fn context_aware(content: &str, find: &str) -> Vec<String> {
    let mut find_lines: Vec<&str> = find.split('\n').collect();
    if find_lines.len() < 3 {
        return vec![];
    }
    if find_lines.last() == Some(&"") {
        find_lines.pop();
    }
    let lines: Vec<&str> = content.split('\n').collect();
    let first = js_trim(find_lines[0]);
    let last = js_trim(find_lines[find_lines.len() - 1]);
    let mut out = Vec::new();
    for i in 0..lines.len() {
        if js_trim(lines[i]) != first {
            continue;
        }
        for j in i + 2..lines.len() {
            if js_trim(lines[j]) != last {
                continue;
            }
            let block = &lines[i..=j];
            if block.len() == find_lines.len() {
                let mut matching = 0usize;
                let mut total = 0usize;
                for k in 1..block.len() - 1 {
                    let a = js_trim(block[k]);
                    let b = js_trim(find_lines[k]);
                    if !a.is_empty() || !b.is_empty() {
                        total += 1;
                        if a == b {
                            matching += 1;
                        }
                    }
                }
                if total == 0 || matching as f64 / total as f64 >= 0.5 {
                    out.push(block.join("\n"));
                }
            }
            break;
        }
    }
    out
}

fn multi_occurrence(content: &str, find: &str) -> Vec<String> {
    if find.is_empty() {
        return vec![];
    }
    vec![find.to_string(); content.matches(find).count()]
}

fn is_disproportionate(search: &str, old: &str) -> bool {
    let old_lines = old.split('\n').count();
    let search_lines = search.split('\n').count();
    if search_lines >= (old_lines + 3).max(old_lines * 2) {
        return true;
    }
    if old_lines == 1 {
        return false;
    }
    let old_len = utf16_len(js_trim(old));
    utf16_len(js_trim(search)) > (old_len + 500).max(old_len * 4)
}

/// `replace(content, oldString, newString, replaceAll)` (`edit.ts:682-729`).
pub fn replace(content: &str, old: &str, new: &str, replace_all: bool) -> Result<String, String> {
    if old == new {
        return Err(ERR_IDENTICAL.to_string());
    }
    if old.is_empty() {
        return Err(ERR_EMPTY_OLD.to_string());
    }
    const REPLACERS: [fn(&str, &str) -> Vec<String>; 9] = [
        simple,
        line_trimmed,
        block_anchor,
        whitespace_normalized,
        indentation_flexible,
        escape_normalized,
        trimmed_boundary,
        context_aware,
        multi_occurrence,
    ];
    let mut not_found = true;
    for replacer in REPLACERS {
        for search in replacer(content, old) {
            let Some(index) = content.find(&search) else {
                continue;
            };
            not_found = false;
            if is_disproportionate(&search, old) {
                return Err(ERR_DISPROPORTIONATE.to_string());
            }
            if replace_all {
                return Ok(content.replace(&search, new));
            }
            if content.rfind(&search) != Some(index) {
                continue;
            }
            return Ok(format!(
                "{}{}{}",
                &content[..index],
                new,
                &content[index + search.len()..]
            ));
        }
    }
    Err(if not_found {
        ERR_NOT_FOUND
    } else {
        ERR_MULTIPLE
    }
    .to_string())
}

#[async_trait::async_trait]
impl Tool for EditTool {
    fn id(&self) -> &str {
        "edit"
    }

    fn description(&self) -> String {
        include_str!("../../../../opencode-ts/packages/opencode/src/tool/edit.txt").to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "$schema": crate::JSON_SCHEMA_URI,
            "type": "object",
            "properties": {
                "filePath": { "type": "string", "description": "The absolute path to the file to modify" },
                "oldString": { "type": "string", "description": "The text to replace" },
                "newString": {
                    "type": "string",
                    "description": "The text to replace it with (must be different from oldString)"
                },
                "replaceAll": {
                    "type": "boolean",
                    "description": "Replace all occurrences of oldString (default false)"
                }
            },
            "required": ["filePath", "oldString", "newString"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let params: Params = parse_args("edit", args)?;
        if params.file_path.is_empty() {
            return Err(ToolError::other("filePath is required"));
        }
        if params.old_string == params.new_string {
            return Err(ToolError::other(ERR_IDENTICAL));
        }
        let file_path = join_path(&ctx.cwd, &params.file_path);
        let shown = display(&file_path);
        assert_external_directory(ctx, &file_path, PathKind::File).await?;
        let rel = relative(&ctx.worktree, &file_path);

        let lock = lock_for(real_path(&file_path));
        let guard = lock.lock().await;

        let content_old: String;
        let mut content_new: String;
        let mut diff: String;
        if params.old_string.is_empty() {
            if tokio::fs::try_exists(&file_path).await.unwrap_or(false) {
                return Err(ToolError::other(ERR_EMPTY_OLD));
            }
            let (bom, text) = bom_split(&params.new_string);
            content_old = String::new();
            content_new = text.to_string();
            diff = trim_diff(&two_files_patch(&shown, &content_old, &content_new));
            ctx.permissions
                .ask(PermissionRequest {
                    permission: "edit".into(),
                    patterns: vec![rel.clone()],
                    always: vec!["*".into()],
                    metadata: json!({ "filepath": shown, "diff": diff }),
                })
                .await?;
            write_with_dirs(&file_path, &bom_join(&content_new, bom)).await?;
            if let Some(formatted) = hooks::format_sync(ctx, &file_path, bom).await {
                content_new = formatted;
            }
            hooks::publish_edited(ctx, &shown);
            hooks::publish_watcher(ctx, &shown, "add");
        } else {
            let info = tokio::fs::metadata(&file_path).await.ok();
            let Some(info) = info else {
                return Err(ToolError::Other(format!("File {shown} not found")));
            };
            if info.is_dir() {
                return Err(ToolError::Other(format!(
                    "Path is a directory, not a file: {shown}"
                )));
            }
            let (source_bom, source) = bom_read(&file_path).await.map_err(io_err)?;
            content_old = source;

            let crlf = content_old.contains("\r\n");
            let old = convert_to_line_ending(normalize_line_endings(&params.old_string), crlf);
            let replacement =
                convert_to_line_ending(normalize_line_endings(&params.new_string), crlf);

            let replaced = replace(
                &content_old,
                &old,
                &replacement,
                params.replace_all.unwrap_or(false),
            )
            .map_err(ToolError::Other)?;
            let (next_bom, next) = bom_split(&replaced);
            content_new = next.to_string();
            diff = trim_diff(&two_files_patch(
                &shown,
                &normalize_line_endings(&content_old),
                &normalize_line_endings(&content_new),
            ));
            ctx.permissions
                .ask(PermissionRequest {
                    permission: "edit".into(),
                    patterns: vec![rel.clone()],
                    always: vec!["*".into()],
                    metadata: json!({ "filepath": shown, "diff": diff }),
                })
                .await?;
            let desired_bom = source_bom || next_bom;
            write_with_dirs(&file_path, &bom_join(&content_new, desired_bom)).await?;
            if let Some(formatted) = hooks::format_sync(ctx, &file_path, desired_bom).await {
                content_new = formatted;
            }
            hooks::publish_edited(ctx, &shown);
            hooks::publish_watcher(ctx, &shown, "change");
            diff = trim_diff(&two_files_patch(
                &shown,
                &normalize_line_endings(&content_old),
                &normalize_line_endings(&content_new),
            ));
        }
        drop(guard);

        let (additions, deletions) = line_counts(&content_old, &content_new);
        let filediff =
            json!({ "file": shown, "patch": diff, "additions": additions, "deletions": deletions });

        let mut output = "Edit applied successfully.".to_string();
        let diagnostics = hooks::collect(ctx, &[&file_path]).await;
        let block = hooks::block(ctx, &diagnostics, &file_path, &shown);
        if !block.is_empty() {
            output.push_str(&format!("\n\nLSP errors detected in this file, please fix:\n{block}"));
        }

        Ok(ToolOutput {
            title: rel,
            output,
            metadata: json!({ "diagnostics": hooks::diagnostics_json(&diagnostics), "diff": diff, "filediff": filediff }),
            attachments: vec![],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Fixture;

    #[test]
    fn simple_and_ambiguous() {
        assert_eq!(replace("a b c", "b", "X", false).unwrap(), "a X c");
        assert_eq!(replace("a b b", "b", "X", false).unwrap_err(), ERR_MULTIPLE);
        assert_eq!(replace("a b b", "b", "X", true).unwrap(), "a X X");
        assert_eq!(
            replace("abc", "zzz", "X", false).unwrap_err(),
            ERR_NOT_FOUND
        );
        assert_eq!(replace("abc", "a", "a", false).unwrap_err(), ERR_IDENTICAL);
        assert_eq!(replace("abc", "", "a", false).unwrap_err(), ERR_EMPTY_OLD);
        assert_eq!(replace("a $& b", "a", "$&", true).unwrap(), "$& $& b");
    }

    #[test]
    fn line_trimmed_matches_indentation_differences() {
        let content = "fn a() {\n    let x = 1;\n    let y = 2;\n}\n";
        let out = replace(content, "let x = 1;\nlet y = 2;", "    let z = 3;", false).unwrap();
        assert_eq!(out, "fn a() {\n    let z = 3;\n}\n");
        assert_eq!(
            line_trimmed(content, "let x = 1;\n"),
            vec!["    let x = 1;"]
        );
    }

    #[test]
    fn block_anchor_fuzzy_middle() {
        let content = "start\n  alpha beta\n  gamma delta\nend\nother\n";
        let found = block_anchor(content, "start\n  alpha beta\n  gamma delto\nend");
        assert_eq!(found, vec!["start\n  alpha beta\n  gamma delta\nend"]);
        assert!(
            block_anchor(
                content,
                "start\n  totally different\n  nothing alike here\nend"
            )
            .is_empty()
        );
        assert!(block_anchor(content, "start\nend").is_empty());
    }

    #[test]
    fn block_anchor_picks_best_of_many() {
        let content = "begin\n  one\nfinish\nbegin\n  two\nfinish\n";
        assert_eq!(
            block_anchor(content, "begin\n  two\nfinish"),
            vec!["begin\n  two\nfinish"]
        );
    }

    #[test]
    fn whitespace_normalized_variants() {
        assert_eq!(
            whitespace_normalized("let   x  =  1;", "let x = 1;"),
            vec!["let   x  =  1;"]
        );
        assert_eq!(
            whitespace_normalized("if (a) { let   x  =  1; }", "let x = 1;"),
            vec!["let   x  =  1;"]
        );
        assert_eq!(
            whitespace_normalized("a  b\nc   d\n", "a b\nc d"),
            vec!["a  b\nc   d"]
        );
    }

    #[test]
    fn indentation_flexible_block() {
        let content = "    if x:\n        y()\n";
        assert_eq!(
            indentation_flexible(content, "if x:\n    y()"),
            vec!["    if x:\n        y()"]
        );
    }

    #[test]
    fn escape_normalized_unescapes() {
        assert_eq!(unescape(r#"a\nb\t\"q\" \$x \z"#), "a\nb\t\"q\" $x \\z");
        assert_eq!(
            escape_normalized("say \"hi\"\n", r#"say \"hi\""#)[0],
            "say \"hi\""
        );
        assert_eq!(
            replace("line1\nline2\n", "line1\\nline2", "X", false).unwrap(),
            "X\n"
        );
    }

    #[test]
    fn trimmed_boundary_and_context_aware() {
        assert_eq!(trimmed_boundary("x = foo(1)", "  foo(1)  "), vec!["foo(1)"]);
        assert!(trimmed_boundary("x", "x").is_empty());
        let content = "head\n  a\n  b\ntail\n";
        assert_eq!(
            context_aware(content, "head\n  a\n  CHANGED\ntail"),
            vec!["head\n  a\n  b\ntail"]
        );
        assert!(context_aware(content, "head\n  q\n  r\ntail").is_empty());
    }

    #[test]
    fn multi_occurrence_counts() {
        assert_eq!(multi_occurrence("aXbXc", "X").len(), 2);
    }

    #[test]
    fn disproportionate_match_is_refused() {
        let body = (0..10)
            .map(|i| format!("  line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let content = format!("start\n{body}\nend\n");
        assert!(is_disproportionate(&content, "start\nx\nend"));
        assert!(!is_disproportionate("a\nb", "a\nb"));
        assert!(!is_disproportionate(&"x".repeat(5000), "x"));
    }

    #[tokio::test]
    async fn edits_file_and_reports_diff() {
        let t = Fixture::new();
        t.write("f.rs", "fn main() {\n    println!(\"a\");\n}\n");
        let out = EditTool
            .execute(json!({"filePath": "f.rs", "oldString": "println!(\"a\");", "newString": "println!(\"b\");"}), &t.ctx())
            .await
            .unwrap();
        assert_eq!(out.output, "Edit applied successfully.");
        assert_eq!(out.title, "f.rs");
        assert_eq!(t.read("f.rs"), "fn main() {\n    println!(\"b\");\n}\n");
        assert_eq!(out.metadata["filediff"]["additions"], 1);
        assert_eq!(out.metadata["filediff"]["deletions"], 1);
        let reqs = t.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(
            (
                reqs[0].permission.as_str(),
                &reqs[0].patterns,
                &reqs[0].always
            ),
            ("edit", &vec!["f.rs".to_string()], &vec!["*".to_string()])
        );
        assert_eq!(reqs[0].metadata["diff"], out.metadata["diff"]);
    }

    #[tokio::test]
    async fn preserves_crlf_and_bom() {
        let t = Fixture::new();
        t.write("w.txt", "\u{feff}one\r\ntwo\r\nthree\r\n");
        EditTool
            .execute(
                json!({"filePath": "w.txt", "oldString": "one\ntwo", "newString": "1\n2"}),
                &t.ctx(),
            )
            .await
            .unwrap();
        assert_eq!(t.read("w.txt"), "\u{feff}1\r\n2\r\nthree\r\n");
    }

    #[tokio::test]
    async fn empty_old_string_creates_or_errors() {
        let t = Fixture::new();
        EditTool
            .execute(
                json!({"filePath": "n/new.txt", "oldString": "", "newString": "hi"}),
                &t.ctx(),
            )
            .await
            .unwrap();
        assert_eq!(t.read("n/new.txt"), "hi");
        let err = EditTool
            .execute(
                json!({"filePath": "n/new.txt", "oldString": "", "newString": "x"}),
                &t.ctx(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), ERR_EMPTY_OLD);
    }

    #[tokio::test]
    async fn error_messages() {
        let t = Fixture::new();
        let ctx = t.ctx();
        let err = EditTool
            .execute(
                json!({"filePath": "nope.txt", "oldString": "a", "newString": "b"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("File {} not found", t.root.join("nope.txt").display())
        );
        std::fs::create_dir(t.root.join("d")).unwrap();
        let err = EditTool
            .execute(
                json!({"filePath": "d", "oldString": "a", "newString": "b"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "Path is a directory, not a file: {}",
                t.root.join("d").display()
            )
        );
        let err = EditTool
            .execute(
                json!({"filePath": "d", "oldString": "a", "newString": "a"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), ERR_IDENTICAL);
        t.write("f.txt", "abc");
        let err = EditTool
            .execute(
                json!({"filePath": "f.txt", "oldString": "zz", "newString": "b"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), ERR_NOT_FOUND);
        t.deny("edit");
        let res = EditTool
            .execute(
                json!({"filePath": "f.txt", "oldString": "abc", "newString": "b"}),
                &ctx,
            )
            .await;
        assert!(matches!(res, Err(ToolError::Rejected)));
        assert_eq!(t.read("f.txt"), "abc");
    }
}
