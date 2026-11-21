// Amazon Bedrock: Converse streaming body, event decoding, credential resolution and cross-region model ids.
use crate::sigv4::Credentials;
use crate::{Completion, Error, Event, Message, Part, Reasoning, Request, ToolCall, ToolChoice, Usage, chat, eventstream, number};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const PROVIDER_ID: &str = "amazon-bedrock";
pub const NPM: &str = "@ai-sdk/amazon-bedrock";
pub const MANTLE_NPM: &str = "@ai-sdk/amazon-bedrock/mantle";
pub const SERVICE: &str = "bedrock";
pub const MANTLE_SERVICE: &str = "bedrock-mantle";
pub const DEFAULT_REGION: &str = "us-east-1";
pub const BEARER_ENV: &str = "AWS_BEARER_TOKEN_BEDROCK";
pub const BLOCK_BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";
const CROSS_REGION_PREFIXES: [&str; 6] = ["global.", "us.", "eu.", "jp.", "apac.", "au."];

pub fn base_url(region: &str) -> String {
    format!("https://bedrock-runtime.{region}.amazonaws.com")
}

pub fn mantle_base_url(region: &str) -> String {
    format!("https://bedrock-mantle.{region}.api.aws/v1")
}

// encodeURIComponent
pub(crate) fn encode_component(input: &str) -> String {
    let mut out = String::new();
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub fn path(model_id: &str) -> String {
    format!("/model/{}/converse-stream", encode_component(model_id))
}

// The inference-profile id a model is called under in `region`.
pub fn model_id(api_id: &str, region: &str) -> String {
    if api_id.starts_with("arn:") || CROSS_REGION_PREFIXES.iter().any(|p| api_id.starts_with(p)) {
        return api_id.to_string();
    }
    let has = |names: &[&str]| names.iter().any(|n| api_id.contains(n));
    let prefix = match region.split('-').next().unwrap_or_default() {
        "us" => (has(&["nova-micro", "nova-lite", "nova-pro", "nova-premier", "nova-2", "claude", "deepseek.r1"]) && !region.starts_with("us-gov")).then_some("us"),
        "eu" => {
            let regional = ["eu-west-1", "eu-west-2", "eu-west-3", "eu-north-1", "eu-central-1", "eu-south-1", "eu-south-2"].iter().any(|r| region.contains(r));
            (regional && has(&["claude", "nova-lite", "nova-micro", "llama3", "pixtral"])).then_some("eu")
        }
        "ap" => {
            if ["ap-southeast-2", "ap-southeast-4"].contains(&region) && has(&["anthropic.claude-sonnet-4-5", "anthropic.claude-haiku"]) {
                Some("au")
            } else if has(&["claude", "nova-lite", "nova-micro", "nova-pro"]) {
                Some(if region == "ap-northeast-1" { "jp" } else { "apac" })
            } else {
                None
            }
        }
        _ => None,
    };
    match prefix {
        Some(prefix) => format!("{prefix}.{api_id}"),
        None => api_id.to_string(),
    }
}

// Mantle serves the Responses API, except for the safeguard models which only have chat.
pub fn mantle_uses_chat(api_id: &str) -> bool {
    api_id.starts_with("openai.gpt-oss-safeguard")
}

#[derive(Debug, Clone, PartialEq)]
pub enum Auth {
    Bearer(String),
    SigV4(Credentials),
}

// Where a request finds its AWS identity; resolved again on every request.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub region: String,
    pub profile: Option<String>,
    // Bearer token from config `apiKey` or a stored api key; `AWS_BEARER_TOKEN_BEDROCK` still wins.
    pub bearer: Option<String>,
    pub service: &'static str,
    // Fixed signing time (seconds), for reproducible signatures.
    pub time: Option<u64>,
}

impl Config {
    pub fn new(region: impl Into<String>) -> Self {
        Config { region: region.into(), profile: None, bearer: None, service: SERVICE, time: None }
    }
}

// Environment and home directory the credential chain reads; injectable for tests.
pub struct Source<'a> {
    pub var: &'a dyn Fn(&str) -> Option<String>,
    pub home: PathBuf,
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

impl Source<'static> {
    pub fn process() -> Self {
        Source { var: &env_var, home: opencode_core::paths::Paths::from_env().home }
    }
}

// Whether any credential source the provider loader looks at is present.
pub fn detected(source: &Source, profile: Option<&str>, stored_key: bool, config_key: bool) -> bool {
    let var = |name: &str| (source.var)(name).is_some();
    profile.is_some()
        || var("AWS_PROFILE")
        || var("AWS_ACCESS_KEY_ID")
        || var(BEARER_ENV)
        || stored_key
        || config_key
        || var("AWS_WEB_IDENTITY_TOKEN_FILE")
        || var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI")
        || var("AWS_CONTAINER_CREDENTIALS_FULL_URI")
}

fn ini_section(text: &str, names: &[String]) -> Option<BTreeMap<String, String>> {
    let mut found: Option<BTreeMap<String, String>> = None;
    let mut inside = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[').and_then(|l| l.split(']').next()) {
            inside = names.iter().any(|n| n == header.trim());
            if inside {
                found.get_or_insert_with(BTreeMap::new);
            }
            continue;
        }
        if let (true, Some((key, value))) = (inside, line.split_once('=')) {
            found.get_or_insert_with(BTreeMap::new).insert(key.trim().to_lowercase(), value.trim().to_string());
        }
    }
    found
}

fn keys(section: &BTreeMap<String, String>) -> Option<Credentials> {
    Some(Credentials {
        access_key_id: section.get("aws_access_key_id").filter(|v| !v.is_empty())?.clone(),
        secret_access_key: section.get("aws_secret_access_key").filter(|v| !v.is_empty())?.clone(),
        session_token: section.get("aws_session_token").filter(|v| !v.is_empty()).cloned(),
    })
}

fn run_process(command: &str) -> Result<Credentials, Error> {
    let fail = |why: String| Error::Config(format!("AWS credential_process failed: {why}"));
    let output = if cfg!(windows) { std::process::Command::new("cmd").args(["/C", command]).output() } else { std::process::Command::new("sh").args(["-c", command]).output() };
    let output = output.map_err(|e| fail(e.to_string()))?;
    if !output.status.success() {
        return Err(fail(format!("exit status {}", output.status)));
    }
    let data: Value = serde_json::from_slice(&output.stdout).map_err(|e| fail(e.to_string()))?;
    let field = |key: &str| data.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    Ok(Credentials {
        access_key_id: field("AccessKeyId").ok_or_else(|| fail("no AccessKeyId".into()))?,
        secret_access_key: field("SecretAccessKey").ok_or_else(|| fail("no SecretAccessKey".into()))?,
        session_token: field("SessionToken"),
    })
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

// Bearer token first, then the node credential chain: environment keys (skipped when a profile is named),
// shared credentials/config files, `credential_process`.
pub fn resolve(config: &Config, source: &Source) -> Result<Auth, Error> {
    let var = |name: &str| (source.var)(name);
    if let Some(token) = var(BEARER_ENV).or_else(|| config.bearer.clone()).map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) {
        return Ok(Auth::Bearer(token));
    }
    let profile = config.profile.clone().or_else(|| var("AWS_PROFILE"));
    if profile.is_none()
        && let (Some(access_key_id), Some(secret_access_key)) = (var("AWS_ACCESS_KEY_ID"), var("AWS_SECRET_ACCESS_KEY"))
    {
        return Ok(Auth::SigV4(Credentials { access_key_id, secret_access_key, session_token: var("AWS_SESSION_TOKEN") }));
    }
    let name = profile.clone().unwrap_or_else(|| "default".to_string());
    let credentials_file = var("AWS_SHARED_CREDENTIALS_FILE").map(PathBuf::from).unwrap_or_else(|| source.home.join(".aws").join("credentials"));
    let config_file = var("AWS_CONFIG_FILE").map(PathBuf::from).unwrap_or_else(|| source.home.join(".aws").join("config"));
    let shared = ini_section(&read(&credentials_file), std::slice::from_ref(&name));
    let configured = ini_section(&read(&config_file), &[format!("profile {name}"), name.clone()]);
    if let Some(found) = shared.as_ref().and_then(keys).or_else(|| configured.as_ref().and_then(keys)) {
        return Ok(Auth::SigV4(found));
    }
    let setting = |key: &str| configured.as_ref().and_then(|s| s.get(key)).or_else(|| shared.as_ref().and_then(|s| s.get(key)));
    if let Some(command) = setting("credential_process") {
        return run_process(command).map(Auth::SigV4);
    }
    let unsupported = [
        (setting("sso_session").or(setting("sso_start_url")).is_some(), "AWS SSO profiles"),
        (setting("role_arn").is_some(), "assume-role profiles"),
        (var("AWS_WEB_IDENTITY_TOKEN_FILE").is_some(), "web identity token files"),
        (var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").or(var("AWS_CONTAINER_CREDENTIALS_FULL_URI")).is_some(), "container credentials"),
    ];
    if let Some((_, what)) = unsupported.iter().find(|(present, _)| *present) {
        return Err(Error::Config(format!("not supported yet: {what} for amazon-bedrock (use access keys, a static-key profile, credential_process or {BEARER_ENV})")));
    }
    Err(match profile {
        Some(profile) => Error::Config(format!("Could not resolve AWS credentials for profile {profile}")),
        None => Error::Config("Could not load AWS credentials from any provider (AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY, shared credentials file, credential_process)".into()),
    })
}

fn anthropic(model: &str) -> bool {
    model.contains("anthropic")
}

fn cache_point() -> Value {
    json!({"cachePoint": {"type": "default"}})
}

fn image_format(mime: &str) -> Option<&'static str> {
    Some(match mime {
        "image/jpeg" | "image/jpg" | "image/*" => "jpeg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => return None,
    })
}

fn document_format(mime: &str) -> Option<&'static str> {
    Some(match mime {
        "application/pdf" => "pdf",
        "text/csv" => "csv",
        "application/msword" => "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.ms-excel" => "xls",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "text/html" => "html",
        "text/plain" => "txt",
        "text/markdown" => "md",
        _ => return None,
    })
}

fn media(part: &Part, documents: &mut usize) -> Result<Value, Error> {
    let (mime, data, filename) = match part {
        Part::Text(text) => return Ok(json!({"text": text})),
        Part::Media { mime, data, filename } => (mime.as_str(), data, filename),
    };
    if let Some(format) = image_format(mime) {
        return Ok(json!({"image": {"format": format, "source": {"bytes": data}}}));
    }
    let Some(format) = document_format(mime) else {
        return Err(Error::Config(format!("'media type: {mime}' functionality not supported.")));
    };
    *documents += 1;
    let stem = filename.as_deref().map(|name| name.rsplit_once('.').map_or(name, |(stem, _)| stem)).filter(|s| !s.is_empty());
    let name = stem.map(str::to_string).unwrap_or_else(|| format!("document-{documents}"));
    Ok(json!({"document": {"format": format, "name": name, "source": {"bytes": data}}}))
}

fn tool_name(name: &str) -> String {
    let clean: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
    if clean.is_empty() { "_".into() } else { clean }
}

fn tool_use(call: &ToolCall) -> Value {
    let raw = if call.arguments.trim().is_empty() { "{}" } else { &call.arguments };
    let input = match serde_json::from_str::<Value>(raw) {
        Ok(value @ Value::Object(_)) => value,
        Ok(other) => json!({"rawInvalidInput": other}),
        Err(_) => json!({"rawInvalidInput": call.arguments}),
    };
    json!({"toolUse": {"toolUseId": call.id, "name": tool_name(&call.name), "input": input}})
}

fn reasoning(part: &Reasoning) -> Option<Value> {
    if let Some(signature) = &part.signature {
        return Some(json!({"reasoningContent": {"reasoningText": {"text": part.text, "signature": signature}}}));
    }
    part.encrypted.as_ref().map(|data| json!({"reasoningContent": {"redactedReasoning": {"data": data}}}))
}

fn only_cache_points(content: &[Value]) -> bool {
    content.iter().all(|block| block.get("cachePoint").is_some())
}

fn messages(req: &Request, sends_tools: bool) -> Result<Vec<Value>, Error> {
    let targets = if req.cache { chat::cache_targets(&req.messages) } else { Vec::new() };
    let mut documents = 0;
    let mut out: Vec<(bool, Vec<Value>)> = Vec::new();
    for (i, message) in req.messages.iter().enumerate() {
        let (assistant, mut content) = match message {
            Message::User(text) => (false, vec![json!({"text": text})]),
            Message::UserParts(parts) => (false, parts.iter().map(|p| media(p, &mut documents)).collect::<Result<Vec<_>, _>>()?),
            Message::Assistant { text, tool_calls, reasoning_parts, .. } => {
                let mut content: Vec<Value> = reasoning_parts.iter().filter_map(reasoning).collect();
                if !text.trim().is_empty() || (!text.is_empty() && !content.is_empty()) {
                    content.push(json!({"text": text}));
                }
                if sends_tools {
                    content.extend(tool_calls.iter().map(tool_use));
                }
                (true, content)
            }
            Message::Tool { call_id, content, attachments, .. } => {
                if !sends_tools {
                    continue;
                }
                let mut items = vec![json!({"text": content})];
                for part in attachments {
                    items.push(media(part, &mut documents)?);
                }
                (false, vec![json!({"toolResult": {"toolUseId": call_id, "content": items}})])
            }
        };
        if targets.contains(&i) {
            content.push(cache_point());
        }
        match out.last_mut() {
            Some((role, existing)) if *role == assistant => existing.extend(content),
            _ => out.push((assistant, content)),
        }
    }
    // The model refuses trailing whitespace on the final turn.
    if let Some(Value::String(text)) = out.last_mut().and_then(|(_, content)| content.iter_mut().rev().find_map(|b| b.get_mut("text"))) {
        *text = text.trim().to_string();
    }
    Ok(out
        .into_iter()
        .filter(|(assistant, content)| !(content.is_empty() || (*assistant && only_cache_points(content))))
        .map(|(assistant, content)| json!({"role": if assistant { "assistant" } else { "user" }, "content": content}))
        .collect())
}

// Options the body builder consumes; every other key is spread into the request as-is.
const CONSUMED: [&str; 4] = ["reasoningConfig", "additionalModelRequestFields", "serviceTier", "anthropicBeta"];

pub fn encode(req: &Request) -> Result<Value, Error> {
    let claude = anthropic(&req.model);
    let sends_tools = !req.tools.is_empty() && req.tool_choice != Some(ToolChoice::None);
    let mut body = Map::new();
    if !req.system.is_empty() {
        let mut system = Vec::new();
        for (i, text) in req.system.iter().enumerate() {
            system.push(json!({"text": text}));
            if req.cache && i < 2 {
                system.push(cache_point());
            }
        }
        body.insert("system".into(), system.into());
    }
    body.insert("messages".into(), messages(req, sends_tools)?.into());

    let config = req.options.get("reasoningConfig").filter(|v| v.is_object());
    let kind = config.and_then(|c| c.get("type")).and_then(Value::as_str);
    let budget = config.and_then(|c| c.get("budgetTokens")).and_then(Value::as_u64);
    let effort = config.and_then(|c| c.get("maxReasoningEffort")).and_then(Value::as_str);
    let thinking = claude && matches!(kind, Some("enabled" | "adaptive"));
    let mut extra = req.options.get("additionalModelRequestFields").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut betas: Vec<Value> = req.options.get("anthropicBeta").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut max_tokens = req.max_tokens;
    if thinking {
        let mut block = Map::new();
        block.insert("type".into(), kind.into());
        match (kind, budget) {
            (Some("enabled"), Some(budget)) => {
                max_tokens = Some(max_tokens.map_or(budget + 4096, |m| m + budget));
                block.insert("budget_tokens".into(), budget.into());
            }
            (Some("adaptive"), _) => {
                if let Some(display) = config.and_then(|c| c.get("display")).filter(|v| v.is_string()) {
                    block.insert("display".into(), display.clone());
                }
            }
            _ => {}
        }
        if let Some(behavior) = config.and_then(|c| c.pointer("/blockBinding/prefixMismatchBehavior")).filter(|v| v.is_string()) {
            block.insert("block_binding".into(), json!({"prefix_mismatch_behavior": behavior}));
            betas.push(BLOCK_BINDING_BETA.into());
        }
        if block.len() > 1 || kind == Some("adaptive") {
            extra.insert("thinking".into(), Value::Object(block));
        }
    }
    if let Some(effort) = effort {
        if claude {
            let mut output = extra.get("output_config").and_then(Value::as_object).cloned().unwrap_or_default();
            output.insert("effort".into(), effort.into());
            extra.insert("output_config".into(), Value::Object(output));
        } else if req.model.contains("openai") {
            if req.model.contains("gpt-oss-") {
                extra.insert("reasoning_effort".into(), effort.into());
            } else {
                extra.insert("reasoning".into(), json!({"effort": effort}));
            }
        } else {
            let mut native = Map::new();
            if let Some(kind) = kind {
                native.insert("type".into(), kind.into());
            }
            if let Some(budget) = budget {
                native.insert("budgetTokens".into(), budget.into());
            }
            native.insert("maxReasoningEffort".into(), effort.into());
            extra.insert("reasoningConfig".into(), Value::Object(native));
        }
    }
    if !betas.is_empty() {
        extra.insert("anthropic_beta".into(), betas.into());
    }
    if !extra.is_empty() {
        body.insert("additionalModelRequestFields".into(), Value::Object(extra));
    }
    if claude {
        body.insert("additionalModelResponseFieldPaths".into(), json!(["/delta/stop_sequence"]));
    }

    let mut inference = Map::new();
    if let Some(max) = max_tokens {
        inference.insert("maxTokens".into(), max.into());
    }
    if !thinking {
        if let Some(v) = req.temperature {
            inference.insert("temperature".into(), number(v.clamp(0.0, 1.0)));
        }
        if let Some(v) = req.top_p {
            inference.insert("topP".into(), number(v));
        }
        if let Some(v) = req.top_k {
            inference.insert("topK".into(), v.into());
        }
    }
    if !inference.is_empty() {
        body.insert("inferenceConfig".into(), Value::Object(inference));
    }
    if let Some(tier) = req.options.get("serviceTier").filter(|v| v.is_string()) {
        body.insert("serviceTier".into(), json!({"type": tier}));
    }
    for (key, value) in req.options.as_object().into_iter().flatten() {
        if !CONSUMED.contains(&key.as_str()) {
            body.insert(key.clone(), value.clone());
        }
    }
    if sends_tools {
        let forced = match &req.tool_choice {
            Some(ToolChoice::Tool(name)) => Some(name.as_str()),
            _ => None,
        };
        let tools: Vec<Value> = req
            .tools
            .iter()
            .filter(|t| forced.is_none_or(|name| t.name == name))
            .map(|t| {
                let mut spec = Map::new();
                spec.insert("name".into(), t.name.clone().into());
                if !t.description.trim().is_empty() {
                    spec.insert("description".into(), t.description.clone().into());
                }
                spec.insert("inputSchema".into(), json!({"json": t.parameters}));
                json!({"toolSpec": spec})
            })
            .collect();
        let mut config = Map::new();
        config.insert("tools".into(), tools.into());
        match &req.tool_choice {
            Some(ToolChoice::Auto) => config.insert("toolChoice".into(), json!({"auto": {}})),
            Some(ToolChoice::Required) => config.insert("toolChoice".into(), json!({"any": {}})),
            Some(ToolChoice::Tool(name)) => config.insert("toolChoice".into(), json!({"tool": {"name": name}})),
            Some(ToolChoice::None) | None => None,
        };
        body.insert("toolConfig".into(), Value::Object(config));
    }
    Ok(Value::Object(body))
}

enum Block {
    Tool(ToolCall),
    Thinking(Reasoning),
}

#[derive(Default)]
pub struct Decoder {
    blocks: BTreeMap<u64, Block>,
    reason: Option<String>,
    usage: Option<Usage>,
    reasoning: Vec<Reasoning>,
}

fn finish_reason(reason: &str) -> &'static str {
    match reason {
        "end_turn" | "stop_sequence" => "stop",
        "max_tokens" => "length",
        "tool_use" => "tool-calls",
        "content_filtered" | "guardrail_intervened" => "content-filter",
        _ => "other",
    }
}

// `inputTokens` excludes cached tokens; the crate's `input` includes them.
fn usage(raw: &Value) -> Usage {
    let count = |key: &str| raw.get(key).and_then(Value::as_u64).unwrap_or(0);
    let (cache_read, cache_write, output) = (count("cacheReadInputTokens"), count("cacheWriteInputTokens"), count("outputTokens"));
    let input = count("inputTokens") + cache_read + cache_write;
    Usage { input, output, reasoning: 0, cache_read, cache_write, total: input + output }
}

impl Decoder {
    pub fn push(&mut self, frame: &eventstream::Frame) -> Result<Vec<Event>, Error> {
        let text = String::from_utf8_lossy(&frame.payload);
        let data: Value = if text.is_empty() { Value::Null } else { serde_json::from_str(&text).map_err(|_| Error::Stream("Failed to parse Bedrock Converse event-stream payload".into()))? };
        let kind = match frame.header(":message-type") {
            Some("event") => frame.header(":event-type"),
            Some("exception") => {
                let code = frame.header(":exception-type").unwrap_or("exception").to_string();
                let message = data.get("message").or_else(|| data.get("Message")).and_then(Value::as_str).unwrap_or(&code).to_string();
                return Err(Error::Provider { code: Some(code), message });
            }
            _ => None,
        };
        let index = data.get("contentBlockIndex").and_then(Value::as_u64).unwrap_or(0);
        let mut events = Vec::new();
        match kind {
            Some("contentBlockStart") => {
                if let Some(tool) = data.pointer("/start/toolUse") {
                    let field = |key: &str| tool.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
                    self.blocks.insert(index, Block::Tool(ToolCall { id: field("toolUseId"), name: field("name"), arguments: String::new() }));
                }
            }
            Some("contentBlockDelta") => {
                let delta = &data["delta"];
                if let Some(text) = delta.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                    events.push(Event::TextDelta(text.to_string()));
                }
                if let Some(input) = delta.pointer("/toolUse/input").and_then(Value::as_str)
                    && let Some(Block::Tool(call)) = self.blocks.get_mut(&index)
                {
                    call.arguments.push_str(input);
                }
                if let Some(thinking) = delta.get("reasoningContent") {
                    let Block::Thinking(part) = self.blocks.entry(index).or_insert_with(|| Block::Thinking(Reasoning::default())) else { return Ok(events) };
                    let field = |key: &str| thinking.get(key).and_then(Value::as_str).filter(|s| !s.is_empty());
                    if let Some(text) = field("text") {
                        part.text.push_str(text);
                        events.push(Event::ReasoningDelta(text.to_string()));
                    }
                    if let Some(signature) = field("signature") {
                        part.signature = Some(signature.to_string());
                    }
                    if let Some(data) = field("data").or_else(|| field("redactedContent")) {
                        part.encrypted = Some(data.to_string());
                    }
                }
            }
            Some("contentBlockStop") => match self.blocks.remove(&index) {
                Some(Block::Tool(mut call)) => {
                    if call.arguments.trim().is_empty() {
                        call.arguments = "{}".into();
                    }
                    events.push(Event::ToolCall(call));
                }
                Some(Block::Thinking(part)) => self.reasoning.push(part),
                None => {}
            },
            Some("messageStop") => self.reason = Some(finish_reason(data.get("stopReason").and_then(Value::as_str).unwrap_or_default()).to_string()),
            Some("metadata") => self.usage = data.get("usage").map(usage),
            _ => {}
        }
        Ok(events)
    }

    pub fn finish(mut self) -> Result<(Vec<Event>, Completion), Error> {
        let Some(reason) = self.reason.take() else {
            return Err(Error::Stream("Bedrock Converse stream ended without messageStop".into()));
        };
        for (_, block) in std::mem::take(&mut self.blocks) {
            if let Block::Thinking(part) = block {
                self.reasoning.push(part);
            }
        }
        Ok((vec![Event::Finish { reason, usage: self.usage.unwrap_or_default() }], Completion { reasoning: self.reasoning, ..Default::default() }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tool;
    use crate::testutil::{cassette, request_body};
    use base64::Engine;

    const DIR: &str = "llm/test/fixtures/recordings/bedrock-converse";

    fn weather() -> Tool {
        Tool {
            name: "get_weather".into(),
            description: "Get current weather for a city.".into(),
            parameters: json!({"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"], "additionalProperties": false}),
        }
    }

    // The recordings come from the native runtime, which repeats the model id in the body and sends structured tool results.
    fn recorded(c: &Value, index: usize) -> Value {
        let mut body = request_body(c, index);
        body.as_object_mut().unwrap().remove("modelId");
        for message in body["messages"].as_array_mut().unwrap() {
            for block in message["content"].as_array_mut().unwrap() {
                if let Some(result) = block.get_mut("toolResult").and_then(Value::as_object_mut) {
                    result.remove("status");
                    for item in result["content"].as_array_mut().unwrap() {
                        if let Some(value) = item.get("json").cloned() {
                            *item = json!({"text": value.to_string()});
                        }
                    }
                }
            }
        }
        body
    }

    fn replay(c: &Value, index: usize) -> (Vec<Event>, Completion) {
        let bytes = base64::engine::general_purpose::STANDARD.decode(c["interactions"][index]["response"]["body"].as_str().unwrap()).unwrap();
        let (mut frames, mut decoder, mut events) = (eventstream::Decoder::default(), Decoder::default(), Vec::new());
        for chunk in bytes.chunks(113) {
            for frame in frames.feed(chunk).unwrap() {
                events.extend(decoder.push(&frame).unwrap());
            }
        }
        assert_eq!(frames.pending(), 0);
        let (tail, completion) = decoder.finish().unwrap();
        events.extend(tail);
        (events, completion)
    }

    fn text(events: &[Event]) -> String {
        events.iter().filter_map(|e| if let Event::TextDelta(t) = e { Some(t.as_str()) } else { None }).collect()
    }

    #[test]
    fn cassette_text_and_forced_tool_call() {
        let Some(c) = cassette(&format!("{DIR}/streams-text.json")) else { return };
        let request = Request {
            model: "us.amazon.nova-micro-v1:0".into(),
            system: vec!["Reply with the single word 'Hello'.".into()],
            messages: vec![Message::User("Say hello.".into())],
            max_tokens: Some(16),
            temperature: Some(0.0),
            ..Default::default()
        };
        assert_eq!(encode(&request).unwrap(), recorded(&c, 0));
        assert_eq!(c["interactions"][0]["request"]["url"], format!("{}{}", base_url("us-east-1"), path(&request.model)));
        let (events, _) = replay(&c, 0);
        assert_eq!(text(&events), "Hello");
        assert_eq!(events.last(), Some(&Event::Finish { reason: "stop".into(), usage: Usage { input: 12, output: 2, total: 14, ..Default::default() } }));

        let Some(c) = cassette(&format!("{DIR}/streams-a-tool-call.json")) else { return };
        let request = Request {
            model: "us.amazon.nova-micro-v1:0".into(),
            system: vec!["Call tools exactly as requested.".into()],
            messages: vec![Message::User("Call get_weather with city exactly Paris.".into())],
            tools: vec![weather(), Tool { name: "other".into(), description: "x".into(), parameters: json!({}) }],
            tool_choice: Some(ToolChoice::Tool("get_weather".into())),
            max_tokens: Some(80),
            temperature: Some(0.0),
            ..Default::default()
        };
        assert_eq!(encode(&request).unwrap(), recorded(&c, 0));
        let (events, _) = replay(&c, 0);
        assert_eq!(events[0], Event::ToolCall(ToolCall { id: "tooluse_6a1pPvnc99GLKO3KGkUA2N".into(), name: "get_weather".into(), arguments: "{\"city\":\"Paris\"}".into() }));
        assert!(matches!(&events[1], Event::Finish { reason, usage } if reason == "tool-calls" && usage.input == 419 && usage.total == 435));
    }

    #[test]
    fn cassette_tool_loop_round_trips() {
        let Some(c) = cassette(&format!("{DIR}/drives-a-tool-loop.json")) else { return };
        let mut request = Request {
            model: "us.amazon.nova-micro-v1:0".into(),
            system: vec!["Use the get_weather tool, then answer in one short sentence.".into()],
            messages: vec![Message::User("What is the weather in Paris?".into())],
            tools: vec![weather()],
            max_tokens: Some(80),
            temperature: Some(0.0),
            ..Default::default()
        };
        assert_eq!(encode(&request).unwrap(), recorded(&c, 0));
        let (events, _) = replay(&c, 0);
        let calls: Vec<ToolCall> = events.iter().filter_map(|e| if let Event::ToolCall(t) = e { Some(t.clone()) } else { None }).collect();
        assert_eq!(calls.len(), 1);
        request.messages.push(Message::assistant(text(&events), calls.clone()));
        request.messages.push(Message::tool(&calls[0].id, "{\"temperature\":22,\"condition\":\"sunny\"}"));
        assert_eq!(encode(&request).unwrap(), recorded(&c, 1));
        assert_eq!(text(&replay(&c, 1).0), "The weather in Paris is sunny with a temperature of 22 degrees.");
    }

    #[test]
    fn thinking_cache_points_and_reasoning_replay() {
        let mut request = Request {
            model: "us.anthropic.claude-sonnet-4-5-20250929-v1:0".into(),
            system: vec!["a".into(), "b".into(), "c".into()],
            messages: vec![
                Message::User("q".into()),
                Message::Assistant {
                    text: "thinking done ".into(),
                    reasoning: String::new(),
                    tool_calls: vec![ToolCall { id: "t1".into(), name: "get.weather".into(), arguments: "{\"city\":\"Paris\"}".into() }],
                    reasoning_parts: vec![Reasoning { text: "hm".into(), signature: Some("sig".into()), ..Default::default() }, Reasoning { text: "unsigned".into(), ..Default::default() }],
                },
                Message::Tool { call_id: "t1".into(), content: "sunny".into(), is_error: false, attachments: vec![Part::Media { mime: "image/png".into(), data: "AAAA".into(), filename: None }] },
                Message::UserParts(vec![Part::Text("see ".into()), Part::Media { mime: "application/pdf".into(), data: "JVBERi0=".into(), filename: Some("report.final.pdf".into()) }]),
            ],
            tools: vec![weather()],
            tool_choice: Some(ToolChoice::Required),
            max_tokens: Some(1000),
            temperature: Some(0.5),
            top_p: Some(0.9),
            options: json!({"reasoningConfig": {"type": "enabled", "budgetTokens": 16000, "blockBinding": {"prefixMismatchBehavior": "drop_block"}}, "anthropicBeta": ["x-beta"]}),
            cache: true,
            ..Default::default()
        };
        let body = encode(&request).unwrap();
        assert_eq!(body["system"], json!([{"text": "a"}, {"cachePoint": {"type": "default"}}, {"text": "b"}, {"cachePoint": {"type": "default"}}, {"text": "c"}]));
        assert_eq!(body["inferenceConfig"], json!({"maxTokens": 17000}));
        assert_eq!(
            body["additionalModelRequestFields"],
            json!({"thinking": {"type": "enabled", "budget_tokens": 16000, "block_binding": {"prefix_mismatch_behavior": "drop_block"}}, "anthropic_beta": ["x-beta", BLOCK_BINDING_BETA]})
        );
        assert_eq!(body["additionalModelResponseFieldPaths"], json!(["/delta/stop_sequence"]));
        assert_eq!(body["toolConfig"]["toolChoice"], json!({"any": {}}));
        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": [{"text": "q"}]},
                {"role": "assistant", "content": [
                    {"reasoningContent": {"reasoningText": {"text": "hm", "signature": "sig"}}},
                    {"text": "thinking done "},
                    {"toolUse": {"toolUseId": "t1", "name": "getweather", "input": {"city": "Paris"}}}
                ]},
                {"role": "user", "content": [
                    {"toolResult": {"toolUseId": "t1", "content": [{"text": "sunny"}, {"image": {"format": "png", "source": {"bytes": "AAAA"}}}]}},
                    {"cachePoint": {"type": "default"}},
                    {"text": "see"},
                    {"document": {"format": "pdf", "name": "report.final", "source": {"bytes": "JVBERi0="}}},
                    {"cachePoint": {"type": "default"}}
                ]}
            ])
        );

        // Adaptive thinking keeps the output cap and moves the effort to `output_config`; other families get `reasoningConfig`.
        request.options = json!({"reasoningConfig": {"type": "adaptive", "maxReasoningEffort": "high", "display": "summarized"}});
        let body = encode(&request).unwrap();
        assert_eq!(body["additionalModelRequestFields"], json!({"thinking": {"type": "adaptive", "display": "summarized"}, "output_config": {"effort": "high"}}));
        assert_eq!(body["inferenceConfig"], json!({"maxTokens": 1000}));
        request.model = "amazon.nova-2-lite-v1:0".into();
        request.options = json!({"reasoningConfig": {"type": "enabled", "maxReasoningEffort": "low"}});
        request.tool_choice = Some(ToolChoice::None);
        let body = encode(&request).unwrap();
        assert_eq!(body["additionalModelRequestFields"], json!({"reasoningConfig": {"type": "enabled", "maxReasoningEffort": "low"}}));
        assert_eq!(body["inferenceConfig"], json!({"maxTokens": 1000, "temperature": 0.5, "topP": 0.9}));
        // Without tools the tool history is dropped.
        assert!(body.get("toolConfig").is_none() && body.get("additionalModelResponseFieldPaths").is_none());
        assert_eq!(body["messages"][1]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn decodes_reasoning_cache_usage_and_exceptions() {
        let frame = |kind: &str, payload: Value| eventstream::Frame { headers: vec![(":message-type".into(), "event".into()), (":event-type".into(), kind.into())], payload: payload.to_string().into_bytes() };
        let mut decoder = Decoder::default();
        let mut events = Vec::new();
        for (kind, payload) in [
            ("messageStart", json!({"role": "assistant"})),
            ("contentBlockDelta", json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"text": "let me "}}})),
            ("contentBlockDelta", json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"text": "think"}}})),
            ("contentBlockDelta", json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"signature": "sig"}}})),
            ("contentBlockStop", json!({"contentBlockIndex": 0})),
            ("contentBlockDelta", json!({"contentBlockIndex": 1, "delta": {"reasoningContent": {"redactedContent": "b64"}}})),
            ("contentBlockStop", json!({"contentBlockIndex": 1})),
            ("contentBlockStart", json!({"contentBlockIndex": 2, "start": {"toolUse": {"toolUseId": "t", "name": "n"}}})),
            ("contentBlockStop", json!({"contentBlockIndex": 2})),
            ("messageStop", json!({"stopReason": "max_tokens"})),
            ("metadata", json!({"usage": {"inputTokens": 10, "outputTokens": 5, "totalTokens": 15, "cacheReadInputTokens": 100, "cacheWriteInputTokens": 20}})),
        ] {
            events.extend(decoder.push(&frame(kind, payload)).unwrap());
        }
        let (tail, completion) = decoder.finish().unwrap();
        events.extend(tail);
        assert_eq!(events[..3], [Event::ReasoningDelta("let me ".into()), Event::ReasoningDelta("think".into()), Event::ToolCall(ToolCall { id: "t".into(), name: "n".into(), arguments: "{}".into() })]);
        assert_eq!(events[3], Event::Finish { reason: "length".into(), usage: Usage { input: 130, output: 5, reasoning: 0, cache_read: 100, cache_write: 20, total: 135 } });
        assert_eq!(completion.reasoning, [Reasoning { text: "let me think".into(), signature: Some("sig".into()), ..Default::default() }, Reasoning { encrypted: Some("b64".into()), ..Default::default() }]);

        let exception = eventstream::Frame { headers: vec![(":message-type".into(), "exception".into()), (":exception-type".into(), "throttlingException".into())], payload: br#"{"message":"Too many requests"}"#.to_vec() };
        let error = Decoder::default().push(&exception).unwrap_err();
        assert!(matches!(&error, Error::Provider { code: Some(code), message } if code == "throttlingException" && message == "Too many requests"));
        assert!(matches!(Decoder::default().finish(), Err(Error::Stream(_))));
    }

    #[test]
    fn cross_region_prefixes() {
        let id = "anthropic.claude-sonnet-4-5-20250929-v1:0";
        assert_eq!(model_id(id, "us-east-1"), format!("us.{id}"));
        assert_eq!(model_id(id, "us-gov-west-1"), id);
        assert_eq!(model_id(id, "eu-central-1"), format!("eu.{id}"));
        assert_eq!(model_id(id, "eu-central-2"), id);
        assert_eq!(model_id(id, "ap-southeast-2"), format!("au.{id}"));
        assert_eq!(model_id(id, "ap-northeast-1"), format!("jp.{id}"));
        assert_eq!(model_id(id, "ap-south-1"), format!("apac.{id}"));
        assert_eq!(model_id("anthropic.claude-opus-4-1", "ap-southeast-4"), "apac.anthropic.claude-opus-4-1");
        assert_eq!(model_id("amazon.nova-pro-v1:0", "eu-west-1"), "amazon.nova-pro-v1:0");
        assert_eq!(model_id("meta.llama3-70b", "eu-west-1"), "eu.meta.llama3-70b");
        assert_eq!(model_id("deepseek.r1-v1:0", "us-west-2"), "us.deepseek.r1-v1:0");
        assert_eq!(model_id("global.anthropic.claude", "us-east-1"), "global.anthropic.claude");
        assert_eq!(model_id("arn:aws:bedrock:us-east-1:1:inference-profile/x", "us-east-1"), "arn:aws:bedrock:us-east-1:1:inference-profile/x");
        assert_eq!(model_id("cohere.command-r", "sa-east-1"), "cohere.command-r");
        assert_eq!(path("us.amazon.nova-micro-v1:0"), "/model/us.amazon.nova-micro-v1%3A0/converse-stream");
        assert!(mantle_uses_chat("openai.gpt-oss-safeguard-20b") && !mantle_uses_chat("openai.gpt-oss-120b"));
    }

    #[test]
    fn credential_chain_order() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".aws")).unwrap();
        std::fs::write(home.path().join(".aws").join("credentials"), "[default]\naws_access_key_id = AKIADEFAULT\naws_secret_access_key = s1\n\n[work]\naws_access_key_id=AKIAWORK\naws_secret_access_key=s2\naws_session_token=tok\n").unwrap();
        std::fs::write(home.path().join(".aws").join("config"), "[profile cfg]\naws_access_key_id = AKIACFG\naws_secret_access_key = s3\n[profile sso]\nsso_session = corp\n[profile role]\nrole_arn = arn:aws:iam::1:role/x\n").unwrap();
        let run = |vars: &[(&str, &str)], config: &Config| {
            let vars: Vec<(String, String)> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            let var = move |name: &str| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
            resolve(config, &Source { var: &var, home: home.path().to_path_buf() })
        };
        let key = |result: Result<Auth, Error>| match result.unwrap() {
            Auth::SigV4(c) => (c.access_key_id, c.session_token),
            Auth::Bearer(token) => (format!("bearer:{token}"), None),
        };
        let base = Config::new("us-east-1");
        let env = [("AWS_ACCESS_KEY_ID", "AKIAENV"), ("AWS_SECRET_ACCESS_KEY", "s"), ("AWS_SESSION_TOKEN", "st")];
        assert_eq!(key(run(&env, &base)), ("AKIAENV".into(), Some("st".into())));
        // The bearer token beats every key; the environment's beats the stored one.
        let stored = Config { bearer: Some(" stored ".into()), ..base.clone() };
        assert_eq!(key(run(&env, &stored)).0, "bearer:stored");
        assert_eq!(key(run(&[(BEARER_ENV, "envtoken")], &stored)).0, "bearer:envtoken");
        // A named profile skips the environment keys.
        assert_eq!(key(run(&[env[0], env[1], ("AWS_PROFILE", "work")], &base)), ("AKIAWORK".into(), Some("tok".into())));
        assert_eq!(key(run(&env, &Config { profile: Some("cfg".into()), ..base.clone() })).0, "AKIACFG");
        assert_eq!(key(run(&[], &base)).0, "AKIADEFAULT");
        for (profile, what) in [("sso", "AWS SSO profiles"), ("role", "assume-role profiles")] {
            let error = run(&[], &Config { profile: Some(profile.into()), ..base.clone() }).unwrap_err();
            assert!(matches!(&error, Error::Config(m) if m.starts_with(&format!("not supported yet: {what}"))), "{error}");
        }
        assert!(matches!(run(&[], &Config { profile: Some("missing".into()), ..base.clone() }), Err(Error::Config(m)) if m.contains("profile missing")));

        let source = |vars: &'static [(&'static str, &'static str)]| move |name: &str| vars.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string());
        let none = source(&[]);
        let container = source(&[("AWS_CONTAINER_CREDENTIALS_FULL_URI", "http://x")]);
        assert!(!detected(&Source { var: &none, home: home.path().to_path_buf() }, None, false, false));
        assert!(detected(&Source { var: &none, home: home.path().to_path_buf() }, None, true, false));
        assert!(detected(&Source { var: &container, home: home.path().to_path_buf() }, None, false, false));
    }
}
