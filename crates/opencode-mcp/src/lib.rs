//! MCP client layer ported from `opencode/src/mcp/*` (v1.19.5).
//!
//! Transports are implemented directly on JSON-RPC: `rmcp` 3.9.0 has no legacy HTTP+SSE client
//! transport, which the original falls back to after Streamable HTTP.

pub mod auth;
pub mod browser;
pub mod callback;
pub mod catalog;
pub mod client;
mod error;
mod http;
mod manager;
pub mod oauth;
mod process;
mod stdio;
mod tool;

#[cfg(test)]
mod test_server;
#[cfg(test)]
mod tests;

pub use auth::{AuthStore, ClientInfo, Entry, Tokens};
pub use catalog::{ToolDef, prompt_key, resource_key, sanitize, tool_name};
pub use client::{Client, Notification, RequestOptions};
pub use error::{ManagerError, McpError};
pub use manager::{
    AuthStart, AuthStatus, BrowserOpener, McpEvent, McpManager, McpOptions, ServerInstructions,
    Status,
};
pub use oauth::{OAUTH_CALLBACK_PATH, OAUTH_CALLBACK_PORT};
pub use tool::McpTool;
