// Anthropic Messages: request encoding and stream decoding.
use crate::{Completion, Error, Event, Message, Part, Reasoning, Request, ToolCall, ToolChoice, Usage, chat, number};
use base64::Engine;
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::LazyLock;

pub const VERSION: &str = "2023-06-01";
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com/v1";
pub const PATH: &str = "/messages";
pub const BLOCK_BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";
pub const PDF_BETA: &str = "pdfs-2024-09-25";
pub const STRUCTURED_OUTPUTS_BETA: &str = "structured-outputs-2025-11-13";
const ROUTE: &str = "Anthropic Messages";
static LEGACY_CLAUDE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"claude-(?:instant(?:-|$)|v?2(?:$|[-.:])|3(?:$|[-.]))").unwrap());

struct Capabilities {
    max_output: u64,
    structured_output: bool,
    rejects_sampling: bool,
    known: bool,
}

// The SDK's per-model table: output ceiling, structured outputs, and whether sampling parameters are refused.
fn capabilities(model: &str) -> Capabilities {
    let has = |names: &[&str]| names.iter().any(|n| model.contains(n));
    let row = |max_output, structured_output, rejects_sampling, known| Capabilities { max_output, structured_output, rejects_sampling, known };
    if has(&["claude-opus-5", "claude-opus-4-8", "claude-opus-4-7", "claude-fable-5", "claude-sonnet-5"]) {
        row(128_000, true, true, true)
    } else if has(&["claude-sonnet-4-6", "claude-opus-4-6"]) {
        row(128_000, true, false, true)
    } else if has(&["claude-sonnet-4-5", "claude-opus-4-5", "claude-haiku-4-5"]) {
        row(64_000, true, false, true)
    } else if has(&["claude-opus-4-1"]) {
        row(32_000, true, false, true)
    } else if has(&["claude-sonnet-4-"]) {
        row(64_000, false, false, true)
    } else if has(&["claude-opus-4-"]) {
        row(32_000, false, false, true)
    } else if has(&["claude-3-haiku"]) {
        row(4096, false, false, true)
    } else if LEGACY_CLAUDE.is_match(model) {
        row(4096, false, false, false)
    } else if has(&["claude-"]) {
        row(128_000, true, true, false)
    } else {
        row(4096, false, false, false)
    }
}

fn ephemeral() -> Value {
    json!({"type": "ephemeral"})
}

fn part(part: &Part) -> Result<Value, Error> {
    let (mime, data, filename) = match part {
        Part::Text(text) => return Ok(json!({"type": "text", "text": text})),
        Part::Media { mime, data, filename } => (mime.as_str(), data, filename),
    };
    if mime.starts_with("image/") {
        let mime = if mime == "image/*" { "image/jpeg" } else { mime };
        return Ok(json!({"type": "image", "source": {"type": "base64", "media_type": mime, "data": data}}));
    }
    let source = match mime {
        "application/pdf" => json!({"type": "base64", "media_type": "application/pdf", "data": data}),
        "text/plain" => {
            let bytes = base64::engine::general_purpose::STANDARD.decode(data).map_err(|_| Error::Config(format!("{ROUTE} media must contain valid base64")))?;
            json!({"type": "text", "media_type": "text/plain", "data": String::from_utf8_lossy(&bytes)})
        }
        _ => return Err(Error::Config(format!("'media type: {mime}' functionality not supported."))),
    };
    let mut block = json!({"type": "document", "source": source});
    if let Some(name) = filename {
        block["title"] = name.clone().into();
    }
    Ok(block)
}

fn has_pdf(req: &Request) -> bool {
    let pdf = |parts: &[Part]| parts.iter().any(|p| matches!(p, Part::Media { mime, .. } if mime == "application/pdf"));
    req.messages.iter().any(|m| match m {
        Message::UserParts(parts) => pdf(parts),
        Message::Tool { attachments, .. } => pdf(attachments),
        _ => false,
    })
}

// Claude rejects tool ids outside [a-zA-Z0-9_-].
fn call_id(req: &Request, id: &str) -> String {
    if !req.model.contains("claude") {
        return id.to_string();
    }
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

fn tool_input(call: &ToolCall) -> Result<Value, Error> {
    let raw = if call.arguments.trim().is_empty() { "{}" } else { &call.arguments };
    serde_json::from_str(raw).map_err(|_| Error::Config(format!("Invalid JSON input for {ROUTE} tool call {}", call.name)))
}

fn kind(block: &Value) -> &str {
    block.get("type").and_then(Value::as_str).unwrap_or_default()
}

// Within each run between thinking blocks, tool calls come after everything else.
fn tool_use_last(content: Vec<Value>) -> Vec<Value> {
    let mut out = Vec::with_capacity(content.len());
    let mut segment: Vec<Value> = Vec::new();
    let flush = |segment: &mut Vec<Value>, out: &mut Vec<Value>| {
        let (calls, rest): (Vec<Value>, Vec<Value>) = std::mem::take(segment).into_iter().partition(|b| kind(b) == "tool_use");
        out.extend(rest);
        out.extend(calls);
    };
    for block in content {
        if matches!(kind(&block), "thinking" | "redacted_thinking") {
            flush(&mut segment, &mut out);
            out.push(block);
        } else {
            segment.push(block);
        }
    }
    flush(&mut segment, &mut out);
    out
}

fn messages(req: &Request) -> Result<Vec<Value>, Error> {
    let targets = if req.cache { chat::cache_targets(&req.messages) } else { Vec::new() };
    // Consecutive assistant turns share one message; so do user turns and tool results.
    let mut out: Vec<(bool, Vec<Value>)> = Vec::new();
    for (i, message) in req.messages.iter().enumerate() {
        let (assistant, mut content) = match message {
            Message::User(text) => (false, vec![json!({"type": "text", "text": text})]),
            Message::UserParts(parts) => (false, parts.iter().map(part).collect::<Result<Vec<_>, _>>()?),
            Message::Assistant { text, tool_calls, reasoning_parts, .. } => {
                let mut content = Vec::new();
                for r in reasoning_parts {
                    if let Some(signature) = &r.signature {
                        content.push(json!({"type": "thinking", "thinking": r.text, "signature": signature}));
                    } else if let Some(data) = &r.encrypted {
                        content.push(json!({"type": "redacted_thinking", "data": data}));
                    }
                }
                if !text.is_empty() {
                    // The API refuses trailing whitespace in a final assistant turn.
                    let last = i + 1 == req.messages.len() && tool_calls.is_empty();
                    content.push(json!({"type": "text", "text": if last { text.trim() } else { text.as_str() }}));
                }
                for call in tool_calls {
                    content.push(json!({"type": "tool_use", "id": call_id(req, &call.id), "name": call.name, "input": tool_input(call)?}));
                }
                (true, content)
            }
            Message::Tool { call_id: id, content, is_error, attachments } => {
                let mut block = Map::new();
                block.insert("type".into(), "tool_result".into());
                block.insert("tool_use_id".into(), call_id(req, id).into());
                if attachments.is_empty() {
                    block.insert("content".into(), content.clone().into());
                } else {
                    let mut items = vec![json!({"type": "text", "text": content})];
                    for a in attachments {
                        items.push(part(a)?);
                    }
                    block.insert("content".into(), items.into());
                }
                if *is_error {
                    block.insert("is_error".into(), true.into());
                }
                (false, vec![Value::Object(block)])
            }
        };
        if targets.contains(&i) {
            // Thinking blocks cannot carry a marker.
            mark(content.last_mut().filter(|b| !matches!(kind(b), "thinking" | "redacted_thinking")));
        }
        if content.is_empty() {
            continue;
        }
        match out.last_mut().filter(|(role, _)| *role == assistant) {
            Some((_, blocks)) => blocks.extend(content),
            None => out.push((assistant, content)),
        }
    }
    Ok(out
        .into_iter()
        .map(|(assistant, content)| if assistant { json!({"role": "assistant", "content": tool_use_last(content)}) } else { json!({"role": "user", "content": content}) })
        .collect())
}

fn thinking(options: &Value) -> Option<Value> {
    let source = options.get("thinking")?.as_object()?;
    let kind = source.get("type")?.as_str()?;
    let mut out = Map::new();
    out.insert("type".into(), kind.into());
    match kind {
        "enabled" => {
            // The SDK falls back to the API minimum when no budget is given.
            let budget = source.get("budgetTokens").or_else(|| source.get("budget_tokens")).filter(|v| v.is_number()).cloned();
            out.insert("budget_tokens".into(), budget.unwrap_or_else(|| 1024.into()));
        }
        "adaptive" => {
            if let Some(display) = source.get("display").filter(|v| v.is_string()) {
                out.insert("display".into(), display.clone());
            }
        }
        "disabled" => return Some(Value::Object(out)),
        _ => return None,
    }
    if let Some(behavior) = binding(options) {
        out.insert("block_binding".into(), json!({"prefix_mismatch_behavior": behavior}));
    }
    Some(Value::Object(out))
}

fn binding(options: &Value) -> Option<&str> {
    let thinking = options.get("thinking")?;
    matches!(thinking.get("type")?.as_str()?, "enabled" | "adaptive").then_some(())?;
    thinking.pointer("/blockBinding/prefixMismatchBehavior")?.as_str()
}

fn sends_tools(req: &Request) -> bool {
    !req.tools.is_empty() && req.tool_choice != Some(ToolChoice::None)
}

// Beta flags the request body itself requires; they go in front of the configured `anthropic-beta` values.
pub fn betas(req: &Request) -> Vec<&'static str> {
    let mut out = Vec::new();
    if has_pdf(req) {
        out.push(PDF_BETA);
    }
    if binding(&req.options).is_some() {
        out.push(BLOCK_BINDING_BETA);
    }
    if sends_tools(req) && capabilities(&req.model).structured_output {
        out.push(STRUCTURED_OUTPUTS_BETA);
    }
    out
}

fn mark(block: Option<&mut Value>) {
    if let Some(Value::Object(block)) = block {
        block.insert("cache_control".into(), ephemeral());
    }
}

pub fn encode(req: &Request) -> Result<Value, Error> {
    let caps = capabilities(&req.model);
    let mut body = Map::new();
    body.insert("model".into(), req.model.clone().into());
    if !req.system.is_empty() {
        let mut system: Vec<Value> = req.system.iter().map(|text| json!({"type": "text", "text": text})).collect();
        if req.cache {
            system.iter_mut().take(2).for_each(|block| mark(Some(block)));
        }
        body.insert("system".into(), system.into());
    }
    body.insert("messages".into(), messages(req)?.into());
    if sends_tools(req) {
        // Tool input streams eagerly unless `toolStreaming` is switched off.
        let eager = req.options.get("toolStreaming").and_then(Value::as_bool) != Some(false);
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                let mut tool = json!({"name": t.name, "description": t.description, "input_schema": t.parameters});
                if eager {
                    tool["eager_input_streaming"] = true.into();
                }
                tool
            })
            .collect();
        body.insert("tools".into(), tools.into());
        match &req.tool_choice {
            Some(ToolChoice::Auto) => body.insert("tool_choice".into(), json!({"type": "auto"})),
            Some(ToolChoice::Required) => body.insert("tool_choice".into(), json!({"type": "any"})),
            Some(ToolChoice::Tool(name)) => body.insert("tool_choice".into(), json!({"type": "tool", "name": name})),
            Some(ToolChoice::None) | None => None,
        };
    }
    body.insert("stream".into(), true.into());
    let thinking = thinking(&req.options);
    let active = thinking.as_ref().is_some_and(|t| t["type"] != "disabled");
    // A thinking budget is spent on top of the visible output; known models cap the sum.
    let budget = thinking.as_ref().and_then(|t| t.get("budget_tokens")).and_then(Value::as_u64).unwrap_or(0);
    let mut max_tokens = req.max_tokens.unwrap_or(caps.max_output) + if active { budget } else { 0 };
    if caps.known {
        max_tokens = max_tokens.min(caps.max_output);
    }
    body.insert("max_tokens".into(), max_tokens.into());
    // Sampling parameters are dropped while thinking and on models that refuse them.
    let sampling = !active && !caps.rejects_sampling;
    let claude = caps.known || req.model.contains("claude-");
    if let Some(v) = req.temperature.filter(|_| sampling) {
        body.insert("temperature".into(), number(v.clamp(0.0, 1.0)));
    }
    if let Some(v) = req.top_p.filter(|_| sampling && !(claude && req.temperature.is_some())) {
        body.insert("top_p".into(), number(v));
    }
    if let Some(v) = req.top_k.filter(|_| sampling) {
        body.insert("top_k".into(), v.into());
    }
    if let Some(thinking) = thinking {
        body.insert("thinking".into(), thinking);
    }
    if let Some(effort) = req.options.get("effort").filter(|v| v.as_str().is_some_and(|s| !s.is_empty())) {
        body.insert("output_config".into(), json!({"effort": effort}));
    }
    if let Some(cache) = req.options.get("cacheControl").filter(|v| v.is_object()) {
        body.insert("cache_control".into(), cache.clone());
    }
    Ok(Value::Object(body))
}

enum Block {
    Tool { call: ToolCall, server: bool },
    Thinking(Reasoning),
}

#[derive(Default)]
struct Tokens {
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
}

#[derive(Default)]
pub struct Decoder {
    blocks: BTreeMap<u64, Block>,
    tokens: Tokens,
    completion: Completion,
    finished: bool,
}

fn finish_reason(reason: Option<&str>) -> &'static str {
    match reason {
        Some("end_turn" | "stop_sequence" | "pause_turn") => "stop",
        Some("max_tokens") => "length",
        Some("tool_use") => "tool-calls",
        Some("refusal") => "content-filter",
        _ => "unknown",
    }
}

impl Decoder {
    // Later usage reports win field by field; the final message_delta carries the totals.
    fn merge_usage(&mut self, usage: Option<&Value>) {
        let Some(usage) = usage else { return };
        let n = |key: &str| usage.get(key).and_then(Value::as_u64);
        self.tokens.input = n("input_tokens").or(self.tokens.input);
        self.tokens.output = n("output_tokens").or(self.tokens.output);
        self.tokens.cache_read = n("cache_read_input_tokens").or(self.tokens.cache_read);
        self.tokens.cache_write = n("cache_creation_input_tokens").or(self.tokens.cache_write);
    }

    fn usage(&self) -> Usage {
        let t = &self.tokens;
        let input = t.input.unwrap_or(0) + t.cache_read.unwrap_or(0) + t.cache_write.unwrap_or(0);
        let output = t.output.unwrap_or(0);
        Usage {
            input,
            output,
            reasoning: 0,
            cache_read: t.cache_read.unwrap_or(0),
            cache_write: t.cache_write.unwrap_or(0),
            total: input + output,
        }
    }

    pub fn push(&mut self, data: &str) -> Result<Vec<Event>, Error> {
        if data.trim().is_empty() || data.trim() == "[DONE]" {
            return Ok(Vec::new());
        }
        let event: Value = serde_json::from_str(data).map_err(|_| Error::Stream("Invalid anthropic/anthropic-messages stream event".into()))?;
        let index = event.get("index").and_then(Value::as_u64);
        let text = |v: Option<&Value>| v.and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
        let mut events = Vec::new();
        match event.get("type").and_then(Value::as_str).unwrap_or_default() {
            "message_start" => self.merge_usage(event.pointer("/message/usage")),
            "content_block_start" => {
                let block = &event["content_block"];
                let kind = block.get("type").and_then(Value::as_str).unwrap_or_default();
                match (kind, index) {
                    ("tool_use" | "server_tool_use", Some(i)) => {
                        let call = ToolCall {
                            id: text(block.get("id")).unwrap_or_else(|| i.to_string()),
                            name: text(block.get("name")).unwrap_or_default(),
                            arguments: String::new(),
                        };
                        self.blocks.insert(i, Block::Tool { call, server: kind == "server_tool_use" });
                    }
                    ("text", _) => events.extend(text(block.get("text")).map(Event::TextDelta)),
                    ("thinking", _) => {
                        let initial = text(block.get("thinking"));
                        let reasoning = Reasoning { text: initial.clone().unwrap_or_default(), signature: text(block.get("signature")), ..Default::default() };
                        self.blocks.insert(index.unwrap_or(0), Block::Thinking(reasoning));
                        events.extend(initial.map(Event::ReasoningDelta));
                    }
                    ("redacted_thinking", _) => {
                        let reasoning = Reasoning { encrypted: text(block.get("data")), ..Default::default() };
                        self.blocks.insert(index.unwrap_or(0), Block::Thinking(reasoning));
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let delta = &event["delta"];
                let slot = self.blocks.get_mut(&index.unwrap_or(0));
                match delta.get("type").and_then(Value::as_str).unwrap_or_default() {
                    "text_delta" => events.extend(text(delta.get("text")).map(Event::TextDelta)),
                    "thinking_delta" => {
                        if let Some(chunk) = text(delta.get("thinking")) {
                            if let Some(Block::Thinking(r)) = slot {
                                r.text.push_str(&chunk);
                            }
                            events.push(Event::ReasoningDelta(chunk));
                        }
                    }
                    "signature_delta" => {
                        if let (Some(chunk), Some(Block::Thinking(r))) = (text(delta.get("signature")), slot) {
                            r.signature.get_or_insert_default().push_str(&chunk);
                        }
                    }
                    "input_json_delta" if index.is_some() => {
                        if let Some(chunk) = text(delta.get("partial_json")) {
                            match slot {
                                Some(Block::Tool { call, .. }) => call.arguments.push_str(&chunk),
                                _ => return Err(Error::Stream(format!("{ROUTE} tool argument delta is missing its tool call"))),
                            }
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => match index.and_then(|i| self.blocks.remove(&i)) {
                Some(Block::Tool { mut call, server }) => {
                    if call.arguments.is_empty() {
                        call.arguments = "{}".into();
                    }
                    if serde_json::from_str::<Value>(&call.arguments).is_err() {
                        return Err(Error::Stream(format!("Invalid JSON input for anthropic-messages tool call {}", call.name)));
                    }
                    // Provider-executed tools are resolved upstream and never dispatched locally.
                    if !server {
                        events.push(Event::ToolCall(call));
                    }
                }
                Some(Block::Thinking(reasoning)) => self.completion.reasoning.push(reasoning),
                None => {}
            },
            "message_delta" => {
                self.merge_usage(event.get("usage"));
                self.finished = true;
                let reason = finish_reason(event.pointer("/delta/stop_reason").and_then(Value::as_str));
                events.push(Event::Finish { reason: reason.to_string(), usage: self.usage() });
            }
            "error" => {
                let kind = text(event.pointer("/error/type"));
                let message = text(event.pointer("/error/message"));
                let message = match (&kind, message) {
                    (Some(kind), Some(message)) => format!("{kind}: {message}"),
                    (_, Some(message)) => message,
                    (Some(kind), None) => kind.clone(),
                    (None, None) => format!("{ROUTE} stream error"),
                };
                return Err(Error::Provider { code: kind, message });
            }
            _ => {}
        }
        Ok(events)
    }

    pub fn finish(self) -> Result<Completion, Error> {
        if !self.finished {
            return Err(Error::Stream("Provider stream ended without a terminal finish event".into()));
        }
        Ok(self.completion)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{cassette, request_body, response_body};
    use crate::{Tool, sse};

    const DIR: &str = "llm/test/fixtures/recordings/anthropic-messages";

    fn replay(body: &str) -> (Vec<Event>, Completion) {
        let mut sse = sse::Decoder::default();
        let mut decoder = Decoder::default();
        let mut events = Vec::new();
        for piece in body.as_bytes().chunks(11) {
            for data in sse.feed(piece) {
                events.extend(decoder.push(&data).unwrap());
            }
        }
        for data in sse.finish() {
            events.extend(decoder.push(&data).unwrap());
        }
        (events, decoder.finish().unwrap())
    }

    fn text(events: &[Event]) -> String {
        events.iter().filter_map(|e| if let Event::TextDelta(t) = e { Some(t.as_str()) } else { None }).collect()
    }

    fn calls(events: &[Event]) -> Vec<ToolCall> {
        events.iter().filter_map(|e| if let Event::ToolCall(t) = e { Some(t.clone()) } else { None }).collect()
    }

    fn finish(events: &[Event]) -> (&str, &Usage) {
        match events.last() {
            Some(Event::Finish { reason, usage }) => (reason, usage),
            other => panic!("no finish: {other:?}"),
        }
    }

    fn weather() -> Tool {
        Tool {
            name: "get_weather".into(),
            description: "Get current weather for a city.".into(),
            parameters: json!({"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"], "additionalProperties": false}),
        }
    }

    fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
        ToolCall { id: id.into(), name: name.into(), arguments: arguments.into() }
    }

    #[test]
    fn text_request_and_stream_match_recording() {
        let Some(c) = cassette(&format!("{DIR}/streams-text.json")) else { return };
        let req = Request {
            model: "claude-haiku-4-5-20251001".into(),
            system: vec!["You are concise.".into()],
            messages: vec![Message::User("Reply with exactly: Hello!".into())],
            max_tokens: Some(20),
            temperature: Some(0.0),
            ..Default::default()
        };
        let body = encode(&req).unwrap();
        assert_eq!(body, request_body(&c, 0));
        assert_eq!(serde_json::to_string(&body).unwrap(), c["interactions"][0]["request"]["body"].as_str().unwrap());
        let (events, completion) = replay(response_body(&c, 0));
        assert_eq!(text(&events), "Hello!");
        let (reason, usage) = finish(&events);
        assert_eq!(reason, "stop");
        assert!(usage.input > 0 && usage.output > 0 && usage.total == usage.input + usage.output);
        assert!(completion.reasoning.is_empty());
    }

    #[test]
    fn tool_call_request_and_stream_match_recording() {
        let Some(c) = cassette(&format!("{DIR}/streams-tool-call.json")) else { return };
        let req = Request {
            model: "claude-haiku-4-5-20251001".into(),
            system: vec!["Call tools exactly as requested.".into()],
            messages: vec![Message::User("Call get_weather with city exactly Paris.".into())],
            tools: vec![weather()],
            tool_choice: Some(ToolChoice::Tool("get_weather".into())),
            max_tokens: Some(80),
            options: json!({"toolStreaming": false}),
            temperature: Some(0.0),
            ..Default::default()
        };
        assert_eq!(serde_json::to_string(&encode(&req).unwrap()).unwrap(), c["interactions"][0]["request"]["body"].as_str().unwrap());
        let (events, _) = replay(response_body(&c, 0));
        assert_eq!(calls(&events), [call("toolu_012rmAruviySvUXSjgCPWVRu", "get_weather", "{\"city\": \"Paris\"}")]);
        let (reason, usage) = finish(&events);
        assert_eq!(reason, "tool-calls");
        assert_eq!((usage.input, usage.output, usage.cache_read, usage.cache_write), (677, 33, 0, 0));
    }

    #[test]
    fn tool_loop_matches_recording() {
        let Some(c) = cassette(&format!("{DIR}/claude-opus-4-7-drives-a-tool-loop.json")) else { return };
        let mut req = Request {
            model: "claude-opus-4-7".into(),
            system: vec!["Use the get_weather tool, then answer in one short sentence.".into()],
            messages: vec![Message::User("What is the weather in Paris?".into())],
            tools: vec![weather()],
            max_tokens: Some(80),
            options: json!({"toolStreaming": false}),
            ..Default::default()
        };
        assert_eq!(encode(&req).unwrap(), request_body(&c, 0));
        let (events, _) = replay(response_body(&c, 0));
        let made = calls(&events);
        assert_eq!(made.len(), 1);
        assert_eq!(finish(&events).0, "tool-calls");
        req.messages.push(Message::assistant(text(&events), made.clone()));
        req.messages.push(Message::tool(&made[0].id, "{\"temperature\":22,\"condition\":\"sunny\"}"));
        assert_eq!(encode(&req).unwrap(), request_body(&c, 1));
        let (events, _) = replay(response_body(&c, 1));
        assert!(!text(&events).is_empty());
        assert_eq!(finish(&events).0, "stop");
    }

    #[test]
    fn image_tool_result_matches_recording() {
        let Some(c) = cassette(&format!("{DIR}/anthropic-opus-4-7-image-tool-result.json")) else { return };
        let recorded = request_body(&c, 0);
        let data = recorded.pointer("/messages/2/content/0/content/1/source/data").unwrap().as_str().unwrap();
        let req = Request {
            model: "claude-opus-4-7".into(),
            system: vec!["Read images carefully. Reply only with the visible text, lowercase, no punctuation.".into()],
            messages: vec![
                Message::User("Use the read_screenshot tool, then reply with the words shown.".into()),
                Message::assistant("", vec![call("call_screenshot_1", "read_screenshot", "")]),
                Message::Tool {
                    call_id: "call_screenshot_1".into(),
                    content: "Image read successfully".into(),
                    is_error: false,
                    attachments: vec![Part::Media { mime: "image/png".into(), data: data.into(), filename: None }],
                },
            ],
            tools: vec![Tool {
                name: "read_screenshot".into(),
                description: "Capture a screenshot of the current screen.".into(),
                parameters: json!({"type": "object", "properties": {}, "additionalProperties": false}),
            }],
            max_tokens: Some(40),
            options: json!({"toolStreaming": false}),
            ..Default::default()
        };
        assert_eq!(encode(&req).unwrap(), recorded);
        let (events, _) = replay(response_body(&c, 0));
        assert_eq!(finish(&events).0, "stop");
    }

    #[test]
    fn malformed_order_recordings() {
        let tool = Tool { name: "get_weather".into(), description: "Get weather".into(), parameters: json!({"type": "object", "properties": {}}) };
        let Some(c) = cassette(&format!("{DIR}/accepts-malformed-assistant-tool-order-with-default-patch.json")) else { return };
        let req = Request {
            model: "claude-haiku-4-5-20251001".into(),
            messages: vec![
                Message::assistant("I will check the weather.", Vec::new()),
                Message::assistant("", vec![call("call_1", "get_weather", "{\"city\":\"Paris\"}")]),
                Message::tool("call_1", "{\"temperature\":\"72F\"}"),
                Message::User("Use that result to answer briefly.".into()),
            ],
            tools: vec![tool],
            max_tokens: Some(4096),
            options: json!({"toolStreaming": false}),
            ..Default::default()
        };
        let mut recorded = request_body(&c, 0);
        // Recorded with a hand-placed marker, and by a runtime that leaves same-role turns apart; the SDK joins them.
        recorded["messages"][3]["content"][0].as_object_mut().unwrap().remove("cache_control");
        let mut turns = recorded["messages"].as_array().unwrap().clone();
        let (question, results) = (turns.remove(3), turns.remove(2));
        let call = turns.remove(1);
        turns[0]["content"].as_array_mut().unwrap().extend(call["content"].as_array().unwrap().clone());
        turns.push(json!({"role": "user", "content": [results["content"][0], question["content"][0]]}));
        recorded["messages"] = turns.into();
        assert_eq!(encode(&req).unwrap(), recorded);
        let (events, _) = replay(response_body(&c, 0));
        assert_eq!(finish(&events).0, "stop");

        let Some(c) = cassette(&format!("{DIR}/rejects-malformed-assistant-tool-order-without-patch.json")) else { return };
        assert_eq!(c["interactions"][0]["response"]["status"], 400);
        let error = Error::Status { status: 400, body: response_body(&c, 0).into(), headers: Vec::new() };
        let class = crate::retry::classify(&error, "anthropic");
        assert!(!class.retryable && !class.overflow);
        assert!(class.message.contains("`tool_use` ids were found without `tool_result` blocks"), "{}", class.message);
    }

    #[test]
    fn cache_markers_and_cache_usage() {
        let Some(c) = cassette("llm/test/fixtures/recordings/anthropic-messages-cache/writes-then-reads-cache-control-on-identical-second-call.json") else {
            return;
        };
        let recorded = request_body(&c, 0);
        let req = Request {
            model: "claude-haiku-4-5-20251001".into(),
            system: vec![recorded["system"][0]["text"].as_str().unwrap().into()],
            messages: vec![Message::User("Say hi.".into())],
            max_tokens: Some(16),
            temperature: Some(0.0),
            cache: true,
            ..Default::default()
        };
        let mut expected = recorded.clone();
        expected["messages"][0]["content"][0]["cache_control"] = ephemeral();
        assert_eq!(encode(&req).unwrap(), expected);
        let (first, _) = replay(response_body(&c, 0));
        let (second, _) = replay(response_body(&c, 1));
        let (_, written) = finish(&first);
        let (_, read) = finish(&second);
        assert!(written.cache_write > 0 && written.input >= written.cache_write);
        assert_eq!((read.cache_read, read.cache_write, read.input, read.output), (5752, 0, 5761, 5));
    }

    #[test]
    fn session_tool_loop_recording() {
        let Some(c) = cassette("opencode/test/fixtures/recordings/session/native-anthropic-tool-loop.json") else { return };
        let system = "Answer using tools when appropriate.\nUse the get_weather tool exactly once to look up Paris, then reply with exactly: Paris is sunny.";
        let mut req = Request {
            model: "claude-haiku-4-5-20251001".into(),
            system: vec![system.into()],
            messages: vec![Message::User("What is the weather in Paris?".into())],
            tools: vec![Tool {
                name: "get_weather".into(),
                description: "Get the current weather for a city.".into(),
                parameters: json!({"$schema": "http://json-schema.org/draft-07/schema#", "type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"], "additionalProperties": false}),
            }],
            max_tokens: Some(32000),
            options: json!({"toolStreaming": false}),
            temperature: Some(0.0),
            cache: true,
            ..Default::default()
        };
        // The native runtime additionally marks the last tool; system and last-user markers coincide.
        let mut recorded = request_body(&c, 0);
        recorded["tools"][0].as_object_mut().unwrap().remove("cache_control");
        assert_eq!(encode(&req).unwrap(), recorded);
        let (events, _) = replay(response_body(&c, 0));
        let made = calls(&events);
        assert_eq!(made.len(), 1);
        req.cache = false;
        // The recorded harness replayed the call with a schema-shaped placeholder input.
        req.messages.push(Message::assistant("", vec![call(&made[0].id, "get_weather", "{\"city\":{}}")]));
        req.messages.push(Message::tool(&made[0].id, "{\"temperature\":22,\"condition\":\"sunny\"}"));
        let mut recorded = request_body(&c, 1);
        recorded["tools"][0].as_object_mut().unwrap().remove("cache_control");
        recorded["system"][0].as_object_mut().unwrap().remove("cache_control");
        recorded["messages"][0]["content"][0].as_object_mut().unwrap().remove("cache_control");
        assert_eq!(encode(&req).unwrap(), recorded);
        let (events, _) = replay(response_body(&c, 1));
        assert_eq!(text(&events), "Paris is sunny.");
    }

    #[test]
    fn thinking_tool_results_and_errors() {
        let req = Request {
            model: "claude-sonnet-4-5".into(),
            messages: vec![
                Message::UserParts(vec![Part::Text("look".into()), Part::Media { mime: "image/png".into(), data: "AAAA".into(), filename: None }]),
                Message::Assistant {
                    text: "ok".into(),
                    reasoning: "ignored".into(),
                    tool_calls: vec![call("a.b", "x", "{}"), call("c", "y", "")],
                    reasoning_parts: vec![
                        Reasoning { text: "plan".into(), signature: Some("sig".into()), ..Default::default() },
                        Reasoning { text: "unsigned".into(), ..Default::default() },
                    ],
                },
                Message::tool("a.b", "one"),
                Message::Tool { call_id: "c".into(), content: "boom".into(), is_error: true, attachments: Vec::new() },
            ],
            tool_choice: Some(ToolChoice::None),
            tools: vec![weather()],
            top_k: Some(40),
            options: json!({"thinking": {"type": "adaptive", "display": "summarized", "blockBinding": {"prefixMismatchBehavior": "drop_block"}}, "effort": "high"}),
            cache: true,
            ..Default::default()
        };
        let body = encode(&req).unwrap();
        assert_eq!(
            body,
            json!({
                "model": "claude-sonnet-4-5",
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "look"}, {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}]},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "plan", "signature": "sig"},
                        {"type": "text", "text": "ok"},
                        {"type": "tool_use", "id": "a_b", "name": "x", "input": {}},
                        {"type": "tool_use", "id": "c", "name": "y", "input": {}, "cache_control": {"type": "ephemeral"}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "a_b", "content": "one"},
                        {"type": "tool_result", "tool_use_id": "c", "content": "boom", "is_error": true, "cache_control": {"type": "ephemeral"}}
                    ]}
                ],
                "stream": true,
                "max_tokens": 64000,
                "thinking": {"type": "adaptive", "display": "summarized", "block_binding": {"prefix_mismatch_behavior": "drop_block"}},
                "output_config": {"effort": "high"}
            })
        );
        assert_eq!(betas(&req), [BLOCK_BINDING_BETA]);
        let enabled = Request { options: json!({"thinking": {"type": "enabled", "budgetTokens": 16000}}), ..Default::default() };
        assert_eq!(encode(&enabled).unwrap()["thinking"], json!({"type": "enabled", "budget_tokens": 16000}));
        assert!(betas(&enabled).is_empty());
        let media = |mime: &str, name: Option<&str>| Part::Media { mime: mime.into(), data: "aGk=".into(), filename: name.map(str::to_string) };
        let video = Request { messages: vec![Message::UserParts(vec![media("video/mp4", None)])], ..Default::default() };
        assert!(matches!(encode(&video), Err(Error::Config(m)) if m == "'media type: video/mp4' functionality not supported."));
        let docs = Request { messages: vec![Message::UserParts(vec![media("application/pdf", Some("a.pdf")), media("text/plain", None)])], ..Default::default() };
        assert_eq!(
            encode(&docs).unwrap()["messages"][0]["content"],
            json!([
                {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "aGk="}, "title": "a.pdf"},
                {"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "hi"}}
            ])
        );
        assert_eq!(betas(&docs), [PDF_BETA]);
    }

    #[test]
    fn sdk_request_rules() {
        let base = Request { model: "claude-sonnet-4-5".into(), messages: vec![Message::User("q".into())], max_tokens: Some(32_000), temperature: Some(1.5), top_p: Some(0.9), top_k: Some(5), ..Default::default() };
        // Temperature is clamped, and wins over top_p on Claude models.
        let body = encode(&base).unwrap();
        assert_eq!((&body["temperature"], body.get("top_p"), &body["top_k"], &body["max_tokens"]), (&json!(1), None, &json!(5), &json!(32000)));
        // A thinking budget is added to the output cap, up to the model's ceiling, and sampling parameters go.
        let thinking = Request { options: json!({"thinking": {"type": "enabled", "budgetTokens": 16000}}), ..base.clone() };
        let body = encode(&thinking).unwrap();
        assert_eq!((&body["max_tokens"], body.get("temperature"), body.get("top_k")), (&json!(48000), None, None));
        let body = encode(&Request { options: json!({"thinking": {"type": "enabled", "budgetTokens": 40000}}), ..base.clone() }).unwrap();
        assert_eq!(body["max_tokens"], json!(64000));
        assert_eq!(encode(&Request { options: json!({"thinking": {"type": "enabled"}}), ..base.clone() }).unwrap()["thinking"], json!({"type": "enabled", "budget_tokens": 1024}));
        // Newer models refuse sampling parameters outright; other vendors' models keep both.
        let body = encode(&Request { model: "claude-opus-4-7".into(), max_tokens: None, ..base.clone() }).unwrap();
        assert_eq!((body.get("temperature"), body.get("top_k"), &body["max_tokens"]), (None, None, &json!(128000)));
        let body = encode(&Request { model: "MiniMax-M2".into(), temperature: Some(0.5), max_tokens: None, ..base.clone() }).unwrap();
        assert_eq!((&body["temperature"], &body["top_p"], &body["max_tokens"]), (&json!(0.5), &json!(0.9), &json!(4096)));

        // Tools stream their input eagerly by default; structured-output models announce the beta.
        let tooled = Request { tools: vec![weather()], tool_choice: Some(ToolChoice::Auto), options: json!({"cacheControl": {"type": "ephemeral"}, "effort": "max"}), ..base.clone() };
        let body = encode(&tooled).unwrap();
        assert_eq!((&body["tools"][0]["eager_input_streaming"], &body["tool_choice"], &body["cache_control"], &body["output_config"]), (&json!(true), &json!({"type": "auto"}), &json!({"type": "ephemeral"}), &json!({"effort": "max"})));
        assert_eq!(betas(&tooled), [STRUCTURED_OUTPUTS_BETA]);
        assert!(betas(&Request { model: "claude-sonnet-4-0".into(), ..tooled.clone() }).is_empty() && betas(&base).is_empty());

        // Same-role turns are joined, tool calls move behind text, and a final assistant turn loses trailing whitespace.
        let call = |id: &str| ToolCall { id: id.into(), name: "t".into(), arguments: "{}".into() };
        let redacted = Reasoning { encrypted: Some("blob".into()), ..Default::default() };
        let merged = Request {
            messages: vec![
                Message::assistant("first", vec![call("a")]),
                Message::Assistant { text: "second".into(), reasoning: String::new(), tool_calls: Vec::new(), reasoning_parts: vec![redacted] },
                Message::tool("a", "done"),
                Message::User("next".into()),
                Message::assistant("draft  \n", Vec::new()),
            ],
            cache: true,
            ..Default::default()
        };
        assert_eq!(
            encode(&merged).unwrap()["messages"],
            json!([
                {"role": "assistant", "content": [{"type": "text", "text": "first"}, {"type": "tool_use", "id": "a", "name": "t", "input": {}}, {"type": "redacted_thinking", "data": "blob"}, {"type": "text", "text": "second"}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "a", "content": "done"}, {"type": "text", "text": "next", "cache_control": {"type": "ephemeral"}}]},
                {"role": "assistant", "content": [{"type": "text", "text": "draft", "cache_control": {"type": "ephemeral"}}]}
            ])
        );
        let mut decoder = Decoder::default();
        for data in [r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"blob"}}"#, r#"{"type":"content_block_stop","index":0}"#, r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#] {
            decoder.push(data).unwrap();
        }
        assert_eq!(decoder.finish().unwrap().reasoning, [Reasoning { encrypted: Some("blob".into()), ..Default::default() }]);
    }

    #[test]
    fn decodes_thinking_signature_and_stream_errors() {
        let stream = [
            r#"{"type":"message_start","message":{"usage":{"input_tokens":10,"cache_creation_input_tokens":4,"cache_read_input_tokens":6,"output_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me "}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"think."}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"c2ln"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"server_tool_use","id":"srv_1","name":"web_search","input":{}}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"text","text":"Hi"}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":9}}"#,
            r#"{"type":"message_stop"}"#,
        ];
        let mut decoder = Decoder::default();
        let events: Vec<Event> = stream.iter().flat_map(|data| decoder.push(data).unwrap()).collect();
        assert_eq!(
            events,
            [
                Event::ReasoningDelta("Let me ".into()),
                Event::ReasoningDelta("think.".into()),
                Event::TextDelta("Hi".into()),
                Event::Finish { reason: "length".into(), usage: Usage { input: 20, output: 9, reasoning: 0, cache_read: 6, cache_write: 4, total: 29 } },
            ]
        );
        let completion = decoder.finish().unwrap();
        assert_eq!(completion.reasoning, [Reasoning { text: "Let me think.".into(), signature: Some("c2ln".into()), ..Default::default() }]);

        let mut decoder = Decoder::default();
        let error = decoder.push(r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#).unwrap_err();
        assert!(matches!(&error, Error::Provider { code: Some(code), message } if code == "overloaded_error" && message == "overloaded_error: Overloaded"));
        assert!(crate::retry::classify(&error, "anthropic").retryable);
        assert!(matches!(Decoder::default().finish(), Err(Error::Stream(_))));
        let mut decoder = Decoder::default();
        assert!(decoder.push(r#"{"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{"}}"#).is_err());
    }
}
