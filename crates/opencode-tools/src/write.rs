//! `write` tool (`opencode/src/tool/write.ts`).

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::util::{
    PathKind, assert_external_directory, bom_join, bom_read, bom_split, display, io_err, normalize,
    relative, trim_diff, two_files_patch, write_with_dirs,
};
use crate::{PermissionRequest, Tool, ToolContext, ToolError, ToolOutput, hooks, parse_args};

const MAX_PROJECT_DIAGNOSTICS_FILES: usize = 5;

#[derive(Debug, serde::Deserialize)]
struct Params {
    content: String,
    #[serde(rename = "filePath")]
    file_path: String,
}

pub struct WriteTool;

/// `path.isAbsolute(p) ? p : path.join(directory, p)`.
pub(crate) fn join_path(cwd: &Path, p: &str) -> PathBuf {
    if Path::new(p).is_absolute() {
        PathBuf::from(p)
    } else {
        normalize(&cwd.join(p))
    }
}

#[async_trait::async_trait]
impl Tool for WriteTool {
    fn id(&self) -> &str {
        "write"
    }

    fn description(&self) -> String {
        include_str!("../../../../opencode-ts/packages/opencode/src/tool/write.txt").to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "$schema": crate::JSON_SCHEMA_URI,
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "The content to write to the file" },
                "filePath": {
                    "type": "string",
                    "description": "The absolute path to the file to write (must be absolute, not relative)"
                }
            },
            "required": ["content", "filePath"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let params: Params = parse_args("write", args)?;
        let filepath = join_path(&ctx.cwd, &params.file_path);
        let shown = display(&filepath);
        assert_external_directory(ctx, &filepath, PathKind::File).await?;

        let exists = tokio::fs::try_exists(&filepath).await.unwrap_or(false);
        let (old_bom, content_old) = if exists {
            bom_read(&filepath).await.map_err(io_err)?
        } else {
            (false, String::new())
        };
        let (new_bom, content_new) = bom_split(&params.content);
        let desired_bom = old_bom || new_bom;

        let diff = trim_diff(&two_files_patch(&shown, &content_old, content_new));
        let rel = relative(&ctx.worktree, &filepath);
        ctx.permissions
            .ask(PermissionRequest {
                permission: "edit".into(),
                patterns: vec![rel.clone()],
                always: vec!["*".into()],
                metadata: json!({ "filepath": shown, "diff": diff }),
            })
            .await?;

        write_with_dirs(&filepath, &bom_join(content_new, desired_bom)).await?;
        hooks::format_sync(ctx, &filepath, desired_bom).await;
        hooks::publish_edited(ctx, &shown);
        hooks::publish_watcher(ctx, &shown, if exists { "change" } else { "add" });

        let mut output = "Wrote file successfully.".to_string();
        let diagnostics = hooks::collect(ctx, &[&filepath]).await;
        let current = ctx.hooks.as_ref().map(|h| h.normalize(&filepath));
        let mut others = 0;
        for item in &diagnostics {
            let is_current = current.as_deref() == Some(item.file.as_str());
            if !is_current && others >= MAX_PROJECT_DIAGNOSTICS_FILES {
                continue;
            }
            let block = hooks::report(if is_current { &shown } else { &item.file }, &item.errors);
            if block.is_empty() {
                continue;
            }
            if is_current {
                output.push_str(&format!("

LSP errors detected in this file, please fix:
{block}"));
                continue;
            }
            others += 1;
            output.push_str(&format!("

LSP errors detected in other files:
{block}"));
        }

        Ok(ToolOutput {
            title: rel,
            output,
            metadata: json!({ "diagnostics": hooks::diagnostics_json(&diagnostics), "filepath": shown, "exists": exists }),
            attachments: vec![],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Fixture;

    #[tokio::test]
    async fn creates_file_with_parent_dirs() {
        let t = Fixture::new();
        let out = WriteTool
            .execute(
                json!({"filePath": "a/b/c.txt", "content": "hello\n"}),
                &t.ctx(),
            )
            .await
            .unwrap();
        assert_eq!(out.output, "Wrote file successfully.");
        assert_eq!(t.read("a/b/c.txt"), "hello\n");
        assert_eq!(out.metadata["exists"], false);
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(out.title, format!("a{sep}b{sep}c.txt"));
        let reqs = t.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].permission, "edit");
        assert_eq!(reqs[0].patterns, vec![format!("a{sep}b{sep}c.txt")]);
        assert_eq!(reqs[0].always, vec!["*"]);
        assert!(
            reqs[0].metadata["diff"]
                .as_str()
                .unwrap()
                .contains("+hello")
        );
        assert_eq!(
            reqs[0].metadata["filepath"],
            t.root
                .join("a")
                .join("b")
                .join("c.txt")
                .to_string_lossy()
                .as_ref()
        );
    }

    #[tokio::test]
    async fn overwrite_keeps_bom_and_reports_exists() {
        let t = Fixture::new();
        t.write("f.txt", "\u{feff}old\n");
        let out = WriteTool
            .execute(json!({"filePath": "f.txt", "content": "new\n"}), &t.ctx())
            .await
            .unwrap();
        assert_eq!(out.metadata["exists"], true);
        assert_eq!(t.read("f.txt"), "\u{feff}new\n");
        let diff = t.requests()[0].metadata["diff"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(diff.contains("-old") && diff.contains("+new"));
    }

    #[tokio::test]
    async fn rejected_permission_leaves_file_untouched() {
        let t = Fixture::new();
        t.deny("edit");
        let res = WriteTool
            .execute(json!({"filePath": "f.txt", "content": "x"}), &t.ctx())
            .await;
        assert!(matches!(res, Err(ToolError::Rejected)));
        assert!(!t.root.join("f.txt").exists());
    }

    #[tokio::test]
    async fn missing_param_is_invalid_arguments() {
        let t = Fixture::new();
        let res = WriteTool
            .execute(json!({"filePath": "f.txt"}), &t.ctx())
            .await;
        assert!(matches!(res, Err(ToolError::InvalidArguments { .. })));
    }
}
