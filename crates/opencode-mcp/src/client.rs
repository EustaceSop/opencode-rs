//! JSON-RPC session layer of the MCP client (`@modelcontextprotocol/sdk@1.29.0` `Client`/`Protocol`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::sync::{Notify, broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::error::McpError;

pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 5] = [
    LATEST_PROTOCOL_VERSION,
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
    "2024-10-07",
];
/// SDK `DEFAULT_REQUEST_TIMEOUT_MSEC`, used when no timeout is configured.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_millis(60_000);

pub(crate) enum Inbound {
    Message(Value),
    Closed,
}

pub(crate) type InboundTx = mpsc::UnboundedSender<Inbound>;

#[async_trait::async_trait]
pub(crate) trait Transport: Send + Sync {
    async fn send(&self, message: &Value) -> Result<(), McpError>;
    async fn close(&self);
    fn pid(&self) -> Option<u32> {
        None
    }
    fn set_protocol_version(&self, _version: &str) {}
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Clone, Default)]
pub struct RequestOptions {
    pub timeout: Option<Duration>,
    pub reset_timeout_on_progress: bool,
    pub abort: Option<CancellationToken>,
}

impl RequestOptions {
    pub fn timeout_ms(ms: Option<u64>) -> Self {
        Self {
            timeout: ms.map(Duration::from_millis),
            ..Self::default()
        }
    }
}

struct Pending {
    tx: oneshot::Sender<Result<Value, McpError>>,
    progress: Arc<Notify>,
}

struct Inner {
    transport: Box<dyn Transport>,
    pending: Mutex<HashMap<u64, Pending>>,
    next_id: AtomicU64,
    closed: CancellationToken,
    notifications: broadcast::Sender<Notification>,
    roots_uri: String,
    init: OnceLock<Value>,
}

struct PendingGuard<'a> {
    inner: &'a Inner,
    id: u64,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.inner.pending.lock().unwrap().remove(&self.id);
    }
}

/// One connected MCP server session.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl Client {
    pub(crate) fn start(
        transport: Box<dyn Transport>,
        mut inbound: mpsc::UnboundedReceiver<Inbound>,
        roots_uri: String,
    ) -> Self {
        let inner = Arc::new(Inner {
            transport,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            closed: CancellationToken::new(),
            notifications: broadcast::channel(64).0,
            roots_uri,
            init: OnceLock::new(),
        });
        let reader = inner.clone();
        tokio::spawn(async move {
            loop {
                let message = tokio::select! {
                    _ = reader.closed.cancelled() => break,
                    m = inbound.recv() => m,
                };
                match message {
                    Some(Inbound::Message(value)) => reader.dispatch(value),
                    Some(Inbound::Closed) | None => break,
                }
            }
            reader.mark_closed();
        });
        Self { inner }
    }

    pub(crate) async fn initialize(&self, version: &str) -> Result<(), McpError> {
        let params = json!({
            "protocolVersion": LATEST_PROTOCOL_VERSION,
            "capabilities": {"roots": {}},
            "clientInfo": {"name": "opencode", "version": version},
        });
        let result = self
            .request("initialize", Some(params), RequestOptions::default())
            .await?;
        let negotiated = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&negotiated.as_str()) {
            return Err(McpError::Other(format!(
                "Server's protocol version is not supported: {negotiated}"
            )));
        }
        self.inner.transport.set_protocol_version(&negotiated);
        let _ = self.inner.init.set(result);
        self.notify("notifications/initialized", None).await
    }

    pub fn server_capabilities(&self) -> Option<&Value> {
        self.inner.init.get().and_then(|v| v.get("capabilities"))
    }

    pub fn has_capability(&self, name: &str) -> bool {
        self.server_capabilities()
            .and_then(|c| c.get(name))
            .is_some_and(js_truthy)
    }

    pub fn server_info(&self) -> Option<&Value> {
        self.inner.init.get().and_then(|v| v.get("serverInfo"))
    }

    pub fn instructions(&self) -> Option<String> {
        let text = self.inner.init.get()?.get("instructions")?.as_str()?.trim();
        (!text.is_empty()).then(|| text.to_string())
    }

    pub fn pid(&self) -> Option<u32> {
        self.inner.transport.pid()
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.is_cancelled()
    }

    pub async fn closed(&self) {
        self.inner.closed.cancelled().await
    }

    pub fn notifications(&self) -> broadcast::Receiver<Notification> {
        self.inner.notifications.subscribe()
    }

    pub(crate) fn same(&self, other: &Client) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub async fn close(&self) {
        self.inner.transport.close().await;
        self.inner.mark_closed();
    }

    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpError> {
        if self.is_closed() {
            return Err(McpError::NotConnected);
        }
        let mut message = Map::new();
        message.insert("jsonrpc".into(), "2.0".into());
        message.insert("method".into(), method.into());
        if let Some(params) = params {
            message.insert("params".into(), params);
        }
        self.inner.transport.send(&Value::Object(message)).await
    }

    pub async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        options: RequestOptions,
    ) -> Result<Value, McpError> {
        let inner = &*self.inner;
        if self.is_closed() {
            return Err(McpError::NotConnected);
        }
        let id = inner.next_id.fetch_add(1, Ordering::Relaxed);
        let mut params = params;
        if options.reset_timeout_on_progress {
            let object = params.get_or_insert_with(|| json!({}));
            if let Some(map) = object.as_object_mut() {
                let meta = map.entry("_meta").or_insert_with(|| json!({}));
                if let Some(meta) = meta.as_object_mut() {
                    meta.insert("progressToken".into(), id.into());
                }
            }
        }
        let mut message = Map::new();
        message.insert("jsonrpc".into(), "2.0".into());
        message.insert("id".into(), id.into());
        message.insert("method".into(), method.into());
        if let Some(params) = params {
            message.insert("params".into(), params);
        }

        let (tx, mut rx) = oneshot::channel();
        let progress = Arc::new(Notify::new());
        inner.pending.lock().unwrap().insert(
            id,
            Pending {
                tx,
                progress: progress.clone(),
            },
        );
        let _guard = PendingGuard { inner, id };

        let timeout = options.timeout.unwrap_or(DEFAULT_REQUEST_TIMEOUT);
        let abort = options.abort.clone().unwrap_or_default();
        let message = Value::Object(message);
        let exchange = async {
            tokio::select! {
                sent = inner.transport.send(&message) => sent?,
                _ = tokio::time::sleep(timeout) => return Err(McpError::Timeout),
            }
            loop {
                tokio::select! {
                    response = &mut rx => return response.unwrap_or(Err(McpError::Closed)),
                    _ = progress.notified(), if options.reset_timeout_on_progress => continue,
                    _ = tokio::time::sleep(timeout) => return Err(McpError::Timeout),
                }
            }
        };
        let result = tokio::select! {
            result = exchange => result,
            _ = abort.cancelled() => Err(McpError::Aborted),
            _ = inner.closed.cancelled() => Err(McpError::Closed),
        };
        if let Err(error @ (McpError::Timeout | McpError::Aborted)) = &result {
            let cancel = json!({
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": {"requestId": id, "reason": error.to_string()},
            });
            let this = self.inner.clone();
            tokio::spawn(async move {
                let _ = this.transport.send(&cancel).await;
            });
        }
        result
    }
}

impl Inner {
    fn mark_closed(&self) {
        self.closed.cancel();
        let pending = std::mem::take(&mut *self.pending.lock().unwrap());
        for (_, entry) in pending {
            let _ = entry.tx.send(Err(McpError::Closed));
        }
    }

    fn dispatch(self: &Arc<Self>, value: Value) {
        let Some(object) = value.as_object() else {
            return;
        };
        let method = object.get("method").and_then(Value::as_str);
        let id = object.get("id").filter(|id| !id.is_null());
        match (method, id) {
            (Some(method), Some(id)) => {
                let reply = match method {
                    "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                    "roots/list" => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"roots": [{"uri": self.roots_uri}]},
                    }),
                    _ => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32601, "message": "Method not found"},
                    }),
                };
                let this = self.clone();
                tokio::spawn(async move {
                    let _ = this.transport.send(&reply).await;
                });
            }
            (Some(method), None) => {
                let params = object.get("params").cloned().unwrap_or(Value::Null);
                if method == "notifications/progress"
                    && let Some(token) = params.get("progressToken").and_then(Value::as_u64)
                    && let Some(entry) = self.pending.lock().unwrap().get(&token)
                {
                    entry.progress.notify_one();
                }
                let _ = self.notifications.send(Notification {
                    method: method.to_string(),
                    params,
                });
            }
            (None, Some(id)) => {
                let Some(id) = id.as_u64() else { return };
                let Some(entry) = self.pending.lock().unwrap().remove(&id) else {
                    return;
                };
                let result = match object.get("error") {
                    Some(error) => Err(McpError::Rpc {
                        code: error.get("code").and_then(Value::as_i64).unwrap_or(-32603),
                        message: error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        data: error.get("data").cloned(),
                    }),
                    None => Ok(object.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = entry.tx.send(result);
            }
            (None, None) => {}
        }
    }
}

pub(crate) fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}
