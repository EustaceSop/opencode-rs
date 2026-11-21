// JSON-RPC 2.0 over newline-delimited JSON, as `Connection` + `ndJsonStream` of `@agentclientprotocol/sdk` 0.21.0 speak it.
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    fn with_detail(code: i64, base: &str, data: Option<Value>, detail: Option<&str>) -> Self {
        let message = match detail {
            Some(detail) => format!("{base}: {detail}"),
            None => base.to_string(),
        };
        Self { code, message, data }
    }

    pub fn method_not_found(method: &str) -> Self {
        Self { code: -32601, message: format!("\"Method not found\": {method}"), data: Some(json!({"method": method})) }
    }

    pub fn invalid_params(data: Option<Value>, detail: Option<&str>) -> Self {
        Self::with_detail(-32602, "Invalid params", data, detail)
    }

    pub fn internal(data: Option<Value>, detail: Option<&str>) -> Self {
        Self::with_detail(-32603, "Internal error", data, detail)
    }

    pub fn auth_required(data: Option<Value>, detail: Option<&str>) -> Self {
        Self::with_detail(-32000, "Authentication required", data, detail)
    }

    fn closed() -> Self {
        Self::internal(None, Some("ACP connection closed"))
    }

    pub fn to_json(&self) -> Value {
        let mut value = json!({"code": self.code, "message": self.message});
        if let Some(data) = &self.data {
            value["data"] = data.clone();
        }
        value
    }
}

type Pending = Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>;

struct Inner {
    out: mpsc::UnboundedSender<Value>,
    pending: Pending,
    next: AtomicU64,
    closed: CancellationToken,
}

// The agent's half of the connection: outgoing notifications, requests to the client and responses.
#[derive(Clone)]
pub struct Peer {
    inner: Arc<Inner>,
}

impl Peer {
    pub fn new(out: mpsc::UnboundedSender<Value>) -> Self {
        Self { inner: Arc::new(Inner { out, pending: Mutex::new(HashMap::new()), next: AtomicU64::new(0), closed: CancellationToken::new() }) }
    }

    fn send(&self, message: Value) {
        if !self.inner.closed.is_cancelled() {
            let _ = self.inner.out.send(message);
        }
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        if self.inner.closed.is_cancelled() {
            return Err(RpcError::closed());
        }
        let id = self.inner.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id, tx);
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        rx.await.unwrap_or_else(|_| Err(RpcError::closed()))
    }

    pub fn respond(&self, id: Value, result: Result<Value, RpcError>) {
        let message = match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error.to_json()}),
        };
        self.send(message);
    }

    // A response from the client to one of our requests.
    pub fn resolve(&self, message: &Value) {
        let Some(id) = message.get("id").and_then(Value::as_u64) else {
            tracing::error!("Got response to unknown request {:?}", message.get("id"));
            return;
        };
        let Some(waiter) = self.inner.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id) else {
            tracing::error!("Got response to unknown request {id}");
            return;
        };
        if let Some(result) = message.get("result") {
            let _ = waiter.send(Ok(result.clone()));
        } else if let Some(error) = message.get("error") {
            let _ = waiter.send(Err(RpcError {
                code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: error.get("message").and_then(Value::as_str).unwrap_or_default().to_string(),
                data: error.get("data").cloned(),
            }));
        }
    }

    pub fn close(&self) {
        self.inner.closed.cancel();
        let waiters: Vec<_> = self.inner.pending.lock().unwrap_or_else(|e| e.into_inner()).drain().map(|(_, w)| w).collect();
        for waiter in waiters {
            let _ = waiter.send(Err(RpcError::closed()));
        }
    }

    pub fn closed(&self) -> CancellationToken {
        self.inner.closed.clone()
    }
}

// One JSON text per line; stops (and closes the peer) when the stream fails or the peer closes.
pub async fn write_loop<W: AsyncWrite + Unpin>(mut writer: W, mut rx: mpsc::UnboundedReceiver<Value>, peer: Peer) {
    let closed = peer.closed();
    let write = async |writer: &mut W, message: Value| -> std::io::Result<()> {
        let mut line = serde_json::to_string(&message).unwrap_or_default();
        line.push('\n');
        writer.write_all(line.as_bytes()).await?;
        writer.flush().await
    };
    loop {
        let message = tokio::select! {
            biased;
            message = rx.recv() => message,
            _ = closed.cancelled() => {
                while let Ok(message) = rx.try_recv() {
                    if write(&mut writer, message).await.is_err() {
                        break;
                    }
                }
                return;
            }
        };
        let Some(message) = message else { return };
        if write(&mut writer, message).await.is_err() {
            peer.close();
            return;
        }
    }
}
