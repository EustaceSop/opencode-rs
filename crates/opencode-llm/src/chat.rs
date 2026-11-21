// OpenAI-compatible Chat Completions: request encoding and stream decoding.
use crate::{Completion, Error, Event, Message, Part, Reasoning, Request, ToolCall, ToolChoice, Usage, copilot, number};
use base64::Engine;
use serde_json::{Map, Value, json};

// Keys of the provider-options schema; every other option key is copied into the body as is.
const OPTION_KEYS: [&str; 4] = ["user", "reasoningEffort", "textVerbosity", "strictJsonSchema"];
// Body keys the SDK assigns after spreading the raw options, so options can never set them.
const RESERVED: [&str; 7] = ["reasoning_effort", "verbosity", "messages", "tools", "tool_choice", "stream", "stream_options"];

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    // `stream_options.include_usage`; off when the provider sets `includeUsage: false`.
    pub include_usage: bool,
    // Message field that carries reasoning back (`interleaved.field`); it is then always present on assistant turns.
    pub reasoning_field: Option<String>,
    // The Copilot SDK dialect: no usage option, opaque reasoning, a fixed option set.
    pub copilot: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config { include_usage: true, reasoning_field: None, copilot: false }
    }
}

fn unsupported(what: String) -> Error {
    Error::Config(format!("'{what}' functionality not supported."))
}

fn part(part: &Part) -> Result<Value, Error> {
    let (mime, data, filename) = match part {
        Part::Text(text) => return Ok(json!({"type": "text", "text": text})),
        Part::Media { mime, data, filename } => (mime.as_str(), data, filename),
    };
    if mime.starts_with("image/") {
        let mime = if mime == "image/*" { "image/jpeg" } else { mime };
        return Ok(json!({"type": "image_url", "image_url": {"url": format!("data:{mime};base64,{data}")}}));
    }
    if mime.starts_with("audio/") {
        let format = match mime {
            "audio/wav" => "wav",
            "audio/mp3" | "audio/mpeg" => "mp3",
            _ => return Err(unsupported(format!("audio media type {mime}"))),
        };
        return Ok(json!({"type": "input_audio", "input_audio": {"data": data, "format": format}}));
    }
    if mime == "application/pdf" {
        let name = filename.as_deref().unwrap_or("document.pdf");
        return Ok(json!({"type": "file", "file": {"filename": name, "file_data": format!("data:application/pdf;base64,{data}")}}));
    }
    if mime.starts_with("text/") {
        let bytes = base64::engine::general_purpose::STANDARD.decode(data).map_err(|_| unsupported(format!("file part media type {mime}")))?;
        return Ok(json!({"type": "text", "text": String::from_utf8_lossy(&bytes)}));
    }
    Err(unsupported(format!("file part media type {mime}")))
}

// `JSON.stringify(input)` of the parsed arguments; text that is not JSON passes through.
pub(crate) fn arguments(raw: &str) -> String {
    serde_json::from_str::<Value>(raw).map(|v| v.to_string()).unwrap_or_else(|_| raw.to_string())
}

// Indexes of the last two non-system messages, a run of tool results counting as one.
pub(crate) fn cache_targets(messages: &[Message]) -> Vec<usize> {
    let mut ends = Vec::new();
    for (i, message) in messages.iter().enumerate() {
        let tool = matches!(message, Message::Tool { .. });
        if !(tool && matches!(messages.get(i + 1), Some(Message::Tool { .. }))) {
            ends.push(i);
        }
    }
    ends.split_off(ends.len().saturating_sub(2))
}

fn messages(req: &Request, config: &Config) -> Result<Vec<Value>, Error> {
    let marker = if config.copilot { "copilot_cache_control" } else { "cache_control" };
    let mark = |value: &mut Value| {
        if let Some(map) = value.as_object_mut() {
            map.insert(marker.into(), json!({"type": "ephemeral"}));
        }
    };
    let targets = if req.cache { cache_targets(&req.messages) } else { Vec::new() };
    let mut out = Vec::new();
    // Images returned by tools cannot ride on a tool message; they follow in a user message.
    let mut pending: Vec<Value> = Vec::new();
    for (i, text) in req.system.iter().enumerate() {
        let mut message = json!({"role": "system", "content": text});
        if req.cache && i < 2 {
            mark(&mut message);
        }
        out.push(message);
    }
    for (i, message) in req.messages.iter().enumerate() {
        if !matches!(message, Message::Tool { .. }) && !pending.is_empty() {
            out.push(json!({"role": "user", "content": std::mem::take(&mut pending)}));
        }
        let cached = targets.contains(&i);
        let mut encoded = match message {
            Message::User(text) => json!({"role": "user", "content": text}),
            Message::UserParts(parts) => match parts.as_slice() {
                [Part::Text(text)] => json!({"role": "user", "content": text}),
                _ => {
                    let mut content = parts.iter().map(part).collect::<Result<Vec<_>, _>>()?;
                    if cached && let Some(last) = content.last_mut() {
                        mark(last);
                    }
                    let message = json!({"role": "user", "content": content});
                    if parts.is_empty() && cached {
                        out.push(message);
                        mark(out.last_mut().unwrap());
                    } else {
                        out.push(message);
                    }
                    continue;
                }
            },
            Message::Assistant { text, reasoning, tool_calls, reasoning_parts } => {
                let thought = if reasoning.is_empty() { reasoning_parts.iter().map(|r| r.text.as_str()).collect::<String>() } else { reasoning.clone() };
                let mut m = Map::new();
                m.insert("role".into(), "assistant".into());
                m.insert("content".into(), if config.copilot && text.is_empty() { Value::Null } else { text.clone().into() });
                if !config.copilot && config.reasoning_field.is_none() && !thought.is_empty() {
                    m.insert("reasoning_content".into(), thought.clone().into());
                }
                if !tool_calls.is_empty() {
                    let mut calls: Vec<Value> = tool_calls
                        .iter()
                        .map(|c| json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": arguments(&c.arguments)}}))
                        .collect();
                    // The marker sits on the last content part, which only a tool call can carry.
                    if cached && let Some(last) = calls.last_mut() {
                        mark(last);
                    }
                    m.insert("tool_calls".into(), calls.into());
                }
                if config.copilot {
                    if let Some(opaque) = reasoning_parts.iter().rev().find_map(|r| r.encrypted.as_ref()) {
                        if !thought.is_empty() {
                            m.insert("reasoning_text".into(), thought.into());
                        }
                        m.insert("reasoning_opaque".into(), opaque.clone().into());
                    }
                } else if let Some(field) = &config.reasoning_field {
                    m.insert(field.clone(), thought.into());
                }
                if cached && text.is_empty() && tool_calls.is_empty() && reasoning_parts.is_empty() && reasoning.is_empty() {
                    mark_map(&mut m, marker);
                }
                out.push(Value::Object(m));
                continue;
            }
            Message::Tool { call_id, content, attachments, .. } => {
                for attachment in attachments {
                    pending.push(part(attachment)?);
                }
                json!({"role": "tool", "tool_call_id": call_id, "content": content})
            }
        };
        if cached {
            mark(&mut encoded);
        }
        out.push(encoded);
    }
    if !pending.is_empty() {
        out.push(json!({"role": "user", "content": pending}));
    }
    Ok(out)
}

fn mark_map(map: &mut Map<String, Value>, marker: &str) {
    map.insert(marker.into(), json!({"type": "ephemeral"}));
}

pub fn encode(req: &Request) -> Result<Value, Error> {
    encode_with(req, &Config::default())
}

pub fn encode_with(req: &Request, config: &Config) -> Result<Value, Error> {
    let option = |key: &str| req.options.get(key).filter(|v| !v.is_null());
    let mut body = Map::new();
    body.insert("model".into(), req.model.clone().into());
    body.insert("messages".into(), messages(req, config)?.into());
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.parameters}}))
            .collect();
        body.insert("tools".into(), tools.into());
        match &req.tool_choice {
            Some(ToolChoice::Auto) => body.insert("tool_choice".into(), "auto".into()),
            Some(ToolChoice::None) => body.insert("tool_choice".into(), "none".into()),
            Some(ToolChoice::Required) => body.insert("tool_choice".into(), "required".into()),
            Some(ToolChoice::Tool(name)) => body.insert("tool_choice".into(), json!({"type": "function", "function": {"name": name}})),
            None => None,
        };
    }
    body.insert("stream".into(), true.into());
    if config.include_usage && !config.copilot {
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    if let Some(v) = req.max_tokens {
        body.insert("max_tokens".into(), v.into());
    }
    if let Some(v) = req.temperature {
        body.insert("temperature".into(), number(v));
    }
    if let Some(v) = req.top_p {
        body.insert("top_p".into(), number(v));
    }
    if let Some(user) = option("user").filter(|v| v.is_string()) {
        body.insert("user".into(), user.clone());
    }
    if !config.copilot {
        for (key, value) in req.options.as_object().into_iter().flatten() {
            if !OPTION_KEYS.contains(&key.as_str()) && !RESERVED.contains(&key.as_str()) && !value.is_null() {
                body.insert(key.clone(), value.clone());
            }
        }
    }
    if let Some(effort) = option("reasoningEffort").filter(|v| v.is_string()) {
        body.insert("reasoning_effort".into(), effort.clone());
    }
    if let Some(verbosity) = option("textVerbosity").filter(|v| v.is_string()) {
        body.insert("verbosity".into(), verbosity.clone());
    }
    if let Some(budget) = option("thinking_budget").filter(|v| config.copilot && v.is_number()) {
        body.insert("thinking_budget".into(), budget.clone());
    }
    Ok(Value::Object(body))
}

#[derive(Default)]
pub struct Decoder {
    calls: Vec<Option<ToolCall>>,
    finish: Option<String>,
    usage: Usage,
    reasoning: String,
    opaque: Option<String>,
    nano_aiu: Option<f64>,
}

impl Decoder {
    pub fn push(&mut self, data: &str) -> Result<Vec<Event>, Error> {
        if data.trim() == "[DONE]" {
            return Ok(Vec::new());
        }
        let chunk: Value = serde_json::from_str(data).map_err(|e| Error::Stream(format!("invalid chunk: {e}")))?;
        if let Some(error) = chunk.get("error").filter(|e| !e.is_null()) {
            // The whole error object is surfaced, so its code drives classification.
            let text = |key: &str| error.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
            return Err(Error::Provider { code: text("code"), message: text("message").unwrap_or_else(|| error.to_string()) });
        }
        self.nano_aiu = copilot::nano_aiu(&chunk).or(self.nano_aiu);
        // Groq reports usage under `x_groq`.
        if let Some(usage) = chunk.get("usage").or_else(|| chunk.pointer("/x_groq/usage")).filter(|u| u.is_object()) {
            let n = |v: Option<&Value>| v.and_then(Value::as_u64).unwrap_or(0);
            let first = |pointers: &[&str]| pointers.iter().find_map(|p| usage.pointer(p).and_then(Value::as_u64)).unwrap_or(0);
            self.usage = Usage {
                input: n(usage.get("prompt_tokens")),
                output: n(usage.get("completion_tokens")),
                reasoning: first(&["/completion_tokens_details/reasoning_tokens", "/reasoning_tokens"]),
                cache_read: first(&["/prompt_tokens_details/cached_tokens", "/num_cached_tokens", "/prompt_token_details/cached_tokens"]),
                cache_write: first(&["/prompt_tokens_details/cache_write_tokens", "/prompt_tokens_details/cache_creation_input_tokens"]),
                total: n(usage.get("total_tokens")),
            };
        }
        let mut events = Vec::new();
        let Some(choice) = chunk.pointer("/choices/0") else {
            return Ok(events);
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish = Some(reason.to_string());
        }
        let delta = &choice["delta"];
        let text = |key: &str| delta.get(key).and_then(Value::as_str).filter(|s| !s.is_empty());
        if let Some(thought) = text("reasoning_content").or_else(|| text("reasoning")).or_else(|| text("reasoning_text")).or_else(|| text("thinking")) {
            self.reasoning.push_str(thought);
            events.push(Event::ReasoningDelta(thought.to_string()));
        }
        if let Some(opaque) = text("reasoning_opaque") {
            self.opaque = Some(opaque.to_string());
        }
        if let Some(content) = text("content") {
            events.push(Event::TextDelta(content.to_string()));
        }
        // Mistral streams content as parts once a model thinks: `text` parts and `thinking` parts holding text parts.
        for part in delta.get("content").and_then(Value::as_array).into_iter().flatten() {
            let piece = |v: &Value| v.get("text").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
            match part.get("type").and_then(Value::as_str) {
                Some("text") => events.extend(piece(part).map(Event::TextDelta)),
                Some("thinking") => {
                    for thought in part.get("thinking").and_then(Value::as_array).into_iter().flatten().filter_map(piece) {
                        self.reasoning.push_str(&thought);
                        events.push(Event::ReasoningDelta(thought));
                    }
                }
                _ => {}
            }
        }
        for delta in delta.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
            let index = delta.get("index").and_then(Value::as_u64).map(|i| i as usize).unwrap_or(self.calls.len());
            if self.calls.len() <= index {
                self.calls.resize(index + 1, None);
            }
            let slot = self.calls[index].get_or_insert_with(|| ToolCall { id: String::new(), name: String::new(), arguments: String::new() });
            if let Some(id) = delta.get("id").and_then(Value::as_str) {
                slot.id = id.to_string();
            }
            if let Some(name) = delta.pointer("/function/name").and_then(Value::as_str) {
                slot.name.push_str(name);
            }
            if let Some(args) = delta.pointer("/function/arguments").and_then(Value::as_str) {
                slot.arguments.push_str(args);
            }
        }
        Ok(events)
    }

    // Tool calls and the finish event are emitted once the stream ends.
    pub fn finish(self) -> Vec<Event> {
        self.finish_with().0
    }

    // `finish`, plus what must be kept to replay the turn (Copilot's opaque reasoning, its billed amount).
    pub fn finish_with(self) -> (Vec<Event>, Completion) {
        let calls: Vec<ToolCall> = self.calls.into_iter().flatten().collect();
        let reason = match self.finish.as_deref() {
            Some("stop") if !calls.is_empty() => "tool-calls",
            Some("stop") => "stop",
            Some("length" | "model_length" | "max_tokens") => "length",
            Some("content_filter") => "content-filter",
            Some("function_call" | "tool_calls" | "tool_use") => "tool-calls",
            _ => "unknown",
        };
        let mut events: Vec<Event> = calls.into_iter().map(Event::ToolCall).collect();
        events.push(Event::Finish { reason: reason.to_string(), usage: self.usage });
        let reasoning = self.opaque.map(|opaque| Reasoning { text: self.reasoning, encrypted: Some(opaque), ..Default::default() }).into_iter().collect();
        (events, Completion { reasoning, response_id: None, nano_aiu: self.nano_aiu })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse;
    use std::path::PathBuf;

    fn cassette(name: &str) -> Option<Value> {
        crate::testutil::cassette(&format!("llm/test/fixtures/recordings/openai-compatible-chat/{name}"))
    }

    fn replay(body: &str) -> Vec<Event> {
        let mut sse = sse::Decoder::default();
        let mut chat = Decoder::default();
        let mut events = Vec::new();
        // Feed in small pieces to exercise chunk-boundary handling.
        for piece in body.as_bytes().chunks(7) {
            for data in sse.feed(piece) {
                events.extend(chat.push(&data).unwrap());
            }
        }
        for data in sse.finish() {
            events.extend(chat.push(&data).unwrap());
        }
        events.extend(chat.finish());
        events
    }

    #[test]
    fn request_matches_recorded_original() {
        let Some(c) = cassette("deepseek-streams-text.json") else { return };
        let recorded: Value = serde_json::from_str(c["interactions"][0]["request"]["body"].as_str().unwrap()).unwrap();
        let req = Request {
            model: "deepseek-chat".into(),
            system: vec!["You are concise.".into()],
            messages: vec![Message::User("Reply with exactly: Hello!".into())],
            max_tokens: Some(20),
            temperature: Some(0.0),
            ..Default::default()
        };
        assert_eq!(serde_json::to_string(&encode(&req).unwrap()).unwrap(), serde_json::to_string(&recorded).unwrap());
    }

    #[test]
    fn decodes_recorded_text_streams() {
        for name in ["deepseek-streams-text.json", "groq-streams-text.json", "togetherai-streams-text.json"] {
            let Some(c) = cassette(name) else { return };
            let events = replay(c["interactions"][0]["response"]["body"].as_str().unwrap());
            let text: String = events.iter().filter_map(|e| if let Event::TextDelta(t) = e { Some(t.as_str()) } else { None }).collect();
            assert_eq!(text, "Hello!", "{name}");
            let Some(Event::Finish { reason, usage }) = events.last() else { panic!("{name}: no finish") };
            assert_eq!(reason, "stop", "{name}");
            assert!(usage.input > 0 && usage.output > 0, "{name}: {usage:?}");
        }
    }

    #[test]
    fn decodes_recorded_tool_calls() {
        for name in ["groq-streams-tool-call.json", "togetherai-streams-tool-call.json"] {
            let Some(c) = cassette(name) else { return };
            let events = replay(c["interactions"][0]["response"]["body"].as_str().unwrap());
            let calls: Vec<&ToolCall> = events.iter().filter_map(|e| if let Event::ToolCall(t) = e { Some(t) } else { None }).collect();
            assert_eq!(calls.len(), 1, "{name}");
            assert!(!calls[0].id.is_empty() && !calls[0].name.is_empty(), "{name}");
            assert!(serde_json::from_str::<Value>(&calls[0].arguments).unwrap().is_object(), "{name}");
            assert!(matches!(events.last(), Some(Event::Finish { reason, .. }) if reason == "tool-calls"), "{name}");
        }
    }

    #[test]
    fn replays_every_recorded_chat_stream() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../opencode-ts/packages");
        let dirs = [
            "llm/test/fixtures/recordings/openai-chat",
            "llm/test/fixtures/recordings/openai-compatible-chat",
            "llm/test/fixtures/recordings/cloudflare-ai-gateway",
            "llm/test/fixtures/recordings/cloudflare-workers-ai",
            "core/test/fixtures/recordings/session-runner",
        ];
        let mut streams = 0;
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(root.join(dir)) else { return };
            for entry in entries.flatten() {
                let name = format!("{dir}/{}", entry.file_name().to_string_lossy());
                let c = crate::testutil::cassette(&name).unwrap();
                for interaction in c["interactions"].as_array().unwrap() {
                    if interaction["response"]["status"] != 200 {
                        continue;
                    }
                    let events = replay(interaction["response"]["body"].as_str().unwrap());
                    let Some(Event::Finish { reason, .. }) = events.last() else { panic!("{name}: no finish") };
                    let called = events.iter().any(|e| matches!(e, Event::ToolCall(_)));
                    assert_eq!(called, reason == "tool-calls", "{name}: {reason}");
                    assert!(called || events.iter().any(|e| matches!(e, Event::TextDelta(_))), "{name}: empty turn");
                    streams += 1;
                }
            }
        }
        assert!(streams >= 20, "{streams}");
    }

    fn media(mime: &str) -> Part {
        Part::Media { mime: mime.into(), data: "AAAA".into(), filename: None }
    }

    #[test]
    fn encodes_parts_tool_choice_and_tool_attachments() {
        let req = Request {
            model: "m".into(),
            system: vec!["one".into(), "two".into()],
            messages: vec![
                Message::UserParts(vec![Part::Text("a".into())]),
                Message::UserParts(vec![Part::Text("a".into()), Part::Text("b".into())]),
                Message::UserParts(vec![Part::Text("see".into()), media("image/png"), media("application/pdf"), media("audio/mpeg"), media("text/plain")]),
                Message::assistant("", vec![ToolCall { id: "c1".into(), name: "shot".into(), arguments: "{ \"a\" : 1 }".into() }]),
                Message::Tool { call_id: "c1".into(), content: "done".into(), is_error: true, attachments: vec![media("image/jpeg")] },
                Message::User("next".into()),
            ],
            tools: vec![crate::Tool { name: "shot".into(), description: "d".into(), parameters: json!({"type": "object"}) }],
            tool_choice: Some(ToolChoice::Tool("shot".into())),
            ..Default::default()
        };
        let body = encode(&req).unwrap();
        assert_eq!(
            body["messages"],
            json!([
                {"role": "system", "content": "one"},
                {"role": "system", "content": "two"},
                {"role": "user", "content": "a"},
                {"role": "user", "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]},
                {"role": "user", "content": [
                    {"type": "text", "text": "see"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
                    {"type": "file", "file": {"filename": "document.pdf", "file_data": "data:application/pdf;base64,AAAA"}},
                    {"type": "input_audio", "input_audio": {"data": "AAAA", "format": "mp3"}},
                    {"type": "text", "text": "\u{0}\u{0}\u{0}"}
                ]},
                {"role": "assistant", "content": "", "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "shot", "arguments": "{\"a\":1}"}}]},
                {"role": "tool", "tool_call_id": "c1", "content": "done"},
                {"role": "user", "content": [{"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,AAAA"}}]},
                {"role": "user", "content": "next"}
            ])
        );
        assert_eq!(body["tool_choice"], json!({"type": "function", "function": {"name": "shot"}}));
        let video = Request { messages: vec![Message::UserParts(vec![media("video/mp4")])], tool_choice: Some(ToolChoice::Auto), ..Default::default() };
        assert!(matches!(encode(&video), Err(Error::Config(m)) if m == "'file part media type video/mp4' functionality not supported."));
        let bare = Request { tool_choice: Some(ToolChoice::Auto), ..Default::default() };
        assert!(encode(&bare).unwrap().get("tool_choice").is_none());
    }

    #[test]
    fn provider_options_reach_the_body() {
        let assistant = Message::Assistant { text: "hi".into(), reasoning: "think".into(), tool_calls: Vec::new(), reasoning_parts: Vec::new() };
        let req = Request {
            model: "glm-4.7".into(),
            messages: vec![Message::User("q".into()), assistant.clone(), Message::assistant("plain", Vec::new())],
            max_tokens: Some(100),
            options: json!({
                "thinking": {"type": "enabled", "clear_thinking": false},
                "reasoningEffort": "high",
                "textVerbosity": "low",
                "user": "u1",
                "strictJsonSchema": false,
                "max_tokens": 7,
                "stream": false,
                "messages": [],
                "reasoning_effort": "ignored",
                "chat_template_args": {"enable_thinking": true},
                "skipped": null
            }),
            ..Default::default()
        };
        let body = encode(&req).unwrap();
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            concat!(
                r#"{"model":"glm-4.7","messages":[{"role":"user","content":"q"},{"role":"assistant","content":"hi","reasoning_content":"think"},{"role":"assistant","content":"plain"}],"#,
                r#""stream":true,"stream_options":{"include_usage":true},"max_tokens":7,"user":"u1","thinking":{"type":"enabled","clear_thinking":false},"#,
                r#""chat_template_args":{"enable_thinking":true},"reasoning_effort":"high","verbosity":"low"}"#
            )
        );
        // An interleaved field is always sent back, even when empty; usage reporting can be switched off.
        let config = Config { include_usage: false, reasoning_field: Some("reasoning_details".into()), copilot: false };
        let body = encode_with(&Request { options: Value::Null, ..req.clone() }, &config).unwrap();
        assert_eq!(body["messages"][1], json!({"role": "assistant", "content": "hi", "reasoning_details": "think"}));
        assert_eq!(body["messages"][2], json!({"role": "assistant", "content": "plain", "reasoning_details": ""}));
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn cache_markers_follow_the_last_content_part() {
        let call = ToolCall { id: "c1".into(), name: "t".into(), arguments: "{}".into() };
        let mut req = Request {
            system: vec!["a".into(), "b".into(), "c".into()],
            messages: vec![
                Message::User("first".into()),
                Message::assistant("text", vec![call.clone(), ToolCall { id: "c2".into(), ..call.clone() }]),
                Message::tool("c1", "one"),
                Message::tool("c2", "two"),
            ],
            cache: true,
            ..Default::default()
        };
        let marker = json!({"type": "ephemeral"});
        let body = encode(&req).unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.iter().map(|m| m.get("cache_control").is_some()).collect::<Vec<_>>(), [true, true, false, false, false, false, true]);
        assert_eq!(messages[4]["tool_calls"][1]["cache_control"], marker);
        assert!(messages[4]["tool_calls"][0].get("cache_control").is_none());
        req.messages.push(Message::UserParts(vec![Part::Text("a".into()), media("image/png")]));
        let body = encode(&req).unwrap();
        assert_eq!(body["messages"][7]["content"][1]["cache_control"], marker);
        assert!(body["messages"][4]["tool_calls"][1].get("cache_control").is_none() && body["messages"][6].get("cache_control").is_some());
    }

    #[test]
    fn copilot_dialect_and_stream_errors() {
        let reasoning = Reasoning { text: "plan".into(), encrypted: Some("opaque".into()), ..Default::default() };
        let req = Request {
            model: "claude-sonnet-4.5".into(),
            messages: vec![
                Message::User("q".into()),
                Message::Assistant { text: String::new(), reasoning: String::new(), tool_calls: Vec::new(), reasoning_parts: vec![reasoning] },
                Message::Assistant { text: "x".into(), reasoning: "dropped".into(), tool_calls: Vec::new(), reasoning_parts: Vec::new() },
            ],
            options: json!({"store": false, "reasoningEffort": "high", "thinking_budget": 4000, "include": ["x"]}),
            cache: true,
            ..Default::default()
        };
        let body = encode_with(&req, &Config { include_usage: false, reasoning_field: None, copilot: true }).unwrap();
        assert_eq!(
            body,
            json!({
                "model": "claude-sonnet-4.5",
                "messages": [
                    {"role": "user", "content": "q"},
                    {"role": "assistant", "content": null, "reasoning_text": "plan", "reasoning_opaque": "opaque"},
                    {"role": "assistant", "content": "x"}
                ],
                "stream": true,
                "reasoning_effort": "high",
                "thinking_budget": 4000
            })
        );

        let mut decoder = Decoder::default();
        let mut events = Vec::new();
        for chunk in [
            r#"{"choices":[{"delta":{"reasoning_text":"pl"}}]}"#,
            r#"{"choices":[{"delta":{"reasoning_text":"an","reasoning_opaque":"op1"}}]}"#,
            r#"{"choices":[{"delta":{"content":"hi"},"finish_reason":"stop"}],"copilot_usage":{"total_nano_aiu":1200}}"#,
        ] {
            events.extend(decoder.push(chunk).unwrap());
        }
        assert_eq!(events, [Event::ReasoningDelta("pl".into()), Event::ReasoningDelta("an".into()), Event::TextDelta("hi".into())]);
        let (events, completion) = decoder.finish_with();
        assert!(matches!(&events[0], Event::Finish { reason, .. } if reason == "stop"));
        assert_eq!(completion.reasoning, [Reasoning { text: "plan".into(), encrypted: Some("op1".into()), ..Default::default() }]);
        assert_eq!(completion.nano_aiu, Some(1200.0));

        let error = Decoder::default().push(r#"{"error":{"message":"too big","code":"context_length_exceeded"}}"#).unwrap_err();
        assert!(crate::retry::classify(&error, "x").overflow);
        let error = Decoder::default().push(r#"{"error":{"message":"upstream hiccup"}}"#).unwrap_err();
        assert!(crate::retry::classify(&error, "x").retryable);
    }
}
