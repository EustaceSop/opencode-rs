// `opencode debug ...` (`opencode/src/cli/cmd/debug/*`).
use crate::ui::fail;
use anyhow::Result;
use base64::Engine as _;
use opencode_core::agent::Agents;
use opencode_core::{config, flag, paths::Paths, permission, skill};
use opencode_lsp::lsp::DiagnosticsMode;
use opencode_session::Engine;
use opencode_tools::ripgrep;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn pretty(value: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn cwd() -> Result<PathBuf> {
    Ok(std::env::current_dir()?)
}

fn secret_name(key: &str) -> bool {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let pattern = PATTERN.get_or_init(|| regex::Regex::new(r"(?i)(?:api.?key|secret|password|token$|authorization$|cookie$|credential|private.?key)").expect("static pattern"));
    pattern.is_match(key)
}

// `redactConfig`: secrets by key name, everything under `headers`, and URLs carrying credentials.
pub fn redact(value: &Value, headers: bool) -> Value {
    match value {
        Value::Array(items) => items.iter().map(|item| redact(item, headers)).collect(),
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, item) in map {
                let hidden = match item {
                    Value::String(_) if headers || secret_name(key) => true,
                    Value::String(text) if text.to_lowercase().starts_with("http://") || text.to_lowercase().starts_with("https://") => match reqwest::Url::parse(text) {
                        Ok(url) => !url.username().is_empty() || url.password().is_some() || url.query_pairs().any(|(name, _)| secret_name(&name)),
                        Err(_) => true,
                    },
                    _ => false,
                };
                let next = if hidden { "***".into() } else { redact(item, headers || key.to_lowercase() == "headers") };
                out.insert(key.clone(), next);
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

fn load(paths: &Paths) -> Result<config::Loaded> {
    let cwd = cwd()?;
    let opts = config::LoadOptions { worktree: config::find_worktree(&cwd), cwd };
    Ok(config::load_with(paths, &opts, &flag::process_env)?)
}

pub fn show_config(paths: &Paths) -> Result<i32> {
    pretty(&redact(&load(paths)?.config, false))?;
    Ok(0)
}

pub fn paths(paths: &Paths) -> Result<i32> {
    for (name, value) in [
        ("home", &paths.home),
        ("data", &paths.data),
        ("bin", &paths.bin()),
        ("log", &paths.log()),
        ("cache", &paths.cache),
        ("config", &paths.config),
        ("state", &paths.state),
        ("tmp", &paths.tmp),
    ] {
        println!("{name:<10} {}", value.display());
    }
    Ok(0)
}

fn capture(program: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program).args(args).output().ok()?;
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string()).filter(|text| !text.is_empty())
}

// `os.type() os.release() os.arch()` in Node's vocabulary.
fn os_line() -> String {
    let kind = match std::env::consts::OS {
        "windows" => "Windows_NT",
        "macos" => "Darwin",
        "linux" => "Linux",
        other => other,
    };
    let release = if cfg!(windows) {
        capture("cmd", &["/c", "ver"]).and_then(|text| {
            let version = text.split("Version ").nth(1)?.trim_end_matches(']').to_string();
            Some(version.split('.').take(3).collect::<Vec<_>>().join("."))
        })
    } else {
        capture("uname", &["-r"])
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    };
    format!("{kind} {} {arch}", release.unwrap_or_else(|| "unknown".into()))
}

pub fn info(paths: &Paths, version: &str) -> Result<i32> {
    let loaded = load(paths)?;
    let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    let program = var("TERM_PROGRAM").map(|program| match var("TERM_PROGRAM_VERSION") {
        Some(version) => format!("{program} {version}"),
        None => program,
    });
    let terminal: Vec<String> = [program, var("TERM")].into_iter().flatten().collect();
    println!("opencode version: {version}");
    println!("os: {}", os_line());
    println!("terminal: {}", if terminal.is_empty() { "unknown".to_string() } else { terminal.join(" / ") });
    println!("plugins:");
    if flag::truthy(&flag::process_env, "OPENCODE_PURE") {
        println!("external plugins disabled (--pure)");
        return Ok(0);
    }
    let plugins: Vec<&str> = loaded.config.get("plugin").and_then(Value::as_array).into_iter().flatten().filter_map(config::plugin::specifier).collect();
    if plugins.is_empty() {
        println!("none");
    }
    for plugin in plugins {
        println!("- {plugin}");
    }
    Ok(0)
}

pub fn skills(paths: &Paths) -> Result<i32> {
    let cwd = cwd()?;
    let worktree = config::find_worktree(&cwd);
    let opts = config::LoadOptions { worktree: worktree.clone(), cwd: cwd.clone() };
    let info = config::load_with(paths, &opts, &flag::process_env)?.info()?;
    let flags = flag::Flags::from_env().map_err(|e| fail(e.to_string()))?;
    let registry = skill::Registry::load(&skill::discover(&skill::DiscoverInput {
        home: &paths.home,
        cwd: &cwd,
        worktree: worktree.as_deref(),
        config_dirs: &config::config_dirs(paths, &opts),
        config: &info.skills,
        disable_external: flags.disable_external_skills,
        disable_claude_code: flags.disable_claude_code_skills,
    }));
    pretty(&registry.all())?;
    Ok(0)
}

pub fn agent(paths: &Paths, name: &str, tool: Option<&str>) -> Result<i32> {
    let cwd = cwd()?;
    let worktree = config::find_worktree(&cwd);
    let opts = config::LoadOptions { worktree: worktree.clone(), cwd };
    let info = config::load_with(paths, &opts, &flag::process_env)?.info()?;
    let root = worktree.unwrap_or_else(|| PathBuf::from("/"));
    let agents = Agents::build(&info, &opencode_core::agent::Context { paths, worktree: &root, skill_dirs: &[], reference_dirs: &[] });
    let Some(agent) = agents.get(name) else {
        eprintln!("Agent {name} not found, run 'opencode agent list' to get an agent list");
        return Err(fail(""));
    };
    if tool.is_some() {
        return Err(crate::unimplemented("debug agent --tool"));
    }
    // Only the built-in tools: MCP, plugin and task tools need a running session.
    let tools = opencode_tools::builtin();
    let ids: Vec<&str> = tools.iter().map(|tool| tool.id()).collect();
    let disabled = permission::disabled(ids.iter().copied(), &agent.permission);
    let resolved: Map<String, Value> = ids.iter().map(|id| (id.to_string(), Value::Bool(!disabled.contains(*id)))).collect();
    let mut output = serde_json::to_value(agent)?;
    output["tools"] = Value::Object(resolved);
    pretty(&output)?;
    Ok(0)
}

fn rg() -> Result<PathBuf> {
    ripgrep::find_rg().ok_or_else(|| fail("ripgrep (rg) not found"))
}

pub async fn rg_files(glob: Option<String>, limit: Option<usize>) -> Result<i32> {
    let files = ripgrep::glob(&rg()?, &cwd()?, glob.as_deref().unwrap_or("**/*"), limit.unwrap_or(10_000), &CancellationToken::new()).await.map_err(|e| fail(e.to_string()))?;
    println!("{}", files.join("\n"));
    Ok(0)
}

pub async fn rg_search(pattern: &str, globs: &[String], limit: Option<usize>) -> Result<i32> {
    let include = globs.first().map(String::as_str);
    let found = ripgrep::grep(&rg()?, &cwd()?, pattern, include, limit.unwrap_or(10_000), &CancellationToken::new()).await.map_err(|e| fail(e.to_string()))?;
    let rows: Vec<Value> = found.iter().map(|m| json!({"path": m.path, "line": m.line, "text": m.text})).collect();
    pretty(&rows)?;
    Ok(0)
}

// The file API of an in-process server on a loopback port.
async fn api(paths: &Paths, route: &str, query: &[(&str, &str)]) -> Result<Value> {
    let engine = Arc::new(Engine::new(paths.clone())?);
    let server = opencode_server::serve(opencode_server::ServerOptions::default(), engine).await?;
    let directory = cwd()?;
    let mut all: Vec<(&str, String)> = query.iter().map(|(key, value)| (*key, value.to_string())).collect();
    all.push(("directory", directory.to_string_lossy().into_owned()));
    let url = reqwest::Url::parse_with_params(&format!("http://127.0.0.1:{}{route}", server.addr.port()), &all)?;
    let response = reqwest::Client::builder().no_proxy().build()?.get(url).send().await?;
    let status = response.status();
    let text = response.text().await?;
    server.shutdown().await;
    if !status.is_success() {
        return Err(fail(text));
    }
    Ok(serde_json::from_str(&text)?)
}

pub async fn file_search(paths: &Paths, query: &str) -> Result<i32> {
    let found = api(paths, "/find/file", &[("query", query)]).await?;
    let lines: Vec<&str> = found.as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    println!("{}", lines.join("\n"));
    Ok(0)
}

pub async fn file_list(paths: &Paths, path: &str) -> Result<i32> {
    pretty(&api(paths, "/file", &[("path", path)]).await?)?;
    Ok(0)
}

pub async fn file_read(paths: &Paths, path: &str) -> Result<i32> {
    let bytes = std::fs::read(cwd()?.join(path)).map_err(|e| fail(e.to_string()))?;
    let shown = api(paths, "/file/content", &[("path", path)]).await?;
    let mime = shown.get("mimeType").and_then(Value::as_str).unwrap_or("text/plain");
    pretty(&json!({"content": base64::engine::general_purpose::STANDARD.encode(&bytes), "encoding": "base64", "mime": mime}))?;
    Ok(0)
}

pub fn scrap(paths: &Paths) -> Result<i32> {
    pretty(&Engine::new(paths.clone())?.store.list_projects()?)?;
    Ok(0)
}

pub enum Snapshot {
    Track,
    Patch(String),
    Diff(String),
}

pub async fn snapshot(paths: &Paths, command: Snapshot) -> Result<i32> {
    let engine = Engine::new(paths.clone())?;
    let workspace = engine.workspace(&cwd()?)?;
    let snapshot = workspace.snapshot();
    match command {
        Snapshot::Track => println!("{}", snapshot.track().await.unwrap_or_else(|| "undefined".into())),
        Snapshot::Patch(hash) => {
            let patch = snapshot.patch(&hash).await;
            println!("{}", serde_json::to_string_pretty(&json!({"hash": patch.hash, "files": patch.files}))?);
        }
        Snapshot::Diff(hash) => println!("{}", snapshot.diff(&hash).await),
    }
    engine.shutdown().await;
    Ok(0)
}

pub enum Lsp {
    Diagnostics(String),
    Symbols(String),
    DocumentSymbols(String),
}

pub async fn lsp(paths: &Paths, command: Lsp) -> Result<i32> {
    let engine = Engine::new(paths.clone())?;
    let directory = cwd()?;
    let workspace = engine.workspace(&directory)?;
    let lsp = workspace.lsp();
    match command {
        Lsp::Diagnostics(file) => {
            let file = if Path::new(&file).is_absolute() { PathBuf::from(&file) } else { directory.join(&file) };
            lsp.touch_file(&file, Some(DiagnosticsMode::Full)).await;
            pretty(&lsp.diagnostics())?;
        }
        Lsp::Symbols(query) => pretty(&lsp.workspace_symbol(&query).await)?,
        Lsp::DocumentSymbols(uri) => pretty(&lsp.document_symbol(&uri).await)?,
    }
    engine.shutdown().await;
    Ok(0)
}

pub async fn wait() -> Result<i32> {
    tokio::time::sleep(std::time::Duration::from_secs(24 * 60 * 60)).await;
    Ok(0)
}

// Milliseconds since process start, like `performance.now()`.
pub fn startup(started: std::time::Instant) -> Result<i32> {
    println!("{}", started.elapsed().as_secs_f64() * 1000.0);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_redacted_by_name_position_and_url() {
        let config = json!({
            "provider": {"p": {"options": {"apiKey": "sk", "api_key": "sk", "baseURL": "https://api.test/v1", "timeout": 5, "headers": {"X-Any": "v", "nested": {"deep": "v"}}}}},
            "mcp": {"a": {"url": "https://u:p@host.test/mcp"}, "b": {"url": "https://host.test/mcp?access_token=1"}, "c": {"url": "https://host.test/mcp?page=1"}},
            "accessToken": "t", "tokenizer": "keep", "Authorization": "Bearer x", "password_hint": "x", "privateKey": "k", "list": [{"client_secret": "s"}, "plain"],
        });
        let expected = json!({
            "provider": {"p": {"options": {"apiKey": "***", "api_key": "***", "baseURL": "https://api.test/v1", "timeout": 5, "headers": {"X-Any": "***", "nested": {"deep": "***"}}}}},
            "mcp": {"a": {"url": "***"}, "b": {"url": "***"}, "c": {"url": "https://host.test/mcp?page=1"}},
            "accessToken": "***", "tokenizer": "keep", "Authorization": "***", "password_hint": "***", "privateKey": "***", "list": [{"client_secret": "***"}, "plain"],
        });
        assert_eq!(redact(&config, false), expected);
    }
}
