//! Remote transports: Streamable HTTP and the legacy HTTP+SSE pair.

use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use indexmap::IndexMap;
use reqwest::{RequestBuilder, Response, Url};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::client::{Inbound, InboundTx, Transport};
use crate::error::McpError;
use crate::oauth::{AuthOutcome, OAuthProvider};

#[derive(Debug, Default, PartialEq)]
pub(crate) struct SseEvent {
    pub event: String,
    pub data: String,
}

#[derive(Default)]
pub(crate) struct SseParser {
    buffer: Vec<u8>,
    event: String,
    data: Vec<String>,
}

impl SseParser {
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(end) = self.buffer.iter().position(|b| *b == b'\n' || *b == b'\r') {
            // A trailing `\r` may be the first half of `\r\n`.
            if self.buffer[end] == b'\r' && end + 1 == self.buffer.len() {
                break;
            }
            let skip = if self.buffer[end] == b'\r' && self.buffer[end + 1] == b'\n' {
                2
            } else {
                1
            };
            let line = String::from_utf8_lossy(&self.buffer[..end]).into_owned();
            self.buffer.drain(..end + skip);
            if line.is_empty() {
                if !self.data.is_empty() || !self.event.is_empty() {
                    events.push(SseEvent {
                        event: std::mem::take(&mut self.event),
                        data: std::mem::take(&mut self.data).join("\n"),
                    });
                }
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = match line.split_once(':') {
                Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
                None => (line.as_str(), ""),
            };
            match field {
                "event" => self.event = value.to_string(),
                "data" => self.data.push(value.to_string()),
                _ => {}
            }
        }
        events
    }
}

struct Shared {
    http: reqwest::Client,
    headers: IndexMap<String, String>,
    auth: Option<Arc<OAuthProvider>>,
    protocol: Mutex<Option<String>>,
    cancel: CancellationToken,
    tx: InboundTx,
}

impl Shared {
    // Configured headers win over the generated ones, as in the SDK's `_commonHeaders`.
    async fn decorate(
        &self,
        mut request: RequestBuilder,
        extra: &[(&str, String)],
    ) -> RequestBuilder {
        let configured = |name: &str| {
            self.headers
                .keys()
                .any(|key| key.eq_ignore_ascii_case(name))
        };
        if let Some(auth) = &self.auth
            && !configured("authorization")
            && let Some(token) = auth.access_token().await
        {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let protocol = self.protocol.lock().unwrap().clone();
        if let Some(version) = protocol
            && !configured("mcp-protocol-version")
        {
            request = request.header("mcp-protocol-version", version);
        }
        for (name, value) in extra {
            if !configured(name) {
                request = request.header(*name, value);
            }
        }
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        request
    }

    async fn reauthorize(&self, response: &Response) -> Result<(), McpError> {
        let Some(auth) = &self.auth else {
            return Err(McpError::unauthorized());
        };
        let header = response
            .headers()
            .get("www-authenticate")
            .and_then(|value| value.to_str().ok());
        match auth.handle_unauthorized(header).await? {
            AuthOutcome::Authorized => Ok(()),
            AuthOutcome::Redirect => Err(McpError::unauthorized()),
        }
    }

    fn deliver(&self, data: &str) {
        match serde_json::from_str::<Value>(data) {
            Ok(Value::Array(items)) => {
                for item in items {
                    let _ = self.tx.send(Inbound::Message(item));
                }
            }
            Ok(value) => {
                let _ = self.tx.send(Inbound::Message(value));
            }
            Err(_) => {}
        }
    }

    /// Pumps `message` events into the session until the stream ends or the transport closes.
    async fn pump(&self, response: Response, mut parser: SseParser) {
        let mut body = response.bytes_stream();
        loop {
            let chunk = tokio::select! {
                _ = self.cancel.cancelled() => return,
                chunk = body.next() => chunk,
            };
            let Some(Ok(bytes)) = chunk else { return };
            for event in parser.feed(&bytes) {
                if event.data.is_empty() {
                    continue;
                }
                if event.event.is_empty() || event.event == "message" {
                    self.deliver(&event.data);
                }
            }
        }
    }
}

fn is_request(message: &Value) -> bool {
    message.get("method").is_some() && message.get("id").is_some_and(|id| !id.is_null())
}

pub(crate) struct StreamableHttpTransport {
    shared: Arc<Shared>,
    url: Url,
    session: Mutex<Option<String>>,
}

impl StreamableHttpTransport {
    pub(crate) fn new(
        http: reqwest::Client,
        url: Url,
        headers: IndexMap<String, String>,
        auth: Option<Arc<OAuthProvider>>,
        tx: InboundTx,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                http,
                headers,
                auth,
                protocol: Mutex::new(None),
                cancel: CancellationToken::new(),
                tx,
            }),
            url,
            session: Mutex::new(None),
        }
    }

    fn session_header(&self) -> Vec<(&'static str, String)> {
        self.session
            .lock()
            .unwrap()
            .clone()
            .map(|id| ("mcp-session-id", id))
            .into_iter()
            .collect()
    }

    // Optional standalone stream for server-initiated messages; a 405 means the server has none.
    fn open_listen_stream(&self) {
        let shared = self.shared.clone();
        let url = self.url.clone();
        let mut extra = self.session_header();
        extra.push(("Accept", "text/event-stream".to_string()));
        tokio::spawn(async move {
            let request = shared.decorate(shared.http.get(url), &extra).await;
            let response = tokio::select! {
                _ = shared.cancel.cancelled() => return,
                response = request.send() => response,
            };
            if let Ok(response) = response
                && response.status().is_success()
            {
                shared.pump(response, SseParser::default()).await;
            }
        });
    }
}

#[async_trait::async_trait]
impl Transport for StreamableHttpTransport {
    async fn send(&self, message: &Value) -> Result<(), McpError> {
        if self.shared.cancel.is_cancelled() {
            return Err(McpError::NotConnected);
        }
        let body = message.to_string();
        let mut authorized = false;
        loop {
            let mut extra = self.session_header();
            extra.push(("content-type", "application/json".to_string()));
            extra.push(("accept", "application/json, text/event-stream".to_string()));
            let request = self
                .shared
                .decorate(self.shared.http.post(self.url.clone()), &extra)
                .await
                .body(body.clone());
            let response = request
                .send()
                .await
                .map_err(|e| McpError::Other(e.to_string()))?;
            if let Some(id) = response
                .headers()
                .get("mcp-session-id")
                .and_then(|value| value.to_str().ok())
            {
                *self.session.lock().unwrap() = Some(id.to_string());
            }
            let status = response.status();
            if !status.is_success() {
                if status.as_u16() == 401 && self.shared.auth.is_some() {
                    if authorized {
                        return Err(McpError::other(
                            "Streamable HTTP error: Server returned 401 after successful authentication",
                        ));
                    }
                    self.shared.reauthorize(&response).await?;
                    authorized = true;
                    continue;
                }
                let text = response.text().await.unwrap_or_default();
                return Err(McpError::Other(format!(
                    "Streamable HTTP error: Error POSTing to endpoint: {text}"
                )));
            }
            if status.as_u16() == 202 {
                if message.get("method").and_then(Value::as_str)
                    == Some("notifications/initialized")
                {
                    self.open_listen_stream();
                }
                return Ok(());
            }
            if !is_request(message) {
                return Ok(());
            }
            let content_type = response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string();
            if content_type.contains("text/event-stream") {
                let shared = self.shared.clone();
                tokio::spawn(async move { shared.pump(response, SseParser::default()).await });
                return Ok(());
            }
            if content_type.contains("application/json") {
                let text = response
                    .text()
                    .await
                    .map_err(|e| McpError::Other(e.to_string()))?;
                serde_json::from_str::<Value>(&text).map_err(|e| McpError::Other(e.to_string()))?;
                self.shared.deliver(&text);
                return Ok(());
            }
            return Err(McpError::Other(format!(
                "Streamable HTTP error: Unexpected content type: {content_type}"
            )));
        }
    }

    async fn close(&self) {
        self.shared.cancel.cancel();
        let _ = self.shared.tx.send(Inbound::Closed);
    }

    fn set_protocol_version(&self, version: &str) {
        *self.shared.protocol.lock().unwrap() = Some(version.to_string());
    }
}

pub(crate) struct SseTransport {
    shared: Arc<Shared>,
    endpoint: Url,
}

impl SseTransport {
    /// Opens the event stream and waits for the `endpoint` event.
    pub(crate) async fn connect(
        http: reqwest::Client,
        url: Url,
        headers: IndexMap<String, String>,
        auth: Option<Arc<OAuthProvider>>,
        tx: InboundTx,
    ) -> Result<Self, McpError> {
        let shared = Arc::new(Shared {
            http,
            headers,
            auth,
            protocol: Mutex::new(None),
            cancel: CancellationToken::new(),
            tx,
        });
        let mut authorized = false;
        let response = loop {
            let request = shared
                .decorate(
                    shared.http.get(url.clone()),
                    &[("Accept", "text/event-stream".to_string())],
                )
                .await;
            let response = request
                .send()
                .await
                .map_err(|e| McpError::Other(format!("SSE error: {e}")))?;
            let status = response.status().as_u16();
            if status == 401 && shared.auth.is_some() && !authorized {
                shared.reauthorize(&response).await?;
                authorized = true;
                continue;
            }
            if status != 200 {
                return Err(McpError::Other(format!(
                    "SSE error: Non-200 status code ({status})"
                )));
            }
            break response;
        };

        let mut parser = SseParser::default();
        let mut body = response.bytes_stream();
        let mut backlog = Vec::new();
        let endpoint = 'wait: loop {
            let Some(Ok(bytes)) = body.next().await else {
                return Err(McpError::other(
                    "SSE error: stream closed before the endpoint event",
                ));
            };
            let mut found = None;
            for event in parser.feed(&bytes) {
                if found.is_none() && event.event == "endpoint" {
                    found = Some(event.data);
                } else if found.is_some() {
                    backlog.push(event);
                }
            }
            if let Some(found) = found {
                break 'wait found;
            }
        };
        let endpoint = url
            .join(&endpoint)
            .map_err(|e| McpError::Other(format!("Invalid endpoint URL: {e}")))?;
        if endpoint.origin() != url.origin() {
            return Err(McpError::Other(format!(
                "Endpoint origin does not match connection origin: {}",
                endpoint.origin().ascii_serialization()
            )));
        }

        let reader = shared.clone();
        tokio::spawn(async move {
            for event in backlog {
                if event.event.is_empty() || event.event == "message" {
                    reader.deliver(&event.data);
                }
            }
            loop {
                let chunk = tokio::select! {
                    _ = reader.cancel.cancelled() => break,
                    chunk = body.next() => chunk,
                };
                let Some(Ok(bytes)) = chunk else { break };
                for event in parser.feed(&bytes) {
                    if event.event.is_empty() || event.event == "message" {
                        reader.deliver(&event.data);
                    }
                }
            }
            let _ = reader.tx.send(Inbound::Closed);
        });
        Ok(Self { shared, endpoint })
    }
}

#[async_trait::async_trait]
impl Transport for SseTransport {
    async fn send(&self, message: &Value) -> Result<(), McpError> {
        if self.shared.cancel.is_cancelled() {
            return Err(McpError::NotConnected);
        }
        let body = message.to_string();
        let mut authorized = false;
        loop {
            let request = self
                .shared
                .decorate(
                    self.shared.http.post(self.endpoint.clone()),
                    &[("content-type", "application/json".to_string())],
                )
                .await
                .body(body.clone());
            let response = request
                .send()
                .await
                .map_err(|e| McpError::Other(e.to_string()))?;
            let status = response.status();
            if status.is_success() {
                return Ok(());
            }
            if status.as_u16() == 401 && self.shared.auth.is_some() && !authorized {
                self.shared.reauthorize(&response).await?;
                authorized = true;
                continue;
            }
            let text = response.text().await.unwrap_or_default();
            return Err(McpError::Other(format!(
                "Error POSTing to endpoint (HTTP {}): {text}",
                status.as_u16()
            )));
        }
    }

    async fn close(&self) {
        self.shared.cancel.cancel();
        let _ = self.shared.tx.send(Inbound::Closed);
    }

    fn set_protocol_version(&self, version: &str) {
        *self.shared.protocol.lock().unwrap() = Some(version.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_parser_handles_split_chunks_and_line_endings() {
        let mut parser = SseParser::default();
        assert!(
            parser
                .feed(b"event: endpoint\r\ndata: /messages?id=1\r")
                .is_empty()
        );
        let events = parser.feed(b"\n\r\n: ping\n\ndata: {\"a\":\ndata: 1}\n\n");
        assert_eq!(
            events,
            vec![
                SseEvent {
                    event: "endpoint".into(),
                    data: "/messages?id=1".into()
                },
                SseEvent {
                    event: String::new(),
                    data: "{\"a\":\n1}".into()
                },
            ]
        );
    }
}
