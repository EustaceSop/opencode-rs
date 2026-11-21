//! Loopback OAuth callback server (`mcp/oauth-callback.ts`). Process-wide, like the original.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use reqwest::Url;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::error::McpError;
use crate::oauth::parse_redirect_uri;

const HOST: &str = "127.0.0.1";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

struct Running {
    port: u16,
    path: String,
    cancel: CancellationToken,
}

#[derive(Default)]
struct State {
    server: Option<Running>,
    pending: HashMap<String, oneshot::Sender<Result<String, String>>>,
    names: HashMap<String, String>,
}

impl State {
    fn stop_if_idle(&mut self) {
        if self.pending.is_empty()
            && let Some(server) = self.server.take()
        {
            server.cancel.cancel();
        }
    }

    fn take(&mut self, oauth_state: &str) -> Option<oneshot::Sender<Result<String, String>>> {
        let sender = self.pending.remove(oauth_state)?;
        self.names.retain(|_, state| state != oauth_state);
        Some(sender)
    }
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(Default::default)
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>OpenCode - MCP {title}</title></head>\
         <body style=\"font-family:system-ui,sans-serif;text-align:center;padding:3rem\">\
         <h1>{title}</h1><p>{}</p></body></html>",
        escape(body)
    )
}

fn handle(target: &str, path: &str) -> (u16, String) {
    let Ok(url) = Url::parse(&format!("http://localhost{target}")) else {
        return (404, "Not found".into());
    };
    if url.path() != path {
        return (404, "Not found".into());
    }
    let param = |name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
            .filter(|value| !value.is_empty())
    };
    let Some(oauth_state) = param("state") else {
        return (
            400,
            page(
                "Authorization Failed",
                "Missing required state parameter - potential CSRF attack",
            ),
        );
    };
    let mut guard = state().lock().unwrap();
    if let Some(error) = param("error") {
        let message = param("error_description").unwrap_or(error);
        if let Some(sender) = guard.take(&oauth_state) {
            let _ = sender.send(Err(message.clone()));
        }
        guard.stop_if_idle();
        return (200, page("Authorization Failed", &message));
    }
    let Some(code) = param("code") else {
        return (
            400,
            page("Authorization Failed", "No authorization code provided"),
        );
    };
    let Some(sender) = guard.take(&oauth_state) else {
        return (
            400,
            page(
                "Authorization Failed",
                "Invalid or expired state parameter - potential CSRF attack",
            ),
        );
    };
    let _ = sender.send(Ok(code));
    guard.stop_if_idle();
    (
        200,
        page(
            "Authorization Successful",
            "You can close this window and return to OpenCode.",
        ),
    )
}

async fn serve(mut stream: TcpStream, path: String) {
    let mut request = Vec::new();
    let mut chunk = [0u8; 2048];
    while !request.windows(4).any(|w| w == b"\r\n\r\n") && request.len() < 64 * 1024 {
        match stream.read(&mut chunk).await {
            Ok(n) if n > 0 => request.extend_from_slice(&chunk[..n]),
            _ => break,
        }
    }
    let head = String::from_utf8_lossy(&request);
    let target = head.split_whitespace().nth(1).unwrap_or("/");
    let (status, body) = handle(target, &path);
    let (reason, content_type) = match status {
        200 => ("OK", "text/html; charset=utf-8"),
        400 => ("Bad Request", "text/html; charset=utf-8"),
        _ => ("Not Found", "text/plain"),
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

pub async fn is_port_in_use(port: u16) -> bool {
    TcpStream::connect((HOST, port)).await.is_ok()
}

/// Starts the listener for `redirect_uri` unless something already owns the port.
pub async fn ensure_running(redirect_uri: Option<&str>) -> Result<(), McpError> {
    let (port, path) = parse_redirect_uri(redirect_uri);
    let mismatch = {
        let guard = state().lock().unwrap();
        match &guard.server {
            Some(server) if server.port == port && server.path == path => return Ok(()),
            Some(_) => true,
            None => false,
        }
    };
    if mismatch {
        stop();
    }
    if is_port_in_use(port).await {
        return Ok(());
    }
    let listener = TcpListener::bind((HOST, port)).await.map_err(|e| {
        McpError::Other(format!(
            "Failed to start OAuth callback server on port {port}: {e}"
        ))
    })?;
    let cancel = CancellationToken::new();
    {
        let mut guard = state().lock().unwrap();
        if guard.server.is_some() {
            return Ok(());
        }
        guard.server = Some(Running {
            port,
            path: path.clone(),
            cancel: cancel.clone(),
        });
    }
    tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                _ = cancel.cancelled() => return,
                accepted = listener.accept() => accepted,
            };
            if let Ok((stream, _)) = accepted {
                tokio::spawn(serve(stream, path.clone()));
            }
        }
    });
    Ok(())
}

/// Registers `oauth_state` immediately; the returned future resolves with the authorization code.
pub fn wait_for_callback(
    oauth_state: &str,
    mcp_name: Option<&str>,
) -> impl Future<Output = Result<String, McpError>> + Send + 'static {
    let (tx, rx) = oneshot::channel();
    {
        let mut guard = state().lock().unwrap();
        if let Some(name) = mcp_name {
            guard
                .names
                .insert(name.to_string(), oauth_state.to_string());
        }
        guard.pending.insert(oauth_state.to_string(), tx);
    }
    let oauth_state = oauth_state.to_string();
    async move {
        match tokio::time::timeout(CALLBACK_TIMEOUT, rx).await {
            Ok(Ok(result)) => result.map_err(McpError::Other),
            Ok(Err(_)) => Err(McpError::other("Authorization cancelled")),
            Err(_) => {
                let mut guard = state().lock().unwrap();
                guard.take(&oauth_state);
                guard.stop_if_idle();
                Err(McpError::other(
                    "OAuth callback timeout - authorization took too long",
                ))
            }
        }
    }
}

pub fn cancel_pending(mcp_name: &str) {
    let mut guard = state().lock().unwrap();
    let key = guard
        .names
        .get(mcp_name)
        .cloned()
        .unwrap_or_else(|| mcp_name.to_string());
    if let Some(sender) = guard.take(&key) {
        guard.names.remove(mcp_name);
        let _ = sender.send(Err("Authorization cancelled".into()));
        guard.stop_if_idle();
    }
}

pub fn stop() {
    let mut guard = state().lock().unwrap();
    if let Some(server) = guard.server.take() {
        server.cancel.cancel();
    }
    for (_, sender) in guard.pending.drain() {
        let _ = sender.send(Err("OAuth callback server stopped".into()));
    }
    guard.names.clear();
}

pub fn is_running() -> bool {
    state().lock().unwrap().server.is_some()
}
