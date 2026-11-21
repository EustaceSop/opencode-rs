// Request shaping: sampling defaults, output limits, per-request headers, default provider options.
use crate::provider::{Model, OUTPUT_TOKEN_MAX, Provider};
use crate::{Request, codex, copilot, variants};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::env;
use std::sync::LazyLock;

static GEMINI_SAMPLING: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [r"(?i)gemini-2[.-]5(?:[.-]|$)", r"gemini-3-(?:flash|pro)(?:[.-]|$)", r"gemini-3[.-]1(?:[.-]|$)"].iter().map(|p| Regex::new(p).unwrap()).collect()
});
static GEMINI_3_5_FLASH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"gemini-3[.-]5-flash").unwrap());
static GEMINI_LEGACY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)gemini-(?:(?:flash|pro)-)?[12](?:[.-]|$)").unwrap());
static GPT_VERSION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"gpt-(\d+)\.(\d+)").unwrap());
static GEMINI_2_5: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)gemini-2[.-]5(?:[.-]|$)").unwrap());

pub(crate) fn gemini_2_5(id: &str) -> bool {
    GEMINI_2_5.is_match(id)
}

pub(crate) fn legacy_gemini(id: &str) -> bool {
    GEMINI_LEGACY.is_match(id)
}

fn gemini_sampling_defaults(id: &str) -> bool {
    // gemini-3.5-flash but not its -lite variant.
    let flash = GEMINI_3_5_FLASH.find_iter(id).any(|m| {
        let rest = &id[m.end()..];
        !rest.starts_with("-lite") && (rest.is_empty() || rest.starts_with(['.', '-']))
    });
    flash || GEMINI_SAMPLING.iter().any(|p| p.is_match(id))
}

pub fn temperature(model: &Model) -> Option<f64> {
    let id = model.api_id.to_lowercase();
    if id.contains("north-mini-code") {
        return Some(1.0);
    }
    if id.contains("claude") {
        return None;
    }
    if id.contains("gemini") {
        return gemini_sampling_defaults(&id).then_some(1.0);
    }
    if id.contains("glm-4.6") || id.contains("glm-4.7") || id.contains("minimax-m2") {
        return Some(1.0);
    }
    if id.contains("kimi-k2") {
        return Some(if ["thinking", "k2.", "k2p", "k2-5"].iter().any(|s| id.contains(s)) { 1.0 } else { 0.6 });
    }
    None
}

pub fn top_p(model: &Model) -> Option<f64> {
    let id = model.api_id.to_lowercase();
    if id.contains("gemini") {
        return gemini_sampling_defaults(&id).then_some(0.95);
    }
    if ["minimax-m2", "kimi-k2.5", "kimi-k2p5", "kimi-k2-5"].iter().any(|s| id.contains(s)) {
        return Some(0.95);
    }
    let dated = ["deepseek-v4-flash-0731", "deepseek-v4-flash:0731"].iter().any(|s| id.contains(s));
    let hosted = id.contains("deepseek-v4-flash") && (model.provider_id == "deepseek" || model.provider_id.starts_with("opencode"));
    (dated || hosted).then_some(0.95)
}

pub fn top_k(model: &Model) -> Option<u64> {
    let id = model.api_id.to_lowercase();
    if id.contains("minimax-m2") {
        return Some(if ["m2.", "m25", "m21"].iter().any(|s| id.contains(s)) { 40 } else { 20 });
    }
    if id.contains("gemini") {
        return gemini_sampling_defaults(&id).then_some(64);
    }
    None
}

// OPENCODE_EXPERIMENTAL_OUTPUT_TOKEN_MAX (positive integer) overrides the built-in ceiling.
pub fn output_token_max() -> u64 {
    env::var("OPENCODE_EXPERIMENTAL_OUTPUT_TOKEN_MAX").ok().and_then(|v| v.trim().parse::<u64>().ok()).filter(|v| *v > 0).unwrap_or(OUTPUT_TOKEN_MAX)
}

pub fn max_output_tokens(model: &Model, ceiling: u64) -> u64 {
    match model.limit_output.min(ceiling) {
        0 => ceiling,
        n => n,
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Sampling {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u64>,
    pub max_tokens: Option<u64>,
}

// Model defaults; an agent's own temperature/top_p take precedence where set.
pub fn sampling(model: &Model, agent_temperature: Option<f64>, agent_top_p: Option<f64>) -> Sampling {
    Sampling {
        temperature: if model.temperature { agent_temperature.or_else(|| temperature(model)) } else { None },
        top_p: agent_top_p.or_else(|| top_p(model)),
        top_k: top_k(model),
        // The ChatGPT/OpenAI backends and Copilot's GPT models are called without an output cap.
        max_tokens: (model.provider_id != codex::PROVIDER_ID && !(copilot::is_provider(&model.provider_id) && model.api_id.contains("gpt")))
            .then(|| max_output_tokens(model, output_token_max())),
    }
}

#[derive(Debug, Clone, Default)]
pub struct Session<'a> {
    pub session_id: &'a str,
    pub parent_session_id: Option<&'a str>,
    // Id of the user message being answered.
    pub request_id: Option<&'a str>,
    pub project_id: Option<&'a str>,
}

pub fn user_agent() -> String {
    format!("opencode/{}", env!("CARGO_PKG_VERSION"))
}

// Headers attached to every LLM request, lowest precedence first.
pub fn headers(model: &Model, session: &Session) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut set = |name: &str, value: &str| {
        out.retain(|(k, _)| k != name);
        out.push((name.to_string(), value.to_string()));
    };
    set("x-opencode-session-id", session.session_id);
    if let Some(parent) = session.parent_session_id {
        set("x-opencode-parent-session-id", parent);
    }
    if model.provider_id.starts_with("opencode") {
        if let Some(project) = session.project_id {
            set("x-opencode-project", project);
        }
        set("x-opencode-session", session.session_id);
        if let Some(request) = session.request_id {
            set("x-opencode-request", request);
        }
        set("x-opencode-client", &env::var("OPENCODE_CLIENT").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "cli".into()));
    } else {
        set("x-session-affinity", session.session_id);
        set("X-Session-Id", session.session_id);
    }
    set("User-Agent", &user_agent());
    if let Some(parent) = session.parent_session_id {
        set("x-parent-session-id", parent);
    }
    for (name, value) in &model.headers {
        if let Some(value) = value.as_str() {
            set(name, value);
        }
    }
    if model.provider_id == codex::PROVIDER_ID {
        for (name, value) in codex::headers(Some(session.session_id)) {
            set(&name, &value);
        }
    }
    if copilot::is_provider(&model.provider_id) {
        set("X-GitHub-Api-Version", copilot::API_VERSION);
        set("X-Interaction-Id", session.session_id);
        if model.npm == "@ai-sdk/anthropic" {
            set("anthropic-beta", "interleaved-thinking-2025-05-14");
        }
        if session.parent_session_id.is_some() {
            set("x-initiator", "agent");
        }
    }
    out
}

pub(crate) fn kimi_family(model: &Model) -> bool {
    let named = [&model.provider_id, &model.api_id].iter().any(|id| {
        let id = id.to_lowercase();
        id.contains("kimi") || id.contains("moonshot")
    });
    let url = model.api_url.to_lowercase();
    named || ["api.kimi.com", "api.moonshot.ai", "api.moonshot.cn", "api.moonshotai.cn"].iter().any(|host| url.contains(host))
}

// Default provider options for a model, before model/agent/variant overrides are merged on top.
pub fn defaults(provider: &Provider, model: &Model, session_id: Option<&str>) -> Value {
    let npm = model.npm.as_str();
    let api_id = model.api_id.as_str();
    let lower = api_id.to_lowercase();
    let provider_id = model.provider_id.as_str();
    let encrypted = json!(["reasoning.encrypted_content"]);
    let mut out = Map::new();
    let mut set = |key: &str, value: Value| {
        out.insert(key.to_string(), value);
    };
    if npm == "@ai-sdk/google-vertex/anthropic" || (!api_id.contains("claude") && npm == "@ai-sdk/anthropic") {
        set("toolStreaming", false.into());
    }
    if provider_id == "openai" || ["@ai-sdk/openai", "@ai-sdk/github-copilot", "@ai-sdk/amazon-bedrock/mantle", "@ai-sdk/xai", "@ai-sdk/azure"].contains(&npm) {
        set("store", false.into());
    }
    if npm == "@openrouter/ai-sdk-provider" || npm == "@llmgateway/ai-sdk-provider" {
        set("usage", json!({"include": true}));
        if lower.contains("gemini") && !GEMINI_LEGACY.is_match(api_id) {
            set("reasoning", json!({"effort": "high"}));
        }
    }
    if provider_id == "baseten" || (provider_id == "opencode" && ["kimi-k2-thinking", "glm-4.6"].contains(&api_id)) {
        set("chat_template_args", json!({"enable_thinking": true}));
    }
    if ["zai", "zhipuai"].iter().any(|id| provider_id.contains(id)) && npm == "@ai-sdk/openai-compatible" {
        set("thinking", json!({"type": "enabled", "clear_thinking": false}));
    }
    if provider_id == "meta" && npm == "@ai-sdk/openai" {
        set("reasoningSummary", "auto".into());
        set("include", encrypted.clone());
    }
    if (npm == "@ai-sdk/google" || npm == "@ai-sdk/google-vertex") && model.reasoning {
        let mut config = json!({"includeThoughts": true});
        if !GEMINI_LEGACY.is_match(api_id) {
            config["thinkingLevel"] = "high".into();
        }
        set("thinkingConfig", config);
    }
    if lower.contains("minimax-m3") && npm == "@ai-sdk/anthropic" {
        set("thinking", json!({"type": "adaptive"}));
    }
    if ["@ai-sdk/anthropic", "@ai-sdk/google-vertex/anthropic"].contains(&npm) && kimi_family(model) && model.reasoning {
        set("thinking", json!({"type": "adaptive", "display": "summarized"}));
        set("effort", "high".into());
    }
    if provider_id == "alibaba-cn" && model.reasoning && npm == "@ai-sdk/openai-compatible" && !lower.contains("kimi-k2-thinking") {
        set("enable_thinking", true.into());
    }
    let cache_key = provider.options.get("setCacheKey").and_then(Value::as_bool);
    if let Some(session) = session_id.filter(|_| cache_key != Some(false)) {
        if npm == "@ai-sdk/deepinfra" || npm == "@ai-sdk/cerebras" {
            set("prompt_cache_key", session.into());
        } else if ["@ai-sdk/openai", "@ai-sdk/azure", "@ai-sdk/xai", "@ai-sdk/mistral", "venice-ai-sdk-provider"].contains(&npm) || cache_key == Some(true) {
            set("promptCacheKey", session.into());
        }
    }
    if npm == "@ai-sdk/gateway" {
        set("gateway", json!({"caching": "auto"}));
    }
    if npm == "@ai-sdk/azure" && provider.options.get("useCompletionUrls").is_some_and(|v| v.as_bool() != Some(false) && !v.is_null()) {
        let version = GPT_VERSION.captures(api_id).map(|c| (c[1].parse::<u64>().unwrap_or(0), c[2].parse::<u64>().unwrap_or(0)));
        if !version.is_some_and(|(major, minor)| major > 5 || (major == 5 && minor >= 5)) {
            set("reasoningEffort", "medium".into());
        }
        return Value::Object(out);
    }
    if api_id.contains("gpt-5") && !api_id.contains("gpt-5-chat") {
        if !api_id.contains("gpt-5-pro") {
            set("reasoningEffort", "medium".into());
            if ["@ai-sdk/openai", "@ai-sdk/azure", "@ai-sdk/github-copilot", "@ai-sdk/amazon-bedrock/mantle"].contains(&npm) {
                set("reasoningSummary", "auto".into());
            }
            if npm == "@ai-sdk/openai" || npm == "@ai-sdk/amazon-bedrock/mantle" {
                set("include", encrypted.clone());
            }
        }
        if api_id.contains("gpt-5.") && !api_id.contains("codex") && !api_id.contains("-chat") && (npm == "@ai-sdk/openai" || npm == "@ai-sdk/amazon-bedrock/mantle") {
            set("textVerbosity", "low".into());
        }
        if let Some(session) = session_id.filter(|_| provider_id.starts_with("opencode") && cache_key != Some(false)) {
            set("promptCacheKey", session.into());
            set("include", encrypted);
            set("reasoningSummary", "auto".into());
        }
    }
    Value::Object(out)
}

// `defaults` with the model's own configured options merged over them.
pub fn options(provider: &Provider, model: &Model, session_id: Option<&str>) -> Value {
    let mut out = defaults(provider, model, session_id);
    opencode_core::config::merge_deep(&mut out, model.options.clone());
    out
}

// Reasoning variants of a model: name -> options merged over the defaults when selected.
pub fn variants(model: &Model) -> &Map<String, Value> {
    &model.variants
}

// Merges the named variant's options into `options`; unknown names change nothing.
pub fn apply_variant(options: &mut Value, model: &Model, name: &str) -> bool {
    let Some(variant) = model.variants.get(name) else { return false };
    if !options.is_object() {
        *options = json!({});
    }
    opencode_core::config::merge_deep(options, variant.clone());
    true
}

// Last step before the options reach an encoder: the reasoning gate of the OpenAI SDKs, or thinking block binding.
pub fn finalize_options(model: &Model, options: &mut Value) {
    let Some(map) = options.as_object_mut() else { return };
    let npm = model.npm.as_str();
    if copilot::is_provider(&model.provider_id) && npm == "@ai-sdk/anthropic" {
        map.insert("toolStreaming".into(), false.into());
    }
    if ["@ai-sdk/openai", "@ai-sdk/azure", "@ai-sdk/amazon-bedrock/mantle"].contains(&npm) {
        if model.reasoning || map.contains_key("reasoningEffort") || map.contains_key("reasoningSummary") {
            map.insert("forceReasoning".into(), true.into());
        }
        return;
    }
    let key = match npm {
        "@ai-sdk/amazon-bedrock" => "reasoningConfig",
        "@ai-sdk/anthropic" | "@ai-sdk/google-vertex/anthropic" => "thinking",
        _ => return,
    };
    // `blockBinding: false` is an opencode-only opt-out and never reaches the wire.
    if map.get(key).and_then(|t| t.get("blockBinding")) == Some(&Value::Bool(false)) {
        let config = map.get_mut(key).and_then(Value::as_object_mut).unwrap();
        config.remove("blockBinding");
        if config.is_empty() {
            map.remove(key);
        }
        return;
    }
    if !variants::binds_thinking(&model.api_id) {
        return;
    }
    let mut config = map.get(key).cloned().unwrap_or_else(|| json!({"type": "adaptive"}));
    let kind = config.get("type").and_then(Value::as_str);
    if !matches!(kind, Some("adaptive" | "enabled")) || config.get("blockBinding").is_some() || !config.is_object() {
        return;
    }
    config["blockBinding"] = json!({"prefixMismatchBehavior": "drop_block"});
    map.insert(key.into(), config);
}

// Whether prompt-cache markers apply to this model.
pub fn caches(model: &Model, options: &Value) -> bool {
    let npm = model.npm.as_str();
    let automatic = options.get("cacheControl").is_some_and(|v| !v.is_null()) && (npm == "@ai-sdk/anthropic" || npm == "@ai-sdk/google-vertex/anthropic");
    let named = [&model.api_id, &model.id].iter().any(|id| id.contains("anthropic") || id.contains("claude"));
    let anthropic = model.provider_id == "anthropic" || model.provider_id == "google-vertex-anthropic" || named || npm == "@ai-sdk/anthropic" || npm == "@ai-sdk/alibaba";
    anthropic && npm != "@ai-sdk/gateway" && !automatic
}

// Fills every unset shaping field of a request for one session turn.
pub fn apply(request: &mut Request, provider: &Provider, model: &Model, session: &Session) {
    let defaults = sampling(model, request.temperature, request.top_p);
    request.temperature = defaults.temperature;
    request.top_p = defaults.top_p;
    request.top_k = request.top_k.or(defaults.top_k);
    request.max_tokens = defaults.max_tokens;
    if request.options.is_null() {
        request.options = options(provider, model, Some(session.session_id));
    }
    if let Some(name) = request.variant.take() {
        apply_variant(&mut request.options, model, &name);
    }
    finalize_options(model, &mut request.options);
    request.cache = caches(model, &request.options);
    let mut merged = headers(model, session);
    for (name, value) in std::mem::take(&mut request.headers) {
        merged.retain(|(k, _)| !k.eq_ignore_ascii_case(&name));
        merged.push((name, value));
    }
    request.headers = merged;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider;
    use opencode_core::auth;

    fn model(provider_id: &str, api_id: &str, npm: &str) -> (Provider, Model) {
        let catalog = json!({provider_id: {"id": provider_id, "env": [], "npm": npm, "api": "https://example.test/v1", "models": {
            api_id: {"id": api_id, "temperature": true, "reasoning": true, "limit": {"context": 200000, "output": 64000}}
        }}});
        let auth = vec![(provider_id.to_string(), auth::Info::Api { key: "k".into(), metadata: None })];
        let provider = provider::build(&catalog, &json!({}), &auth).remove(0);
        let model = provider.models[0].clone();
        (provider, model)
    }

    fn only(api_id: &str) -> Model {
        model("p", api_id, "@ai-sdk/openai-compatible").1
    }

    #[test]
    fn sampling_defaults_follow_model_family() {
        let t = |id: &str| temperature(&only(id));
        assert_eq!([t("claude-sonnet-4-5"), t("gpt-5.5"), t("qwen-north-mini-code")], [None, None, Some(1.0)]);
        assert_eq!([t("gemini-2.5-pro"), t("gemini-3-flash-preview"), t("gemini-3.1-pro"), t("gemini-3.5-flash")], [Some(1.0); 4]);
        assert_eq!([t("gemini-3.5-flash-lite"), t("gemini-2.0-flash"), t("gemini-1.5-pro")], [None; 3]);
        assert_eq!([t("glm-4.6"), t("glm-4.7-flash"), t("MiniMax-M2"), t("glm-5")], [Some(1.0), Some(1.0), Some(1.0), None]);
        assert_eq!([t("kimi-k2-0905"), t("kimi-k2-thinking"), t("kimi-k2.5"), t("kimi-k2p5")], [Some(0.6), Some(1.0), Some(1.0), Some(1.0)]);
        let p = |id: &str| top_p(&only(id));
        assert_eq!([p("gemini-2.5-flash"), p("minimax-m2.1"), p("kimi-k2-5"), p("deepseek-v4-flash-0731"), p("deepseek-v4-flash"), p("gpt-5")], [Some(0.95), Some(0.95), Some(0.95), Some(0.95), None, None]);
        assert_eq!(top_p(&model("deepseek", "deepseek-v4-flash", "@ai-sdk/openai-compatible").1), Some(0.95));
        assert_eq!(top_p(&model("opencode-go", "deepseek-v4-flash", "@ai-sdk/openai-compatible").1), Some(0.95));
        let k = |id: &str| top_k(&only(id));
        assert_eq!([k("minimax-m2"), k("minimax-m2.5"), k("minimax-m21"), k("gemini-3.1-pro"), k("gemini-2.0-flash"), k("gpt-5")], [Some(20), Some(40), Some(40), Some(64), None, None]);
    }

    #[test]
    fn sampling_respects_capabilities_and_limits() {
        let (_, mut kimi) = model("moonshotai", "kimi-k2-0905", "@ai-sdk/openai-compatible");
        assert_eq!(sampling(&kimi, None, None), Sampling { temperature: Some(0.6), top_p: None, top_k: None, max_tokens: Some(32_000) });
        assert_eq!(sampling(&kimi, Some(0.2), Some(0.5)).temperature, Some(0.2));
        kimi.temperature = false;
        kimi.limit_output = 8192;
        assert_eq!(sampling(&kimi, Some(0.2), Some(0.5)), Sampling { temperature: None, top_p: Some(0.5), top_k: None, max_tokens: Some(8192) });
        kimi.limit_output = 0;
        assert_eq!(max_output_tokens(&kimi, 32_000), 32_000);
        assert_eq!(sampling(&model("openai", "gpt-5.5", "@ai-sdk/openai").1, None, None).max_tokens, None);
    }

    #[test]
    fn request_headers() {
        let session = Session { session_id: "ses_1", parent_session_id: Some("ses_0"), request_id: Some("msg_1"), project_id: Some("prj") };
        let (_, mut m) = model("deepseek", "deepseek-chat", "@ai-sdk/openai-compatible");
        m.headers.insert("X-Custom".into(), "1".into());
        let pairs = |list: &[(&str, &str)]| list.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<Vec<_>>();
        let agent = user_agent();
        assert_eq!(
            headers(&m, &session),
            pairs(&[("x-opencode-session-id", "ses_1"), ("x-opencode-parent-session-id", "ses_0"), ("x-session-affinity", "ses_1"), ("X-Session-Id", "ses_1"), ("User-Agent", &agent), ("x-parent-session-id", "ses_0"), ("X-Custom", "1")])
        );
        let zen = headers(&model("opencode", "big-pickle", "@ai-sdk/openai-compatible").1, &session);
        let names: Vec<&str> = zen.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["x-opencode-session-id", "x-opencode-parent-session-id", "x-opencode-project", "x-opencode-session", "x-opencode-request", "x-opencode-client", "User-Agent", "x-parent-session-id"]);
        let openai = headers(&model("openai", "gpt-5.5", "@ai-sdk/openai").1, &Session { session_id: "ses_1", ..Default::default() });
        let get = |name: &str| openai.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
        assert_eq!((get("originator"), get("session-id"), get("X-Session-Id")), (Some("opencode"), Some("ses_1"), Some("ses_1")));
        assert!(get("User-Agent").unwrap().starts_with(&format!("{agent} (")));
        assert_eq!(openai.iter().filter(|(k, _)| k == "User-Agent").count(), 1);
    }

    #[test]
    fn default_options_per_provider() {
        let (p, m) = model("openai", "gpt-5.5", "@ai-sdk/openai");
        assert_eq!(
            serde_json::to_string(&defaults(&p, &m, Some("ses_1"))).unwrap(),
            r#"{"store":false,"promptCacheKey":"ses_1","reasoningEffort":"medium","reasoningSummary":"auto","include":["reasoning.encrypted_content"],"textVerbosity":"low"}"#
        );
        let (p, m) = model("openai", "gpt-5.2-codex", "@ai-sdk/openai");
        assert_eq!(defaults(&p, &m, None), json!({"store": false, "reasoningEffort": "medium", "reasoningSummary": "auto", "include": ["reasoning.encrypted_content"]}));
        let (p, m) = model("openai", "gpt-5-pro", "@ai-sdk/openai");
        assert_eq!(defaults(&p, &m, None), json!({"store": false}));
        let (p, m) = model("opencode", "gpt-5.2-codex", "@ai-sdk/openai");
        assert_eq!(defaults(&p, &m, Some("s"))["promptCacheKey"], json!("s"));
        let (p, m) = model("zai-coding-plan", "glm-4.7", "@ai-sdk/openai-compatible");
        assert_eq!(defaults(&p, &m, Some("s")), json!({"thinking": {"type": "enabled", "clear_thinking": false}}));
        let (p, m) = model("minimax", "MiniMax-M3", "@ai-sdk/anthropic");
        assert_eq!(defaults(&p, &m, None), json!({"toolStreaming": false, "thinking": {"type": "adaptive"}}));
        let (p, m) = model("kimi-for-coding", "k2p5", "@ai-sdk/anthropic");
        assert_eq!(defaults(&p, &m, None), json!({"toolStreaming": false, "thinking": {"type": "adaptive", "display": "summarized"}, "effort": "high"}));
        let (p, mut m) = model("anthropic", "claude-sonnet-4-5", "@ai-sdk/anthropic");
        assert_eq!(defaults(&p, &m, Some("s")), json!({}));
        assert!(caches(&m, &json!({})) && !caches(&m, &json!({"cacheControl": {"type": "ephemeral"}})));
        m.options = json!({"thinking": {"type": "enabled", "budgetTokens": 16000}});
        assert_eq!(options(&p, &m, None), m.options);
        assert!(caches(&model("openrouter", "anthropic/claude-opus-4.7", "@openrouter/ai-sdk-provider").1, &json!({})));
        assert!(!caches(&model("deepseek", "deepseek-chat", "@ai-sdk/openai-compatible").1, &json!({})));
    }

    #[test]
    fn variants_and_final_option_rules() {
        let (p, m) = model("openai", "gpt-5.5", "@ai-sdk/openai");
        assert_eq!(variants(&m).keys().map(String::as_str).collect::<Vec<_>>(), ["none", "low", "medium", "high", "xhigh"]);
        let mut request = Request { variant: Some("xhigh".into()), ..Default::default() };
        apply(&mut request, &p, &m, &Session { session_id: "ses_1", ..Default::default() });
        assert_eq!((&request.options["reasoningEffort"], &request.options["forceReasoning"], &request.options["store"]), (&json!("xhigh"), &json!(true), &json!(false)));
        let mut options = json!({"store": false});
        assert!(!apply_variant(&mut options, &m, "missing") && options == json!({"store": false}));

        // Claude 5.1+ binds thinking blocks; `blockBinding: false` opts out and is never sent.
        let (_, fable) = model("anthropic", "claude-fable-5-1", "@ai-sdk/anthropic");
        let finalize = |model: &Model, mut options: Value| {
            finalize_options(model, &mut options);
            options
        };
        let binding = json!({"prefixMismatchBehavior": "drop_block"});
        assert_eq!(finalize(&fable, json!({})), json!({"thinking": {"type": "adaptive", "blockBinding": binding}}));
        assert_eq!(finalize(&fable, json!({"thinking": {"type": "enabled", "budgetTokens": 1}}))["thinking"]["blockBinding"], binding);
        assert_eq!(finalize(&fable, json!({"thinking": {"type": "disabled"}})), json!({"thinking": {"type": "disabled"}}));
        assert_eq!(finalize(&fable, json!({"thinking": {"blockBinding": false}})), json!({}));
        assert_eq!(finalize(&fable, json!({"thinking": {"type": "adaptive", "blockBinding": false}})), json!({"thinking": {"type": "adaptive"}}));
        let (_, sonnet) = model("anthropic", "claude-sonnet-4-5", "@ai-sdk/anthropic");
        assert_eq!(finalize(&sonnet, json!({"effort": "high"})), json!({"effort": "high"}));
        let plain = Model { reasoning: false, ..m.clone() };
        assert_eq!(finalize(&plain, json!({"store": false})), json!({"store": false}));
        assert_eq!(finalize(&plain, json!({"reasoningSummary": "auto"}))["forceReasoning"], json!(true));

        // Copilot: its own headers, no output cap for GPT models, no eager tool streaming on its Anthropic shim.
        let (_, mut copilot) = model("github-copilot", "gpt-4.1", "@ai-sdk/openai-compatible");
        assert_eq!(copilot.npm, crate::copilot::NPM);
        let sent = headers(&copilot, &Session { session_id: "ses_1", parent_session_id: Some("ses_0"), ..Default::default() });
        let get = |name: &str| sent.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
        assert_eq!((get("X-GitHub-Api-Version"), get("X-Interaction-Id"), get("x-initiator"), get("anthropic-beta")), (Some("2026-06-01"), Some("ses_1"), Some("agent"), None));
        assert_eq!(sampling(&copilot, None, None).max_tokens, None);
        copilot.npm = "@ai-sdk/anthropic".into();
        copilot.api_id = "claude-sonnet-4.5".into();
        assert_eq!(finalize(&copilot, json!({})), json!({"toolStreaming": false}));
        assert_eq!(sampling(&copilot, None, None).max_tokens, Some(32_000));
        let sent = headers(&copilot, &Session { session_id: "ses_1", ..Default::default() });
        assert!(sent.contains(&("anthropic-beta".to_string(), "interleaved-thinking-2025-05-14".to_string())) && !sent.iter().any(|(k, _)| k == "x-initiator"));
    }

    #[test]
    fn apply_fills_a_request() {
        let (p, m) = model("anthropic", "claude-sonnet-4-5", "@ai-sdk/anthropic");
        let mut request = Request { headers: vec![("user-agent".into(), "custom".into())], ..Default::default() };
        apply(&mut request, &p, &m, &Session { session_id: "ses_1", ..Default::default() });
        assert_eq!((request.temperature, request.max_tokens, request.cache), (None, Some(32_000), true));
        assert_eq!(request.options, json!({}));
        assert_eq!(request.headers.last().unwrap(), &("user-agent".to_string(), "custom".to_string()));
        assert!(!request.headers.iter().any(|(k, _)| k == "User-Agent"));
    }
}
