//! `Tool` proxy for plugin / custom tools living in the sidecar (`tool/registry.ts:124-178`).

use std::sync::Arc;

use opencode_tools::{Attachment, Tool, ToolContext, ToolError, ToolOutput};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::host::{Ask, Inner, PluginError, ToolInfo};

pub struct PluginTool {
    inner: Arc<Inner>,
    info: ToolInfo,
}

impl PluginTool {
    pub(crate) fn new(inner: Arc<Inner>, info: ToolInfo) -> Self {
        Self { inner, info }
    }
}

struct Registration<'a> {
    inner: &'a Inner,
    id: u64,
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        self.inner.unregister_asks(self.id);
    }
}

fn output(value: Value) -> ToolOutput {
    let text = |key: &str| value.get(key).and_then(Value::as_str).map(String::from);
    let attachments = value
        .get("attachments")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|item| {
                    Some(Attachment {
                        mime: item.get("mime")?.as_str()?.to_string(),
                        url: item.get("url")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    ToolOutput {
        title: text("title").unwrap_or_default(),
        output: text("output")
            .unwrap_or_else(|| value.get("output").filter(|v| !v.is_null()).map(Value::to_string).unwrap_or_default()),
        metadata: value.get("metadata").cloned().unwrap_or_else(|| json!({})),
        attachments,
    }
}

#[async_trait::async_trait]
impl Tool for PluginTool {
    fn id(&self) -> &str {
        &self.info.id
    }

    fn description(&self) -> String {
        self.info.description.clone()
    }

    fn parameters(&self) -> Value {
        self.info.parameters.clone()
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        if ctx.abort.is_cancelled() {
            return Err(ToolError::Aborted);
        }
        let inner = &*self.inner;
        let id = inner.next_id();
        let (ask_tx, mut asks) = mpsc::unbounded_channel::<Ask>();
        inner.register_asks(id, ask_tx);
        let _registration = Registration { inner, id };
        let params = json!({
            "id": self.info.id,
            "args": args,
            "context": {
                "sessionID": ctx.session_id,
                "messageID": ctx.message_id,
                "agent": ctx.agent,
                "callID": ctx.call_id,
            },
        });
        let mut reply = inner.begin(id, "tool.execute", params).map_err(|e| ToolError::Other(e.to_string()))?;
        let deadline = inner.tool_timeout.map(|limit| (limit, tokio::time::Instant::now() + limit));
        let timer = async {
            match deadline {
                Some((_, at)) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(timer);
        let mut rejected = None;

        let result = loop {
            tokio::select! {
                result = &mut reply => break result,
                Some(ask) = asks.recv() => {
                    let decision = tokio::select! {
                        decision = ctx.permissions.ask(ask.request) => decision,
                        _ = ctx.abort.cancelled() => Err(ToolError::Aborted),
                    };
                    match decision {
                        Ok(()) => inner.respond(ask.id, Ok(Value::Null)),
                        Err(error) => {
                            inner.respond(ask.id, Err(error.to_string()));
                            rejected = Some(error);
                        }
                    }
                }
                _ = ctx.abort.cancelled() => {
                    inner.cancel(id);
                    return Err(ToolError::Aborted);
                }
                _ = &mut timer => {
                    inner.cancel(id);
                    let error = PluginError::Timeout {
                        method: format!("tool.execute {}", self.info.id),
                        timeout: deadline.map(|(limit, _)| limit).unwrap_or_default(),
                    };
                    return Err(ToolError::Other(error.to_string()));
                }
            }
        };

        match result {
            Ok(Ok(value)) => Ok(output(value)),
            Ok(Err(PluginError::Remote { code: -32602, message, data, .. }))
                if data.get("kind").and_then(Value::as_str) == Some("invalid_arguments") =>
            {
                Err(ToolError::InvalidArguments { tool: self.info.id.clone(), detail: message })
            }
            Ok(Err(error)) => Err(rejected.unwrap_or_else(|| ToolError::Other(error.to_string()))),
            Err(_) => Err(ToolError::Other(inner.lost().to_string())),
        }
    }
}
