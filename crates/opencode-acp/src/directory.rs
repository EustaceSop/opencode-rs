// What ACP knows about a directory: providers, modes (agents), slash commands, default model
// (`acp/directory.ts`, `loadDirectorySnapshot` in `acp/service.ts`).
use opencode_core::agent::{self, Agents};
use opencode_core::config::{self, AgentMode, LoadOptions};
use opencode_core::flag;
use opencode_session::Engine;
use std::cmp::Ordering;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRef {
    pub provider_id: String,
    pub model_id: String,
}

impl ModelRef {
    pub fn new(provider_id: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self { provider_id: provider_id.into(), model_id: model_id.into() }
    }

    pub fn key(&self) -> String {
        format!("{}/{}", self.provider_id, self.model_id)
    }
}

#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub variants: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    pub models: Vec<ModelInfo>,
}

#[derive(Debug, Clone)]
pub struct Mode {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Command {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub providers: Vec<ProviderInfo>,
    pub model_options: Vec<ModelRef>,
    pub modes: Vec<Mode>,
    pub default_mode_id: String,
    pub commands: Vec<Command>,
    pub default_model: Option<ModelRef>,
}

impl Snapshot {
    pub fn provider(&self, id: &str) -> Option<&ProviderInfo> {
        self.providers.iter().find(|p| p.id == id)
    }

    pub fn model(&self, model: &ModelRef) -> Option<&ModelInfo> {
        self.provider(&model.provider_id)?.models.iter().find(|m| m.id == model.model_id)
    }

    // `Directory.variants`: every known model has a (possibly empty) variant record.
    pub fn variants(&self, model: &ModelRef) -> Option<&[String]> {
        self.model(model).map(|m| m.variants.as_slice())
    }

    pub fn has_mode(&self, id: Option<&str>) -> bool {
        id.is_some_and(|id| self.modes.iter().any(|m| m.id == id))
    }

    pub fn default_mode(&self) -> Option<String> {
        (!self.modes.is_empty()).then(|| self.default_mode_id.clone())
    }

    // `selectDefaultModel`
    pub fn default_selection(&self) -> ModelRef {
        self.default_model.clone().or_else(|| self.model_options.first().cloned()).unwrap_or_else(|| ModelRef::new("unknown", "unknown"))
    }

    // `selectVariant`
    pub fn select_variant(&self, model: &ModelRef) -> Option<String> {
        let variants = self.variants(model)?;
        if variants.iter().any(|v| v == "default") {
            return Some("default".into());
        }
        variants.first().cloned()
    }
}

// "default" also stands for "no explicit variant".
pub fn has_variant(variants: &[String], variant: &str) -> bool {
    variant == "default" || variants.iter().any(|v| v == variant)
}

// `localeCompare` as the CLI approximates it.
pub fn locale_cmp(a: &str, b: &str) -> Ordering {
    a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| b.cmp(a))
}

// `Provider.sort`: preferred families first, "latest" ids first, then id descending.
pub fn sort_by_preference<T>(items: &mut [T], id: impl Fn(&T) -> &str) {
    const PRIORITY: [&str; 4] = ["gpt-5", "claude-sonnet-4", "big-pickle", "gemini-3-pro"];
    let rank = |id: &str| PRIORITY.iter().position(|p| id.contains(p)).map_or(-1, |i| i as i64);
    items.sort_by(|a, b| {
        let (a, b) = (id(a), id(b));
        rank(b).cmp(&rank(a)).then_with(|| (!a.contains("latest")).cmp(&!b.contains("latest"))).then_with(|| b.cmp(a))
    });
}

// `Provider.parseModel`
pub fn parse_model(spec: &str) -> ModelRef {
    match spec.split_once('/') {
        Some((provider, model)) => ModelRef::new(provider, model),
        None => ModelRef::new(spec, ""),
    }
}

fn best(models: &[ModelRef]) -> Option<ModelRef> {
    let mut sorted = models.to_vec();
    sort_by_preference(&mut sorted, |m| m.model_id.as_str());
    sorted.into_iter().next()
}

// `defaultModelFromConfig`: configured model, the opencode provider's best, the overall best, then the configured one anyway.
fn default_model(configured: Option<&str>, providers: &[ProviderInfo]) -> Option<ModelRef> {
    let configured = configured.map(parse_model);
    let known = |m: &ModelRef| providers.iter().any(|p| p.id == m.provider_id && p.models.iter().any(|x| x.id == m.model_id));
    if let Some(model) = configured.as_ref().filter(|m| known(m)) {
        return Some(model.clone());
    }
    let refs = |p: &ProviderInfo| p.models.iter().map(|m| ModelRef::new(&p.id, &m.id)).collect::<Vec<_>>();
    if let Some(model) = providers.iter().find(|p| p.id == "opencode").and_then(|p| best(&refs(p))) {
        return Some(model);
    }
    best(&providers.iter().flat_map(refs).collect::<Vec<_>>()).or(configured)
}

pub async fn load(engine: &Engine, directory: &str) -> Result<Snapshot, String> {
    let path = PathBuf::from(directory);
    let providers = engine.providers(&path).await.map_err(|e| e.to_string())?;
    let commands = engine.commands(&path).await.map_err(|e| e.to_string())?;
    let worktree = config::find_worktree(&path);
    let options = LoadOptions { cwd: path.clone(), worktree: worktree.clone() };
    let info = config::load_with(&engine.paths, &options, &flag::process_env).map_err(|e| e.to_string())?.info().map_err(|e| e.to_string())?;
    let root = worktree.unwrap_or_else(|| PathBuf::from("/"));
    let agents = Agents::build(&info, &agent::Context { paths: &engine.paths, worktree: &root, skill_dirs: &[], reference_dirs: &[] });
    let listed = agents.list();

    let providers: Vec<ProviderInfo> = providers
        .iter()
        .map(|p| ProviderInfo {
            id: p.id.clone(),
            name: p.name.clone(),
            models: p.models.iter().map(|m| ModelInfo { id: m.id.clone(), name: m.name.clone(), variants: m.variants.keys().cloned().collect() }).collect(),
        })
        .collect();
    let mut model_options: Vec<ModelRef> = providers.iter().flat_map(|p| p.models.iter().map(|m| ModelRef::new(&p.id, &m.id))).collect();
    sort_by_preference(&mut model_options, |m| m.model_id.as_str());
    let modes: Vec<Mode> = listed
        .iter()
        .filter(|a| a.mode != AgentMode::Subagent && a.hidden != Some(true))
        .map(|a| Mode { id: a.name.clone(), name: a.name.clone(), description: a.description.clone().filter(|d| !d.is_empty()) })
        .collect();
    let primary = listed.iter().find(|a| a.mode == AgentMode::Primary && a.hidden != Some(true)).map_or_else(|| "build".to_string(), |a| a.name.clone());
    let default_mode_id = if modes.iter().any(|m| m.id == primary) { primary } else { modes.first().map_or(primary, |m| m.id.clone()) };
    let mut commands: Vec<Command> = commands.into_iter().map(|c| Command { name: c.name, description: c.description.unwrap_or_default() }).collect();
    commands.sort_by(|a, b| locale_cmp(&a.name, &b.name));
    let default_model = default_model(info.model.as_deref(), &providers);
    Ok(Snapshot { providers, model_options, modes, default_mode_id, commands, default_model })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(id: &str, models: &[&str]) -> ProviderInfo {
        ProviderInfo { id: id.into(), name: id.into(), models: models.iter().map(|m| ModelInfo { id: m.to_string(), name: m.to_string(), variants: Vec::new() }).collect() }
    }

    #[test]
    fn default_model_prefers_config_then_opencode_then_best() {
        let providers = vec![provider("a", &["x-1", "claude-sonnet-4-5"]), provider("opencode", &["big", "gpt-5-latest", "gpt-5"])];
        assert_eq!(default_model(Some("a/x-1"), &providers), Some(ModelRef::new("a", "x-1")));
        assert_eq!(default_model(Some("a/missing"), &providers), Some(ModelRef::new("opencode", "gpt-5-latest")));
        assert_eq!(default_model(None, &providers[..1]), Some(ModelRef::new("a", "claude-sonnet-4-5")));
        assert_eq!(default_model(Some("z/q/r"), &[]), Some(ModelRef::new("z", "q/r")));
        assert_eq!(default_model(None, &[]), None);
    }
}
