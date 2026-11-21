// Google Gemini `streamGenerateContent`: request encoding and stream decoding.
use crate::{Completion, Error, Event, Message, Part, Reasoning, Request, ToolCall, ToolChoice, Usage, number, transform};
use serde_json::{Map, Value, json};

pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
pub const API_KEY_HEADER: &str = "x-goog-api-key";
pub const API_KEY_ENV: &str = "GOOGLE_GENERATIVE_AI_API_KEY";

// Path and query appended to the base URL; ids that already name a resource path are used as is.
pub fn path(model: &str) -> String {
    let resource = if model.contains('/') { model.to_string() } else { format!("models/{model}") };
    format!("/{resource}:streamGenerateContent?alt=sse")
}

fn empty_object(schema: &Value) -> bool {
    schema.get("type").and_then(Value::as_str) == Some("object")
        && schema.get("properties").and_then(Value::as_object).is_none_or(Map::is_empty)
        && !schema.get("additionalProperties").is_some_and(|v| v.as_bool() != Some(false) && !v.is_null())
}

fn truthy(value: &Value) -> bool {
    !matches!(value, Value::Null | Value::Bool(false)) && value.as_str() != Some("") && value.as_f64() != Some(0.0)
}

// JSON Schema -> the OpenAPI subset Gemini accepts. None means "send no schema".
pub fn openapi_schema(schema: &Value, root: bool) -> Option<Value> {
    if schema.is_null() {
        return None;
    }
    if schema.is_object() && empty_object(schema) {
        if root {
            return None;
        }
        return Some(match schema.get("description").filter(|d| truthy(d)) {
            Some(description) => json!({"type": "object", "description": description}),
            None => json!({"type": "object"}),
        });
    }
    let Some(source) = schema.as_object() else {
        return Some(if schema.is_boolean() { json!({"type": "boolean", "properties": {}}) } else { schema.clone() });
    };
    let nested = |v: &Value| openapi_schema(v, false).unwrap_or(Value::Null);
    let list = |v: &Value| v.as_array().map(|items| items.iter().map(nested).collect::<Vec<_>>());
    let mut out = Map::new();
    for key in ["description", "required", "format"] {
        if let Some(value) = source.get(key).filter(|v| truthy(v)) {
            out.insert(key.into(), value.clone());
        }
    }
    if let Some(constant) = source.get("const") {
        out.insert("enum".into(), json!([constant]));
    }
    match source.get("type") {
        Some(Value::Array(types)) => {
            let non_null: Vec<&Value> = types.iter().filter(|t| t.as_str() != Some("null")).collect();
            if non_null.is_empty() {
                out.insert("type".into(), "null".into());
            } else {
                out.insert("anyOf".into(), non_null.iter().map(|t| json!({"type": t})).collect());
                if non_null.len() != types.len() {
                    out.insert("nullable".into(), true.into());
                }
            }
        }
        Some(kind) if truthy(kind) => {
            out.insert("type".into(), kind.clone());
        }
        _ => {}
    }
    if let Some(values) = source.get("enum") {
        out.insert("enum".into(), values.clone());
    }
    if let Some(Value::Object(properties)) = source.get("properties") {
        out.insert("properties".into(), Value::Object(properties.iter().map(|(k, v)| (k.clone(), nested(v))).collect()));
    }
    if let Some(items) = source.get("items").filter(|v| truthy(v)) {
        out.insert("items".into(), list(items).map(Value::Array).unwrap_or_else(|| nested(items)));
    }
    if let Some(all) = source.get("allOf").and_then(list) {
        out.insert("allOf".into(), all.into());
    }
    if let Some(any) = source.get("anyOf").and_then(Value::as_array) {
        let null = |s: &&Value| s.get("type").and_then(Value::as_str) == Some("null");
        if any.iter().any(|s| null(&s)) {
            let rest: Vec<&Value> = any.iter().filter(|s| !null(s)).collect();
            out.insert("nullable".into(), true.into());
            match rest.as_slice() {
                [only] => {
                    if let Some(Value::Object(converted)) = openapi_schema(only, false) {
                        out.extend(converted);
                    }
                }
                _ => {
                    out.shift_remove("nullable");
                    out.insert("anyOf".into(), rest.iter().map(|s| nested(s)).collect());
                    out.insert("nullable".into(), true.into());
                }
            }
        } else {
            out.insert("anyOf".into(), any.iter().map(nested).collect());
        }
    }
    if let Some(one) = source.get("oneOf").and_then(list) {
        out.insert("oneOf".into(), one.into());
    }
    if let Some(min) = source.get("minLength") {
        out.insert("minLength".into(), min.clone());
    }
    Some(Value::Object(out))
}

fn inline(part: &Part) -> Value {
    match part {
        Part::Text(text) => json!({"text": text}),
        Part::Media { mime, data, .. } => {
            let mime = if mime == "image/*" { "image/jpeg" } else { mime.as_str() };
            json!({"inlineData": {"mimeType": mime, "data": data}})
        }
    }
}

fn arguments(call: &ToolCall) -> Result<Value, Error> {
    let raw = if call.arguments.trim().is_empty() { "{}" } else { &call.arguments };
    serde_json::from_str(raw).map_err(|_| Error::Config(format!("Invalid JSON input for Gemini tool call {}", call.name)))
}

fn signed(mut part: Value, signature: Option<&String>) -> Value {
    if let Some(signature) = signature {
        part["thoughtSignature"] = signature.clone().into();
    }
    part
}

fn contents(req: &Request) -> Result<Vec<Value>, Error> {
    // Gemini 3 takes media inside a function response; older models get it as sibling parts.
    let response_parts = req.model.starts_with("gemini-3");
    let mut out: Vec<Value> = Vec::new();
    let mut in_tool_group = false;
    for (i, message) in req.messages.iter().enumerate() {
        let tool = matches!(message, Message::Tool { .. });
        match message {
            Message::User(text) => out.push(json!({"role": "user", "parts": [{"text": text}]})),
            Message::UserParts(parts) => out.push(json!({"role": "user", "parts": parts.iter().map(inline).collect::<Vec<_>>()})),
            Message::Assistant { text, tool_calls, reasoning_parts, .. } => {
                let mut parts = Vec::new();
                // A reasoning entry whose item id names a tool call only carries that call's signature.
                let call_signature = |id: &str| reasoning_parts.iter().find(|r| r.item_id.as_deref() == Some(id)).and_then(|r| r.signature.as_ref());
                for r in reasoning_parts.iter().filter(|r| r.item_id.is_none() && !r.text.is_empty()) {
                    parts.push(signed(json!({"text": r.text, "thought": true}), r.signature.as_ref()));
                }
                if !text.is_empty() {
                    parts.push(signed(json!({"text": text}), call_signature("text")));
                }
                for call in tool_calls {
                    parts.push(signed(json!({"functionCall": {"name": call.name, "args": arguments(call)?}}), call_signature(&call.id)));
                }
                // A model turn left without parts is not sent.
                if !parts.is_empty() {
                    out.push(json!({"role": "model", "parts": parts}));
                }
            }
            Message::Tool { call_id, content, attachments, .. } => {
                let name = transform::tool_name(&req.messages[..i], call_id).map(|c| c.name.as_str()).unwrap_or(call_id.as_str());
                let mut parts = Vec::new();
                let media: Vec<Value> = attachments.iter().filter(|a| matches!(a, Part::Media { .. })).map(inline).collect();
                if media.is_empty() {
                    parts.push(json!({"functionResponse": {"name": name, "response": {"name": name, "content": content}}}));
                } else if response_parts {
                    let text = if content.is_empty() { "Tool executed successfully." } else { content.as_str() };
                    parts.push(json!({"functionResponse": {"name": name, "response": {"name": name, "content": text}, "parts": media}}));
                } else {
                    parts.push(json!({"functionResponse": {"name": name, "response": {"name": name, "content": content}}}));
                    for item in media {
                        parts.push(item);
                        parts.push(json!({"text": "Tool executed successfully and returned this image as a response"}));
                    }
                }
                // All results answering one model turn share a single user message.
                match out.last_mut().filter(|_| in_tool_group).and_then(|m| m["parts"].as_array_mut()) {
                    Some(existing) => existing.extend(parts),
                    None => out.push(json!({"role": "user", "parts": parts})),
                }
            }
        }
        in_tool_group = tool;
    }
    Ok(out)
}

pub fn encode(req: &Request) -> Result<Value, Error> {
    let gemma = req.model.to_lowercase().starts_with("gemma-");
    let mut contents = contents(req)?;
    let mut body = Map::new();
    if gemma && !req.system.is_empty() && contents.first().is_some_and(|c| c["role"] == "user") {
        // Gemma has no system instruction; the text leads the first user turn instead.
        let text = format!("{}\n\n", req.system.join("\n\n"));
        if let Some(parts) = contents[0]["parts"].as_array_mut() {
            parts.insert(0, json!({"text": text}));
        }
    }
    body.insert("contents".into(), contents.into());
    if !req.system.is_empty() && !gemma {
        let parts: Vec<Value> = req.system.iter().map(|text| json!({"text": text})).collect();
        body.insert("systemInstruction".into(), json!({"parts": parts}));
    }
    if !req.tools.is_empty() {
        let declarations: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                let mut declaration = json!({"name": t.name, "description": t.description});
                if let Some(parameters) = openapi_schema(&t.parameters, true) {
                    declaration["parameters"] = parameters;
                }
                declaration
            })
            .collect();
        body.insert("tools".into(), json!([{"functionDeclarations": declarations}]));
        let config = match &req.tool_choice {
            Some(ToolChoice::Auto) => Some(json!({"mode": "AUTO"})),
            Some(ToolChoice::None) => Some(json!({"mode": "NONE"})),
            Some(ToolChoice::Required) => Some(json!({"mode": "ANY"})),
            Some(ToolChoice::Tool(name)) => Some(json!({"mode": "ANY", "allowedFunctionNames": [name]})),
            None => None,
        };
        if let Some(config) = config {
            body.insert("toolConfig".into(), json!({"functionCallingConfig": config}));
        }
    }
    let mut generation = Map::new();
    if let Some(v) = req.max_tokens {
        generation.insert("maxOutputTokens".into(), v.into());
    }
    if let Some(v) = req.temperature {
        generation.insert("temperature".into(), number(v));
    }
    if let Some(v) = req.top_k {
        generation.insert("topK".into(), v.into());
    }
    if let Some(v) = req.top_p {
        generation.insert("topP".into(), number(v));
    }
    let option = |key: &str| req.options.get(key).filter(|v| !v.is_null());
    for key in ["responseModalities", "thinkingConfig", "mediaResolution", "imageConfig"] {
        if let Some(value) = option(key) {
            generation.insert(key.into(), value.clone());
        }
    }
    body.insert("generationConfig".into(), generation.into());
    for key in ["safetySettings", "cachedContent", "labels", "serviceTier"] {
        if let Some(value) = option(key) {
            body.insert(key.into(), value.clone());
        }
    }
    Ok(Value::Object(body))
}

#[derive(Default)]
pub struct Decoder {
    calls: u64,
    finish: Option<String>,
    usage: Option<Value>,
    thoughts: Vec<Reasoning>,
    thinking: bool,
    completion: Completion,
}

fn usage(usage: Option<&Value>) -> Usage {
    let n = |key: &str| usage.and_then(|u| u.get(key)).and_then(Value::as_u64).unwrap_or(0);
    let (input, thoughts) = (n("promptTokenCount"), n("thoughtsTokenCount"));
    let output = n("candidatesTokenCount") + thoughts;
    Usage { input, output, reasoning: thoughts, cache_read: n("cachedContentTokenCount"), cache_write: 0, total: input + output }
}

impl Decoder {
    pub fn push(&mut self, data: &str) -> Result<Vec<Event>, Error> {
        if data.trim().is_empty() || data.trim() == "[DONE]" {
            return Ok(Vec::new());
        }
        let chunk: Value = serde_json::from_str(data).map_err(|_| Error::Stream("Invalid google/gemini stream event".into()))?;
        if let Some(error) = chunk.get("error").filter(|e| e.is_object()) {
            let text = |key: &str| error.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
            return Err(Error::Provider { code: text("status"), message: text("message").unwrap_or_else(|| error.to_string()) });
        }
        if let Some(usage) = chunk.get("usageMetadata").filter(|u| u.is_object()) {
            self.usage = Some(usage.clone());
        }
        let mut events = Vec::new();
        let Some(candidate) = chunk.pointer("/candidates/0") else { return Ok(events) };
        for part in candidate.pointer("/content/parts").and_then(Value::as_array).into_iter().flatten() {
            let signature = part.get("thoughtSignature").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
            if let Some(call) = part.get("functionCall").filter(|c| c.get("name").is_some_and(Value::is_string)) {
                self.thinking = false;
                let id = match call.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    Some(id) => id.to_string(),
                    None => format!("tool_{}", self.calls),
                };
                self.calls += 1;
                let arguments = match call.get("args") {
                    Some(Value::String(raw)) => raw.clone(),
                    Some(args) if !args.is_null() => args.to_string(),
                    _ => "{}".to_string(),
                };
                if let Some(signature) = signature {
                    // Replayed with the call: Gemini rejects function calls stripped of their signature.
                    self.completion.reasoning.push(Reasoning { signature: Some(signature), item_id: Some(id.clone()), ..Default::default() });
                }
                events.push(Event::ToolCall(ToolCall { id, name: call["name"].as_str().unwrap_or_default().to_string(), arguments }));
            } else if let Some(text) = part.get("text").and_then(Value::as_str) {
                if part.get("thought") == Some(&Value::Bool(true)) {
                    if !self.thinking {
                        self.thoughts.push(Reasoning::default());
                        self.thinking = true;
                    }
                    let block = self.thoughts.last_mut().unwrap();
                    block.text.push_str(text);
                    block.signature = signature.or(block.signature.take());
                    if !text.is_empty() {
                        events.push(Event::ReasoningDelta(text.to_string()));
                    }
                } else {
                    self.thinking = false;
                    if let Some(signature) = signature {
                        self.completion.reasoning.push(Reasoning { signature: Some(signature), item_id: Some("text".into()), ..Default::default() });
                    }
                    if !text.is_empty() {
                        events.push(Event::TextDelta(text.to_string()));
                    }
                }
            }
        }
        if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
            self.finish = Some(reason.to_string());
        }
        Ok(events)
    }

    // The finish event is emitted once the stream ends; usage arrives on any chunk.
    pub fn finish(mut self) -> Result<(Vec<Event>, Completion), Error> {
        let Some(reason) = self.finish else {
            return Err(Error::Stream("Provider stream ended without a terminal finish event".into()));
        };
        let reason = match reason.as_str() {
            "STOP" if self.calls > 0 => "tool-calls",
            "STOP" => "stop",
            "MAX_TOKENS" => "length",
            "IMAGE_SAFETY" | "RECITATION" | "SAFETY" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" => "content-filter",
            "MALFORMED_FUNCTION_CALL" => "error",
            _ => "unknown",
        };
        let mut reasoning = std::mem::take(&mut self.thoughts);
        reasoning.append(&mut self.completion.reasoning);
        self.completion.reasoning = reasoning;
        Ok((vec![Event::Finish { reason: reason.to_string(), usage: usage(self.usage.as_ref()) }], self.completion))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{cassette, response_body};
    use crate::{Tool, sse};

    const DIR: &str = "llm/test/fixtures/recordings";

    fn replay(body: &str) -> (Vec<Event>, Completion) {
        let mut sse = sse::Decoder::default();
        let mut decoder = Decoder::default();
        let mut events = Vec::new();
        for piece in body.as_bytes().chunks(9) {
            for data in sse.feed(piece) {
                events.extend(decoder.push(&data).unwrap());
            }
        }
        for data in sse.finish() {
            events.extend(decoder.push(&data).unwrap());
        }
        let (last, completion) = decoder.finish().unwrap();
        events.extend(last);
        (events, completion)
    }

    fn weather() -> Tool {
        Tool {
            name: "get_weather".into(),
            description: "Get current weather for a city.".into(),
            parameters: json!({"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"], "additionalProperties": false}),
        }
    }

    #[test]
    fn text_request_and_stream_match_recording() {
        let Some(c) = cassette(&format!("{DIR}/gemini/streams-text.json")) else { return };
        let req = Request {
            model: "gemini-2.5-flash".into(),
            system: vec!["You are concise.".into()],
            messages: vec![Message::User("Reply with exactly: Hello!".into())],
            max_tokens: Some(80),
            temperature: Some(0.0),
            ..Default::default()
        };
        assert_eq!(serde_json::to_string(&encode(&req).unwrap()).unwrap(), c["interactions"][0]["request"]["body"].as_str().unwrap());
        assert_eq!(format!("{DEFAULT_BASE_URL}{}", path(&req.model)), c["interactions"][0]["request"]["url"].as_str().unwrap());
        let (events, completion) = replay(response_body(&c, 0));
        assert_eq!(events, [Event::TextDelta("Hello!".into()), Event::Finish { reason: "stop".into(), usage: Usage { input: 11, output: 18, reasoning: 16, cache_read: 0, cache_write: 0, total: 29 } }]);
        assert!(completion.reasoning.is_empty());
    }

    #[test]
    fn tool_call_request_stream_and_replay() {
        let Some(c) = cassette(&format!("{DIR}/gemini/streams-tool-call.json")) else { return };
        let mut req = Request {
            model: "gemini-2.5-flash".into(),
            system: vec!["Call tools exactly as requested.".into()],
            messages: vec![Message::User("Call get_weather with city exactly Paris.".into())],
            tools: vec![weather()],
            tool_choice: Some(ToolChoice::Tool("get_weather".into())),
            max_tokens: Some(80),
            temperature: Some(0.0),
            ..Default::default()
        };
        assert_eq!(serde_json::to_string(&encode(&req).unwrap()).unwrap(), c["interactions"][0]["request"]["body"].as_str().unwrap());
        let (events, completion) = replay(response_body(&c, 0));
        let Event::ToolCall(call) = &events[0] else { panic!("{events:?}") };
        assert_eq!((call.id.as_str(), call.name.as_str(), call.arguments.as_str()), ("tool_0", "get_weather", "{\"city\":\"Paris\"}"));
        assert!(matches!(&events[1], Event::Finish { reason, usage } if reason == "tool-calls" && usage.reasoning == 45 && usage.output == 60));
        let signature = completion.reasoning[0].signature.clone().unwrap();
        assert_eq!(completion.reasoning[0].item_id.as_deref(), Some("tool_0"));

        // The signature returns on the function call; the result is matched to the call by name.
        req.messages.push(Message::Assistant { text: String::new(), reasoning: String::new(), tool_calls: vec![call.clone()], reasoning_parts: completion.reasoning });
        req.messages.push(Message::tool("tool_0", "{\"temperature\":22}"));
        req.messages.push(Message::Tool { call_id: "tool_0".into(), content: "shot".into(), is_error: false, attachments: vec![Part::Media { mime: "image/png".into(), data: "AAAA".into(), filename: None }] });
        let body = encode(&req).unwrap();
        assert_eq!(
            body["contents"],
            json!([
                {"role": "user", "parts": [{"text": "Call get_weather with city exactly Paris."}]},
                {"role": "model", "parts": [{"functionCall": {"name": "get_weather", "args": {"city": "Paris"}}, "thoughtSignature": signature}]},
                {"role": "user", "parts": [
                    {"functionResponse": {"name": "get_weather", "response": {"name": "get_weather", "content": "{\"temperature\":22}"}}},
                    {"functionResponse": {"name": "get_weather", "response": {"name": "get_weather", "content": "shot"}}},
                    {"inlineData": {"mimeType": "image/png", "data": "AAAA"}},
                    {"text": "Tool executed successfully and returned this image as a response"}
                ]}
            ])
        );
        req.model = "gemini-3-pro-preview".into();
        assert_eq!(
            encode(&req).unwrap()["contents"][2]["parts"][1],
            json!({"functionResponse": {"name": "get_weather", "response": {"name": "get_weather", "content": "shot"}, "parts": [{"inlineData": {"mimeType": "image/png", "data": "AAAA"}}]}})
        );
    }

    #[test]
    fn cached_tokens_and_image_recordings() {
        let Some(c) = cassette(&format!("{DIR}/gemini-cache/reports-cachedcontenttokencount-on-identical-second-call.json")) else { return };
        let (first, _) = replay(response_body(&c, 0));
        let (second, _) = replay(response_body(&c, 1));
        assert!(matches!(first.last(), Some(Event::Finish { usage, .. }) if usage.cache_read == 0 && usage.input == 1200));
        assert!(matches!(second.last(), Some(Event::Finish { usage, .. }) if usage.cache_read == 1100 && usage.input == 1200 && usage.total == 1202));
        let Some(c) = cassette(&format!("{DIR}/gemini/gemini-2-5-flash-image.json")) else { return };
        for interaction in c["interactions"].as_array().unwrap() {
            let (events, _) = replay(interaction["response"]["body"].as_str().unwrap());
            assert!(matches!(events.last(), Some(Event::Finish { .. })));
        }
    }

    #[test]
    fn thinking_options_parts_and_errors() {
        let req = Request {
            model: "gemini-3-flash-preview".into(),
            system: vec!["a".into(), "b".into()],
            messages: vec![
                Message::UserParts(vec![Part::Text("look".into()), Part::Media { mime: "application/pdf".into(), data: "AAAA".into(), filename: None }]),
                Message::Assistant {
                    text: "answer".into(),
                    reasoning: String::new(),
                    tool_calls: Vec::new(),
                    reasoning_parts: vec![Reasoning { text: "plan".into(), signature: Some("sig".into()), ..Default::default() }, Reasoning::default()],
                },
                Message::Assistant { text: String::new(), reasoning: String::new(), tool_calls: Vec::new(), reasoning_parts: Vec::new() },
            ],
            tools: vec![Tool { name: "noargs".into(), description: "d".into(), parameters: json!({"type": "object", "properties": {}}) }],
            top_p: Some(0.95),
            top_k: Some(64),
            options: json!({"thinkingConfig": {"includeThoughts": true, "thinkingLevel": "high"}, "ignored": 1}),
            ..Default::default()
        };
        assert_eq!(
            encode(&req).unwrap(),
            json!({
                "contents": [
                    {"role": "user", "parts": [{"text": "look"}, {"inlineData": {"mimeType": "application/pdf", "data": "AAAA"}}]},
                    {"role": "model", "parts": [{"text": "plan", "thought": true, "thoughtSignature": "sig"}, {"text": "answer"}]}
                ],
                "systemInstruction": {"parts": [{"text": "a"}, {"text": "b"}]},
                "tools": [{"functionDeclarations": [{"name": "noargs", "description": "d"}]}],
                "generationConfig": {"topK": 64, "topP": 0.95, "thinkingConfig": {"includeThoughts": true, "thinkingLevel": "high"}}
            })
        );
        let schema = json!({"type": "object", "properties": {
            "a": {"type": ["string", "null"], "description": "d", "default": 1},
            "b": {"anyOf": [{"type": "integer", "minimum": 1}, {"type": "null"}]},
            "c": {"type": "object", "properties": {}},
            "d": {"const": "x"},
            "e": {"type": "array", "items": {"type": "string", "minLength": 2, "pattern": "x"}}
        }, "required": ["a"], "additionalProperties": false});
        assert_eq!(
            openapi_schema(&schema, true).unwrap(),
            json!({"required": ["a"], "type": "object", "properties": {
                "a": {"description": "d", "anyOf": [{"type": "string"}], "nullable": true},
                "b": {"nullable": true, "type": "integer"},
                "c": {"type": "object"},
                "d": {"enum": ["x"]},
                "e": {"type": "array", "items": {"type": "string", "minLength": 2}}
            }})
        );

        let stream = [
            r#"{"candidates":[{"content":{"parts":[{"text":"Let me ","thought":true}]}}]}"#,
            r#"{"candidates":[{"content":{"parts":[{"text":"think.","thought":true,"thoughtSignature":"s1"},{"text":"Hi"}]}}],"usageMetadata":{"promptTokenCount":5}}"#,
            r#"{"candidates":[{"content":{"parts":[{"text":""}]},"finishReason":"MAX_TOKENS"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"thoughtsTokenCount":3,"cachedContentTokenCount":1}}"#,
        ];
        let mut decoder = Decoder::default();
        let events: Vec<Event> = stream.iter().flat_map(|data| decoder.push(data).unwrap()).collect();
        assert_eq!(events, [Event::ReasoningDelta("Let me ".into()), Event::ReasoningDelta("think.".into()), Event::TextDelta("Hi".into())]);
        let (last, completion) = decoder.finish().unwrap();
        assert_eq!(last, [Event::Finish { reason: "length".into(), usage: Usage { input: 5, output: 5, reasoning: 3, cache_read: 1, cache_write: 0, total: 10 } }]);
        assert_eq!(completion.reasoning, [Reasoning { text: "Let me think.".into(), signature: Some("s1".into()), ..Default::default() }]);

        let error = Decoder::default().push(r#"{"error":{"code":429,"message":"Resource has been exhausted","status":"RESOURCE_EXHAUSTED"}}"#).unwrap_err();
        assert!(crate::retry::classify(&error, "google").retryable);
        assert!(matches!(Decoder::default().finish(), Err(Error::Stream(_))));
    }
}
