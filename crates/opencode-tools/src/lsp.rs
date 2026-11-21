//! `lsp` tool (`opencode/src/tool/lsp.ts`); offered when `OPENCODE_EXPERIMENTAL_LSP_TOOL` is on.

use serde_json::{Value, json};

use crate::util::{PathKind, assert_external_directory, display, relative};
use crate::write::join_path;
use crate::{PermissionRequest, Tool, ToolContext, ToolError, ToolOutput, parse_args};

pub const OPERATIONS: [&str; 9] = [
    "goToDefinition",
    "findReferences",
    "hover",
    "documentSymbol",
    "workspaceSymbol",
    "goToImplementation",
    "prepareCallHierarchy",
    "incomingCalls",
    "outgoingCalls",
];

#[derive(Debug, serde::Deserialize)]
struct Params {
    operation: String,
    #[serde(rename = "filePath")]
    file_path: String,
    line: i64,
    character: i64,
    #[serde(default)]
    query: Option<String>,
}

pub struct LspTool;

fn invalid(detail: impl Into<String>) -> ToolError {
    ToolError::InvalidArguments { tool: "lsp".into(), detail: detail.into() }
}

#[async_trait::async_trait]
impl Tool for LspTool {
    fn id(&self) -> &str {
        "lsp"
    }

    fn description(&self) -> String {
        include_str!("../../../../opencode-ts/packages/opencode/src/tool/lsp.txt").to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "$schema": crate::JSON_SCHEMA_URI,
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": OPERATIONS,
                    "description": "The LSP operation to perform"
                },
                "filePath": { "type": "string", "description": "The absolute or relative path to the file" },
                "line": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 9007199254740991u64,
                    "description": "The line number (1-based, as shown in editors)"
                },
                "character": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 9007199254740991u64,
                    "description": "The character offset (1-based, as shown in editors)"
                },
                "query": {
                    "type": "string",
                    "description": "Search query for workspaceSymbol. Empty string requests all symbols."
                }
            },
            "required": ["operation", "filePath", "line", "character"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let params: Params = parse_args("lsp", args)?;
        let operation = params.operation.as_str();
        if !OPERATIONS.contains(&operation) {
            return Err(invalid(format!("unknown operation `{operation}`")));
        }
        if params.line < 1 || params.character < 1 {
            return Err(invalid("line and character must be >= 1"));
        }
        let file = join_path(&ctx.cwd, &params.file_path);
        let shown = display(&file);
        assert_external_directory(ctx, &file, PathKind::File).await?;
        let metadata = match operation {
            "workspaceSymbol" => json!({ "operation": operation }),
            "documentSymbol" => json!({ "operation": operation, "filePath": shown }),
            _ => json!({ "operation": operation, "filePath": shown, "line": params.line, "character": params.character }),
        };
        ctx.permissions
            .ask(PermissionRequest {
                permission: "lsp".into(),
                patterns: vec!["*".into()],
                always: vec!["*".into()],
                metadata,
            })
            .await?;

        let rel = relative(&ctx.worktree, &file);
        let title = match operation {
            "workspaceSymbol" => operation.to_string(),
            "documentSymbol" => format!("{operation} {rel}"),
            _ => format!("{operation} {rel}:{}:{}", params.line, params.character),
        };

        if !tokio::fs::try_exists(&file).await.unwrap_or(false) {
            return Err(ToolError::Other(format!("File not found: {shown}")));
        }
        let hooks = ctx.hooks.as_ref();
        let available = match hooks {
            Some(hooks) => hooks.lsp_available(&file).await,
            None => false,
        };
        let Some(hooks) = hooks.filter(|_| available) else {
            return Err(ToolError::other("No LSP server available for this file type."));
        };
        hooks.touch(&file, true).await;
        let result = hooks
            .lsp_request(
                operation,
                &file,
                (params.line - 1) as u32,
                (params.character - 1) as u32,
                params.query.as_deref().unwrap_or(""),
            )
            .await;
        let output = if result.is_empty() {
            format!("No results found for {operation}")
        } else {
            serde_json::to_string_pretty(&result).map_err(|e| ToolError::Other(e.to_string()))?
        };
        Ok(ToolOutput { title, output, metadata: json!({ "result": result }), attachments: vec![] })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WorkspaceHooks;
    use crate::testing::Fixture;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl WorkspaceHooks for Fake {
        async fn touch(&self, _file: &Path, wait: bool) {
            self.calls.lock().unwrap().push(format!("touch {wait}"));
        }
        async fn lsp_available(&self, file: &Path) -> bool {
            file.extension().is_some_and(|e| e == "rs")
        }
        async fn lsp_request(&self, operation: &str, _file: &Path, line: u32, character: u32, query: &str) -> Vec<Value> {
            self.calls.lock().unwrap().push(format!("{operation} {line}:{character} {query}"));
            if operation == "hover" { vec![json!({"contents": "fn main()"})] } else { Vec::new() }
        }
    }

    #[tokio::test]
    async fn operations_permissions_and_errors() {
        let mut t = Fixture::new();
        let fake = Arc::new(Fake::default());
        t.hooks = Some(fake.clone());
        t.write("src/main.rs", "fn main() {}\n");
        t.write("notes.txt", "x\n");
        let run = |args: Value| async { LspTool.execute(args, &t.ctx()).await };

        let out = run(json!({"operation": "hover", "filePath": "src/main.rs", "line": 1, "character": 4})).await.unwrap();
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(out.title, format!("hover src{sep}main.rs:1:4"));
        assert_eq!(out.output, "[\n  {\n    \"contents\": \"fn main()\"\n  }\n]");
        assert_eq!(out.metadata["result"][0]["contents"], "fn main()");
        let reqs = t.requests();
        assert_eq!((reqs[0].permission.as_str(), reqs[0].patterns.clone(), reqs[0].always.clone()), ("lsp", vec!["*".to_string()], vec!["*".to_string()]));
        assert_eq!(reqs[0].metadata["line"], 1);
        assert_eq!(*fake.calls.lock().unwrap(), ["touch true", "hover 0:3 "]);

        let out = run(json!({"operation": "workspaceSymbol", "filePath": "src/main.rs", "line": 1, "character": 1, "query": "ma"})).await.unwrap();
        assert_eq!((out.title.as_str(), out.output.as_str()), ("workspaceSymbol", "No results found for workspaceSymbol"));
        assert_eq!(t.requests()[1].metadata, json!({"operation": "workspaceSymbol"}));

        let missing = run(json!({"operation": "hover", "filePath": "nope.rs", "line": 1, "character": 1})).await.unwrap_err();
        assert!(missing.to_string().starts_with("File not found: "), "{missing}");
        let none = run(json!({"operation": "hover", "filePath": "notes.txt", "line": 1, "character": 1})).await.unwrap_err();
        assert_eq!(none.to_string(), "No LSP server available for this file type.");
        assert!(run(json!({"operation": "rename", "filePath": "src/main.rs", "line": 1, "character": 1})).await.is_err());
        assert!(run(json!({"operation": "hover", "filePath": "src/main.rs", "line": 0, "character": 1})).await.is_err());
    }
}
