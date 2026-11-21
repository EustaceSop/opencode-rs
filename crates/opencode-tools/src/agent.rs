//! Rust build, "agent" tool profile: the Glob and Grep behaviour that profile's descriptions promise (Glob newest
//! first; Grep output modes, case folding, context and limits). Ids, permissions and requests match `glob` / `grep`.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{Value, json};

use crate::util::{PathKind, assert_external_directory, de_opt_uint, display, real_path, relative, resolve};
use crate::write::join_path;
use crate::{PermissionRequest, Tool, ToolContext, ToolError, ToolOutput, parse_args, ripgrep};

pub const GLOB_LIMIT: usize = 100;
pub const FILES_LIMIT: usize = 100;
pub const CONTENT_LIMIT: usize = 200;
// Paths gathered before sorting by modification time.
const SCAN: usize = 10_000;

fn modified(path: &Path) -> SystemTime {
    std::fs::metadata(path).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Absolute paths, most recently modified first (ties by path).
fn newest_first(base: &Path, files: &[String]) -> Vec<PathBuf> {
    let mut stamped: Vec<(SystemTime, PathBuf)> = files
        .iter()
        .map(|file| {
            let path = resolve(base, file);
            (modified(&path), path)
        })
        .collect();
    stamped.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    stamped.into_iter().map(|(_, path)| path).collect()
}

#[derive(Debug, serde::Deserialize)]
struct GlobParams {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

#[derive(Default)]
pub struct GlobTool {
    /// Explicit `rg` binary; `ripgrep::filepath()` when unset.
    pub rg: Option<PathBuf>,
}

#[async_trait::async_trait]
impl Tool for GlobTool {
    fn id(&self) -> &str {
        "glob"
    }

    fn description(&self) -> String {
        crate::glob::GlobTool::default().description()
    }

    fn parameters(&self) -> Value {
        crate::glob::GlobTool::default().parameters()
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let params: GlobParams = parse_args("glob", args)?;
        let mut metadata = json!({ "pattern": params.pattern });
        if let Some(path) = &params.path {
            metadata["path"] = json!(path);
        }
        ctx.permissions
            .ask(PermissionRequest { permission: "glob".into(), patterns: vec![params.pattern.clone()], always: vec!["*".into()], metadata })
            .await?;
        let search = match &params.path {
            Some(path) => resolve(&ctx.cwd, path),
            None => ctx.cwd.clone(),
        };
        if tokio::fs::metadata(&search).await.is_ok_and(|m| m.is_file()) {
            return Err(ToolError::Other(format!("Glob path must be a directory: {}", display(&search))));
        }
        assert_external_directory(ctx, &search, PathKind::Directory).await?;

        let rg = ripgrep::rg_or_download(&self.rg, &ctx.abort).await?;
        let found = ripgrep::glob(&rg, &search, &params.pattern, SCAN + 1, &ctx.abort).await?;
        let more = found.len() > SCAN;
        let total = found.len().min(SCAN);
        let sorted = newest_first(&search, &found[..total]);
        let shown: Vec<String> = sorted.iter().take(GLOB_LIMIT).map(|p| display(p)).collect();
        let truncated = total > GLOB_LIMIT;
        let mut output = if shown.is_empty() { "No files found".to_string() } else { shown.join("\n") };
        if truncated {
            let plus = if more { "+" } else { "" };
            output.push_str(&format!(
                "\n\n(Showing the {GLOB_LIMIT} most recently modified of {total}{plus} matching files. Use a more specific pattern or path to see the rest.)"
            ));
        }
        Ok(ToolOutput {
            title: relative(&ctx.worktree, &search),
            output,
            metadata: json!({ "count": shown.len(), "truncated": truncated }),
            attachments: vec![],
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Files,
    Content,
    Count,
}

#[derive(Debug, serde::Deserialize)]
struct GrepParams {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    include: Option<String>,
    #[serde(default)]
    output_mode: Option<String>,
    #[serde(default, rename = "-i")]
    insensitive: Option<bool>,
    #[serde(default, deserialize_with = "de_opt_uint")]
    context: Option<u64>,
    #[serde(default, deserialize_with = "de_opt_uint")]
    head_limit: Option<u64>,
}

#[derive(Default)]
pub struct GrepTool {
    /// Explicit `rg` binary; `ripgrep::filepath()` when unset.
    pub rg: Option<PathBuf>,
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

#[async_trait::async_trait]
impl Tool for GrepTool {
    fn id(&self) -> &str {
        "grep"
    }

    fn description(&self) -> String {
        crate::grep::GrepTool::default().description()
    }

    fn parameters(&self) -> Value {
        crate::grep::GrepTool::default().parameters()
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let params: GrepParams = parse_args("grep", args)?;
        if params.pattern.is_empty() {
            return Err(ToolError::other("pattern is required"));
        }
        let mode = match params.output_mode.as_deref().unwrap_or("files_with_matches") {
            "files_with_matches" => Mode::Files,
            "content" => Mode::Content,
            "count" => Mode::Count,
            other => {
                return Err(ToolError::InvalidArguments {
                    tool: "grep".into(),
                    detail: format!("output_mode must be files_with_matches, content or count, got \"{other}\""),
                });
            }
        };
        let mut metadata = json!({ "pattern": params.pattern });
        if let Some(path) = &params.path {
            metadata["path"] = json!(path);
        }
        if let Some(include) = &params.include {
            metadata["include"] = json!(include);
        }
        ctx.permissions
            .ask(PermissionRequest { permission: "grep".into(), patterns: vec![params.pattern.clone()], always: vec!["*".into()], metadata })
            .await?;

        let requested = match &params.path {
            Some(path) => join_path(&ctx.cwd, path),
            None => ctx.cwd.clone(),
        };
        let requested_is_dir = tokio::fs::metadata(&requested).await.is_ok_and(|m| m.is_dir());
        assert_external_directory(ctx, &requested, if requested_is_dir { PathKind::Directory } else { PathKind::File }).await?;
        let search = real_path(&requested);
        let (cwd, target, base) = if tokio::fs::metadata(&search).await.is_ok_and(|m| m.is_dir()) {
            (search.clone(), ".".to_string(), requested.clone())
        } else {
            let name = search.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let parent = |p: &Path| p.parent().map(Path::to_path_buf).unwrap_or_else(|| p.to_path_buf());
            (parent(&search), name, parent(&requested))
        };

        let mut args: Vec<String> = ["--no-config", "--hidden", "--no-messages"].map(String::from).to_vec();
        if params.insensitive == Some(true) {
            args.push("--ignore-case".into());
        }
        if let Some(include) = params.include.as_deref().filter(|i| !i.is_empty()) {
            args.push(format!("--glob={include}"));
        }
        args.push("--glob=!**/.git/**".into());
        let rg = ripgrep::rg_or_download(&self.rg, &ctx.abort).await?;
        let tail = |args: &mut Vec<String>| args.extend(["--".to_string(), params.pattern.clone(), target.clone()]);
        let absolute = |rel: &str| display(&resolve(&base, rel));

        let (output, matches, truncated) = match mode {
            Mode::Files => {
                let limit = params.head_limit.map_or(FILES_LIMIT, |n| n.max(1) as usize);
                args.push("--files-with-matches".into());
                tail(&mut args);
                let found = ripgrep::output_lines(&rg, &cwd, args, SCAN + 1, &ctx.abort).await?;
                let more = found.len() > SCAN;
                let files: Vec<String> = found.iter().take(SCAN).map(|line| ripgrep::clean(line)).collect();
                let sorted = newest_first(&base, &files);
                let total = sorted.len();
                let shown: Vec<String> = sorted.iter().take(limit).map(|p| display(p)).collect();
                let truncated = total > limit;
                let output = if shown.is_empty() {
                    "No matches found".to_string()
                } else {
                    let plus = if more { "+" } else { "" };
                    let mut text = format!("Found {total}{plus} {}\n{}", if total == 1 { "file" } else { "files" }, shown.join("\n"));
                    if truncated {
                        text.push_str(&format!("\n\n(Showing the {limit} most recently modified files. Raise head_limit or narrow the search to see more.)"));
                    }
                    text
                };
                (output, total, truncated)
            }
            Mode::Count => {
                let limit = params.head_limit.map_or(FILES_LIMIT, |n| n.max(1) as usize);
                args.extend(["--count".to_string(), "--with-filename".to_string()]);
                tail(&mut args);
                let found = ripgrep::output_lines(&rg, &cwd, args, SCAN + 1, &ctx.abort).await?;
                let mut counts: Vec<(String, u64)> = found
                    .iter()
                    .take(SCAN)
                    .filter_map(|line| line.rsplit_once(':').and_then(|(path, n)| Some((absolute(&ripgrep::clean(path)), n.trim().parse().ok()?))))
                    .collect();
                counts.sort();
                let lines: u64 = counts.iter().map(|(_, n)| n).sum();
                let total = counts.len();
                let truncated = total > limit;
                let output = if counts.is_empty() {
                    "No matches found".to_string()
                } else {
                    let rows: Vec<String> = counts.iter().take(limit).map(|(path, n)| format!("{path}:{n}")).collect();
                    let mut text = format!("{}\n\nFound {} in {}", rows.join("\n"), plural(lines as usize, "matching line", "matching lines"), plural(total, "file", "files"));
                    if truncated {
                        text.push_str(&format!(" (showing {limit}; raise head_limit to see more)"));
                    }
                    text
                };
                (output, lines as usize, truncated)
            }
            Mode::Content => {
                let limit = params.head_limit.map_or(CONTENT_LIMIT, |n| n.max(1) as usize);
                let context = params.context.unwrap_or(0);
                args.extend(["--json".to_string(), "--sort=path".to_string()]);
                if context > 0 {
                    args.extend(["--context".to_string(), context.to_string()]);
                }
                tail(&mut args);
                let mut records = ripgrep::line_records(&rg, &cwd, args, limit + 1, &ctx.abort).await?;
                let truncated = records.len() > limit;
                records.truncate(limit);
                let mut rows: Vec<String> = Vec::new();
                let mut previous: Option<(String, u64)> = None;
                for (matched, record) in &records {
                    let path = absolute(&record.path);
                    if context > 0 && previous.as_ref().is_some_and(|(p, line)| *p != path || record.line != line + 1) {
                        rows.push("--".to_string());
                    }
                    let sep = if *matched { ':' } else { '-' };
                    rows.push(format!("{path}{sep}{}{sep}{}", record.line, record.text));
                    previous = Some((path, record.line));
                }
                let count = records.iter().filter(|(matched, _)| *matched).count();
                let output = if rows.is_empty() {
                    "No matches found".to_string()
                } else if truncated {
                    format!("{}\n\n(Showing the first {limit} lines. Raise head_limit or narrow the search to see more.)", rows.join("\n"))
                } else {
                    rows.join("\n")
                };
                (output, count, truncated)
            }
        };
        Ok(ToolOutput {
            title: params.pattern,
            output,
            metadata: json!({ "matches": matches, "truncated": truncated }),
            attachments: vec![],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Fixture;

    fn age(path: &Path, seconds: u64) {
        let time = SystemTime::now() - std::time::Duration::from_secs(seconds);
        std::fs::OpenOptions::new().write(true).open(path).unwrap().set_modified(time).unwrap();
    }

    #[test]
    fn sorts_newest_first() {
        let t = Fixture::new();
        let old = t.write("old.rs", "");
        let new = t.write("new.rs", "");
        age(&old, 500);
        age(&new, 5);
        assert_eq!(newest_first(&t.root, &["old.rs".into(), "new.rs".into()]), [new, old]);
    }

    // A file under `dir`, returned with native separators.
    fn put(t: &Fixture, dir: &str, name: &str, content: &str) -> PathBuf {
        t.write(&format!("{dir}/{name}"), content);
        t.root.join(dir).join(name)
    }

    #[tokio::test]
    async fn glob_lists_newest_first_and_asks_like_glob() {
        let t = Fixture::new();
        let old = put(&t, "src", "old.rs", "");
        let new = put(&t, "src", "new.rs", "");
        t.write("src/skip.txt", "");
        age(&old, 1000);
        age(&new, 10);
        let res = GlobTool::default().execute(json!({"pattern": "**/*.rs"}), &t.ctx()).await;
        let reqs = t.requests();
        assert_eq!((reqs[0].permission.as_str(), reqs[0].patterns.clone()), ("glob", vec!["**/*.rs".to_string()]));
        if ripgrep::find_rg().is_none() {
            assert!(res.is_err());
            return;
        }
        let out = res.unwrap();
        assert_eq!(out.output, format!("{}\n{}", new.display(), old.display()));
        assert_eq!(out.metadata, json!({"count": 2, "truncated": false}));
        for i in 0..(GLOB_LIMIT + 3) {
            t.write(&format!("many/f{i}.md"), "");
        }
        let out = GlobTool::default().execute(json!({"pattern": "*.md", "path": "many"}), &t.ctx()).await.unwrap();
        assert_eq!(out.output.lines().count(), GLOB_LIMIT + 2);
        assert!(out.output.ends_with("(Showing the 100 most recently modified of 103 matching files. Use a more specific pattern or path to see the rest.)"), "{}", out.output);
        assert_eq!(out.metadata["truncated"], true);
    }

    #[tokio::test]
    async fn grep_modes() {
        let t = Fixture::new();
        let a = put(&t, "src", "a.rs", "fn one() {}\nlet Needle = 1;\nfn two() {}\nlet needle = 2;\n");
        let b = put(&t, "src", "b.txt", "needle here\n");
        t.write("src/c.rs", "nothing\n");
        age(&a, 100);
        age(&b, 10);
        let grep = |args: Value| {
            let ctx = t.ctx();
            async move { GrepTool::default().execute(args, &ctx).await }
        };
        let res = grep(json!({"pattern": "needle"})).await;
        assert_eq!(t.requests()[0].permission, "grep");
        if ripgrep::find_rg().is_none() {
            assert!(res.is_err());
            return;
        }
        let out = res.unwrap();
        assert_eq!(out.output, format!("Found 2 files\n{}\n{}", b.display(), a.display()));
        assert_eq!(out.metadata, json!({"matches": 2, "truncated": false}));

        let out = grep(json!({"pattern": "needle", "include": "*.rs", "output_mode": "content"})).await.unwrap();
        assert_eq!(out.output, format!("{}:4:let needle = 2;", a.display()));
        let out = grep(json!({"pattern": "needle", "include": "*.rs", "output_mode": "content", "-i": true, "context": 1})).await.unwrap();
        let p = a.display();
        assert_eq!(out.output, format!("{p}-1-fn one() {{}}\n{p}:2:let Needle = 1;\n{p}-3-fn two() {{}}\n{p}:4:let needle = 2;"));
        let out = grep(json!({"pattern": "needle", "output_mode": "content", "-i": true, "head_limit": 1})).await.unwrap();
        assert!(out.output.starts_with(&format!("{p}:2:let Needle = 1;\n\n(Showing the first 1 lines.")), "{}", out.output);
        assert_eq!(out.metadata["truncated"], true);

        let out = grep(json!({"pattern": "needle", "output_mode": "count", "-i": true})).await.unwrap();
        assert_eq!(out.output, format!("{}:2\n{}:1\n\nFound 3 matching lines in 2 files", a.display(), b.display()));
        let out = grep(json!({"pattern": "needle", "path": a.to_string_lossy(), "output_mode": "count"})).await.unwrap();
        assert_eq!(out.output, format!("{}:1\n\nFound 1 matching line in 1 file", a.display()));
        let out = grep(json!({"pattern": "absent"})).await.unwrap();
        assert_eq!(out.output, "No matches found");
        let err = grep(json!({"pattern": "x", "output_mode": "lines"})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments { .. }));
        let err = grep(json!({"pattern": "("})).await.unwrap_err();
        assert!(err.to_string().contains("regex parse error"), "{err}");
    }
}
