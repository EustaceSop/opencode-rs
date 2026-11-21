// Reasoning variants per model: name -> provider options merged over the defaults when selected.
use crate::provider::{Model, OUTPUT_TOKEN_MAX};
use crate::shape::{gemini_2_5, kimi_family, legacy_gemini};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::sync::LazyLock;

const WIDE: [&str; 3] = ["low", "medium", "high"];
const OPENAI: [&str; 6] = ["none", "minimal", "low", "medium", "high", "xhigh"];
const GPT5_1: [&str; 4] = ["none", "low", "medium", "high"];
const GPT5_2_PLUS: [&str; 5] = ["none", "low", "medium", "high", "xhigh"];
const CODEX_XHIGH: [&str; 4] = ["low", "medium", "high", "xhigh"];
const NONE_RELEASE: &str = "2025-11-13";
const XHIGH_RELEASE: &str = "2025-12-04";

static GPT5_FAMILY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:^|/)gpt-5(?:[.-]|$)").unwrap());
static GPT5_VERSION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:^|/)gpt-5[.-](\d+)(?:[.-]|$)").unwrap());
static GPT5_PRO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:^|/)gpt-5[.-]?pro(?:[.-]|$)").unwrap());
static GPT5_VERSIONED_PRO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:^|/)gpt-5[.-]\d+[.-]pro(?:[.-]|$)").unwrap());
static CLAUDE_VERSION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)claude-(?:[a-z]+-)?(\d+)(?:[.-](\d{1,2}))?(?:[.@-]|$)").unwrap());
static CLAUDE_BINDING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)claude-(?:([a-z]+)-)?(\d+)(?:[.-](\d{1,2}))?(?:-([a-z]+))?(?:[.@-]|$)").unwrap());
static O_SERIES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bo[1-9]").unwrap());

fn list(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn gpt5_version(id: &str) -> Option<u64> {
    GPT5_VERSION.captures(id).and_then(|c| c[1].parse::<u64>().ok()).filter(|v| *v != 0)
}

fn versioned_gpt5(id: &str) -> Option<Vec<String>> {
    if GPT5_VERSIONED_PRO.is_match(id) {
        return Some(list(&["medium", "high", "xhigh"]));
    }
    Some(if gpt5_version(id)? == 1 { list(&GPT5_1) } else { list(&GPT5_2_PLUS) })
}

fn gpt5_codex(id: &str) -> Option<Vec<String>> {
    if !GPT5_FAMILY.is_match(id) || !id.contains("codex") {
        return None;
    }
    let version = gpt5_version(id);
    Some(if version.is_some_and(|v| v >= 3) {
        list(&GPT5_2_PLUS)
    } else if id.contains("codex-max") || version.is_some_and(|v| v >= 2) {
        list(&CODEX_XHIGH)
    } else {
        list(&WIDE)
    })
}

fn gpt5_chat(id: &str) -> Option<Vec<String>> {
    if !GPT5_FAMILY.is_match(id) || !id.contains("-chat") {
        return None;
    }
    Some(if gpt5_version(id).is_none() { Vec::new() } else { list(&["medium"]) })
}

pub fn openai_efforts(api_id: &str, release_date: &str) -> Vec<String> {
    let id = api_id.to_lowercase();
    if id.contains("deep-research") {
        return list(&["medium"]);
    }
    if let Some(found) = gpt5_chat(&id) {
        return found;
    }
    if GPT5_PRO.is_match(&id) {
        return list(&["high"]);
    }
    if let Some(found) = gpt5_codex(&id).or_else(|| versioned_gpt5(&id)) {
        return found;
    }
    let mut efforts = list(&WIDE);
    if GPT5_FAMILY.is_match(&id) {
        efforts.insert(0, "minimal".into());
    }
    if release_date >= NONE_RELEASE {
        efforts.insert(0, "none".into());
    }
    if release_date >= XHIGH_RELEASE {
        efforts.push("xhigh".into());
    }
    efforts
}

fn openai_compatible_efforts(api_id: &str) -> Vec<String> {
    let id = api_id.to_lowercase();
    if let Some(found) = gpt5_chat(&id) {
        return found;
    }
    if GPT5_PRO.is_match(&id) {
        return list(&["high"]);
    }
    gpt5_codex(&id).or_else(|| versioned_gpt5(&id)).unwrap_or_else(|| list(&OPENAI))
}

fn modern_adaptive(api_id: &str) -> bool {
    if !api_id.to_lowercase().contains("claude-") {
        return false;
    }
    let Some(version) = CLAUDE_VERSION.captures(api_id) else { return true };
    let major: u64 = version[1].parse().unwrap_or(0);
    let minor: u64 = version.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
    major > 4 || (major == 4 && minor >= 7)
}

fn opus_4_5(api_id: &str) -> bool {
    api_id.contains("opus-4-5") || api_id.contains("opus-4.5")
}

fn adaptive_efforts(api_id: &str) -> Option<Vec<String>> {
    if modern_adaptive(api_id) {
        return Some(list(&["low", "medium", "high", "xhigh", "max"]));
    }
    let v46 = ["opus-4-6", "opus-4.6", "4-6-opus", "4.6-opus", "sonnet-4-6", "sonnet-4.6", "4-6-sonnet", "4.6-sonnet"];
    v46.iter().any(|v| api_id.contains(v)).then(|| list(&["low", "medium", "high", "max"]))
}

// Claude 5.1+ (except Mythos 5.1) binds thinking signatures to the conversation prefix.
pub fn binds_thinking(api_id: &str) -> bool {
    let Some(version) = CLAUDE_BINDING.captures(api_id) else { return false };
    let major: u64 = version[2].parse().unwrap_or(0);
    let minor: u64 = version.get(3).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
    let family = version.get(1).or(version.get(4)).map(|m| m.as_str().to_lowercase());
    if major == 5 && minor == 1 && family.as_deref() == Some("mythos") {
        return false;
    }
    major > 5 || (major == 5 && minor >= 1)
}

fn adaptive_thinking(api_id: &str) -> Value {
    if modern_adaptive(api_id) { json!({"type": "adaptive", "display": "summarized"}) } else { json!({"type": "adaptive"}) }
}

fn budget_half(model: &Model) -> i64 {
    16_000.min((model.limit_output as f64 / 2.0 - 1.0).floor() as i64)
}

fn opus_4_5_effort(model: &Model, effort: &str) -> Value {
    json!({"thinking": {"type": "enabled", "budgetTokens": budget_half(model)}, "effort": effort})
}

fn google_level_efforts(api_id: &str) -> Vec<String> {
    let id = api_id.to_lowercase();
    if id.contains("gemma") {
        list(&["minimal", "high"])
    } else if legacy_gemini(&id) {
        list(&["low", "high"])
    } else if id.contains("flash-image") {
        list(&["minimal", "high"])
    } else if id.contains("pro-image") {
        list(&["high"])
    } else if id.contains("flash") {
        list(&["minimal", "low", "medium", "high"])
    } else {
        list(&WIDE)
    }
}

fn google_budget_max(id: &str) -> u64 {
    if gemini_2_5(id) && id.contains("pro") && !id.contains("flash") { 32_768 } else { 24_576 }
}

fn each(efforts: &[String], build: impl Fn(&str) -> Value) -> Map<String, Value> {
    efforts.iter().map(|e| (e.clone(), build(e))).collect()
}

fn google_variants(model: &Model) -> Map<String, Value> {
    let id = model.api_id.to_lowercase();
    if gemini_2_5(&id) {
        let budget = |n: u64| json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": n}});
        return [("high".to_string(), budget(16000)), ("max".to_string(), budget(google_budget_max(&id)))].into_iter().collect();
    }
    each(&google_level_efforts(&id), |e| json!({"thinkingConfig": {"includeThoughts": true, "thinkingLevel": e}}))
}

fn object(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
}

fn sap(variants: Map<String, Value>) -> Map<String, Value> {
    variants.into_iter().map(|(k, v)| (k, json!({"modelParams": v}))).collect()
}

fn openai_shape(effort: &str) -> Value {
    json!({"reasoningEffort": effort, "reasoningSummary": "auto", "include": ["reasoning.encrypted_content"]})
}

// Rust build: v2.2.7's `Variant.resolve` for a model without reasoning information. Only the OpenAI-compatible chat
// protocol is ported (`openaiChat`: low, medium, high as `reasoningEffort`); other packages keep v1's "none".
pub fn default_efforts(model: &Model) -> Map<String, Value> {
    match model.npm.as_str() {
        "@ai-sdk/openai-compatible" if model.provider_id != "nvidia" => each(&list(&["low", "medium", "high"]), |e| json!({"reasoningEffort": e})),
        _ => Map::new(),
    }
}

// Heuristic variants for a reasoning model, keyed by its SDK package.
pub fn variants(model: &Model) -> Map<String, Value> {
    if !model.reasoning {
        return Map::new();
    }
    let id = model.id.to_lowercase();
    let api = model.api_id.as_str();
    let api_lower = api.to_lowercase();
    let npm = model.npm.as_str();
    let effort = |e: &str| json!({"reasoningEffort": e});
    let nested = |e: &str| json!({"reasoning": {"effort": e}});
    let glm52 = ["glm-5.2", "glm-5-2", "glm-5p2"].iter().any(|n| id.contains(n) || api_lower.contains(n));
    if api_lower.contains("minimax-m3") && ["@ai-sdk/anthropic", "@ai-sdk/openai-compatible"].contains(&npm) {
        if ["nvidia", "lilac"].contains(&model.provider_id.as_str()) {
            let mode = |m: &str| json!({"chat_template_kwargs": {"thinking_mode": m}});
            return object(&[("none", mode("disabled")), ("thinking", mode("enabled"))]);
        }
        return object(&[("none", json!({"thinking": {"type": "disabled"}})), ("thinking", json!({"thinking": {"type": "adaptive"}}))]);
    }
    let adaptive = adaptive_efforts(api);
    if glm52 {
        match npm {
            "@openrouter/ai-sdk-provider" => return object(&[("high", nested("high")), ("xhigh", nested("xhigh"))]),
            "@ai-sdk/openai-compatible" => return object(&[("high", effort("high")), ("max", effort("max"))]),
            "@ai-sdk/anthropic" => return object(&[("high", json!({"effort": "high"})), ("max", json!({"effort": "max"}))]),
            _ => {}
        }
    }
    if kimi_family(model) && ["@ai-sdk/anthropic", "@ai-sdk/google-vertex/anthropic"].contains(&npm) {
        return each(&list(&["low", "medium", "high", "xhigh", "max"]), |e| json!({"thinking": {"type": "adaptive", "display": "summarized"}, "effort": e}));
    }
    let plain = ["deepseek-chat", "deepseek-reasoner", "deepseek-r1", "deepseek-v3", "minimax", "kimi", "k2p", "qwen", "big-pickle"];
    if plain.iter().any(|n| id.contains(n)) || (id.contains("glm") && !glm52) {
        return Map::new();
    }
    if id.contains("grok") && id.contains("grok-3-mini") {
        return if npm == "@openrouter/ai-sdk-provider" {
            object(&[("low", nested("low")), ("high", nested("high"))])
        } else {
            object(&[("low", effort("low")), ("high", effort("high"))])
        };
    }
    match npm {
        "@openrouter/ai-sdk-provider" => {
            let efforts = if api.starts_with("openai/") || id.contains("gpt") { openai_compatible_efforts(api) } else { list(&WIDE) };
            each(&efforts, nested)
        }
        "ai-gateway-provider" => {
            let efforts = if api.starts_with("openai/") { openai_efforts(api, &model.release_date) } else { list(&WIDE) };
            each(&efforts, effort)
        }
        "@ai-sdk/gateway" => {
            if api.contains("anthropic") {
                return match &adaptive {
                    Some(efforts) => each(efforts, |e| json!({"thinking": adaptive_thinking(api), "effort": e})),
                    None => object(&[
                        ("high", json!({"thinking": {"type": "enabled", "budgetTokens": 16000}})),
                        ("max", json!({"thinking": {"type": "enabled", "budgetTokens": 31999}})),
                    ]),
                };
            }
            if api.contains("google") {
                if gemini_2_5(api) {
                    let budget = |n: u64| json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": n}});
                    return object(&[("high", budget(16000)), ("max", budget(google_budget_max(&api_lower)))]);
                }
                return each(&list(&["low", "high"]), |e| json!({"includeThoughts": true, "thinkingLevel": e}));
            }
            each(&openai_compatible_efforts(api), effort)
        }
        "@ai-sdk/github-copilot" => {
            if model.id.contains("gemini") {
                return Map::new();
            }
            if model.id.contains("claude") {
                return each(&list(&WIDE), effort);
            }
            let mut efforts = list(&WIDE);
            if ["5.1-codex-max", "5.2", "5.3"].iter().any(|n| id.contains(n)) || (id.contains("gpt-5") && model.release_date.as_str() >= XHIGH_RELEASE) {
                efforts.push("xhigh".into());
            }
            each(&efforts, openai_shape)
        }
        "@ai-sdk/cerebras" | "@ai-sdk/togetherai" | "@ai-sdk/xai" | "@ai-sdk/deepinfra" | "venice-ai-sdk-provider" | "@ai-sdk/openai-compatible" => {
            if api_lower.contains("north-mini-code") {
                return each(&list(&["none", "high"]), effort);
            }
            let mut efforts = list(&WIDE);
            if api_lower.contains("deepseek-v4") {
                efforts.push("max".into());
            }
            each(&efforts, effort)
        }
        "@ai-sdk/azure" => {
            if id == "o1-mini" {
                return Map::new();
            }
            each(&openai_efforts(&id, &model.release_date), openai_shape)
        }
        "@ai-sdk/amazon-bedrock/mantle" | "@ai-sdk/openai" => {
            let efforts = if model.provider_id == "meta" { list(&OPENAI) } else { openai_efforts(api, &model.release_date) };
            each(&efforts, openai_shape)
        }
        "@ai-sdk/anthropic" | "@ai-sdk/google-vertex/anthropic" => {
            if let Some(mut efforts) = adaptive {
                if model.provider_id == "github-copilot" {
                    if api.contains("opus-4.7") {
                        efforts = list(&["medium"]);
                    }
                    efforts.retain(|e| e != "max" && e != "xhigh");
                }
                return each(&efforts, |e| json!({"thinking": adaptive_thinking(api), "effort": e}));
            }
            if opus_4_5(api) {
                return each(&list(&WIDE), |e| opus_4_5_effort(model, e));
            }
            object(&[
                ("high", json!({"thinking": {"type": "enabled", "budgetTokens": budget_half(model)}})),
                ("max", json!({"thinking": {"type": "enabled", "budgetTokens": 31_999.min(model.limit_output as i64 - 1)}})),
            ])
        }
        "@ai-sdk/amazon-bedrock" => {
            if let Some(efforts) = adaptive {
                return each(&efforts, |e| {
                    let mut config = json!({"type": "adaptive", "maxReasoningEffort": e});
                    if modern_adaptive(api) {
                        config["display"] = "summarized".into();
                    }
                    json!({"reasoningConfig": config})
                });
            }
            if api.contains("anthropic") {
                let budget = |n: u64| json!({"reasoningConfig": {"type": "enabled", "budgetTokens": n}});
                return object(&[("high", budget(16000)), ("max", budget(31999))]);
            }
            each(&list(&WIDE), |e| json!({"reasoningConfig": {"type": "enabled", "maxReasoningEffort": e}}))
        }
        "@ai-sdk/google-vertex" | "@ai-sdk/google" => google_variants(model),
        "@ai-sdk/mistral" => {
            let ids = ["mistral-small-2603", "mistral-small-latest", "mistral-medium-3.5", "mistral-medium-2604"];
            if ids.iter().any(|n| api_lower.contains(n)) { object(&[("high", effort("high"))]) } else { Map::new() }
        }
        "@ai-sdk/groq" => each(&list(&["none", "low", "medium", "high"]), effort),
        "@jerome-benoit/sap-ai-provider-v2" => {
            let raw = |e: &str| json!({"reasoning_effort": e});
            if id.contains("anthropic") {
                return sap(match adaptive {
                    Some(efforts) => each(&efforts, |e| json!({"thinking": adaptive_thinking(api), "output_config": {"effort": e}})),
                    None => object(&[
                        ("high", json!({"thinking": {"type": "enabled", "budget_tokens": 16000}})),
                        ("max", json!({"thinking": {"type": "enabled", "budget_tokens": 31999}})),
                    ]),
                });
            }
            if gemini_2_5(&id) || gemini_2_5(api) {
                return sap(google_variants(model));
            }
            if id.contains("gpt") || O_SERIES.is_match(&id) {
                return sap(each(&openai_efforts(&id, &model.release_date), raw));
            }
            sap(each(&list(&WIDE), raw))
        }
        _ => Map::new(),
    }
}

fn anthropic_effort(model: &Model, effort: &str) -> Option<Value> {
    if opus_4_5(&model.api_id) {
        return Some(opus_4_5_effort(model, effort));
    }
    if kimi_family(model) {
        return Some(json!({"thinking": {"type": "adaptive", "display": "summarized"}, "effort": effort}));
    }
    adaptive_efforts(&model.api_id)?;
    Some(json!({"thinking": adaptive_thinking(&model.api_id), "effort": effort}))
}

fn reasoning_effort(model: &Model, effort: &str) -> Option<Value> {
    let api = model.api_id.as_str();
    Some(match model.npm.as_str() {
        "@openrouter/ai-sdk-provider" => json!({"reasoning": {"effort": effort}}),
        "@ai-sdk/anthropic" | "@ai-sdk/google-vertex/anthropic" => anthropic_effort(model, effort).unwrap_or_else(|| json!({"effort": effort})),
        "@ai-sdk/google" | "@ai-sdk/google-vertex" => json!({"thinkingConfig": {"includeThoughts": true, "thinkingLevel": effort}}),
        "@ai-sdk/amazon-bedrock" => {
            if adaptive_efforts(api).is_some() {
                let mut config = json!({"type": "adaptive", "maxReasoningEffort": effort});
                if modern_adaptive(api) {
                    config["display"] = "summarized".into();
                }
                json!({"reasoningConfig": config})
            } else if opus_4_5(api) {
                json!({"reasoningConfig": {"type": "enabled", "budgetTokens": budget_half(model), "maxReasoningEffort": effort}})
            } else if api.contains("anthropic") {
                return None;
            } else {
                json!({"reasoningConfig": {"type": "enabled", "maxReasoningEffort": effort}})
            }
        }
        "@ai-sdk/gateway" => {
            if model.id.contains("anthropic") {
                json!({"thinking": {"type": "adaptive", "display": "summarized"}, "effort": effort})
            } else if model.id.contains("google") {
                json!({"thinkingConfig": {"includeThoughts": true, "thinkingLevel": effort}})
            } else {
                json!({"reasoningEffort": effort})
            }
        }
        "@ai-sdk/github-copilot" => {
            if model.id.contains("gemini") {
                return None;
            }
            if model.id.contains("claude") { json!({"reasoningEffort": effort}) } else { openai_shape(effort) }
        }
        "@ai-sdk/openai" | "@ai-sdk/amazon-bedrock/mantle" | "@ai-sdk/azure" => openai_shape(effort),
        "@jerome-benoit/sap-ai-provider-v2" => {
            if model.id.contains("anthropic") {
                json!({"modelParams": {"thinking": {"type": "adaptive", "display": "summarized"}, "output_config": {"effort": effort}}})
            } else {
                json!({"modelParams": {"reasoning_effort": effort}})
            }
        }
        "@ai-sdk/openai-compatible" | "@ai-sdk/xai" | "@ai-sdk/mistral" | "@ai-sdk/groq" | "@ai-sdk/cerebras" | "@ai-sdk/deepinfra" | "@ai-sdk/togetherai"
        | "venice-ai-sdk-provider" | "ai-gateway-provider" | "merge-gateway-ai-sdk-provider" => json!({"reasoningEffort": effort}),
        "gitlab-ai-provider" => {
            if model.family.starts_with("gpt") {
                json!({"reasoningEffort": effort})
            } else if model.family.starts_with("claude") {
                json!({"thinking": {"type": "adaptive", "effort": effort}})
            } else {
                return None;
            }
        }
        _ => return None,
    })
}

fn reasoning_budget(model: &Model, budget: i64) -> Option<Value> {
    Some(match model.npm.as_str() {
        "@openrouter/ai-sdk-provider" => json!({"reasoning": {"max_tokens": budget}}),
        "@ai-sdk/anthropic" | "@ai-sdk/google-vertex/anthropic" => json!({"thinking": {"type": "enabled", "budgetTokens": budget}}),
        "@ai-sdk/google" | "@ai-sdk/google-vertex" => json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": budget}}),
        "@ai-sdk/amazon-bedrock" => json!({"reasoningConfig": {"type": "enabled", "budgetTokens": budget}}),
        "@ai-sdk/gateway" if model.id.contains("anthropic") => json!({"thinking": {"type": "enabled", "budgetTokens": budget}}),
        "@ai-sdk/gateway" if model.id.contains("google") => json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": budget}}),
        "@ai-sdk/cohere" => json!({"thinking": {"type": "enabled", "tokenBudget": budget}}),
        "@ai-sdk/alibaba" => json!({"enableThinking": true, "thinkingBudget": budget}),
        "@jerome-benoit/sap-ai-provider-v2" if model.id.contains("anthropic") => json!({"modelParams": {"thinking": {"type": "enabled", "budget_tokens": budget}}}),
        "@jerome-benoit/sap-ai-provider-v2" if model.id.contains("gemini") => {
            json!({"modelParams": {"thinkingConfig": {"includeThoughts": true, "thinkingBudget": budget}}})
        }
        _ => return None,
    })
}

fn toggle(model: &Model) -> Map<String, Value> {
    match model.npm.as_str() {
        "@ai-sdk/alibaba" => object(&[("none", json!({"enableThinking": false})), ("high", json!({"enableThinking": true}))]),
        "@ai-sdk/cohere" => object(&[("none", json!({"thinking": {"type": "disabled"}})), ("high", json!({"thinking": {"type": "enabled"}}))]),
        _ => Map::new(),
    }
}

// Variants from a catalog/config `reasoning_options` list; None means "fall back to the heuristic".
pub fn reasoning_variants(options: Option<&Value>, model: &Model) -> Option<Map<String, Value>> {
    let options = options?.as_array()?;
    if options.is_empty() {
        return Some(Map::new());
    }
    let kind = |name: &str| options.iter().find(|o| o.get("type").and_then(Value::as_str) == Some(name));
    if let Some(effort) = kind("effort") {
        let mut out = Map::new();
        for value in effort.get("values").and_then(Value::as_array).into_iter().flatten() {
            let id = match value {
                Value::Null => "none",
                Value::String(s) => s.as_str(),
                _ => continue,
            };
            if let Some(settings) = reasoning_effort(model, id) {
                out.insert(id.to_string(), settings);
            }
        }
        return Some(out);
    }
    let mut out = if kind("toggle").is_some() { toggle(model) } else { Map::new() };
    if let Some(budget) = kind("budget_tokens") {
        let cap = OUTPUT_TOKEN_MAX as i64 - 1;
        let number = |key: &str| budget.get(key).and_then(Value::as_i64);
        let maximum = number("max").unwrap_or(cap).min(model.limit_output as i64 - 1).min(cap);
        if maximum > 0 {
            let high = number("min").unwrap_or(0).max((maximum + 1) / 2).min(maximum);
            for (id, amount) in [("high", high), ("max", maximum)] {
                if let Some(settings) = reasoning_budget(model, amount) {
                    out.insert(id.to_string(), settings);
                }
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

// Config `variants` merged over computed ones; `disabled: true` removes a variant.
pub fn merge_config(base: &Map<String, Value>, config: Option<&Value>) -> Map<String, Value> {
    let mut merged = Value::Object(base.clone());
    if let Some(config) = config.filter(|c| c.is_object()) {
        opencode_core::config::merge_deep(&mut merged, config.clone());
    }
    let Value::Object(merged) = merged else { return Map::new() };
    merged
        .into_iter()
        .filter(|(_, v)| v.get("disabled").and_then(Value::as_bool) != Some(true))
        .map(|(k, mut v)| {
            if let Some(map) = v.as_object_mut() {
                map.remove("disabled");
            }
            (k, v)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider;
    use opencode_core::auth;

    fn model(provider_id: &str, api_id: &str, npm: &str, extra: Value) -> Model {
        let mut entry = json!({"id": api_id, "reasoning": true, "limit": {"context": 200000, "output": 64000}});
        opencode_core::config::merge_deep(&mut entry, extra);
        let catalog = json!({provider_id: {"id": provider_id, "env": [], "npm": npm, "api": "https://example.test/v1", "models": {api_id: entry}}});
        let auth = vec![(provider_id.to_string(), auth::Info::Api { key: "k".into(), metadata: None })];
        provider::build(&catalog, &json!({}), &auth).remove(0).models.remove(0)
    }

    fn names(model: &Model) -> Vec<String> {
        model.variants.keys().cloned().collect()
    }

    #[test]
    fn openai_effort_tiers() {
        let e = |id: &str, date: &str| openai_efforts(id, date).join(",");
        assert_eq!(e("gpt-5", "2025-08-07"), "minimal,low,medium,high");
        assert_eq!(e("gpt-5.1", ""), "none,low,medium,high");
        assert_eq!(e("gpt-5.5", ""), "none,low,medium,high,xhigh");
        assert_eq!(e("gpt-5.2-pro", ""), "medium,high,xhigh");
        assert_eq!(e("gpt-5-pro", ""), "high");
        assert_eq!(e("gpt-5-codex", ""), "low,medium,high");
        assert_eq!(e("gpt-5.1-codex-max", ""), "low,medium,high,xhigh");
        assert_eq!(e("gpt-5.3-codex", ""), "none,low,medium,high,xhigh");
        assert_eq!(e("gpt-5-chat-latest", ""), "");
        assert_eq!(e("gpt-5.2-chat-latest", ""), "medium");
        assert_eq!(e("o3-deep-research", ""), "medium");
        assert_eq!(e("o4-mini", "2025-12-10"), "none,low,medium,high,xhigh");
        assert_eq!(e("o3", "2025-04-16"), "low,medium,high");
    }

    #[test]
    fn variants_per_package() {
        let m = model("openai", "gpt-5.5", "@ai-sdk/openai", json!({}));
        assert_eq!(names(&m), ["none", "low", "medium", "high", "xhigh"]);
        assert_eq!(m.variants["high"], json!({"reasoningEffort": "high", "reasoningSummary": "auto", "include": ["reasoning.encrypted_content"]}));
        let m = model("anthropic", "claude-opus-4-7", "@ai-sdk/anthropic", json!({}));
        assert_eq!(names(&m), ["low", "medium", "high", "xhigh", "max"]);
        assert_eq!(m.variants["max"], json!({"thinking": {"type": "adaptive", "display": "summarized"}, "effort": "max"}));
        let m = model("anthropic", "claude-sonnet-4-6", "@ai-sdk/anthropic", json!({}));
        assert_eq!(m.variants["max"], json!({"thinking": {"type": "adaptive"}, "effort": "max"}));
        assert_eq!(names(&m), ["low", "medium", "high", "max"]);
        let m = model("anthropic", "claude-sonnet-4-5", "@ai-sdk/anthropic", json!({}));
        assert_eq!(Value::Object(m.variants.clone()), json!({"high": {"thinking": {"type": "enabled", "budgetTokens": 16000}}, "max": {"thinking": {"type": "enabled", "budgetTokens": 31999}}}));
        let m = model("anthropic", "claude-opus-4-5", "@ai-sdk/anthropic", json!({"limit": {"output": 8000}}));
        assert_eq!(m.variants["low"], json!({"thinking": {"type": "enabled", "budgetTokens": 3999}, "effort": "low"}));
        let m = model("deepseek", "deepseek-v4-pro", "@ai-sdk/openai-compatible", json!({}));
        assert_eq!(names(&m), ["low", "medium", "high", "max"]);
        assert!(model("deepseek", "deepseek-reasoner", "@ai-sdk/openai-compatible", json!({})).variants.is_empty());
        assert!(model("x", "plain", "@ai-sdk/openai-compatible", json!({"reasoning": false})).variants.is_empty());
        let m = model("google", "gemini-2.5-pro", "@ai-sdk/google", json!({}));
        assert_eq!(m.variants["max"], json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": 32768}}));
        let m = model("google", "gemini-3-flash-preview", "@ai-sdk/google", json!({}));
        assert_eq!(names(&m), ["minimal", "low", "medium", "high"]);
        let m = model("openrouter", "openai/gpt-5.2", "@openrouter/ai-sdk-provider", json!({}));
        assert_eq!(m.variants["xhigh"], json!({"reasoning": {"effort": "xhigh"}}));
        let m = model("github-copilot", "claude-opus-4.7", "@ai-sdk/anthropic", json!({}));
        assert_eq!(names(&m), ["medium"]);
        let m = model("zai", "glm-5.2", "@ai-sdk/openai-compatible", json!({}));
        assert_eq!(Value::Object(m.variants.clone()), json!({"high": {"reasoningEffort": "high"}, "max": {"reasoningEffort": "max"}}));
        assert!(binds_thinking("claude-fable-5-1") && binds_thinking("claude-6-opus") && !binds_thinking("claude-mythos-5-1") && !binds_thinking("claude-opus-4-7"));
    }

    #[test]
    fn reasoning_options_and_config_merge() {
        let m = model("groq", "m", "@ai-sdk/groq", json!({"reasoning_options": [{"type": "effort", "values": [null, "low", 3]}]}));
        assert_eq!(Value::Object(m.variants.clone()), json!({"none": {"reasoningEffort": "none"}, "low": {"reasoningEffort": "low"}}));
        let m = model("anthropic", "claude-x", "@ai-sdk/anthropic", json!({"reasoning_options": [{"type": "budget_tokens", "min": 1024, "max": 20000}]}));
        assert_eq!(Value::Object(m.variants.clone()), json!({"high": {"thinking": {"type": "enabled", "budgetTokens": 10000}}, "max": {"thinking": {"type": "enabled", "budgetTokens": 20000}}}));
        assert!(model("p", "m", "@ai-sdk/openai", json!({"reasoning_options": []})).variants.is_empty());
        // A toggle with no mapping for the package falls back to the heuristic.
        assert_eq!(names(&model("groq", "m", "@ai-sdk/groq", json!({"reasoning_options": [{"type": "toggle"}]}))), ["none", "low", "medium", "high"]);

        let catalog = json!({"openai": {"id": "openai", "env": [], "npm": "@ai-sdk/openai", "models": {"gpt-5.1": {"id": "gpt-5.1", "reasoning": true}}}});
        let config = json!({"provider": {"openai": {"models": {"gpt-5.1": {"variants": {"high": {"disabled": true}, "low": {"textVerbosity": "high"}, "turbo": {"reasoningEffort": "high", "disabled": false}}}}}}});
        let auth = vec![("openai".to_string(), auth::Info::Api { key: "k".into(), metadata: None })];
        let m = provider::build(&catalog, &config, &auth).remove(0).models.remove(0);
        assert_eq!(names(&m), ["none", "low", "medium", "turbo"]);
        assert_eq!(m.variants["low"]["textVerbosity"], json!("high"));
        assert_eq!(m.variants["turbo"], json!({"reasoningEffort": "high"}));
    }
}
