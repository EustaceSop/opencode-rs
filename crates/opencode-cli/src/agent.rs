// `opencode agent ...` (`opencode/src/cli/cmd/agent.ts`).
use crate::prompt::{self, Choice};
use crate::ui;
use anyhow::Result;
use opencode_core::agent::{Agents, Context, PROMPT_GENERATE};
use opencode_core::{auth, config, flag, paths::Paths};
use opencode_llm::{Event, Message, Request, client, provider};
use opencode_session::Engine;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const AVAILABLE_PERMISSIONS: [&str; 11] = ["bash", "read", "edit", "glob", "grep", "webfetch", "task", "todowrite", "websearch", "lsp", "skill"];

pub fn build(paths: &Paths, directory: &Path) -> Result<(Agents, Value, Option<PathBuf>)> {
    let worktree = config::find_worktree(directory);
    let opts = config::LoadOptions { worktree: worktree.clone(), cwd: directory.to_path_buf() };
    let loaded = config::load_with(paths, &opts, &flag::process_env)?;
    let root = worktree.clone().unwrap_or_else(|| PathBuf::from("/"));
    let agents = Agents::build(&loaded.info()?, &Context { paths, worktree: &root, skill_dirs: &[], reference_dirs: &[] });
    Ok((agents, loaded.config, worktree))
}

pub fn list(paths: &Paths) -> Result<i32> {
    let (agents, _, _) = build(paths, &std::env::current_dir()?)?;
    let mut all: Vec<_> = agents.iter().collect();
    all.sort_by(|a, b| b.native.unwrap_or(false).cmp(&a.native.unwrap_or(false)).then_with(|| crate::locale_cmp(&a.name, &b.name)));
    for agent in all {
        println!("{} ({})", agent.name, serde_json::to_value(agent.mode)?.as_str().unwrap_or_default());
        println!("  {}", serde_json::to_string_pretty(&agent.permission)?);
    }
    Ok(0)
}

pub struct Generated {
    pub identifier: String,
    pub when_to_use: String,
    pub system_prompt: String,
}

// The model's answer is expected to be one JSON object; a code fence around it is tolerated.
pub fn parse_generated(text: &str) -> Result<Generated> {
    let trimmed = text.trim();
    let body = match (trimmed.find('{'), trimmed.rfind('}')) {
        (Some(start), Some(end)) if end > start => &trimmed[start..=end],
        _ => trimmed,
    };
    let value: Value = serde_json::from_str(body).map_err(|e| anyhow::anyhow!("invalid agent JSON: {e}"))?;
    let field = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string).ok_or_else(|| anyhow::anyhow!("missing \"{key}\" in the generated agent"));
    Ok(Generated { identifier: field("identifier")?, when_to_use: field("whenToUse")?, system_prompt: field("systemPrompt")? })
}

// `Agent.generate`: one request with the generator prompt, temperature 0.3.
async fn generate(paths: &Paths, directory: &Path, config: &Value, existing: &[String], description: &str, model: Option<&str>) -> Result<Generated> {
    let (provider_id, model_id) = match model {
        Some(spec) => provider::parse_model(spec).map(|(p, m)| (p.to_string(), m.to_string())).ok_or_else(|| anyhow::anyhow!("invalid model: {spec}"))?,
        None => Engine::new(paths.clone())?.default_model(directory)?.ok_or_else(|| anyhow::anyhow!("No providers found"))?,
    };
    let providers = provider::build(&provider::load_catalog(paths), config, &auth::read_all(&paths.auth_file()));
    let target = providers.iter().find(|p| p.id == provider_id).ok_or_else(|| anyhow::anyhow!("Model not found: {provider_id}/{model_id}"))?;
    let model = target.models.iter().find(|m| m.id == model_id).ok_or_else(|| anyhow::anyhow!("Model not found: {provider_id}/{model_id}"))?;
    let user = format!(
        "Create an agent configuration based on this request: \"{description}\".\n\nIMPORTANT: The following identifiers already exist and must NOT be used: {}\n  Return ONLY the JSON object, no other text, do not wrap in backticks",
        existing.join(", ")
    );
    let request = Request { system: vec![PROMPT_GENERATE.to_string()], messages: vec![Message::User(user)], temperature: Some(0.3), ..Default::default() };
    let mut text = String::new();
    client::stream(&reqwest::Client::new(), target, model, &request, |event| {
        if let Event::TextDelta(delta) = event {
            text.push_str(&delta);
        }
    })
    .await?;
    parse_generated(&text)
}

fn yaml_scalar(text: &str) -> String {
    let plain = !text.is_empty()
        && !text.contains('\n')
        && !text.contains(": ")
        && !text.contains(" #")
        && !text.ends_with(':')
        && !text.starts_with(|c: char| c.is_whitespace() || "!&*-?[]{}|>@`\"'#%,".contains(c))
        && !text.ends_with(char::is_whitespace)
        && !matches!(text.to_lowercase().as_str(), "true" | "false" | "null" | "yes" | "no" | "on" | "off" | "~")
        && text.parse::<f64>().is_err();
    if plain { text.to_string() } else { Value::String(text.to_string()).to_string() }
}

// The file `gray-matter` writes: YAML front matter, then the prompt. Strings that are not plain scalars are double-quoted.
pub fn render(generated: &Generated, mode: &str, selected: &[String]) -> String {
    let mut out = format!("---\ndescription: {}\nmode: {mode}\n", yaml_scalar(&generated.when_to_use));
    let denied: Vec<&str> = AVAILABLE_PERMISSIONS.into_iter().filter(|permission| !selected.iter().any(|chosen| chosen == permission)).collect();
    if !denied.is_empty() {
        out.push_str("permission:\n");
        for permission in denied {
            out.push_str(&format!("  {permission}: deny\n"));
        }
    }
    out.push_str("---\n");
    out.push_str(&generated.system_prompt);
    if !generated.system_prompt.ends_with('\n') {
        out.push('\n');
    }
    out
}

pub struct CreateArgs {
    pub path: Option<String>,
    pub description: Option<String>,
    pub mode: Option<String>,
    pub permissions: Option<String>,
    pub model: Option<String>,
}

pub async fn create(paths: &Paths, args: CreateArgs) -> Result<i32> {
    let directory = std::env::current_dir()?;
    let (agents, config, worktree) = build(paths, &directory)?;
    let scripted = args.path.is_some() && args.description.is_some() && args.mode.is_some() && args.permissions.is_some();
    if !scripted {
        ui::empty();
        ui::intro("Create agent");
    }

    let target = match &args.path {
        Some(path) => Path::new(path).join("agents"),
        None => {
            let mut global = true;
            if let Some(worktree) = &worktree {
                let choices = vec![Choice::hint("Current project", worktree.to_string_lossy()), Choice::hint("Global", paths.config.to_string_lossy())];
                global = prompt::select("Location", choices, 0, 2)? == 1;
            }
            match (&worktree, global) {
                (Some(worktree), false) => worktree.join(".opencode").join("agents"),
                _ => paths.config.join("agents"),
            }
        }
    };
    let description = match args.description {
        Some(description) => description,
        None => prompt::text("Description", Some("What should this agent do?"), Some(|x| x.is_empty().then_some("Required")))?,
    };

    if !scripted {
        ui::step("Generating agent configuration...");
    }
    let existing: Vec<String> = agents.list().into_iter().map(|agent| agent.name.clone()).collect();
    let generated = match generate(paths, &directory, &config, &existing, &description, args.model.as_deref()).await {
        Ok(generated) => generated,
        Err(error) => {
            ui::done(&format!("LLM failed to generate agent: {error}"), false);
            return Err(ui::cancelled());
        }
    };
    if !scripted {
        ui::done(&format!("Agent {} generated", generated.identifier), true);
    }

    let selected: Vec<String> = match &args.permissions {
        Some(list) if list.is_empty() => AVAILABLE_PERMISSIONS.map(str::to_string).to_vec(),
        Some(list) => list.split(',').map(|item| item.trim().to_string()).collect(),
        None => prompt::multiselect("Select permissions to allow (Space to toggle)", &AVAILABLE_PERMISSIONS)?,
    };
    let mode = match args.mode {
        Some(mode) => mode,
        None => {
            let choices = vec![
                Choice::hint("All", "Can function in both primary and subagent roles"),
                Choice::hint("Primary", "Acts as a primary/main agent"),
                Choice::hint("Subagent", "Can be used as a subagent by other agents"),
            ];
            ["all", "primary", "subagent"][prompt::select("Agent mode", choices, 0, 3)?].to_string()
        }
    };

    let file = target.join(format!("{}.md", generated.identifier));
    std::fs::create_dir_all(&target)?;
    if file.exists() {
        if scripted {
            eprintln!("Error: Agent file already exists: {}", file.display());
            return Ok(1);
        }
        ui::failure(&format!("Agent file already exists: {}", file.display()));
        return Err(ui::cancelled());
    }
    std::fs::write(&file, render(&generated, &mode, &selected))?;
    if scripted {
        println!("{}", file.display());
    } else {
        ui::success(&format!("Agent created: {}", file.display()));
        ui::outro("Done");
    }
    Ok(0)
}
