// `/mcp` family and `/experimental/resource`, backed by the engine's per-directory MCP connections.
use crate::error::{ApiError, ApiResult, json};
use crate::routes::session::payload;
use crate::state::{AppState, Inner, Instance, lock};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use opencode_core::config::McpServer;
use opencode_mcp::{ManagerError, McpEvent};
use opencode_session::McpManager;
use serde::Deserialize;
use serde_json::json as value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

fn missing(name: &str) -> ApiError {
    ApiError::tagged_not_found("McpServerNotFoundError", "name", name, format!("MCP server not found: {name}"))
}

fn failed(error: ManagerError) -> ApiError {
    match error {
        ManagerError::NotFound { name } => missing(&name),
        ManagerError::Other(message) => ApiError::internal(message),
    }
}

impl Inner {
    // The directory's MCP manager; its events are forwarded to the bus from the first time it is seen.
    pub async fn mcp(self: &Arc<Self>, directory: &str) -> ApiResult<McpManager> {
        let manager = self
            .engine
            .mcp(&PathBuf::from(directory))
            .await
            .map_err(|error| ApiError::unknown(StatusCode::BAD_REQUEST, error.to_string()))?;
        if lock(&self.mcp_watched).insert(directory.to_string()) {
            let mut events = manager.subscribe();
            let state = Arc::downgrade(self);
            let location = self.instance_location(&self.instance_for(directory, false));
            tokio::spawn(async move {
                loop {
                    let event = match events.recv().await {
                        Ok(event) => event,
                        Err(RecvError::Lagged(_)) => continue,
                        Err(RecvError::Closed) => return,
                    };
                    let Some(state) = state.upgrade() else { return };
                    let (kind, properties) = match event {
                        McpEvent::ToolsChanged { server } => ("mcp.tools.changed", value!({"server": server})),
                        McpEvent::BrowserOpenFailed { mcp_name, url } => ("mcp.browser.open.failed", value!({"mcpName": mcp_name, "url": url})),
                        McpEvent::Toast { title, message, variant, duration } => {
                            ("tui.toast.show", value!({"title": title, "message": message, "variant": variant, "duration": duration}))
                        }
                    };
                    state.bus.publish(&location, kind, properties);
                }
            });
        }
        Ok(manager)
    }

    // Drops a directory's MCP connections so the next use reads the config again.
    pub async fn dispose_mcp(&self, directory: &str) {
        lock(&self.mcp_watched).remove(directory);
        self.engine.dispose(&PathBuf::from(directory)).await;
    }

    pub async fn dispose_all(&self) {
        lock(&self.mcp_watched).clear();
        lock(&self.touched).clear();
        self.pty.dispose_all();
        self.engine.shutdown().await;
    }
}

pub async fn status(State(state): State<AppState>, instance: Instance) -> ApiResult {
    Ok(json(state.mcp(&instance.directory).await?.status()))
}

#[derive(Deserialize)]
struct AddBody {
    name: String,
    config: McpServer,
}

pub async fn add(State(state): State<AppState>, instance: Instance, body: Bytes) -> ApiResult {
    let body: AddBody = payload(&body)?;
    let manager = state.mcp(&instance.directory).await?;
    Ok(json(manager.add(&body.name, body.config).await))
}

fn unsupported(name: &str) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, value!({"error": format!("MCP server {name} does not support OAuth")}))
}

pub async fn auth_start(State(state): State<AppState>, instance: Instance, Path(name): Path<String>) -> ApiResult {
    let manager = state.mcp(&instance.directory).await?;
    if !manager.supports_oauth(&name).map_err(failed)? {
        return Err(unsupported(&name));
    }
    Ok(json(manager.start_auth(&name).await.map_err(failed)?))
}

#[derive(Deserialize)]
struct CallbackBody {
    code: String,
}

pub async fn auth_callback(State(state): State<AppState>, instance: Instance, Path(name): Path<String>, body: Bytes) -> ApiResult {
    let body: CallbackBody = payload(&body)?;
    let manager = state.mcp(&instance.directory).await?;
    Ok(json(manager.finish_auth(&name, &body.code).await.map_err(failed)?))
}

pub async fn auth_authenticate(State(state): State<AppState>, instance: Instance, Path(name): Path<String>) -> ApiResult {
    let manager = state.mcp(&instance.directory).await?;
    if !manager.supports_oauth(&name).map_err(failed)? {
        return Err(unsupported(&name));
    }
    Ok(json(manager.authenticate(&name, None).await.map_err(failed)?))
}

pub async fn auth_remove(State(state): State<AppState>, instance: Instance, Path(name): Path<String>) -> ApiResult {
    let manager = state.mcp(&instance.directory).await?;
    if !manager.status().contains_key(&name) {
        return Err(missing(&name));
    }
    manager.remove_auth(&name).await;
    Ok(json(value!({"success": true})))
}

pub async fn connect(State(state): State<AppState>, instance: Instance, Path(name): Path<String>) -> ApiResult {
    state.mcp(&instance.directory).await?.connect_server(&name).await.map_err(failed)?;
    Ok(json(true))
}

pub async fn disconnect(State(state): State<AppState>, instance: Instance, Path(name): Path<String>) -> ApiResult {
    state.mcp(&instance.directory).await?.disconnect(&name).await.map_err(failed)?;
    Ok(json(true))
}

pub async fn resources(State(state): State<AppState>, instance: Instance) -> ApiResult {
    Ok(json(state.mcp(&instance.directory).await?.resources(None).await))
}
