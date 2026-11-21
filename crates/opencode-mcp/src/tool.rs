//! MCP tools as `opencode_tools::Tool` (`session/tools.ts:390-470`).

use opencode_tools::{PermissionRequest, Tool, ToolContext, ToolError, ToolOutput};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::catalog::{self, ToolDef};
use crate::client::Client;
use crate::error::McpError;

pub struct McpTool {
    pub(crate) id: String,
    pub(crate) server: String,
    pub(crate) def: ToolDef,
    pub(crate) client: Client,
    pub(crate) timeout: Option<u64>,
}

impl McpTool {
    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn def(&self) -> &ToolDef {
        &self.def
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Per-request timeout in ms; `None` means the SDK default of 60 s.
    pub fn timeout(&self) -> Option<u64> {
        self.timeout
    }

    /// The raw `CallToolResult`, without permission checks (what `tool.execute.after` hooks receive).
    pub async fn call(
        &self,
        args: Value,
        abort: Option<CancellationToken>,
    ) -> Result<Value, McpError> {
        catalog::call_tool(&self.client, &self.def.name, args, self.timeout, abort).await
    }
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn id(&self) -> &str {
        &self.id
    }

    fn description(&self) -> String {
        self.def.description.clone().unwrap_or_default()
    }

    fn parameters(&self) -> Value {
        self.def.parameters()
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        ctx.permissions
            .ask(PermissionRequest {
                permission: self.id.clone(),
                patterns: vec!["*".into()],
                always: vec!["*".into()],
                metadata: json!({}),
            })
            .await?;
        match self.call(args, Some(ctx.abort.clone())).await {
            Ok(result) => Ok(catalog::tool_output(&result)),
            Err(McpError::Aborted) => Err(ToolError::Aborted),
            Err(error) => Err(ToolError::Other(error.to_string())),
        }
    }
}
