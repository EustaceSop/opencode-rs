// `opencode acp`: the Agent Client Protocol (v1, as `@agentclientprotocol/sdk` 0.21.0 speaks it) over stdin/stdout,
// answered by the engine directly (`opencode/src/acp/*`, `opencode/src/cli/cmd/acp.ts`).
mod agent;
mod config_option;
mod content;
mod directory;
mod patch;
mod permission;
mod rpc;
mod tool;

use agent::Agent;
use opencode_session::Engine;
use rpc::Peer;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::sync::mpsc;

pub use agent::AUTH_METHOD_ID;
pub use rpc::RpcError;

pub const PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Clone)]
pub struct AcpOptions {
    // `--cwd`; where `session/list` without a `cwd` looks. Defaults to the process directory.
    pub cwd: Option<PathBuf>,
    // `agentInfo.version` in the `initialize` response.
    pub version: String,
}

impl Default for AcpOptions {
    fn default() -> Self {
        Self { cwd: None, version: opencode_session::VERSION.to_string() }
    }
}

// Serves ACP on stdin/stdout until stdin ends.
pub async fn run(options: AcpOptions, engine: Arc<Engine>) -> anyhow::Result<()> {
    // SAFETY: set before the connection starts, as the original sets `process.env.OPENCODE_CLIENT`.
    unsafe { std::env::set_var("OPENCODE_CLIENT", "acp") };
    serve(tokio::io::stdin(), tokio::io::stdout(), options, engine).await
}

// Serves ACP on any byte streams until `reader` ends or `writer` fails.
pub async fn serve<R, W>(reader: R, writer: W, options: AcpOptions, engine: Arc<Engine>) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send + 'static,
{
    engine.attach();
    let (tx, rx) = mpsc::unbounded_channel();
    let peer = Peer::new(tx);
    let writer = tokio::spawn(rpc::write_loop(writer, rx, peer.clone()));
    let directory = options.cwd.clone().or_else(|| std::env::current_dir().ok()).unwrap_or_default();
    let agent = Arc::new(Agent::new(engine, peer.clone(), options.version, directory));
    let closed = peer.closed();
    let mut reader = BufReader::new(reader);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        let read = tokio::select! {
            read = reader.read_until(b'\n', &mut buffer) => read,
            _ = closed.cancelled() => break,
        };
        if !matches!(read, Ok(n) if n > 0) {
            break;
        }
        let text = String::from_utf8_lossy(&buffer);
        let line = text.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(message) => dispatch(&agent, message),
            Err(error) => tracing::error!("Failed to parse JSON message: {line} {error}"),
        }
    }
    peer.close();
    agent.shutdown();
    let _ = writer.await;
    Ok(())
}

// Requests and notifications run concurrently, as the SDK handles each message without awaiting the previous one.
fn dispatch(agent: &Arc<Agent>, message: Value) {
    let Value::Object(object) = &message else {
        tracing::error!("Invalid message {message}");
        return;
    };
    let method = object.get("method").map(|m| m.as_str().map_or_else(|| m.to_string(), str::to_string));
    let params = object.get("params").cloned().unwrap_or(Value::Null);
    match (method, object.get("id").cloned()) {
        (Some(method), Some(id)) => {
            let agent = agent.clone();
            tokio::spawn(async move {
                let mut after = Vec::new();
                let result = agent.request(&method, params, &mut after).await;
                if let Err(error) = &result {
                    tracing::error!("Error handling request {method}: {}", error.to_json());
                }
                agent.peer.respond(id, result);
                for update in after {
                    agent.peer.notify("session/update", update);
                }
            });
        }
        (Some(method), None) => {
            let agent = agent.clone();
            tokio::spawn(async move {
                if let Err(error) = agent.notification(&method, params).await {
                    tracing::error!("Error handling notification {method}: {}", error.to_json());
                }
            });
        }
        (None, Some(_)) => agent.peer.resolve(&message),
        (None, None) => tracing::error!("Invalid message {message}"),
    }
}
