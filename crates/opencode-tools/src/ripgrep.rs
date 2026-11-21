//! Ripgrep adapter (`core/src/ripgrep.ts`): shells out to `rg`, resolved by `ripgrep_binary`.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio_util::sync::CancellationToken;

use crate::ToolError;

const ERROR_BYTES: u64 = 8 * 1024;
const MAX_RECORD_BYTES: usize = 64 * 1024;
const MAX_TEXT_LENGTH: usize = 2000;

#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    pub path: String,
    pub line: u64,
    pub text: String,
}

pub use crate::ripgrep_binary::{
    Installer, RELEASES_URL, VERSION, bin_dir, cache_dir, filepath, find_rg, set_cache_dir, which,
    which_in,
};

pub(crate) async fn rg_or_download(
    configured: &Option<PathBuf>,
    abort: &CancellationToken,
) -> Result<PathBuf, ToolError> {
    if let Some(rg) = configured {
        return Ok(rg.clone());
    }
    tokio::select! {
        result = filepath() => result,
        _ = abort.cancelled() => Err(ToolError::Aborted),
    }
}

fn clean_path(line: &str) -> String {
    let mut rest = line;
    while let Some(next) = rest.strip_prefix("./").or_else(|| rest.strip_prefix(".\\")) {
        rest = next;
    }
    rest.trim_start_matches(['/', '\\']).replace('\\', "/")
}

async fn run<T>(
    rg: &Path,
    cwd: &Path,
    args: Vec<String>,
    limit: usize,
    abort: &CancellationToken,
    parse: impl Fn(&str) -> Result<Option<T>, ToolError>,
) -> Result<Vec<T>, ToolError> {
    let mut command = tokio::process::Command::new(rg);
    command
        .args(&args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    let mut child = command
        .spawn()
        .map_err(|e| ToolError::Other(format!("ripgrep execution failed: {e}")))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.take(ERROR_BYTES).read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf).into_owned()
    });

    let work = async {
        let mut rows = Vec::new();
        let mut reader = BufReader::new(stdout);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let read = reader
                .read_until(b'\n', &mut buf)
                .await
                .map_err(|e| ToolError::Other(format!("ripgrep execution failed: {e}")))?;
            if read == 0 {
                break;
            }
            let text = String::from_utf8_lossy(&buf);
            let line = text.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                continue;
            }
            if let Some(row) = parse(line)? {
                rows.push(row);
                if rows.len() > limit {
                    rows.truncate(limit);
                    return Ok((rows, true));
                }
            }
        }
        Ok::<_, ToolError>((rows, false))
    };

    let (rows, truncated) = tokio::select! {
        result = work => result?,
        _ = abort.cancelled() => {
            let _ = child.start_kill();
            return Err(ToolError::Aborted);
        }
    };
    if truncated {
        let _ = child.start_kill();
        return Ok(rows);
    }

    let status = child
        .wait()
        .await
        .map_err(|e| ToolError::Other(format!("ripgrep execution failed: {e}")))?;
    let stderr = stderr_task.await.unwrap_or_default();
    match status.code() {
        Some(0) => Ok(rows),
        Some(1) => Ok(Vec::new()),
        Some(2)
            if stderr.contains("regex parse error") || stderr.contains("error parsing regex") =>
        {
            Err(ToolError::Other(stderr.trim().to_string()))
        }
        Some(2) => Ok(rows),
        code => {
            let trimmed = stderr.trim();
            Err(ToolError::Other(if trimmed.is_empty() {
                format!(
                    "ripgrep failed with code {}",
                    code.map_or("null".to_string(), |c| c.to_string())
                )
            } else {
                trimmed.to_string()
            }))
        }
    }
}

/// `ripgrep.glob`: `rg --no-config --files --glob=<pattern> --glob=!**/.git/** .`
pub async fn glob(
    rg: &Path,
    cwd: &Path,
    pattern: &str,
    limit: usize,
    abort: &CancellationToken,
) -> Result<Vec<String>, ToolError> {
    let args = vec![
        "--no-config".to_string(),
        "--files".to_string(),
        format!("--glob={pattern}"),
        "--glob=!**/.git/**".to_string(),
        ".".to_string(),
    ];
    run(rg, cwd, args, limit, abort, |line| {
        Ok(Some(clean_path(line)))
    })
    .await
}

/// `ripgrep.find`: `rg --no-config --files [--hidden] --glob=<pattern> --glob=!**/.git/** .`, first `limit` paths in rg order.
pub async fn find(
    rg: &Path,
    cwd: &Path,
    pattern: &str,
    hidden: bool,
    limit: usize,
    abort: &CancellationToken,
) -> Result<Vec<String>, ToolError> {
    let mut args = vec!["--no-config".to_string(), "--files".to_string()];
    if hidden {
        args.push("--hidden".to_string());
    }
    args.extend([
        format!("--glob={pattern}"),
        "--glob=!**/.git/**".to_string(),
        ".".to_string(),
    ]);
    run(rg, cwd, args, limit, abort, |line| {
        Ok(Some(clean_path(line)))
    })
    .await
}

pub(crate) fn parse_match(line: &str) -> Result<Option<Match>, ToolError> {
    if line.len() > MAX_RECORD_BYTES {
        return Err(ToolError::Other(format!(
            "Ripgrep JSON record exceeded {MAX_RECORD_BYTES} bytes"
        )));
    }
    let json: serde_json::Value =
        serde_json::from_str(line).map_err(|_| ToolError::other("Invalid ripgrep JSON output"))?;
    if json.get("type").and_then(|t| t.as_str()) != Some("match") {
        return Ok(None);
    }
    let data = &json["data"];
    let invalid = || ToolError::other("Invalid ripgrep match output");
    let path = data["path"]["text"].as_str().ok_or_else(invalid)?;
    let line_number = data["line_number"].as_u64().ok_or_else(invalid)?;
    let text = data["lines"]["text"].as_str().ok_or_else(invalid)?;
    let text = match text.char_indices().nth(MAX_TEXT_LENGTH) {
        Some((idx, _)) => format!("{}...", &text[..idx]),
        None => text.to_string(),
    };
    Ok(Some(Match {
        path: clean_path(path),
        line: line_number,
        text,
    }))
}

/// `ripgrep.grep`: `rg --no-config --json --hidden --no-messages [--glob=<include>] --glob=!**/.git/** -- <pattern> .`
pub async fn grep(
    rg: &Path,
    cwd: &Path,
    pattern: &str,
    include: Option<&str>,
    limit: usize,
    abort: &CancellationToken,
) -> Result<Vec<Match>, ToolError> {
    let mut args: Vec<String> = ["--no-config", "--json", "--hidden", "--no-messages"]
        .map(String::from)
        .to_vec();
    if let Some(include) = include.filter(|i| !i.is_empty()) {
        args.push(format!("--glob={include}"));
    }
    args.extend(["--glob=!**/.git/**", "--", pattern, "."].map(String::from));
    run(rg, cwd, args, limit, abort, parse_match).await
}

/// Raw stdout lines of `rg <args>` (for `--files-with-matches`, `--count`, `--files`), at most `limit`.
pub(crate) async fn output_lines(
    rg: &Path,
    cwd: &Path,
    args: Vec<String>,
    limit: usize,
    abort: &CancellationToken,
) -> Result<Vec<String>, ToolError> {
    run(rg, cwd, args, limit, abort, |line| Ok(Some(line.to_string()))).await
}

/// A `match` (true) or `context` (false) record of `rg --json`.
pub(crate) fn parse_line_record(line: &str) -> Result<Option<(bool, Match)>, ToolError> {
    if line.len() > MAX_RECORD_BYTES {
        return Err(ToolError::Other(format!(
            "Ripgrep JSON record exceeded {MAX_RECORD_BYTES} bytes"
        )));
    }
    let json: serde_json::Value =
        serde_json::from_str(line).map_err(|_| ToolError::other("Invalid ripgrep JSON output"))?;
    let matched = match json.get("type").and_then(|t| t.as_str()) {
        Some("match") => true,
        Some("context") => false,
        _ => return Ok(None),
    };
    let data = &json["data"];
    let invalid = || ToolError::other("Invalid ripgrep match output");
    let path = data["path"]["text"].as_str().ok_or_else(invalid)?;
    let line_number = data["line_number"].as_u64().ok_or_else(invalid)?;
    let text = data["lines"]["text"].as_str().unwrap_or_default();
    let text = text.trim_end_matches(['\n', '\r']);
    let text = match text.char_indices().nth(MAX_TEXT_LENGTH) {
        Some((idx, _)) => format!("{}...", &text[..idx]),
        None => text.to_string(),
    };
    Ok(Some((matched, Match { path: clean_path(path), line: line_number, text })))
}

/// `rg --json <args>` match and context records, at most `limit`.
pub(crate) async fn line_records(
    rg: &Path,
    cwd: &Path,
    args: Vec<String>,
    limit: usize,
    abort: &CancellationToken,
) -> Result<Vec<(bool, Match)>, ToolError> {
    run(rg, cwd, args, limit, abort, parse_line_record).await
}

pub(crate) fn clean(line: &str) -> String {
    clean_path(line)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Submatch {
    pub text: String,
    pub start: u64,
    pub end: u64,
}

/// A match with the byte offset and submatches ripgrep reports (`Ripgrep.Match`).
#[derive(Debug, Clone, PartialEq)]
pub struct DetailedMatch {
    pub path: String,
    pub line: u64,
    pub offset: u64,
    pub text: String,
    pub submatches: Vec<Submatch>,
}

fn parse_detailed(line: &str) -> Result<Option<DetailedMatch>, ToolError> {
    let Some(found) = parse_match(line)? else {
        return Ok(None);
    };
    let json: serde_json::Value =
        serde_json::from_str(line).map_err(|_| ToolError::other("Invalid ripgrep JSON output"))?;
    let data = &json["data"];
    let submatches = data["submatches"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| Submatch {
            text: item["match"]["text"].as_str().unwrap_or_default().to_string(),
            start: item["start"].as_u64().unwrap_or(0),
            end: item["end"].as_u64().unwrap_or(0),
        })
        .collect();
    Ok(Some(DetailedMatch {
        path: found.path,
        line: found.line,
        offset: data["absolute_offset"].as_u64().unwrap_or(0),
        text: found.text,
        submatches,
    }))
}

/// `ripgrep.grep` keeping offsets and submatches.
pub async fn grep_detailed(
    rg: &Path,
    cwd: &Path,
    pattern: &str,
    limit: usize,
    abort: &CancellationToken,
) -> Result<Vec<DetailedMatch>, ToolError> {
    let args: Vec<String> = [
        "--no-config",
        "--json",
        "--hidden",
        "--no-messages",
        "--glob=!**/.git/**",
        "--",
        pattern,
        ".",
    ]
    .map(String::from)
    .to_vec();
    run(rg, cwd, args, limit, abort, parse_detailed).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_detailed_match_records() {
        let line = r#"{"type":"match","data":{"path":{"text":"./a.rs"},"lines":{"text":"let x = 1;\n"},"line_number":7,"absolute_offset":42,"submatches":[{"match":{"text":"x"},"start":4,"end":5}]}}"#;
        let found = parse_detailed(line).unwrap().unwrap();
        assert_eq!((found.path.as_str(), found.line, found.offset), ("a.rs", 7, 42));
        assert_eq!(found.submatches, [Submatch { text: "x".into(), start: 4, end: 5 }]);
    }

    #[test]
    fn cleans_paths() {
        assert_eq!(clean_path("./a/b.txt"), "a/b.txt");
        assert_eq!(clean_path(".\\.\\a\\b.txt"), "a/b.txt");
        assert_eq!(clean_path("/a"), "a");
    }

    #[test]
    fn parses_match_records() {
        let line = r#"{"type":"match","data":{"path":{"text":"./src/a.rs"},"lines":{"text":"let x = 1;\n"},"line_number":7,"absolute_offset":0,"submatches":[]}}"#;
        assert_eq!(
            parse_match(line).unwrap(),
            Some(Match {
                path: "src/a.rs".into(),
                line: 7,
                text: "let x = 1;\n".into()
            })
        );
        assert_eq!(parse_match(r#"{"type":"begin","data":{}}"#).unwrap(), None);
        assert_eq!(
            parse_match("nope").unwrap_err().to_string(),
            "Invalid ripgrep JSON output"
        );
        let long = format!(
            r#"{{"type":"match","data":{{"path":{{"text":"a"}},"lines":{{"text":"{}"}},"line_number":1}}}}"#,
            "y".repeat(2500)
        );
        let m = parse_match(&long).unwrap().unwrap();
        assert_eq!(m.text, format!("{}...", "y".repeat(2000)));
        let huge = "z".repeat(MAX_RECORD_BYTES + 1);
        assert_eq!(
            parse_match(&huge).unwrap_err().to_string(),
            "Ripgrep JSON record exceeded 65536 bytes"
        );
    }
}
