//! Built-in tools ported from `opencode/src/tool/*` (legacy runtime, v1.19.5).

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub mod agent;
pub mod apply_patch;
pub mod arity;
pub mod bash;
mod bash_profiles;
mod bash_prompt;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod hooks;
pub mod invalid;
pub mod lsp;
pub mod mingit;
pub mod patch;
pub mod read;
pub mod repair;
pub mod ripgrep;
mod ripgrep_binary;
pub mod shell;
pub mod todo;
pub mod truncate;
pub mod util;
pub mod webfetch;
pub mod write;

#[cfg(test)]
pub(crate) mod testing;

pub use hooks::{FileDiagnostics, WorkspaceHooks};
pub use truncate::{
    Direction, MAX_BYTES, MAX_LINES, TruncateOptions, Truncated, cleanup_tool_output,
    truncate_output,
};

#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequest {
    pub permission: String,
    pub patterns: Vec<String>,
    pub always: Vec<String>,
    pub metadata: Value,
}

#[async_trait::async_trait]
pub trait PermissionAsker: Send + Sync {
    async fn ask(&self, req: PermissionRequest) -> Result<(), ToolError>;
}

pub struct ToolContext {
    pub session_id: String,
    pub message_id: String,
    pub call_id: String,
    pub agent: String,
    pub cwd: PathBuf,
    pub worktree: PathBuf,
    pub data_dir: PathBuf,
    pub abort: CancellationToken,
    pub permissions: Arc<dyn PermissionAsker>,
    /// Formatter, bus and language-server access of the instance; `None` runs the tools bare.
    pub hooks: Option<Arc<dyn WorkspaceHooks>>,
}

/// `{type:"file", mime, url:"data:<mime>;base64,..."}` attachment of the TS `ExecuteResult`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Attachment {
    pub mime: String,
    pub url: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolOutput {
    pub title: String,
    pub output: String,
    pub metadata: Value,
    pub attachments: Vec<Attachment>,
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("The user rejected permission to use this specific tool call.")]
    Rejected,
    #[error(
        "The {tool} tool was called with invalid arguments: {detail}.\nPlease rewrite the input so it satisfies the expected schema."
    )]
    InvalidArguments { tool: String, detail: String },
    #[error("Aborted")]
    Aborted,
    #[error("{0}")]
    Other(String),
}

impl ToolError {
    pub fn other(message: impl Into<String>) -> Self {
        ToolError::Other(message.into())
    }
}

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn id(&self) -> &str;
    fn description(&self) -> String;
    fn parameters(&self) -> Value;
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError>;
}

/// Built-ins in registry order (`registry.ts:231-249`), minus tools that need other subsystems.
pub fn builtin() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(invalid::InvalidTool),
        Arc::new(bash::BashTool::new(bash::BashConfig::default())),
        Arc::new(read::ReadTool),
        Arc::new(glob::GlobTool::default()),
        Arc::new(grep::GrepTool::default()),
        Arc::new(edit::EditTool),
        Arc::new(write::WriteTool),
        Arc::new(webfetch::WebFetchTool),
        Arc::new(todo::TodoWriteTool::default()),
        Arc::new(apply_patch::ApplyPatchTool),
    ]
}

/// `registry.ts:297-300`: models that get `apply_patch` instead of `edit` + `write`.
pub fn uses_apply_patch(model_id: &str) -> bool {
    model_id.contains("gpt-") && !model_id.contains("oss") && !model_id.contains("gpt-4")
}

/// The `tool.ts` wrapper: run the tool, then apply the shared truncation unless the tool set `metadata.truncated`.
pub async fn execute_with_truncation(
    tool: &dyn Tool,
    args: Value,
    ctx: &ToolContext,
    opts: &TruncateOptions,
) -> Result<ToolOutput, ToolError> {
    let mut result = tool.execute(args, ctx).await?;
    if result.metadata.get("truncated").is_some() {
        return Ok(result);
    }
    let truncated = truncate_output(&result.output, &ctx.data_dir, opts)
        .map_err(|e| ToolError::Other(format!("Failed to write truncated tool output: {e}")))?;
    if !result.metadata.is_object() {
        result.metadata = Value::Object(Default::default());
    }
    let meta = result.metadata.as_object_mut().expect("object");
    meta.insert("truncated".into(), Value::Bool(truncated.truncated));
    if let Some(path) = &truncated.output_path {
        meta.insert(
            "outputPath".into(),
            Value::String(path.to_string_lossy().into_owned()),
        );
    }
    result.output = truncated.content;
    Ok(result)
}

pub(crate) fn parse_args<T: serde::de::DeserializeOwned>(
    tool: &str,
    args: Value,
) -> Result<T, ToolError> {
    serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments {
        tool: tool.to_string(),
        detail: e.to_string(),
    })
}

pub(crate) const JSON_SCHEMA_URI: &str = "https://json-schema.org/draft/2020-12/schema";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_ids_in_registry_order() {
        let ids: Vec<String> = builtin().iter().map(|t| t.id().to_string()).collect();
        assert_eq!(
            ids,
            [
                "invalid",
                "bash",
                "read",
                "glob",
                "grep",
                "edit",
                "write",
                "webfetch",
                "todowrite",
                "apply_patch"
            ]
        );
        for tool in builtin() {
            let schema = tool.parameters();
            assert_eq!(schema["type"], "object", "{}", tool.id());
            assert!(!tool.description().is_empty());
            assert!(!tool.description().contains("${intro}"), "{}", tool.id());
        }
    }

    #[test]
    fn patch_model_swap() {
        assert!(uses_apply_patch("gpt-5"));
        assert!(!uses_apply_patch("gpt-4o"));
        assert!(!uses_apply_patch("gpt-oss-120b"));
        assert!(!uses_apply_patch("claude-sonnet-4"));
    }

    #[tokio::test]
    async fn wrapper_truncates_and_marks_metadata() {
        let t = testing::Fixture::new();
        let big = "x\n".repeat(3000);
        let out = execute_with_truncation(
            &invalid::InvalidTool,
            serde_json::json!({"tool": "a", "error": big}),
            &t.ctx(),
            &TruncateOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(out.metadata["truncated"], true);
        let path = out.metadata["outputPath"].as_str().unwrap();
        assert!(std::path::Path::new(path).starts_with(t.data.path()));
        assert!(out.output.contains("lines truncated..."));
    }

    #[test]
    fn invalid_arguments_message() {
        let err = parse_args::<read::Params>("read", serde_json::json!({})).unwrap_err();
        let text = err.to_string();
        assert!(text.starts_with("The read tool was called with invalid arguments: "));
        assert!(text.ends_with(".\nPlease rewrite the input so it satisfies the expected schema."));
    }
}
