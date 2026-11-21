// Per-package differences of the SDKs that speak OpenAI chat completions with their own request builders.
use crate::{Request, ToolChoice};
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Wrapper {
    pub base: &'static str,
    // Variables the SDK itself reads when no key was configured, in order.
    pub env: &'static [&'static str],
    // Whether the package asks for a usage chunk (`stream_options.include_usage`).
    pub include_usage: bool,
}

// Packages that are OpenAI chat on the wire; `None` for everything else.
pub fn wrapper(npm: &str) -> Option<Wrapper> {
    let w = |base, env, include_usage| Some(Wrapper { base, env, include_usage });
    match npm {
        "@ai-sdk/groq" => w("https://api.groq.com/openai/v1", &["GROQ_API_KEY"], false),
        "@ai-sdk/cerebras" => w("https://api.cerebras.ai/v1", &["CEREBRAS_API_KEY"], false),
        "@ai-sdk/deepinfra" => w("https://api.deepinfra.com/v1/openai", &["DEEPINFRA_API_KEY"], false),
        "@ai-sdk/togetherai" => w("https://api.together.xyz/v1", &["TOGETHER_API_KEY", "TOGETHER_AI_API_KEY"], true),
        "@ai-sdk/vercel" => w("https://api.v0.dev/v1", &["VERCEL_API_KEY"], false),
        "@ai-sdk/mistral" => w("https://api.mistral.ai/v1", &["MISTRAL_API_KEY"], false),
        "@ai-sdk/perplexity" => w("https://api.perplexity.ai", &["PERPLEXITY_API_KEY"], false),
        "@ai-sdk/alibaba" => w("https://dashscope-intl.aliyuncs.com/compatible-mode/v1", &["ALIBABA_API_KEY"], true),
        "@openrouter/ai-sdk-provider" => w("https://openrouter.ai/api/v1", &["OPENROUTER_API_KEY"], false),
        "venice-ai-sdk-provider" => w("https://api.venice.ai/api/v1", &["VENICE_API_KEY"], false),
        "@saladtechnologies-oss/ai-sdk-provider" => w("https://ai.salad.cloud/v1", &["SALAD_CLOUD_API_KEY"], false),
        "merge-gateway-ai-sdk-provider" => w("https://api-gateway.merge.dev/v1/ai-sdk", &["MERGE_GATEWAY_API_KEY"], false),
        "@qvac/ai-sdk-provider" => w("http://127.0.0.1:11435/v1", &[], false),
        _ => None,
    }
}

// Packages with a known wire protocol this build does not implement, and what they would need.
pub fn unsupported(npm: &str) -> Option<&'static str> {
    Some(match npm {
        "@ai-sdk/cohere" => "Cohere v2 chat (POST /v2/chat with its own event stream)",
        "@ai-sdk/gateway" => "Vercel AI Gateway (POST /v3/ai/language-model carrying AI SDK v3 call options)",
        "ai-gateway-provider" => "Cloudflare AI Gateway universal endpoint (wrapped provider requests)",
        "gitlab-ai-provider" => "GitLab Duo agentic chat / workflow (WebSocket)",
        "watsonx-ai-provider" => "IBM watsonx (IAM token exchange and /ml/v1/text/chat_stream)",
        "@jerome-benoit/sap-ai-provider-v2" => "SAP AI Core orchestration",
        _ => return None,
    })
}

// Packages that build their own request instead of extending the generic OpenAI-compatible model.
pub fn own_builder(npm: &str) -> bool {
    matches!(
        npm,
        "@ai-sdk/groq" | "@ai-sdk/cerebras" | "@ai-sdk/mistral" | "@ai-sdk/perplexity" | "@ai-sdk/alibaba" | "@openrouter/ai-sdk-provider" | "venice-ai-sdk-provider" | "@saladtechnologies-oss/ai-sdk-provider" | "merge-gateway-ai-sdk-provider"
    )
}

const COMMON: [&str; 9] =["model", "messages", "tools", "tool_choice", "stream", "stream_options", "max_tokens", "temperature", "top_p"];

fn retain(body: &mut Map<String, Value>, extra: &[&str]) {
    body.retain(|key, _| COMMON.contains(&key.as_str()) || extra.contains(&key.as_str()));
}

fn rename(body: &mut Map<String, Value>, from: &str, to: &str) {
    if let Some(value) = body.remove(from) {
        body.insert(to.into(), value);
    }
}

fn each_message(body: &mut Map<String, Value>, mut apply: impl FnMut(&mut Map<String, Value>)) {
    for message in body.get_mut("messages").and_then(Value::as_array_mut).into_iter().flatten() {
        if let Some(map) = message.as_object_mut() {
            apply(map);
        }
    }
}

// Past reasoning travels in a different assistant field, or is folded into the text.
fn reasoning(body: &mut Map<String, Value>, field: Option<&str>) {
    each_message(body, |message| {
        let Some(Value::String(thought)) = message.remove("reasoning_content") else { return };
        match field {
            Some(field) => {
                message.insert(field.into(), thought.into());
            }
            None => {
                let text = message.get("content").and_then(Value::as_str).unwrap_or_default();
                message.insert("content".into(), format!("{thought}{text}").into());
            }
        }
    });
}

// Rewrites a generic chat-completions body into what `npm` sends. Options named in `req.options` are the flat provider options.
pub fn adapt(body: &mut Value, npm: &str, req: &Request) {
    let Some(body) = body.as_object_mut() else { return };
    let option = |key: &str| req.options.get(key).filter(|v| !v.is_null()).cloned();
    let set = |body: &mut Map<String, Value>, key: &str, value: Option<Value>| {
        if let Some(value) = value {
            body.insert(key.into(), value);
        }
    };
    match npm {
        "@ai-sdk/cerebras" => reasoning(body, Some("reasoning")),
        "@ai-sdk/groq" => {
            retain(body, &["user", "reasoning_effort"]);
            reasoning(body, Some("reasoning"));
            set(body, "reasoning_format", option("reasoningFormat"));
            set(body, "service_tier", option("serviceTier"));
            set(body, "parallel_tool_calls", option("parallelToolCalls"));
        }
        "@ai-sdk/mistral" => {
            retain(body, &["reasoning_effort"]);
            reasoning(body, None);
            set(body, "safe_prompt", option("safePrompt"));
            set(body, "prompt_cache_key", option("promptCacheKey"));
            set(body, "parallel_tool_calls", option("parallelToolCalls"));
            // Mistral has no "required": it forces a call with "any", narrowing the tool list for a named tool.
            if let Some(ToolChoice::Tool(name)) = &req.tool_choice
                && let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut)
            {
                tools.retain(|t| t.pointer("/function/name").and_then(Value::as_str) == Some(name));
            }
            if matches!(req.tool_choice, Some(ToolChoice::Required | ToolChoice::Tool(_))) && body.contains_key("tool_choice") {
                body.insert("tool_choice".into(), "any".into());
            }
            let names: Vec<(String, String)> = req
                .messages
                .iter()
                .filter_map(|m| if let crate::Message::Assistant { tool_calls, .. } = m { Some(tool_calls) } else { None })
                .flatten()
                .map(|c| (c.id.clone(), c.name.clone()))
                .collect();
            let count = body.get("messages").and_then(Value::as_array).map_or(0, Vec::len);
            let mut index = 0;
            each_message(body, |message| {
                index += 1;
                match message.get("role").and_then(Value::as_str) {
                    Some("tool") => {
                        let id = message.get("tool_call_id").and_then(Value::as_str).unwrap_or_default();
                        if let Some((_, name)) = names.iter().find(|(call, _)| call == id) {
                            message.insert("name".into(), name.clone().into());
                        }
                    }
                    Some("assistant") if index == count => {
                        message.insert("prefix".into(), true.into());
                    }
                    _ => {}
                }
                // Images are bare URL strings.
                for part in message.get_mut("content").and_then(Value::as_array_mut).into_iter().flatten() {
                    if let Some(url) = part.pointer("/image_url/url").cloned() {
                        part["image_url"] = url;
                    }
                }
            });
        }
        "@ai-sdk/perplexity" => {
            let passthrough: Vec<String> = req.options.as_object().into_iter().flatten().map(|(k, _)| k.clone()).collect();
            let keep: Vec<&str> = passthrough.iter().map(String::as_str).collect();
            retain(body, &keep);
            body.remove("tools");
            body.remove("tool_choice");
            body.remove("stream_options");
            reasoning(body, None);
            set(body, "top_k", req.top_k.map(Into::into));
        }
        "@ai-sdk/alibaba" => {
            retain(body, &[]);
            reasoning(body, None);
            set(body, "top_k", req.top_k.map(Into::into));
            set(body, "enable_thinking", option("enableThinking"));
            set(body, "thinking_budget", option("thinkingBudget"));
            if body.contains_key("tools") {
                set(body, "parallel_tool_calls", option("parallelToolCalls"));
            }
        }
        "venice-ai-sdk-provider" => {
            body.remove("stream_options");
            rename(body, "max_tokens", "max_completion_tokens");
            if let Some(effort) = body.remove("reasoning_effort") {
                body.insert("reasoning".into(), json!({"effort": effort}));
            }
            set(body, "prompt_cache_key", option("promptCacheKey"));
            body.remove("promptCacheKey");
            set(body, "top_k", req.top_k.map(Into::into));
            body.remove("veniceParameters");
            let mut parameters = option("veniceParameters").and_then(|v| v.as_object().cloned()).unwrap_or_default();
            parameters.entry("include_venice_system_prompt").or_insert(false.into());
            body.insert("venice_parameters".into(), Value::Object(parameters));
        }
        "@saladtechnologies-oss/ai-sdk-provider" => {
            retain(body, &["reasoning_effort"]);
            set(body, "top_k", req.top_k.map(Into::into));
            if body.get("reasoning_effort").and_then(Value::as_str) == Some("none") {
                body.insert("chat_template_kwargs".into(), json!({"enable_thinking": false}));
            }
        }
        "merge-gateway-ai-sdk-provider" => {
            retain(body, &["tags", "vendor", "vendors", "project_id", "include_routing_metadata", "thinking", "user"]);
            each_message(body, |message| {
                message.remove("reasoning_content");
            });
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, Part, Tool, ToolCall, chat};

    fn request() -> Request {
        Request {
            model: "m".into(),
            system: vec!["sys".into()],
            messages: vec![
                Message::UserParts(vec![Part::Text("look".into()), Part::Media { mime: "image/png".into(), data: "AAAA".into(), filename: None }]),
                Message::Assistant { text: "ok".into(), reasoning: "hm ".into(), tool_calls: vec![ToolCall { id: "c1".into(), name: "get_weather".into(), arguments: "{}".into() }], reasoning_parts: Vec::new() },
                Message::tool("c1", "sunny"),
            ],
            tools: vec![Tool { name: "get_weather".into(), description: "d".into(), parameters: json!({"type": "object"}) }, Tool { name: "other".into(), description: "d".into(), parameters: json!({}) }],
            tool_choice: Some(ToolChoice::Tool("get_weather".into())),
            max_tokens: Some(100),
            top_k: Some(40),
            options: json!({"reasoningEffort": "high", "reasoningFormat": "parsed", "promptCacheKey": "ses", "custom": 1, "enableThinking": true, "thinkingBudget": 2048}),
            ..Default::default()
        }
    }

    fn body(npm: &str) -> Value {
        let req = request();
        let mut body = chat::encode_with(&req, &chat::Config { include_usage: wrapper(npm).unwrap().include_usage, ..Default::default() }).unwrap();
        adapt(&mut body, npm, &req);
        body
    }

    #[test]
    fn bodies_follow_each_package() {
        let groq = body("@ai-sdk/groq");
        assert_eq!((&groq["reasoning_effort"], &groq["reasoning_format"], groq.get("custom"), groq.get("stream_options")), (&json!("high"), &json!("parsed"), None, None));
        assert_eq!((&groq["messages"][2]["reasoning"], groq["messages"][2].get("reasoning_content")), (&json!("hm "), None));

        let mistral = body("@ai-sdk/mistral");
        assert_eq!((&mistral["tool_choice"], mistral["tools"].as_array().unwrap().len(), &mistral["prompt_cache_key"]), (&json!("any"), 1, &json!("ses")));
        assert_eq!(mistral["messages"][1]["content"][1], json!({"type": "image_url", "image_url": "data:image/png;base64,AAAA"}));
        assert_eq!((&mistral["messages"][2]["content"], &mistral["messages"][3]["name"], mistral["messages"][3].get("prefix")), (&json!("hm ok"), &json!("get_weather"), None));

        let cerebras = body("@ai-sdk/cerebras");
        assert_eq!((&cerebras["messages"][2]["reasoning"], &cerebras["custom"]), (&json!("hm "), &json!(1)));

        let perplexity = body("@ai-sdk/perplexity");
        assert!(perplexity.get("tools").is_none() && perplexity.get("tool_choice").is_none());
        assert_eq!((&perplexity["top_k"], &perplexity["custom"]), (&json!(40), &json!(1)));

        let alibaba = body("@ai-sdk/alibaba");
        assert_eq!((&alibaba["enable_thinking"], &alibaba["thinking_budget"], &alibaba["stream_options"], alibaba.get("custom")), (&json!(true), &json!(2048), &json!({"include_usage": true}), None));

        let venice = body("venice-ai-sdk-provider");
        assert_eq!((&venice["max_completion_tokens"], venice.get("max_tokens"), &venice["reasoning"]), (&json!(100), None, &json!({"effort": "high"})));
        assert_eq!((&venice["venice_parameters"], &venice["prompt_cache_key"]), (&json!({"include_venice_system_prompt": false}), &json!("ses")));

        let merge = body("merge-gateway-ai-sdk-provider");
        assert!(merge["messages"][2].get("reasoning_content").is_none() && merge.get("custom").is_none());
        assert_eq!(body("@ai-sdk/togetherai")["stream_options"], json!({"include_usage": true}));
        assert!(wrapper("@ai-sdk/cohere").is_none() && unsupported("@ai-sdk/cohere").is_some() && unsupported("@ai-sdk/groq").is_none());
    }
}
