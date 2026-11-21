// `/find`, `/find/file`, `/file`, `/file/content`, `/file/status` (`handlers/file.ts`, `core/src/filesystem*.ts`).
use crate::error::{ApiError, ApiResult, json};
use crate::state::{Instance, Params};
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ignore::gitignore::GitignoreBuilder;
use opencode_tools::{ripgrep, util};
use serde_json::{Value, json as value};
use std::collections::BTreeSet;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};
use tokio_util::sync::CancellationToken;

// The ripgrep binary; a missing one is downloaded, as the original does.
async fn rg() -> ApiResult<PathBuf> {
    ripgrep::filepath().await.map_err(ApiError::internal)
}

pub async fn find_text(instance: Instance, params: Params) -> ApiResult {
    let pattern = params.required("pattern")?;
    let found = ripgrep::grep_detailed(&rg().await?, Path::new(&instance.directory), pattern, 10, &CancellationToken::new())
        .await
        .map_err(ApiError::internal)?;
    let matches: Vec<Value> = found
        .into_iter()
        .map(|item| {
            let submatches: Vec<Value> =
                item.submatches.iter().map(|sub| value!({"match": {"text": sub.text}, "start": sub.start, "end": sub.end})).collect();
            value!({
                "path": {"text": item.path},
                "lines": {"text": item.text},
                "line_number": item.line,
                "absolute_offset": item.offset,
                "submatches": submatches,
            })
        })
        .collect();
    Ok(json(matches))
}

// Subsequence match, case-insensitive. Higher is better: contiguous runs and matches right after a separator count most.
fn fuzzy_score(query: &str, target: &str) -> Option<i64> {
    let needle: Vec<char> = query.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    if needle.is_empty() {
        return None;
    }
    let hay: Vec<char> = target.to_lowercase().chars().collect();
    let mut score = 0i64;
    let mut next = 0usize;
    let mut previous: Option<usize> = None;
    for (index, c) in hay.iter().enumerate() {
        if next == needle.len() {
            break;
        }
        if *c != needle[next] {
            continue;
        }
        let boundary = index == 0 || matches!(hay[index - 1], '/' | '\\' | '_' | '-' | '.' | ' ');
        score += match previous {
            Some(last) if last + 1 == index => 8,
            _ if boundary => 6,
            _ => 1,
        };
        previous = Some(index);
        next += 1;
    }
    (next == needle.len()).then(|| score * 16 - hay.len() as i64)
}

pub async fn find_file(instance: Instance, params: Params) -> ApiResult {
    let query = params.required("query")?;
    let dirs = params.bool("dirs")?;
    let kind = match params.get("type") {
        None => (dirs == Some(false)).then_some("file"),
        Some(kind @ ("file" | "directory")) => Some(kind),
        Some(other) => return Err(ApiError::schema("Query", format!("Expected \"file\" | \"directory\", got \"{other}\" at [\"type\"]"))),
    };
    let limit = match params.number("limit")? {
        None => 10,
        Some(n) if n.fract() == 0.0 && (1.0..=200.0).contains(&n) => n as usize,
        Some(n) => return Err(ApiError::schema("Query", format!("Expected an integer between 1 and 200, got {n} at [\"limit\"]"))),
    };
    Ok(json(search(&instance, query, kind, limit).await?))
}

// Fuzzy matches among the directory's files and their parent directories (which end in a separator).
pub async fn search(instance: &Instance, query: &str, kind: Option<&str>, limit: usize) -> ApiResult<Vec<String>> {
    if let Some(found) = browse(instance, query, kind, limit) {
        return Ok(found);
    }
    let cap = if instance.worktree.is_some() { usize::MAX } else { 100_000 };
    let files = ripgrep::find(&rg().await?, Path::new(&instance.directory), "*", false, cap, &CancellationToken::new())
        .await
        .map_err(ApiError::internal)?;
    let mut directories: BTreeSet<String> = BTreeSet::new();
    if kind != Some("file") {
        for file in &files {
            let parts: Vec<&str> = file.split('/').collect();
            for end in 1..parts.len() {
                directories.insert(format!("{}{MAIN_SEPARATOR}", parts[..end].join("/")));
            }
        }
    }
    let files = if kind == Some("directory") { Vec::new() } else { files };
    let mut scored: Vec<(i64, String)> =
        files.into_iter().chain(directories).filter_map(|item| fuzzy_score(query, &item).map(|score| (score, item))).collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    Ok(scored.into_iter().take(limit).map(|(_, item)| item).collect())
}

// Empty or path-like queries ("a/b/", "a/b/pa"): list that folder's entries directly, as the original's finder matches
// path prefixes. A capped ripgrep scan misses deep paths and skips hidden folders such as AppData on Windows.
fn browse(instance: &Instance, query: &str, kind: Option<&str>, limit: usize) -> Option<Vec<String>> {
    let query = query.trim().replace('\\', "/");
    if !query.is_empty() && !query.contains('/') {
        return None;
    }
    let (parent, partial) = match query.rfind('/') {
        Some(i) => (&query[..=i], &query[i + 1..]),
        None => ("", ""),
    };
    let dir = Path::new(&instance.directory).join(parent);
    if !util::contains(Path::new(&instance.directory), &dir) || !dir.is_dir() {
        return None;
    }
    let mut entries: Vec<(bool, String)> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let is_dir = entry.file_type().ok()?.is_dir();
            let name = entry.file_name().to_string_lossy().into_owned();
            let wanted = match kind {
                Some("directory") => is_dir,
                Some("file") => !is_dir,
                _ => true,
            };
            (wanted && (partial.is_empty() || fuzzy_score(partial, &name).is_some())).then_some((is_dir, name))
        })
        .collect();
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| locale_cmp(&a.1, &b.1)));
    Some(
        entries
            .into_iter()
            .take(limit)
            .map(|(is_dir, name)| if is_dir { format!("{parent}{name}{MAIN_SEPARATOR}") } else { format!("{parent}{name}") })
            .collect(),
    )
}

// `path.resolve(directory, input)`, which must stay inside the directory, also after following links.
pub fn resolve(directory: &str, input: &str) -> ApiResult<PathBuf> {
    let root = Path::new(directory);
    let absolute = util::resolve(root, input);
    let escapes = || ApiError::internal("Path escapes the location");
    if !util::contains(root, &absolute) {
        return Err(escapes());
    }
    if absolute.exists() && !util::contains(&util::real_path(root), &util::real_path(&absolute)) {
        return Err(escapes());
    }
    Ok(absolute)
}

pub fn locale_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| b.cmp(a))
}

pub async fn list(instance: Instance, params: Params) -> ApiResult {
    let directory = Path::new(&instance.directory);
    let target = resolve(&instance.directory, params.required("path")?)?;
    if !target.is_dir() {
        return Err(ApiError::internal("Path is not a directory"));
    }
    let project = instance.worktree.clone().unwrap_or_else(|| directory.to_path_buf());
    let mut builder = GitignoreBuilder::new(&project);
    for name in [".gitignore", ".ignore"] {
        for line in std::fs::read_to_string(project.join(name)).unwrap_or_default().lines() {
            let _ = builder.add_line(None, line);
        }
    }
    let ignored = builder.build().map_err(ApiError::internal)?;

    let mut entries: Vec<(bool, String, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(&target).map_err(ApiError::internal)?.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        if !kind.is_dir() && !kind.is_file() {
            continue;
        }
        let absolute = target.join(entry.file_name());
        let mut relative = util::relative(directory, &absolute);
        if cfg!(windows) {
            relative = relative.replace('/', "\\");
        }
        if kind.is_dir() {
            relative.push(MAIN_SEPARATOR);
        }
        entries.push((kind.is_dir(), relative, absolute));
    }
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| locale_cmp(&a.1, &b.1)));
    let nodes: Vec<Value> = entries
        .into_iter()
        .map(|(is_dir, relative, absolute)| {
            let hidden = absolute.strip_prefix(&project).is_ok_and(|inside| ignored.matched_path_or_any_parents(inside, is_dir).is_ignore());
            value!({
                "name": absolute.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default(),
                "path": relative,
                "absolute": absolute.to_string_lossy(),
                "type": if is_dir { "directory" } else { "file" },
                "ignored": hidden,
            })
        })
        .collect();
    Ok(json(nodes))
}

// The common part of the `mime-types` table; anything else is `application/octet-stream`.
pub fn mime_type(path: &Path) -> &'static str {
    let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or_default().to_lowercase();
    match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "ico" => "image/vnd.microsoft.icon",
        "svg" => "image/svg+xml",
        "avif" => "image/avif",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "wasm" => "application/wasm",
        "json" => "application/json",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "txt" => "text/plain",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "md" => "text/markdown",
        _ => "application/octet-stream",
    }
}

pub async fn content(instance: Instance, params: Params) -> ApiResult {
    let file = resolve(&instance.directory, params.required("path")?)?;
    if !file.exists() {
        return Ok(json(value!({"type": "text", "content": ""})));
    }
    if !file.is_file() {
        return Err(ApiError::internal("Path is not a file"));
    }
    let bytes = std::fs::read(&file).map_err(ApiError::internal)?;
    let text = if bytes.contains(&0) { None } else { String::from_utf8(bytes.clone()).ok() };
    Ok(json(match text {
        Some(text) => value!({"type": "text", "content": util::js_trim(&text)}),
        None => value!({"type": "binary", "content": STANDARD.encode(&bytes), "encoding": "base64", "mimeType": mime_type(&file)}),
    }))
}

pub async fn status() -> Response {
    json(Vec::<Value>::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_ranking_prefers_tight_matches() {
        assert!(fuzzy_score("", "a").is_none() && fuzzy_score("zz", "main.rs").is_none());
        let score = |target: &str| fuzzy_score("main", target).unwrap();
        assert!(score("src/main.rs") > score("src/domain/in.rs"));
        assert!(score("main.rs") > score("src/main.rs"));
        assert!(fuzzy_score("MaIn", "SRC/MAIN.rs").is_some());
    }

    #[test]
    fn mime_lookup_falls_back() {
        assert_eq!((mime_type(Path::new("a.PNG")), mime_type(Path::new("a.bin"))), ("image/png", "application/octet-stream"));
    }
}
