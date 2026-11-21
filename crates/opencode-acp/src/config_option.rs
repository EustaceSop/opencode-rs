// Session config options `model`, `effort`, `mode` (`acp/config-option.ts`).
use crate::directory::{Mode, ModelRef, ProviderInfo, Snapshot, locale_cmp};
use serde_json::{Map, Value, json};

pub const DEFAULT_VARIANT: &str = "default";

pub fn format_variant_name(variant: &str) -> String {
    variant
        .split(['_', '-'])
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn select_variant(variant: Option<&str>, variants: &[String]) -> Option<String> {
    if let Some(variant) = variant.filter(|v| !v.is_empty() && variants.iter().any(|x| x == v)) {
        return Some(variant.to_string());
    }
    if variants.iter().any(|v| v == DEFAULT_VARIANT) {
        return Some(DEFAULT_VARIANT.into());
    }
    variants.first().cloned()
}

fn model_option(providers: &[ProviderInfo], current: &ModelRef) -> Value {
    let options: Vec<Value> = providers
        .iter()
        .flat_map(|provider| {
            let mut models: Vec<_> = provider.models.iter().collect();
            models.sort_by(|a, b| locale_cmp(&a.name, &b.name));
            models.into_iter().map(move |model| json!({"value": format!("{}/{}", provider.id, model.id), "name": format!("{}/{}", provider.name, model.name)}))
        })
        .collect();
    json!({"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": current.key(), "options": options})
}

fn effort_option(variants: &[String], current: Option<&str>) -> Option<Value> {
    if variants.is_empty() {
        return None;
    }
    let current = if current == Some(DEFAULT_VARIANT) { Some(DEFAULT_VARIANT.to_string()) } else { select_variant(current, variants) };
    let mut values: Vec<&str> = Vec::new();
    for value in variants.iter().map(String::as_str).chain([DEFAULT_VARIANT]) {
        if !values.contains(&value) {
            values.push(value);
        }
    }
    let options: Vec<Value> = values.into_iter().map(|v| json!({"value": v, "name": format_variant_name(v)})).collect();
    Some(json!({
        "id": "effort",
        "name": "Effort",
        "description": "Available effort levels for this model",
        "category": "thought_level",
        "type": "select",
        "currentValue": current,
        "options": options,
    }))
}

fn mode_option(modes: &[Mode], current: &str) -> Value {
    let options: Vec<Value> = modes
        .iter()
        .map(|mode| {
            let mut option = Map::new();
            option.insert("value".into(), mode.id.clone().into());
            option.insert("name".into(), mode.name.clone().into());
            if let Some(description) = &mode.description {
                option.insert("description".into(), description.clone().into());
            }
            Value::Object(option)
        })
        .collect();
    json!({"id": "mode", "name": "Session Mode", "category": "mode", "type": "select", "currentValue": current, "options": options})
}

pub fn build(snapshot: &Snapshot, model: &ModelRef, variant: Option<&str>, mode_id: Option<&str>) -> Value {
    let variants = snapshot.variants(model).unwrap_or_default();
    let mut options = vec![model_option(&snapshot.providers, model)];
    options.extend(effort_option(variants, variant));
    if let Some(mode) = mode_id.filter(|m| !m.is_empty()) {
        options.push(mode_option(&snapshot.modes, mode));
    }
    Value::Array(options)
}

// `parseModelSelection`: "provider/model" or "provider/model/variant".
pub fn parse_model_selection(model_id: &str, providers: &[ProviderInfo]) -> (ModelRef, Option<String>) {
    if let Some(provider) = providers.iter().find(|p| model_id.starts_with(&format!("{}/", p.id))) {
        let rest = &model_id[provider.id.len() + 1..];
        if provider.models.iter().any(|m| m.id == rest) {
            return (ModelRef::new(&provider.id, rest), None);
        }
        if let Some((base, variant)) = rest.rsplit_once('/')
            && provider.models.iter().any(|m| m.id == base && m.variants.iter().any(|v| v == variant))
        {
            return (ModelRef::new(&provider.id, base), Some(variant.to_string()));
        }
        return (ModelRef::new(&provider.id, rest), None);
    }
    match model_id.split_once('/') {
        Some((provider, model)) => (ModelRef::new(provider, model), None),
        None => (ModelRef::new(model_id, ""), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::ModelInfo;

    #[test]
    fn variant_names_and_model_selection() {
        assert_eq!(format_variant_name("high"), "High");
        assert_eq!(format_variant_name("extra_high-x"), "Extra High X");
        let providers = vec![ProviderInfo {
            id: "p".into(),
            name: "P".into(),
            models: vec![ModelInfo { id: "m".into(), name: "M".into(), variants: vec!["low".into(), "high".into()] }],
        }];
        assert_eq!(parse_model_selection("p/m", &providers), (ModelRef::new("p", "m"), None));
        assert_eq!(parse_model_selection("p/m/high", &providers), (ModelRef::new("p", "m"), Some("high".into())));
        assert_eq!(parse_model_selection("p/m/nope", &providers), (ModelRef::new("p", "m/nope"), None));
        assert_eq!(parse_model_selection("q", &providers), (ModelRef::new("q", ""), None));
        let effort = effort_option(&providers[0].models[0].variants, None).unwrap();
        assert_eq!(effort["currentValue"], "low");
        assert_eq!(effort["options"], json!([{"value": "low", "name": "Low"}, {"value": "high", "name": "High"}, {"value": "default", "name": "Default"}]));
        assert_eq!(effort_option(&providers[0].models[0].variants, Some("default")).unwrap()["currentValue"], "default");
    }
}
