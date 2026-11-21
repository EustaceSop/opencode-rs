// The session loop (`session/prompt.ts` runLoop), one model step (`session/processor.ts`) and tool execution.
use crate::history::{self, Latest, Row, Task};
use crate::tools::{self, Def, QuestionInfo, QuestionRequest, TaskParams};
use crate::{
    Acquire, Asker, ChildHost, Ctx, Engine, Error, Host, Run, RunEnd, Status, Submission, Update, compaction, mcp, object,
    plugins, profile, report, system, unknown_error, usage,
};
use indexmap::IndexMap;
use opencode_mcp::{McpError, McpManager, McpTool};
use opencode_core::agent::Agent;
use opencode_core::config::merge_deep;
use opencode_core::permission::{self, Action};
use opencode_core::{instruction, skill};
use opencode_llm::provider::{Model, Provider};
use opencode_llm::{self as llm, Event, Request, ToolCall, client, retry, shape};
use opencode_plugin::HookName;
use opencode_storage::{
    ApiErrorData, AssistantError, AssistantMessage, AssistantTime, FileAttachment, FileKind, FilePart, JsonMap, Message,
    MessagePath, NewSession, Part, PartBody, PartTime, PatchPart, ReasoningPart, Session, StepFinishPart, StepStartPart, SubtaskPart,
    TextPart, ToolPart, ToolState, ToolStateRunning, ToolTimeRunning, UserMessage, UserTime, id,
};
use opencode_tools::{PermissionRequest, Tool, ToolContext, ToolError, ToolOutput, TruncateOptions};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

const DOOM_LOOP_THRESHOLD: usize = 3;
const ABORT_GRACE: Duration = Duration::from_millis(250);
const ABORTED_TOOL: &str = "Tool execution aborted";
const TITLE_RETRIES: u32 = 2;
const INSTRUCTION_TIMEOUT: Duration = Duration::from_millis(5000);

pub(crate) enum Outcome {
    Continue,
    Stop,
    Compact,
    Failed(llm::Error),
}

// What tools of one step share.
pub(crate) struct Step<'a> {
    ctx: &'a Arc<Ctx>,
    session: &'a Session,
    agent: &'a Agent,
    rows: &'a [Row],
    defs: Vec<Def>,
    // The user addressed a subagent directly; the task tool skips its permission check.
    bypass: bool,
    host: &'a Arc<dyn Host>,
    abort: &'a CancellationToken,
    asker: Arc<Asker>,
    blocked: AtomicBool,
    mcp: McpManager,
    // MCP tools offered this step, by exposed name.
    mcp_tools: IndexMap<String, Arc<McpTool>>,
    // "agent" profile: files this session has read or written (`profile::key`), for the read-before-write rule.
    reads: Mutex<HashSet<String>>,
}

// Files the session's history shows as known to the model: completed read/write/edit calls and attached files.
fn known_files(rows: &[Row], cwd: &Path) -> HashSet<String> {
    let mut out = HashSet::new();
    for part in rows.iter().flat_map(|(_, parts)| parts) {
        match &part.body {
            PartBody::Tool(tool) if matches!(tool.tool.as_str(), "read" | "write" | "edit") && matches!(tool.state, ToolState::Completed(_)) => {
                if let Some(file) = profile::file_of(cwd, &Value::Object(tool.state.input().clone())) {
                    out.insert(profile::key(&file));
                }
            }
            PartBody::File(file) => {
                if let Some((path, _)) = crate::file_url_path(&file.url) {
                    out.insert(profile::key(&opencode_tools::util::normalize_path(&path)));
                }
            }
            _ => {}
        }
    }
    out
}

// The text and reasoning parts of one model attempt, stored when their first chunk arrives.
struct Live<'a> {
    pub(crate) engine: &'a Engine,
    host: &'a Arc<dyn Host>,
    pub(crate) session_id: &'a str,
    message_id: &'a str,
    text: Option<Part>,
    reasoning: Option<Part>,
    failed: Option<opencode_storage::StoreError>,
}

impl Live<'_> {
    fn push(&mut self, reasoning: bool, delta: String) {
        if delta.is_empty() || self.failed.is_some() {
            return;
        }
        let slot = if reasoning { &mut self.reasoning } else { &mut self.text };
        if slot.is_none() {
            let time = PartTime { start: id::now_ms(), end: None };
            let body = if reasoning {
                PartBody::Reasoning(ReasoningPart { text: String::new(), metadata: None, time })
            } else {
                PartBody::Text(TextPart { time: Some(time), ..Default::default() })
            };
            let part = Part::new(self.session_id, self.message_id, body);
            if let Err(error) = self.engine.store.upsert_part(&part) {
                self.failed = Some(error);
                return;
            }
            *slot = Some(part);
        }
        let Some(part) = slot else { return };
        match &mut part.body {
            PartBody::Text(body) => body.text.push_str(&delta),
            PartBody::Reasoning(body) => body.text.push_str(&delta),
            _ => {}
        }
        let update = if reasoning { Update::Reasoning(delta.clone()) } else { Update::Text(delta.clone()) };
        self.host.update(update);
        self.host.update(Update::Delta {
            session_id: self.session_id.to_string(),
            message_id: self.message_id.to_string(),
            part_id: part.id.clone(),
            field: "text",
            delta,
        });
    }

    // Stores the parts in their final form; provider reasoning blocks replace the streamed reasoning text.
    fn finish(self, blocks: &[llm::Reasoning]) -> Result<(), Error> {
        if let Some(error) = self.failed {
            return Err(error.into());
        }
        let end = Some(id::now_ms());
        let store = &self.engine.store;
        let mut reasoning = self.reasoning;
        for block in blocks {
            let mut metadata = JsonMap::new();
            for (key, value) in [("signature", &block.signature), ("itemId", &block.item_id), ("encrypted", &block.encrypted)] {
                if let Some(value) = value {
                    metadata.insert(key.to_string(), value.clone().into());
                }
            }
            let mut part = reasoning.take().unwrap_or_else(|| {
                let time = PartTime { start: id::now_ms(), end: None };
                Part::new(self.session_id, self.message_id, PartBody::Reasoning(ReasoningPart { text: String::new(), metadata: None, time }))
            });
            if let PartBody::Reasoning(body) = &mut part.body {
                body.text = block.text.clone();
                body.metadata = (!metadata.is_empty()).then_some(metadata);
                body.time.end = end;
            }
            store.upsert_part(&part)?;
        }
        if let Some(mut part) = reasoning {
            if let PartBody::Reasoning(body) = &mut part.body {
                body.time.end = end;
            }
            store.upsert_part(&part)?;
        }
        if let Some(mut part) = self.text {
            if let PartBody::Text(body) = &mut part.body
                && let Some(time) = &mut body.time
            {
                time.end = end;
            }
            store.upsert_part(&part)?;
        }
        Ok(())
    }
}

struct Call<'a> {
    provider: &'a Provider,
    model: &'a Model,
    system: Vec<String>,
    user_system: Option<&'a str>,
    messages: Vec<llm::Message>,
    // The user message being answered, for the plugin chat hooks.
    user: Option<&'a UserMessage>,
}

struct Prepared {
    part: Mutex<Part>,
    name: String,
    args: Value,
    failure: Option<ToolError>,
}

pub(crate) struct Guard<'a> {
    pub(crate) engine: &'a Engine,
    pub(crate) session_id: &'a str,
    pub(crate) end: RunEnd,
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        let run = self.engine.running.lock().unwrap_or_else(|e| e.into_inner()).remove(self.session_id);
        if let Some(run) = run {
            run.done.send_replace(std::mem::replace(&mut self.end, RunEnd::Failed));
        }
    }
}

pub(crate) fn zero_assistant(session_id: &str, parent_id: &str, agent: &str, provider_id: &str, model_id: &str, variant: Option<String>, ctx: &Ctx) -> AssistantMessage {
    AssistantMessage {
        id: id::message(),
        session_id: session_id.to_string(),
        time: AssistantTime { created: id::now_ms(), completed: None },
        error: None,
        parent_id: parent_id.to_string(),
        model_id: model_id.to_string(),
        provider_id: provider_id.to_string(),
        mode: agent.to_string(),
        agent: agent.to_string(),
        path: MessagePath { cwd: ctx.cwd.to_string_lossy().into_owned(), root: ctx.root.to_string_lossy().into_owned() },
        summary: None,
        cost: 0.0,
        tokens: Default::default(),
        structured: None,
        variant,
        finish: None,
    }
}

fn orphaned(tool: &ToolPart) -> bool {
    matches!(&tool.state, ToolState::Error(s) if s.metadata.as_ref().is_some_and(|m| m.get("interrupted") == Some(&Value::Bool(true))))
}

fn provider_executed(tool: &ToolPart) -> bool {
    tool.metadata.as_ref().is_some_and(|m| m.get("providerExecuted") == Some(&Value::Bool(true)))
}

// The last assistant answers the last user message and left no tool results to send back.
fn settled(latest: &Latest, rows: &[Row]) -> bool {
    let (Some(user), Some(assistant)) = (&latest.user, &latest.assistant) else { return false };
    let pending = rows
        .iter()
        .rev()
        .find(|(m, _)| m.id() == assistant.id)
        .is_some_and(|(_, parts)| parts.iter().any(|p| matches!(&p.body, PartBody::Tool(t) if !provider_executed(t) && !orphaned(t))));
    assistant.finish.as_deref().is_some_and(|f| f != "tool-calls" && f != "unknown") && !pending && assistant.parent_id == user.id
}

fn api_error(error: &llm::Error, class: &retry::Classified) -> AssistantError {
    match error {
        llm::Error::Status { status, body, headers } => AssistantError::ApiError(ApiErrorData {
            message: class.message.clone(),
            status_code: Some(u64::from(*status)),
            is_retryable: class.retryable,
            response_headers: Some(headers.iter().cloned().collect()),
            response_body: Some(body.clone()),
            metadata: None,
        }),
        llm::Error::Provider { .. } | llm::Error::Stream(_) | llm::Error::Http(_) | llm::Error::Timeout { .. } if class.retryable => {
            AssistantError::ApiError(ApiErrorData { message: class.message.clone(), is_retryable: true, ..Default::default() })
        }
        _ => AssistantError::UnknownError { message: class.message.clone(), r#ref: None },
    }
}

// `ProviderTransform.smallOptions` without model variants.
pub(crate) fn small_options(model: &Model) -> Value {
    if model.provider_id == "openai" || matches!(model.npm.as_str(), "@ai-sdk/openai" | "@ai-sdk/github-copilot" | "@ai-sdk/xai") {
        return json!({"store": false});
    }
    if matches!(model.provider_id.as_str(), "openrouter" | "llmgateway") && model.api_id.contains("google") {
        return json!({"reasoning": {"enabled": false}});
    }
    if model.provider_id == "venice" {
        return json!({"veniceParameters": {"disableThinking": true}});
    }
    json!({})
}

// First non-empty line of the model's answer, without `<think>` blocks, at most 100 UTF-16 units.
pub(crate) fn clean_title(text: &str) -> Option<String> {
    let mut rest = text;
    let mut cleaned = String::new();
    while let Some(start) = rest.find("<think>") {
        let Some(end) = rest[start..].find("</think>") else { break };
        cleaned.push_str(&rest[..start]);
        rest = rest[start + end + "</think>".len()..].trim_start_matches(opencode_tools::util::is_js_whitespace);
    }
    cleaned.push_str(rest);
    let line = cleaned.split('\n').map(opencode_tools::util::js_trim).find(|line| !line.is_empty())?;
    let units: Vec<u16> = line.encode_utf16().collect();
    Some(if units.len() > 100 { format!("{}...", String::from_utf16_lossy(&units[..97])) } else { line.to_string() })
}

struct TaskTool<'a> {
    pub(crate) engine: &'a Engine,
    step: &'a Step<'a>,
    live: &'a Mutex<Part>,
}

#[async_trait::async_trait]
impl Tool for TaskTool<'_> {
    fn id(&self) -> &str {
        "task"
    }
    fn description(&self) -> String {
        String::new()
    }
    fn parameters(&self) -> Value {
        Value::Null
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        self.engine.task(self.step, args, ctx, Some(self.live)).await
    }
}

struct SkillTool<'a> {
    step: &'a Step<'a>,
}

#[async_trait::async_trait]
impl Tool for SkillTool<'_> {
    fn id(&self) -> &str {
        "skill"
    }
    fn description(&self) -> String {
        String::new()
    }
    fn parameters(&self) -> Value {
        Value::Null
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let name = args.get("name").and_then(Value::as_str).ok_or_else(|| ToolError::InvalidArguments {
            tool: "skill".into(),
            detail: "missing field `name`".into(),
        })?;
        let registry = &self.step.ctx.skills;
        let info = registry.get(name).ok_or_else(|| ToolError::Other(registry.not_found_message(name)))?;
        ctx.permissions
            .ask(PermissionRequest { permission: "skill".into(), patterns: vec![name.into()], always: vec![name.into()], metadata: json!({}) })
            .await?;
        let dir = Path::new(&info.location).parent().filter(|p| !p.as_os_str().is_empty()).map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        let files = if info.location != skill::BUILTIN_LOCATION { skill_files(&dir, &ctx.abort).await } else { Vec::new() };
        let base = dir.to_string_lossy().into_owned();
        Ok(ToolOutput {
            title: format!("Loaded skill: {}", info.name),
            output: tools::skill_output(&info.name, &info.content, &base, &files),
            metadata: json!({"name": info.name, "dir": base}),
            attachments: Vec::new(),
        })
    }
}

// `Plugin.trigger` for the loop (failures also drop a dead host) or for background work without the engine.
pub(crate) struct Hooks<'a> {
    pub(crate) engine: Option<&'a Engine>,
    pub(crate) ctx: &'a Ctx,
}

impl Hooks<'_> {
    fn has(&self, name: HookName) -> bool {
        self.ctx.plugins.as_ref().is_some_and(|p| p.has(name))
    }

    async fn run(&self, name: HookName, input: Value, output: Value) -> Result<Value, String> {
        if let Some(engine) = self.engine {
            return engine.hook(self.ctx, name, input, output).await;
        }
        match &self.ctx.plugins {
            Some(plugins) if plugins.has(name) => plugins.host.trigger(name, input, output).await.map_err(|e| format!("plugin hook {name} failed: {e}")),
            _ => Ok(output),
        }
    }
}

// `experimental.chat.system.transform` with the original's re-join rule (two blocks at most when the head is kept).
pub(crate) async fn system_transform(hooks: &Hooks<'_>, session_id: &str, model: &Model, system: Vec<String>) -> Result<Vec<String>, String> {
    let name = HookName::ExperimentalChatSystemTransform;
    if !hooks.has(name) {
        return Ok(system);
    }
    let header = system.first().cloned().unwrap_or_default();
    let input = json!({"sessionID": session_id, "model": plugins::model_json(model)});
    let changed = hooks.run(name, input, json!({"system": system})).await?;
    let mut system: Vec<String> = changed.get("system").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect();
    if system.len() > 2 && system[0] == header {
        let rest = system.split_off(1).join("\n");
        system.push(rest);
    }
    Ok(system)
}

// `chat.params` then `chat.headers` (`session/llm/request.ts`).
pub(crate) async fn chat_hooks(
    hooks: &Hooks<'_>,
    session_id: &str,
    agent: &str,
    provider: &Provider,
    model: &Model,
    user: Option<&UserMessage>,
    request: &mut Request,
) -> Result<(), String> {
    if !hooks.has(HookName::ChatParams) && !hooks.has(HookName::ChatHeaders) {
        return Ok(());
    }
    let input = json!({
        "sessionID": session_id,
        "agent": agent,
        "model": plugins::model_json(model),
        "provider": plugins::provider_json(provider),
        "message": user.map(|user| Message::User(user.clone())),
    });
    if hooks.has(HookName::ChatParams) {
        let options = if request.options.is_null() { shape::options(provider, model, Some(session_id)) } else { request.options.clone() };
        let mut output = JsonMap::new();
        for (key, value) in [("temperature", request.temperature), ("topP", request.top_p)] {
            if let Some(value) = value {
                output.insert(key.into(), value.into());
            }
        }
        for (key, value) in [("topK", request.top_k), ("maxOutputTokens", request.max_tokens)] {
            if let Some(value) = value {
                output.insert(key.into(), value.into());
            }
        }
        output.insert("options".into(), options.clone());
        let changed = hooks.run(HookName::ChatParams, input.clone(), Value::Object(output)).await?;
        request.temperature = changed.get("temperature").and_then(Value::as_f64);
        request.top_p = changed.get("topP").and_then(Value::as_f64);
        request.top_k = changed.get("topK").and_then(Value::as_u64);
        request.max_tokens = changed.get("maxOutputTokens").and_then(Value::as_u64);
        if let Some(next) = changed.get("options").filter(|next| next.is_object() && **next != options) {
            request.options = next.clone();
        }
    }
    if hooks.has(HookName::ChatHeaders) {
        let changed = hooks.run(HookName::ChatHeaders, input, json!({"headers": {}})).await?;
        for (key, value) in changed.get("headers").and_then(Value::as_object).into_iter().flatten() {
            if let Some(value) = value.as_str() {
                request.headers.push((key.clone(), value.to_string()));
            }
        }
    }
    Ok(())
}

// An MCP tool whose `tool.execute.after` hook gets the raw `CallToolResult`, as in the original.
struct McpCall<'a> {
    engine: &'a Engine,
    ctx: &'a Ctx,
    tool: &'a McpTool,
    // The arguments after `tool.execute.before`.
    args: &'a Value,
}

#[async_trait::async_trait]
impl Tool for McpCall<'_> {
    fn id(&self) -> &str {
        self.tool.id()
    }
    fn description(&self) -> String {
        self.tool.description()
    }
    fn parameters(&self) -> Value {
        self.tool.parameters()
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let id = self.tool.id();
        ctx.permissions.ask(PermissionRequest { permission: id.into(), patterns: vec!["*".into()], always: vec!["*".into()], metadata: json!({}) }).await?;
        let result = match self.tool.call(args, Some(ctx.abort.clone())).await {
            Ok(result) => result,
            Err(McpError::Aborted) => return Err(ToolError::Aborted),
            Err(error) => return Err(ToolError::Other(error.to_string())),
        };
        let input = json!({"tool": id, "sessionID": ctx.session_id, "callID": ctx.call_id, "args": self.args});
        let result = self.engine.hook(self.ctx, HookName::ToolExecuteAfter, input, result).await.map_err(ToolError::Other)?;
        Ok(opencode_mcp::catalog::tool_output(&result))
    }
}

// Up to 10 files next to SKILL.md, as absolute paths.
async fn skill_files(dir: &Path, abort: &CancellationToken) -> Vec<String> {
    let absolute = |relative: &str| opencode_tools::util::resolve(dir, relative).to_string_lossy().into_owned();
    if let Ok(rg) = opencode_tools::ripgrep::filepath().await
        && let Ok(found) = opencode_tools::ripgrep::find(&rg, dir, "!**/SKILL.md", true, 10, abort).await
    {
        return found.iter().map(|file| absolute(file)).collect();
    }
    let mut out = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in std::fs::read_dir(&current).into_iter().flatten().flatten() {
            let path = entry.path();
            let name = entry.file_name();
            if path.is_dir() {
                if name != ".git" {
                    pending.push(path);
                }
            } else if name != "SKILL.md" && out.len() < 10 {
                out.push(path.to_string_lossy().into_owned());
            }
        }
    }
    out
}

struct QuestionTool<'a> {
    step: &'a Step<'a>,
}

#[async_trait::async_trait]
impl Tool for QuestionTool<'_> {
    fn id(&self) -> &str {
        "question"
    }
    fn description(&self) -> String {
        String::new()
    }
    fn parameters(&self) -> Value {
        Value::Null
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let invalid = |e: serde_json::Error| ToolError::InvalidArguments { tool: "question".into(), detail: e.to_string() };
        let questions: Vec<QuestionInfo> = serde_json::from_value(args.get("questions").cloned().unwrap_or(Value::Null)).map_err(invalid)?;
        let request = QuestionRequest {
            id: id::create("que", false, None),
            session_id: ctx.session_id.clone(),
            questions: questions.clone(),
            tool: Some((ctx.message_id.clone(), ctx.call_id.clone())),
        };
        let Some(answers) = self.step.host.question(&request).await else {
            self.step.blocked.store(true, Ordering::SeqCst);
            return Err(ToolError::Other(tools::QUESTION_REJECTED.into()));
        };
        Ok(ToolOutput {
            title: format!("Asked {} question{}", questions.len(), if questions.len() > 1 { "s" } else { "" }),
            output: tools::question_output(&questions, &answers),
            metadata: json!({"answers": answers}),
            attachments: Vec::new(),
        })
    }
}

impl Engine {
    pub(crate) fn acquire(&self, session_id: &str, abort: &CancellationToken) -> Acquire {
        let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(run) = running.get(session_id) {
            return Acquire::Join(run.done.subscribe());
        }
        let cancel = abort.child_token();
        let (done, _) = watch::channel(RunEnd::Running);
        running.insert(session_id.to_string(), Run { cancel: cancel.clone(), done, status: Status::Busy });
        Acquire::Owner(cancel)
    }

    pub(crate) fn set_status(&self, session_id: &str, status: Status, host: &Arc<dyn Host>) {
        if let Some(run) = self.running.lock().unwrap_or_else(|e| e.into_inner()).get_mut(session_id) {
            run.status = status.clone();
        }
        host.update(Update::Status { session_id: session_id.to_string(), status });
    }

    // One loop per session: a prompt arriving while it runs has stored its message and waits for the same result.
    pub(crate) async fn run(&self, ctx: Arc<Ctx>, session_id: &str, host: Arc<dyn Host>, abort: CancellationToken) -> Result<Session, Error> {
        loop {
            match self.acquire(session_id, &abort) {
                Acquire::Owner(cancel) => {
                    let mut guard = Guard { engine: self, session_id, end: RunEnd::Failed };
                    let mut result = self.run_loop(&ctx, session_id, &host, &cancel).await;
                    // Rust build: v2 `/goal`: when the run would go idle, a reviewer decides whether to continue.
                    while result.is_ok() && self.active_goal(session_id).is_some() {
                        match self.goal_round(&ctx, session_id, &cancel).await {
                            Ok(true) => result = self.run_loop(&ctx, session_id, &host, &cancel).await,
                            Ok(false) => break,
                            Err(error) => {
                                result = Err(error);
                                break;
                            }
                        }
                    }
                    if let Err(error) = &result {
                        self.goal_interrupted(session_id, error);
                    }
                    if result.is_ok() {
                        guard.end = RunEnd::Ok;
                    }
                    drop(guard);
                    // Model failures and aborts were reported where they were recorded on the message.
                    if let Err(error) = &result
                        && !matches!(error, Error::Aborted | Error::Llm(_))
                    {
                        report(&host, session_id, unknown_error(error.to_string()));
                    }
                    host.update(Update::Status { session_id: session_id.to_string(), status: Status::Idle });
                    result?;
                    self.prune(&ctx, session_id)?;
                    return self.session(session_id);
                }
                Acquire::Join(mut done) => {
                    tokio::select! {
                        _ = done.changed() => {}
                        _ = abort.cancelled() => return Err(Error::Aborted),
                    }
                    let clean = matches!(*done.borrow(), RunEnd::Ok);
                    let rows = self.rows(session_id)?;
                    if !clean || settled(&history::latest(&rows), &rows) {
                        return self.session(session_id);
                    }
                }
            }
        }
    }

    async fn run_loop(&self, ctx: &Arc<Ctx>, session_id: &str, host: &Arc<dyn Host>, abort: &CancellationToken) -> Result<(), Error> {
        let session = self.session(session_id)?;
        let manager = self.mcp_for(ctx).await;
        let mut step = 0u64;
        // Rust build: v2 resets the agent's step allowance when new user input is promoted, e.g. a steered prompt.
        let (mut allowance, mut answering) = (0u64, String::new());
        loop {
            if abort.is_cancelled() {
                return Err(Error::Aborted);
            }
            self.set_status(session_id, Status::Busy, host);
            let mut rows = self.rows(session_id)?;
            let mut latest = history::latest(&rows);
            let Some(last_user) = latest.user.clone() else {
                return Err(Error::Config("No user message found in stream. This should never happen.".into()));
            };
            if settled(&latest, &rows) {
                return Ok(());
            }
            step += 1;
            if step == 1 {
                self.spawn_title(ctx, &session, &rows, &last_user, host);
            }
            let (provider, model) = ctx.model(&last_user.model.provider_id, &last_user.model.model_id)?;

            match latest.tasks.pop() {
                Some(Task::Subtask(task)) => {
                    self.subtask(ctx, &session, &rows, &last_user, task, host, abort, &manager).await?;
                    continue;
                }
                Some(Task::Compaction(task)) => {
                    let flags = (task.auto, task.overflow == Some(true));
                    if !self.compact(ctx, &session, &rows, &last_user, flags, host, abort, &manager).await? {
                        return Ok(());
                    }
                    continue;
                }
                None => {}
            }
            if let Some(finished) = &latest.finished
                && finished.summary != Some(true)
                && usage::is_overflow(&ctx.info, &finished.tokens, model)
            {
                self.create_compaction(session_id, &last_user.agent, last_user.model.clone(), true, None)?;
                continue;
            }

            let agent = ctx.agent(&last_user.agent)?;
            if answering != last_user.id {
                answering = last_user.id.clone();
                allowance = 0;
            }
            allowance += 1;
            let last_step = agent.steps.is_some_and(|max| allowance >= max);
            self.remind(ctx, &session, agent, &mut rows)?;

            let mut assistant =
                zero_assistant(session_id, &last_user.id, &agent.name, &provider.id, &model.id, last_user.model.variant.clone(), ctx);
            self.store.upsert_message(&Message::Assistant(assistant.clone()))?;

            let session_rules = tools::to_rules(session.permission.as_deref().unwrap_or_default());
            let ruleset = permission::merge(&[&agent.permission, &session_rules]);
            let bypass = rows.iter().rev().find(|(m, _)| m.as_user().is_some()).is_some_and(|(_, parts)| parts.iter().any(|p| matches!(p.body, PartBody::Agent(_))));
            let mcp_tools = if ctx.flags.experimental_code_mode { IndexMap::new() } else { manager.mcp_tools() };
            let mcp_system = mcp::system(&manager.instructions(), &ruleset);
            let mut defs = Self::definitions(ctx, agent, model, &ruleset, last_user.tools.as_ref(), &manager, &mcp_tools);
            self.tool_definitions(ctx, &mut defs, &mcp_tools).await?;
            let state = Step {
                ctx,
                session: &session,
                agent,
                rows: &rows,
                defs,
                bypass,
                host,
                abort,
                asker: Arc::new(Asker::new(host.clone(), self.approvals.clone(), ruleset, session_id)),
                blocked: AtomicBool::new(false),
                mcp: manager.clone(),
                mcp_tools,
                reads: Mutex::new(if ctx.names.active() { known_files(&rows, &ctx.cwd) } else { HashSet::new() }),
            };

            let mut system = vec![system::environment(model, &ctx.cwd, &ctx.root, ctx.worktree.is_some())];
            system.extend(self.instructions(ctx).await);
            system.extend(mcp_system);
            system.extend(system::skills(&ctx.skills, &agent.permission));
            if step == 1 {
                self.spawn_summarize(ctx, session_id, &last_user.id, host);
            }
            let transformed = self.transform_messages(ctx, &rows).await?;
            let mut messages = history::to_model_messages(transformed.as_deref().unwrap_or(&rows), model);
            profile::replay(&mut messages, &ctx.names);
            if let Some(goal) = self.active_goal(session_id) {
                crate::goal::remind(&mut messages, &goal);
            }
            if last_step {
                messages.push(llm::Message::assistant(system::MAX_STEPS, Vec::new()));
            }
            let call = Call { provider, model, system, user_system: last_user.system.as_deref(), messages, user: Some(&last_user) };
            let outcome = self.process(&state, call, &mut assistant).await;
            self.claims.clear(&assistant.id);

            let done = assistant.finish.as_deref().is_some_and(|f| f != "tool-calls" && f != "unknown");
            if done && assistant.error.is_none() && assistant.finish.as_deref() == Some("content-filter") {
                let message = "The response was blocked by the provider's content filter".to_string();
                let error = AssistantError::ContentFilterError { message };
                assistant.error = Some(error.clone());
                self.store.upsert_message(&Message::Assistant(assistant))?;
                report(host, session_id, error);
                return Ok(());
            }
            match outcome? {
                Outcome::Continue => {}
                Outcome::Stop => return Ok(()),
                Outcome::Failed(error) => return Err(error.into()),
                Outcome::Compact => {
                    let overflow = assistant.finish.is_none();
                    self.create_compaction(session_id, &last_user.agent, last_user.model.clone(), true, Some(overflow))?;
                }
            }
        }
    }

    // Tools offered to the model this step, sorted by name.
    pub(crate) fn definitions(
        ctx: &Ctx,
        agent: &Agent,
        model: &Model,
        ruleset: &[permission::Rule],
        switches: Option<&std::collections::BTreeMap<String, bool>>,
        manager: &McpManager,
        mcp_tools: &IndexMap<String, Arc<McpTool>>,
    ) -> Vec<Def> {
        let patch = opencode_tools::uses_apply_patch(&model.api_id);
        let question = ["app", "cli", "desktop"].contains(&ctx.flags.client.as_str()) || ctx.flags.enable_question_tool;
        let mut defs: Vec<Def> = ctx
            .tools
            .iter()
            .filter(|t| match t.id() {
                "apply_patch" => patch,
                "edit" | "write" => !patch,
                _ => true,
            })
            .map(|t| Def { name: t.id().to_string(), description: t.description(), parameters: t.parameters() })
            .collect();
        defs.push(tools::task_def(&ctx.agents, agent));
        defs.push(tools::skill_def());
        if question {
            defs.push(tools::question_def());
        }
        if !manager.resource_servers().is_empty() {
            defs.extend(mcp::resource_defs());
        }
        // An MCP tool replaces a built-in of the same name.
        defs.retain(|d| !mcp_tools.contains_key(&d.name));
        // A plugin or custom tool replaces a built-in of the same id.
        let last = |index: usize, name: &str, all: &[Def]| !all[index + 1..].iter().any(|later| later.name == name);
        let keep: Vec<bool> = defs.iter().enumerate().map(|(index, def)| last(index, &def.name, &defs)).collect();
        let mut keep = keep.into_iter();
        defs.retain(|_| keep.next().unwrap_or(true));
        // "agent" profile: adapted built-ins get that profile's description and schema; names stay internal here.
        let names = &ctx.names;
        if names.active() {
            let agents = tools::subagent_lines(&ctx.agents, agent);
            for def in defs.iter_mut().filter(|d| names.adapted(&d.name)) {
                def.description = profile::description(&def.name, names, &agents);
                def.parameters = profile::schema(&def.name);
            }
            // Another tool already named like an exposed built-in replaces it.
            let taken: HashSet<String> = defs.iter().filter(|d| !names.adapted(&d.name)).map(|d| d.name.clone()).chain(mcp_tools.keys().cloned()).collect();
            defs.retain(|d| !names.adapted(&d.name) || !taken.contains(names.exposed(&d.name)));
        }
        defs.extend(mcp_tools.iter().map(|(name, tool)| Def {
            name: name.clone(),
            description: tool.def().description.clone().unwrap_or_default(),
            parameters: tool.def().parameters(),
        }));
        let disabled = permission::disabled(defs.iter().map(|d| d.name.as_str()), ruleset);
        defs.retain(|d| d.name == "invalid" || (!disabled.contains(&d.name) && switches.and_then(|s| s.get(&d.name)) != Some(&false)));
        defs.sort_by(|a, b| names.exposed(&a.name).cmp(names.exposed(&b.name)));
        defs
    }

    // Instruction files and remote `instructions` urls, re-read every step.
    pub(crate) async fn instructions(&self, ctx: &Ctx) -> Vec<String> {
        let paths = instruction::system_paths(&ctx.instruction, &ctx.info.instructions);
        let mut out = instruction::load(&paths.files);
        let fetched = futures_util::future::join_all(paths.remote.iter().map(|url| async move {
            let fetch = async {
                let response = self.http.get(url).send().await.ok()?.error_for_status().ok()?;
                response.text().await.ok()
            };
            tokio::time::timeout(INSTRUCTION_TIMEOUT, fetch).await.ok().flatten().unwrap_or_default()
        }))
        .await;
        out.extend(paths.remote.iter().zip(fetched).filter(|(_, body)| !body.is_empty()).map(|(url, body)| instruction::format(url, &body)));
        out
    }

    // Plan / build-switch reminders on the last user message (`session/reminders.ts`).
    fn remind(&self, ctx: &Ctx, session: &Session, agent: &Agent, rows: &mut [Row]) -> Result<(), Error> {
        let was_plan = rows.iter().any(|(m, _)| m.as_assistant().is_some_and(|a| a.agent == "plan"));
        let last_plan = rows.iter().rev().find_map(|(m, _)| m.as_assistant()).is_some_and(|a| a.agent == "plan");
        let Some((message, parts)) = rows.iter_mut().rev().find(|(m, _)| m.as_user().is_some()) else { return Ok(()) };
        let reminder = |text: String| Part::new(&session.id, message.id(), PartBody::Text(TextPart { text, synthetic: Some(true), ..Default::default() }));
        if !ctx.flags.experimental_plan_mode {
            if agent.name == "plan" {
                parts.push(reminder(system::PLAN.to_string()));
            }
            if was_plan && agent.name == "build" {
                parts.push(reminder(system::BUILD_SWITCH.to_string()));
            }
            return Ok(());
        }
        let plan = ctx.plan_path(session);
        let text = if agent.name != "plan" && last_plan {
            match plan.exists() {
                true => format!("{}\n\nA plan file exists at {}. You should execute on the plan defined within it", system::BUILD_SWITCH, plan.display()),
                false => system::BUILD_SWITCH.to_string(),
            }
        } else if agent.name == "plan" && !last_plan {
            let info = if plan.exists() {
                format!("A plan file already exists at {}. You can read it and make incremental edits using the edit tool.", plan.display())
            } else {
                if let Some(dir) = plan.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| Error::Config(format!("cannot create {}: {e}", dir.display())))?;
                }
                format!("No plan file exists yet. You should create your plan at {} using the write tool.", plan.display())
            };
            system::PLAN_MODE.replacen("${planInfo}", &info, 1)
        } else {
            return Ok(());
        };
        let part = reminder(text);
        self.store.upsert_part(&part)?;
        parts.push(part);
        Ok(())
    }

    fn save_assistant(&self, assistant: &AssistantMessage) -> Result<(), Error> {
        Ok(self.store.upsert_message(&Message::Assistant(assistant.clone()))?)
    }

    fn abort_assistant(&self, assistant: &mut AssistantMessage, host: &Arc<dyn Host>) -> Result<Error, Error> {
        let error = assistant.error.get_or_insert_with(AssistantError::aborted).clone();
        assistant.time.completed = Some(id::now_ms());
        self.save_assistant(assistant)?;
        host.update(Update::SessionError { session_id: assistant.session_id.clone(), error });
        Ok(Error::Aborted)
    }

    // `processor.ts` create + cleanup around one step: the snapshot taken before the model call becomes a patch part.
    async fn process(&self, step: &Step<'_>, call: Call<'_>, assistant: &mut AssistantMessage) -> Result<Outcome, Error> {
        let mut snapshot = step.ctx.workspace.snapshot().track().await;
        let result = self.process_step(step, call, assistant, &mut snapshot).await;
        if let Some(hash) = snapshot.take() {
            self.patch_part(step, assistant, &hash).await?;
        }
        result
    }

    // Files changed since `hash`, as a `patch` part.
    async fn patch_part(&self, step: &Step<'_>, assistant: &AssistantMessage, hash: &str) -> Result<(), Error> {
        let patch = step.ctx.workspace.snapshot().patch(hash).await;
        if !patch.files.is_empty() {
            let body = PartBody::Patch(PatchPart { hash: patch.hash, files: patch.files });
            self.store.upsert_part(&Part::new(&assistant.session_id, &assistant.id, body))?;
        }
        Ok(())
    }

    // One model call with retries, then its tool calls; everything is persisted on `assistant`.
    async fn process_step(
        &self,
        step: &Step<'_>,
        call: Call<'_>,
        assistant: &mut AssistantMessage,
        snapshot: &mut Option<String>,
    ) -> Result<Outcome, Error> {
        let session_id = step.session.id.as_str();
        let (provider, model, user) = (call.provider, call.model, call.user);
        // An agent's own prompt wins in both profiles; otherwise the "agent" profile replaces the provider-family prompt.
        let head = step.agent.prompt.clone().unwrap_or_else(|| {
            if step.ctx.info.original_tools() { system::provider_prompt(model) } else { profile::SYSTEM.to_string() }
        });
        let system: Vec<String> = std::iter::once(head)
            .chain(call.system)
            .chain(call.user_system.map(str::to_string))
            .filter(|s| !s.is_empty())
            .collect();
        let system = self.system_transform(step, model, vec![system.join("\n")]).await?;
        let mut request = Request {
            model: model.api_id.clone(),
            system,
            messages: call.messages,
            temperature: step.agent.temperature,
            top_p: step.agent.top_p,
            tools: step
                .defs
                .iter()
                .filter(|d| d.name != "invalid")
                .map(|d| llm::Tool { name: step.ctx.names.exposed(&d.name).to_string(), description: d.description.clone(), parameters: d.parameters.clone() })
                .collect(),
            ..Default::default()
        };
        if !step.agent.options.is_empty() {
            let mut options = shape::options(provider, model, Some(session_id));
            merge_deep(&mut options, Value::Object(step.agent.options.clone()));
            request.options = options;
        }
        let meta = shape::Session {
            session_id,
            parent_session_id: step.session.parent_id.as_deref(),
            request_id: Some(&assistant.parent_id),
            project_id: Some(&step.session.project_id),
        };
        shape::apply(&mut request, provider, model, &meta);
        self.chat_hooks(step, provider, model, user, &mut request).await?;
        if snapshot.is_none() {
            *snapshot = step.ctx.workspace.snapshot().track().await;
        }
        self.store.upsert_part(&Part::new(session_id, &assistant.id, PartBody::StepStart(StepStartPart { snapshot: snapshot.clone() })))?;

        let loader = plugins::PluginAuth::new(step.ctx.plugins.as_ref());
        let mut attempt = 0u32;
        let (calls, reason, used, nano_aiu) = loop {
            let mut live =
                Live { engine: self, host: step.host, session_id, message_id: &assistant.id, text: None, reasoning: None, failed: None };
            let mut calls: Vec<ToolCall> = Vec::new();
            let mut finish = (String::from("unknown"), llm::Usage::default());
            let streaming = client::stream_via(&self.http, &loader, provider, model, &request, |event| match event {
                Event::TextDelta(delta) => live.push(false, delta),
                Event::ReasoningDelta(delta) => live.push(true, delta),
                Event::ToolCall(call) => calls.push(call),
                Event::Finish { reason, usage } => finish = (reason, usage),
            });
            let result = tokio::select! {
                result = streaming => Some(result),
                _ = step.abort.cancelled() => None,
            };
            let error = match result {
                None => {
                    live.finish(&[])?;
                    return Err(self.abort_assistant(assistant, step.host)?);
                }
                Some(Ok(completion)) => {
                    self.text_complete(step, &mut live).await?;
                    live.finish(&completion.reasoning)?;
                    break (calls, finish.0, finish.1, completion.nano_aiu);
                }
                Some(Err(error)) => error,
            };
            // Output of a failed attempt stays; the next attempt writes new parts.
            live.finish(&[])?;
            attempt += 1;
            let random = (id::now_ms() % 1000) as f64 / 1000.0;
            if let Some((info, delay_ms)) = retry::next(&error, &provider.id, attempt, random) {
                step.host.update(Update::Retry { attempt, message: info.message.clone(), delay_ms });
                self.set_status(session_id, Status::Retry { attempt, message: info.message, next: id::now_ms() + delay_ms }, step.host);
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
                    _ = step.abort.cancelled() => return Err(self.abort_assistant(assistant, step.host)?),
                }
                self.set_status(session_id, Status::Busy, step.host);
                continue;
            }
            let class = retry::classify(&error, &provider.id);
            assistant.time.completed = Some(id::now_ms());
            if class.overflow {
                let body = match &error {
                    llm::Error::Status { body, .. } => Some(body.clone()),
                    _ => None,
                };
                let overflow = AssistantError::ContextOverflowError { message: class.message, response_body: body };
                if step.ctx.info.compaction.auto == Some(false) && assistant.summary != Some(true) {
                    assistant.error = Some(overflow.clone());
                    assistant.finish = Some("error".into());
                    self.save_assistant(assistant)?;
                    report(step.host, session_id, overflow);
                    return Ok(Outcome::Failed(error));
                }
                self.save_assistant(assistant)?;
                step.host.update(Update::SessionError { session_id: session_id.to_string(), error: overflow });
                return Ok(Outcome::Compact);
            }
            let failure = api_error(&error, &class);
            assistant.error = Some(failure.clone());
            self.save_assistant(assistant)?;
            report(step.host, session_id, failure);
            return Ok(Outcome::Failed(error));
        };

        if !calls.is_empty() && assistant.summary != Some(true) {
            let mut ids = Vec::new();
            {
                let shared: &AssistantMessage = assistant;
                let mut prepared = Vec::new();
                for call in &calls {
                    let next = self.prepare(step, shared, call, &prepared).await?;
                    ids.push(next.part.lock().unwrap_or_else(|e| e.into_inner()).id.clone());
                    prepared.push(next);
                }
                // Parts exist in call order; the calls themselves run concurrently.
                let mut running = std::pin::pin!(futures_util::future::join_all(prepared.into_iter().map(|p| self.run_tool(step, shared, p))));
                // A subagent needs its own grace period to wind down before this one ends.
                let grace = if calls.iter().any(|c| c.name == "task" || step.ctx.names.internal(&c.name) == Some("task")) { ABORT_GRACE * 4 } else { ABORT_GRACE };
                let results = tokio::select! {
                    results = &mut running => Some(results),
                    _ = step.abort.cancelled() => tokio::time::timeout(grace, &mut running).await.ok(),
                };
                for result in results.into_iter().flatten() {
                    result?;
                }
            }
            if step.abort.is_cancelled() {
                for part_id in &ids {
                    self.interrupt(part_id, step.host)?;
                }
                return Err(self.abort_assistant(assistant, step.host)?);
            }
        }

        let completed = step.ctx.workspace.snapshot().track().await;
        let tokens = usage::tokens(&used);
        // Copilot reports its own billed amount; it overrides token pricing when present.
        let cost = nano_aiu.map_or_else(|| usage::cost(model, &used), |nano| nano / 1e11);
        assistant.finish = Some(reason.clone());
        assistant.cost += cost;
        assistant.tokens = tokens;
        assistant.time.completed = Some(id::now_ms());
        self.store.upsert_part(&Part::new(
            session_id,
            &assistant.id,
            PartBody::StepFinish(StepFinishPart { reason: reason.clone(), snapshot: completed, cost, tokens }),
        ))?;
        self.save_assistant(assistant)?;
        if let Some(hash) = snapshot.take() {
            self.patch_part(step, assistant, &hash).await?;
        }
        self.spawn_summarize(step.ctx, session_id, &assistant.parent_id, step.host);
        step.host.update(Update::StepFinish { reason, tokens });

        if assistant.summary != Some(true) && usage::is_overflow(&step.ctx.info, &tokens, model) {
            return Ok(Outcome::Compact);
        }
        let stop = step.blocked.load(Ordering::SeqCst) && step.ctx.info.experimental.continue_loop_on_deny != Some(true);
        Ok(if stop { Outcome::Stop } else { Outcome::Continue })
    }

    // A tool part still running after an abort: mark it interrupted.
    fn interrupt(&self, part_id: &str, host: &Arc<dyn Host>) -> Result<(), Error> {
        let Some(mut part) = self.store.get_part(part_id)? else { return Ok(()) };
        let PartBody::Tool(tool) = &mut part.body else { return Ok(()) };
        if tool.state.is_terminal() {
            return Ok(());
        }
        let mut metadata = match &tool.state {
            ToolState::Running(s) => s.metadata.clone().unwrap_or_default(),
            _ => JsonMap::new(),
        };
        metadata.insert("interrupted".into(), true.into());
        tool.state = tool.state.to_error(ABORTED_TOOL, Some(metadata), id::now_ms());
        self.store.upsert_part(&part)?;
        host.update(Update::Tool(Box::new(part)));
        Ok(())
    }

    // Registers a tool call as a running part. Unknown tools and unparsable arguments are rerouted to `invalid`.
    async fn prepare(&self, step: &Step<'_>, assistant: &AssistantMessage, call: &ToolCall, earlier: &[Prepared]) -> Result<Prepared, Error> {
        let names = &step.ctx.names;
        let known = |name: &str| step.defs.iter().any(|d| d.name == name);
        let lower = call.name.to_lowercase();
        // An exposed name ("agent" profile) maps to its internal tool; internal names are accepted as well.
        let resolved = if let Some(internal) = names.internal(&call.name).filter(|id| known(id)) {
            Some(internal.to_string())
        } else if known(&call.name) {
            Some(call.name.clone())
        } else {
            Some(lower).filter(|lower| *lower != call.name && known(lower))
        };
        let parsed: Result<Value, _> = if call.arguments.trim().is_empty() { Ok(json!({})) } else { serde_json::from_str(&call.arguments) };
        // Rust build: v2's tool input repair (JSON sent as a string, numbers and booleans as strings, nulls for
        // optional fields), against the schema the model was shown, before the arguments are translated and parsed.
        let parsed = match (&resolved, parsed) {
            (Some(name), Ok(args)) => match step.defs.iter().find(|d| d.name == *name) {
                Some(def) => Ok(opencode_tools::repair::repair_input(args, &def.parameters)),
                None => Ok(args),
            },
            (_, parsed) => parsed,
        };
        let parsed = match (&resolved, parsed) {
            (Some(name), Ok(args)) if names.adapted(name) => Ok(profile::to_internal(name, args)),
            (_, parsed) => parsed,
        };
        let (name, args) = match (resolved, parsed) {
            (None, _) => {
                let available: Vec<&str> = step.defs.iter().map(|d| names.exposed(&d.name)).filter(|n| *n != "invalid").collect();
                let error = format!("Model tried to call unavailable tool '{}'. Available tools: {}.", call.name, available.join(", "));
                ("invalid".to_string(), json!({"tool": call.name, "error": error}))
            }
            (Some(_), Err(error)) => {
                let error = format!("Invalid input for tool {}: JSON parsing failed: Text: {}.
Error message: {error}", call.name, call.arguments);
                ("invalid".to_string(), json!({"tool": call.name, "error": error}))
            }
            (Some(name), Ok(args)) => (name, args),
        };
        let input = match &args {
            Value::Object(map) => map.clone(),
            other => object(json!({"value": other})),
        };
        let state = ToolState::Running(ToolStateRunning { input: input.clone(), title: None, metadata: None, time: ToolTimeRunning { start: id::now_ms() } });
        let part = Part::new(
            &assistant.session_id,
            &assistant.id,
            PartBody::Tool(ToolPart { call_id: call.id.clone(), tool: name.clone(), state, metadata: None }),
        );
        self.store.upsert_part(&part)?;
        step.host.update(Update::Tool(Box::new(part.clone())));

        // Three identical calls in a row need the user's go-ahead.
        let mut failure = None;
        let same = |p: &Prepared| {
            let part = p.part.lock().unwrap_or_else(|e| e.into_inner());
            matches!(&part.body, PartBody::Tool(t) if t.tool == name && *t.state.input() == input)
        };
        if earlier.len() + 1 >= DOOM_LOOP_THRESHOLD && earlier[earlier.len() + 1 - DOOM_LOOP_THRESHOLD..].iter().all(same) {
            let request = PermissionRequest {
                permission: "doom_loop".into(),
                patterns: vec![name.clone()],
                always: vec![name.clone()],
                metadata: json!({"tool": name, "input": input}),
            };
            let tool = Some((assistant.id.clone(), call.id.clone()));
            failure = step.asker.ask_with(request, &step.agent.permission, tool).await.err();
        }
        Ok(Prepared { part: Mutex::new(part), name, args, failure })
    }

    async fn run_tool(&self, step: &Step<'_>, assistant: &AssistantMessage, mut prepared: Prepared) -> Result<(), Error> {
        let outcome = match prepared.failure.take() {
            Some(error) => Err(error),
            None => self.dispatch(step, assistant, &prepared).await,
        };
        let mut part = prepared.part.into_inner().unwrap_or_else(|e| e.into_inner());
        let PartBody::Tool(tool) = &mut part.body else { return Ok(()) };
        let end = id::now_ms();
        let progress = match &tool.state {
            ToolState::Running(s) => s.metadata.clone(),
            _ => None,
        };
        tool.state = match outcome {
            Ok(output) => {
                let attachments: Vec<FileAttachment> = output
                    .attachments
                    .into_iter()
                    .map(|a| FileAttachment {
                        id: id::part(),
                        session_id: part.session_id.clone(),
                        message_id: part.message_id.clone(),
                        kind: FileKind::File,
                        file: FilePart { mime: a.mime, url: a.url, filename: None, source: None },
                    })
                    .collect();
                tool.state.to_completed(output.output, output.title, object(output.metadata), (!attachments.is_empty()).then_some(attachments), end)
            }
            Err(_) if step.abort.is_cancelled() => {
                let mut metadata = progress.unwrap_or_default();
                metadata.insert("interrupted".into(), true.into());
                tool.state.to_error(ABORTED_TOOL, Some(metadata), end)
            }
            Err(error) => {
                if matches!(error, ToolError::Rejected) {
                    step.blocked.store(true, Ordering::SeqCst);
                }
                tool.state.to_error(error.to_string(), progress, end)
            }
        };
        let todos = tool.tool == "todowrite" && matches!(tool.state, ToolState::Completed(_));
        self.store.upsert_part(&part)?;
        step.host.update(Update::Tool(Box::new(part.clone())));
        if todos {
            step.host.update(Update::Todos { session_id: part.session_id.clone(), todos: self.store.get_todos(&part.session_id)? });
        }
        Ok(())
    }

    async fn dispatch(&self, step: &Step<'_>, assistant: &AssistantMessage, prepared: &Prepared) -> Result<ToolOutput, ToolError> {
        let ctx = step.ctx;
        let call_id = match &prepared.part.lock().unwrap_or_else(|e| e.into_inner()).body {
            PartBody::Tool(tool) => tool.call_id.clone(),
            _ => String::new(),
        };
        let tctx = ToolContext {
            session_id: assistant.session_id.clone(),
            message_id: assistant.id.clone(),
            permissions: step.asker.for_call(&assistant.id, &call_id),
            call_id,
            agent: step.agent.name.clone(),
            cwd: ctx.cwd.clone(),
            worktree: ctx.root.clone(),
            data_dir: self.paths.data.clone(),
            abort: step.abort.clone(),
            hooks: Some(self.tool_hooks(ctx, Some(step.host))),
        };
        let opts = TruncateOptions {
            max_lines: ctx.info.tool_output.max_lines() as usize,
            max_bytes: ctx.info.tool_output.max_bytes() as usize,
            has_task_tool: permission::evaluate("task", "*", &[&step.agent.permission]).action != Action::Deny,
            ..Default::default()
        };
        let name = prepared.name.as_str();
        let args = self.tool_before(ctx, name, &tctx.session_id, &tctx.call_id, prepared.args.clone()).await?;
        let used = args.clone();
        if ctx.names.adapted(name) {
            let result = self.adapted(step, assistant, prepared, args, &tctx, &opts).await;
            let output = result.map_err(|e| profile::error(name, &ctx.names, &used, &ctx.cwd, e))?;
            return self.tool_after(ctx, name, &tctx.session_id, &tctx.call_id, &used, output).await;
        }
        let output = match name {
            "task" => opencode_tools::execute_with_truncation(&TaskTool { engine: self, step, live: &prepared.part }, args, &tctx, &opts).await,
            "skill" => opencode_tools::execute_with_truncation(&SkillTool { step }, args, &tctx, &opts).await,
            "question" => opencode_tools::execute_with_truncation(&QuestionTool { step }, args, &tctx, &opts).await,
            name if step.mcp_tools.contains_key(name) => {
                let tool = McpCall { engine: self, ctx, tool: step.mcp_tools[name].as_ref(), args: &used };
                return opencode_tools::execute_with_truncation(&tool, args, &tctx, &opts).await;
            }
            name if let Some(kind) = mcp::kind(name) => {
                opencode_tools::execute_with_truncation(&mcp::ResourceTool { kind, manager: step.mcp.clone() }, args, &tctx, &opts).await
            }
            name => {
                let tool = ctx.tools.iter().rev().find(|t| t.id() == name).ok_or_else(|| ToolError::Other(format!("Unknown tool: {name}")))?;
                let output = opencode_tools::execute_with_truncation(tool.as_ref(), args, &tctx, &opts).await?;
                Ok(if name == "read" { self.nearby_instructions(step, assistant, &used, output) } else { output })
            }
        }?;
        self.tool_after(ctx, name, &tctx.session_id, &tctx.call_id, &used, output).await
    }

    // A built-in shown under the "agent" profile: the behaviour its description promises, on internal arguments.
    async fn adapted(
        &self,
        step: &Step<'_>,
        assistant: &AssistantMessage,
        prepared: &Prepared,
        args: Value,
        tctx: &ToolContext,
        opts: &TruncateOptions,
    ) -> Result<ToolOutput, ToolError> {
        let ctx = step.ctx;
        let name = prepared.name.as_str();
        let reads = || step.reads.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(file) = profile::file_of(&ctx.cwd, &args) {
            match name {
                "read" if file.is_dir() => return Err(profile::directory(&file)),
                "write" | "edit" if file.is_file() && !reads().contains(&profile::key(&file)) => return Err(profile::unread(&file, name)),
                _ => {}
            }
        }
        let used = args.clone();
        let mut output = match name {
            "task" => opencode_tools::execute_with_truncation(&TaskTool { engine: self, step, live: &prepared.part }, args, tctx, opts).await?,
            "glob" => opencode_tools::execute_with_truncation(&opencode_tools::agent::GlobTool::default(), args, tctx, opts).await?,
            "grep" => opencode_tools::execute_with_truncation(&opencode_tools::agent::GrepTool::default(), args, tctx, opts).await?,
            name => {
                let tool = ctx.tools.iter().rev().find(|t| t.id() == name).ok_or_else(|| ToolError::Other(format!("Unknown tool: {name}")))?;
                opencode_tools::execute_with_truncation(tool.as_ref(), args, tctx, opts).await?
            }
        };
        if name == "read" {
            if let Some(text) = opencode_tools::read::numbered(&output.output) {
                output.output = text;
            }
            output = self.nearby_instructions(step, assistant, &used, output);
        }
        // Resolved again: a file that was just written now has its real path.
        if matches!(name, "read" | "write" | "edit")
            && let Some(file) = profile::file_of(&ctx.cwd, &used)
        {
            reads().insert(profile::key(&file));
        }
        Ok(output)
    }

    // `tool.execute.before`: plugins may rewrite the arguments.
    async fn tool_before(&self, ctx: &Ctx, tool: &str, session_id: &str, call_id: &str, args: Value) -> Result<Value, ToolError> {
        let input = json!({"tool": tool, "sessionID": session_id, "callID": call_id});
        let mut output = self.hook(ctx, HookName::ToolExecuteBefore, input, json!({"args": args})).await.map_err(ToolError::Other)?;
        Ok(output.get_mut("args").map(Value::take).unwrap_or(Value::Null))
    }

    // `tool.execute.after`: plugins may rewrite title, output and metadata.
    async fn tool_after(&self, ctx: &Ctx, tool: &str, session_id: &str, call_id: &str, args: &Value, mut output: ToolOutput) -> Result<ToolOutput, ToolError> {
        if !ctx.plugins.as_ref().is_some_and(|p| p.has(HookName::ToolExecuteAfter)) {
            return Ok(output);
        }
        let input = json!({"tool": tool, "sessionID": session_id, "callID": call_id, "args": args});
        let mut result = json!({"title": output.title, "output": output.output, "metadata": output.metadata});
        if !output.attachments.is_empty() {
            let files: Vec<Value> = output.attachments.iter().map(|a| json!({"type": "file", "mime": a.mime, "url": a.url})).collect();
            result["attachments"] = files.into();
        }
        let changed = self.hook(ctx, HookName::ToolExecuteAfter, input, result).await.map_err(ToolError::Other)?;
        if let Some(title) = changed.get("title").and_then(Value::as_str) {
            output.title = title.to_string();
        }
        if let Some(text) = changed.get("output").and_then(Value::as_str) {
            output.output = text.to_string();
        }
        if let Some(metadata) = changed.get("metadata") {
            output.metadata = metadata.clone();
        }
        Ok(output)
    }

    // `tool.definition`, once per registry tool per request.
    pub(crate) async fn tool_definitions(&self, ctx: &Ctx, defs: &mut [Def], mcp_tools: &IndexMap<String, Arc<McpTool>>) -> Result<(), Error> {
        if !ctx.plugins.as_ref().is_some_and(|p| p.has(HookName::ToolDefinition)) {
            return Ok(());
        }
        for def in defs.iter_mut() {
            if mcp_tools.contains_key(&def.name) || mcp::kind(&def.name).is_some() {
                continue;
            }
            let output = json!({"description": def.description, "parameters": def.parameters, "jsonSchema": def.parameters});
            let changed = self.hook(ctx, HookName::ToolDefinition, json!({"toolID": def.name}), output).await.map_err(Error::Config)?;
            if let Some(description) = changed.get("description").and_then(Value::as_str) {
                def.description = description.to_string();
            }
            let schema = ["jsonSchema", "parameters"].iter().filter_map(|key| changed.get(*key)).find(|schema| schema.is_object() && **schema != def.parameters);
            if let Some(schema) = schema.cloned() {
                def.parameters = schema;
            }
        }
        Ok(())
    }

    // `experimental.chat.messages.transform`: the history as plugins rewrote it, when any plugin wants to.
    pub(crate) async fn transform_messages(&self, ctx: &Ctx, rows: &[Row]) -> Result<Option<Vec<Row>>, Error> {
        let name = HookName::ExperimentalChatMessagesTransform;
        if !ctx.plugins.as_ref().is_some_and(|p| p.has(name)) {
            return Ok(None);
        }
        let messages: Vec<Value> = rows.iter().map(|(info, parts)| json!({"info": info, "parts": parts})).collect();
        let mut changed = self.hook(ctx, name, json!({}), json!({"messages": messages})).await.map_err(Error::Config)?;
        let list: Vec<opencode_storage::MessageWithParts> = serde_json::from_value(changed.get_mut("messages").map(Value::take).unwrap_or_default())
            .map_err(|e| Error::Config(format!("plugin hook {name} returned invalid messages: {e}")))?;
        Ok(Some(list.into_iter().map(|item| (item.info, item.parts)).collect()))
    }

    async fn system_transform(&self, step: &Step<'_>, model: &Model, system: Vec<String>) -> Result<Vec<String>, Error> {
        system_transform(&Hooks { engine: Some(self), ctx: step.ctx }, &step.session.id, model, system).await.map_err(Error::Config)
    }

    async fn chat_hooks(&self, step: &Step<'_>, provider: &Provider, model: &Model, user: Option<&UserMessage>, request: &mut Request) -> Result<(), Error> {
        let hooks = Hooks { engine: Some(self), ctx: step.ctx };
        chat_hooks(&hooks, &step.session.id, &step.agent.name, provider, model, user, request).await.map_err(Error::Config)
    }

    // `experimental.text.complete`: plugins may rewrite the finished text part.
    async fn text_complete(&self, step: &Step<'_>, live: &mut Live<'_>) -> Result<(), Error> {
        let name = HookName::ExperimentalTextComplete;
        if !step.ctx.plugins.as_ref().is_some_and(|p| p.has(name)) {
            return Ok(());
        }
        let Some(part) = &mut live.text else { return Ok(()) };
        let input = json!({"sessionID": part.session_id, "messageID": part.message_id, "partID": part.id});
        let PartBody::Text(body) = &mut part.body else { return Ok(()) };
        let changed = self.hook(step.ctx, name, input, json!({"text": body.text})).await.map_err(Error::Config)?;
        if let Some(text) = changed.get("text").and_then(Value::as_str) {
            body.text = text.to_string();
        }
        Ok(())
    }

    // The read tool also surfaces AGENTS.md-style files found between the file and the instance directory.
    fn nearby_instructions(&self, step: &Step<'_>, assistant: &AssistantMessage, args: &Value, mut output: ToolOutput) -> ToolOutput {
        let Some(file) = args.get("filePath").and_then(Value::as_str) else { return output };
        let path = opencode_tools::util::resolve(&step.ctx.cwd, file);
        if !path.is_file() {
            return output;
        }
        let system = instruction::system_paths(&step.ctx.instruction, &step.ctx.info.instructions).files;
        let mut loaded: HashSet<PathBuf> = HashSet::new();
        for part in step.rows.iter().flat_map(|(_, parts)| parts) {
            if let PartBody::Tool(tool) = &part.body
                && tool.tool == "read"
                && let ToolState::Completed(state) = &tool.state
                && state.time.compacted.is_none()
            {
                loaded.extend(state.metadata.get("loaded").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(PathBuf::from));
            }
        }
        let found = self.claims.resolve(&step.ctx.instruction, &system, &loaded, &path, &assistant.id);
        if found.is_empty() {
            return output;
        }
        if let Some(meta) = output.metadata.as_object_mut() {
            meta.insert("loaded".into(), found.iter().map(|n| n.filepath.to_string_lossy().into_owned()).collect::<Vec<_>>().into());
        }
        if output.attachments.is_empty() {
            let contents: Vec<&str> = found.iter().map(|n| n.content.as_str()).collect();
            output.output.push_str(&format!("\n\n<system-reminder>\n{}\n</system-reminder>", contents.join("\n\n")));
        }
        output
    }

    fn child_prompt<'a>(
        &'a self,
        ctx: Arc<Ctx>,
        session: Session,
        submission: Submission,
        host: Arc<dyn Host>,
        abort: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Session, Error>> + Send + 'a>> {
        Box::pin(self.submit(ctx, session, submission, host, abort))
    }

    // The task tool: run a subagent in a child session and return its last text.
    async fn task(&self, step: &Step<'_>, args: Value, tctx: &ToolContext, live: Option<&Mutex<Part>>) -> Result<ToolOutput, ToolError> {
        let params: TaskParams =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments { tool: "task".into(), detail: e.to_string() })?;
        let ctx = step.ctx;
        if params.background == Some(true) {
            return Err(ToolError::Other(if ctx.flags.experimental_background_subagents {
                "Background subagents are not supported".into()
            } else {
                "Background subagents require OPENCODE_EXPERIMENTAL_BACKGROUND_SUBAGENTS=true".into()
            }));
        }
        let store = |e: opencode_storage::StoreError| ToolError::Other(e.to_string());
        let parent = self.store.get_session(&tctx.session_id).map_err(store)?.ok_or_else(|| ToolError::Other(format!("Session not found: {}", tctx.session_id)))?;
        let mut depth = 0u64;
        let mut current = parent.parent_id.clone();
        while let Some(id) = current {
            depth += 1;
            current = self.store.get_session(&id).map_err(store)?.and_then(|s| s.parent_id);
        }
        let limit = ctx.info.subagent_depth();
        if depth >= limit {
            return Err(ToolError::Other(format!("Subagent depth limit reached ({limit}). Increase \"subagent_depth\" to allow nested subagents.")));
        }
        if !step.bypass {
            tctx.permissions
                .ask(PermissionRequest {
                    permission: "task".into(),
                    patterns: vec![params.subagent_type.clone()],
                    always: vec!["*".into()],
                    metadata: json!({"description": params.description, "subagent_type": params.subagent_type}),
                })
                .await?;
        }
        let next = ctx
            .agents
            .get(&params.subagent_type)
            .ok_or_else(|| ToolError::Other(format!("Unknown agent type: {} is not a valid agent type", params.subagent_type)))?;
        let existing = match &params.task_id {
            Some(id) => self.store.get_session(id).ok().flatten(),
            None => None,
        };
        let child = match existing {
            Some(session) => session,
            None => {
                let session = Session::new(NewSession {
                    project_id: parent.project_id.clone(),
                    directory: parent.directory.clone(),
                    version: crate::VERSION.to_string(),
                    path: parent.path.clone(),
                    parent_id: Some(parent.id.clone()),
                    workspace_id: parent.workspace_id.clone(),
                    title: Some(format!("{} (@{} subagent)", params.description, next.name)),
                    agent: Some(next.name.clone()),
                    permission: Some(tools::subagent_permission(
                        parent.permission.as_deref().unwrap_or_default(),
                        next,
                        &ctx.info.experimental.primary_tools,
                    )),
                    ..Default::default()
                })
                .map_err(|e| ToolError::Other(e.to_string()))?;
                self.store.create_session(&session).map_err(store)?;
                session
            }
        };
        let caller = match self.store.get_message(&tctx.message_id).map_err(store)? {
            Some((Message::Assistant(message), _)) => message,
            _ => return Err(ToolError::Other("Not an assistant message".into())),
        };
        let (model, model_meta) = match &next.model {
            Some(m) => ((m.provider_id.clone(), m.model_id.clone()), json!({"providerID": m.provider_id, "modelID": m.model_id})),
            None => (
                (caller.provider_id.clone(), caller.model_id.clone()),
                json!({"modelID": caller.model_id, "providerID": caller.provider_id}),
            ),
        };
        let metadata = json!({"parentSessionId": tctx.session_id, "sessionId": child.id, "model": model_meta});
        if let Some(live) = live {
            let part = {
                let mut part = live.lock().unwrap_or_else(|e| e.into_inner());
                if let PartBody::Tool(tool) = &mut part.body
                    && let ToolState::Running(state) = &mut tool.state
                {
                    state.title = Some(params.description.clone());
                    state.metadata = Some(object(metadata.clone()));
                }
                part.clone()
            };
            self.store.upsert_part(&part).map_err(store)?;
            step.host.update(Update::Tool(Box::new(part)));
        }

        let submission = Submission {
            agent: Some(next.name.clone()),
            model: Some(model),
            variant: if next.model.is_some() { None } else { caller.variant.clone() },
            parts: self.resolve_prompt_parts(ctx, &params.prompt),
            ..Default::default()
        };
        let host: Arc<dyn Host> = Arc::new(ChildHost { parent: step.host.clone(), session_id: child.id.clone() });
        let result = self.child_prompt(ctx.clone(), child.clone(), submission, host, tctx.abort.child_token()).await;
        if matches!(result, Err(Error::Aborted)) || tctx.abort.is_cancelled() {
            return Err(ToolError::Aborted);
        }
        let failed = |message: &str| ToolError::Other(format!("Subagent failed (task_id: {}): {message}", child.id));
        let rows = self.store.list_messages(&child.id).map_err(store)?;
        let last = rows.iter().rev().find(|(m, _)| m.as_assistant().is_some());
        if let Some(error) = last.and_then(|(m, _)| m.as_assistant()).and_then(|a| a.error.as_ref()) {
            let value = serde_json::to_value(error).unwrap_or_default();
            let message = value.pointer("/data/message").and_then(Value::as_str).or_else(|| value.get("name").and_then(Value::as_str));
            return Err(failed(message.unwrap_or("UnknownError")));
        }
        if let Err(error) = result {
            return Err(failed(&error.to_string()));
        }
        let parts = last.map(|(_, parts)| parts.as_slice()).unwrap_or_default();
        let tool_error = parts.iter().rev().find_map(|p| match &p.body {
            PartBody::Tool(ToolPart { state: ToolState::Error(state), .. }) => Some(state.error.as_str()),
            _ => None,
        });
        if let Some(error) = tool_error {
            return Err(failed(error));
        }
        let text = parts.iter().rev().find_map(|p| match &p.body {
            PartBody::Text(t) => Some(t.text.as_str()),
            _ => None,
        });
        Ok(ToolOutput {
            title: params.description,
            output: tools::task_output(&child.id, text.unwrap_or("")),
            metadata,
            attachments: Vec::new(),
        })
    }

    // A `subtask` part (from a command) runs the task tool on the user's behalf.
    #[allow(clippy::too_many_arguments)]
    async fn subtask(
        &self,
        ctx: &Arc<Ctx>,
        session: &Session,
        rows: &[Row],
        last_user: &UserMessage,
        task: SubtaskPart,
        host: &Arc<dyn Host>,
        abort: &CancellationToken,
        manager: &McpManager,
    ) -> Result<(), Error> {
        let (provider_id, model_id) = match &task.model {
            Some(model) => (model.provider_id.as_str(), model.model_id.as_str()),
            None => (last_user.model.provider_id.as_str(), last_user.model.model_id.as_str()),
        };
        ctx.model(provider_id, model_id)?;
        let mut assistant = zero_assistant(&session.id, &last_user.id, &task.agent, provider_id, model_id, last_user.model.variant.clone(), ctx);
        self.save_assistant(&assistant)?;
        let mut args = json!({"prompt": task.prompt, "description": task.description, "subagent_type": task.agent});
        if let Some(command) = &task.command {
            args["command"] = command.clone().into();
        }
        let state = ToolState::Running(ToolStateRunning { input: object(args.clone()), title: None, metadata: None, time: ToolTimeRunning { start: id::now_ms() } });
        let part = Part::new(
            &session.id,
            &assistant.id,
            PartBody::Tool(ToolPart { call_id: opencode_tools::util::tool_id(), tool: "task".into(), state, metadata: None }),
        );
        self.store.upsert_part(&part)?;
        host.update(Update::Tool(Box::new(part.clone())));
        let agent = ctx.agent(&task.agent)?;

        let session_rules = tools::to_rules(session.permission.as_deref().unwrap_or_default());
        let step = Step {
            ctx,
            session,
            agent,
            rows,
            defs: Vec::new(),
            bypass: true,
            host,
            abort,
            asker: Arc::new(Asker::new(
                host.clone(),
                self.approvals.clone(),
                permission::merge(&[&agent.permission, &session_rules]),
                &session.id,
            )),
            blocked: AtomicBool::new(false),
            mcp: manager.clone(),
            mcp_tools: IndexMap::new(),
            reads: Mutex::default(),
        };
        let call_id = match &part.body {
            PartBody::Tool(tool) => tool.call_id.clone(),
            _ => String::new(),
        };
        let tctx = ToolContext {
            session_id: session.id.clone(),
            message_id: assistant.id.clone(),
            permissions: step.asker.for_call(&assistant.id, &call_id),
            call_id,
            agent: task.agent.clone(),
            cwd: ctx.cwd.clone(),
            worktree: ctx.root.clone(),
            data_dir: self.paths.data.clone(),
            abort: abort.clone(),
            hooks: Some(self.tool_hooks(ctx, Some(host))),
        };
        // The original passes the part id as `callID` here.
        let part_id = part.id.clone();
        let args = self.tool_before(ctx, "task", &session.id, &part_id, args).await.map_err(|e| Error::Config(e.to_string()))?;
        let live = Mutex::new(part);
        let result = match self.task(&step, args.clone(), &tctx, Some(&live)).await {
            Ok(output) => Ok(self.tool_after(ctx, "task", &session.id, &part_id, &args, output).await.map_err(|e| Error::Config(e.to_string()))?),
            Err(error) => Err(error),
        };
        let mut part = live.into_inner().unwrap_or_else(|e| e.into_inner());
        let end = id::now_ms();
        assistant.finish = Some("tool-calls".into());
        assistant.time.completed = Some(end);
        self.save_assistant(&assistant)?;
        let cancelled = abort.is_cancelled();
        if let PartBody::Tool(tool) = &mut part.body {
            let progress = match &tool.state {
                ToolState::Running(s) => s.metadata.clone(),
                _ => None,
            };
            tool.state = match result {
                Ok(output) => tool.state.to_completed(output.output, output.title, object(output.metadata), None, end),
                Err(_) if cancelled => tool.state.to_error("Cancelled", progress, end),
                Err(error) => tool.state.to_error(format!("Tool execution failed: {error}"), progress, end),
            };
        }
        self.store.upsert_part(&part)?;
        host.update(Update::Tool(Box::new(part)));
        if cancelled {
            return Err(Error::Aborted);
        }
        if task.command.is_some() {
            let user = UserMessage {
                id: id::message(),
                session_id: session.id.clone(),
                time: UserTime { created: id::now_ms() },
                format: None,
                summary: None,
                agent: last_user.agent.clone(),
                model: last_user.model.clone(),
                system: None,
                tools: None,
            };
            self.store.upsert_message(&Message::User(user.clone()))?;
            let text = "Summarize the task tool output above and continue with your task.".to_string();
            self.store.upsert_part(&Part::new(&session.id, &user.id, PartBody::Text(TextPart { text, synthetic: Some(true), ..Default::default() })))?;
        }
        Ok(())
    }

    // Summarizes history into a `summary` assistant message. False stops the loop.
    #[allow(clippy::too_many_arguments)]
    async fn compact(
        &self,
        ctx: &Arc<Ctx>,
        session: &Session,
        rows: &[Row],
        parent: &UserMessage,
        (auto, overflow): (bool, bool),
        host: &Arc<dyn Host>,
        abort: &CancellationToken,
        manager: &McpManager,
    ) -> Result<bool, Error> {
        let real = |(m, parts): &Row| m.as_user().is_some() && !history::has_compaction(parts);
        let position = rows.iter().rposition(|(m, _)| m.id() == parent.id);
        let marker = position.and_then(|i| rows[i].1.iter().find(|p| matches!(p.body, PartBody::Compaction(_))).cloned());
        let mut messages = rows;
        let mut replay: Option<&Row> = None;
        if overflow
            && let Some(index) = position
            && let Some(at) = rows[..index].iter().rposition(real)
            && rows[..at].iter().any(real)
        {
            replay = Some(&rows[at]);
            messages = &rows[..at];
        }

        let agent = ctx.agent("compaction")?;
        let (provider, model) = match &agent.model {
            Some(m) => ctx.model(&m.provider_id, &m.model_id)?,
            None => ctx.model(&parent.model.provider_id, &parent.model.model_id)?,
        };
        let trailing = marker.is_some() && messages.last().is_some_and(|(m, _)| m.id() == parent.id);
        let history = if trailing { &messages[..messages.len() - 1] } else { messages };
        let (kept, previous) = compaction::hide_completed(history.to_vec());
        let (head, tail_start) = compaction::select(&kept, &ctx.info, model);
        // Plugins may add context to the compaction prompt or replace it.
        let compacting = self
            .hook(ctx, HookName::ExperimentalSessionCompacting, json!({"sessionID": session.id}), json!({"context": []}))
            .await
            .map_err(Error::Config)?;
        let transformed = self.transform_messages(ctx, &kept[..head]).await?;
        let selected: &[Row] = transformed.as_deref().unwrap_or(&kept[..head]);
        let prompt = match compacting.get("prompt").and_then(Value::as_str) {
            Some(prompt) => prompt.to_string(),
            None => {
                // Rust build: v2's request, as one message: the previous checkpoint and the conversation since, shortened
                // in stages to fit the compaction model, then the summary prompt (an update when a summary exists).
                let extra: Vec<&str> =
                    compacting.get("context").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
                let legacy = previous.as_deref().is_some_and(|s| s.contains(compaction::LEGACY_HEADING));
                let built = compaction::build_prompt(previous.is_some(), legacy);
                let system = agent.prompt.as_deref().unwrap_or_default();
                let overhead = [built.as_str(), system].iter().chain(&extra).map(|s| usage::estimate(s)).sum::<u64>();
                let target = compaction::budget(&ctx.info, model).saturating_sub(overhead);
                let conversation = compaction::transcript(selected, previous.as_deref(), target);
                std::iter::once(conversation.as_str()).chain(std::iter::once(built.as_str())).chain(extra).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n\n")
            }
        };

        let mut assistant =
            zero_assistant(&session.id, &parent.id, "compaction", &provider.id, &model.id, parent.model.variant.clone(), ctx);
        assistant.summary = Some(true);
        self.save_assistant(&assistant)?;
        let step = Step {
            ctx,
            session,
            agent,
            rows,
            defs: Vec::new(),
            bypass: false,
            host,
            abort,
            asker: Arc::new(Asker::new(host.clone(), self.approvals.clone(), agent.permission.clone(), &session.id)),
            blocked: AtomicBool::new(false),
            mcp: manager.clone(),
            mcp_tools: IndexMap::new(),
            reads: Mutex::default(),
        };
        let call =
            Call { provider, model, system: Vec::new(), user_system: parent.system.as_deref(), messages: vec![llm::Message::User(prompt)], user: Some(parent) };
        let outcome = self.process(&step, call, &mut assistant).await?;
        match outcome {
            Outcome::Compact => {
                let message = if replay.is_some() { compaction::OVERFLOW_REPLAY } else { compaction::OVERFLOW_MEDIA };
                let error = AssistantError::ContextOverflowError { message: message.to_string(), response_body: None };
                assistant.error = Some(error.clone());
                assistant.finish = Some("error".into());
                self.save_assistant(&assistant)?;
                report(host, &session.id, error);
                return Ok(false);
            }
            Outcome::Failed(error) => return Err(error.into()),
            Outcome::Stop => return Ok(false),
            Outcome::Continue => {}
        }

        if let Some(mut part) = marker
            && let Some(start) = tail_start
            && let PartBody::Compaction(body) = &mut part.body
            && body.tail_start_id.as_deref() != Some(start.as_str())
        {
            body.tail_start_id = Some(start);
            self.store.upsert_part(&part)?;
        }

        if auto {
            let user = |source: &UserMessage, full: bool| UserMessage {
                id: id::message(),
                session_id: session.id.clone(),
                time: UserTime { created: id::now_ms() },
                format: source.format.clone().filter(|_| full),
                summary: None,
                agent: source.agent.clone(),
                model: source.model.clone(),
                system: source.system.clone().filter(|_| full),
                tools: source.tools.clone().filter(|_| full),
            };
            match replay {
                Some((Message::User(original), parts)) => {
                    let message = user(original, true);
                    self.store.upsert_message(&Message::User(message.clone()))?;
                    for part in parts {
                        let body = match &part.body {
                            PartBody::Compaction(_) => continue,
                            PartBody::File(file) if history::is_media(&file.mime) => PartBody::Text(TextPart {
                                text: format!("[Attached {}: {}]", file.mime, file.filename.as_deref().unwrap_or("file")),
                                ..Default::default()
                            }),
                            other => other.clone(),
                        };
                        self.store.upsert_part(&Part::new(&session.id, &message.id, body))?;
                    }
                }
                _ if !self.autocontinue(ctx, session, parent, overflow).await? => {}
                _ => {
                    let message = user(parent, false);
                    self.store.upsert_message(&Message::User(message.clone()))?;
                    let now = id::now_ms();
                    let text = format!("{}{}", if overflow { compaction::CONTINUE_OVERFLOW } else { "" }, compaction::CONTINUE);
                    self.store.upsert_part(&Part::new(
                        &session.id,
                        &message.id,
                        PartBody::Text(TextPart {
                            text,
                            synthetic: Some(true),
                            ignored: None,
                            time: Some(PartTime { start: now, end: Some(now) }),
                            metadata: Some(object(json!({"compaction_continue": true}))),
                        }),
                    ))?;
                }
            }
        }
        host.update(Update::Compacted { session_id: session.id.clone() });
        Ok(true)
    }

    // `experimental.compaction.autocontinue`: plugins may veto the synthetic "continue" message.
    async fn autocontinue(&self, ctx: &Ctx, session: &Session, parent: &UserMessage, overflow: bool) -> Result<bool, Error> {
        let name = HookName::ExperimentalCompactionAutocontinue;
        if !ctx.plugins.as_ref().is_some_and(|p| p.has(name)) {
            return Ok(true);
        }
        let (provider, model) = ctx.model(&parent.model.provider_id, &parent.model.model_id)?;
        let provider = plugins::provider_json(provider);
        let input = json!({
            "sessionID": session.id,
            "agent": parent.agent,
            "model": plugins::model_json(model),
            "provider": provider,
            "message": Message::User(parent.clone()),
            "overflow": overflow,
        });
        let changed = self.hook(ctx, name, input, json!({"enabled": true})).await.map_err(Error::Config)?;
        Ok(changed.get("enabled").and_then(Value::as_bool).unwrap_or(false))
    }

    // Clears old tool outputs once enough newer output protects the recent context.
    fn prune(&self, ctx: &Ctx, session_id: &str) -> Result<(), Error> {
        if !ctx.info.compaction.prune_enabled() {
            return Ok(());
        }
        let now = id::now_ms();
        for mut part in compaction::prunable(&self.store.list_messages(session_id)?) {
            if let PartBody::Tool(tool) = &mut part.body
                && let ToolState::Completed(state) = &mut tool.state
            {
                state.time.compacted = Some(now);
                self.store.upsert_part(&part)?;
            }
        }
        Ok(())
    }

    // Names a fresh session after its first real user message, in the background.
    fn spawn_title(&self, ctx: &Arc<Ctx>, session: &Session, rows: &[Row], user: &UserMessage, host: &Arc<dyn Host>) {
        if session.parent_id.is_some() || !session.has_default_title() {
            return;
        }
        let real = |(m, parts): &Row| {
            m.as_user().is_some() && !parts.iter().all(|p| matches!(&p.body, PartBody::Text(t) if t.synthetic == Some(true)))
        };
        let Some(index) = rows.iter().position(real) else { return };
        if rows.iter().filter(|row| real(row)).count() != 1 {
            return;
        }
        let context: Vec<Row> = rows[..=index].to_vec();
        let Some(first) = context[index].0.as_user().cloned() else { return };
        let (ctx, store, http, host) = (ctx.clone(), self.store.clone(), self.http.clone(), host.clone());
        let (session_id, project_id) = (session.id.clone(), session.project_id.clone());
        let (provider_id, model_id) = (user.model.provider_id.clone(), user.model.model_id.clone());
        let handle = tokio::spawn(async move {
            let Some(agent) = ctx.agents.get("title") else { return };
            let chosen = match &agent.model {
                Some(m) => ctx.model(&m.provider_id, &m.model_id).ok(),
                None => {
                    // `Provider.getSmallModel`: config first, then the plugin hook, then the built-in priorities.
                    let hinted = match ctx.info.small_model_ref() {
                        Some(_) => None,
                        None => plugins::small_model(&ctx, &provider_id).await,
                    };
                    let hinted = hinted.as_ref().and_then(|(provider, model)| ctx.model(provider, model).ok());
                    hinted.or_else(|| ctx.small_model(&provider_id)).or_else(|| ctx.model(&provider_id, &model_id).ok())
                }
            };
            let Some((provider, model)) = chosen else { return };
            let subtasks: Vec<&str> = context[index]
                .1
                .iter()
                .filter_map(|p| match &p.body {
                    PartBody::Subtask(s) => Some(s.prompt.as_str()),
                    _ => None,
                })
                .collect();
            let mut messages = vec![llm::Message::User("Generate a title for this conversation:\n".into())];
            if !subtasks.is_empty() && subtasks.len() == context[index].1.len() {
                messages.push(llm::Message::User(subtasks.join("\n")));
            } else {
                messages.extend(history::to_model_messages(&context, model));
            }
            let system: Vec<&str> = [agent.prompt.as_deref(), first.system.as_deref()].into_iter().flatten().filter(|s| !s.is_empty()).collect();
            // The title request goes through the same request preparation as the loop, plugin hooks included.
            let hooks = Hooks { engine: None, ctx: &ctx };
            let Ok(system) = system_transform(&hooks, &session_id, model, vec![system.join("\n")]).await else { return };
            let mut options = small_options(model);
            merge_deep(&mut options, model.options.clone());
            merge_deep(&mut options, Value::Object(agent.options.clone()));
            let mut request = Request {
                model: model.api_id.clone(),
                system,
                messages,
                temperature: agent.temperature,
                top_p: agent.top_p,
                options,
                ..Default::default()
            };
            let meta = shape::Session { session_id: &session_id, parent_session_id: None, request_id: Some(&first.id), project_id: Some(&project_id) };
            shape::apply(&mut request, provider, model, &meta);
            if chat_hooks(&hooks, &session_id, &agent.name, provider, model, Some(&first), &mut request).await.is_err() {
                return;
            }
            let loader = plugins::PluginAuth::new(ctx.plugins.as_ref());
            let mut attempt = 0;
            let text = loop {
                let mut text = String::new();
                let result = client::stream_via(&http, &loader, provider, model, &request, |event| {
                    if let Event::TextDelta(delta) = event {
                        text.push_str(&delta);
                    }
                })
                .await;
                match result {
                    Ok(_) => break text,
                    Err(error) if attempt < TITLE_RETRIES && retry::classify(&error, &provider.id).retryable => attempt += 1,
                    Err(_) => return,
                }
            };
            let Some(title) = clean_title(&text) else { return };
            if store.patch_session(&session_id, |s| s.title = title.clone()).is_ok() {
                host.update(Update::Title { session_id, title });
            }
        });
        self.background.lock().unwrap_or_else(|e| e.into_inner()).push(handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_cleanup() {
        assert_eq!(clean_title("<think>a\nb</think>  \n\n  Fix bug  \nmore").as_deref(), Some("Fix bug"));
        assert_eq!(clean_title("\n \n").as_deref(), None);
        assert_eq!(clean_title("<think>open").as_deref(), Some("<think>open"));
        let long = "t".repeat(101);
        assert_eq!(clean_title(&long), Some(format!("{}...", "t".repeat(97))));
        assert_eq!(clean_title(&"t".repeat(100)).map(|t| t.len()), Some(100));
    }
}
