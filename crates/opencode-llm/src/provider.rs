// Provider registry: models.dev catalog + config + env + auth.json.
use crate::hooks::Overrides;
use crate::{Error, anthropic, azure, bedrock, codex, copilot, dialect, gemini, responses, shape, snowflake, variants, vertex, xai};
use opencode_core::{auth, config::merge_deep, paths::Paths};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::sync::LazyLock;
use std::{env, fs};

const DEFAULT_NPM: &str = "@ai-sdk/openai-compatible";
pub const OUTPUT_TOKEN_MAX: u64 = 32_000;
pub const DEFAULT_TIMEOUT_MS: u64 = 300_000;
// Providers whose stored oauth entry is turned into request credentials by an auth plugin.
const OAUTH_PROVIDERS: [&str; 5] = ["openai", "github-copilot", "xai", "azure", "snowflake-cortex"];
static GPT_MAJOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^gpt-(\d+)").unwrap());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Chat,
    Anthropic,
    Responses,
    Gemini,
    // Bedrock Converse: JSON request, AWS event-stream response.
    Bedrock,
}

pub const AIHUBMIX_NPM: &str = "@aihubmix/ai-sdk-provider";
const AIHUBMIX_ROOT: &str = "https://aihubmix.com";
const AIHUBMIX_APP_CODE: &str = "WHVL9885";

// Wire protocol spoken by the SDK package a model is bound to.
pub fn protocol(npm: &str) -> Option<Protocol> {
    if npm.contains("@ai-sdk/openai-compatible") || dialect::wrapper(npm).is_some() {
        return Some(Protocol::Chat);
    }
    match npm {
        "@ai-sdk/anthropic" | vertex::ANTHROPIC_NPM => Some(Protocol::Anthropic),
        "@ai-sdk/openai" | "@ai-sdk/xai" | azure::NPM | bedrock::MANTLE_NPM => Some(Protocol::Responses),
        "@ai-sdk/google" | vertex::NPM => Some(Protocol::Gemini),
        bedrock::NPM => Some(Protocol::Bedrock),
        AIHUBMIX_NPM | copilot::NPM => Some(Protocol::Chat),
        _ => None,
    }
}

// Every package a catalog can name that resolves to a wire protocol here, with the protocol it defaults to.
pub const SUPPORTED_NPM: [&str; 25] = [
    "@ai-sdk/openai-compatible",
    "@ai-sdk/anthropic",
    "@ai-sdk/openai",
    "@ai-sdk/google",
    "@ai-sdk/xai",
    "@ai-sdk/azure",
    "@ai-sdk/amazon-bedrock",
    "@ai-sdk/amazon-bedrock/mantle",
    "@ai-sdk/google-vertex",
    "@ai-sdk/google-vertex/anthropic",
    "@ai-sdk/github-copilot",
    "@ai-sdk/groq",
    "@ai-sdk/cerebras",
    "@ai-sdk/deepinfra",
    "@ai-sdk/togetherai",
    "@ai-sdk/vercel",
    "@ai-sdk/mistral",
    "@ai-sdk/perplexity",
    "@ai-sdk/alibaba",
    "@openrouter/ai-sdk-provider",
    "venice-ai-sdk-provider",
    "@saladtechnologies-oss/ai-sdk-provider",
    "merge-gateway-ai-sdk-provider",
    "@qvac/ai-sdk-provider",
    AIHUBMIX_NPM,
];

// Like `protocol`, with the per-model endpoint choices of the Copilot, Mantle and AIHubMix packages.
pub fn protocol_for(model: &Model) -> Option<Protocol> {
    if model.npm == bedrock::MANTLE_NPM && bedrock::mantle_uses_chat(&model.api_id) {
        return Some(Protocol::Chat);
    }
    if model.npm == AIHUBMIX_NPM {
        let id = model.api_id.as_str();
        return Some(if id.starts_with("claude-") {
            Protocol::Anthropic
        } else if (id.starts_with("gemini") || id.starts_with("imagen")) && !id.ends_with("-nothink") && !id.ends_with("-search") {
            Protocol::Gemini
        } else {
            Protocol::Chat
        });
    }
    if model.npm != copilot::NPM {
        return protocol(&model.npm);
    }
    match model.endpoint.as_deref() {
        Some("responses") => return Some(Protocol::Responses),
        Some("chat") => return Some(Protocol::Chat),
        _ => {}
    }
    let major = GPT_MAJOR.captures(&model.api_id).and_then(|c| c[1].parse::<u64>().ok());
    Some(if major.is_some_and(|m| m >= 5) && !model.api_id.starts_with("gpt-5-mini") { Protocol::Responses } else { Protocol::Chat })
}

fn default_base(npm: &str) -> Option<&'static str> {
    match npm {
        "@ai-sdk/anthropic" => Some(anthropic::DEFAULT_BASE_URL),
        "@ai-sdk/openai" => Some(responses::DEFAULT_BASE_URL),
        "@ai-sdk/google" => Some(gemini::DEFAULT_BASE_URL),
        "@ai-sdk/xai" => Some("https://api.x.ai/v1"),
        _ => dialect::wrapper(npm).map(|w| w.base),
    }
}

// The variable each SDK reads on its own when the registry found no single key.
fn sdk_key_env(npm: &str) -> &'static [&'static str] {
    match npm {
        "@ai-sdk/anthropic" => &["ANTHROPIC_API_KEY"],
        "@ai-sdk/openai" => &["OPENAI_API_KEY"],
        "@ai-sdk/google" => &[gemini::API_KEY_ENV],
        "@ai-sdk/xai" => &["XAI_API_KEY"],
        azure::NPM => &[azure::API_KEY_ENV],
        vertex::NPM => &[vertex::API_KEY_ENV],
        AIHUBMIX_NPM => &["AIHUBMIX_API_KEY"],
        _ => dialect::wrapper(npm).map_or(&[], |w| w.env),
    }
}

fn env_var(name: &str) -> Option<String> {
    env::var(name).ok().filter(|v| !v.is_empty())
}

#[derive(Debug, Clone)]
pub struct Model {
    pub id: String,
    pub provider_id: String,
    pub api_id: String,
    pub api_url: String,
    pub npm: String,
    pub name: String,
    pub limit_output: u64,
    pub limit_context: u64,
    pub status: String,
    pub headers: Map<String, Value>,
    pub options: Value,
    pub temperature: bool,
    pub reasoning: bool,
    pub attachment: bool,
    pub cost_input: f64,
    pub release_date: String,
    // 0 when the catalog has no separate input limit.
    pub limit_input: u64,
    pub family: String,
    pub cost: Cost,
    pub toolcall: bool,
    pub input: Modalities,
    // Message field that carries reasoning back to the provider (`capabilities.interleaved.field`).
    pub interleaved: Option<String>,
    // Reasoning variants: name -> options merged over the defaults when selected.
    pub variants: Map<String, Value>,
    // Copilot only: "chat" or "responses" when the API announced it.
    pub endpoint: Option<String>,
}

// Input kinds a model accepts besides text.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Modalities {
    pub text: bool,
    pub audio: bool,
    pub image: bool,
    pub video: bool,
    pub pdf: bool,
}

fn modalities(m: &Value) -> Option<Modalities> {
    let list = m.pointer("/modalities/input")?.as_array()?;
    let has = |name: &str| list.iter().any(|v| v == name);
    Some(Modalities { text: has("text"), audio: has("audio"), image: has("image"), video: has("video"), pdf: has("pdf") })
}

fn interleaved(m: &Value) -> Option<Option<String>> {
    match m.get("interleaved")? {
        Value::String(field) => Some(Some(field.clone())),
        Value::Object(o) => Some(o.get("field").and_then(Value::as_str).map(str::to_string)),
        Value::Bool(_) => Some(None),
        _ => None,
    }
}

// USD per million tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rates {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Cost {
    pub base: Rates,
    // (context size threshold, rates) from `cost.tiers[]` with `tier.type == "context"`.
    pub tiers: Vec<(f64, Rates)>,
    pub over_200k: Option<Rates>,
}

impl Cost {
    // Rates for a request whose raw input token count is `context`.
    pub fn rates(&self, context: f64) -> Rates {
        let tier = self.tiers.iter().filter(|(size, _)| context > *size).max_by(|a, b| a.0.total_cmp(&b.0));
        match (tier, self.over_200k) {
            (Some((_, rates)), _) => *rates,
            (None, Some(rates)) if context > 200_000.0 => rates,
            _ => self.base,
        }
    }
}

fn rates(c: Option<&Value>) -> Rates {
    let num = |key: &str| c.and_then(|c| c.get(key)).and_then(Value::as_f64).filter(|v| v.is_finite()).unwrap_or(0.0);
    Rates { input: num("input"), output: num("output"), cache_read: num("cache_read"), cache_write: num("cache_write") }
}

fn catalog_cost(c: Option<&Value>) -> Cost {
    let tiers = c
        .and_then(|c| c.get("tiers"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|t| t.pointer("/tier/type").and_then(Value::as_str) == Some("context"))
        .filter_map(|t| Some((t.pointer("/tier/size")?.as_f64()?, rates(Some(t)))))
        .collect();
    Cost { base: rates(c), tiers, over_200k: c.and_then(|c| c.get("context_over_200k")).filter(|v| v.is_object()).map(|v| rates(Some(v))) }
}

#[derive(Debug, Clone)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub env: Vec<String>,
    pub key: Option<String>,
    pub options: Value,
    pub models: Vec<Model>,
    // Credentials come from an oauth entry in auth.json rather than a key.
    pub oauth: bool,
    // Handle of a plugin `fetch` every request of this provider goes through.
    pub fetch: Option<String>,
    // `${NAME}` substitutions for base URLs, applied before the environment.
    pub vars: Vec<(String, String)>,
    // Why the provider cannot be called although it is listed (missing resource name, project, ...).
    pub unavailable: Option<String>,
    active: bool,
}

fn text(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array).into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect()
}

pub fn load_catalog(paths: &Paths) -> Value {
    let file = env::var_os("OPENCODE_MODELS_PATH").map(Into::into).unwrap_or_else(|| paths.cache.join("models.json"));
    fs::read_to_string(file).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| json!({}))
}

fn referer(extra: &[(&str, &str)]) -> Value {
    let mut headers = Map::new();
    headers.insert("HTTP-Referer".into(), "https://opencode.ai/".into());
    headers.insert("X-Title".into(), "opencode".into());
    for (k, v) in extra {
        headers.insert((*k).into(), (*v).into());
    }
    json!({"headers": headers})
}

// Built-in per-provider loaders: (activate on its own, options to merge).
fn custom(provider: &mut Provider, stored: Option<&auth::Info>, configured: Option<&Value>) -> Option<(bool, Value)> {
    let has_auth = stored.is_some();
    let empty = json!({});
    let config_options = configured.and_then(|c| c.get("options")).unwrap_or(&empty);
    let api_key = match stored {
        Some(auth::Info::Api { key, .. }) => Some(key.clone()),
        _ => None,
    };
    let metadata = |name: &str| match stored {
        Some(auth::Info::Api { metadata: Some(m), .. }) => m.get(name).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string),
        _ => None,
    };
    Some(match provider.id.as_str() {
        bedrock::PROVIDER_ID => {
            let source = bedrock::Source::process();
            let profile = text(config_options.get("profile"));
            let config_key = text(config_options.get("apiKey")).is_some();
            if !bedrock::detected(&source, profile.as_deref(), api_key.is_some(), config_key) {
                return Some((false, json!({})));
            }
            let region = text(config_options.get("region")).or_else(|| env_var("AWS_REGION")).unwrap_or_else(|| bedrock::DEFAULT_REGION.into());
            let mut options = json!({"region": region});
            if let Some(endpoint) = text(config_options.get("endpoint")).or_else(|| text(config_options.get("baseURL"))) {
                options["baseURL"] = endpoint.into();
            }
            provider.vars = vec![("AWS_REGION".into(), region)];
            (true, options)
        }
        azure::PROVIDER_ID => {
            match azure::resource_name(config_options, stored, env_var(azure::RESOURCE_ENV)) {
                Some(resource) => {
                    provider.vars = vec![(azure::RESOURCE_ENV.into(), resource.clone())];
                    return Some((false, json!({"resourceName": resource})));
                }
                None if text(config_options.get("baseURL")).is_none() => {
                    provider.unavailable = Some("AZURE_RESOURCE_NAME is missing, set it using env var or reconnecting the azure provider and setting it".into());
                }
                None => {}
            }
            (false, json!({}))
        }
        "azure-cognitive-services" => match env_var("AZURE_COGNITIVE_SERVICES_RESOURCE_NAME") {
            Some(resource) => {
                let deployment_based = config_options.get("useDeploymentBasedUrls").and_then(Value::as_bool).unwrap_or(false);
                (false, json!({"baseURL": azure::cognitive_services_base_url(&resource, deployment_based)}))
            }
            None => (false, json!({})),
        },
        "google-vertex" => {
            let location = vertex::location(config_options, &env_var);
            match vertex::project(config_options, &env_var) {
                Some(project) => {
                    provider.vars = vertex::vars(&project, &location);
                    (true, json!({"project": project, "location": location}))
                }
                None => (false, json!({})),
            }
        }
        "google-vertex-anthropic" => {
            let location = vertex::anthropic_location(&env_var);
            match vertex::anthropic_project(&env_var) {
                Some(project) => (true, json!({"project": project, "location": location})),
                None => (false, json!({})),
            }
        }
        snowflake::PROVIDER_ID => {
            let oauth = match stored {
                Some(auth::Info::Oauth { access, account_id, .. }) => Some((access.clone(), account_id.clone())),
                _ => None,
            };
            let account = env_var("SNOWFLAKE_ACCOUNT").or_else(|| metadata("account")).or_else(|| oauth.as_ref().and_then(|o| o.1.clone())).or_else(|| text(config_options.get("account")));
            let env_token = env_var("SNOWFLAKE_CORTEX_TOKEN").or_else(|| env_var("SNOWFLAKE_CORTEX_PAT"));
            let config_token = text(config_options.get("token")).or_else(|| text(config_options.get("apiKey")));
            let token = env_token.clone().or_else(|| api_key.clone()).or_else(|| oauth.as_ref().map(|o| o.0.clone())).or_else(|| config_token.clone());
            let (Some(account), Some(token)) = (account.clone(), token.clone()) else {
                let missing: Vec<&str> = [(account.is_none(), "SNOWFLAKE_ACCOUNT"), (token.is_none(), "SNOWFLAKE_CORTEX_TOKEN")].iter().filter(|(m, _)| *m).map(|(_, n)| *n).collect();
                provider.unavailable = Some(format!(
                    "Snowflake Cortex: missing credentials ({}). Provide a bearer token (OAuth, JWT, or PAT) via env var, opencode auth, or provider options.",
                    missing.join(", ")
                ));
                return Some((false, json!({})));
            };
            // The login only supplies the request credential when nothing else overrides it.
            provider.oauth = oauth.is_some() && env_token.is_none() && api_key.is_none() && config_token.is_none();
            (configured.is_some(), json!({"baseURL": snowflake::base_url(&account), "apiKey": token}))
        }
        "cloudflare-workers-ai" => {
            if text(config_options.get("baseURL")).is_some() {
                return Some((false, json!({})));
            }
            if let Some(account) = env_var("CLOUDFLARE_ACCOUNT_ID").or_else(|| metadata("accountId")) {
                provider.vars = vec![("CLOUDFLARE_ACCOUNT_ID".into(), account)];
            }
            match env_var("CLOUDFLARE_API_KEY").or(api_key) {
                Some(key) => (true, json!({"apiKey": key})),
                None => (false, json!({})),
            }
        }
        "anthropic" => (false, json!({"headers": {"anthropic-beta": "interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14"}})),
        "opencode" => {
            let has_env = provider.env.iter().any(|name| env::var(name).is_ok_and(|v| !v.is_empty()));
            let has_config = configured.and_then(|c| c.pointer("/options/apiKey")).is_some_and(|v| !v.is_null() && v != "");
            let ok = has_env || has_auth || has_config;
            if !ok {
                provider.models.retain(|m| m.cost_input == 0.0);
            }
            (!provider.models.is_empty(), if ok { json!({}) } else { json!({"apiKey": "public"}) })
        }
        "llmgateway" => (false, referer(&[("X-Source", "opencode")])),
        "openrouter" | "zenmux" | "kilo" => (false, referer(&[])),
        "nvidia" => (configured.is_some(), referer(&[("X-BILLING-INVOKE-ORIGIN", "OpenCode")])),
        "vercel" => (false, json!({"headers": {"http-referer": "https://opencode.ai/", "x-title": "opencode"}})),
        "cerebras" => (false, json!({"headers": {"X-Cerebras-3rd-Party-Integration": "opencode"}})),
        _ => return None,
    })
}

fn catalog_model(provider_id: &str, key: &str, p: &Value, m: &Value) -> Model {
    let flag = |name: &str| m.get(name).and_then(Value::as_bool).unwrap_or(false);
    let mut model = Model {
        toolcall: m.get("tool_call").and_then(Value::as_bool).unwrap_or(true),
        input: modalities(m).unwrap_or_default(),
        interleaved: interleaved(m).flatten(),
        variants: Map::new(),
        endpoint: None,
        id: key.to_string(),
        provider_id: provider_id.to_string(),
        api_id: text(m.get("id")).unwrap_or_else(|| key.to_string()),
        api_url: text(m.pointer("/provider/api")).or_else(|| text(p.get("api"))).unwrap_or_default(),
        npm: text(m.pointer("/provider/npm")).or_else(|| text(p.get("npm"))).unwrap_or_else(|| DEFAULT_NPM.into()),
        name: text(m.get("name")).unwrap_or_else(|| key.to_string()),
        limit_output: m.pointer("/limit/output").and_then(Value::as_u64).unwrap_or(0),
        limit_context: m.pointer("/limit/context").and_then(Value::as_u64).unwrap_or(0),
        status: text(m.get("status")).unwrap_or_else(|| "active".into()),
        headers: Map::new(),
        options: json!({}),
        temperature: flag("temperature"),
        reasoning: flag("reasoning"),
        attachment: flag("attachment"),
        cost_input: m.pointer("/cost/input").and_then(Value::as_f64).unwrap_or(0.0),
        release_date: text(m.get("release_date")).unwrap_or_default(),
        limit_input: m.pointer("/limit/input").and_then(Value::as_u64).unwrap_or(0),
        family: text(m.get("family")).unwrap_or_default(),
        cost: catalog_cost(m.get("cost")),
    };
    model.variants = variants::reasoning_variants(m.get("reasoning_options"), &model).unwrap_or_else(|| variants::variants(&model));
    model
}

// A config model entry layered over the catalog model of the same key, if any.
fn config_model(provider_id: &str, key: &str, c: &Value, m: &Value, old: Option<&Model>, catalog_entry: Option<&Value>) -> Model {
    let flag = |name: &str| m.get(name).and_then(Value::as_bool);
    let price = |name: &str| m.get("cost").and_then(|c| c.get(name)).and_then(Value::as_f64);
    let mut headers = old.map(|o| o.headers.clone()).unwrap_or_default();
    headers.extend(m.get("headers").and_then(Value::as_object).cloned().unwrap_or_default());
    let mut options = old.map(|o| o.options.clone()).unwrap_or_else(|| json!({}));
    merge_deep(&mut options, m.get("options").filter(|v| v.is_object()).cloned().unwrap_or_else(|| json!({})));
    let mut model = Model {
        toolcall: m.get("tool_call").and_then(Value::as_bool).or(old.map(|o| o.toolcall)).unwrap_or(true),
        input: modalities(m).or(old.map(|o| o.input)).unwrap_or(Modalities { text: true, ..Default::default() }),
        interleaved: None,
        variants: Map::new(),
        endpoint: old.and_then(|o| o.endpoint.clone()),
        id: key.to_string(),
        provider_id: provider_id.to_string(),
        api_id: text(m.get("id")).or_else(|| old.map(|o| o.api_id.clone())).unwrap_or_else(|| key.to_string()),
        npm: text(m.pointer("/provider/npm"))
            .or_else(|| text(c.get("npm")))
            .or_else(|| old.map(|o| o.npm.clone()))
            .or_else(|| text(catalog_entry.and_then(|e| e.get("npm"))))
            .unwrap_or_else(|| DEFAULT_NPM.into()),
        api_url: text(m.pointer("/provider/api"))
            .or_else(|| text(c.get("api")))
            .or_else(|| old.map(|o| o.api_url.clone()).filter(|u| !u.is_empty()))
            .or_else(|| text(catalog_entry.and_then(|e| e.get("api"))))
            .unwrap_or_default(),
        name: text(m.get("name")).or_else(|| old.map(|o| o.name.clone())).unwrap_or_else(|| key.to_string()),
        limit_output: m.pointer("/limit/output").and_then(Value::as_u64).or(old.map(|o| o.limit_output)).unwrap_or(0),
        limit_context: m.pointer("/limit/context").and_then(Value::as_u64).or(old.map(|o| o.limit_context)).unwrap_or(0),
        status: text(m.get("status")).or_else(|| old.map(|o| o.status.clone())).unwrap_or_else(|| "active".into()),
        headers,
        options,
        temperature: flag("temperature").or(old.map(|o| o.temperature)).unwrap_or(false),
        reasoning: flag("reasoning").or(old.map(|o| o.reasoning)).unwrap_or(false),
        attachment: flag("attachment").or(old.map(|o| o.attachment)).unwrap_or(false),
        cost_input: m.pointer("/cost/input").and_then(Value::as_f64).or(old.map(|o| o.cost_input)).unwrap_or(0.0),
        release_date: text(m.get("release_date")).or_else(|| old.map(|o| o.release_date.clone())).unwrap_or_default(),
        limit_input: m.pointer("/limit/input").and_then(Value::as_u64).or(old.map(|o| o.limit_input)).unwrap_or(0),
        family: text(m.get("family")).or_else(|| old.map(|o| o.family.clone())).unwrap_or_default(),
        // A config entry rebuilds the price row from its own keys over the catalog's base row; tiers are not carried.
        cost: Cost {
            base: Rates {
                input: price("input").or(old.map(|o| o.cost.base.input)).unwrap_or(0.0),
                output: price("output").or(old.map(|o| o.cost.base.output)).unwrap_or(0.0),
                cache_read: price("cache_read").or(old.map(|o| o.cost.base.cache_read)).unwrap_or(0.0),
                cache_write: price("cache_write").or(old.map(|o| o.cost.base.cache_write)).unwrap_or(0.0),
            },
            tiers: Vec::new(),
            over_200k: None,
        },
    };
    model.interleaved = match (interleaved(m), old) {
        (Some(field), _) => field,
        (None, Some(old)) => old.interleaved.clone(),
        (None, None) => (model.npm == DEFAULT_NPM && model.api_id.contains("deepseek")).then(|| "reasoning_content".to_string()),
    };
    // Configured reasoning options replace the catalog variants; configured variants still apply on top.
    let computed = if m.get("reasoning_options").is_some() {
        variants::reasoning_variants(m.get("reasoning_options"), &model).unwrap_or_else(|| variants::variants(&model))
    } else {
        match old.filter(|o| o.npm == model.npm) {
            Some(old) => old.variants.clone(),
            // Rust build, from v2.2.7 (`config/plugin/provider.ts`, `variant.ts`): a configured model that says
            // nothing about reasoning gets its SDK's default effort levels instead of none.
            None if m.get("reasoning").is_none() && variants::variants(&model).is_empty() => variants::default_efforts(&model),
            None => variants::variants(&model),
        }
    };
    model.variants = variants::merge_config(&computed, m.get("variants"));
    model
}

// The catalog's models of one provider entry.
pub fn catalog_models(provider_id: &str, entry: &Value) -> Vec<Model> {
    entry["models"].as_object().into_iter().flatten().map(|(key, m)| catalog_model(provider_id, key, entry, m)).collect()
}

fn rates_info(rates: &Rates) -> Value {
    json!({"input": rates.input, "output": rates.output, "cache": {"read": rates.cache_read, "write": rates.cache_write}})
}

// A model in the `Provider.Model` JSON shape plugins see.
pub fn model_info(model: &Model) -> Value {
    let mut api = json!({"id": model.api_id, "url": model.api_url, "npm": model.npm});
    if let Some(endpoint) = &model.endpoint {
        api["endpoint"] = endpoint.clone().into();
    }
    let mut cost = rates_info(&model.cost.base);
    if !model.cost.tiers.is_empty() {
        cost["tiers"] = model.cost.tiers.iter().map(|(size, rates)| {
            let mut tier = rates_info(rates);
            tier["tier"] = json!({"type": "context", "size": size});
            tier
        }).collect();
    }
    if let Some(over) = &model.cost.over_200k {
        cost["experimentalOver200K"] = rates_info(over);
    }
    let mut limit = json!({"context": model.limit_context, "output": model.limit_output});
    if model.limit_input > 0 {
        limit["input"] = model.limit_input.into();
    }
    let input = model.input;
    json!({
        "id": model.id, "providerID": model.provider_id, "api": api, "name": model.name, "family": model.family,
        "capabilities": {
            "temperature": model.temperature, "reasoning": model.reasoning, "attachment": model.attachment, "toolcall": model.toolcall,
            "input": {"text": input.text, "audio": input.audio, "image": input.image, "video": input.video, "pdf": input.pdf},
            "output": {"text": true, "audio": false, "image": false, "video": false, "pdf": false},
            "interleaved": match &model.interleaved { Some(field) => json!({"field": field}), None => Value::Bool(false) }
        },
        "cost": cost, "limit": limit, "status": model.status, "options": model.options, "headers": model.headers,
        "release_date": model.release_date, "variants": model.variants
    })
}

// A provider in the JSON shape plugin hooks receive.
pub fn info(provider_id: &str, entry: &Value, models: &[Model]) -> Value {
    let models: Map<String, Value> = models.iter().map(|m| (m.id.clone(), model_info(m))).collect();
    json!({"id": provider_id, "name": text(entry.get("name")).unwrap_or_else(|| provider_id.to_string()), "source": "custom", "env": strings(entry.get("env")), "options": {}, "models": models})
}

// A model from the `Provider.Model` JSON shape (a plugin's `provider.models` result, or built-in discovery).
pub fn model_from_info(provider_id: &str, key: &str, m: &Value) -> Model {
    let flag = |pointer: &str, default: bool| m.pointer(pointer).and_then(Value::as_bool).unwrap_or(default);
    let number = |pointer: &str| m.pointer(pointer).and_then(Value::as_f64).filter(|v| v.is_finite()).unwrap_or(0.0);
    let whole = |pointer: &str| m.pointer(pointer).and_then(Value::as_f64).filter(|v| *v >= 0.0).map_or(0, |v| v as u64);
    let rates = |c: &Value| Rates {
        input: c.get("input").and_then(Value::as_f64).unwrap_or(0.0),
        output: c.get("output").and_then(Value::as_f64).unwrap_or(0.0),
        cache_read: c.pointer("/cache/read").and_then(Value::as_f64).unwrap_or(0.0),
        cache_write: c.pointer("/cache/write").and_then(Value::as_f64).unwrap_or(0.0),
    };
    let cost = m.get("cost").cloned().unwrap_or_else(|| json!({}));
    let tiers = cost.get("tiers").and_then(Value::as_array).into_iter().flatten().filter_map(|t| Some((t.pointer("/tier/size")?.as_f64()?, rates(t)))).collect();
    let mut model = Model {
        id: key.to_string(),
        provider_id: provider_id.to_string(),
        api_id: text(m.pointer("/api/id")).unwrap_or_else(|| key.to_string()),
        api_url: text(m.pointer("/api/url")).unwrap_or_default(),
        npm: text(m.pointer("/api/npm")).unwrap_or_else(|| DEFAULT_NPM.into()),
        name: text(m.get("name")).unwrap_or_else(|| key.to_string()),
        limit_output: whole("/limit/output"),
        limit_context: whole("/limit/context"),
        status: text(m.get("status")).unwrap_or_else(|| "active".into()),
        headers: m.get("headers").and_then(Value::as_object).cloned().unwrap_or_default(),
        options: m.get("options").filter(|v| v.is_object()).cloned().unwrap_or_else(|| json!({})),
        temperature: flag("/capabilities/temperature", false),
        reasoning: flag("/capabilities/reasoning", false),
        attachment: flag("/capabilities/attachment", false),
        cost_input: number("/cost/input"),
        release_date: text(m.get("release_date")).unwrap_or_default(),
        limit_input: whole("/limit/input"),
        family: text(m.get("family")).unwrap_or_default(),
        cost: Cost { base: rates(&cost), tiers, over_200k: cost.get("experimentalOver200K").filter(|v| v.is_object()).map(rates) },
        toolcall: flag("/capabilities/toolcall", true),
        input: Modalities {
            text: flag("/capabilities/input/text", true),
            audio: flag("/capabilities/input/audio", false),
            image: flag("/capabilities/input/image", false),
            video: flag("/capabilities/input/video", false),
            pdf: flag("/capabilities/input/pdf", false),
        },
        interleaved: text(m.pointer("/capabilities/interleaved/field")),
        variants: Map::new(),
        endpoint: text(m.pointer("/api/endpoint")),
    };
    model.variants = match m.get("variants").and_then(Value::as_object) {
        Some(given) => given.clone(),
        None => variants::variants(&model),
    };
    model
}

pub fn build(catalog: &Value, config: &Value, auth: &auth::Entries) -> Vec<Provider> {
    build_with(catalog, config, auth, &Overrides::default())
}

// `build`, with plugin-provided model lists, auth-loader options and fetch handles applied where the original applies them.
pub fn build_with(catalog: &Value, config: &Value, auth: &auth::Entries, overrides: &Overrides) -> Vec<Provider> {
    let blank = |id: &str, name: String, env: Vec<String>, models: Vec<Model>| Provider {
        id: id.to_string(),
        name,
        env,
        key: None,
        options: json!({}),
        models,
        oauth: false,
        fetch: None,
        vars: Vec::new(),
        unavailable: None,
        active: false,
    };
    let mut providers: Vec<Provider> = Vec::new();
    for (id, p) in catalog.as_object().into_iter().flatten() {
        providers.push(blank(id, text(p.get("name")).unwrap_or_else(|| id.clone()), strings(p.get("env")), catalog_models(id, p)));
    }

    let disabled = strings(config.get("disabled_providers"));
    // A plugin's model list replaces the catalog's before config is layered on.
    for provider in &mut providers {
        if let Some(models) = overrides.models_for(&provider.id).filter(|_| !disabled.contains(&provider.id)) {
            provider.models = models.clone();
        }
    }
    let enabled = config.get("enabled_providers").filter(|v| v.is_array()).map(|v| strings(Some(v)));
    let stored = |id: &str| auth.iter().find(|(key, _)| key == id).map(|(_, info)| info);

    // A ChatGPT login only reaches the models its subscription serves.
    if let Some(auth::Info::Oauth { .. }) = stored(codex::PROVIDER_ID)
        && !disabled.iter().any(|d| d == codex::PROVIDER_ID)
        && let Some(openai) = providers.iter_mut().find(|p| p.id == codex::PROVIDER_ID)
    {
        openai.models.retain(|m| codex::allows_model(&m.api_id));
        for model in &mut openai.models {
            model.cost_input = 0.0;
            model.cost = Cost::default();
            if model.id.contains("gpt-5.5") || model.id.contains("gpt-5.6") {
                model.limit_context = 400_000;
                model.limit_output = 128_000;
            }
        }
    }

    // Every Copilot model goes through the Copilot SDK, at the host the login belongs to.
    if !disabled.iter().any(|d| d == copilot::PROVIDER_ID)
        && overrides.models_for(copilot::PROVIDER_ID).is_none()
        && let Some(provider) = providers.iter_mut().find(|p| p.id == copilot::PROVIDER_ID)
    {
        let enterprise = match stored(copilot::PROVIDER_ID) {
            Some(auth::Info::Oauth { enterprise_url, .. }) => enterprise_url.clone(),
            _ => None,
        };
        for model in &mut provider.models {
            model.api_url = copilot::base(enterprise.as_deref());
            model.npm = copilot::NPM.into();
        }
    }

    let configured = config.get("provider").and_then(Value::as_object);
    for (id, c) in configured.into_iter().flatten() {
        let catalog_entry = catalog.get(id);
        if !providers.iter().any(|p| &p.id == id) {
            providers.push(blank(id, id.clone(), Vec::new(), Vec::new()));
        }
        let provider = providers.iter_mut().find(|p| &p.id == id).unwrap();
        for (key, m) in c.get("models").and_then(Value::as_object).into_iter().flatten() {
            let existing = provider.models.iter().position(|x| &x.id == key);
            let model = config_model(id, key, c, m, existing.map(|i| &provider.models[i]), catalog_entry);
            match existing {
                Some(i) => provider.models[i] = model,
                None => provider.models.push(model),
            }
        }
    }

    let experimental = env::var("OPENCODE_ENABLE_EXPERIMENTAL_MODELS").is_ok_and(|v| matches!(v.to_lowercase().as_str(), "true" | "1"));
    for provider in &mut providers {
        if disabled.contains(&provider.id) {
            continue;
        }
        let config_entry = configured.and_then(|c| c.get(&provider.id));
        if let Some(value) = provider.env.iter().find_map(|name| env::var(name).ok().filter(|v| !v.is_empty())) {
            provider.active = true;
            if provider.env.len() == 1 {
                provider.key = Some(value);
            }
        }
        match stored(&provider.id) {
            Some(auth::Info::Api { key, .. }) => {
                provider.active = true;
                provider.key = Some(key.clone());
            }
            Some(auth::Info::Oauth { .. }) if OAUTH_PROVIDERS.contains(&provider.id.as_str()) => {
                provider.active = true;
                provider.oauth = true;
                if provider.id == codex::PROVIDER_ID {
                    merge_deep(&mut provider.options, json!({"apiKey": auth::OAUTH_DUMMY_KEY}));
                }
            }
            _ => {}
        }
        // A plugin auth loader's options activate the provider its stored entry belongs to.
        if stored(&provider.id).is_some() {
            for (_, options) in overrides.options.iter().filter(|(id, _)| id == &provider.id) {
                provider.active = true;
                merge_deep(&mut provider.options, options.clone());
            }
            if let Some((_, handle)) = overrides.fetch.iter().rev().find(|(id, _)| id == &provider.id) {
                provider.active = true;
                provider.fetch = Some(handle.clone());
            }
        }
        // Built-in loaders run before config is re-applied, so config options win over theirs.
        let entry = stored(&provider.id);
        if let Some((autoload, options)) = custom(provider, entry, config_entry)
            && (autoload || provider.active)
        {
            provider.active = true;
            merge_deep(&mut provider.options, options);
        }
        if let Some(c) = config_entry {
            provider.active = true;
            if let Some(name) = text(c.get("name")) {
                provider.name = name;
            }
            let extra = strings(c.get("env"));
            if !extra.is_empty() {
                provider.env = extra;
            }
            merge_deep(&mut provider.options, c.get("options").cloned().unwrap_or_else(|| json!({})));
        }
        if enabled.as_ref().is_some_and(|allow| !allow.contains(&provider.id)) {
            provider.active = false;
        }
        let list = |name: &str| config_entry.and_then(|c| c.get(name)).filter(|v| v.is_array()).map(|v| strings(Some(v)));
        let (blacklist, whitelist) = (list("blacklist"), list("whitelist"));
        let chat_alias = matches!(provider.id.as_str(), "openai" | "github-copilot" | "openrouter");
        let openrouter = provider.id == "openrouter";
        provider.models.retain(|m| {
            m.status != "deprecated"
                && (m.status != "alpha" || experimental)
                && !(chat_alias && m.id == "gpt-5-chat-latest")
                && !(openrouter && m.id == "openai/gpt-5-chat")
                && !blacklist.as_ref().is_some_and(|list| list.contains(&m.id))
                && !whitelist.as_ref().is_some_and(|list| !list.contains(&m.id))
        });
    }
    providers.retain(|p| p.active && !p.models.is_empty());
    providers
}

// "p/m/x" -> ("p", "m/x")
pub fn parse_model(spec: &str) -> Option<(&str, &str)> {
    spec.split_once('/')
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub url: String,
    pub api_key: Option<String>,
    pub headers: Vec<(String, String)>,
    pub max_tokens: u64,
    pub protocol: Protocol,
    pub provider_id: String,
    // Provider options and cache policy applied when a request leaves `Request::options` null.
    pub options: Value,
    pub cache: bool,
    // Set when the request authenticates with a ChatGPT login instead of `api_key`.
    pub codex: Option<codex::Config>,
    // Set when the request authenticates with a GitHub Copilot login.
    pub copilot: Option<copilot::Config>,
    // The model behind the target; enables prompt and tool-schema transforms.
    pub model: Option<Model>,
    pub timeouts: Timeouts,
    // Chat only: ask for a usage chunk (`stream_options.include_usage`).
    pub include_usage: bool,
    // A credential minted per request: SigV4 keys, a cloud access token, or a refreshed OAuth login.
    pub credential: Option<Credential>,
    // Handle of a plugin `fetch` the request goes through (see `hooks::AuthLoader`).
    pub fetch: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Credential {
    Bedrock(bedrock::Config),
    Vertex(vertex::Config),
    Azure(azure::Config),
    Xai(xai::Config),
    Snowflake(snowflake::Config),
}

// Milliseconds; None disables the timer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Timeouts {
    // Until response headers arrive.
    pub header: Option<u64>,
    // Between two body chunks of an event stream.
    pub chunk: Option<u64>,
    // Whole request.
    pub total: Option<u64>,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts { header: Some(DEFAULT_TIMEOUT_MS), chunk: Some(DEFAULT_TIMEOUT_MS), total: None }
    }
}

impl Timeouts {
    // `headerTimeout` / `chunkTimeout` / `timeout` provider options; `false` disables a timer.
    pub fn from_options(options: &Value) -> Self {
        let number = |key: &str| options.get(key).and_then(Value::as_f64).filter(|v| *v >= 0.0).map(|v| v as u64);
        let header = match options.get("headerTimeout") {
            Some(Value::Bool(false)) => None,
            Some(Value::Number(_)) => number("headerTimeout"),
            _ => Some(DEFAULT_TIMEOUT_MS),
        };
        let chunk = match options.get("chunkTimeout") {
            None | Some(Value::Null) => Some(DEFAULT_TIMEOUT_MS),
            _ => number("chunkTimeout").filter(|v| *v > 0),
        };
        Timeouts { header, chunk, total: number("timeout") }
    }
}

fn expand_env(input: &str) -> String {
    let mut out = String::new();
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        let Some(end) = rest[start..].find('}') else { break };
        let name = &rest[start + 2..start + end];
        out.push_str(&rest[..start]);
        match env::var(name) {
            Ok(v) => out.push_str(&v),
            Err(_) => out.push_str(&rest[start..=start + end]),
        }
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    out
}

// `${NAME}` from the provider's loader variables, then from the environment; unknown names stay literal.
fn expand(input: &str, vars: &[(String, String)]) -> String {
    let mut out = input.to_string();
    for (name, value) in vars {
        out = out.replace(&format!("${{{name}}}"), value);
    }
    expand_env(&out)
}

fn unsupported(provider: &Provider, model: &Model, npm: &str) -> Error {
    let detail = dialect::unsupported(npm).map(|what| format!(" ({what})")).unwrap_or_default();
    Error::Config(format!("{}/{}: not supported yet: {npm}{detail}", provider.id, model.id))
}

// Where and how one model is called: (url, protocol, credential, api key).
struct Route {
    url: String,
    protocol: Protocol,
    credential: Option<Credential>,
    api_key: Option<String>,
}

fn route(provider: &Provider, model: &Model, protocol: Protocol, configured_key: Option<String>) -> Result<Route, Error> {
    let npm = model.npm.as_str();
    let options = &provider.options;
    let configured_base = text(options.get("baseURL")).map(|b| expand(&b, &provider.vars));
    let plain = |base: String, path: String, api_key: Option<String>| Route { url: format!("{}{path}", base.trim_end_matches('/')), protocol, credential: None, api_key };
    let generic_path = |protocol: Protocol| match protocol {
        Protocol::Chat => "/chat/completions".to_string(),
        Protocol::Anthropic => anthropic::PATH.to_string(),
        Protocol::Responses => responses::PATH.to_string(),
        Protocol::Gemini => gemini::path(&model.api_id),
        Protocol::Bedrock => bedrock::path(&model.api_id),
    };
    match npm {
        bedrock::NPM | bedrock::MANTLE_NPM => {
            let region = text(options.get("region")).or_else(|| env_var("AWS_REGION")).unwrap_or_else(|| bedrock::DEFAULT_REGION.into());
            let mantle = npm == bedrock::MANTLE_NPM;
            let config = bedrock::Config {
                profile: text(options.get("profile")),
                bearer: configured_key,
                service: if mantle { bedrock::MANTLE_SERVICE } else { bedrock::SERVICE },
                time: None,
                region: region.clone(),
            };
            let (base, path) = if mantle {
                (configured_base.unwrap_or_else(|| bedrock::mantle_base_url(&region)), generic_path(protocol))
            } else {
                (configured_base.unwrap_or_else(|| bedrock::base_url(&region)), bedrock::path(&bedrock::model_id(&model.api_id, &region)))
            };
            Ok(Route { credential: Some(Credential::Bedrock(config)), ..plain(base, path, None) })
        }
        azure::NPM => {
            let prefix = match (&configured_base, text(options.get("resourceName"))) {
                (Some(base), _) => base.clone(),
                (None, Some(resource)) => azure::base_url(&resource),
                (None, None) => return Err(Error::Config(provider.unavailable.clone().unwrap_or_else(|| "AZURE_RESOURCE_NAME is missing".into()))),
            };
            let flag = |key: &str| options.get(key).is_some_and(|v| v.as_bool() != Some(false) && !v.is_null());
            let protocol = if flag("useCompletionUrls") { Protocol::Chat } else { protocol };
            let version = text(options.get("apiVersion")).unwrap_or_else(|| azure::DEFAULT_API_VERSION.into());
            let url = azure::url(&prefix, configured_base.is_some(), flag("useDeploymentBasedUrls"), &model.api_id, &version, &generic_path(protocol));
            let entra = provider.oauth && provider.id == azure::PROVIDER_ID;
            Ok(Route { url, protocol, credential: entra.then(|| Credential::Azure(azure::Config::default())), api_key: if entra { None } else { configured_key } })
        }
        vertex::NPM => {
            if let Some(key) = configured_key {
                let base = configured_base.unwrap_or_else(|| vertex::EXPRESS_BASE_URL.into());
                return Ok(plain(base, vertex::gemini_path(&model.api_id), Some(key)));
            }
            let Some(project) = text(options.get("project")) else {
                return Err(Error::Config("google-vertex needs a project: set GOOGLE_VERTEX_PROJECT (or GOOGLE_CLOUD_PROJECT) or the `project` option".into()));
            };
            let location = text(options.get("location")).unwrap_or_else(|| "us-central1".into());
            let base = configured_base.unwrap_or_else(|| vertex::base_url(&project, &location));
            Ok(Route { credential: Some(Credential::Vertex(vertex::Config::default())), ..plain(base, vertex::gemini_path(model.api_id.trim()), None) })
        }
        vertex::ANTHROPIC_NPM => {
            let base = match (configured_base, text(options.get("project"))) {
                (Some(base), _) => base,
                (None, Some(project)) => vertex::anthropic_base_url(&project, &text(options.get("location")).unwrap_or_else(|| "global".into())),
                (None, None) => return Err(Error::Config(format!("{} needs a project: set GOOGLE_CLOUD_PROJECT or the `project` option", provider.id))),
            };
            Ok(Route { url: vertex::anthropic_url(&base, model.api_id.trim()), protocol, credential: Some(Credential::Vertex(vertex::Config::default())), api_key: None })
        }
        AIHUBMIX_NPM => {
            let root = configured_base.unwrap_or_else(|| AIHUBMIX_ROOT.into());
            let base = match protocol {
                Protocol::Gemini => format!("{}/gemini/v1beta", root.trim_end_matches('/')),
                _ => format!("{}/v1", root.trim_end_matches('/')),
            };
            Ok(plain(base, generic_path(protocol), configured_key))
        }
        _ => {
            let base = configured_base
                .or_else(|| Some(expand(&model.api_url, &provider.vars)).filter(|u| !u.is_empty()))
                .or_else(|| default_base(npm).map(str::to_string));
            let Some(base) = base else {
                return Err(Error::Config(format!("{}/{} has no base URL", provider.id, model.id)));
            };
            // Vertex partner models are OpenAI-compatible endpoints behind a Google access token.
            let vertex_token = provider.id == "google-vertex" && npm.contains("@ai-sdk/openai-compatible");
            let credential = match provider.id.as_str() {
                xai::PROVIDER_ID if provider.oauth => Some(Credential::Xai(xai::Config::default())),
                snowflake::PROVIDER_ID if provider.oauth => Some(Credential::Snowflake(snowflake::Config::default())),
                azure::PROVIDER_ID if provider.oauth => Some(Credential::Azure(azure::Config::default())),
                _ if vertex_token => Some(Credential::Vertex(vertex::Config::default())),
                _ => None,
            };
            let api_key = if credential.is_some() { None } else { configured_key.or_else(|| (npm == "@qvac/ai-sdk-provider").then(|| "qvac".to_string())) };
            Ok(Route { credential, ..plain(base, generic_path(protocol), api_key) })
        }
    }
}

pub fn resolve(provider: &Provider, model: &Model) -> Result<Resolved, Error> {
    // Every model of the Cloudflare gateway goes through its wrapping client, whatever package the model names.
    if provider.id == "cloudflare-ai-gateway" {
        return Err(unsupported(provider, model, "ai-gateway-provider"));
    }
    let Some(protocol) = protocol_for(model) else {
        return Err(unsupported(provider, model, &model.npm));
    };
    if let Some(reason) = provider.unavailable.as_ref().filter(|_| text(provider.options.get("baseURL")).is_none()) {
        return Err(Error::Config(reason.clone()));
    }
    let codex = provider.oauth && provider.id == codex::PROVIDER_ID;
    let copilot = provider.oauth && provider.id == copilot::PROVIDER_ID;
    let mut configured_key = text(provider.options.get("apiKey")).or_else(|| provider.key.clone());
    if configured_key.is_none() && provider.fetch.is_none() {
        configured_key = sdk_key_env(&model.npm).iter().find_map(|name| env_var(name));
    }
    let Route { url, protocol, credential, api_key } = route(provider, model, protocol, configured_key)?;
    if provider.oauth && !codex && !copilot && credential.is_none() && provider.fetch.is_none() {
        return Err(Error::Config(format!("{} is logged in with OAuth, which needs a plugin auth loader this build does not have", provider.id)));
    }
    let mut headers: Vec<(String, String)> = Vec::new();
    if model.npm == AIHUBMIX_NPM {
        headers.push(("APP-Code".into(), AIHUBMIX_APP_CODE.into()));
    }
    for source in [provider.options.get("headers").and_then(Value::as_object), Some(&model.headers)] {
        for (k, v) in source.into_iter().flatten() {
            if let Some(v) = v.as_str() {
                headers.retain(|(name, _)| !name.eq_ignore_ascii_case(k));
                headers.push((k.clone(), v.to_string()));
            }
        }
    }
    let mut options = shape::options(provider, model, None);
    shape::finalize_options(model, &mut options);
    // Only the generic package makes usage reporting optional; each wrapper fixes its own choice.
    let include_usage = match dialect::wrapper(&model.npm) {
        Some(wrapper) => wrapper.include_usage,
        None => model.npm != copilot::NPM && model.npm != "@ai-sdk/xai" && provider.options.get("includeUsage") != Some(&Value::Bool(false)),
    };
    Ok(Resolved {
        url,
        credential,
        fetch: provider.fetch.clone(),
        api_key: if codex || copilot { None } else { api_key },
        headers,
        max_tokens: shape::max_output_tokens(model, OUTPUT_TOKEN_MAX),
        protocol,
        provider_id: provider.id.clone(),
        cache: shape::caches(model, &options),
        options,
        codex: codex.then(codex::Config::default),
        copilot: copilot.then(copilot::Config::default),
        model: Some(model.clone()),
        timeouts: Timeouts::from_options(&provider.options),
        include_usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oauth() -> auth::Info {
        auth::Info::Oauth { refresh: "r".into(), access: "a".into(), expires: 0, account_id: Some("acct".into()), enterprise_url: None }
    }

    #[test]
    fn cost_rows_follow_catalog_tiers_and_config_overrides() {
        let catalog = json!({"p": {"id": "p", "env": [], "models": {"m": {"family": "gpt-nano", "limit": {"context": 9, "input": 7, "output": 2}, "cost": {
            "input": 1, "output": 2, "cache_read": 0.1,
            "tiers": [
                {"input": 3, "output": 4, "tier": {"type": "context", "size": 100}},
                {"input": 5, "output": 6, "cache_write": 7, "tier": {"type": "context", "size": 500}}
            ],
            "context_over_200k": {"input": 9, "output": 10}
        }}, "plain": {"cost": {"input": 1, "output": 2, "context_over_200k": {"input": 9, "output": 10}}}}}});
        let auth = vec![("p".to_string(), auth::Info::Api { key: "k".into(), metadata: None })];
        let providers = build(&catalog, &json!({}), &auth);
        let model = &providers[0].models[0];
        assert_eq!((model.limit_input, model.family.as_str(), model.cost_input), (7, "gpt-nano", 1.0));
        assert_eq!(model.cost.rates(100.0), Rates { input: 1.0, output: 2.0, cache_read: 0.1, cache_write: 0.0 });
        assert_eq!(model.cost.rates(101.0).input, 3.0);
        assert_eq!(model.cost.rates(300_000.0), Rates { input: 5.0, output: 6.0, cache_read: 0.0, cache_write: 7.0 });
        let plain = &providers[0].models[1];
        assert_eq!((plain.cost.rates(200_000.0).input, plain.cost.rates(200_001.0).input), (1.0, 9.0));

        // A config entry keeps unset prices of the catalog row but drops its tiers.
        let config = json!({"provider": {"p": {"models": {"m": {"cost": {"output": 8}}}}}});
        let model = build(&catalog, &config, &auth).remove(0).models.remove(0);
        assert_eq!(model.cost, Cost { base: Rates { input: 1.0, output: 8.0, cache_read: 0.1, cache_write: 0.0 }, tiers: Vec::new(), over_200k: None });
        assert_eq!((model.limit_input, model.family.as_str()), (7, "gpt-nano"));
    }

    #[test]
    fn builds_from_catalog_config_and_auth() {
        let catalog = json!({
            "deepseek": {"id": "deepseek", "name": "DeepSeek", "env": ["OC_TEST_UNSET_KEY"], "npm": "@ai-sdk/openai-compatible", "api": "https://api.deepseek.com",
                "models": {"a": {"id": "a", "name": "A", "limit": {"context": 1, "output": 8000}}, "old": {"id": "old", "status": "deprecated"}}},
            "anthropic": {"id": "anthropic", "name": "Anthropic", "env": ["OC_TEST_UNSET_KEY2"], "npm": "@ai-sdk/anthropic", "models": {"c": {"id": "c"}}},
            "idle": {"id": "idle", "env": [], "models": {"x": {}}},
            "exotic": {"id": "exotic", "env": [], "npm": "@ai-sdk/gateway", "models": {"g": {}}}
        });
        let config = json!({"provider": {
            "deepseek": {"name": "Mine", "options": {"baseURL": "https://gw.example/v1/", "apiKey": "cfg"}, "models": {"b": {"name": "B", "options": {"x": 1}, "temperature": true}}},
            "empty": {"options": {}}
        }});
        let auth = vec![
            ("anthropic".to_string(), auth::Info::Api { key: "sk".into(), metadata: None }),
            ("exotic".to_string(), auth::Info::Api { key: "k".into(), metadata: None }),
            ("not-in-catalog".to_string(), auth::Info::Api { key: "k".into(), metadata: None }),
        ];
        let providers = build(&catalog, &config, &auth);
        let ids: Vec<&str> = providers.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["deepseek", "anthropic", "exotic"]);
        let deepseek = &providers[0];
        assert_eq!(deepseek.name, "Mine");
        assert_eq!(deepseek.models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!((&deepseek.models[1].options, deepseek.models[1].temperature), (&json!({"x": 1}), true));
        // v2: a custom OpenAI-compatible model that says nothing about reasoning gets low/medium/high efforts.
        assert_eq!(deepseek.models[1].variants.keys().collect::<Vec<_>>(), ["low", "medium", "high"]);
        assert_eq!(deepseek.models[1].variants["high"], json!({"reasoningEffort": "high"}));
        let config = json!({"provider": {"deepseek": {"models": {"b": {"reasoning": false}}}}});
        assert!(build(&catalog, &config, &auth)[0].models[1].variants.is_empty(), "an explicit `reasoning: false` keeps none");
        let r = resolve(deepseek, &deepseek.models[0]).unwrap();
        assert_eq!(r.url, "https://gw.example/v1/chat/completions");
        assert_eq!(r.api_key.as_deref(), Some("cfg"));
        assert_eq!((r.max_tokens, r.protocol), (8000, Protocol::Chat));
        assert_eq!(resolve(deepseek, &deepseek.models[1]).unwrap().max_tokens, OUTPUT_TOKEN_MAX);

        let anthropic = resolve(&providers[1], &providers[1].models[0]).unwrap();
        assert_eq!((anthropic.url.as_str(), anthropic.protocol, anthropic.api_key.as_deref()), ("https://api.anthropic.com/v1/messages", Protocol::Anthropic, Some("sk")));
        assert_eq!(anthropic.headers, [("anthropic-beta".to_string(), "interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14".to_string())]);
        assert!(anthropic.cache);
        let error = resolve(&providers[2], &providers[2].models[0]).unwrap_err();
        assert!(matches!(&error, Error::Config(m) if m.contains("not supported yet: @ai-sdk/gateway")), "{error}");
    }

    #[test]
    fn oauth_login_activates_openai_with_subscription_models() {
        let catalog = json!({
            "openai": {"id": "openai", "name": "OpenAI", "env": ["OC_TEST_UNSET_KEY3"], "npm": "@ai-sdk/openai", "models": {
                "gpt-5.5": {"id": "gpt-5.5", "cost": {"input": 5}, "limit": {"context": 1050000, "output": 128000}},
                "gpt-5.5-pro": {"id": "gpt-5.5-pro"}, "gpt-5.4-mini": {"id": "gpt-5.4-mini", "limit": {"context": 9, "output": 9}},
                "gpt-4o": {"id": "gpt-4o"}, "gpt-6-sol": {"id": "gpt-6-sol"}, "gpt-5.6": {"id": "gpt-5.6"}
            }},
            "github-copilot": {"id": "github-copilot", "env": ["OC_TEST_UNSET_KEY4"], "npm": "@ai-sdk/openai-compatible", "api": "https://api.githubcopilot.com", "models": {"gpt-4.1": {}}},
            "deepseek": {"id": "deepseek", "env": ["OC_TEST_UNSET_KEY"], "api": "https://api.deepseek.com", "models": {"a": {}}}
        });
        let auth = vec![("openai".to_string(), oauth()), ("github-copilot".to_string(), oauth()), ("deepseek".to_string(), oauth())];
        let providers = build(&catalog, &json!({}), &auth);
        assert_eq!(providers.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["openai", "github-copilot"]);
        let openai = &providers[0];
        assert!(openai.oauth && openai.key.is_none());
        assert_eq!(openai.models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["gpt-5.5", "gpt-5.4-mini", "gpt-6-sol"]);
        assert_eq!((openai.models[0].limit_context, openai.models[0].cost_input, openai.models[1].limit_context), (400_000, 0.0, 9));
        let target = resolve(openai, &openai.models[0]).unwrap();
        assert_eq!((target.url.as_str(), target.protocol, &target.api_key), ("https://api.openai.com/v1/responses", Protocol::Responses, &None));
        assert!(target.codex.is_some());
        assert_eq!(target.options["store"], json!(false));
        let copilot = resolve(&providers[1], &providers[1].models[0]).unwrap();
        assert_eq!((copilot.url.as_str(), copilot.protocol, &copilot.api_key), ("https://api.githubcopilot.com/chat/completions", Protocol::Chat, &None));
        assert!(copilot.copilot.is_some() && copilot.codex.is_none() && !copilot.include_usage);
        let xai = build(&json!({"xai": {"id": "xai", "env": [], "npm": "@ai-sdk/xai", "models": {"grok": {}}}}), &json!({}), &vec![("xai".to_string(), oauth())]);
        let grok = resolve(&xai[0], &xai[0].models[0]).unwrap();
        assert!(matches!(&grok.credential, Some(Credential::Xai(_))) && grok.api_key.is_none());
        assert_eq!((grok.url.as_str(), grok.protocol), ("https://api.x.ai/v1/responses", Protocol::Responses));

        let with_key = build(&catalog, &json!({}), &vec![("openai".to_string(), auth::Info::Api { key: "sk".into(), metadata: None })]);
        assert_eq!(with_key[0].models.len(), 6);
        assert!(resolve(&with_key[0], &with_key[0].models[0]).unwrap().codex.is_none());
        assert!(build(&catalog, &json!({"disabled_providers": ["openai"]}), &auth).iter().all(|p| p.id != "openai"));
        assert_eq!(build(&catalog, &json!({"enabled_providers": ["github-copilot"]}), &auth).len(), 1);
    }

    #[test]
    fn builtin_loaders_and_model_filters() {
        let catalog = json!({
            "opencode": {"id": "opencode", "env": ["OC_TEST_UNSET_KEY5"], "api": "https://opencode.ai/inference/openai/v1", "models": {
                "free": {"cost": {"input": 0}}, "paid": {"cost": {"input": 1}}, "claude": {"cost": {"input": 0}, "provider": {"npm": "@ai-sdk/anthropic", "api": "https://opencode.ai/inference/anthropic/v1"}}
            }},
            "openrouter": {"id": "openrouter", "env": ["OC_TEST_UNSET_KEY6"], "npm": "@openrouter/ai-sdk-provider", "models": {"openai/gpt-5-chat": {}, "gpt-5-chat-latest": {}, "x/y": {}, "x/z": {}}},
            "groq": {"id": "groq", "env": ["OC_TEST_UNSET_KEY7"], "npm": "@ai-sdk/groq", "models": {"m": {}}}
        });
        let config = json!({"provider": {"openrouter": {"options": {"apiKey": "k", "headers": {"X-Title": "mine"}}, "blacklist": ["x/z"]}, "groq": {"options": {"apiKey": "g"}}}});
        let providers = build(&catalog, &config, &Vec::new());
        assert_eq!(providers.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["opencode", "openrouter", "groq"]);
        let zen = &providers[0];
        assert_eq!(zen.models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["free", "claude"]);
        let target = resolve(zen, &zen.models[1]).unwrap();
        assert_eq!((target.url.as_str(), target.api_key.as_deref()), ("https://opencode.ai/inference/anthropic/v1/messages", Some("public")));
        // Config alone does not trigger the loader, so only the configured header is present.
        let openrouter = &providers[1];
        assert_eq!(openrouter.models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["x/y"]);
        let target = resolve(openrouter, &openrouter.models[0]).unwrap();
        assert_eq!(target.url, "https://openrouter.ai/api/v1/chat/completions");
        assert_eq!(target.headers, [("X-Title".to_string(), "mine".to_string())]);
        assert_eq!(resolve(&providers[2], &providers[2].models[0]).unwrap().url, "https://api.groq.com/openai/v1/chat/completions");

        let keyed = build(&catalog, &config, &vec![("openrouter".to_string(), auth::Info::Api { key: "sk".into(), metadata: None }), ("opencode".to_string(), auth::Info::Api { key: "zen".into(), metadata: None })]);
        assert_eq!(keyed[0].models.len(), 3);
        assert_eq!(resolve(&keyed[1], &keyed[1].models[0]).unwrap().headers, [("HTTP-Referer".to_string(), "https://opencode.ai/".to_string()), ("X-Title".to_string(), "mine".to_string())]);
    }
}
