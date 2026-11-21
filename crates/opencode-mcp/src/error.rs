use serde_json::Value;

#[derive(Debug, Clone, thiserror::Error)]
pub enum McpError {
    #[error("MCP error {code}: {message}")]
    Rpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    #[error("MCP error -32001: Request timed out")]
    Timeout,
    #[error("MCP error -32000: Connection closed")]
    Closed,
    #[error("Not connected")]
    NotConnected,
    /// The SDK's `UnauthorizedError`.
    #[error("{0}")]
    Unauthorized(String),
    #[error("Aborted")]
    Aborted,
    #[error("{0}")]
    Other(String),
}

impl McpError {
    pub fn other(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }

    pub(crate) fn unauthorized() -> Self {
        Self::Unauthorized("Unauthorized".into())
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ManagerError {
    /// `MCP.NotFoundError` (HTTP 404 `McpServerNotFoundError{name}`).
    #[error("MCP server not found: {name}")]
    NotFound { name: String },
    #[error("{0}")]
    Other(String),
}

impl ManagerError {
    pub(crate) fn other(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }
}
