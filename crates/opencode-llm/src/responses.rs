// OpenAI Responses: request encoding and stream decoding.
use crate::{Completion, Error, Event, Message, Part, Reasoning, Request, ToolCall, ToolChoice, Usage, number};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub const PATH: &str = "/responses";
const ROUTE: &str = "OpenAI Responses";
const ENCRYPTED_REASONING: &str = "reasoning.encrypted_content";
// Options copied to the body under their wire names.
const PASSTHROUGH: [(&str, &str); 8] = [
    ("maxToolCalls", "max_tool_calls"),
    ("metadata", "metadata"),
    ("parallelToolCalls", "parallel_tool_calls"),
    ("previousResponseId", "previous_response_id"),
    ("user", "user"),
    ("promptCacheRetention", "prompt_cache_retention"),
    ("safetyIdentifier", "safety_identifier"),
    ("truncation", "truncation"),
];
static O_SERIES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^o(\d+)(?:-|$)").unwrap());
static GPT_VERSION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^gpt-(\d+)(?:\.(\d+))?(?:-(.+))?$").unwrap());
const INCLUDABLES: [&str; 8] = [
    "file_search_call.results",
    "web_search_call.results",
    "web_search_call.action.sources",
    "message.input_image.image_url",
    "computer_call_output.output.image_url",
    "code_interpreter_call.outputs",
    "reasoning.encrypted_content",
    "message.output_text.logprobs",
];
const SERVICE_TIERS: [&str; 5] = ["auto", "default", "flex", "priority", "ultrafast"];

fn strip_null_variants(value: &Value) -> Value {
    match value {
        Value::Array(items) => items.iter().map(strip_null_variants).collect(),
        Value::Object(map) => {
            let mut out: Map<String, Value> = map.iter().filter(|(k, _)| *k != "anyOf").map(|(k, v)| (k.clone(), strip_null_variants(v))).collect();
            if let Some(variants) = map.get("anyOf").and_then(Value::as_array) {
                let kept: Vec<Value> = variants.iter().filter(|v| v.get("type").and_then(Value::as_str) != Some("null")).map(strip_null_variants).collect();
                match kept.as_slice() {
                    [Value::Object(only)] => out.extend(only.clone()),
                    _ => {
                        out.insert("anyOf".into(), kept.into());
                    }
                }
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

// Function parameters must be a root object schema; root unions are flattened and null variants dropped.
pub fn tool_schema(schema: &Value) -> Value {
    let Some(source) = schema.as_object() else { return json!({"type": "object"}) };
    let variants: Vec<&Map<String, Value>> = source.get("anyOf").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_object).collect();
    let mut flat: Map<String, Value> = source.iter().filter(|(k, _)| variants.is_empty() || *k != "anyOf").map(|(k, v)| (k.clone(), v.clone())).collect();
    flat.insert("type".into(), "object".into());
    if !variants.is_empty() {
        let mut properties = Map::new();
        for variant in variants.iter().rev() {
            for (k, v) in variant.get("properties").and_then(Value::as_object).into_iter().flatten() {
                properties.insert(k.clone(), v.clone());
            }
        }
        flat.insert("properties".into(), properties.into());
        flat.insert("additionalProperties".into(), false.into());
    }
    strip_null_variants(&Value::Object(flat))
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Config {
    // The Copilot SDK dialect: reasoning is only ever replayed by reference.
    pub copilot: bool,
    // Not the OpenAI SDK (xAI): no reasoning-model gate, so the system role, sampling and reasoning options pass as given.
    pub ungated: bool,
}

struct Capabilities {
    reasoning: bool,
    // GPT-5.1 and later accept sampling parameters once reasoning is switched off.
    non_reasoning_parameters: bool,
}

// The SDK's model-id heuristics: o-series and GPT-5+ (except `gpt-5-chat*`) reason.
fn capabilities(model: &str) -> Capabilities {
    let o_series = O_SERIES.is_match(model);
    let gpt = GPT_VERSION.captures(model).map(|c| {
        let number = |i: usize| c.get(i).and_then(|m| m.as_str().parse::<u64>().ok());
        (number(1).unwrap_or(0), number(2), c.get(3).map(|m| m.as_str().to_string()))
    });
    let chat = gpt.as_ref().is_some_and(|(_, minor, variant)| minor.is_none() && variant.as_deref().is_some_and(|v| v.starts_with("chat")));
    Capabilities {
        reasoning: o_series || gpt.as_ref().is_some_and(|(major, _, _)| *major >= 5 && !chat),
        non_reasoning_parameters: gpt.as_ref().is_some_and(|(major, minor, _)| *major > 5 || (*major == 5 && minor.unwrap_or(0) >= 1)),
    }
}

fn reasons(req: &Request) -> bool {
    req.options.get("forceReasoning").and_then(Value::as_bool).unwrap_or_else(|| capabilities(&req.model).reasoning)
}

fn content(part: &Part, index: usize) -> Result<Value, Error> {
    match part {
        Part::Text(text) => Ok(json!({"type": "input_text", "text": text})),
        Part::Media { mime, data, filename } => {
            let mime = if mime == "image/*" { "image/jpeg" } else { mime.as_str() };
            let url = format!("data:{mime};base64,{data}");
            if mime.starts_with("image/") {
                Ok(json!({"type": "input_image", "image_url": url}))
            } else if mime == "application/pdf" {
                let name = filename.clone().unwrap_or_else(|| format!("part-{index}.pdf"));
                Ok(json!({"type": "input_file", "filename": name, "file_data": url}))
            } else {
                Err(Error::Config(format!("'file part media type {mime}' functionality not supported.")))
            }
        }
    }
}

fn store(req: &Request) -> Option<bool> {
    req.options.get("store").and_then(Value::as_bool)
}

fn input(req: &Request, config: &Config) -> Result<Vec<Value>, Error> {
    let mut input = Vec::new();
    let mode = req.options.get("systemMessageMode").and_then(Value::as_str).unwrap_or(if reasons(req) && !config.ungated { "developer" } else { "system" });
    if mode != "remove" {
        input.extend(req.system.iter().map(|text| json!({"role": mode, "content": text})));
    }
    let store = store(req);
    for message in &req.messages {
        match message {
            Message::User(text) => input.push(json!({"role": "user", "content": [{"type": "input_text", "text": text}]})),
            Message::UserParts(parts) => {
                let parts = parts.iter().enumerate().map(|(i, p)| content(p, i)).collect::<Result<Vec<_>, _>>()?;
                input.push(json!({"role": "user", "content": parts}));
            }
            Message::Assistant { text, tool_calls, reasoning_parts, .. } => {
                // Summary blocks of one reasoning item are merged; the encrypted state may arrive on any of them.
                let mut items: Vec<(Option<&str>, Vec<Value>, Option<&str>)> = Vec::new();
                for r in reasoning_parts.iter().filter(|r| r.item_id.is_some() || r.encrypted.is_some()) {
                    let id = r.item_id.as_deref();
                    let position = items.iter().position(|(existing, _, _)| id.is_some() && *existing == id);
                    let index = position.unwrap_or_else(|| {
                        items.push((id, Vec::new(), None));
                        items.len() - 1
                    });
                    if !r.text.is_empty() {
                        items[index].1.push(json!({"type": "summary_text", "text": r.text}));
                    }
                    items[index].2 = r.encrypted.as_deref().or(items[index].2);
                }
                for (id, summary, encrypted) in items {
                    // Item ids only survive when the response is stored; otherwise the encrypted state is the replay.
                    match (store, id, encrypted) {
                        (Some(true), Some(id), _) => input.push(json!({"type": "item_reference", "id": id})),
                        (_, _, Some(encrypted)) if !config.copilot => input.push(json!({"type": "reasoning", "summary": summary, "encrypted_content": encrypted})),
                        _ => {}
                    }
                }
                if !text.is_empty() {
                    input.push(json!({"role": "assistant", "content": [{"type": "output_text", "text": text}]}));
                }
                for call in tool_calls {
                    let arguments = if call.arguments.trim().is_empty() { "{}" } else { &call.arguments };
                    input.push(json!({"type": "function_call", "call_id": call.id, "name": call.name, "arguments": arguments}));
                }
            }
            Message::Tool { call_id, content: text, attachments, .. } => {
                let output: Value = if attachments.is_empty() {
                    text.clone().into()
                } else {
                    let mut items = vec![json!({"type": "input_text", "text": text})];
                    for a in attachments {
                        items.push(match a {
                            Part::Media { mime, data, filename } if !mime.starts_with("image/") => {
                                json!({"type": "input_file", "filename": filename.as_deref().unwrap_or("data"), "file_data": format!("data:{mime};base64,{data}")})
                            }
                            other => content(other, 0)?,
                        });
                    }
                    items.into()
                };
                input.push(json!({"type": "function_call_output", "call_id": call_id, "output": output}));
            }
        }
    }
    Ok(input)
}

pub fn encode(req: &Request) -> Result<Value, Error> {
    encode_with(req, &Config::default())
}

pub fn encode_with(req: &Request, config: &Config) -> Result<Value, Error> {
    let option = |key: &str| req.options.get(key).filter(|v| !v.is_null());
    let text = |key: &str| option(key).and_then(Value::as_str).filter(|s| !s.is_empty());
    let gated = !config.ungated;
    let reasons = reasons(req);
    let mut body = Map::new();
    body.insert("model".into(), req.model.clone().into());
    body.insert("input".into(), input(req, config)?.into());
    if let Some(instructions) = text("instructions") {
        body.insert("instructions".into(), instructions.into());
    }
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| json!({"type": "function", "name": t.name, "description": t.description, "parameters": t.parameters, "strict": false}))
            .collect();
        body.insert("tools".into(), tools.into());
        match &req.tool_choice {
            Some(ToolChoice::Auto) => body.insert("tool_choice".into(), "auto".into()),
            Some(ToolChoice::None) => body.insert("tool_choice".into(), "none".into()),
            Some(ToolChoice::Required) => body.insert("tool_choice".into(), "required".into()),
            Some(ToolChoice::Tool(name)) => body.insert("tool_choice".into(), json!({"type": "function", "name": name})),
            None => None,
        };
    }
    let store = store(req);
    if let Some(store) = store {
        body.insert("store".into(), store.into());
    }
    if let Some(tier) = text("serviceTier").filter(|t| SERVICE_TIERS.contains(t)) {
        body.insert("service_tier".into(), tier.into());
    }
    if let Some(key) = text("promptCacheKey") {
        body.insert("prompt_cache_key".into(), key.into());
    }
    let mut include: Vec<&str> = option("include").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).filter(|v| INCLUDABLES.contains(v)).collect();
    // Stateless reasoning needs the encrypted state back.
    if store == Some(false) && reasons && gated && !include.contains(&ENCRYPTED_REASONING) {
        include.push(ENCRYPTED_REASONING);
    }
    if !include.is_empty() {
        body.insert("include".into(), include.into());
    }
    let effort = text("reasoningEffort");
    let mut reasoning = Map::new();
    for (key, name) in [("reasoningEffort", "effort"), ("reasoningSummary", "summary"), ("reasoningMode", "mode"), ("reasoningContext", "context")] {
        if let Some(value) = text(key).filter(|_| reasons || !gated) {
            reasoning.insert(name.into(), value.into());
        }
    }
    if !reasoning.is_empty() {
        body.insert("reasoning".into(), reasoning.into());
    }
    if let Some(verbosity) = text("textVerbosity").filter(|v| ["low", "medium", "high"].contains(v)) {
        body.insert("text".into(), json!({"verbosity": verbosity}));
    }
    if let Some(v) = req.max_tokens {
        body.insert("max_output_tokens".into(), v.into());
    }
    // Reasoning models take no sampling parameters, unless reasoning is off on a model that allows it.
    let sampling = !gated || !reasons || (effort == Some("none") && capabilities(&req.model).non_reasoning_parameters);
    if let Some(v) = req.temperature.filter(|_| sampling) {
        body.insert("temperature".into(), number(v));
    }
    if let Some(v) = req.top_p.filter(|_| sampling) {
        body.insert("top_p".into(), number(v));
    }
    for (key, name) in PASSTHROUGH {
        if let Some(value) = option(key) {
            body.insert(name.into(), value.clone());
        }
    }
    body.insert("stream".into(), true.into());
    Ok(Value::Object(body))
}

#[derive(Default)]
struct ReasoningItem {
    encrypted: Option<String>,
    parts: BTreeMap<u64, String>,
}

#[derive(Default)]
pub struct Decoder {
    calls: HashMap<String, ToolCall>,
    reasoning: HashMap<String, ReasoningItem>,
    has_call: bool,
    done: bool,
    completion: Completion,
}

fn text(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

fn usage(usage: Option<&Value>) -> Usage {
    let n = |pointer: &str| usage.and_then(|u| u.pointer(pointer)).and_then(Value::as_u64);
    let input = n("/input_tokens").unwrap_or(0);
    let output = n("/output_tokens").unwrap_or(0);
    Usage {
        input,
        output,
        reasoning: n("/output_tokens_details/reasoning_tokens").unwrap_or(0),
        cache_read: n("/input_tokens_details/cached_tokens").unwrap_or(0),
        cache_write: 0,
        total: n("/total_tokens").unwrap_or(input + output),
    }
}

fn failure(event: &Value, fallback: &str) -> Error {
    let first = |pointers: &[&str]| pointers.iter().find_map(|p| text(event.pointer(p)));
    let code = first(&["/code", "/error/code", "/response/error/code"]);
    let message = first(&["/message", "/error/message", "/response/error/message"]);
    let message = match (&code, message) {
        (Some(code), Some(message)) => format!("{code}: {message}"),
        (_, Some(message)) => message,
        (Some(code), None) => code.clone(),
        (None, None) => fallback.to_string(),
    };
    Error::Provider { code, message }
}

impl Decoder {
    pub fn push(&mut self, data: &str) -> Result<Vec<Event>, Error> {
        if self.done || data.trim().is_empty() || data.trim() == "[DONE]" {
            return Ok(Vec::new());
        }
        let event: Value = serde_json::from_str(data).map_err(|_| Error::Stream("Invalid openai/openai-responses stream event".into()))?;
        self.completion.nano_aiu = crate::copilot::nano_aiu(&event).or(self.completion.nano_aiu);
        let item = &event["item"];
        let item_id = text(event.get("item_id"));
        let delta = text(event.get("delta"));
        let mut events = Vec::new();
        match event.get("type").and_then(Value::as_str).unwrap_or_default() {
            "response.output_text.delta" => events.extend(delta.map(Event::TextDelta)),
            "response.reasoning_text.delta" | "response.reasoning_summary.delta" | "response.reasoning_summary_text.delta" => {
                if let Some(delta) = delta {
                    let entry = self.reasoning.entry(item_id.unwrap_or_else(|| "reasoning-0".into())).or_default();
                    entry.parts.entry(event.get("summary_index").and_then(Value::as_u64).unwrap_or(0)).or_default().push_str(&delta);
                    events.push(Event::ReasoningDelta(delta));
                }
            }
            "response.reasoning_summary_part.added" => {
                let index = event.get("summary_index").and_then(Value::as_u64);
                if let (Some(id), Some(index)) = (item_id, index) {
                    let entry = self.reasoning.entry(id).or_default();
                    if index > 0 && !entry.parts.contains_key(&index) {
                        events.push(Event::ReasoningDelta("\n\n".into()));
                    }
                    entry.parts.entry(index).or_default();
                }
            }
            "response.output_item.added" => match (item.get("type").and_then(Value::as_str), text(item.get("id"))) {
                (Some("reasoning"), Some(id)) => self.reasoning.entry(id).or_default().encrypted = text(item.get("encrypted_content")),
                (Some("function_call"), Some(id)) => {
                    let call = ToolCall {
                        id: text(item.get("call_id")).unwrap_or_else(|| id.clone()),
                        name: text(item.get("name")).unwrap_or_default(),
                        arguments: text(item.get("arguments")).unwrap_or_default(),
                    };
                    self.calls.insert(id, call);
                }
                _ => {}
            },
            "response.function_call_arguments.delta" => {
                if let (Some(id), Some(delta)) = (item_id, delta) {
                    match self.calls.get_mut(&id) {
                        Some(call) => call.arguments.push_str(&delta),
                        None => return Err(Error::Stream(format!("{ROUTE} tool argument delta is missing its tool call"))),
                    }
                }
            }
            "response.output_item.done" => match item.get("type").and_then(Value::as_str) {
                Some("function_call") => {
                    if let (Some(id), Some(call_id), Some(name)) = (text(item.get("id")), text(item.get("call_id")), text(item.get("name"))) {
                        let mut call = self.calls.remove(&id).unwrap_or(ToolCall { id: call_id, name, arguments: String::new() });
                        // The completed item is authoritative over accumulated deltas.
                        if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
                            call.arguments = arguments.to_string();
                        }
                        if call.arguments.is_empty() {
                            call.arguments = "{}".into();
                        }
                        if serde_json::from_str::<Value>(&call.arguments).is_err() {
                            return Err(Error::Stream(format!("Invalid JSON input for openai-responses tool call {}", call.name)));
                        }
                        self.has_call = true;
                        events.push(Event::ToolCall(call));
                    }
                }
                Some("reasoning") => {
                    if let Some(id) = text(item.get("id")) {
                        let entry = self.reasoning.remove(&id).unwrap_or_default();
                        let encrypted = text(item.get("encrypted_content")).or(entry.encrypted);
                        let mut parts: Vec<String> = entry.parts.into_values().collect();
                        if parts.is_empty() {
                            parts.push(String::new());
                        }
                        for part in parts {
                            self.completion.reasoning.push(Reasoning { text: part, signature: None, item_id: Some(id.clone()), encrypted: encrypted.clone() });
                        }
                    }
                }
                _ => {}
            },
            "response.completed" | "response.incomplete" => {
                self.done = true;
                self.completion.response_id = text(event.pointer("/response/id"));
                let reason = match event.pointer("/response/incomplete_details/reason").and_then(Value::as_str) {
                    Some("max_output_tokens") => "length",
                    Some("content_filter") => "content-filter",
                    Some(_) if !self.has_call => "unknown",
                    _ if self.has_call => "tool-calls",
                    _ => "stop",
                };
                events.push(Event::Finish { reason: reason.to_string(), usage: usage(event.pointer("/response/usage")) });
            }
            "response.failed" => return Err(failure(&event, "OpenAI Responses response failed")),
            "error" => return Err(failure(&event, "OpenAI Responses stream error")),
            _ => {}
        }
        Ok(events)
    }

    pub fn finish(self) -> Result<Completion, Error> {
        if !self.done {
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

    const DIR: &str = "llm/test/fixtures/recordings/openai-responses";

    pub(crate) fn replay(body: &str) -> (Vec<Event>, Completion) {
        let mut sse = sse::Decoder::default();
        let mut decoder = Decoder::default();
        let mut events = Vec::new();
        for piece in body.as_bytes().chunks(13) {
            for data in sse.feed(piece) {
                events.extend(decoder.push(&data).unwrap());
            }
        }
        for data in sse.finish() {
            events.extend(decoder.push(&data).unwrap());
        }
        (events, decoder.finish().unwrap())
    }

    fn text_of(events: &[Event]) -> String {
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

    fn weather(description: &str, schema: Value) -> Tool {
        Tool { name: "get_weather".into(), description: description.into(), parameters: schema }
    }

    // The recordings come from the native runtime, which always uses the `system` role.
    fn native(mut options: Value) -> Value {
        options["systemMessageMode"] = "system".into();
        options
    }

    fn city_schema() -> Value {
        json!({"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"], "additionalProperties": false})
    }

    #[test]
    fn text_request_and_stream_match_recording() {
        let Some(c) = cassette(&format!("{DIR}/gpt-5-5-streams-text.json")) else { return };
        let req = Request {
            model: "gpt-5.5".into(),
            system: vec!["You are concise.".into()],
            messages: vec![Message::User("Reply with exactly: Hello!".into())],
            max_tokens: Some(80),
            options: native(json!({})),
            ..Default::default()
        };
        assert_eq!(encode(&req).unwrap(), request_body(&c, 0));
        let (events, completion) = replay(response_body(&c, 0));
        assert_eq!(text_of(&events), "Hello!");
        let (reason, usage) = finish(&events);
        assert_eq!(reason, "stop");
        assert!(usage.input > 0 && usage.output > 0 && usage.total == usage.input + usage.output);
        assert!(completion.response_id.unwrap().starts_with("resp_"));
    }

    #[test]
    fn tool_call_request_and_stream_match_recording() {
        let Some(c) = cassette(&format!("{DIR}/gpt-5-5-streams-tool-call.json")) else { return };
        let req = Request {
            model: "gpt-5.5".into(),
            system: vec!["Call tools exactly as requested.".into()],
            messages: vec![Message::User("Call get_weather with city exactly Paris.".into())],
            tools: vec![weather("Get current weather for a city.", city_schema())],
            tool_choice: Some(ToolChoice::Tool("get_weather".into())),
            max_tokens: Some(80),
            options: native(json!({})),
            ..Default::default()
        };
        assert_eq!(encode(&req).unwrap(), request_body(&c, 0));
        let (events, _) = replay(response_body(&c, 0));
        let made = calls(&events);
        assert_eq!(made.len(), 1);
        assert_eq!((made[0].name.as_str(), made[0].arguments.as_str()), ("get_weather", "{\"city\":\"Paris\"}"));
        assert!(made[0].id.starts_with("call_"));
        assert_eq!(finish(&events).0, "tool-calls");
    }

    #[test]
    fn tool_loop_matches_recording() {
        let Some(c) = cassette(&format!("{DIR}/gpt-5-5-drives-a-tool-loop.json")) else { return };
        let mut req = Request {
            model: "gpt-5.5".into(),
            system: vec!["Use the get_weather tool, then answer in one short sentence.".into()],
            messages: vec![Message::User("What is the weather in Paris?".into())],
            tools: vec![weather("Get current weather for a city.", city_schema())],
            max_tokens: Some(80),
            options: native(json!({})),
            ..Default::default()
        };
        assert_eq!(encode(&req).unwrap(), request_body(&c, 0));
        let (events, _) = replay(response_body(&c, 0));
        let made = calls(&events);
        // Recorded without reasoning replay, so only the call and its output follow.
        req.messages.push(Message::assistant(text_of(&events), made.clone()));
        req.messages.push(Message::tool(&made[0].id, "{\"temperature\":22,\"condition\":\"sunny\"}"));
        assert_eq!(encode(&req).unwrap(), request_body(&c, 1));
        let (events, _) = replay(response_body(&c, 1));
        assert!(!text_of(&events).is_empty());
        assert_eq!(finish(&events).0, "stop");
    }

    #[test]
    fn image_tool_result_matches_recording() {
        let Some(c) = cassette(&format!("{DIR}/openai-responses-gpt-5-5-image-tool-result.json")) else { return };
        let recorded = request_body(&c, 0);
        let url = recorded.pointer("/input/3/output/1/image_url").unwrap().as_str().unwrap();
        let mut req = Request {
            model: "gpt-5.5".into(),
            system: vec!["Read images carefully. Reply only with the visible text, lowercase, no punctuation.".into()],
            messages: vec![
                Message::User("Use the read_screenshot tool, then reply with the words shown.".into()),
                Message::assistant("", vec![ToolCall { id: "call_screenshot_1".into(), name: "read_screenshot".into(), arguments: String::new() }]),
                Message::Tool {
                    call_id: "call_screenshot_1".into(),
                    content: "Image read successfully".into(),
                    is_error: false,
                    attachments: vec![Part::Media { mime: "image/png".into(), data: url.strip_prefix("data:image/png;base64,").unwrap().into(), filename: None }],
                },
            ],
            ..Default::default()
        };
        req.tools = recorded["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| Tool { name: t["name"].as_str().unwrap().into(), description: t["description"].as_str().unwrap().into(), parameters: t["parameters"].clone() })
            .collect();
        req.max_tokens = recorded["max_output_tokens"].as_u64();
        req.options = native(json!({"store": false, "include": ["reasoning.encrypted_content"], "reasoningEffort": "medium", "reasoningSummary": "auto", "textVerbosity": "low"}));
        assert_eq!(encode(&req).unwrap(), recorded);
        let (events, _) = replay(response_body(&c, 0));
        assert_eq!(finish(&events).0, "stop");
    }

    #[test]
    fn reasoning_round_trips_through_recorded_continuation() {
        let options = native(json!({"store": false, "include": ["reasoning.encrypted_content", "bogus"], "reasoningEffort": "low", "reasoningSummary": "auto", "textVerbosity": "low"}));
        let Some(single) = cassette(&format!("{DIR}/openai-responses-gpt-5-5-reasoning.json")) else { return };
        let Some(c) = cassette(&format!("{DIR}/openai-responses-gpt-5-5-reasoning-continuation.json")) else { return };
        let mut req = Request {
            model: "gpt-5.5".into(),
            system: vec!["Show concise reasoning when the provider supports visible reasoning summaries.".into()],
            messages: vec![Message::User("Think briefly, then reply exactly with: Hello!".into())],
            max_tokens: Some(120),
            options,
            ..Default::default()
        };
        assert_eq!(encode(&req).unwrap(), request_body(&single, 0));
        assert_eq!(serde_json::to_string(&encode(&req).unwrap()).unwrap(), c["interactions"][0]["request"]["body"].as_str().unwrap());
        let (events, _) = replay(response_body(&single, 0));
        assert_eq!(text_of(&events), "Hello!");
        let (events, completion) = replay(response_body(&c, 0));
        assert_eq!(completion.reasoning.len(), 1);
        assert!(completion.reasoning[0].item_id.as_deref().unwrap().starts_with("rs_") && completion.reasoning[0].encrypted.is_some());
        assert_eq!(finish(&events).1.reasoning, 12);
        req.system.clear();
        req.max_tokens = Some(40);
        req.messages.push(Message::Assistant { text: text_of(&events), reasoning: String::new(), tool_calls: Vec::new(), reasoning_parts: completion.reasoning });
        req.messages.push(Message::User("Now reply exactly with: Done.".into()));
        assert_eq!(serde_json::to_string(&encode(&req).unwrap()).unwrap(), c["interactions"][1]["request"]["body"].as_str().unwrap());
        let (events, _) = replay(response_body(&c, 1));
        assert_eq!(text_of(&events), "Done.");
    }

    #[test]
    fn cached_tokens_are_reported() {
        let Some(c) = cassette("llm/test/fixtures/recordings/openai-responses-cache/reports-cached-tokens-on-identical-second-call.json") else { return };
        let (first, _) = replay(response_body(&c, 0));
        let (second, _) = replay(response_body(&c, 1));
        assert!(finish(&first).1.input > 0);
        let usage = finish(&second).1;
        assert!(usage.cache_read > 0 && usage.cache_read <= usage.input, "{usage:?}");
    }

    #[test]
    fn session_recordings_replay_with_reasoning_and_tools() {
        let schema = json!({"$schema": "http://json-schema.org/draft-07/schema#", "type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"], "additionalProperties": false});
        let system = "Answer using tools when appropriate.\nUse the get_weather tool exactly once to look up Paris, then reply with exactly: Paris is sunny.";
        let Some(c) = cassette("opencode/test/fixtures/recordings/session/native-zen-tool-loop.json") else { return };
        let mut req = Request {
            model: "gpt-5.2-codex".into(),
            system: vec![system.into()],
            messages: vec![Message::User("What is the weather in Paris?".into())],
            tools: vec![weather("Get the current weather for a city.", schema)],
            max_tokens: Some(32000),
            options: native(json!({"store": false, "promptCacheKey": "session-recorded-opencode-loop", "include": ["reasoning.encrypted_content"], "reasoningEffort": "medium", "reasoningSummary": "auto"})),
            ..Default::default()
        };
        assert_eq!(serde_json::to_string(&encode(&req).unwrap()).unwrap(), c["interactions"][0]["request"]["body"].as_str().unwrap());
        let (events, completion) = replay(response_body(&c, 0));
        let made = calls(&events);
        assert_eq!(finish(&events).0, "tool-calls");
        let replayed = ToolCall { arguments: "{\"city\":{}}".into(), ..made[0].clone() };
        req.messages.push(Message::Assistant { text: String::new(), reasoning: String::new(), tool_calls: vec![replayed], reasoning_parts: completion.reasoning });
        req.messages.push(Message::tool(&made[0].id, "{\"temperature\":22,\"condition\":\"sunny\"}"));
        assert_eq!(serde_json::to_string(&encode(&req).unwrap()).unwrap(), c["interactions"][1]["request"]["body"].as_str().unwrap());
        let (events, _) = replay(response_body(&c, 1));
        assert_eq!(text_of(&events), "Paris is sunny.");
    }

    #[test]
    fn references_stored_reasoning_and_flattens_tool_schemas() {
        let reasoning = |text: &str, encrypted: Option<&str>| Reasoning { text: text.into(), signature: None, item_id: Some("rs_1".into()), encrypted: encrypted.map(str::to_string) };
        let mut req = Request {
            model: "gpt-5".into(),
            messages: vec![
                Message::UserParts(vec![Part::Media { mime: "application/pdf".into(), data: "AAAA".into(), filename: Some("a.pdf".into()) }]),
                Message::Assistant { text: "hi".into(), reasoning: String::new(), tool_calls: Vec::new(), reasoning_parts: vec![reasoning("one", None), reasoning("two", Some("enc"))] },
            ],
            options: json!({"store": true, "serviceTier": "priority", "reasoningEffort": "high"}),
            temperature: Some(1.0),
            ..Default::default()
        };
        let body = encode(&req).unwrap();
        assert_eq!(
            body["input"],
            json!([
                {"role": "user", "content": [{"type": "input_file", "filename": "a.pdf", "file_data": "data:application/pdf;base64,AAAA"}]},
                {"type": "item_reference", "id": "rs_1"},
                {"role": "assistant", "content": [{"type": "output_text", "text": "hi"}]}
            ])
        );
        assert_eq!((&body["store"], &body["service_tier"], &body["reasoning"], body.get("temperature")), (&json!(true), &json!("priority"), &json!({"effort": "high"}), None));
        req.options = json!({"store": false});
        assert_eq!(
            encode(&req).unwrap()["input"][1],
            json!({"type": "reasoning", "summary": [{"type": "summary_text", "text": "one"}, {"type": "summary_text", "text": "two"}], "encrypted_content": "enc"})
        );
        // Without a stored response the item id is useless; only encrypted state is replayed. Copilot never replays it.
        req.options = json!({});
        assert_eq!(encode(&req).unwrap()["input"][1]["type"], json!("reasoning"));
        assert_eq!(encode_with(&req, &Config { copilot: true, ungated: false }).unwrap()["input"][1]["role"], json!("assistant"));
        let grok = Request { model: "grok-4".into(), system: vec!["s".into()], temperature: Some(0.5), options: json!({"reasoningEffort": "high", "store": false}), ..Default::default() };
        let body = encode_with(&grok, &Config { copilot: false, ungated: true }).unwrap();
        assert_eq!((&body["input"][0]["role"], &body["reasoning"], &body["temperature"], body.get("include")), (&json!("system"), &json!({"effort": "high"}), &json!(0.5), None));
        // Efforts are free-form, and sampling parameters return once reasoning is switched off on GPT-5.1+.
        req.options = json!({"reasoningEffort": "max", "user": "u", "parallelToolCalls": false});
        let body = encode(&req).unwrap();
        assert_eq!((&body["reasoning"], &body["user"], &body["parallel_tool_calls"], body.get("temperature")), (&json!({"effort": "max"}), &json!("u"), &json!(false), None));
        req.model = "gpt-5.1".into();
        req.options = json!({"reasoningEffort": "none"});
        assert_eq!(encode(&req).unwrap()["temperature"], json!(1));
        // Other models keep the `system` role and never get a reasoning block unless forced.
        req.model = "gpt-4.1".into();
        req.system = vec!["a".into(), "b".into()];
        req.options = json!({"reasoningEffort": "high", "store": false});
        let body = encode(&req).unwrap();
        assert_eq!((&body["input"][0], &body["input"][1], body.get("reasoning"), body.get("include"), &body["temperature"]), (&json!({"role": "system", "content": "a"}), &json!({"role": "system", "content": "b"}), None, None, &json!(1)));
        req.options = json!({"reasoningEffort": "high", "store": false, "forceReasoning": true});
        let body = encode(&req).unwrap();
        assert_eq!((&body["input"][0]["role"], &body["reasoning"], &body["include"]), (&json!("developer"), &json!({"effort": "high"}), &json!(["reasoning.encrypted_content"])));
        let unnamed = Request { messages: vec![Message::UserParts(vec![Part::Text("x".into()), Part::Media { mime: "application/pdf".into(), data: "AAAA".into(), filename: None }])], ..Default::default() };
        assert_eq!(encode(&unnamed).unwrap()["input"][0]["content"][1]["filename"], json!("part-1.pdf"));

        let schema = json!({"anyOf": [
            {"type": "object", "properties": {"a": {"anyOf": [{"type": "string"}, {"type": "null"}]}}},
            {"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "boolean"}}}
        ]});
        assert_eq!(tool_schema(&schema), json!({"type": "object", "properties": {"a": {"type": "string"}, "b": {"type": "boolean"}}, "additionalProperties": false}));
        assert_eq!(tool_schema(&json!({"properties": {}})), json!({"properties": {}, "type": "object"}));
    }

    #[test]
    fn decodes_summaries_incomplete_and_failures() {
        let stream = [
            r#"{"type":"response.output_item.added","item":{"id":"rs_1","type":"reasoning","encrypted_content":"early"}}"#,
            r#"{"type":"response.reasoning_summary_part.added","item_id":"rs_1","summary_index":0}"#,
            r#"{"type":"response.reasoning_summary_text.delta","item_id":"rs_1","summary_index":0,"delta":"First"}"#,
            r#"{"type":"response.reasoning_summary_part.added","item_id":"rs_1","summary_index":1}"#,
            r#"{"type":"response.reasoning_summary_text.delta","item_id":"rs_1","summary_index":1,"delta":"Second"}"#,
            r#"{"type":"response.output_item.done","item":{"id":"rs_1","type":"reasoning","encrypted_content":"final"}}"#,
            r#"{"type":"response.output_item.added","item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"read","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"pa"}"#,
            r#"{"type":"response.output_item.done","item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"read","arguments":"{\"path\":\"a\"}"}}"#,
            r#"{"type":"response.incomplete","response":{"id":"resp_1","incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":4},"output_tokens":7,"output_tokens_details":{"reasoning_tokens":3},"total_tokens":17}}}"#,
            r#"{"type":"response.output_text.delta","delta":"ignored after terminal"}"#,
        ];
        let mut decoder = Decoder::default();
        let events: Vec<Event> = stream.iter().flat_map(|data| decoder.push(data).unwrap()).collect();
        assert_eq!(
            events,
            [
                Event::ReasoningDelta("First".into()),
                Event::ReasoningDelta("\n\n".into()),
                Event::ReasoningDelta("Second".into()),
                Event::ToolCall(ToolCall { id: "call_1".into(), name: "read".into(), arguments: "{\"path\":\"a\"}".into() }),
                Event::Finish { reason: "length".into(), usage: Usage { input: 10, output: 7, reasoning: 3, cache_read: 4, cache_write: 0, total: 17 } },
            ]
        );
        let completion = decoder.finish().unwrap();
        assert_eq!(completion.reasoning.iter().map(|r| (r.text.as_str(), r.encrypted.as_deref())).collect::<Vec<_>>(), [("First", Some("final")), ("Second", Some("final"))]);

        let failed = r#"{"type":"response.failed","response":{"error":{"code":"context_length_exceeded","message":"Your input exceeds the context window of this model."}}}"#;
        let error = Decoder::default().push(failed).unwrap_err();
        assert!(crate::retry::classify(&error, "openai").overflow);
        let error = Decoder::default().push(r#"{"type":"error","code":"server_is_overloaded","message":"busy"}"#).unwrap_err();
        assert!(matches!(&error, Error::Provider { message, .. } if message == "server_is_overloaded: busy"));
        assert!(crate::retry::classify(&error, "openai").retryable);
        let error = Decoder::default().push(r#"{"type":"error","error":{"code":"usage_not_included","message":"x"}}"#).unwrap_err();
        assert!(!crate::retry::classify(&error, "openai").retryable);
        assert!(matches!(Decoder::default().finish(), Err(Error::Stream(_))));
    }
}
