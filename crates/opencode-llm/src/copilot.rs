// GitHub Copilot: device login, API host, and the per-request headers its auth plugin sets.
use crate::Error;
use opencode_core::{auth, paths::Paths};
use serde_json::{Value, json};
use std::path::PathBuf;

pub const CLIENT_ID: &str = "Ov23li8tweQw6odWQebz";
pub const API_VERSION: &str = "2026-06-01";
pub const PROVIDER_ID: &str = "github-copilot";
pub const NPM: &str = "@ai-sdk/github-copilot";
pub const SCOPE: &str = "read:user";
pub const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
pub const SYNTHETIC_ATTACHMENT_PROMPT: &str = "Attached media from tool result:";
// Models tried, in order, for titles and other utility calls.
pub const UTILITY_MODELS: [&str; 4] = ["gpt-5.4-nano", "gpt-4.1", "gpt-4o", "gpt-4o-mini"];
const POLL_MARGIN_MS: u64 = 3000;

pub fn is_provider(provider_id: &str) -> bool {
    provider_id.contains(PROVIDER_ID)
}

pub fn normalize_domain(url: &str) -> String {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")).unwrap_or(url);
    rest.strip_suffix('/').unwrap_or(rest).to_string()
}

// API host: public Copilot, or the enterprise deployment the login belongs to.
pub fn base(enterprise_url: Option<&str>) -> String {
    match enterprise_url.filter(|u| !u.is_empty()) {
        Some(url) => format!("https://copilot-api.{}", normalize_domain(url)),
        None => "https://api.githubcopilot.com".to_string(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub verification_uri: String,
    pub user_code: String,
    pub device_code: String,
    // Seconds between polls, as announced by GitHub.
    pub interval: u64,
    // Set for GitHub Enterprise logins; stored as `enterpriseUrl`.
    pub enterprise_domain: Option<String>,
    pub token_url: String,
}

impl Device {
    pub fn instructions(&self) -> String {
        format!("Enter code: {}", self.user_code)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Poll {
    // Store this entry under `github-copilot`.
    Success(auth::Info),
    // Not authorized yet; poll again after this many milliseconds.
    Pending { wait_ms: u64 },
    Failed,
}

fn post(http: &reqwest::Client, url: &str, body: Value) -> reqwest::RequestBuilder {
    http.post(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("User-Agent", crate::shape::user_agent())
        .body(body.to_string())
}

pub(crate) async fn start_at(http: &reqwest::Client, device_url: &str, token_url: &str, enterprise_domain: Option<String>) -> Result<Device, Error> {
    let response = post(http, device_url, json!({"client_id": CLIENT_ID, "scope": SCOPE})).send().await?;
    if !response.status().is_success() {
        return Err(Error::Config("Failed to initiate device authorization".into()));
    }
    let data: Value = serde_json::from_str(&response.text().await?).map_err(|_| Error::Config("Failed to initiate device authorization".into()))?;
    let field = |key: &str| data.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
    Ok(Device {
        verification_uri: field("verification_uri"),
        user_code: field("user_code"),
        device_code: field("device_code"),
        interval: data.get("interval").and_then(Value::as_u64).unwrap_or(5),
        enterprise_domain,
        token_url: token_url.to_string(),
    })
}

// Starts the device flow on github.com, or on an enterprise host when `enterprise_url` is given.
pub async fn device_start(http: &reqwest::Client, enterprise_url: Option<&str>) -> Result<Device, Error> {
    let enterprise = enterprise_url.filter(|u| !u.is_empty()).map(normalize_domain);
    let domain = enterprise.clone().unwrap_or_else(|| "github.com".to_string());
    start_at(http, &format!("https://{domain}/login/device/code"), &format!("https://{domain}/login/oauth/access_token"), enterprise).await
}

// One poll of the token endpoint. The GitHub token is the credential itself: stored as both refresh and access, never exchanged.
pub async fn device_poll(http: &reqwest::Client, device: &Device) -> Result<Poll, Error> {
    let body = json!({"client_id": CLIENT_ID, "device_code": device.device_code, "grant_type": DEVICE_GRANT});
    let response = post(http, &device.token_url, body).send().await?;
    if !response.status().is_success() {
        return Ok(Poll::Failed);
    }
    let data: Value = serde_json::from_str(&response.text().await?).unwrap_or(Value::Null);
    if let Some(token) = data.get("access_token").and_then(Value::as_str).filter(|t| !t.is_empty()) {
        return Ok(Poll::Success(auth::Info::Oauth {
            refresh: token.to_string(),
            access: token.to_string(),
            expires: 0,
            account_id: None,
            enterprise_url: device.enterprise_domain.clone(),
        }));
    }
    let pending = device.interval * 1000 + POLL_MARGIN_MS;
    Ok(match data.get("error").and_then(Value::as_str) {
        Some("authorization_pending") | None | Some("") => Poll::Pending { wait_ms: pending },
        // RFC 8628 adds five seconds; GitHub may announce its own interval instead.
        Some("slow_down") => {
            let server = data.get("interval").and_then(Value::as_f64).filter(|i| *i > 0.0);
            let seconds = server.unwrap_or((device.interval + 5) as f64);
            Poll::Pending { wait_ms: (seconds * 1000.0) as u64 + POLL_MARGIN_MS }
        }
        Some(_) => Poll::Failed,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub auth_file: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Config { auth_file: Paths::from_env().auth_file() }
    }
}

// The stored GitHub token; auth.json is re-read on every request.
pub fn token(config: &Config) -> Result<String, Error> {
    match auth::read_all(&config.auth_file).into_iter().find(|(id, _)| id == PROVIDER_ID) {
        Some((_, auth::Info::Oauth { refresh, .. })) => Ok(refresh),
        _ => Err(Error::Config("github-copilot is not logged in (no oauth entry in auth.json)".into())),
    }
}

fn synthetic(message: &Value) -> bool {
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    match &message["content"] {
        Value::String(text) => text == SYNTHETIC_ATTACHMENT_PROMPT,
        Value::Array(parts) => parts.iter().any(|p| {
            matches!(p.get("type").and_then(Value::as_str), Some("text" | "input_text")) && p.get("text").and_then(Value::as_str) == Some(SYNTHETIC_ATTACHMENT_PROMPT)
        }),
        _ => false,
    }
}

fn any_part(items: &[Value], test: impl Fn(&Value) -> bool) -> bool {
    items.iter().any(|item| item.get("content").and_then(Value::as_array).is_some_and(|parts| parts.iter().any(&test)))
}

// (vision, agent-initiated) for a chat, responses or messages request body.
pub fn classify(url: &str, body: &Value) -> (bool, bool) {
    let kind = |part: &Value| part.get("type").and_then(Value::as_str).unwrap_or_default().to_string();
    let user = |message: Option<&Value>| message.and_then(|m| m.get("role")).and_then(Value::as_str) == Some("user");
    let messages = body.get("messages").and_then(Value::as_array);
    if let Some(messages) = messages.filter(|_| url.contains("completions")) {
        let last = messages.last();
        return (any_part(messages, |p| kind(p) == "image_url"), !user(last) || last.is_some_and(synthetic));
    }
    if let Some(input) = body.get("input").and_then(Value::as_array) {
        let last = input.last();
        return (any_part(input, |p| kind(p) == "input_image"), !user(last) || last.is_some_and(synthetic));
    }
    if let Some(messages) = messages {
        let last = messages.last();
        let own = last.and_then(|m| m.get("content")).and_then(Value::as_array).is_some_and(|parts| parts.iter().any(|p| kind(p) != "tool_result"));
        let nested = |p: &Value| kind(p) == "tool_result" && p.get("content").and_then(Value::as_array).is_some_and(|c| c.iter().any(|n| kind(n) == "image"));
        return (any_part(messages, |p| kind(p) == "image" || nested(p)), !(user(last) && own) || last.is_some_and(synthetic));
    }
    (false, false)
}

// Applies what the plugin's fetch does to an outgoing request's headers.
pub fn apply_headers(headers: &mut Vec<(String, String)>, url: &str, body: &Value, token: &str) {
    let (vision, agent) = classify(url, body);
    let mut set = |name: &str, value: &str, replace: bool| {
        let present = headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name));
        if replace {
            headers.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
        }
        if replace || !present {
            headers.push((name.to_string(), value.to_string()));
        }
    };
    // A header set earlier (sub-agent or compaction turns) wins over the body classification.
    set("x-initiator", if agent { "agent" } else { "user" }, false);
    set("User-Agent", &crate::shape::user_agent(), true);
    set("Authorization", &format!("Bearer {token}"), true);
    set("Openai-Intent", "conversation-edits", true);
    if vision {
        set("Copilot-Vision-Request", "true", true);
    }
    headers.retain(|(k, _)| !k.eq_ignore_ascii_case("x-api-key"));
}

// `copilot_usage.total_nano_aiu` of a raw stream chunk: the billed amount, top-level or under `response`.
pub fn nano_aiu(chunk: &Value) -> Option<f64> {
    let read = |v: &Value| v.pointer("/copilot_usage/total_nano_aiu").and_then(Value::as_f64).filter(|n| n.is_finite() && *n >= 0.0);
    read(chunk).or_else(|| chunk.get("response").and_then(read))
}

const DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

// An item of `GET /models` that can be offered: enabled by policy, with limits and a tool-call flag.
fn usable(item: &Value) -> bool {
    let typed = item.get("model_picker_enabled").is_some_and(Value::is_boolean)
        && ["id", "name", "version"].iter().all(|key| item.get(*key).is_some_and(Value::is_string))
        && item.pointer("/capabilities/family").is_some_and(Value::is_string)
        && item.pointer("/capabilities/supports").is_some_and(Value::is_object);
    typed
        && item.pointer("/policy/state").and_then(Value::as_str) != Some("disabled")
        && item.pointer("/capabilities/limits/max_output_tokens").is_some_and(Value::is_number)
        && item.pointer("/capabilities/limits/max_prompt_tokens").is_some_and(Value::is_number)
        && item.pointer("/capabilities/supports/tool_calls").is_some_and(Value::is_boolean)
}

// One model from its `/models` item, keeping what the catalog already says about it.
pub fn discovered_model(key: &str, remote: &Value, base: &str, prev: Option<&crate::provider::Model>) -> crate::provider::Model {
    let supports = &remote["capabilities"]["supports"];
    let limits = &remote["capabilities"]["limits"];
    let id = remote["id"].as_str().unwrap_or(key);
    let endpoints: Vec<&str> = remote.get("supported_endpoints").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    let media: Vec<&str> = limits.pointer("/vision/supported_media_types").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    let efforts: Vec<&str> = supports.get("reasoning_effort").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    let flag = |key: &str| supports.get(key).and_then(Value::as_bool).unwrap_or(false);
    let messages = endpoints.contains(&"/v1/messages");
    let endpoint = if messages {
        Some("messages")
    } else if endpoints.contains(&"/responses") {
        Some("responses")
    } else if endpoints.contains(&"/chat/completions") {
        Some("chat")
    } else {
        None
    };
    let reasoning = flag("adaptive_thinking") || !efforts.is_empty() || supports.get("max_thinking_budget").is_some_and(Value::is_number) || supports.get("min_thinking_budget").is_some_and(Value::is_number);
    let prices = remote.pointer("/billing/token_prices");
    let batch = prices.and_then(|p| p.get("batch_size")).and_then(Value::as_f64).unwrap_or(0.0);
    let per_million = if batch > 0.0 { 10_000.0 / batch } else { 0.0 };
    let price = |key: &str| prices.and_then(|p| p.pointer(&format!("/default/{key}"))).and_then(Value::as_f64).unwrap_or(0.0) * per_million;
    let version = remote["version"].as_str().unwrap_or_default();
    let release = version.strip_prefix(&format!("{id}-")).unwrap_or(version);

    let mut variants = serde_json::Map::new();
    if !messages && !efforts.is_empty() {
        for effort in &efforts {
            variants.insert((*effort).into(), json!({"reasoningEffort": effort, "reasoningSummary": "auto", "include": ["reasoning.encrypted_content"]}));
        }
    } else if !efforts.is_empty() && flag("adaptive_thinking") {
        for effort in &efforts {
            variants.insert((*effort).into(), json!({"thinking": {"type": "adaptive", "display": "summarized"}, "effort": effort}));
        }
    } else if let Some(max) = supports.get("max_thinking_budget").and_then(Value::as_u64).filter(|m| *m > 0) {
        variants.insert("max".into(), json!({"thinking": {"type": "enabled", "budgetTokens": max - 1}}));
        variants.insert("high".into(), json!({"thinking": {"type": "enabled", "budgetTokens": max / 2}}));
    }
    let mut info = json!({
        "id": key,
        "name": prev.map(|p| p.name.clone()).unwrap_or_else(|| remote["name"].as_str().unwrap_or(key).to_string()),
        "family": prev.map(|p| p.family.clone()).filter(|f| !f.is_empty()).unwrap_or_else(|| remote["capabilities"]["family"].as_str().unwrap_or_default().to_string()),
        "api": {"id": id, "url": if messages { format!("{base}/v1") } else { base.to_string() }, "npm": if messages { "@ai-sdk/anthropic" } else { NPM }, "endpoint": endpoint},
        "status": "active",
        "limit": {"context": limits.get("max_context_window_tokens").filter(|v| v.is_number()).unwrap_or(&limits["max_prompt_tokens"]), "input": limits["max_prompt_tokens"], "output": limits["max_output_tokens"]},
        "capabilities": {
            "temperature": prev.is_none_or(|p| p.temperature),
            "reasoning": prev.map_or(reasoning, |p| p.reasoning),
            "attachment": prev.is_none_or(|p| p.attachment),
            "toolcall": flag("tool_calls"),
            "input": {"text": true, "audio": false, "image": flag("vision") || media.iter().any(|m| m.starts_with("image/")), "video": false, "pdf": flag("vision") && media.contains(&"application/pdf")},
            "interleaved": false
        },
        "cost": {"input": price("input_price"), "output": price("output_price"), "cache": {"read": price("cache_price"), "write": 0}},
        "options": prev.map(|p| p.options.clone()).unwrap_or_else(|| json!({})),
        "headers": prev.map(|p| Value::Object(p.headers.clone())).unwrap_or_else(|| json!({})),
        "release_date": prev.map(|p| p.release_date.clone()).filter(|d| !d.is_empty()).unwrap_or_else(|| release.to_string())
    });
    if !variants.is_empty() {
        info["variants"] = Value::Object(variants);
    }
    crate::provider::model_from_info(PROVIDER_ID, key, &info)
}

// `GET {base}/models`: the models this login may pick, rebuilt from what the API announces. `headers` are the provider's
// configured headers. On any error the caller keeps the catalog list (already pointed at `base`).
pub async fn discover(http: &reqwest::Client, base: &str, headers: &[(String, String)], token: &str, existing: &[crate::provider::Model]) -> Result<Vec<crate::provider::Model>, Error> {
    let mut request = http.get(format!("{base}/models")).timeout(DISCOVERY_TIMEOUT);
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let response = request.header("Authorization", format!("Bearer {token}")).header("User-Agent", crate::shape::user_agent()).header("X-GitHub-Api-Version", API_VERSION).send().await?;
    if !response.status().is_success() {
        return Err(Error::Config(format!("Failed to fetch models: {}", response.status().as_u16())));
    }
    let data: Value = serde_json::from_str(&response.text().await?).map_err(|e| Error::Config(format!("Failed to fetch models: {e}")))?;
    let Some(items) = data.get("data").and_then(Value::as_array) else {
        return Err(Error::Config("Failed to fetch models: no data".into()));
    };
    let remote: Vec<(&str, &Value)> = items.iter().filter(|item| usable(item)).filter_map(|item| Some((item["id"].as_str()?, item))).collect();
    let find = |id: &str| remote.iter().rev().find(|(remote_id, _)| *remote_id == id).map(|(_, item)| *item);
    let mut out: Vec<crate::provider::Model> = existing.iter().filter_map(|model| Some(discovered_model(&model.id, find(&model.api_id)?, base, Some(model)))).collect();
    for (i, (id, item)) in remote.iter().enumerate() {
        let last = !remote[i + 1..].iter().any(|(other, _)| other == id);
        if last && !out.iter().any(|m| m.id == *id) {
            out.push(discovered_model(id, item, base, None));
        }
    }
    out.retain(|model| find(&model.api_id).is_some_and(|item| item["model_picker_enabled"] == true));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_and_domains() {
        assert_eq!(base(None), "https://api.githubcopilot.com");
        assert_eq!(base(Some("https://company.ghe.com/")), "https://copilot-api.company.ghe.com");
        assert_eq!(normalize_domain("http://x.test"), "x.test");
        assert!(is_provider("github-copilot") && is_provider("github-copilot-enterprise") && !is_provider("openai"));
    }

    #[test]
    fn classifies_bodies_and_sets_headers() {
        let chat = "https://api.githubcopilot.com/chat/completions";
        let user = json!({"messages": [{"role": "user", "content": "hi"}]});
        assert_eq!(classify(chat, &user), (false, false));
        let tool = json!({"messages": [{"role": "user", "content": [{"type": "image_url", "image_url": {"url": "data:x"}}]}, {"role": "tool", "content": "x"}]});
        assert_eq!(classify(chat, &tool), (true, true));
        let synthetic = json!({"messages": [{"role": "user", "content": [{"type": "text", "text": SYNTHETIC_ATTACHMENT_PROMPT}]}]});
        assert_eq!(classify(chat, &synthetic), (false, true));
        let responses = json!({"input": [{"role": "user", "content": [{"type": "input_image", "image_url": "data:x"}]}]});
        assert_eq!(classify("https://api.githubcopilot.com/responses", &responses), (true, false));
        assert_eq!(classify("https://api.githubcopilot.com/responses", &json!({"input": [{"type": "function_call_output", "call_id": "c", "output": "x"}]})), (false, true));
        let messages = "https://api.githubcopilot.com/v1/messages";
        let results = json!({"messages": [{"role": "user", "content": [{"type": "tool_result", "content": [{"type": "image"}]}]}]});
        assert_eq!(classify(messages, &results), (true, true));
        assert_eq!(classify(messages, &json!({"messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]})), (false, false));
        assert_eq!(classify(messages, &json!({})), (false, false));

        let mut headers = vec![("user-agent".to_string(), "sdk".to_string()), ("x-api-key".to_string(), "k".to_string()), ("authorization".to_string(), "Bearer old".to_string())];
        apply_headers(&mut headers, chat, &tool, "gho_token");
        let get = |name: &str| headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str()).collect::<Vec<_>>();
        assert_eq!(get("authorization"), ["Bearer gho_token"]);
        assert_eq!(get("x-initiator"), ["agent"]);
        assert_eq!((get("openai-intent"), get("copilot-vision-request")), (vec!["conversation-edits"], vec!["true"]));
        assert_eq!(get("user-agent"), [crate::shape::user_agent().as_str()]);
        assert!(get("x-api-key").is_empty() && get("copilot-integration-id").is_empty());
        let mut headers = vec![("x-initiator".to_string(), "agent".to_string())];
        apply_headers(&mut headers, chat, &user, "t");
        assert_eq!(headers[0], ("x-initiator".to_string(), "agent".to_string()));
        assert!(!headers.iter().any(|(k, _)| k == "Copilot-Vision-Request"));
    }

    #[test]
    fn billed_amount_from_raw_chunks() {
        assert_eq!(nano_aiu(&json!({"copilot_usage": {"total_nano_aiu": 1500}})), Some(1500.0));
        assert_eq!(nano_aiu(&json!({"type": "response.completed", "response": {"copilot_usage": {"total_nano_aiu": 2.5}}})), Some(2.5));
        assert_eq!(nano_aiu(&json!({"copilot_usage": {"total_nano_aiu": -1}})), None);
        assert_eq!(nano_aiu(&json!({"usage": {}})), None);
    }
}
