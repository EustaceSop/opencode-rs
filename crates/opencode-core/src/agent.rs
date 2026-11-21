use crate::config::{AgentMode, Info as Config, ModelRef, merge_deep};
use crate::paths::Paths;
use crate::permission::{self, Action, Rule, Ruleset};
use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::path::{Component, Path, PathBuf};

pub const PROMPT_GENERATE: &str = include_str!("../../../../opencode-ts/packages/opencode/src/agent/generate.txt");
pub const PROMPT_COMPACTION: &str =
    include_str!("../../../../opencode-ts/packages/opencode/src/agent/prompt/compaction.txt");
pub const PROMPT_EXPLORE: &str = include_str!("../../../../opencode-ts/packages/opencode/src/agent/prompt/explore.txt");
pub const PROMPT_SUMMARY: &str = include_str!("../../../../opencode-ts/packages/opencode/src/agent/prompt/summary.txt");
pub const PROMPT_TITLE: &str = include_str!("../../../../opencode-ts/packages/opencode/src/agent/prompt/title.txt");

const DESCRIPTION_BUILD: &str = "The default agent. Executes tools based on configured permissions.";
const DESCRIPTION_PLAN: &str = "Plan mode. Disallows all edit tools.";
const DESCRIPTION_GENERAL: &str = "General-purpose agent for researching complex questions and executing multi-step tasks. Use this agent to execute multiple units of work in parallel.";
const DESCRIPTION_EXPLORE: &str = r#"Fast agent specialized for exploring codebases. Use this when you need to quickly find files by patterns (eg. "src/components/**/*.tsx"), search code for keywords (eg. "API endpoints"), or answer questions about the codebase (eg. "how do API endpoints work?"). When calling this agent, specify the desired thoroughness level: "quick" for basic searches, "medium" for moderate exploration, or "very thorough" for comprehensive analysis across multiple locations and naming conventions."#;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Agent {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub mode: AgentMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    #[serde(rename = "topP", skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    pub permission: Ruleset,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    pub options: Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AgentError {
    #[error("default agent \"{0}\" not found")]
    NotFound(String),
    #[error("default agent \"{0}\" is a subagent")]
    Subagent(String),
    #[error("default agent \"{0}\" is hidden")]
    Hidden(String),
    #[error("no primary visible agent found")]
    NoPrimary,
}

pub struct Context<'a> {
    pub paths: &'a Paths,
    // Instance worktree; "/" for non-VCS projects in the original.
    pub worktree: &'a Path,
    pub skill_dirs: &'a [PathBuf],
    pub reference_dirs: &'a [PathBuf],
}

fn glob_in(dir: &Path, leaf: &str) -> String {
    dir.join(leaf).to_string_lossy().into_owned()
}

// Node `path.relative` for two absolute paths; a different root yields `to` unchanged.
pub fn relative(from: &Path, to: &Path) -> String {
    let same = |a: &Component, b: &Component| {
        let (a, b) = (a.as_os_str().to_string_lossy(), b.as_os_str().to_string_lossy());
        if cfg!(windows) { a.eq_ignore_ascii_case(&b) } else { a == b }
    };
    let is_root = |c: &Component| matches!(c, Component::Prefix(_) | Component::RootDir);
    let from: Vec<Component> = from.components().collect();
    let target: Vec<Component> = to.components().collect();
    let roots = |list: &[Component]| list.iter().take_while(|c| is_root(c)).count();
    let (from_root, to_root) = (roots(&from), roots(&target));
    if from_root != to_root || !from[..from_root].iter().zip(&target).all(|(a, b)| same(a, b)) {
        return to.to_string_lossy().into_owned();
    }
    let common = from.iter().zip(&target).take_while(|(a, b)| same(a, b)).count();
    let mut out = PathBuf::new();
    for _ in common..from.len() {
        out.push("..");
    }
    for part in &target[common..] {
        out.push(part.as_os_str());
    }
    out.to_string_lossy().into_owned()
}

fn rules(config: Value, home: &Path) -> Ruleset {
    config.as_object().map(|map| permission::from_config(map, home)).unwrap_or_default()
}

fn pattern_map(first: Option<(&str, &str)>, allow: &[String]) -> Value {
    let mut map = Map::new();
    if let Some((pattern, action)) = first {
        map.insert(pattern.to_string(), action.into());
    }
    for pattern in allow {
        map.insert(pattern.clone(), "allow".into());
    }
    Value::Object(map)
}

#[derive(Debug, Clone)]
pub struct Agents {
    agents: IndexMap<String, Agent>,
    default_agent: Option<String>,
}

impl Agents {
    pub fn build(cfg: &Config, ctx: &Context) -> Self {
        let home = &ctx.paths.home;
        let truncate_glob = glob_in(&ctx.paths.data.join("tool-output"), "*");
        let mut whitelist = vec![truncate_glob.clone(), glob_in(&ctx.paths.tmp, "*")];
        whitelist.extend(ctx.skill_dirs.iter().chain(ctx.reference_dirs).map(|dir| glob_in(dir, "*")));
        let external = pattern_map(Some(("*", "ask")), &whitelist);

        let defaults = rules(
            json!({
                "*": "allow",
                "doom_loop": "ask",
                "external_directory": external,
                "question": "deny",
                "plan_enter": "deny",
                "plan_exit": "deny",
                "read": {"*": "allow", "*.env": "ask", "*.env.*": "ask", "*.env.example": "allow"},
            }),
            home,
        );
        let user = permission::from_config(&cfg.permission, home);
        let with = |extra: Value| permission::merge(&[&defaults, &rules(extra, home), &user]);
        let native = |name: &str, mode: AgentMode, extra: Value| Agent {
            name: name.to_string(),
            description: None,
            mode,
            native: Some(true),
            hidden: None,
            top_p: None,
            temperature: None,
            color: None,
            permission: with(extra),
            model: None,
            variant: None,
            prompt: None,
            options: Map::new(),
            steps: None,
        };

        let plans = ctx.paths.data.join("plans");
        let mut plan_edit = Map::new();
        plan_edit.insert("*".into(), "deny".into());
        plan_edit.insert(glob_in(&Path::new(".opencode").join("plans"), "*.md"), "allow".into());
        plan_edit.insert(relative(ctx.worktree, &plans.join("*.md")), "allow".into());
        let deny_all = || json!({"*": "deny"});

        let mut agents: IndexMap<String, Agent> = IndexMap::new();
        let mut add = |agent: Agent| {
            agents.insert(agent.name.clone(), agent);
        };
        add(Agent {
            description: Some(DESCRIPTION_BUILD.into()),
            ..native("build", AgentMode::Primary, json!({"question": "allow", "plan_enter": "allow"}))
        });
        add(Agent {
            description: Some(DESCRIPTION_PLAN.into()),
            ..native(
                "plan",
                AgentMode::Primary,
                json!({
                    "question": "allow",
                    "plan_exit": "allow",
                    "task": {"general": "deny"},
                    "external_directory": pattern_map(None, &[glob_in(&plans, "*")]),
                    "edit": plan_edit,
                }),
            )
        });
        add(Agent {
            description: Some(DESCRIPTION_GENERAL.into()),
            ..native("general", AgentMode::Subagent, json!({"todowrite": "deny"}))
        });
        add(Agent {
            description: Some(DESCRIPTION_EXPLORE.into()),
            prompt: Some(PROMPT_EXPLORE.into()),
            ..native(
                "explore",
                AgentMode::Subagent,
                json!({
                    "*": "deny",
                    "grep": "allow",
                    "glob": "allow",
                    "list": "allow",
                    "bash": "allow",
                    "webfetch": "allow",
                    "websearch": "allow",
                    "read": "allow",
                    "external_directory": external,
                }),
            )
        });
        add(Agent {
            hidden: Some(true),
            prompt: Some(PROMPT_COMPACTION.into()),
            ..native("compaction", AgentMode::Primary, deny_all())
        });
        add(Agent {
            hidden: Some(true),
            temperature: Some(0.5),
            prompt: Some(PROMPT_TITLE.into()),
            ..native("title", AgentMode::Primary, deny_all())
        });
        add(Agent {
            hidden: Some(true),
            prompt: Some(PROMPT_SUMMARY.into()),
            ..native("summary", AgentMode::Primary, deny_all())
        });

        for (key, value) in &cfg.agent {
            if value.disable == Some(true) {
                agents.shift_remove(key);
                continue;
            }
            let item = agents.entry(key.clone()).or_insert_with(|| Agent {
                name: key.clone(),
                description: None,
                mode: AgentMode::All,
                native: Some(false),
                hidden: None,
                top_p: None,
                temperature: None,
                color: None,
                permission: permission::merge(&[&defaults, &user]),
                model: None,
                variant: None,
                prompt: None,
                options: Map::new(),
                steps: None,
            });
            if let Some(model) = value.model.as_deref().filter(|m| !m.is_empty()) {
                item.model = Some(ModelRef::parse(model));
            }
            item.variant = value.variant.clone().or(item.variant.take());
            item.prompt = value.prompt.clone().or(item.prompt.take());
            item.description = value.description.clone().or(item.description.take());
            item.temperature = value.temperature.or(item.temperature);
            item.top_p = value.top_p.or(item.top_p);
            item.mode = value.mode.unwrap_or(item.mode);
            item.color = value.color.clone().or(item.color.take());
            item.hidden = value.hidden.or(item.hidden);
            if let Some(name) = &value.name {
                item.name = name.clone();
            }
            item.steps = value.steps.or(item.steps);
            let mut options = Value::Object(std::mem::take(&mut item.options));
            merge_deep(&mut options, Value::Object(value.options.clone()));
            if let Value::Object(options) = options {
                item.options = options;
            }
            item.permission.extend(permission::from_config(&value.permission, home));
        }

        // The truncation dir stays readable unless an agent explicitly denies exactly that glob.
        for agent in agents.values_mut() {
            let explicit = agent.permission.iter().any(|r| {
                r.permission == "external_directory" && r.action == Action::Deny && r.pattern == truncate_glob
            });
            if !explicit {
                agent.permission.push(Rule::new("external_directory", truncate_glob.clone(), Action::Allow));
            }
        }

        Self { agents, default_agent: cfg.default_agent.clone().filter(|name| !name.is_empty()) }
    }

    pub fn get(&self, name: &str) -> Option<&Agent> {
        self.agents.get(name)
    }

    // Insertion order (built-ins first, then config order).
    pub fn iter(&self) -> impl Iterator<Item = &Agent> {
        self.agents.values()
    }

    // Default agent first, then by name.
    pub fn list(&self) -> Vec<&Agent> {
        let default = self.default_agent.as_deref().unwrap_or("build");
        let mut list: Vec<&Agent> = self.agents.values().collect();
        list.sort_by(|a, b| (b.name == default).cmp(&(a.name == default)).then_with(|| a.name.cmp(&b.name)));
        list
    }

    pub fn default_info(&self) -> Result<&Agent, AgentError> {
        if let Some(name) = &self.default_agent {
            let agent = self.agents.get(name).ok_or_else(|| AgentError::NotFound(name.clone()))?;
            if agent.mode == AgentMode::Subagent {
                return Err(AgentError::Subagent(name.clone()));
            }
            if agent.hidden == Some(true) {
                return Err(AgentError::Hidden(name.clone()));
            }
            return Ok(agent);
        }
        self.agents
            .values()
            .find(|a| a.mode != AgentMode::Subagent && a.hidden != Some(true))
            .ok_or(AgentError::NoPrimary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::evaluate;

    fn paths() -> Paths {
        let root = if cfg!(windows) { PathBuf::from("C:\\t") } else { PathBuf::from("/t") };
        Paths {
            home: root.join("home"),
            data: root.join("data"),
            cache: root.join("cache"),
            config: root.join("config"),
            state: root.join("state"),
            tmp: root.join("tmp"),
        }
    }

    fn build(config: Value) -> (Agents, Paths) {
        let paths = paths();
        let cfg = Config::from_value(&config).unwrap();
        let worktree = paths.home.join("repo");
        let skills = [paths.home.join("skills").join("x")];
        let ctx = Context { paths: &paths, worktree: &worktree, skill_dirs: &skills, reference_dirs: &[] };
        (Agents::build(&cfg, &ctx), paths)
    }

    fn action(agent: &Agent, permission: &str, pattern: &str) -> Action {
        evaluate(permission, pattern, &[&agent.permission]).action
    }

    #[test]
    fn prompts_embedded() {
        for prompt in [PROMPT_GENERATE, PROMPT_COMPACTION, PROMPT_EXPLORE, PROMPT_SUMMARY, PROMPT_TITLE] {
            assert!(!prompt.trim().is_empty());
        }
    }

    #[test]
    fn builtins() {
        let (agents, paths) = build(json!({}));
        let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["build", "plan", "general", "explore", "compaction", "title", "summary"]);
        assert!(agents.iter().all(|a| a.native == Some(true)));

        let build = agents.get("build").unwrap();
        assert_eq!(build.mode, AgentMode::Primary);
        assert_eq!(action(build, "bash", "ls"), Action::Allow);
        assert_eq!(action(build, "question", "*"), Action::Allow);
        assert_eq!(action(build, "plan_enter", "*"), Action::Allow);
        assert_eq!(action(build, "plan_exit", "*"), Action::Deny);
        assert_eq!(action(build, "doom_loop", "*"), Action::Ask);
        assert_eq!(action(build, "read", "src/.env"), Action::Ask);
        assert_eq!(action(build, "read", "a/.env.local"), Action::Ask);
        assert_eq!(action(build, "read", "a/.env.example"), Action::Allow);
        assert_eq!(action(build, "external_directory", "/somewhere/else"), Action::Ask);
        let out = paths.data.join("tool-output").join("tool_1").to_string_lossy().into_owned();
        assert_eq!(action(build, "external_directory", &out), Action::Allow);
        let tmp = paths.tmp.join("f").to_string_lossy().into_owned();
        assert_eq!(action(build, "external_directory", &tmp), Action::Allow);
        let skill = paths.home.join("skills").join("x").join("SKILL.md").to_string_lossy().into_owned();
        assert_eq!(action(build, "external_directory", &skill), Action::Allow);
        assert_eq!(build.permission.last().unwrap().pattern, glob_in(&paths.data.join("tool-output"), "*"));

        let plan = agents.get("plan").unwrap();
        assert_eq!(action(plan, "edit", "src/main.rs"), Action::Deny);
        assert_eq!(action(plan, "edit", ".opencode/plans/x.md"), Action::Allow);
        assert_eq!(action(plan, "task", "general"), Action::Deny);
        assert_eq!(action(plan, "task", "explore"), Action::Allow);
        assert_eq!(action(plan, "plan_exit", "*"), Action::Allow);
        let plan_file = paths.data.join("plans").join("p.md").to_string_lossy().into_owned();
        assert_eq!(action(plan, "external_directory", &plan_file), Action::Allow);
        let rel = relative(&paths.home.join("repo"), &paths.data.join("plans").join("p.md"));
        assert_eq!(action(plan, "edit", &rel), Action::Allow);

        let general = agents.get("general").unwrap();
        assert_eq!(general.mode, AgentMode::Subagent);
        assert_eq!(action(general, "todowrite", "*"), Action::Deny);

        let explore = agents.get("explore").unwrap();
        assert_eq!(explore.prompt.as_deref(), Some(PROMPT_EXPLORE));
        assert_eq!(action(explore, "edit", "x"), Action::Deny);
        assert_eq!(action(explore, "grep", "x"), Action::Allow);
        assert_eq!(action(explore, "read", "x/.env"), Action::Allow);
        assert_eq!(action(explore, "external_directory", "/x"), Action::Ask);

        for name in ["compaction", "title", "summary"] {
            let agent = agents.get(name).unwrap();
            assert_eq!(agent.hidden, Some(true));
            assert_eq!(action(agent, "bash", "ls"), Action::Deny);
            assert_eq!(action(agent, "external_directory", &out), Action::Allow);
        }
        assert_eq!(agents.get("title").unwrap().temperature, Some(0.5));
        assert_eq!(agents.default_info().unwrap().name, "build");
        assert_eq!(agents.list()[0].name, "build");
        assert_eq!(agents.list()[1].name, "compaction");
    }

    #[test]
    fn config_overrides_and_custom_agents() {
        let (agents, paths) = build(json!({
            "permission": {"bash": {"*": "ask", "git *": "allow"}, "edit": "deny"},
            "default_agent": "rev",
            "agent": {
                "build": {"model": "anthropic/claude/x", "temperature": 0.3, "options": {"a": {"b": 1}}, "permission": {"edit": "allow"}},
                "general": {"disable": true},
                "rev": {
                    "description": "Reviewer", "mode": "primary", "prompt": "review", "top_p": 0.9, "steps": 3,
                    "color": "#aabbcc", "variant": "high", "options": {"k": true},
                    "permission": {"external_directory": {(glob_in(&paths().data.join("tool-output"), "*")): "deny"}}
                },
                "explore": {"hidden": true, "name": "scout", "options": {"z": 1}}
            }
        }));
        let build = agents.get("build").unwrap();
        assert_eq!(build.model, Some(ModelRef { provider_id: "anthropic".into(), model_id: "claude/x".into() }));
        assert_eq!(build.temperature, Some(0.3));
        assert_eq!(Value::Object(build.options.clone()), json!({"a": {"b": 1}}));
        assert_eq!(action(build, "bash", "rm x"), Action::Ask);
        assert_eq!(action(build, "bash", "git status"), Action::Allow);
        assert_eq!(action(build, "edit", "x"), Action::Allow);
        assert_eq!(action(agents.get("plan").unwrap(), "edit", ".opencode/plans/x.md"), Action::Deny);
        assert!(agents.get("general").is_none());

        let rev = agents.get("rev").unwrap();
        assert_eq!(rev.native, Some(false));
        assert_eq!(rev.mode, AgentMode::Primary);
        assert_eq!((rev.top_p, rev.steps), (Some(0.9), Some(3)));
        assert_eq!(rev.variant.as_deref(), Some("high"));
        assert_eq!(action(rev, "edit", "x"), Action::Deny);
        let out = paths.data.join("tool-output").join("t").to_string_lossy().into_owned();
        assert_eq!(action(rev, "external_directory", &out), Action::Deny);
        assert_eq!(rev.permission.last().unwrap().action, Action::Deny);

        let explore = agents.get("explore").unwrap();
        assert_eq!(explore.name, "scout");
        assert_eq!(explore.prompt.as_deref(), Some(PROMPT_EXPLORE));
        assert_eq!(agents.default_info().unwrap().name, "rev");
        assert_eq!(agents.list()[0].name, "rev");
    }

    #[test]
    fn default_agent_errors() {
        let err = |config: Value| build(config).0.default_info().unwrap_err().to_string();
        assert_eq!(err(json!({"default_agent": "nope"})), "default agent \"nope\" not found");
        assert_eq!(err(json!({"default_agent": "explore"})), "default agent \"explore\" is a subagent");
        assert_eq!(err(json!({"default_agent": "title"})), "default agent \"title\" is hidden");
        assert_eq!(
            err(json!({"agent": {"build": {"disable": true}, "plan": {"mode": "subagent"}}})),
            "no primary visible agent found"
        );
        let (agents, _) = build(json!({"agent": {"build": {"disable": true}}}));
        assert_eq!(agents.default_info().unwrap().name, "plan");
        let (agents, _) = build(json!({"agent": {"build": {"disable": true}, "plan": {"disable": true}, "mine": {}}}));
        assert_eq!(agents.default_info().unwrap().name, "mine");
        assert_eq!(agents.get("mine").unwrap().mode, AgentMode::All);
    }

    #[test]
    fn relative_paths() {
        let p = |s: &str| PathBuf::from(if cfg!(windows) { format!("C:{}", s.replace('/', "\\")) } else { s.to_string() });
        let norm = |s: String| s.replace('\\', "/");
        assert_eq!(norm(relative(&p("/a/b"), &p("/a/b/c/d"))), "c/d");
        assert_eq!(norm(relative(&p("/a/b/c"), &p("/a/x/y"))), "../../x/y");
        assert_eq!(relative(&p("/a"), &p("/a")), "");
        assert_eq!(norm(relative(&p("/"), &p("/data/plans/*.md"))), "data/plans/*.md");
        if cfg!(windows) {
            assert_eq!(relative(Path::new("D:\\x"), Path::new("C:\\y\\z")), "C:\\y\\z");
        }
    }
}
