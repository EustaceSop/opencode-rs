// `opencode run` (non-interactive), after `opencode/src/cli/cmd/run.ts`.
use crate::ui::{self, DIM, NORMAL, WARNING_BOLD};
use anyhow::Result;
use opencode_core::config::AgentMode;
use opencode_core::paths::Paths;
use opencode_session::{
    CommandInput, CreateSession, Engine, Host, PermissionAnswer, PermissionAsk, PromptInput, QuestionRequest, Reply, Update, file_url,
};
use opencode_storage::{Change, FilePart, Message, Part, PartBody, PermissionAction, PermissionRule, ToolPart, ToolState};
use opencode_tools::PermissionRequest;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

pub struct RunArgs {
    pub message: Vec<String>,
    pub command: Option<String>,
    pub model: Option<String>,
    pub agent: Option<String>,
    pub session: Option<String>,
    pub resume: bool,
    pub fork: bool,
    pub format: String,
    pub file: Vec<String>,
    pub title: Option<String>,
    pub variant: Option<String>,
    pub thinking: bool,
    pub auto: bool,
    pub dir: Option<PathBuf>,
    pub share: bool,
    // `--attach <url>` with its Basic auth credentials.
    pub attach: Option<String>,
    pub password: Option<String>,
    pub username: Option<String>,
}

pub(crate) struct Inline {
    pub(crate) icon: &'static str,
    pub(crate) title: String,
    pub(crate) description: Option<String>,
    // Block tools print their body under the title.
    pub(crate) block: bool,
    pub(crate) body: Option<String>,
}

fn inline(icon: &'static str, title: String) -> Inline {
    Inline { icon, title, description: None, block: false, body: None }
}

fn block(icon: &'static str, title: String, body: Option<String>) -> Inline {
    Inline { icon, title, description: None, block: true, body }
}

// `toolPath`: relative to the run directory when inside it, else absolute; always forward slashes.
pub(crate) fn tool_path(cwd: &Path, input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    let absolute = opencode_tools::util::resolve(cwd, input);
    let relative = opencode_tools::util::relative(cwd, &absolute);
    if relative.is_empty() {
        return ".".into();
    }
    if !relative.starts_with("..") {
        return relative.replace('\\', "/");
    }
    absolute.to_string_lossy().replace('\\', "/")
}

fn count(n: u64, label: &str) -> String {
    format!("{n} {label}{}", if n == 1 { "" } else { "es" })
}

fn titlecase(text: &str) -> String {
    let mut out = String::new();
    let mut boundary = true;
    for c in text.chars() {
        let word = c.is_alphanumeric() || c == '_';
        if word && boundary {
            out.extend(c.to_uppercase());
        } else {
            out.push(c);
        }
        boundary = !word;
    }
    out
}

// `toolInlineInfo` for the tools this port has.
pub(crate) fn tool_inline(cwd: &Path, tool: &ToolPart) -> Inline {
    let input = tool.state.input();
    let text = |key: &str| input.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
    let (output, title, metadata, status) = match &tool.state {
        ToolState::Completed(state) => (Some(state.output.clone()), state.title.clone(), Some(&state.metadata), "completed"),
        ToolState::Error(_) => (None, String::new(), None, "error"),
        ToolState::Running(state) => (None, state.title.clone().unwrap_or_default(), state.metadata.as_ref(), "running"),
        ToolState::Pending(_) => (None, String::new(), None, "pending"),
    };
    let meta = |key: &str| metadata.and_then(|m| m.get(key));
    let search = |label: &str, matches: Option<u64>| {
        let root = text("path");
        let suffix = if root.is_empty() { String::new() } else { format!("in {}", tool_path(cwd, &root)) };
        let description = match matches {
            Some(n) => format!("{suffix}{}{}", if suffix.is_empty() { "" } else { " · " }, count(n, "match")),
            None => suffix,
        };
        Inline { description: Some(description).filter(|d| !d.is_empty()), ..inline("✱", format!("{label} \"{}\"", text("pattern"))) }
    };
    match tool.tool.as_str() {
        "bash" => block("$", text("command"), output.map(|o| o.trim().to_string())),
        "glob" => search("Glob", meta("count").and_then(Value::as_u64)),
        "grep" => search("Grep", meta("matches").and_then(Value::as_u64)),
        "read" => {
            let extra: Vec<String> = input
                .iter()
                .filter(|(key, value)| *key != "filePath" && (value.is_string() || value.is_number() || value.is_boolean()))
                .map(|(key, value)| format!("{key}={}", value.as_str().map_or_else(|| value.to_string(), str::to_string)))
                .collect();
            let description = (!extra.is_empty()).then(|| format!("[{}]", extra.join(", ")));
            Inline { description, ..inline("→", format!("Read {}", tool_path(cwd, &text("filePath")))) }
        }
        "write" => block("←", format!("Write {}", tool_path(cwd, &text("filePath"))), output),
        "edit" => block("←", format!("Edit {}", tool_path(cwd, &text("filePath"))), meta("diff").and_then(Value::as_str).map(str::to_string)),
        "webfetch" => {
            let url = text("url");
            inline("%", if url.is_empty() { "WebFetch".into() } else { format!("WebFetch {url}") })
        }
        "task" => {
            let kind = titlecase(&Some(text("subagent_type")).filter(|k| !k.is_empty()).unwrap_or_else(|| "unknown".into()));
            let description = text("description");
            let icon = match status {
                "error" => "✗",
                "running" => "•",
                _ => "✓",
            };
            if description.is_empty() {
                inline(icon, format!("{kind} Task"))
            } else {
                Inline { description: Some(format!("{kind} Agent")), ..inline(icon, description) }
            }
        }
        "todowrite" => {
            let lines: Vec<String> = input
                .get("todos")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|item| {
                    let body = item.get("content").and_then(Value::as_str).filter(|body| !body.is_empty())?;
                    let mark = match item.get("status").and_then(Value::as_str) {
                        Some("completed") => "[✓]",
                        Some("in_progress") => "[•]",
                        _ => "[ ]",
                    };
                    Some(format!("{mark} {body}"))
                })
                .collect();
            block("#", "Todos".into(), Some(lines.join("\n")))
        }
        "skill" => inline("→", format!("Skill \"{}\"", text("name"))),
        "apply_patch" => match meta("files").and_then(Value::as_array).map_or(0, Vec::len) {
            0 => inline("%", "Patch".into()),
            files => inline("%", format!("Patch {files} file{}", if files == 1 { "" } else { "s" })),
        },
        "question" => {
            let total = input.get("questions").and_then(Value::as_array).map_or(0, Vec::len);
            inline("→", format!("Asked {total} question{}", if total == 1 { "" } else { "s" }))
        }
        "invalid" => block("✗", Some(title).filter(|t| !t.is_empty()).unwrap_or_else(|| "Invalid Tool".into()), output),
        name => {
            let fallback = if input.is_empty() { "Unknown".to_string() } else { Value::Object(input.clone()).to_string() };
            inline("⚙", format!("{name} {}", Some(title).filter(|t| !t.is_empty()).unwrap_or(fallback)))
        }
    }
}

fn print_inline(info: &Inline) {
    let suffix = info.description.as_ref().map_or_else(String::new, |d| format!("{DIM} {d}{NORMAL}"));
    ui::println(&format!("{NORMAL}{} {NORMAL}{}{suffix}", info.icon, info.title));
}

fn print_tool(info: &Inline) {
    if !info.block {
        return print_inline(info);
    }
    ui::empty();
    print_inline(info);
    if let Some(body) = info.body.as_ref().filter(|body| !body.trim().is_empty()) {
        ui::println(body);
        ui::empty();
    }
}

struct Output {
    session_id: String,
    cwd: PathBuf,
    json: bool,
    thinking: bool,
    auto: bool,
    started: Mutex<bool>,
    tasks: Mutex<HashSet<String>>,
    errors: Mutex<Vec<String>>,
}

impl Output {
    // One JSON line on stdout; false when the default format is in use.
    fn emit(&self, kind: &str, key: &str, data: Value) -> bool {
        if !self.json {
            return false;
        }
        let mut line = json!({"type": kind, "timestamp": opencode_storage::id::now_ms(), "sessionID": self.session_id});
        line[key] = data;
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{line}");
        true
    }

    fn message(&self, message: &Message) {
        let Message::Assistant(assistant) = message else { return };
        if assistant.session_id != self.session_id || self.json {
            return;
        }
        let mut started = self.started.lock().unwrap_or_else(|e| e.into_inner());
        if !*started {
            *started = true;
            ui::empty();
            ui::println(&format!("> {} · {}", assistant.agent, assistant.model_id));
            ui::empty();
        }
    }

    fn part(&self, part: &Part) {
        if part.session_id != self.session_id {
            return;
        }
        let value = || serde_json::to_value(part).unwrap_or_default();
        match &part.body {
            PartBody::Tool(tool) => match &tool.state {
                ToolState::Completed(_) => {
                    if !self.emit("tool_use", "part", value()) {
                        print_tool(&tool_inline(&self.cwd, tool));
                    }
                }
                ToolState::Error(state) => {
                    if !self.emit("tool_use", "part", value()) {
                        let info = tool_inline(&self.cwd, tool);
                        print_inline(&Inline { description: info.description, ..inline("✗", format!("{} failed", info.title)) });
                        ui::error(&state.error);
                    }
                }
                ToolState::Running(_)
                    if tool.tool == "task" && !self.json && self.tasks.lock().unwrap_or_else(|e| e.into_inner()).insert(part.id.clone()) =>
                {
                    print_tool(&tool_inline(&self.cwd, tool));
                }
                _ => {}
            },
            PartBody::StepStart(_) => {
                self.emit("step_start", "part", value());
            }
            PartBody::StepFinish(_) => {
                self.emit("step_finish", "part", value());
            }
            PartBody::Text(text) if text.time.is_some_and(|time| time.end.is_some()) => {
                if self.emit("text", "part", value()) {
                    return;
                }
                let text = opencode_tools::util::js_trim(&text.text);
                if text.is_empty() {
                    return;
                }
                if !std::io::stdout().is_terminal() {
                    println!("{text}");
                    return;
                }
                ui::empty();
                ui::println(text);
                ui::empty();
            }
            PartBody::Reasoning(reasoning) if reasoning.time.end.is_some() && self.thinking => {
                if self.emit("reasoning", "part", value()) {
                    return;
                }
                let text = opencode_tools::util::js_trim(&reasoning.text);
                if text.is_empty() {
                    return;
                }
                let line = format!("Thinking: {text}");
                if std::io::stdout().is_terminal() {
                    ui::empty();
                    ui::println(&format!("{DIM}\u{1b}[3m{line}\u{1b}[0m{NORMAL}"));
                    ui::empty();
                } else {
                    println!("{line}");
                }
            }
            _ => {}
        }
    }

    fn failed(&self, error: Value) {
        let message = error.pointer("/data/message").and_then(Value::as_str).or_else(|| error["name"].as_str()).unwrap_or_default().to_string();
        self.errors.lock().unwrap_or_else(|e| e.into_inner()).push(message.clone());
        if !self.emit("error", "error", error) {
            ui::error(&message);
        }
    }
}

#[async_trait::async_trait]
impl Host for Output {
    fn update(&self, update: Update) {
        if let Update::SessionError { session_id, error } = update
            && session_id == self.session_id
        {
            self.failed(serde_json::to_value(error).unwrap_or_default());
        }
    }

    async fn permission(&self, request: &PermissionRequest) -> Reply {
        if self.auto {
            return Reply::Once;
        }
        ui::println(&format!(
            "{WARNING_BOLD}! {NORMAL}permission requested: {} ({}); auto-rejecting",
            request.permission,
            request.patterns.join(", ")
        ));
        Reply::Reject
    }

    async fn ask(&self, ask: &PermissionAsk) -> PermissionAnswer {
        self.permission(&ask.request).await.into()
    }

    // The original's non-interactive run has no way to answer: its new sessions deny the question tool, and a
    // question from a resumed session would wait forever. This port dismisses it instead.
    async fn question(&self, _request: &QuestionRequest) -> Option<Vec<Vec<String>>> {
        ui::println(&format!("{WARNING_BOLD}! {NORMAL}question asked in a non-interactive run; dismissing"));
        None
    }
}

fn deny(permission: &str) -> PermissionRule {
    PermissionRule { permission: permission.into(), pattern: "*".into(), action: PermissionAction::Deny }
}

fn die(message: &str) -> Result<i32> {
    ui::error(message);
    Ok(1)
}

// `--agent` falls back to the default agent, with a warning, when it is unknown or a subagent.
fn pick_agent(paths: &Paths, directory: &Path, name: Option<String>) -> Result<Option<String>> {
    let Some(name) = name else { return Ok(None) };
    let agents = crate::agents(paths, directory)?;
    let warn = |reason: &str| ui::println(&format!("{WARNING_BOLD}! {NORMAL} agent \"{name}\" {reason}. Falling back to default agent"));
    match agents.get(&name) {
        None => warn("not found"),
        Some(agent) if agent.mode == AgentMode::Subagent => warn("is a subagent, not a primary agent"),
        Some(_) => return Ok(Some(name)),
    }
    Ok(None)
}

pub async fn run(paths: &Paths, args: RunArgs) -> Result<i32> {
    if let Some(url) = args.attach.clone() {
        return attach(args, url).await;
    }
    let directory = match &args.dir {
        Some(dir) if dir.is_dir() => std::path::absolute(dir)?,
        Some(dir) => return die(&format!("Failed to change directory to {}", dir.display())),
        None => std::env::current_dir()?,
    };

    let mut files: Vec<FilePart> = Vec::new();
    for file in &args.file {
        let resolved = opencode_tools::util::resolve(&directory, file);
        if !resolved.exists() {
            return die(&format!("File not found: {file}"));
        }
        files.push(FilePart {
            mime: if resolved.is_dir() { "application/x-directory" } else { "text/plain" }.into(),
            filename: resolved.file_name().map(|name| name.to_string_lossy().into_owned()),
            url: file_url(&resolved),
            source: None,
        });
    }

    let mut message = args.message.join(" ");
    if !std::io::stdin().is_terminal() {
        let mut piped = String::new();
        let _ = std::io::stdin().read_to_string(&mut piped);
        if !piped.is_empty() {
            message = if message.is_empty() { piped } else { format!("{message}\n{piped}") };
        }
    }
    if message.trim().is_empty() && args.command.is_none() {
        return die("You must provide a message or a command");
    }
    if args.fork && !args.resume && args.session.is_none() {
        return die("--fork requires --continue or --session");
    }

    let engine = Arc::new(Engine::new(paths.clone())?);
    // Plugins expect a server for their SDK client; one is started on demand, loopback only.
    opencode_server::attach_plugin_server(&engine);
    let base = match &args.session {
        Some(id) => match engine.store.get_session(id)? {
            Some(session) => Some(session),
            None => return die("Session not found"),
        },
        None if args.resume => {
            let project = opencode_storage::identity::resolve(&directory).id;
            engine.store.list_sessions(&project)?.into_iter().find(|session| !session.is_child())
        }
        None => None,
    };
    let session = match base {
        Some(base) if args.fork => engine.fork(&base.id, None)?,
        Some(base) => base,
        None => {
            let title = args.title.as_ref().map(|title| match title.as_str() {
                "" => {
                    let units: Vec<u16> = message.encode_utf16().collect();
                    format!("{}{}", String::from_utf16_lossy(&units[..units.len().min(50)]), if units.len() > 50 { "..." } else { "" })
                }
                given => given.to_string(),
            });
            let permission = Some(["question", "plan_enter", "plan_exit"].map(deny).to_vec());
            engine.create_session(&directory, CreateSession { title, permission, ..Default::default() })?
        }
    };
    let agent = pick_agent(paths, &directory, args.agent)?;

    let output = Arc::new(Output {
        session_id: session.id.clone(),
        cwd: directory.clone(),
        json: args.format == "json",
        thinking: args.thinking,
        auto: args.auto,
        started: Mutex::new(false),
        tasks: Mutex::default(),
        errors: Mutex::default(),
    });
    // Rows the engine stores are the original's `message.updated` / `message.part.updated` events.
    let watcher = output.clone();
    engine.store.watch(move |change| match change {
        Change::MessageUpdated(message) => watcher.message(message),
        Change::PartUpdated(part) => watcher.part(part),
        _ => {}
    });

    let abort = CancellationToken::new();
    let cancel = abort.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel.cancel();
        }
    });
    let host: Arc<dyn Host> = output.clone();
    let result = match args.command {
        Some(command) => {
            #[allow(clippy::needless_update)]
            let input = CommandInput {
                directory,
                session_id: Some(session.id.clone()),
                command,
                arguments: message,
                agent,
                model: args.model,
                variant: args.variant,
                ..Default::default()
            };
            engine.command(input, host, abort).await
        }
        None => {
            #[allow(clippy::needless_update)]
            let input = PromptInput {
                directory,
                session_id: Some(session.id.clone()),
                text: message,
                model: args.model,
                agent,
                files,
                variant: args.variant,
                ..Default::default()
            };
            engine.prompt(input, host, abort).await
        }
    };
    // Title generation runs in the background; let it finish before the process exits.
    engine.settle().await;
    engine.shutdown().await;

    let reported = !output.errors.lock().unwrap_or_else(|e| e.into_inner()).is_empty();
    if let Err(error) = result {
        // Failures inside the loop were already reported as session errors.
        if !reported && !matches!(error, opencode_session::Error::Aborted) {
            output.failed(json!({"name": "UnknownError", "data": {"message": error.to_string()}}));
        }
        return Ok(1);
    }
    Ok(i32::from(reported))
}

const ATTACH_FILE_MAX_BYTES: u64 = 10 * 1024 * 1024;

// `FSUtil.mimeType` for the files that are not text; text files are sent as text/plain.
fn binary_mime(path: &Path) -> &'static str {
    let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or_default().to_lowercase();
    match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "ico" => "image/vnd.microsoft.icon",
        "svg" => "image/svg+xml",
        "avif" => "image/avif",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "wasm" => "application/wasm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        _ => "application/octet-stream",
    }
}

// Attach mode has no shared filesystem: files travel inline as `data:` URLs, directories cannot.
fn inline_files(root: &Path, list: &[String]) -> Result<Vec<FilePart>, String> {
    use base64::Engine as _;
    let mut files = Vec::new();
    for file in list {
        let resolved = opencode_tools::util::resolve(root, file);
        if !resolved.exists() {
            return Err(format!("File not found: {file}"));
        }
        if resolved.is_dir() {
            return Err(format!("Cannot attach local directory without a shared filesystem: {file}"));
        }
        let meta = std::fs::metadata(&resolved).map_err(|error| error.to_string())?;
        if !meta.is_file() || meta.len() > ATTACH_FILE_MAX_BYTES {
            return Err(format!("Cannot attach local file larger than 10 MiB or a special file: {file}"));
        }
        let content = std::fs::read(&resolved).map_err(|error| error.to_string())?;
        let mime = if std::str::from_utf8(&content).is_ok() { "text/plain" } else { binary_mime(&resolved) };
        files.push(FilePart {
            url: format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(&content)),
            mime: mime.to_string(),
            filename: resolved.file_name().map(|name| name.to_string_lossy().into_owned()),
            source: None,
        });
    }
    Ok(files)
}

// `formatRunError`: the known error shapes in words, anything else as indented JSON.
fn format_error(body: &Value) -> String {
    let data = |name: &str| (body["name"] == name).then(|| &body["data"]).filter(|data| data.is_object());
    if let Some(model) = data("ProviderModelNotFoundError") {
        let text = |key: &str| model[key].as_str().unwrap_or("undefined").to_string();
        let mut lines = vec![format!("Model not found: {}/{}", text("providerID"), text("modelID"))];
        let suggestions: Vec<&str> = model["suggestions"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        if !suggestions.is_empty() {
            lines.push(format!("Did you mean: {}", suggestions.join(", ")));
        }
        lines.push("Try: `opencode models` to list available models".into());
        lines.push("Or check your config (opencode.json) provider/model names".into());
        return lines.join("\n");
    }
    if let Some(provider) = data("ProviderInitError") {
        return format!("Failed to initialize provider \"{}\". Check credentials and configuration.", provider["providerID"].as_str().unwrap_or("undefined"));
    }
    match body {
        Value::String(text) => text.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

// `pickAgent` against a server: its `/agent` list decides.
async fn attach_agent(client: &opencode_client::Client, url: &str, name: Option<String>) -> Option<String> {
    let name = name?;
    let warn = |reason: String| ui::println(&format!("{WARNING_BOLD}! {NORMAL} {reason}"));
    let Ok(agents) = client.agents().await else {
        warn(format!("failed to list agents from {url}. Falling back to default agent"));
        return None;
    };
    match agents.iter().find(|agent| agent.name == name) {
        None => warn(format!("agent \"{name}\" not found. Falling back to default agent")),
        Some(agent) if agent.mode == "subagent" => warn(format!("agent \"{name}\" is a subagent, not a primary agent. Falling back to default agent")),
        Some(_) => return Some(name),
    }
    None
}

// The `--format json` rules over the part as the server sent it.
fn emit_part(output: &Output, part: &Value) {
    if part["sessionID"] != output.session_id.as_str() {
        return;
    }
    let ended = part.pointer("/time/end").is_some_and(|end| !end.is_null());
    let kind = match part["type"].as_str() {
        Some("tool") if matches!(part.pointer("/state/status").and_then(Value::as_str), Some("completed" | "error")) => "tool_use",
        Some("step-start") => "step_start",
        Some("step-finish") => "step_finish",
        Some("text") if ended => "text",
        Some("reasoning") if ended && output.thinking => "reasoning",
        _ => return,
    };
    output.emit(kind, "part", part.clone());
}

// Mirrors the session's events to the terminal until it goes idle; permissions of the session tree are answered.
async fn follow(mut events: opencode_client::EventStream, client: opencode_client::Client, output: Arc<Output>) {
    use opencode_client::{Event, PermissionReply, SessionStatus};
    let mut sessions = HashSet::from([output.session_id.clone()]);
    while let Some(frame) = events.next().await {
        match frame.event {
            Event::SessionCreated(info) => {
                if info.parent_id.as_ref().is_some_and(|parent| sessions.contains(parent)) {
                    sessions.insert(info.id.clone());
                }
            }
            Event::MessageUpdated(message) => output.message(&message),
            Event::PartUpdated(part) if !output.json => output.part(&part),
            Event::SessionError { session_id: Some(session_id), error: Some(error) } if session_id == output.session_id => output.failed(error),
            Event::SessionStatus { session_id, status: SessionStatus::Idle } if session_id == output.session_id => return,
            Event::PermissionAsked(request) if sessions.contains(&request.session_id) => {
                let reply = if output.auto {
                    PermissionReply::Once
                } else {
                    ui::println(&format!("{WARNING_BOLD}! {NORMAL}permission requested: {} ({}); auto-rejecting", request.permission, request.patterns.join(", ")));
                    PermissionReply::Reject
                };
                let _ = client.reply_permission(&request.id, reply, None).await;
            }
            _ if output.json && frame.kind == "message.part.updated" => emit_part(&output, &frame.properties["part"]),
            _ => {}
        }
    }
}

// `run --attach <url>` (`run.ts` with `attachSDK`): the same flow against a server, whose paths are its own.
async fn attach(args: RunArgs, url: String) -> Result<i32> {
    use opencode_client::{Client, CommandBody, CreateSession as Create, Credentials, ModelRef, PromptBody, PromptPart, SessionQuery};
    let root = std::env::var_os("PWD").map(PathBuf::from).filter(|pwd| pwd.is_absolute()).map_or_else(std::env::current_dir, Ok)?;
    // `--dir` names a directory on the server; it is not looked up here.
    let directory = args.dir.as_ref().map(|dir| dir.to_string_lossy().into_owned());
    let credentials = Credentials::from_env(args.password.clone(), args.username.clone());
    let sdk = |dir: Option<String>| Client::new(&url).with_directory(dir).with_credentials(credentials.clone());
    let files = match inline_files(&root, &args.file) {
        Ok(files) => files,
        Err(message) => return die(&message),
    };

    let mut message = args.message.join(" ");
    if !std::io::stdin().is_terminal() {
        let mut piped = String::new();
        let _ = std::io::stdin().read_to_string(&mut piped);
        if !piped.is_empty() {
            message = if message.is_empty() { piped } else { format!("{message}\n{piped}") };
        }
    }
    if message.trim().is_empty() && args.command.is_none() {
        return die("You must provide a message or a command");
    }
    if args.fork && !args.resume && args.session.is_none() {
        return die("--fork requires --continue or --session");
    }

    let base = sdk(directory.clone());
    let session = match &args.session {
        Some(id) => match base.session(id).await {
            Ok(current) if args.fork => base.fork(&current.id, None).await.ok(),
            Ok(current) => Some(current),
            Err(_) => return die("Session not found"),
        },
        None => {
            let continued = match args.resume {
                true => base.sessions(&SessionQuery::default()).await.ok().and_then(|list| list.into_iter().find(|session| session.parent_id.is_none())),
                false => None,
            };
            match continued {
                Some(continued) if args.fork => base.fork(&continued.id, None).await.ok(),
                Some(continued) => Some(continued),
                None => {
                    let title = args.title.as_ref().map(|title| match title.as_str() {
                        "" => {
                            let units: Vec<u16> = message.encode_utf16().collect();
                            format!("{}{}", String::from_utf16_lossy(&units[..units.len().min(50)]), if units.len() > 50 { "..." } else { "" })
                        }
                        given => given.to_string(),
                    });
                    let permission = Some(["question", "plan_enter", "plan_exit"].map(deny).to_vec());
                    match base.create_session(&Create { title, permission, ..Default::default() }).await {
                        Ok(created) => Some(created),
                        Err(error @ opencode_client::Error::Transport(_)) => return die(&error.to_string()),
                        Err(_) => None,
                    }
                }
            }
        }
    };
    let Some(session) = session else { return die("Session not found") };
    // Replies from inside the loop must reach the session's own directory.
    let cwd = match directory.or_else(|| Some(session.directory.clone()).filter(|dir| !dir.is_empty())) {
        Some(cwd) => cwd,
        None => match base.path().await.map(|path| path.directory) {
            Ok(dir) if !dir.is_empty() => dir,
            _ => return die("Failed to resolve remote directory"),
        },
    };
    let client = sdk(Some(cwd.clone()));
    let agent = attach_agent(&client, &url, args.agent).await;
    if args.share {
        ui::println(&format!("{}!  Session sharing has been discontinued", ui::DANGER_BOLD));
    }

    let output = Arc::new(Output {
        session_id: session.id.clone(),
        cwd: PathBuf::from(&cwd),
        json: args.format == "json",
        thinking: args.thinking,
        auto: args.auto,
        started: Mutex::new(false),
        tasks: Mutex::default(),
        errors: Mutex::default(),
    });
    let events = match client.events().await {
        Ok(events) => events,
        Err(error) => return die(&error.to_string()),
    };
    let follower = tokio::spawn(follow(events, client.clone(), output.clone()));
    let result = match args.command {
        Some(command) => client.command(&session.id, &CommandBody { agent, model: args.model, arguments: message, command, variant: args.variant, ..Default::default() }).await.map(drop),
        None => {
            let mut parts: Vec<PromptPart> = files.into_iter().map(PromptPart::File).collect();
            parts.push(PromptPart::Text { text: message });
            let body = PromptBody { agent, model: args.model.as_deref().and_then(ModelRef::parse), variant: args.variant, parts, ..Default::default() };
            client.prompt(&session.id, &body).await.map(drop)
        }
    };
    if let Err(error) = result {
        follower.abort();
        let body = error.body().cloned().unwrap_or_else(|| json!({"name": "UnknownError", "data": {"message": error.to_string()}}));
        if !output.emit("error", "error", body.clone()) {
            ui::error(&format_error(&body));
        }
        return Ok(1);
    }
    // The answer can arrive before the stream's last events; give them a moment to print.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), follower).await;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencode_storage::{ToolStateCompleted, ToolTimeCompleted};

    #[test]
    fn attach_errors_and_files_follow_the_original() {
        let body = json!({"name": "ProviderModelNotFoundError", "data": {"providerID": "nope", "modelID": "none", "suggestions": ["mock/m1"]}});
        assert_eq!(format_error(&body), "Model not found: nope/none\nDid you mean: mock/m1\nTry: `opencode models` to list available models\nOr check your config (opencode.json) provider/model names");
        assert_eq!(format_error(&json!({"_tag": "BadRequest"})), "{\n  \"_tag\": \"BadRequest\"\n}");
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hi").unwrap();
        std::fs::write(dir.path().join("b.png"), [0x89, 0x50, 0xff, 0xfe]).unwrap();
        let files = inline_files(dir.path(), &["a.txt".into(), "b.png".into()]).unwrap();
        assert_eq!((files[0].mime.as_str(), files[0].url.as_str(), files[0].filename.as_deref()), ("text/plain", "data:text/plain;base64,aGk=", Some("a.txt")));
        assert_eq!((files[1].mime.as_str(), files[1].url.as_str()), ("image/png", "data:image/png;base64,iVD//g=="));
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        assert_eq!(inline_files(dir.path(), &["sub".into()]).unwrap_err(), "Cannot attach local directory without a shared filesystem: sub");
        assert_eq!(inline_files(dir.path(), &["gone".into()]).unwrap_err(), "File not found: gone");
    }

    fn completed(tool: &str, input: Value, output: &str, metadata: Value) -> ToolPart {
        let object = |value: Value| value.as_object().cloned().unwrap_or_default();
        ToolPart {
            call_id: "c".into(),
            tool: tool.into(),
            state: ToolState::Completed(ToolStateCompleted {
                input: object(input),
                output: output.into(),
                title: String::new(),
                metadata: object(metadata),
                time: ToolTimeCompleted { start: 1, end: 2, compacted: None },
                attachments: None,
            }),
            metadata: None,
        }
    }

    #[test]
    fn tools_render_like_the_original_table() {
        let cwd = if cfg!(windows) { PathBuf::from(r"C:\work") } else { PathBuf::from("/work") };
        let inside = cwd.join("src").join("a.rs").to_string_lossy().into_owned();
        let show = |tool: ToolPart| {
            let info = tool_inline(&cwd, &tool);
            (info.icon, info.title, info.description, info.block, info.body)
        };
        let read = show(completed("read", json!({"filePath": inside, "limit": 5}), "x", json!({})));
        assert_eq!(read, ("→", "Read src/a.rs".into(), Some("[limit=5]".into()), false, None));
        let bash = show(completed("bash", json!({"command": "ls"}), " out \n", json!({})));
        assert_eq!(bash, ("$", "ls".into(), None, true, Some("out".into())));
        let grep = show(completed("grep", json!({"pattern": "x", "path": "src"}), "", json!({"matches": 1})));
        assert_eq!(grep, ("✱", "Grep \"x\"".into(), Some("in src · 1 match".into()), false, None));
        let glob = show(completed("glob", json!({"pattern": "*.rs"}), "", json!({"count": 2})));
        assert_eq!(glob, ("✱", "Glob \"*.rs\"".into(), Some("2 matches".into()), false, None));
        let task = show(completed("task", json!({"description": "Look", "subagent_type": "general"}), "", json!({})));
        assert_eq!(task, ("✓", "Look".into(), Some("General Agent".into()), false, None));
        let todos = json!({"todos": [{"content": "a", "status": "completed"}, {"content": "b", "status": "pending"}]});
        assert_eq!(show(completed("todowrite", todos, "", json!({}))).4.as_deref(), Some("[✓] a\n[ ] b"));
        let other = show(completed("demo_echo", json!({"text": "hi"}), "", json!({})));
        assert_eq!(other.1, "demo_echo {\"text\":\"hi\"}");
        assert_eq!(tool_path(&cwd, &cwd.to_string_lossy()), ".");
    }
}
