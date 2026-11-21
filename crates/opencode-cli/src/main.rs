mod agent;
mod console;
mod data;
mod db;
mod debug;
mod import;
mod log;
mod mcp;
mod mini;
mod plugin;
mod pr;
mod prompt;
mod providers;
mod remote;
mod run;
mod serve;
mod session;
mod setup;
mod stats;
mod ui;
mod uninstall;

use anyhow::Result;
use clap::error::ErrorKind;
use clap::{Args, CommandFactory, Parser, Subcommand};
use opencode_core::{auth, config, paths::Paths};
use opencode_llm::{catalog, provider};
use serde_json::{Value, json};
use serve::NetworkArgs;
use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use ui::{CliError, NORMAL, SUCCESS_BOLD, fail};

// `InstallationVersion`: a release build sets OPENCODE_VERSION at compile time.
pub const VERSION: &str = match option_env!("OPENCODE_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

pub fn unimplemented(what: &str) -> anyhow::Error {
    fail(format!("`opencode {what}` is not implemented in the Rust build yet"))
}

#[derive(Parser)]
#[command(name = "opencode", bin_name = "opencode", disable_version_flag = true, disable_help_subcommand = true, max_term_width = 100)]
#[command(args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    global: Global,
    #[command(flatten)]
    tui: TuiArgs,
}

#[derive(Args)]
struct Global {
    /// show version number
    #[arg(short = 'v', long, global = true)]
    version: bool,
    /// print logs to stderr
    #[arg(long = "print-logs", global = true)]
    print_logs: bool,
    /// log level
    #[arg(long = "log-level", global = true, value_parser = ["DEBUG", "INFO", "WARN", "ERROR"])]
    log_level: Option<String>,
    /// run without external plugins
    #[arg(long, global = true)]
    pure: bool,
}

// The default command (`$0 [project]`): start opencode tui.
#[derive(Args)]
struct TuiArgs {
    /// path to start opencode in
    project: Option<PathBuf>,
    #[command(flatten)]
    network: NetworkArgs,
    /// model to use in the format of provider/model
    #[arg(short, long)]
    model: Option<String>,
    /// continue the last session
    #[arg(short = 'c', long = "continue")]
    resume: bool,
    /// session id to continue
    #[arg(short, long)]
    session: Option<String>,
    /// fork the session when continuing (use with --continue or --session)
    #[arg(long)]
    fork: bool,
    /// prompt to use
    #[arg(long)]
    prompt: Option<String>,
    /// agent to use
    #[arg(long)]
    agent: Option<String>,
    /// auto-approve permissions that are not explicitly denied (dangerous!)
    #[arg(long, alias = "yolo", alias = "dangerously-skip-permissions")]
    auto: bool,
    /// start the minimal interactive interface
    #[arg(long)]
    mini: bool,
    #[arg(long, hide = true)]
    replay: bool,
    /// disable mini session history replay on resume and after resize
    #[arg(long = "no-replay")]
    no_replay: bool,
    /// cap visible mini replay to the newest N messages
    #[arg(long = "replay-limit")]
    replay_limit: Option<f64>,
    #[arg(long, hide = true)]
    demo: bool,
}

// Declared in the original's registration order, which is its help order.
#[derive(Subcommand)]
enum Command {
    /// generate shell completion script
    Completion { shell: Option<clap_complete::Shell> },
    /// start ACP (Agent Client Protocol) server
    Acp {
        #[command(flatten)]
        network: NetworkArgs,
        /// working directory
        #[arg(long)]
        cwd: Option<String>,
    },
    /// manage MCP (Model Context Protocol) servers
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    /// attach to a running opencode server
    Attach {
        /// http://localhost:4096
        url: String,
        /// directory to run in
        #[arg(long)]
        dir: Option<String>,
        /// continue the last session
        #[arg(short = 'c', long = "continue")]
        resume: bool,
        /// session id to continue
        #[arg(short, long)]
        session: Option<String>,
        /// fork the session when continuing (use with --continue or --session)
        #[arg(long)]
        fork: bool,
        /// basic auth password (defaults to OPENCODE_SERVER_PASSWORD)
        #[arg(short, long)]
        password: Option<String>,
        /// basic auth username (defaults to OPENCODE_SERVER_USERNAME or 'opencode')
        #[arg(short, long)]
        username: Option<String>,
        /// start the minimal interactive interface
        #[arg(long)]
        mini: bool,
        #[arg(long, hide = true)]
        replay: bool,
        /// disable mini session history replay on resume and after resize
        #[arg(long = "no-replay")]
        no_replay: bool,
        /// cap visible mini replay to the newest N messages
        #[arg(long = "replay-limit")]
        replay_limit: Option<f64>,
    },
    /// run opencode with a message
    Run(Box<RunArgs>),
    #[command(hide = true)]
    Generate,
    /// debugging and troubleshooting tools
    Debug {
        #[command(subcommand)]
        command: DebugCommand,
    },
    #[command(hide = true)]
    Console {
        #[command(subcommand)]
        command: ConsoleCommand,
    },
    /// manage AI providers and credentials
    #[command(alias = "auth")]
    Providers {
        #[command(subcommand)]
        command: ProvidersCommand,
    },
    /// manage agents
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// upgrade opencode to the latest or a specific version
    Upgrade {
        /// version to upgrade to, for ex '0.1.48' or 'v0.1.48'
        target: Option<String>,
        /// installation method to use
        #[arg(short, long, value_parser = ["curl", "npm", "pnpm", "bun", "brew", "choco", "scoop"])]
        method: Option<String>,
        /// upgrade to OpenCode 2 and remove OpenCode 1
        #[arg(long)]
        major: bool,
    },
    /// uninstall opencode and remove all related files
    Uninstall {
        /// keep configuration files
        #[arg(short = 'c', long = "keep-config")]
        keep_config: bool,
        /// keep session data and snapshots
        #[arg(short = 'd', long = "keep-data")]
        keep_data: bool,
        /// show what would be removed without removing
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// skip confirmation prompts
        #[arg(short, long)]
        force: bool,
    },
    /// starts a headless opencode server
    Serve {
        #[command(flatten)]
        network: NetworkArgs,
    },
    /// start opencode server and open web interface
    Web {
        #[command(flatten)]
        network: NetworkArgs,
    },
    /// list all available models
    Models {
        /// provider ID to filter models by
        provider: Option<String>,
        /// use more verbose model output (includes metadata like costs)
        #[arg(long)]
        verbose: bool,
        /// refresh the models cache from models.dev
        #[arg(long)]
        refresh: bool,
    },
    /// show token usage and cost statistics
    Stats {
        /// show stats for the last N days (default: all time)
        #[arg(long)]
        days: Option<f64>,
        /// number of tools to show (default: all)
        #[arg(long)]
        tools: Option<usize>,
        /// show model statistics (default: hidden). Pass a number to show top N, otherwise shows all
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        models: Option<String>,
        /// filter by project (default: all projects, empty string: current project)
        #[arg(long)]
        project: Option<String>,
    },
    /// export session data as JSON
    Export {
        /// session id to export
        #[arg(value_name = "sessionID")]
        session_id: Option<String>,
        /// redact sensitive transcript and file data
        #[arg(long)]
        sanitize: bool,
    },
    /// import session data from JSON file, or data from other AI agents
    Import(Box<ImportArgs>),
    /// manage GitHub agent
    Github {
        #[command(subcommand)]
        command: GithubCommand,
    },
    /// fetch and checkout a GitHub PR branch, then run opencode
    Pr {
        /// PR number to checkout
        number: u64,
    },
    /// manage sessions
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// install plugin and update config
    #[command(alias = "plug")]
    Plugin {
        /// npm module name
        module: String,
        /// install in global config
        #[arg(short, long)]
        global: bool,
        /// replace existing plugin version
        #[arg(short, long)]
        force: bool,
    },
    /// database tools
    #[command(args_conflicts_with_subcommands = true)]
    Db {
        #[command(subcommand)]
        command: Option<DbCommand>,
        /// SQL query to execute
        query: Option<String>,
        /// Output format
        #[arg(long, default_value = "tsv", value_parser = ["json", "tsv"])]
        format: String,
    },
    /// set up optional components (Rust build)
    Setup {
        #[command(subcommand)]
        command: SetupCommand,
    },
}

#[derive(Subcommand)]
enum SetupCommand {
    /// download a portable bash (Git for Windows MinGit) for running commands on Windows
    Bash {
        /// download without asking
        #[arg(short, long, conflicts_with = "decline")]
        yes: bool,
        /// do not offer the download again
        #[arg(long)]
        decline: bool,
    },
}

// A struct of its own keeps these arguments out of the stack frame that builds every inline subcommand.
#[derive(Args)]
struct ImportArgs {
    /// path to JSON file (omit it to import from other AI agents)
    file: Option<String>,
    /// list what other AI agents have, with counts
    #[arg(long, conflicts_with = "file")]
    list: bool,
    /// show what would be imported without writing anything
    #[arg(long = "dry-run", conflicts_with = "file")]
    dry_run: bool,
    /// import everything selected by --source/--category without asking
    #[arg(short, long, conflicts_with = "file")]
    yes: bool,
    /// only this source (repeatable): opencode, claude-code, codex, cursor, windsurf, mcp-json, grok-build, deepseek-harness
    #[arg(long = "source", value_name = "ID", conflicts_with = "file")]
    source: Vec<String>,
    /// only this category (repeatable)
    #[arg(long = "category", value_parser = ["mcp", "agents", "skills", "sessions"], conflicts_with = "file")]
    category: Vec<String>,
}

#[derive(Args)]
struct RunArgs {
    /// message to send
    message: Vec<String>,
    /// the command to run, use message for args
    #[arg(long)]
    command: Option<String>,
    /// continue the last session
    #[arg(short = 'c', long = "continue")]
    resume: bool,
    /// session id to continue
    #[arg(short, long)]
    session: Option<String>,
    /// fork the session before continuing (requires --continue or --session)
    #[arg(long)]
    fork: bool,
    #[arg(long, hide = true)]
    share: bool,
    /// model to use in the format of provider/model
    #[arg(short, long)]
    model: Option<String>,
    /// agent to use
    #[arg(long)]
    agent: Option<String>,
    /// format: default (formatted) or json (raw JSON events)
    #[arg(long, default_value = "default", value_parser = ["default", "json"])]
    format: String,
    /// file(s) to attach to message
    #[arg(short, long)]
    file: Vec<String>,
    /// title for the session (uses truncated prompt if no value provided)
    #[arg(long, num_args = 0..=1, default_missing_value = "")]
    title: Option<String>,
    /// attach to a running opencode server (e.g., http://localhost:4096)
    #[arg(long)]
    attach: Option<String>,
    /// basic auth password (defaults to OPENCODE_SERVER_PASSWORD)
    #[arg(short, long)]
    password: Option<String>,
    /// basic auth username (defaults to OPENCODE_SERVER_USERNAME or 'opencode')
    #[arg(short, long)]
    username: Option<String>,
    /// directory to run in, path on remote server if attaching
    #[arg(long)]
    dir: Option<PathBuf>,
    /// port for the local server (defaults to random port if no value provided)
    #[arg(long)]
    port: Option<f64>,
    /// model variant (provider-specific reasoning effort, e.g., high, max, minimal)
    #[arg(long)]
    variant: Option<String>,
    /// show thinking blocks
    #[arg(long)]
    thinking: bool,
    #[arg(long, hide = true)]
    mini: bool,
    #[arg(long, hide = true)]
    replay: bool,
    #[arg(long = "no-replay", hide = true)]
    no_replay: bool,
    #[arg(long = "replay-limit", hide = true)]
    replay_limit: Option<f64>,
    /// run in direct interactive split-footer mode
    #[arg(short, long)]
    interactive: bool,
    /// auto-approve permissions that are not explicitly denied (dangerous!)
    #[arg(long, alias = "yolo", alias = "dangerously-skip-permissions")]
    auto: bool,
    #[arg(long, hide = true)]
    demo: bool,
}

#[derive(Subcommand)]
enum SessionCommand {
    /// list sessions
    List {
        /// limit to N most recent sessions
        #[arg(short = 'n', long = "max-count")]
        max_count: Option<usize>,
        /// output format
        #[arg(long, default_value = "table", value_parser = ["table", "json"])]
        format: String,
    },
    /// delete a session
    Delete {
        /// session ID to delete
        #[arg(value_name = "sessionID")]
        session_id: String,
    },
}

#[derive(Subcommand)]
enum AgentCommand {
    /// create a new agent
    Create {
        /// directory path to generate the agent file
        #[arg(long)]
        path: Option<String>,
        /// what the agent should do
        #[arg(long)]
        description: Option<String>,
        /// agent mode
        #[arg(long, value_parser = ["all", "primary", "subagent"])]
        mode: Option<String>,
        /// comma-separated list of permissions to allow (default: all). Available: "bash, read, edit, glob, grep, webfetch, task, todowrite, websearch, lsp, skill"
        #[arg(long, alias = "tools")]
        permissions: Option<String>,
        /// model to use in the format of provider/model
        #[arg(short, long)]
        model: Option<String>,
    },
    /// list all available agents
    List,
}

#[derive(Subcommand)]
enum McpCommand {
    /// add an MCP server
    Add {
        /// name of the MCP server
        name: Option<String>,
        /// URL for a remote MCP server
        #[arg(long)]
        url: Option<String>,
        /// environment variable for a local MCP server (KEY=VALUE)
        #[arg(long)]
        env: Vec<String>,
        /// HTTP header for a remote MCP server (KEY=VALUE)
        #[arg(long)]
        header: Vec<String>,
        /// local server command, after `--`
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// list MCP servers and their status
    #[command(alias = "ls")]
    List,
    /// authenticate with an OAuth-enabled MCP server (`auth list` shows their status)
    Auth { name: Option<String> },
    /// remove OAuth credentials for an MCP server
    Logout { name: Option<String> },
    /// debug OAuth connection for an MCP server
    Debug {
        /// name of the MCP server
        name: String,
    },
}

#[derive(Subcommand)]
enum DbCommand {
    /// print the database path
    Path,
}

#[derive(Subcommand)]
enum ProvidersCommand {
    /// list providers and credentials
    #[command(alias = "ls")]
    List,
    /// log in to a provider
    Login {
        /// opencode auth provider
        url: Option<String>,
        /// provider id or name to log in to (skips provider selection)
        #[arg(short, long)]
        provider: Option<String>,
        /// login method label (skips method selection)
        #[arg(short, long)]
        method: Option<String>,
    },
    /// log out from a configured provider
    Logout {
        /// provider id or name to log out from
        provider: Option<String>,
    },
}

#[derive(Subcommand)]
enum GithubCommand {
    /// install the GitHub agent
    Install,
    /// run the GitHub agent
    Run {
        /// GitHub mock event to run the agent for
        #[arg(long)]
        event: Option<String>,
        /// GitHub personal access token (github_pat_********)
        #[arg(long)]
        token: Option<String>,
    },
}

#[derive(Subcommand)]
enum ConsoleCommand {
    /// log in to console
    Login { url: Option<String> },
    /// log out from console
    Logout { email: Option<String> },
    /// switch active org
    Switch,
    /// list orgs
    Orgs,
    /// open active console account
    Open,
}

#[derive(Subcommand)]
enum DebugCommand {
    /// show resolved configuration
    Config,
    /// LSP debugging utilities
    Lsp {
        #[command(subcommand)]
        command: DebugLsp,
    },
    /// ripgrep debugging utilities
    Rg {
        #[command(subcommand)]
        command: DebugRg,
    },
    /// file system debugging utilities
    File {
        #[command(subcommand)]
        command: DebugFile,
    },
    /// list all known projects
    Scrap,
    /// list all available skills
    Skill,
    /// snapshot debugging utilities
    Snapshot {
        #[command(subcommand)]
        command: DebugSnapshot,
    },
    /// print startup timing
    Startup,
    /// show agent configuration details
    Agent {
        /// Agent name
        name: String,
        /// Tool id to execute
        #[arg(long)]
        tool: Option<String>,
        /// Tool params as JSON or a JS object literal
        #[arg(long)]
        params: Option<String>,
    },
    /// debug v2 catalog and built-in plugins
    V2,
    /// show debug information
    Info,
    /// show global paths (data, config, cache, state)
    Paths,
    /// wait indefinitely (for debugging)
    Wait,
}

#[derive(Subcommand)]
enum DebugLsp {
    /// get diagnostics for a file
    Diagnostics { file: String },
    /// search workspace symbols
    Symbols { query: String },
    /// get symbols from a document
    DocumentSymbols { uri: String },
}

#[derive(Subcommand)]
enum DebugRg {
    /// list files using ripgrep
    Files {
        /// Filter files by query
        #[arg(long)]
        query: Option<String>,
        /// Glob pattern to match files
        #[arg(long)]
        glob: Option<String>,
        /// Limit number of results
        #[arg(long)]
        limit: Option<usize>,
    },
    /// search file contents using ripgrep
    Search {
        /// Search pattern
        pattern: String,
        /// File glob patterns
        #[arg(long)]
        glob: Vec<String>,
        /// Limit number of results
        #[arg(long)]
        limit: Option<usize>,
    },
}

#[derive(Subcommand)]
enum DebugFile {
    /// read file contents as JSON
    Read {
        /// File path to read
        path: String,
    },
    /// list files in a directory
    List {
        /// File path to list
        path: String,
    },
    /// search files by query
    Search {
        /// Search query
        query: String,
    },
}

#[derive(Subcommand)]
enum DebugSnapshot {
    /// track current snapshot state
    Track,
    /// show patch for a snapshot hash
    Patch { hash: String },
    /// show diff for a snapshot hash
    Diff { hash: String },
}

fn load_config(paths: &Paths) -> Result<Value> {
    let cwd = std::env::current_dir()?;
    let opts = config::LoadOptions { worktree: config::find_worktree(&cwd), cwd };
    Ok(config::load(paths, &opts)?)
}

fn load_providers(paths: &Paths, config: &Value) -> Vec<provider::Provider> {
    provider::build(&provider::load_catalog(paths), config, &auth::read_all(&paths.auth_file()))
}

// The agents of a directory, as `agent list` shows them.
fn agents(paths: &Paths, directory: &std::path::Path) -> Result<opencode_core::agent::Agents> {
    Ok(agent::build(paths, directory)?.0)
}

pub fn locale_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| b.cmp(a))
}

async fn models(paths: &Paths, only: Option<String>, verbose: bool, refresh: bool) -> Result<i32> {
    if refresh {
        let _ = catalog::refresh(&reqwest::Client::new(), &catalog::Source::from_env(paths), true).await;
        ui::println(&format!("{SUCCESS_BOLD}Models cache refreshed{NORMAL}"));
    }
    let config = load_config(paths)?;
    let catalog = provider::load_catalog(paths);
    let mut providers = load_providers(paths, &config);
    if let Some(want) = &only {
        if !providers.iter().any(|p| &p.id == want) {
            return Err(fail(format!("Provider not found: {want}")));
        }
        providers.retain(|p| &p.id == want);
    }
    // Providers whose id starts with "opencode" come first.
    providers.sort_by(|a, b| b.id.starts_with("opencode").cmp(&a.id.starts_with("opencode")).then_with(|| locale_cmp(&a.id, &b.id)));
    for p in &providers {
        let mut objects = opencode_server::model_objects(p, &config, &catalog);
        let mut ids: Vec<&str> = p.models.iter().map(|m| m.id.as_str()).collect();
        ids.sort_by(|a, b| locale_cmp(a, b));
        for id in ids {
            println!("{}/{id}", p.id);
            if verbose && let Some(model) = objects.remove(id) {
                println!("{}", serde_json::to_string_pretty(&model)?);
            }
        }
    }
    Ok(0)
}

// `opencode generate`: the OpenAPI document with an SDK sample per operation.
fn generate() -> Result<i32> {
    let mut document: Value = serde_json::from_str(opencode_server::openapi())?;
    for item in document.get_mut("paths").and_then(Value::as_object_mut).into_iter().flatten().map(|(_, item)| item) {
        for method in ["get", "post", "put", "delete", "patch"] {
            let Some(operation) = item.get_mut(method).filter(|operation| operation.get("operationId").is_some_and(Value::is_string)) else { continue };
            let id = operation["operationId"].as_str().unwrap_or_default().to_string();
            let source = format!("import {{ createOpencodeClient }} from \"@opencode-ai/sdk\n\nconst client = createOpencodeClient()\nawait client.{id}({{\n  ...\n}})");
            operation["x-codeSamples"] = json!([{"lang": "js", "source": source}]);
        }
    }
    println!("{}", serde_json::to_string_pretty(&document)?);
    Ok(0)
}

async fn execute(paths: Paths, command: Command, started: std::time::Instant) -> Result<i32> {
    match command {
        Command::Completion { shell } => {
            let shell = shell.unwrap_or_else(|| match std::env::var("SHELL").unwrap_or_default().rsplit('/').next() {
                Some("zsh") => clap_complete::Shell::Zsh,
                Some("fish") => clap_complete::Shell::Fish,
                _ => clap_complete::Shell::Bash,
            });
            clap_complete::generate(shell, &mut Cli::command(), "opencode", &mut std::io::stdout());
            Ok(0)
        }
        Command::Acp { network: _, cwd } => {
            let options = opencode_acp::AcpOptions { cwd: cwd.map(PathBuf::from), version: VERSION.to_string() };
            opencode_acp::run(options, std::sync::Arc::new(opencode_session::Engine::new(paths.clone())?)).await.map(|()| 0)
        }
        Command::Attach { url, dir, resume, session, fork, password, username, mini, replay, no_replay, replay_limit } => {
            remote::attach(&paths, remote::AttachArgs { url, dir, resume, session, fork, password, username, mini, replay, no_replay, replay_limit }).await
        }
        Command::Upgrade { .. } => Err(unimplemented("upgrade")),
        Command::Github { command: GithubCommand::Install } => Err(unimplemented("github install")),
        Command::Github { command: GithubCommand::Run { .. } } => Err(unimplemented("github run")),
        Command::Console { command } => match command {
            ConsoleCommand::Login { url } => console::login(&paths, url).await,
            ConsoleCommand::Logout { email } => console::logout(&paths, email).await,
            ConsoleCommand::Switch => console::switch(&paths).await,
            ConsoleCommand::Orgs => console::orgs(&paths).await,
            ConsoleCommand::Open => console::open(&paths).await,
        },
        Command::Generate => generate(),
        Command::Providers { command: ProvidersCommand::List } => providers::list(&paths),
        Command::Providers { command: ProvidersCommand::Login { url, provider, method } } => providers::login(&paths, url, provider, method).await,
        Command::Providers { command: ProvidersCommand::Logout { provider } } => providers::logout(&paths, provider),
        Command::Models { provider, verbose, refresh } => models(&paths, provider, verbose, refresh).await,
        Command::Run(args) => {
            // `--mini` is only reachable as `opencode --mini`; `--interactive` is declared but unused, as in the original.
            if args.mini && args.command.is_some() {
                return Err(fail("--mini cannot be used with --command"));
            }
            if args.mini {
                return Err(fail("--mini must be used without the run subcommand"));
            }
            if args.demo {
                return Err(fail("--demo requires --mini"));
            }
            if args.replay_limit.is_some() {
                return Err(fail("--replay-limit requires --mini"));
            }
            if args.share && args.attach.is_none() {
                ui::println(&format!("{}!  Session sharing has been discontinued", ui::DANGER_BOLD));
            }
            import::hint(&paths);
            setup::hint(&paths);
            let args = *args;
            run::run(
                &paths,
                run::RunArgs {
                    message: args.message,
                    command: args.command,
                    model: args.model,
                    agent: args.agent,
                    session: args.session,
                    resume: args.resume,
                    fork: args.fork,
                    format: args.format,
                    file: args.file,
                    title: args.title,
                    variant: args.variant,
                    thinking: args.thinking,
                    auto: args.auto,
                    dir: args.dir,
                    share: args.share,
                    attach: args.attach,
                    password: args.password,
                    username: args.username,
                },
            )
            .await
        }
        Command::Serve { network } => serve::serve(&paths, &network).await,
        Command::Web { network } => serve::web(&paths, &network).await,
        Command::Session { command: SessionCommand::List { max_count, format } } => session::list(&paths, max_count, &format),
        Command::Session { command: SessionCommand::Delete { session_id } } => session::delete(&paths, &session_id),
        Command::Stats { days, tools, models, project } => {
            let models = match models.as_deref() {
                None | Some("false") => None,
                Some(text) => Some(text.parse::<usize>().unwrap_or(usize::MAX)),
            };
            stats::run(&paths, days, tools, models, project)
        }
        Command::Export { session_id, sanitize } => data::export(&paths, session_id, sanitize),
        Command::Import(args) => match args.file {
            Some(file) => data::import(&paths, &file),
            None => import::run(&paths, import::Args { list: args.list, dry_run: args.dry_run, yes: args.yes, sources: args.source, categories: args.category }),
        },
        Command::Agent { command: AgentCommand::List } => agent::list(&paths),
        Command::Agent { command: AgentCommand::Create { path, description, mode, permissions, model } } => agent::create(&paths, agent::CreateArgs { path, description, mode, permissions, model }).await,
        Command::Mcp { command } => {
            match command {
                McpCommand::List => mcp::list(&paths).await?,
                McpCommand::Add { name, url, env, header, command } => mcp::add(&paths, name, url, env, header, command)?,
                // `mcp auth list` / `mcp auth ls` is a subcommand in the original; a server of that name needs no auth listing.
                McpCommand::Auth { name } if matches!(name.as_deref(), Some("list" | "ls")) => mcp::auth_list(&paths).await?,
                McpCommand::Auth { name } => mcp::auth(&paths, name).await?,
                McpCommand::Logout { name } => mcp::logout(&paths, name).await?,
                McpCommand::Debug { name } => mcp::debug(&paths, &name, VERSION).await?,
            }
            Ok(0)
        }
        Command::Pr { number } => pr::run(number),
        Command::Plugin { module, global, force } => plugin::run(&paths, &module, global, force).await,
        Command::Uninstall { keep_config, keep_data, dry_run, force } => uninstall::run(&paths, uninstall::Args { keep_config, keep_data, dry_run, force }),
        Command::Db { command: Some(DbCommand::Path), .. } => {
            println!("{}", db::path(&paths).display());
            Ok(0)
        }
        Command::Db { command: None, query: Some(query), format } => db::query(&paths, &query, &format),
        Command::Db { command: None, query: None, .. } => db::shell(&paths),
        Command::Setup { command: SetupCommand::Bash { yes, decline } } => setup::bash(&paths, setup::Args { yes, decline }).await,
        Command::Debug { command } => match command {
            DebugCommand::Config => debug::show_config(&paths),
            DebugCommand::Paths => debug::paths(&paths),
            DebugCommand::Info => debug::info(&paths, VERSION),
            DebugCommand::Skill => debug::skills(&paths),
            DebugCommand::Scrap => debug::scrap(&paths),
            DebugCommand::Startup => debug::startup(started),
            DebugCommand::Wait => debug::wait().await,
            DebugCommand::V2 => Err(unimplemented("debug v2")),
            DebugCommand::Agent { name, tool, params: _ } => debug::agent(&paths, &name, tool.as_deref()),
            DebugCommand::Rg { command: DebugRg::Files { query: _, glob, limit } } => debug::rg_files(glob, limit).await,
            DebugCommand::Rg { command: DebugRg::Search { pattern, glob, limit } } => debug::rg_search(&pattern, &glob, limit).await,
            DebugCommand::File { command: DebugFile::Search { query } } => debug::file_search(&paths, &query).await,
            DebugCommand::File { command: DebugFile::List { path } } => debug::file_list(&paths, &path).await,
            DebugCommand::File { command: DebugFile::Read { path } } => debug::file_read(&paths, &path).await,
            DebugCommand::Snapshot { command } => {
                let command = match command {
                    DebugSnapshot::Track => debug::Snapshot::Track,
                    DebugSnapshot::Patch { hash } => debug::Snapshot::Patch(hash),
                    DebugSnapshot::Diff { hash } => debug::Snapshot::Diff(hash),
                };
                debug::snapshot(&paths, command).await
            }
            DebugCommand::Lsp { command } => {
                let command = match command {
                    DebugLsp::Diagnostics { file } => debug::Lsp::Diagnostics(file),
                    DebugLsp::Symbols { query } => debug::Lsp::Symbols(query),
                    DebugLsp::DocumentSymbols { uri } => debug::Lsp::DocumentSymbols(uri),
                };
                debug::lsp(&paths, command).await
            }
        },
    }
}

// `resolveThreadDirectory`: a relative project is resolved from $PWD.
fn thread_directory(project: Option<PathBuf>) -> Result<PathBuf> {
    let base = std::env::var_os("PWD").map(PathBuf::from).filter(|pwd| pwd.is_absolute()).map_or_else(std::env::current_dir, Ok)?;
    Ok(match project {
        Some(project) if project.is_absolute() => project,
        Some(project) => base.join(project),
        None => std::env::current_dir()?,
    })
}

// The default command (`opencode/src/cli/cmd/tui.ts`).
async fn tui(paths: Paths, args: TuiArgs) -> Result<i32> {
    if args.replay {
        return Err(fail("--replay is not supported; replay is enabled by default"));
    }
    if args.mini {
        if let Some(network) = args.network.given() {
            return Err(fail(format!("{network} cannot be used with --mini")));
        }
        let options = mini::Options {
            directory: Some(thread_directory(args.project)?.to_string_lossy().into_owned()),
            resume: args.resume,
            session: args.session,
            fork: args.fork,
            model: args.model,
            agent: args.agent,
            prompt: args.prompt,
            replay: !args.no_replay,
            replay_limit: args.replay_limit,
            demo: args.demo,
            ..Default::default()
        };
        import::hint(&paths);
        setup::hint(&paths);
        return mini::run(&paths, options).await;
    }
    let needs_mini = [("--no-replay", args.no_replay), ("--replay-limit", args.replay_limit.is_some()), ("--demo", args.demo)];
    if let Some((flag, _)) = needs_mini.into_iter().find(|(_, set)| *set) {
        return Err(fail(format!("{flag} requires --mini")));
    }
    if args.fork && !args.resume && args.session.is_none() {
        return Err(fail("--fork requires --continue or --session"));
    }
    // A relative project is resolved from $PWD, like the original.
    let base = std::env::var_os("PWD").map(PathBuf::from).filter(|pwd| pwd.is_absolute()).map_or_else(std::env::current_dir, Ok)?;
    let directory = match args.project {
        Some(project) => {
            let target = if project.is_absolute() { project } else { base.join(project) };
            if !target.is_dir() {
                ui::error(&format!("Failed to change directory to {}", target.display()));
                return Ok(0);
            }
            std::path::absolute(&target)?
        }
        None => std::env::current_dir()?,
    };
    import::hint(&paths);
    setup::hint(&paths);
    let mut prompt = args.prompt;
    if !std::io::stdin().is_terminal() {
        let mut piped = String::new();
        let _ = std::io::stdin().read_to_string(&mut piped);
        if !piped.is_empty() {
            prompt = Some(match prompt {
                Some(value) if !value.is_empty() => format!("{piped}\n{value}"),
                _ => piped,
            });
        }
    }
    let options = opencode_tui::TuiOptions {
        directory,
        model: args.model,
        agent: args.agent,
        session: args.session,
        continue_last: args.resume,
        fork: args.fork,
        prompt,
        auto: args.auto,
    };
    // `--port`, `--hostname` or `--mdns` put a real HTTP server beside the UI, resolved from the flags alone
    // (`resolveNetworkOptionsNoConfig`); `--cors` or `--mdns-domain` by themselves do not.
    let network = serve::resolve(&args.network, &Value::Null);
    if args.network.port.is_some() || args.network.hostname.is_some() || network.mdns {
        return remote::tui_with_server(&paths, network, options).await;
    }
    if let Some(session) = &options.session {
        remote::validate_local(&paths, session)?;
    }
    opencode_tui::run(options).await?;
    Ok(0)
}

// `-v` / `--version` anywhere before `--` prints the bare version, as yargs does.
fn wants_version(args: &[String]) -> bool {
    args.iter().skip(1).take_while(|arg| *arg != "--").any(|arg| arg == "-v" || arg == "--version")
}

const LOG_LEVELS: [&str; 4] = ["DEBUG", "INFO", "WARN", "ERROR"];

// yargs takes `--print-logs`, `--log-level` and `--pure` anywhere before `--`. clap rejects default-command options in
// front of a subcommand, so these three are lifted out of the arguments first. An invalid level stays for clap to report.
fn lift_globals(args: Vec<String>) -> (Vec<String>, Global) {
    let mut global = Global { version: false, print_logs: false, log_level: None, pure: false };
    let mut rest = Vec::with_capacity(args.len());
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            rest.push(arg);
            rest.extend(iter);
            break;
        }
        let level = |value: &str| LOG_LEVELS.contains(&value).then(|| value.to_string());
        match arg.as_str() {
            "--print-logs" => global.print_logs = true,
            "--pure" => global.pure = true,
            "--log-level" => match iter.next() {
                Some(value) => match level(&value) {
                    Some(value) => global.log_level = Some(value),
                    None => rest.extend([arg, value]),
                },
                None => rest.push(arg),
            },
            other => match other.strip_prefix("--log-level=").and_then(level) {
                Some(value) => global.log_level = Some(value),
                None => rest.push(arg),
            },
        }
    }
    (rest, global)
}

fn parse() -> Cli {
    let args: Vec<String> = std::env::args().collect();
    if wants_version(&args) {
        println!("{VERSION}");
        std::process::exit(0);
    }
    let (args, global) = lift_globals(args);
    match Cli::try_parse_from(&args) {
        Ok(cli) => Cli { global, ..cli },
        Err(error) => {
            let _ = error.print();
            // yargs exits 1 for every usage error; only an explicit --help succeeds.
            std::process::exit(if error.kind() == ErrorKind::DisplayHelp { 0 } else { 1 });
        }
    }
}

fn report(error: &anyhow::Error) -> i32 {
    match error.downcast_ref::<CliError>() {
        Some(cli) => {
            if !cli.message.is_empty() {
                ui::error(&cli.message);
            }
            cli.code
        }
        None => {
            ui::error(&format!("{error:#}"));
            1
        }
    }
}

fn main() {
    let started = std::time::Instant::now();
    let cli = parse();
    // The original's yargs middleware; done before any thread exists.
    unsafe {
        if cli.global.print_logs {
            std::env::set_var("OPENCODE_PRINT_LOGS", "1");
        }
        if let Some(level) = &cli.global.log_level {
            std::env::set_var("OPENCODE_LOG_LEVEL", level);
        }
        if cli.global.pure {
            std::env::set_var("OPENCODE_PURE", "1");
        }
        std::env::set_var("AGENT", "1");
        std::env::set_var("OPENCODE", "1");
        std::env::set_var("OPENCODE_PID", std::process::id().to_string());
    }
    if cli.global.version {
        println!("{VERSION}");
        return;
    }
    let paths = Paths::from_env();
    log::init(&paths.log());
    tracing::info!(version = VERSION, "opencode");
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            ui::error(&error.to_string());
            std::process::exit(1);
        }
    };
    let result = runtime.block_on(async {
        match cli.command {
            Some(command) => execute(paths, command, started).await,
            None => tui(paths, cli.tui).await,
        }
    });
    let code = match result {
        Ok(code) => code,
        Err(error) => {
            tracing::error!(error = %format!("{error:#}"), "command failed");
            report(&error)
        }
    };
    // Like the original's `finally { process.exit() }`: child processes that ignore signals must not keep the CLI alive.
    std::process::exit(code);
}
