//! Path, text, BOM, id and diff helpers shared by the tools.

use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use similar::{ChangeTag, TextDiff};

use crate::{PermissionRequest, ToolContext, ToolError};

// ---------- JS string semantics ----------

/// JS `WhiteSpace` + `LineTerminator` (the set used by `String.prototype.trim` and regex `\s`).
pub fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

pub fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

pub fn js_trim_end(s: &str) -> &str {
    s.trim_end_matches(is_js_whitespace)
}

pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Number of leading JS-whitespace chars (`/^(\s*)/` match length).
pub fn leading_whitespace(s: &str) -> usize {
    s.chars().take_while(|c| is_js_whitespace(*c)).count()
}

pub fn skip_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[i..],
        None => "",
    }
}

// ---------- paths ----------

/// Lexical normalization (`path.normalize` without touching the filesystem).
pub fn normalize(path: &Path) -> PathBuf {
    let mut out: Vec<Component> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir) | Some(Component::Prefix(_)) => {}
                _ => out.push(comp),
            },
            other => out.push(other),
        }
    }
    out.iter().collect()
}

/// `path.resolve(base, p)` for an absolute `base`.
pub fn resolve(base: &Path, p: &str) -> PathBuf {
    let candidate = Path::new(p);
    if candidate.is_absolute() {
        normalize(candidate)
    } else {
        normalize(&base.join(candidate))
    }
}

fn comp_eq(a: &Component, b: &Component) -> bool {
    if cfg!(windows) {
        a.as_os_str().to_string_lossy().to_lowercase()
            == b.as_os_str().to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

/// `path.relative(from, to)`; returns `to` when the roots differ (Windows drives).
pub fn relative(from: &Path, to: &Path) -> String {
    let from = normalize(from);
    let to = normalize(to);
    let a: Vec<Component> = from.components().collect();
    let b: Vec<Component> = to.components().collect();
    let root_len = |c: &[Component]| {
        c.iter()
            .take_while(|x| !matches!(x, Component::Normal(_)))
            .count()
    };
    let (ra, rb) = (root_len(&a), root_len(&b));
    if ra != rb || !a[..ra].iter().zip(&b[..rb]).all(|(x, y)| comp_eq(x, y)) {
        return to.to_string_lossy().into_owned();
    }
    let mut common = ra;
    while common < a.len() && common < b.len() && comp_eq(&a[common], &b[common]) {
        common += 1;
    }
    let mut parts: Vec<String> = Vec::new();
    for _ in common..a.len() {
        parts.push("..".to_string());
    }
    for comp in &b[common..] {
        parts.push(comp.as_os_str().to_string_lossy().into_owned());
    }
    parts.join(std::path::MAIN_SEPARATOR_STR)
}

/// `FSUtil.contains(parent, child)`.
pub fn contains(parent: &Path, child: &Path) -> bool {
    let rel = relative(parent, child);
    rel.is_empty()
        || (!Path::new(&rel).is_absolute()
            && rel != ".."
            && !rel.starts_with(&format!("..{}", std::path::MAIN_SEPARATOR)))
}

/// `containsPath(filepath, instance)`: inside `cwd`, or inside `worktree` unless it is `/`.
pub fn contains_path(ctx: &ToolContext, path: &Path) -> bool {
    let check = |cwd: &Path, worktree: &Path| {
        contains(cwd, path) || (worktree != Path::new("/") && contains(worktree, path))
    };
    if check(&ctx.cwd, &ctx.worktree) {
        return true;
    }
    // Windows only: the target was canonicalized, so compare against canonical roots too.
    cfg!(windows)
        && ctx.worktree != Path::new("/")
        && check(&normalize_path(&ctx.cwd), &normalize_path(&ctx.worktree))
}

fn drive_prefix(p: &str, prefix: &str, colon: bool) -> Option<String> {
    let rest = p.strip_prefix(prefix)?;
    let mut chars = rest.chars();
    let drive = chars.next().filter(|c| c.is_ascii_alphabetic())?;
    let mut tail = chars.as_str();
    if colon {
        tail = tail.strip_prefix(':')?;
        if !(tail.is_empty() || tail.starts_with('/') || tail.starts_with('\\')) {
            return None;
        }
    } else if !(tail.is_empty() || tail.starts_with('/')) {
        return None;
    }
    let tail = if tail.is_empty() { "" } else { &tail[1..] };
    Some(format!("{}:/{}", drive.to_ascii_uppercase(), tail))
}

fn windows_path_impl(p: &str) -> String {
    drive_prefix(p, "/", true)
        .or_else(|| drive_prefix(p, "/", false))
        .or_else(|| drive_prefix(p, "/cygdrive/", false))
        .or_else(|| drive_prefix(p, "/mnt/", false))
        .unwrap_or_else(|| p.to_string())
}

/// `FSUtil.windowsPath`: `/c/x`, `/cygdrive/c/x`, `/mnt/c/x` -> `C:/x` (win32 only).
pub fn windows_path(p: &str) -> String {
    if cfg!(windows) {
        windows_path_impl(p)
    } else {
        p.to_string()
    }
}

/// `FSUtil.normalizePath`: on win32 resolve + native realpath (falls back to the resolved path).
pub fn normalize_path(p: &Path) -> PathBuf {
    if !cfg!(windows) {
        return p.to_path_buf();
    }
    let converted = PathBuf::from(windows_path(&p.to_string_lossy()));
    let resolved = normalize(&std::path::absolute(&converted).unwrap_or(converted));
    dunce::canonicalize(&resolved).unwrap_or(resolved)
}

/// `FSUtil.normalizePathPattern`.
pub fn normalize_path_pattern(p: &str) -> String {
    if !cfg!(windows) || p == "*" {
        return p.to_string();
    }
    let dir = p.strip_suffix("\\*").or_else(|| p.strip_suffix("/*"));
    let Some(dir) = dir else {
        return normalize_path(Path::new(p)).to_string_lossy().into_owned();
    };
    let bytes = dir.as_bytes();
    let dir = if bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        format!("{dir}\\")
    } else {
        dir.to_string()
    };
    normalize_path(Path::new(&dir))
        .join("*")
        .to_string_lossy()
        .into_owned()
}

/// `FSUtil.resolve`: absolute + realpath (missing paths stay lexical).
pub fn real_path(p: &Path) -> PathBuf {
    let converted = PathBuf::from(windows_path(&p.to_string_lossy()));
    let resolved = normalize(&std::path::absolute(&converted).unwrap_or(converted));
    dunce::canonicalize(&resolved).unwrap_or(resolved)
}

pub fn display(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    File,
    Directory,
}

/// `assertExternalDirectoryEffect` (`external-directory.ts:15-45`).
pub async fn assert_external_directory(
    ctx: &ToolContext,
    target: &Path,
    kind: PathKind,
) -> Result<bool, ToolError> {
    let full = normalize_path(target);
    if contains_path(ctx, &full) {
        return Ok(false);
    }
    let dir = match kind {
        PathKind::Directory => full.clone(),
        PathKind::File => full
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| full.clone()),
    };
    let joined = display(&dir.join("*"));
    let glob = if cfg!(windows) {
        normalize_path_pattern(&joined)
    } else {
        joined.replace('\\', "/")
    };
    ctx.permissions
        .ask(PermissionRequest {
            permission: "external_directory".into(),
            patterns: vec![glob.clone()],
            always: vec![glob],
            metadata: json!({ "filepath": display(&full), "parentDir": display(&dir) }),
        })
        .await?;
    Ok(true)
}

/// `fs.writeWithDirs`: write, creating parent directories when missing.
pub async fn write_with_dirs(path: &Path, content: &str) -> Result<(), ToolError> {
    match tokio::fs::write(path, content).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(io_err)?;
            }
            tokio::fs::write(path, content).await.map_err(io_err)
        }
        Err(e) => Err(io_err(e)),
    }
}

pub fn io_err(e: std::io::Error) -> ToolError {
    ToolError::Other(e.to_string())
}

// ---------- BOM ----------

pub const BOM: char = '\u{feff}';

pub fn bom_split(text: &str) -> (bool, &str) {
    match text.strip_prefix(BOM) {
        Some(rest) => (true, rest),
        None => (false, text),
    }
}

pub fn bom_join(text: &str, bom: bool) -> String {
    let stripped = bom_split(text).1;
    if bom {
        format!("{BOM}{stripped}")
    } else {
        stripped.to_string()
    }
}

/// `Bom.readFile`: lossy UTF-8 decode, BOM split off.
pub async fn bom_read(path: &Path) -> std::io::Result<(bool, String)> {
    let bytes = tokio::fs::read(path).await?;
    let text = String::from_utf8_lossy(&bytes);
    let (bom, rest) = bom_split(&text);
    Ok((bom, rest.to_string()))
}

// ---------- ids ----------

static ID_STATE: Mutex<(u64, u64)> = Mutex::new((0, 0));

fn random_base62(len: usize) -> String {
    use std::hash::{BuildHasher, Hasher};
    const CHARS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut out = String::with_capacity(len);
    let mut pool = 0u64;
    let mut left = 0;
    for _ in 0..len {
        if left == 0 {
            pool = std::collections::hash_map::RandomState::new()
                .build_hasher()
                .finish();
            left = 8;
        }
        out.push(CHARS[(pool & 0xff) as usize % 62] as char);
        pool >>= 8;
        left -= 1;
    }
    out
}

/// `ToolID.ascending()`: `tool_` + 12 hex (ms * 0x1000 + counter) + 14 base62.
pub fn tool_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let value = {
        let mut state = ID_STATE.lock().unwrap_or_else(|e| e.into_inner());
        if state.0 != now {
            *state = (now, 0);
        }
        state.1 += 1;
        now.wrapping_mul(0x1000).wrapping_add(state.1)
    };
    format!(
        "tool_{:012x}{}",
        value & 0xffff_ffff_ffff,
        random_base62(14)
    )
}

// ---------- diff ----------

/// Stand-in for jsdiff `createTwoFilesPatch(name, name, old, new)` (4 context lines).
pub fn two_files_patch(name: &str, old: &str, new: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut out = format!(
        "Index: {name}\n{}\n--- {name}\n+++ {name}\n",
        "=".repeat(67)
    );
    for group in diff.grouped_ops(4) {
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            continue;
        };
        let old_range = first.old_range().start..last.old_range().end;
        let new_range = first.new_range().start..last.new_range().end;
        let start = |r: &std::ops::Range<usize>| if r.is_empty() { r.start } else { r.start + 1 };
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            start(&old_range),
            old_range.len(),
            start(&new_range),
            new_range.len()
        ));
        for op in &group {
            for change in diff.iter_changes(op) {
                out.push(match change.tag() {
                    ChangeTag::Delete => '-',
                    ChangeTag::Insert => '+',
                    ChangeTag::Equal => ' ',
                });
                let value: &str = change.value();
                out.push_str(value);
                if !value.ends_with('\n') {
                    out.push_str("\n\\ No newline at end of file\n");
                }
            }
        }
    }
    out
}

/// `(additions, deletions)` as summed from jsdiff `diffLines` counts.
pub fn line_counts(old: &str, new: &str) -> (usize, usize) {
    let diff = TextDiff::from_lines(old, new);
    let mut counts = (0, 0);
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => counts.0 += 1,
            ChangeTag::Delete => counts.1 += 1,
            ChangeTag::Equal => {}
        }
    }
    counts
}

fn is_diff_content(line: &str) -> bool {
    (line.starts_with('+') || line.starts_with('-') || line.starts_with(' '))
        && !line.starts_with("---")
        && !line.starts_with("+++")
}

/// `trimDiff` (`edit.ts:646-680`): strip the common indentation of all hunk lines.
pub fn trim_diff(diff: &str) -> String {
    let lines: Vec<&str> = diff.split('\n').collect();
    let mut min: Option<usize> = None;
    let mut any = false;
    for line in lines.iter().filter(|l| is_diff_content(l)) {
        any = true;
        let content = &line[1..];
        if !js_trim(content).is_empty() {
            let indent = leading_whitespace(content);
            min = Some(min.map_or(indent, |m| m.min(indent)));
        }
    }
    let Some(min) = min.filter(|m| any && *m > 0) else {
        return diff.to_string();
    };
    lines
        .iter()
        .map(|line| {
            if is_diff_content(line) {
                format!("{}{}", &line[..1], skip_chars(&line[1..], min))
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------- serde ----------

/// Non-negative integer that also accepts integral JSON floats (`2000.0`), like a JS number would.
pub fn de_opt_uint<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Option<u64>, D::Error> {
    use serde::Deserialize;
    use serde::de::Error;
    let Some(value) = Option::<serde_json::Number>::deserialize(de)? else {
        return Ok(None);
    };
    if let Some(n) = value.as_u64() {
        return Ok(Some(n));
    }
    match value.as_f64() {
        Some(f) if f >= 0.0 && f.fract() == 0.0 && f <= 9_007_199_254_740_991.0 => {
            Ok(Some(f as u64))
        }
        _ => Err(D::Error::custom(format!(
            "expected a non-negative integer, got {value}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_and_relative() {
        let sep = std::path::MAIN_SEPARATOR;
        let root = if cfg!(windows) { "C:\\proj" } else { "/proj" };
        let base = Path::new(root);
        assert_eq!(
            resolve(base, "a/./b/../c.txt"),
            base.join("a").join("c.txt")
        );
        assert_eq!(
            relative(base, &base.join("a").join("c.txt")),
            format!("a{sep}c.txt")
        );
        assert_eq!(relative(base, base), "");
        assert_eq!(
            relative(&base.join("x"), &base.join("y")),
            format!("..{sep}y")
        );
        assert!(contains(base, &base.join("a")));
        assert!(contains(base, base));
        assert!(!contains(&base.join("a"), base));
        assert!(!contains(&base.join("a"), &base.join("ab")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_specifics() {
        assert_eq!(relative(Path::new("C:\\a"), Path::new("D:\\b")), "D:\\b");
        assert_eq!(
            relative(Path::new("C:\\Proj"), Path::new("c:\\proj\\X")),
            "X"
        );
        assert_eq!(windows_path("/c/Users/x"), "C:/Users/x");
        assert_eq!(windows_path("/cygdrive/d/x"), "D:/x");
        assert_eq!(windows_path("/mnt/e"), "E:/");
        assert_eq!(windows_path("/c:/x"), "C:/x");
        assert_eq!(windows_path("/usr/bin"), "/usr/bin");
        assert_eq!(normalize_path_pattern("*"), "*");
    }

    #[test]
    fn js_whitespace_rules() {
        assert_eq!(js_trim("\u{feff} a\u{a0}"), "a");
        assert_eq!(js_trim("\u{85}a"), "\u{85}a");
        assert_eq!(leading_whitespace("\t  x"), 3);
        assert_eq!(utf16_len("a😀"), 3);
    }

    #[test]
    fn bom_roundtrip() {
        assert_eq!(bom_split("\u{feff}x"), (true, "x"));
        assert_eq!(bom_join("x", true), "\u{feff}x");
        assert_eq!(bom_join("\u{feff}x", false), "x");
    }

    #[test]
    fn tool_ids_ascend() {
        let a = tool_id();
        let b = tool_id();
        assert!(a.starts_with("tool_") && a.len() == 5 + 12 + 14);
        assert!(a[..17] < b[..17]);
    }

    #[test]
    fn patch_format() {
        let patch = two_files_patch("f.txt", "a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(
            patch,
            format!(
                "Index: f.txt\n{}\n--- f.txt\n+++ f.txt\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n",
                "=".repeat(67)
            )
        );
        let added = two_files_patch("f", "", "x\ny");
        assert!(added.ends_with("@@ -0,0 +1,2 @@\n+x\n+y\n\\ No newline at end of file\n"));
        assert_eq!(line_counts("a\nb\n", "a\nc\nd\n"), (2, 1));
    }

    #[test]
    fn trim_diff_strips_common_indent() {
        let diff = "--- a\n+++ a\n@@ -1,2 +1,2 @@\n     keep\n-    old\n+      new\n";
        assert_eq!(
            trim_diff(diff),
            "--- a\n+++ a\n@@ -1,2 +1,2 @@\n keep\n-old\n+  new\n"
        );
        assert_eq!(trim_diff("@@\n+x\n"), "@@\n+x\n");
    }

    #[test]
    fn uint_accepts_integral_floats() {
        #[derive(serde::Deserialize)]
        struct P {
            #[serde(default, deserialize_with = "de_opt_uint")]
            n: Option<u64>,
        }
        assert_eq!(
            serde_json::from_str::<P>(r#"{"n": 5.0}"#).unwrap().n,
            Some(5)
        );
        assert_eq!(serde_json::from_str::<P>("{}").unwrap().n, None);
        assert!(serde_json::from_str::<P>(r#"{"n": -1}"#).is_err());
        assert!(serde_json::from_str::<P>(r#"{"n": 1.5}"#).is_err());
    }
}
