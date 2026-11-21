//! Rust build: tool input repair from opencode 2.x (`packages/core/src/plugin/tool-input-repair.ts`), applied to a
//! call's arguments before the tool parses them. Repairs apply only when the input schema unambiguously supports them:
//! - Stringified root or nested object: '{"limit":"20"}' -> { limit: 20 }
//! - Closed object: { limit: "20", extra: true } -> { limit: 20 }
//! - Optional null or empty-object placeholder: { limit: null } -> {}
//! - Numeric or boolean string: { limit: "20", enabled: "false" } -> { limit: 20, enabled: false }
//! - Nullable field: { count: "2" } -> { count: 2 }
//! - Stringified array or compatible item: { tags: '["a"]', count: "2" } -> { tags: ["a"], count: [2] }
//! - Positional tuple: { pair: ["2", "false"] } -> { pair: [2, false] }
//! - Typed dictionary: { counts: { first: "2" } } -> { counts: { first: 2 } }
//! - Nested fields and local references: { items: [{ count: "2" }] } -> { items: [{ count: 2 }] }

use serde_json::{Map, Value};

const MAX_DEPTH: usize = 6;
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// Repairs `input` against an object `schema`; anything else is returned unchanged.
pub fn repair_input(input: Value, schema: &Value) -> Value {
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return input;
    }
    repair(&input, schema, schema, 0).unwrap_or(input)
}

// `Predicate.isObject`: records and arrays.
fn is_object(value: &Value) -> bool {
    value.is_object() || value.is_array()
}

// JavaScript `typeof` of a JSON value.
fn type_of(value: &Value) -> &'static str {
    match value {
        Value::Null | Value::Object(_) | Value::Array(_) => "object",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
    }
}

fn decode_json(text: &str) -> Option<Value> {
    serde_json::from_str(text).ok()
}

// `Number(text)` for a non-blank string: decimal, exponent and 0x/0o/0b forms.
fn js_number(text: &str) -> Option<f64> {
    let trimmed = text.trim();
    let radix = |prefix: [&str; 2], base: u32| {
        prefix.iter().find_map(|p| trimmed.strip_prefix(p)).map(|digits| u64::from_str_radix(digits, base).ok().map(|n| n as f64))
    };
    if let Some(parsed) = radix(["0x", "0X"], 16).or_else(|| radix(["0o", "0O"], 8)).or_else(|| radix(["0b", "0B"], 2)) {
        return parsed;
    }
    if trimmed.chars().any(|c| c.is_ascii_alphabetic() && c != 'e' && c != 'E') {
        return None;
    }
    trimmed.parse::<f64>().ok()
}

fn number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() <= MAX_SAFE_INTEGER {
        Value::from(value as i64)
    } else {
        serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
    }
}

// None means "unchanged".
fn repair(value: &Value, schema: &Value, root: &Value, depth: usize) -> Option<Value> {
    if depth > MAX_DEPTH {
        return None;
    }
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let definitions = if is_pointer(reference, "#/$defs/") {
            root.get("$defs")
        } else if is_pointer(reference, "#/definitions/") {
            root.get("definitions")
        } else {
            None
        };
        let name = reference[reference.rfind('/').map_or(0, |i| i + 1)..].replace("~1", "/").replace("~0", "~");
        let target = definitions.filter(|d| d.is_object())?.get(&name).filter(|t| t.is_object())?;
        return repair(value, target, root, depth + 1);
    }

    match schema.get("type") {
        Some(Value::Array(types)) => {
            let has = |name: &str| types.iter().any(|t| t.as_str() == Some(name));
            if value.is_null() && has("null") {
                return None;
            }
            if has(type_of(value)) {
                return None;
            }
            let rest: Vec<&Value> = types.iter().filter(|t| t.as_str() != Some("null")).collect();
            if rest.len() != 1 {
                return None;
            }
            let mut single = schema.clone();
            single["type"] = rest[0].clone();
            repair(value, &single, root, depth + 1)
        }
        None => {
            let any = schema.get("anyOf").and_then(Value::as_array);
            let one = schema.get("oneOf").and_then(Value::as_array);
            if any.is_some() && one.is_some() {
                return None;
            }
            let branches = any.or(one)?;
            if value.is_null() {
                return None;
            }
            if branches.iter().any(|b| !is_object(b) || b.get("type").and_then(Value::as_str) == Some(type_of(value))) {
                return None;
            }
            let candidates: Vec<&Value> =
                branches.iter().filter(|b| is_object(b) && b.get("type").and_then(Value::as_str) != Some("null")).collect();
            if candidates.len() != 1 {
                return None;
            }
            repair(value, candidates[0], root, depth + 1)
        }
        Some(kind) => match kind.as_str() {
            Some(kind @ ("number" | "integer")) => {
                let Value::String(text) = value else { return None };
                if text.trim().is_empty() {
                    return None;
                }
                let parsed = js_number(text)?;
                let integer = parsed.fract() == 0.0 && parsed.abs() <= MAX_SAFE_INTEGER;
                (parsed.is_finite() && (kind != "integer" || integer)).then(|| number(parsed))
            }
            Some("boolean") => match value.as_str() {
                Some("true") => Some(Value::Bool(true)),
                Some("false") => Some(Value::Bool(false)),
                _ => None,
            },
            Some("object") => repair_object(value, schema, root, depth),
            Some("array") => repair_array(value, schema, root, depth),
            _ => None,
        },
    }
}

fn is_pointer(reference: &str, prefix: &str) -> bool {
    reference.strip_prefix(prefix).is_some_and(|name| !name.is_empty() && !name.contains('/'))
}

fn repair_object(value: &Value, schema: &Value, root: &Value, depth: usize) -> Option<Value> {
    let parsed = match value {
        Value::String(text) => decode_json(text)?,
        other => other.clone(),
    };
    let Value::Object(fields) = &parsed else { return None };
    let empty = Map::new();
    let properties = schema.get("properties").and_then(Value::as_object).unwrap_or(&empty);
    let required: Vec<&str> = schema.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    let patterned = schema.get("patternProperties").is_some_and(is_object);
    let composed = ["allOf", "anyOf", "oneOf"].iter().any(|k| schema.get(*k).is_some_and(Value::is_array));
    let additional = schema.get("additionalProperties");

    let mut result = fields.clone();
    for (key, current) in fields {
        let declared = properties.contains_key(key);
        let property = if declared { properties.get(key) } else if !patterned { additional } else { None };
        if !declared && additional == Some(&Value::Bool(false)) && !patterned && !composed {
            result.remove(key);
            continue;
        }
        let Some(property) = property.filter(|p| is_object(p)) else { continue };
        // Only a bare single-type property provably rejects null and `{}`. Compositions, enums, constants,
        // references and nullable flags may accept them, so those are left alone.
        let kind = property.get("type").and_then(Value::as_str);
        let plain = kind.is_some_and(|k| k != "null")
            && !composed
            && ["anyOf", "oneOf", "allOf", "enum", "const", "$ref", "nullable"].iter().all(|k| property.get(*k).is_none());
        let placeholder = match current {
            Value::Object(map) => map.is_empty(),
            Value::Array(items) => items.is_empty(),
            _ => false,
        } && kind != Some("object");
        if declared && !required.contains(&key.as_str()) && plain && (current.is_null() || placeholder) {
            result.remove(key);
            continue;
        }
        if let Some(repaired) = repair(current, property, root, depth + 1) {
            result.insert(key.clone(), repaired);
        }
    }
    let changed = Value::Object(result);
    (changed != *value).then_some(changed)
}

fn repair_array(value: &Value, schema: &Value, root: &Value, depth: usize) -> Option<Value> {
    let parsed = match value {
        Value::String(text) => decode_json(text),
        other => Some(other.clone()),
    };
    let prefix = schema.get("prefixItems").and_then(Value::as_array);
    let tuple = prefix.or_else(|| schema.get("items").and_then(Value::as_array));
    let items = schema.get("items");

    if let Some(Value::Array(list)) = &parsed {
        let repaired: Vec<Value> = list
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let member = match tuple {
                    Some(tuple) => tuple.get(index).or(if prefix.is_some() { items } else { schema.get("additionalItems") }),
                    None => items,
                };
                member.filter(|m| is_object(m)).and_then(|m| repair(item, m, root, depth + 1)).unwrap_or_else(|| item.clone())
            })
            .collect();
        let changed = Value::Array(repaired);
        return (changed != *value).then_some(changed);
    }

    let items = items.filter(|i| is_object(i))?;
    if tuple.is_some() {
        return None;
    }
    let repaired = repair(value, items, root, depth + 1).unwrap_or_else(|| value.clone());
    let compatible = match items.get("type").and_then(Value::as_str) {
        Some("object") => is_object(&repaired),
        Some("array") => repaired.is_array(),
        Some("integer") => repaired.as_f64().is_some_and(|n| n.fract() == 0.0 && n.abs() <= MAX_SAFE_INTEGER),
        Some("number") => repaired.as_f64().is_some_and(f64::is_finite),
        Some("string") => repaired.is_string(),
        Some("boolean") => repaired.is_boolean(),
        _ => false,
    };
    compatible.then(|| Value::Array(vec![repaired]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema(properties: Value, required: &[&str]) -> Value {
        json!({"type": "object", "properties": properties, "required": required})
    }

    #[test]
    fn repairs_the_documented_cases() {
        let limits = schema(json!({"limit": {"type": "integer"}, "enabled": {"type": "boolean"}}), &[]);
        assert_eq!(repair_input(json!("{\"limit\":\"20\"}"), &limits), json!({"limit": 20}));
        assert_eq!(repair_input(json!({"limit": "20", "enabled": "false"}), &limits), json!({"limit": 20, "enabled": false}));
        assert_eq!(repair_input(json!({"limit": null}), &limits), json!({}));
        assert_eq!(repair_input(json!({"limit": {}}), &limits), json!({}));

        let mut closed = limits.clone();
        closed["additionalProperties"] = json!(false);
        assert_eq!(repair_input(json!({"limit": "20", "extra": true}), &closed), json!({"limit": 20}));
        // An open object keeps unknown keys.
        assert_eq!(repair_input(json!({"limit": "20", "extra": true}), &limits), json!({"limit": 20, "extra": true}));

        let nullable = schema(json!({"count": {"type": ["integer", "null"]}}), &["count"]);
        assert_eq!(repair_input(json!({"count": "2"}), &nullable), json!({"count": 2}));
        assert_eq!(repair_input(json!({"count": null}), &nullable), json!({"count": null}));

        let arrays = schema(json!({"tags": {"type": "array", "items": {"type": "string"}}, "count": {"type": "array", "items": {"type": "number"}}}), &[]);
        assert_eq!(repair_input(json!({"tags": "[\"a\"]", "count": "2"}), &arrays), json!({"tags": ["a"], "count": [2]}));

        let tuple = schema(json!({"pair": {"type": "array", "prefixItems": [{"type": "integer"}, {"type": "boolean"}]}}), &[]);
        assert_eq!(repair_input(json!({"pair": ["2", "false"]}), &tuple), json!({"pair": [2, false]}));

        let dictionary = schema(json!({"counts": {"type": "object", "additionalProperties": {"type": "integer"}}}), &[]);
        assert_eq!(repair_input(json!({"counts": {"first": "2"}}), &dictionary), json!({"counts": {"first": 2}}));

        let nested = json!({
            "type": "object",
            "$defs": {"Item": {"type": "object", "properties": {"count": {"type": "integer"}}}},
            "properties": {"items": {"type": "array", "items": {"$ref": "#/$defs/Item"}}}
        });
        assert_eq!(repair_input(json!({"items": [{"count": "2"}]}), &nested), json!({"items": [{"count": 2}]}));
    }

    #[test]
    fn leaves_ambiguous_input_alone() {
        let required = schema(json!({"limit": {"type": "integer"}}), &["limit"]);
        // A required field keeps its null; the tool reports it.
        assert_eq!(repair_input(json!({"limit": null}), &required), json!({"limit": null}));
        // Not a number, a fraction for an integer, and blank text stay as sent.
        assert_eq!(repair_input(json!({"limit": "ten"}), &required), json!({"limit": "ten"}));
        assert_eq!(repair_input(json!({"limit": "1.5"}), &required), json!({"limit": "1.5"}));
        assert_eq!(repair_input(json!({"limit": " "}), &required), json!({"limit": " "}));
        let number = schema(json!({"x": {"type": "number"}}), &[]);
        assert_eq!(repair_input(json!({"x": "1.5"}), &number), json!({"x": 1.5}));
        assert_eq!(repair_input(json!({"x": "Infinity"}), &number), json!({"x": "Infinity"}));
        // Enums and compositions may accept null.
        let enumerated = schema(json!({"mode": {"type": "string", "enum": ["a"]}}), &[]);
        assert_eq!(repair_input(json!({"mode": null}), &enumerated), json!({"mode": null}));
        // A union with several typed branches is not guessed.
        let union = schema(json!({"v": {"anyOf": [{"type": "integer"}, {"type": "boolean"}]}}), &[]);
        assert_eq!(repair_input(json!({"v": "2"}), &union), json!({"v": "2"}));
        let single = schema(json!({"v": {"anyOf": [{"type": "integer"}, {"type": "null"}]}}), &[]);
        assert_eq!(repair_input(json!({"v": "2"}), &single), json!({"v": 2}));
        // Unparsable text for an object schema stays a string.
        assert_eq!(repair_input(json!("not json"), &required), json!("not json"));
        // Non-object schemas are not touched.
        assert_eq!(repair_input(json!("5"), &json!({"type": "string"})), json!("5"));
    }
}
