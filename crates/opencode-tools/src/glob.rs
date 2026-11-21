//! `glob` tool (`opencode/src/tool/glob.ts`).

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::util::{PathKind, assert_external_directory, display, relative, resolve};
use crate::{PermissionRequest, Tool, ToolContext, ToolError, ToolOutput, parse_args, ripgrep};

const LIMIT: usize = 100;

#[derive(Debug, serde::Deserialize)]
struct Params {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

#[derive(Default)]
pub struct GlobTool {
    /// Explicit `rg` binary; `ripgrep::filepath()` when unset.
    pub rg: Option<PathBuf>,
}

pub(crate) fn format_output(search: &Path, files: &[String]) -> String {
    let mut output: Vec<String> = Vec::new();
    if files.is_empty() {
        output.push("No files found".to_string());
    } else {
        output.extend(files.iter().map(|file| display(&resolve(search, file))));
        if files.len() == LIMIT {
            output.push(String::new());
            output.push(format!(
                "(Results are truncated: showing first {LIMIT} results. Consider using a more specific path or pattern.)"
            ));
        }
    }
    output.join("\n")
}

#[async_trait::async_trait]
impl Tool for GlobTool {
    fn id(&self) -> &str {
        "glob"
    }

    fn description(&self) -> String {
        include_str!("../../../../opencode-ts/packages/opencode/src/tool/glob.txt").to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "$schema": crate::JSON_SCHEMA_URI,
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "The glob pattern to match files against" },
                "path": {
                    "type": "string",
                    "description": "The directory to search in. If not specified, the current working directory will be used. IMPORTANT: Omit this field to use the default directory. DO NOT enter \"undefined\" or \"null\" - simply omit it for the default behavior. Must be a valid directory path if provided."
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let params: Params = parse_args("glob", args)?;
        let mut metadata = json!({ "pattern": params.pattern });
        if let Some(path) = &params.path {
            metadata["path"] = json!(path);
        }
        ctx.permissions
            .ask(PermissionRequest {
                permission: "glob".into(),
                patterns: vec![params.pattern.clone()],
                always: vec!["*".into()],
                metadata,
            })
            .await?;

        let search = match &params.path {
            Some(path) => resolve(&ctx.cwd, path),
            None => ctx.cwd.clone(),
        };
        if tokio::fs::metadata(&search)
            .await
            .is_ok_and(|m| m.is_file())
        {
            return Err(ToolError::Other(format!(
                "glob path must be a directory: {}",
                display(&search)
            )));
        }
        assert_external_directory(ctx, &search, PathKind::Directory).await?;

        let rg = ripgrep::rg_or_download(&self.rg, &ctx.abort).await?;
        let files = ripgrep::glob(&rg, &search, &params.pattern, LIMIT, &ctx.abort).await?;
        let truncated = files.len() == LIMIT;

        Ok(ToolOutput {
            title: relative(&ctx.worktree, &search),
            output: format_output(&search, &files),
            metadata: json!({ "count": files.len(), "truncated": truncated }),
            attachments: vec![],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Fixture;

    #[test]
    fn formats_results() {
        let base = if cfg!(windows) {
            Path::new("C:\\p")
        } else {
            Path::new("/p")
        };
        assert_eq!(format_output(base, &[]), "No files found");
        let out = format_output(base, &["a/b.txt".to_string(), "c.txt".to_string()]);
        assert_eq!(
            out,
            format!(
                "{}\n{}",
                base.join("a").join("b.txt").display(),
                base.join("c.txt").display()
            )
        );
        let many: Vec<String> = (0..100).map(|i| format!("f{i}")).collect();
        assert!(format_output(base, &many).ends_with(
            "\n\n(Results are truncated: showing first 100 results. Consider using a more specific path or pattern.)"
        ));
    }

    #[tokio::test]
    async fn asks_permission_then_globs() {
        let t = Fixture::new();
        t.write("src/a.rs", "");
        t.write("src/b.txt", "");
        let res = GlobTool::default()
            .execute(json!({"pattern": "**/*.rs"}), &t.ctx())
            .await;
        let reqs = t.requests();
        assert_eq!(reqs[0].permission, "glob");
        assert_eq!(reqs[0].patterns, vec!["**/*.rs"]);
        assert_eq!(reqs[0].always, vec!["*"]);
        assert_eq!(reqs[0].metadata, json!({"pattern": "**/*.rs"}));
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
            t.root.join("src").join("a.rs").to_string_lossy()
        );
        assert_eq!(out.metadata, json!({"count": 1, "truncated": false}));
        assert_eq!(out.title, "");
    }

    #[tokio::test]
    async fn file_path_is_rejected() {
        let t = Fixture::new();
        let file = t.write("a.txt", "");
        let err = GlobTool::default()
            .execute(json!({"pattern": "*", "path": "a.txt"}), &t.ctx())
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("glob path must be a directory: {}", file.display())
        );
    }
}
