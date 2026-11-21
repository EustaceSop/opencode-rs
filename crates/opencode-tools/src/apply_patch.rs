//! `apply_patch` tool (`opencode/src/tool/apply_patch.ts`).

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::patch::{Hunk, derive_new_contents, parse_patch};
use crate::util::{
    PathKind, assert_external_directory, bom_join, bom_read, bom_split, display, io_err,
    line_counts, relative, resolve, trim_diff, two_files_patch, write_with_dirs,
};
use crate::{PermissionRequest, Tool, ToolContext, ToolError, ToolOutput, hooks, parse_args};

#[derive(Debug, serde::Deserialize)]
struct Params {
    #[serde(rename = "patchText")]
    patch_text: String,
}

pub struct ApplyPatchTool;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Add,
    Update,
    Delete,
    Move,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Add => "add",
            Kind::Update => "update",
            Kind::Delete => "delete",
            Kind::Move => "move",
        }
    }
}

struct Change {
    file_path: PathBuf,
    new_content: String,
    kind: Kind,
    move_path: Option<PathBuf>,
    diff: String,
    additions: usize,
    deletions: usize,
    bom: bool,
}

fn fail(message: impl std::fmt::Display) -> ToolError {
    ToolError::Other(format!("apply_patch verification failed: {message}"))
}

#[async_trait::async_trait]
impl Tool for ApplyPatchTool {
    fn id(&self) -> &str {
        "apply_patch"
    }

    fn description(&self) -> String {
        include_str!("../../../../opencode-ts/packages/opencode/src/tool/apply_patch.txt")
            .to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "$schema": crate::JSON_SCHEMA_URI,
            "type": "object",
            "properties": {
                "patchText": {
                    "type": "string",
                    "description": "The full patch text that describes all changes to be made"
                }
            },
            "required": ["patchText"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let params: Params = parse_args("apply_patch", args)?;
        if params.patch_text.is_empty() {
            return Err(ToolError::other("patchText is required"));
        }

        // JS `String(new Error(msg))` renders as `Error: msg`.
        let hunks = parse_patch(&params.patch_text).map_err(|e| fail(format!("Error: {e}")))?;
        if hunks.is_empty() {
            let normalized = params.patch_text.replace("\r\n", "\n").replace('\r', "\n");
            if crate::util::js_trim(&normalized) == "*** Begin Patch\n*** End Patch" {
                return Err(ToolError::other("patch rejected: empty patch"));
            }
            return Err(fail("no hunks found"));
        }

        let rel = |path: &PathBuf| relative(&ctx.worktree, path).replace('\\', "/");
        let mut changes: Vec<Change> = Vec::new();
        let mut total_diff = String::new();

        for hunk in &hunks {
            match hunk {
                Hunk::Add { path, contents } => {
                    let file_path = resolve(&ctx.cwd, path);
                    assert_external_directory(ctx, &file_path, PathKind::File).await?;
                    let new_content = if contents.is_empty() || contents.ends_with('\n') {
                        contents.clone()
                    } else {
                        format!("{contents}\n")
                    };
                    let (bom, text) = bom_split(&new_content);
                    let diff = trim_diff(&two_files_patch(&display(&file_path), "", text));
                    let (additions, deletions) = line_counts("", text);
                    total_diff.push_str(&diff);
                    total_diff.push('\n');
                    changes.push(Change {
                        file_path,
                        new_content: text.to_string(),
                        kind: Kind::Add,
                        move_path: None,
                        diff,
                        additions,
                        deletions,
                        bom,
                    });
                }
                Hunk::Update {
                    path,
                    move_path,
                    chunks,
                } => {
                    let file_path = resolve(&ctx.cwd, path);
                    assert_external_directory(ctx, &file_path, PathKind::File).await?;
                    let shown = display(&file_path);
                    if !tokio::fs::metadata(&file_path)
                        .await
                        .is_ok_and(|m| !m.is_dir())
                    {
                        return Err(fail(format!("Failed to read file to update: {shown}")));
                    }
                    let (source_bom, old_content) = bom_read(&file_path).await.map_err(io_err)?;
                    let (new_content, bom) =
                        derive_new_contents(&shown, chunks, &bom_join(&old_content, source_bom))
                            .map_err(|e| fail(format!("Error: {e}")))?;
                    let diff = trim_diff(&two_files_patch(&shown, &old_content, &new_content));
                    let (additions, deletions) = line_counts(&old_content, &new_content);
                    let move_path = move_path.as_ref().map(|target| resolve(&ctx.cwd, target));
                    if let Some(target) = &move_path {
                        assert_external_directory(ctx, target, PathKind::File).await?;
                    }
                    total_diff.push_str(&diff);
                    total_diff.push('\n');
                    changes.push(Change {
                        file_path,
                        new_content,
                        kind: if move_path.is_some() {
                            Kind::Move
                        } else {
                            Kind::Update
                        },
                        move_path,
                        diff,
                        additions,
                        deletions,
                        bom,
                    });
                }
                Hunk::Delete { path } => {
                    let file_path = resolve(&ctx.cwd, path);
                    assert_external_directory(ctx, &file_path, PathKind::File).await?;
                    let (bom, content) = bom_read(&file_path).await.map_err(fail)?;
                    let diff = trim_diff(&two_files_patch(&display(&file_path), &content, ""));
                    let deletions = content.split('\n').count();
                    total_diff.push_str(&diff);
                    total_diff.push('\n');
                    changes.push(Change {
                        file_path,
                        new_content: String::new(),
                        kind: Kind::Delete,
                        move_path: None,
                        diff,
                        additions: 0,
                        deletions,
                        bom,
                    });
                }
            }
        }

        let files: Vec<Value> = changes
            .iter()
            .map(|change| {
                let mut file = json!({
                    "filePath": display(&change.file_path),
                    "relativePath": rel(change.move_path.as_ref().unwrap_or(&change.file_path)),
                    "type": change.kind.as_str(),
                    "patch": change.diff,
                    "additions": change.additions,
                    "deletions": change.deletions,
                });
                if let Some(target) = &change.move_path {
                    file["movePath"] = json!(display(target));
                }
                file
            })
            .collect();

        let relative_paths: Vec<String> = changes.iter().map(|c| rel(&c.file_path)).collect();
        ctx.permissions
            .ask(PermissionRequest {
                permission: "edit".into(),
                patterns: relative_paths.clone(),
                always: vec!["*".into()],
                metadata: json!({ "filepath": relative_paths.join(", "), "diff": total_diff, "files": files }),
            })
            .await?;

        let mut updates: Vec<(String, &str)> = Vec::new();
        for change in &changes {
            let content = bom_join(&change.new_content, change.bom);
            let source = display(&change.file_path);
            match change.kind {
                Kind::Add => {
                    write_with_dirs(&change.file_path, &content).await?;
                    updates.push((source, "add"));
                }
                Kind::Update => {
                    write_with_dirs(&change.file_path, &content).await?;
                    updates.push((source, "change"));
                }
                Kind::Move => {
                    if let Some(target) = &change.move_path {
                        write_with_dirs(target, &content).await?;
                        tokio::fs::remove_file(&change.file_path)
                            .await
                            .map_err(io_err)?;
                        updates.push((source, "unlink"));
                        updates.push((display(target), "add"));
                    }
                }
                Kind::Delete => {
                    tokio::fs::remove_file(&change.file_path)
                        .await
                        .map_err(io_err)?;
                    updates.push((source, "unlink"));
                }
            }
            if change.kind != Kind::Delete {
                let edited = change.move_path.as_ref().unwrap_or(&change.file_path);
                hooks::format_sync(ctx, edited, change.bom).await;
                hooks::publish_edited(ctx, &display(edited));
            }
        }
        for (file, event) in &updates {
            hooks::publish_watcher(ctx, file, event);
        }

        let targets: Vec<&PathBuf> = changes
            .iter()
            .filter(|change| change.kind != Kind::Delete)
            .map(|change| change.move_path.as_ref().unwrap_or(&change.file_path))
            .collect();
        let touched: Vec<&std::path::Path> = targets.iter().map(|path| path.as_path()).collect();
        let diagnostics = hooks::collect(ctx, &touched).await;

        let summary: Vec<String> = changes
            .iter()
            .map(|change| match change.kind {
                Kind::Add => format!("A {}", rel(&change.file_path)),
                Kind::Delete => format!("D {}", rel(&change.file_path)),
                _ => format!(
                    "M {}",
                    rel(change.move_path.as_ref().unwrap_or(&change.file_path))
                ),
            })
            .collect();
        let mut output = format!(
            "Success. Updated the following files:\n{}",
            summary.join("\n")
        );
        for target in targets {
            let block = hooks::block(ctx, &diagnostics, target, &display(target));
            if block.is_empty() {
                continue;
            }
            output.push_str(&format!(
                "\n\nLSP errors detected in {}, please fix:\n{block}",
                rel(target)
            ));
        }

        Ok(ToolOutput {
            title: output.clone(),
            output,
            metadata: json!({ "diff": total_diff, "files": files, "diagnostics": hooks::diagnostics_json(&diagnostics) }),
            attachments: vec![],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Fixture;

    async fn run(t: &Fixture, patch: &str) -> Result<ToolOutput, ToolError> {
        ApplyPatchTool
            .execute(json!({ "patchText": patch }), &t.ctx())
            .await
    }

    #[tokio::test]
    async fn add_update_move_delete() {
        let t = Fixture::new();
        t.write("up.txt", "one\ntwo\nthree\n");
        t.write("mv.txt", "a\nb\n");
        t.write("del.txt", "x\ny\n");
        let patch = "*** Begin Patch\n*** Add File: sub/new.txt\n+hello\n+world\n*** Update File: up.txt\n@@\n one\n-two\n+TWO\n*** Update File: mv.txt\n*** Move to: moved/mv2.txt\n@@\n-a\n+A\n*** Delete File: del.txt\n*** End Patch";
        let out = run(&t, patch).await.unwrap();
        assert_eq!(
            out.output,
            "Success. Updated the following files:\nA sub/new.txt\nM up.txt\nM moved/mv2.txt\nD del.txt"
        );
        assert_eq!(out.title, out.output);
        assert_eq!(t.read("sub/new.txt"), "hello\nworld\n");
        assert_eq!(t.read("up.txt"), "one\nTWO\nthree\n");
        assert_eq!(t.read("moved/mv2.txt"), "A\nb\n");
        assert!(!t.root.join("mv.txt").exists());
        assert!(!t.root.join("del.txt").exists());

        let reqs = t.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].permission, "edit");
        assert_eq!(
            reqs[0].patterns,
            vec!["sub/new.txt", "up.txt", "mv.txt", "del.txt"]
        );
        assert_eq!(reqs[0].always, vec!["*"]);
        assert_eq!(
            reqs[0].metadata["filepath"],
            "sub/new.txt, up.txt, mv.txt, del.txt"
        );
        let files = out.metadata["files"].as_array().unwrap();
        assert_eq!(files[0]["type"], "add");
        assert_eq!(files[0]["additions"], 2);
        assert_eq!(files[2]["type"], "move");
        assert_eq!(files[2]["relativePath"], "moved/mv2.txt");
        assert_eq!(
            files[2]["movePath"],
            t.root
                .join("moved")
                .join("mv2.txt")
                .to_string_lossy()
                .as_ref()
        );
        assert_eq!(files[3]["deletions"], 3);
        assert!(
            out.metadata["diff"]
                .as_str()
                .unwrap()
                .contains("-two\n+TWO")
        );
    }

    #[tokio::test]
    async fn error_messages() {
        let t = Fixture::new();
        assert_eq!(
            run(&t, "").await.unwrap_err().to_string(),
            "patchText is required"
        );
        assert_eq!(
            run(&t, "garbage").await.unwrap_err().to_string(),
            "apply_patch verification failed: Error: Invalid patch format: missing Begin/End markers"
        );
        assert_eq!(
            run(&t, "*** Begin Patch\r\n*** End Patch\r\n")
                .await
                .unwrap_err()
                .to_string(),
            "patch rejected: empty patch"
        );
        assert_eq!(
            run(&t, "*** Begin Patch\nnoise\n*** End Patch")
                .await
                .unwrap_err()
                .to_string(),
            "apply_patch verification failed: no hunks found"
        );
        assert_eq!(
            run(
                &t,
                "*** Begin Patch\n*** Update File: missing.txt\n@@\n-a\n+b\n*** End Patch"
            )
            .await
            .unwrap_err()
            .to_string(),
            format!(
                "apply_patch verification failed: Failed to read file to update: {}",
                t.root.join("missing.txt").display()
            )
        );
        t.write("f.txt", "a\n");
        assert_eq!(
            run(
                &t,
                "*** Begin Patch\n*** Update File: f.txt\n@@\n-zzz\n+b\n*** End Patch"
            )
            .await
            .unwrap_err()
            .to_string(),
            format!(
                "apply_patch verification failed: Error: Failed to find expected lines in {}:\nzzz",
                t.root.join("f.txt").display()
            )
        );
        assert!(t.requests().is_empty());
    }

    #[tokio::test]
    async fn rejection_applies_nothing() {
        let t = Fixture::new();
        t.deny("edit");
        let res = run(
            &t,
            "*** Begin Patch\n*** Add File: n.txt\n+x\n*** End Patch",
        )
        .await;
        assert!(matches!(res, Err(ToolError::Rejected)));
        assert!(!t.root.join("n.txt").exists());
    }
}
