use super::ConfigError;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelRef {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(rename = "modelID")]
    pub model_id: String,
}

impl ModelRef {
    // Provider.parseModel: provider is everything before the first "/".
    pub fn parse(model: &str) -> Self {
        let (provider, rest) = model.split_once('/').unwrap_or((model, ""));
        Self { provider_id: provider.to_string(), model_id: rest.to_string() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    Subagent,
    Primary,
    All,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub name: Option<String>,
    pub model: Option<String>,
    pub variant: Option<String>,
    pub prompt: Option<String>,
    pub description: Option<String>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub disable: Option<bool>,
    pub hidden: Option<bool>,
    pub mode: Option<AgentMode>,
    pub color: Option<String>,
    pub steps: Option<u64>,
    pub options: Map<String, Value>,
    pub permission: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct CommandConfig {
    pub template: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub variant: Option<String>,
    #[serde(default)]
    pub subtask: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct McpOAuth {
    #[serde(rename = "clientId")]
    pub client_id: Option<String>,
    #[serde(rename = "clientSecret")]
    pub client_secret: Option<String>,
    pub scope: Option<String>,
    #[serde(rename = "callbackPort")]
    pub callback_port: Option<u16>,
    #[serde(rename = "redirectUri")]
    pub redirect_uri: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum McpOAuthSetting {
    Toggle(bool),
    Config(McpOAuth),
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpServer {
    Local {
        command: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        environment: IndexMap<String, String>,
        #[serde(default)]
        enabled: Option<bool>,
        #[serde(default)]
        timeout: Option<u64>,
    },
    Remote {
        url: String,
        #[serde(default)]
        enabled: Option<bool>,
        #[serde(default)]
        headers: IndexMap<String, String>,
        #[serde(default)]
        oauth: Option<McpOAuthSetting>,
        #[serde(default)]
        timeout: Option<u64>,
    },
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
#[allow(clippy::large_enum_variant)]
pub enum McpConfig {
    Server(McpServer),
    Toggle { enabled: bool },
}

impl McpConfig {
    pub fn enabled(&self) -> bool {
        match self {
            Self::Toggle { enabled } => *enabled,
            Self::Server(McpServer::Local { enabled, .. } | McpServer::Remote { enabled, .. }) => {
                enabled.unwrap_or(true)
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    pub api: Option<String>,
    pub name: Option<String>,
    pub id: Option<String>,
    pub npm: Option<String>,
    pub env: Option<Vec<String>>,
    pub whitelist: Option<Vec<String>>,
    pub blacklist: Option<Vec<String>>,
    pub options: Map<String, Value>,
    pub models: IndexMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct SkillsConfig {
    pub paths: Vec<String>,
    pub urls: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct CompactionConfig {
    pub auto: Option<bool>,
    pub prune: Option<bool>,
    pub tail_turns: Option<u64>,
    pub preserve_recent_tokens: Option<u64>,
    pub reserved: Option<u64>,
}

impl CompactionConfig {
    pub fn auto_enabled(&self) -> bool {
        self.auto.unwrap_or(true)
    }
    pub fn prune_enabled(&self) -> bool {
        self.prune.unwrap_or(false)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ToolOutputConfig {
    pub max_lines: Option<u64>,
    pub max_bytes: Option<u64>,
}

impl ToolOutputConfig {
    pub fn max_lines(&self) -> u64 {
        self.max_lines.unwrap_or(2000)
    }
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes.unwrap_or(51200)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ExperimentalConfig {
    pub primary_tools: Vec<String>,
    pub mcp_timeout: Option<u64>,
    pub batch_tool: Option<bool>,
    pub continue_loop_on_deny: Option<bool>,
    pub disable_paste_summary: Option<bool>,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

// Typed view over the merged config; unknown keys are ignored.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Info {
    pub model: Option<String>,
    pub small_model: Option<String>,
    pub default_agent: Option<String>,
    pub username: Option<String>,
    pub shell: Option<String>,
    pub share: Option<String>,
    pub snapshot: Option<bool>,
    pub subagent_depth: Option<u64>,
    pub agent: IndexMap<String, AgentConfig>,
    pub command: IndexMap<String, CommandConfig>,
    pub permission: Map<String, Value>,
    pub instructions: Vec<String>,
    pub mcp: IndexMap<String, McpConfig>,
    pub provider: IndexMap<String, ProviderConfig>,
    pub disabled_providers: Vec<String>,
    pub enabled_providers: Option<Vec<String>>,
    pub skills: SkillsConfig,
    pub compaction: CompactionConfig,
    pub tool_output: ToolOutputConfig,
    pub experimental: ExperimentalConfig,
    pub references: Option<IndexMap<String, Value>>,
    pub reference: Option<IndexMap<String, Value>>,
    // Rust build only: "agent" (default) or "opencode" (the original's tool names, descriptions and prompts).
    pub tool_profile: Option<String>,
}

impl Info {
    pub fn from_value(value: &Value) -> Result<Self, ConfigError> {
        Self::deserialize(value)
            .map_err(|e| ConfigError::Invalid { path: "config".to_string(), message: e.to_string() })
    }

    pub fn model_ref(&self) -> Option<ModelRef> {
        self.model.as_deref().map(ModelRef::parse)
    }

    pub fn small_model_ref(&self) -> Option<ModelRef> {
        self.small_model.as_deref().map(ModelRef::parse)
    }

    pub fn subagent_depth(&self) -> u64 {
        self.subagent_depth.unwrap_or(1)
    }

    // `tool_profile == "opencode"`; anything else, or nothing, is the "agent" profile.
    pub fn original_tools(&self) -> bool {
        self.tool_profile.as_deref() == Some("opencode")
    }

    // `cfg.references ?? cfg.reference`
    pub fn references(&self) -> Option<&IndexMap<String, Value>> {
        self.references.as_ref().or(self.reference.as_ref())
    }

    // Instruction entries that are URLs; the rest are path/glob lookups.
    pub fn remote_instructions(&self) -> Vec<&str> {
        self.instructions
            .iter()
            .map(String::as_str)
            .filter(|i| i.starts_with("https://") || i.starts_with("http://"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn typed_view() {
        let info = Info::from_value(&json!({
            "$schema": "x",
            "model": "anthropic/claude/x",
            "unknown_key": {"a": 1},
            "agent": {"rev": {"mode": "subagent", "temperature": 1, "steps": 4, "permission": {"bash": "ask"}, "custom": 1}},
            "command": {"t": {"template": "hi", "subtask": true}},
            "mcp": {
                "a": {"type": "local", "command": ["x"], "environment": {"K": "V"}},
                "b": {"type": "remote", "url": "u", "oauth": false, "enabled": false},
                "c": {"type": "remote", "url": "u", "oauth": {"clientId": "i", "callbackPort": 99}},
                "d": {"enabled": false}
            },
            "provider": {"p": {"options": {"baseURL": "b"}, "models": {"m": {"name": "M"}}}},
            "compaction": {"reserved": 10},
            "enabled_providers": [],
            "instructions": ["a.md", "https://x/y"],
            "experimental": {"primary_tools": ["bash"], "other": 1}
        }))
        .unwrap();
        assert_eq!(info.model_ref().unwrap(), ModelRef { provider_id: "anthropic".into(), model_id: "claude/x".into() });
        let rev = &info.agent["rev"];
        assert_eq!(rev.mode, Some(AgentMode::Subagent));
        assert_eq!(rev.temperature, Some(1.0));
        assert_eq!(rev.steps, Some(4));
        assert_eq!(info.command["t"].subtask, Some(true));
        assert!(info.mcp["a"].enabled());
        assert!(!info.mcp["b"].enabled());
        assert!(matches!(&info.mcp["b"], McpConfig::Server(McpServer::Remote { oauth: Some(McpOAuthSetting::Toggle(false)), .. })));
        assert!(matches!(
            &info.mcp["c"],
            McpConfig::Server(McpServer::Remote { oauth: Some(McpOAuthSetting::Config(o)), .. }) if o.callback_port == Some(99)
        ));
        assert_eq!(info.mcp["d"], McpConfig::Toggle { enabled: false });
        assert_eq!(info.provider["p"].options["baseURL"], "b");
        assert!(info.compaction.auto_enabled() && !info.compaction.prune_enabled());
        assert_eq!(info.compaction.reserved, Some(10));
        assert_eq!(info.enabled_providers, Some(vec![]));
        assert_eq!(info.remote_instructions(), ["https://x/y"]);
        assert_eq!(info.experimental.primary_tools, ["bash"]);
        assert_eq!(info.experimental.rest["other"], 1);
        assert_eq!(info.subagent_depth(), 1);
        assert_eq!(info.tool_output.max_bytes(), 51200);
        assert!(!info.original_tools());
        assert!(Info::from_value(&json!({"tool_profile": "opencode"})).unwrap().original_tools());
        assert!(!Info::from_value(&json!({"tool_profile": "agent"})).unwrap().original_tools());
    }

    #[test]
    fn wrong_types_are_invalid() {
        assert!(Info::from_value(&json!({"model": 5})).is_err());
        assert!(Info::from_value(&json!({})).is_ok());
    }
}
