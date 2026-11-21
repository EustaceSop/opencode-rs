// Wire shapes of the legacy API (`packages/sdk/openapi.json`). Readers are lenient: unknown keys are ignored and
// optional ones default, so newer servers stay readable.
use indexmap::IndexMap;
use opencode_storage::{FilePart, PermissionRule};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Health {
    pub healthy: bool,
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct PathInfo {
    pub home: String,
    pub state: String,
    pub config: String,
    pub worktree: String,
    pub directory: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRef {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(rename = "modelID")]
    pub model_id: String,
}

impl ModelRef {
    // `provider/model`, split at the first slash.
    pub fn parse(spec: &str) -> Option<Self> {
        let (provider_id, model_id) = spec.split_once('/')?;
        Some(Self { provider_id: provider_id.to_string(), model_id: model_id.to_string() })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Capabilities {
    pub reasoning: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Limit {
    pub context: f64,
    pub output: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Model {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(rename = "providerID", default)]
    pub provider_id: String,
    #[serde(default)]
    pub capabilities: Option<Capabilities>,
    #[serde(default)]
    pub limit: Option<Limit>,
    #[serde(default)]
    pub variants: Option<IndexMap<String, Value>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Provider {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub models: IndexMap<String, Model>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ProviderList {
    pub providers: Vec<Provider>,
    // providerID -> modelID
    pub default: IndexMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Agent {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    // "primary" | "subagent" | "all"
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub hidden: Option<bool>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub model: Option<ModelRef>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Command {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SessionStatus {
    Idle,
    Busy,
    Retry {
        attempt: u32,
        #[serde(default)]
        message: String,
        #[serde(default)]
        next: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolRef {
    #[serde(rename = "messageID")]
    pub message_id: String,
    #[serde(rename = "callID")]
    pub call_id: String,
}

// `permission.asked` / `GET /permission`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub permission: String,
    #[serde(default)]
    pub patterns: Vec<String>,
    #[serde(default)]
    pub metadata: Value,
    #[serde(default)]
    pub always: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<ToolRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionReply {
    Once,
    Always,
    Reject,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionInfo {
    pub question: String,
    #[serde(default)]
    pub header: String,
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiple: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<bool>,
}

// `question.asked` / `GET /question`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionRequest {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub questions: Vec<QuestionInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<ToolRef>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionQuery {
    pub roots: Option<bool>,
    pub start: Option<u64>,
    pub search: Option<String>,
    pub limit: Option<usize>,
}

// `POST /session`
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct CreateSession {
    #[serde(rename = "parentID", skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission: Option<Vec<PermissionRule>>,
}

// `PATCH /session/{id}`
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SessionPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PromptPart {
    Text { text: String },
    File(FilePart),
}

// `POST /session/{id}/message` and `/prompt_async`
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PromptBody {
    #[serde(rename = "messageID", skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(rename = "noReply", skip_serializing_if = "Option::is_none")]
    pub no_reply: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<BTreeMap<String, bool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    pub parts: Vec<PromptPart>,
}

// `POST /session/{id}/command`; `model` is a `provider/model` string here.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct CommandBody {
    #[serde(rename = "messageID", skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub arguments: String,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<PromptPart>,
}

// `POST /session/{id}/shell`
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ShellBody {
    #[serde(rename = "messageID", skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    pub agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    pub command: String,
}
