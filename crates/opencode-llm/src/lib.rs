pub mod anthropic;
pub mod azure;
pub mod bedrock;
pub mod catalog;
pub mod chat;
pub mod client;
pub mod codex;
pub mod copilot;
pub mod dialect;
pub mod digitalocean;
pub mod eventstream;
pub mod flock;
pub mod gemini;
pub mod hooks;
pub mod oauth;
pub mod provider;
pub mod responses;
pub mod retry;
pub mod shape;
pub mod sigv4;
pub mod snowflake;
pub mod sse;
pub mod transform;
pub mod variants;
pub mod vertex;
pub mod xai;

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

// One replayable reasoning block: Anthropic `thinking` (signature) or an OpenAI Responses reasoning item (item_id/encrypted).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Reasoning {
    pub text: String,
    pub signature: Option<String>,
    pub item_id: Option<String>,
    pub encrypted: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Text(String),
    // `data` is raw base64 (no data: prefix).
    Media { mime: String, data: String, filename: Option<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    User(String),
    UserParts(Vec<Part>),
    Assistant { text: String, reasoning: String, tool_calls: Vec<ToolCall>, reasoning_parts: Vec<Reasoning> },
    Tool { call_id: String, content: String, is_error: bool, attachments: Vec<Part> },
}

impl Message {
    pub fn tool(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Message::Tool { call_id: call_id.into(), content: content.into(), is_error: false, attachments: Vec::new() }
    }

    pub fn assistant(text: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Message::Assistant { text: text.into(), reasoning: String::new(), tool_calls, reasoning_parts: Vec::new() }
    }
}

#[derive(Debug, Clone)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Tool(String),
}

#[derive(Debug, Clone, Default)]
pub struct Request {
    pub model: String,
    pub system: Vec<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<Tool>,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u64>,
    pub tool_choice: Option<ToolChoice>,
    // Flat provider options (`store`, `reasoningEffort`, `thinking`, ...); Null means "use the resolved defaults".
    pub options: Value,
    // Per-call headers, applied over provider and model headers.
    pub headers: Vec<(String, String)>,
    // Prompt-cache markers on the first two system blocks and the last two messages.
    pub cache: bool,
    // Name of the model variant whose options `shape::apply` merges over the defaults.
    pub variant: Option<String>,
}

// `input` includes cached tokens and `output` includes reasoning tokens.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCall(ToolCall),
    Finish { reason: String, usage: Usage },
}

// What a finished stream leaves behind for replaying the turn later.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Completion {
    pub reasoning: Vec<Reasoning>,
    pub response_id: Option<String>,
    // Copilot's billed amount (`copilot_usage.total_nano_aiu`); cost = value / 100_000_000_000 when present.
    pub nano_aiu: Option<f64>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Http(#[from] reqwest::Error),
    #[error("provider returned {status}: {body}")]
    Status { status: u16, body: String, headers: Vec<(String, String)> },
    #[error("provider stream error: {0}")]
    Stream(String),
    // An error event delivered inside a successful stream.
    #[error("{message}")]
    Provider { code: Option<String>, message: String },
    #[error("{0}")]
    Config(String),
    // `code` is the original's error name: HeaderTimeoutError, ResponseStreamError or TimeoutError.
    #[error("{message}")]
    Timeout { code: &'static str, message: String },
}

impl Error {
    pub fn header_timeout(ms: u64) -> Self {
        Error::Timeout { code: "HeaderTimeoutError", message: format!("Provider response headers timed out after {ms}ms") }
    }

    pub fn chunk_timeout() -> Self {
        Error::Timeout { code: "ResponseStreamError", message: "SSE read timed out".into() }
    }

    pub fn total_timeout() -> Self {
        Error::Timeout { code: "TimeoutError", message: "The operation timed out.".into() }
    }
}

// JSON.stringify prints whole floats as integers; keep the wire form identical.
pub(crate) fn number(v: f64) -> Value {
    if v.fract() == 0.0 && v.abs() < 9e15 { (v as i64).into() } else { v.into() }
}

#[cfg(test)]
mod flows_tests;

#[cfg(test)]
pub(crate) mod testutil {
    use serde_json::Value;
    use std::path::PathBuf;

    // None when the TypeScript checkout (and its recordings) is not next to this workspace.
    pub fn cassette(path: &str) -> Option<Value> {
        let file = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../opencode-ts/packages").join(path);
        serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()
    }

    pub fn request_body(cassette: &Value, index: usize) -> Value {
        serde_json::from_str(cassette["interactions"][index]["request"]["body"].as_str().unwrap()).unwrap()
    }

    pub fn response_body(cassette: &Value, index: usize) -> &str {
        cassette["interactions"][index]["response"]["body"].as_str().unwrap()
    }
}
