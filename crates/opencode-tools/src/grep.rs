//! `grep` tool (`opencode/src/tool/grep.ts`).

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::ripgrep::Match;
use crate::util::{PathKind, assert_external_directory, display, real_path, resolve};
use crate::write::join_path;
use crate::{PermissionRequest, Tool, ToolContext, ToolError, ToolOutput, parse_args, ripgrep};

const LIMIT: usize = 100;

#[derive(Debug, serde::Deserialize)]
struct Params {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    include: Option<String>,
}

#[derive(Default)]
pub struct GrepTool {
    /// Explicit `rg` binary; `ripgrep::filepath()` when unset.
    pub rg: Option<PathBuf>,
}

pub(crate) fn format_output(base: &Path, matches: &[Match]) -> String {
    let truncated = matches.len() == LIMIT;
    let mut output = vec![format!(
        "Found {} matches{}",
        matches.len(),
        if truncated {
            " (more matches available)"
        } else {
            ""
        }
    )];
    let mut current = String::new();
    for item in matches {
        let path = display(&resolve(base, &item.path));
        if current != path {
            if !current.is_empty() {
                output.push(String::new());
            }
            current = path.clone();
            output.push(format!("{path}:"));
        }
        output.push(format!("  Line {}: {}", item.line, item.text));
    }
    if truncated {
        output.push(String::new());
        output.push(
            "(Results truncated. Consider using a more specific path or pattern.)".to_string(),
        );
    }
    output.join("\n")
}

#[async_trait::async_trait]
impl Tool for GrepTool {
    fn id(&self) -> &str {
        "grep"
    }

    fn description(&self) -> String {
        include_str!("../../../../opencode-ts/packages/opencode/src/tool/grep.txt").to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "$schema": crate::JSON_SCHEMA_URI,
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "The regex pattern to search for in file contents" },
                "path": {
                    "type": "string",
                    "description": "The directory to search in. Defaults to the current working directory."
                },
                "include": {
                    "type": "string",
                    "description": "File pattern to include in the search (e.g. \"*.js\", \"*.{ts,tsx}\")"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let params: Params = parse_args("grep", args)?;
        if params.pattern.is_empty() {
            return Err(ToolError::other("pattern is required"));
        }

        let mut metadata = json!({ "pattern": params.pattern });
        if let Some(path) = &params.path {
            metadata["path"] = json!(path);
        }
        if let Some(include) = &params.include {
            metadata["include"] = json!(include);
        }
        ctx.permissions
            .ask(PermissionRequest {
                permission: "grep".into(),
                patterns: vec![params.pattern.clone()],
                always: vec!["*".into()],
                metadata,
            })
            .await?;

        let requested = match &params.path {
            Some(path) => join_path(&ctx.cwd, path),
            None => ctx.cwd.clone(),
        };
        let requested_is_dir = tokio::fs::metadata(&requested)
            .await
            .is_ok_and(|m| m.is_dir());
        assert_external_directory(
            ctx,
            &requested,
            if requested_is_dir {
                PathKind::Directory
            } else {
                PathKind::File
            },
        )
        .await?;

        let search = real_path(&requested);
        let search_is_dir = tokio::fs::metadata(&search).await.is_ok_and(|m| m.is_dir());
        let cwd = if search_is_dir {
            search.clone()
        } else {
            search.parent().map(Path::to_path_buf).unwrap_or(search)
        };

        let rg = ripgrep::rg_or_download(&self.rg, &ctx.abort).await?;
        let result = ripgrep::grep(
            &rg,
            &cwd,
            &params.pattern,
            params.include.as_deref(),
            LIMIT,
            &ctx.abort,
        )
        .await?;
        if result.is_empty() {
            return Ok(ToolOutput {
                title: params.pattern,
                output: "No files found".to_string(),
                metadata: json!({ "matches": 0, "truncated": false }),
                attachments: vec![],
            });
        }

        let base = if requested_is_dir {
            requested
        } else {
            requested
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or(requested)
        };
        Ok(ToolOutput {
            title: params.pattern,
            output: format_output(&base, &result),
            metadata: json!({ "matches": result.len(), "truncated": result.len() == LIMIT }),
            attachments: vec![],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Fixture;

    fn m(path: &str, line: u64, text: &str) -> Match {
        Match {
            path: path.into(),
            line,
            text: text.into(),
        }
    }

    #[test]
    fn formats_matches_grouped_by_file() {
        let base = if cfg!(windows) {
            Path::new("C:\\p")
        } else {
            Path::new("/p")
        };
        let out = format_output(
            base,
            &[
                m("a.rs", 1, "x\n"),
                m("a.rs", 3, "y\n"),
                m("b/c.rs", 2, "z\n"),
            ],
        );
        assert_eq!(
            out,
            format!(
                "Found 3 matches\n{}:\n  Line 1: x\n\n  Line 3: y\n\n\n{}:\n  Line 2: z\n",
                base.join("a.rs").display(),
                base.join("b").join("c.rs").display()
            )
        );
        let many: Vec<Match> = (0..100).map(|i| m("a", i, "t")).collect();
        let out = format_output(base, &many);
        assert!(out.starts_with("Found 100 matches (more matches available)\n"));
        assert!(
            out.ends_with(
                "\n\n(Results truncated. Consider using a more specific path or pattern.)"
            )
        );
    }

    #[tokio::test]
    async fn asks_permission_then_greps() {
        let t = Fixture::new();
        t.write("src/a.rs", "let needle = 1;\nother\n");
        t.write("src/b.txt", "needle too\n");
        let res = GrepTool::default()
            .execute(json!({"pattern": "needle", "include": "*.rs"}), &t.ctx())
            .await;
        let reqs = t.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].permission, "grep");
        assert_eq!(reqs[0].patterns, vec!["needle"]);
        assert_eq!(reqs[0].always, vec!["*"]);
        assert_eq!(
            reqs[0].metadata,
            json!({"pattern": "needle", "include": "*.rs"})
        );
        if ripgrep::find_rg().is_none() {
            assert!(
                res.unwrap_err()
                    .to_string()
                    .contains("failed to download ripgrep")
            );
            return;
        }
        let out = res.unwrap();
        assert_eq!(
            out.output,
            format!(
                "Found 1 matches\n{}:\n  Line 1: let needle = 1;\n",
                t.root.join("src").join("a.rs").display()
            )
        );
        assert_eq!(out.title, "needle");
        assert_eq!(out.metadata, json!({"matches": 1, "truncated": false}));
    }

    #[tokio::test]
    async fn empty_pattern_errors_before_asking() {
        let t = Fixture::new();
        let err = GrepTool::default()
            .execute(json!({"pattern": ""}), &t.ctx())
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "pattern is required");
        assert!(t.requests().is_empty());
    }
}
