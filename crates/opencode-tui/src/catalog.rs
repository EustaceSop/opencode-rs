use crate::state::ModelKey;
use opencode_core::agent::{self, Agents};
use opencode_core::config::{self, AgentMode, LoadOptions};
use opencode_core::flag::{self, Lookup};
use opencode_core::paths::Paths;
use opencode_core::{auth, command, skill};
use opencode_llm::provider;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub context: u64,
    pub reasoning: bool,
    // Names of the model's reasoning variants.
    pub variants: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    pub models: Vec<ModelInfo>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentInfo {
    pub name: String,
    pub description: Option<String>,
    pub subagent: bool,
    pub hidden: bool,
    pub color: Option<String>,
    pub model: Option<ModelKey>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CommandInfo {
    pub name: String,
    pub description: Option<String>,
}

// What the TUI needs from the instance: the lists the original fetches during sync bootstrap.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog {
    pub providers: Vec<ProviderInfo>,
    pub agents: Vec<AgentInfo>,
    pub commands: Vec<CommandInfo>,
    pub config_model: Option<ModelKey>,
    pub worktree: Option<PathBuf>,
    pub error: Option<String>,
    // `experimental.disable_paste_summary`
    pub disable_paste_summary: bool,
    // Configured MCP servers: (name, enabled).
    pub mcp: Vec<(String, bool)>,
}

impl Catalog {
    pub fn model(&self, key: &ModelKey) -> Option<(&ProviderInfo, &ModelInfo)> {
        let provider = self.providers.iter().find(|p| p.id == key.provider_id)?;
        Some((provider, provider.models.iter().find(|m| m.id == key.model_id)?))
    }

    pub fn is_valid(&self, key: &ModelKey) -> bool {
        self.model(key).is_some()
    }

    // Agents a user can switch to.
    pub fn primary_agents(&self) -> Vec<&AgentInfo> {
        self.agents.iter().filter(|a| !a.subagent && !a.hidden).collect()
    }

    // `Provider.sort` preference, used when nothing else selects a model.
    pub fn first_model(&self) -> Option<ModelKey> {
        const PRIORITY: [&str; 4] = ["gpt-5", "claude-sonnet-4", "big-pickle", "gemini-3-pro"];
        let provider = self.providers.first()?;
        let rank = |m: &ModelInfo| PRIORITY.iter().position(|p| m.id.contains(p)).map_or(-1, |i| i as i64);
        let best = provider
            .models
            .iter()
            .max_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.id.contains("latest").cmp(&b.id.contains("latest"))).then_with(|| a.id.cmp(&b.id)))?;
        Some(ModelKey::new(&provider.id, &best.id))
    }
}

pub fn load(paths: &Paths, directory: &Path) -> Catalog {
    load_with(paths, directory, &flag::process_env)
}

pub fn load_with(paths: &Paths, directory: &Path, env: Lookup) -> Catalog {
    let worktree = config::find_worktree(directory);
    let opts = LoadOptions { cwd: directory.to_path_buf(), worktree: worktree.clone() };
    let loaded = match config::load_with(paths, &opts, env) {
        Ok(loaded) => loaded,
        Err(error) => return Catalog { worktree, error: Some(error.to_string()), ..Default::default() },
    };
    let info = match loaded.info() {
        Ok(info) => info,
        Err(error) => return Catalog { worktree, error: Some(error.to_string()), ..Default::default() },
    };
    let root = worktree.clone().unwrap_or_else(|| PathBuf::from("/"));
    let skills = skill::Registry::load(&skill::discover(&skill::DiscoverInput {
        home: &paths.home,
        cwd: directory,
        worktree: worktree.as_deref(),
        config_dirs: &config::config_dirs_with(paths, &opts, env),
        config: &info.skills,
        disable_external: flag::truthy(env, "OPENCODE_DISABLE_EXTERNAL_SKILLS"),
        disable_claude_code: flag::truthy(env, "OPENCODE_DISABLE_CLAUDE_CODE_SKILLS"),
    }));
    let agents = Agents::build(&info, &agent::Context { paths, worktree: &root, skill_dirs: &skills.dirs, reference_dirs: &[] });
    let providers = provider::build(&provider::load_catalog(paths), &loaded.config, &auth::read_all(&paths.auth_file()));
    let listed: Vec<skill::Skill> = skills.all().into_iter().cloned().collect();
    let commands = command::registry(&info, &root, Vec::new(), &listed);
    Catalog {
        providers: providers
            .iter()
            .map(|p| ProviderInfo {
                id: p.id.clone(),
                name: p.name.clone(),
                models: p
                    .models
                    .iter()
                    .map(|m| ModelInfo {
                        id: m.id.clone(),
                        name: m.name.clone(),
                        context: m.limit_context,
                        reasoning: m.reasoning,
                        variants: m.variants.keys().cloned().collect(),
                    })
                    .collect(),
            })
            .collect(),
        agents: agents
            .list()
            .into_iter()
            .map(|a| AgentInfo {
                name: a.name.clone(),
                description: a.description.clone(),
                subagent: a.mode == AgentMode::Subagent,
                hidden: a.hidden == Some(true),
                color: a.color.clone(),
                model: a.model.as_ref().map(|m| ModelKey::new(&m.provider_id, &m.model_id)),
            })
            .collect(),
        commands: commands.values().map(|c| CommandInfo { name: c.name.clone(), description: c.description.clone() }).collect(),
        config_model: info.model_ref().map(|m| ModelKey::new(m.provider_id, m.model_id)),
        worktree,
        error: None,
        disable_paste_summary: loaded.config.pointer("/experimental/disable_paste_summary").and_then(|v| v.as_bool()).unwrap_or(false),
        mcp: loaded
            .config
            .get("mcp")
            .and_then(|v| v.as_object())
            .map(|map| map.iter().map(|(name, server)| (name.clone(), server.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true))).collect())
            .unwrap_or_default(),
    }
}

// The same lists from a server, as the original's sync bootstrap fetches them (`config.providers`, `app.agents`,
// `command.list`, `config.get`, `path.get`). A failed request leaves its list empty and reports the first error.
pub async fn remote(client: &opencode_client::Client) -> Catalog {
    let (providers, agents, commands, config, path) = tokio::join!(client.config_providers(), client.agents(), client.commands(), client.config(), client.path());
    fn ok<T>(errors: &mut Vec<String>, result: Result<T, opencode_client::Error>) -> Option<T> {
        result.map_err(|error| errors.push(error.to_string())).ok()
    }
    let mut errors = Vec::new();
    let providers = ok(&mut errors, providers).unwrap_or_default();
    let agents = ok(&mut errors, agents).unwrap_or_default();
    let commands = ok(&mut errors, commands).unwrap_or_default();
    let config: serde_json::Value = ok(&mut errors, config).unwrap_or_default();
    let worktree = ok(&mut errors, path).map(|path| path.worktree).filter(|worktree| !worktree.is_empty() && worktree != "/").map(PathBuf::from);
    Catalog {
        providers: providers
            .providers
            .into_iter()
            .map(|p| ProviderInfo {
                id: p.id,
                name: p.name,
                models: p
                    .models
                    .into_values()
                    .map(|m| ModelInfo {
                        name: if m.name.is_empty() { m.id.clone() } else { m.name },
                        id: m.id,
                        context: m.limit.map_or(0, |limit| limit.context as u64),
                        reasoning: m.capabilities.is_some_and(|c| c.reasoning),
                        variants: m.variants.map(|v| v.into_keys().collect()).unwrap_or_default(),
                    })
                    .collect(),
            })
            .collect(),
        agents: agents
            .into_iter()
            .map(|a| AgentInfo {
                subagent: a.mode == "subagent",
                hidden: a.hidden == Some(true),
                model: a.model.map(|m| ModelKey::new(m.provider_id, m.model_id)),
                name: a.name,
                description: a.description,
                color: a.color,
            })
            .collect(),
        commands: commands.into_iter().map(|c| CommandInfo { name: c.name, description: c.description }).collect(),
        config_model: config.get("model").and_then(|v| v.as_str()).and_then(ModelKey::parse),
        worktree,
        error: errors.into_iter().next(),
        disable_paste_summary: config.pointer("/experimental/disable_paste_summary").and_then(|v| v.as_bool()).unwrap_or(false),
        mcp: config
            .get("mcp")
            .and_then(|v| v.as_object())
            .map(|map| map.iter().map(|(name, server)| (name.clone(), server.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true))).collect())
            .unwrap_or_default(),
    }
}

// Test fixture shared by the reducer and view tests.
#[cfg(test)]
pub fn fixture() -> Catalog {
    let model = |id: &str, name: &str| ModelInfo { id: id.into(), name: name.into(), context: 200_000, reasoning: false, variants: Vec::new() };
    let agent = |name: &str, subagent: bool| AgentInfo { name: name.into(), description: None, subagent, hidden: false, color: None, model: None };
    Catalog {
        providers: vec![
            ProviderInfo { id: "anthropic".into(), name: "Anthropic".into(), models: vec![model("claude-sonnet-4-5", "Claude Sonnet 4.5"), model("claude-haiku-4-5", "Claude Haiku 4.5")] },
            ProviderInfo { id: "openai".into(), name: "OpenAI".into(), models: vec![model("gpt-5", "GPT-5")] },
        ],
        agents: vec![agent("build", false), agent("plan", false), agent("general", true)],
        commands: vec![CommandInfo { name: "init".into(), description: Some("create/update AGENTS.md".into()) }],
        config_model: None,
        worktree: None,
        error: None,
        disable_paste_summary: false,
        mcp: Vec::new(),
    }
}
