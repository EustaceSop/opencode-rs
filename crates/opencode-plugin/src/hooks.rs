//! Hook names and payload shapes from `@opencode-ai/plugin` v1.19.5 (`plugin/src/index.ts:222-335`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

macro_rules! hook_names {
    ($($variant:ident => $name:literal),* $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum HookName {
            $(#[serde(rename = $name)] $variant,)*
        }

        impl HookName {
            pub const ALL: &'static [HookName] = &[$(HookName::$variant,)*];

            pub fn as_str(self) -> &'static str {
                match self {
                    $(HookName::$variant => $name,)*
                }
            }

            pub fn parse(name: &str) -> Option<HookName> {
                match name {
                    $($name => Some(HookName::$variant),)*
                    _ => None,
                }
            }
        }
    };
}

hook_names! {
    ChatMessage => "chat.message",
    ChatParams => "chat.params",
    ChatHeaders => "chat.headers",
    PermissionAsk => "permission.ask",
    CommandExecuteBefore => "command.execute.before",
    ToolExecuteBefore => "tool.execute.before",
    ShellEnv => "shell.env",
    ToolExecuteAfter => "tool.execute.after",
    ExperimentalChatMessagesTransform => "experimental.chat.messages.transform",
    ExperimentalChatSystemTransform => "experimental.chat.system.transform",
    ExperimentalProviderSmallModel => "experimental.provider.small_model",
    ExperimentalSessionCompacting => "experimental.session.compacting",
    ExperimentalCompactionAutocontinue => "experimental.compaction.autocontinue",
    ExperimentalTextComplete => "experimental.text.complete",
    ToolDefinition => "tool.definition",
}

impl std::fmt::Display for HookName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A trigger hook with typed input/output; `HookName::Experimental*` use raw `Value` via `PluginHost::trigger`.
pub trait Hook {
    const NAME: HookName;
    type Input: Serialize;
    type Output: Serialize + serde::de::DeserializeOwned;
}

macro_rules! hook {
    ($marker:ident, $name:ident, $input:ty, $output:ty) => {
        pub struct $marker;
        impl Hook for $marker {
            const NAME: HookName = HookName::$name;
            type Input = $input;
            type Output = $output;
        }
    };
}

hook!(ChatMessage, ChatMessage, ChatMessageInput, ChatMessageOutput);
hook!(ChatParams, ChatParams, ChatInput, ChatParamsOutput);
hook!(ChatHeaders, ChatHeaders, ChatInput, ChatHeadersOutput);
hook!(ToolExecuteBefore, ToolExecuteBefore, ToolExecuteBeforeInput, ToolExecuteBeforeOutput);
hook!(ToolExecuteAfter, ToolExecuteAfter, ToolExecuteAfterInput, ToolExecuteAfterOutput);
hook!(ToolDefinition, ToolDefinition, ToolDefinitionInput, ToolDefinitionOutput);
hook!(ShellEnv, ShellEnv, ShellEnvInput, ShellEnvOutput);
hook!(PermissionAsk, PermissionAsk, Value, PermissionAskOutput);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRef {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(rename = "modelID")]
    pub model_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatMessageInput {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    #[serde(rename = "messageID", default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatMessageOutput {
    pub message: Value,
    pub parts: Vec<Value>,
}

/// Input of `chat.params` and `chat.headers`: `model` is the v1 SDK `Model`, `provider` the `ProviderContext`
/// (`{source, info, options}`), `message` the SDK `UserMessage`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatInput {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub agent: String,
    pub model: Value,
    pub provider: Value,
    pub message: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatParamsOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(rename = "topP", default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(rename = "topK", default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<f64>,
    #[serde(rename = "maxOutputTokens", default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub options: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatHeadersOutput {
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolExecuteBeforeInput {
    pub tool: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "callID")]
    pub call_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolExecuteBeforeOutput {
    #[serde(default)]
    pub args: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolExecuteAfterInput {
    pub tool: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "callID")]
    pub call_id: String,
    pub args: Value,
}

/// `extra` carries whatever else the call site spreads in (`attachments`, raw MCP `CallToolResult` fields).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolExecuteAfterOutput {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub metadata: Value,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinitionInput {
    #[serde(rename = "toolID")]
    pub tool_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinitionOutput {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub parameters: Value,
    #[serde(rename = "jsonSchema", default, skip_serializing_if = "Option::is_none")]
    pub json_schema: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ShellEnvInput {
    pub cwd: String,
    #[serde(rename = "sessionID", default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(rename = "callID", default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ShellEnvOutput {
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionStatus {
    #[default]
    Ask,
    Deny,
    Allow,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PermissionAskOutput {
    pub status: PermissionStatus,
}

/// Payload of the non-trigger `event` hook: `{event: {id, type, properties}}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub properties: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fifteen_hooks_round_trip() {
        assert_eq!(HookName::ALL.len(), 15);
        for hook in HookName::ALL {
            assert_eq!(HookName::parse(hook.as_str()), Some(*hook));
            assert_eq!(serde_json::to_value(hook).unwrap(), json!(hook.as_str()));
        }
        assert_eq!(HookName::parse("event"), None);
    }

    #[test]
    fn wire_names() {
        let input = ToolExecuteAfterInput {
            tool: "bash".into(),
            session_id: "s".into(),
            call_id: "c".into(),
            args: json!({"a": 1}),
        };
        assert_eq!(
            serde_json::to_value(&input).unwrap(),
            json!({"tool": "bash", "sessionID": "s", "callID": "c", "args": {"a": 1}})
        );
        let out: ToolExecuteAfterOutput =
            serde_json::from_value(json!({"title": "t", "output": "o", "metadata": {}, "attachments": []})).unwrap();
        assert_eq!(out.extra["attachments"], json!([]));
        let params = ChatParamsOutput { temperature: Some(0.5), ..Default::default() };
        assert_eq!(serde_json::to_value(&params).unwrap(), json!({"temperature": 0.5, "options": {}}));
        let ask: PermissionAskOutput = serde_json::from_value(json!({"status": "allow"})).unwrap();
        assert_eq!(ask.status, PermissionStatus::Allow);
    }
}
