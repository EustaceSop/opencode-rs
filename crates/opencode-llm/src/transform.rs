// Model-specific rewrites of the outgoing prompt and of tool schemas.
use crate::provider::Model;
use crate::{Message, Part, ToolCall};
use serde_json::{Map, Value, json};

pub const EMPTY_IMAGE: &str = "ERROR: Image file is empty or corrupted. Please provide a valid image.";

fn modality(mime: &str) -> Option<&'static str> {
    if mime.starts_with("image/") {
        Some("image")
    } else if mime.starts_with("audio/") {
        Some("audio")
    } else if mime.starts_with("video/") {
        Some("video")
    } else if mime == "application/pdf" {
        Some("pdf")
    } else {
        None
    }
}

fn unsupported(part: Part, model: &Model) -> Part {
    let Part::Media { mime, data, filename } = &part else { return part };
    if mime.starts_with("image/") && data.is_empty() {
        return Part::Text(EMPTY_IMAGE.into());
    }
    let Some(kind) = modality(mime) else { return part };
    let supported = match kind {
        "image" => model.input.image,
        "audio" => model.input.audio,
        "video" => model.input.video,
        _ => model.input.pdf,
    };
    if supported {
        return part;
    }
    let name = filename.as_ref().filter(|f| !f.is_empty()).map(|f| format!("\"{f}\"")).unwrap_or_else(|| kind.to_string());
    Part::Text(format!("ERROR: Cannot read {name} (this model does not support {kind} input). Inform the user."))
}

fn scrub_ids(messages: &mut [Message], scrub: impl Fn(&str) -> String) {
    for message in messages {
        match message {
            Message::Assistant { tool_calls, .. } => tool_calls.iter_mut().for_each(|c| c.id = scrub(&c.id)),
            Message::Tool { call_id, .. } => *call_id = scrub(call_id),
            _ => {}
        }
    }
}

fn mistral_family(model: &Model) -> bool {
    let id = model.api_id.to_lowercase();
    model.provider_id == "mistral" || ["mistral", "devstral", "codestral", "pixtral", "mixtral"].iter().any(|f| id.contains(f))
}

// `ProviderTransform.message` for the parts of it that change the wire body.
pub fn messages(messages: Vec<Message>, model: &Model) -> Vec<Message> {
    let mut out: Vec<Message> = messages
        .into_iter()
        .map(|m| match m {
            Message::UserParts(parts) => Message::UserParts(parts.into_iter().map(|p| unsupported(p, model)).collect()),
            other => other,
        })
        .collect();

    if model.npm == "@ai-sdk/anthropic" {
        out = out
            .into_iter()
            .filter_map(|m| match m {
                Message::User(text) if text.is_empty() => None,
                Message::UserParts(mut parts) => {
                    parts.retain(|p| !matches!(p, Part::Text(t) if t.is_empty()));
                    (!parts.is_empty()).then_some(Message::UserParts(parts))
                }
                Message::Assistant { text, reasoning, tool_calls, mut reasoning_parts } => {
                    reasoning_parts.retain(|r| !r.text.trim().is_empty() || r.signature.is_some() || r.encrypted.is_some());
                    let empty = text.is_empty() && tool_calls.is_empty() && reasoning_parts.is_empty();
                    (!empty).then_some(Message::Assistant { text, reasoning, tool_calls, reasoning_parts })
                }
                other => Some(other),
            })
            .collect();
    }
    if model.api_id.contains("claude") {
        scrub_ids(&mut out, |id| id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect());
    }
    if mistral_family(model) {
        scrub_ids(&mut out, |id| {
            let mut short: String = id.chars().filter(char::is_ascii_alphanumeric).take(9).collect();
            while short.len() < 9 {
                short.push('0');
            }
            short
        });
        let mut fixed = Vec::with_capacity(out.len());
        let mut iter = out.into_iter().peekable();
        while let Some(message) = iter.next() {
            let tool = matches!(message, Message::Tool { .. });
            fixed.push(message);
            if tool && matches!(iter.peek(), Some(Message::User(_) | Message::UserParts(_))) {
                fixed.push(Message::assistant("Done.", Vec::new()));
            }
        }
        return fixed;
    }
    out
}

// Whether any earlier turn used tools; Copilot rejects tool history without a tool list.
pub fn has_tool_history(messages: &[Message]) -> bool {
    messages.iter().any(|m| matches!(m, Message::Tool { .. }) || matches!(m, Message::Assistant { tool_calls, .. } if !tool_calls.is_empty()))
}

pub fn noop_tool() -> crate::Tool {
    crate::Tool {
        name: "_noop".into(),
        description: "Do not call this tool. It exists only for API compatibility and must never be invoked.".into(),
        parameters: json!({"type": "object", "properties": {"reason": {"type": "string", "description": "Unused"}}}),
    }
}

// Name of the tool a result answers, looked up in the preceding assistant turns.
pub(crate) fn tool_name<'a>(messages: &'a [Message], call_id: &str) -> Option<&'a ToolCall> {
    messages.iter().rev().find_map(|m| match m {
        Message::Assistant { tool_calls, .. } => tool_calls.iter().find(|c| c.id == call_id),
        _ => None,
    })
}

const JSON_TYPES: [&str; 7] = ["string", "number", "boolean", "integer", "object", "array", "null"];
const COMPOSITION: [&str; 3] = ["anyOf", "oneOf", "allOf"];

// Codex-compatible lowering of a JSON schema for OpenAI tool definitions.
pub fn sanitize_openai(value: &Value) -> Value {
    let source = match value {
        Value::Bool(_) => return json!({"type": "string"}),
        Value::Array(items) => return items.iter().map(sanitize_openai).collect(),
        Value::Object(map) => map,
        other => return other.clone(),
    };
    let mut out = Map::new();
    for key in ["$ref", "description"] {
        if let Some(Value::String(text)) = source.get(key) {
            out.insert(key.into(), text.clone().into());
        }
    }
    if let Some(constant) = source.get("const") {
        out.insert("enum".into(), json!([constant]));
    } else if let Some(Value::Array(values)) = source.get("enum") {
        out.insert("enum".into(), values.clone().into());
    }
    let each = |map: &Map<String, Value>| Value::Object(map.iter().map(|(k, v)| (k.clone(), sanitize_openai(v))).collect());
    if let Some(Value::Object(properties)) = source.get("properties") {
        out.insert("properties".into(), each(properties));
    }
    if let Some(Value::Array(required)) = source.get("required") {
        out.insert("required".into(), required.iter().filter(|v| v.is_string()).cloned().collect());
    }
    if let Some(items) = source.get("items") {
        out.insert("items".into(), sanitize_openai(items));
    }
    if let Some(extra) = source.get("additionalProperties") {
        out.insert("additionalProperties".into(), if extra.is_boolean() { extra.clone() } else { sanitize_openai(extra) });
    }
    for key in COMPOSITION {
        if let Some(Value::Array(items)) = source.get(key) {
            out.insert(key.into(), items.iter().map(sanitize_openai).collect());
        }
    }
    for key in ["$defs", "definitions"] {
        if let Some(Value::Object(defs)) = source.get(key) {
            out.insert(key.into(), each(defs));
        }
    }
    let known = |v: &Value| v.as_str().filter(|t| JSON_TYPES.contains(t)).map(str::to_string);
    let mut types: Vec<String> = match source.get("type") {
        Some(Value::String(_)) => known(&source["type"]).into_iter().collect(),
        Some(Value::Array(list)) => list.iter().filter_map(known).collect(),
        _ => Vec::new(),
    };
    if types.is_empty() && (out.contains_key("$ref") || COMPOSITION.iter().any(|k| out.contains_key(*k))) {
        return Value::Object(out);
    }
    if types.is_empty() {
        let has = |keys: &[&str]| keys.iter().any(|k| source.contains_key(*k));
        let inferred = if has(&["properties", "required", "additionalProperties"]) {
            "object"
        } else if has(&["items", "prefixItems"]) {
            "array"
        } else if out.contains_key("enum") || source.contains_key("format") {
            "string"
        } else if has(&["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum", "multipleOf"]) {
            "number"
        } else {
            return json!({});
        };
        types.push(inferred.into());
    }
    let (object, array) = (types.iter().any(|t| t == "object"), types.iter().any(|t| t == "array"));
    out.insert("type".into(), if types.len() == 1 { types.remove(0).into() } else { types.into() });
    if object && !out.contains_key("properties") {
        out.insert("properties".into(), json!({}));
    }
    if array && !out.contains_key("items") {
        out.insert("items".into(), json!({"type": "string"}));
    }
    Value::Object(out)
}

fn sanitize_moonshot(value: &Value) -> Value {
    match value {
        Value::Array(items) => items.iter().map(sanitize_moonshot).collect(),
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get("$ref") {
                return json!({"$ref": reference});
            }
            let mut out: Map<String, Value> = map.iter().map(|(k, v)| (k.clone(), sanitize_moonshot(v))).collect();
            if let Some(Value::Array(items)) = out.get("items") {
                let first = items.first().cloned().unwrap_or_else(|| json!({}));
                out.insert("items".into(), first);
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

// JavaScript `String(value)` for enum members.
fn js_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if n.is_f64() && f.fract() == 0.0 && f.abs() < 1e21 => format!("{f:.0}"),
            _ => n.to_string(),
        },
        Value::Array(items) => items.iter().map(js_string).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".into(),
        other => other.to_string(),
    }
}

fn has_combiner(map: &Map<String, Value>) -> bool {
    COMPOSITION.iter().any(|k| map.get(*k).is_some_and(Value::is_array))
}

fn sanitize_gemini(value: &Value) -> Value {
    let map = match value {
        Value::Array(items) => return items.iter().map(sanitize_gemini).collect(),
        Value::Object(map) => map,
        other => return other.clone(),
    };
    let mut out = Map::new();
    for (key, value) in map {
        match value {
            Value::Array(values) if key == "enum" => {
                out.insert(key.clone(), values.iter().map(|v| Value::String(js_string(v))).collect());
                if matches!(out.get("type").and_then(Value::as_str), Some("integer" | "number")) {
                    out.insert("type".into(), "string".into());
                }
            }
            Value::Array(_) | Value::Object(_) => {
                out.insert(key.clone(), sanitize_gemini(value));
            }
            other => {
                out.insert(key.clone(), other.clone());
            }
        }
    }
    if let Some(Value::Array(types)) = out.get("type").cloned() {
        let non_null: Vec<&Value> = types.iter().filter(|t| t.as_str() != Some("null")).collect();
        if non_null.is_empty() {
            out.insert("type".into(), "null".into());
        } else {
            out.shift_remove("type");
            out.insert("anyOf".into(), non_null.iter().map(|t| json!({"type": t})).collect());
            if non_null.len() != types.len() {
                out.insert("nullable".into(), true.into());
            }
        }
    }
    let kind = out.get("type").and_then(Value::as_str).map(str::to_string);
    if kind.as_deref() == Some("object")
        && let Some(properties) = out.get("properties").filter(|p| !p.is_null() && p != &&Value::Bool(false)).cloned()
        && let Some(Value::Array(required)) = out.get("required").cloned()
    {
        let exists = |field: &Value| match (&properties, field.as_str()) {
            (Value::Object(p), Some(name)) => p.contains_key(name),
            _ => false,
        };
        out.insert("required".into(), required.into_iter().filter(exists).collect());
    }
    if kind.as_deref() == Some("array") && !has_combiner(&out) {
        if out.get("items").is_none_or(Value::is_null) {
            out.insert("items".into(), json!({}));
        }
        if let Some(Value::Object(items)) = out.get_mut("items") {
            let intent = ["type", "properties", "items", "prefixItems", "enum", "const", "$ref", "additionalProperties", "patternProperties", "required", "not", "if", "then", "else"];
            if !has_combiner(items) && !intent.iter().any(|k| items.contains_key(*k)) {
                items.insert("type".into(), "string".into());
            }
        }
    }
    if kind.as_deref().is_some_and(|k| !k.is_empty() && k != "object") && !has_combiner(&out) {
        out.shift_remove("properties");
        out.shift_remove("required");
    }
    Value::Object(out)
}

// `ProviderTransform.schema`: per-provider tool schema rewrites, applied in the original's order.
pub fn schema(model: &Model, schema: &Value) -> Value {
    let mut out = schema.clone();
    if model.npm == "@ai-sdk/openai" || model.npm == "@ai-sdk/azure" {
        out = sanitize_openai(&out);
    }
    if model.provider_id == "moonshotai" || model.api_id.to_lowercase().contains("kimi") {
        let sanitized = sanitize_moonshot(&out);
        if sanitized.is_object() {
            out = sanitized;
        }
    }
    if model.provider_id == "google" || model.api_id.contains("gemini") {
        out = sanitize_gemini(&out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Reasoning, provider};
    use opencode_core::auth;

    fn model(provider_id: &str, api_id: &str, npm: &str, input: &[&str]) -> Model {
        let catalog = json!({provider_id: {"id": provider_id, "env": [], "npm": npm, "api": "https://example.test/v1", "models": {
            api_id: {"id": api_id, "modalities": {"input": input, "output": ["text"]}}
        }}});
        let auth = vec![(provider_id.to_string(), auth::Info::Api { key: "k".into(), metadata: None })];
        provider::build(&catalog, &json!({}), &auth).remove(0).models.remove(0)
    }

    fn media(mime: &str, data: &str, name: Option<&str>) -> Part {
        Part::Media { mime: mime.into(), data: data.into(), filename: name.map(str::to_string) }
    }

    fn call(id: &str) -> ToolCall {
        ToolCall { id: id.into(), name: "t".into(), arguments: "{}".into() }
    }

    #[test]
    fn unsupported_and_empty_parts() {
        let m = model("anthropic", "claude-sonnet-4-5", "@ai-sdk/anthropic", &["text", "image"]);
        let input = vec![
            Message::User(String::new()),
            Message::UserParts(vec![Part::Text(String::new())]),
            Message::UserParts(vec![media("image/png", "AAAA", None), media("image/png", "", None), media("application/pdf", "AAAA", Some("a.pdf")), media("audio/wav", "AAAA", None), media("text/csv", "AAAA", None)]),
            Message::Assistant { text: String::new(), reasoning: String::new(), tool_calls: Vec::new(), reasoning_parts: vec![Reasoning { text: "  ".into(), ..Default::default() }] },
            Message::Assistant {
                text: String::new(),
                reasoning: String::new(),
                tool_calls: vec![call("a.b:c")],
                reasoning_parts: vec![Reasoning { text: " ".into(), ..Default::default() }, Reasoning { signature: Some("s".into()), ..Default::default() }],
            },
            Message::tool("a.b:c", "ok"),
        ];
        let out = messages(input, &m);
        assert_eq!(out.len(), 3);
        assert_eq!(
            out[0],
            Message::UserParts(vec![
                media("image/png", "AAAA", None),
                Part::Text(EMPTY_IMAGE.into()),
                Part::Text("ERROR: Cannot read \"a.pdf\" (this model does not support pdf input). Inform the user.".into()),
                Part::Text("ERROR: Cannot read audio (this model does not support audio input). Inform the user.".into()),
                media("text/csv", "AAAA", None),
            ])
        );
        assert!(matches!(&out[1], Message::Assistant { tool_calls, reasoning_parts, .. } if tool_calls[0].id == "a_b_c" && reasoning_parts.len() == 1));
        assert!(matches!(&out[2], Message::Tool { call_id, .. } if call_id == "a_b_c"));
    }

    #[test]
    fn mistral_ids_and_sequence() {
        let m = model("mistral", "devstral-medium", "@ai-sdk/mistral", &["text"]);
        let input = vec![Message::assistant("", vec![call("call_ab-1")]), Message::tool("call_ab-1", "ok"), Message::User("next".into()), Message::User(String::new())];
        let out = messages(input, &m);
        assert_eq!(out.len(), 5);
        assert!(matches!(&out[0], Message::Assistant { tool_calls, .. } if tool_calls[0].id == "callab100"));
        assert!(matches!(&out[1], Message::Tool { call_id, .. } if call_id == "callab100"));
        assert_eq!(out[2], Message::assistant("Done.", Vec::new()));
        assert!(has_tool_history(&out) && !has_tool_history(&out[2..]));
    }

    #[test]
    fn openai_schema_lowering() {
        let m = model("openai", "gpt-5.5", "@ai-sdk/openai", &["text"]);
        let input = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "title": "dropped",
            "properties": {
                "flag": true,
                "kind": {"const": "a", "default": "a"},
                "list": {"type": "array", "minItems": 1},
                "implied": {"properties": {"x": {"type": ["string", "null", "bogus"]}}, "required": ["x", 3]},
                "num": {"minimum": 1},
                "any": {"title": "nothing"},
                "ref": {"$ref": "#/$defs/thing", "description": "d", "default": 1},
                "union": {"oneOf": [{"type": "string", "pattern": "x"}, false]},
                "fmt": {"format": "uri"},
                "map": {"type": "object", "additionalProperties": {"type": "integer", "maximum": 3}}
            },
            "required": ["flag"],
            "additionalProperties": false,
            "$defs": {"thing": {"enum": ["a", 1]}}
        });
        assert_eq!(
            schema(&m, &input),
            json!({
                "properties": {
                    "flag": {"type": "string"},
                    "kind": {"enum": ["a"], "type": "string"},
                    "list": {"type": "array", "items": {"type": "string"}},
                    "implied": {"properties": {"x": {"type": ["string", "null"]}}, "required": ["x"], "type": "object"},
                    "num": {"type": "number"},
                    "any": {},
                    "ref": {"$ref": "#/$defs/thing", "description": "d"},
                    "union": {"oneOf": [{"type": "string"}, {"type": "string"}]},
                    "fmt": {"type": "string"},
                    "map": {"additionalProperties": {"type": "integer"}, "type": "object", "properties": {}}
                },
                "required": ["flag"],
                "additionalProperties": false,
                "$defs": {"thing": {"enum": ["a", 1], "type": "string"}},
                "type": "object"
            })
        );
        assert_eq!(serde_json::to_string(&sanitize_openai(&json!({"type": "object"}))).unwrap(), r#"{"type":"object","properties":{}}"#);
        let plain = model("deepseek", "deepseek-chat", "@ai-sdk/openai-compatible", &["text"]);
        assert_eq!(schema(&plain, &input), input);
    }

    #[test]
    fn moonshot_and_gemini_schemas() {
        let kimi = model("moonshotai", "kimi-k2", "@ai-sdk/openai-compatible", &["text"]);
        let input = json!({"type": "object", "properties": {"a": {"$ref": "#/x", "description": "d"}, "t": {"type": "array", "items": [{"type": "string"}, {"type": "number"}]}, "e": {"type": "array", "items": []}}});
        assert_eq!(schema(&kimi, &input), json!({"type": "object", "properties": {"a": {"$ref": "#/x"}, "t": {"type": "array", "items": {"type": "string"}}, "e": {"type": "array", "items": {}}}}));

        let gemini = model("google", "gemini-2.5-pro", "@ai-sdk/google", &["text"]);
        let input = json!({
            "type": "object",
            "properties": {
                "level": {"type": "integer", "enum": [1, 2.0, true]},
                "mixed": {"type": ["number", "string", "null"]},
                "nothing": {"type": ["null"]},
                "list": {"type": "array"},
                "typed": {"type": "array", "items": {"description": "x"}},
                "kept": {"type": "array", "items": {"anyOf": [{"type": "string"}]}},
                "text": {"type": "string", "properties": {"x": {}}, "required": ["x"]}
            },
            "required": ["level", "ghost"]
        });
        assert_eq!(
            schema(&gemini, &input),
            json!({
                "type": "object",
                "properties": {
                    "level": {"type": "string", "enum": ["1", "2", "true"]},
                    "mixed": {"anyOf": [{"type": "number"}, {"type": "string"}], "nullable": true},
                    "nothing": {"type": "null"},
                    "list": {"type": "array", "items": {"type": "string"}},
                    "typed": {"type": "array", "items": {"description": "x", "type": "string"}},
                    "kept": {"type": "array", "items": {"anyOf": [{"type": "string"}]}},
                    "text": {"type": "string"}
                },
                "required": ["level"]
            })
        );
    }
}
