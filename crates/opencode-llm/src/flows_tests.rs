// Cross-module tests: package coverage, Bedrock end to end, model discovery, login flows and plugin hooks.
use crate::client::tests::{SSE, http, serve, text};
use crate::client::{self, prepare};
use crate::hooks::{self, AuthLoader, Executor, HttpRequest, HttpResponse, Loaded, Overrides};
use crate::oauth::{self, Poll};
use crate::provider::{self, Credential, Protocol, Provider};
use crate::testutil::{cassette, request_body};
use crate::{Error, Event, Message, Request, azure, bedrock, codex, copilot, dialect, digitalocean, sigv4, snowflake, vertex, xai};
use base64::Engine;
use futures_util::future::BoxFuture;
use opencode_core::auth;
use serde_json::{Value, json};
use std::sync::Mutex;

const JSON: &str = "content-type: application/json\r\n";

fn one(id: &str, npm: &str, models: Value, options: Value) -> Provider {
    let catalog = json!({id: {"id": id, "env": [], "npm": npm, "models": models}});
    provider::build(&catalog, &json!({"provider": {id: {"options": options}}}), &Vec::new()).remove(0)
}

fn url_of(id: &str, npm: &str, model: &str, options: Value) -> String {
    let provider = one(id, npm, json!({model: {"id": model}}), options);
    provider::resolve(&provider, &provider.models[0]).unwrap().url
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

fn ask() -> Request {
    Request { model: "m".into(), system: vec!["sys".into()], messages: vec![Message::User("q".into())], max_tokens: Some(64), ..Default::default() }
}

fn param(url: &str, name: &str) -> String {
    reqwest::Url::parse(url).unwrap().query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.to_string()).unwrap_or_default()
}

// Every distinct `npm` value of the audited models.dev catalog (232 providers), provider-level and per-model.
const CATALOG_NPM: [&str; 29] = [
    "@ai-sdk/openai-compatible",
    "@ai-sdk/anthropic",
    "@ai-sdk/openai",
    "@ai-sdk/azure",
    "@ai-sdk/deepinfra",
    "@ai-sdk/cerebras",
    "@ai-sdk/groq",
    "@ai-sdk/cohere",
    "venice-ai-sdk-provider",
    "@openrouter/ai-sdk-provider",
    "@ai-sdk/perplexity",
    "gitlab-ai-provider",
    "@saladtechnologies-oss/ai-sdk-provider",
    "@ai-sdk/google",
    "ai-gateway-provider",
    "watsonx-ai-provider",
    "@ai-sdk/xai",
    "@ai-sdk/gateway",
    "@ai-sdk/google-vertex/anthropic",
    "@aihubmix/ai-sdk-provider",
    "merge-gateway-ai-sdk-provider",
    "@ai-sdk/mistral",
    "@ai-sdk/amazon-bedrock",
    "@jerome-benoit/sap-ai-provider-v2",
    "@ai-sdk/google-vertex",
    "@ai-sdk/togetherai",
    "@qvac/ai-sdk-provider",
    "@ai-sdk/amazon-bedrock/mantle",
    "@ai-sdk/github-copilot",
];

#[test]
fn every_catalog_package_resolves_or_names_itself() {
    let mut missing = Vec::new();
    for npm in CATALOG_NPM {
        match (provider::protocol(npm), dialect::unsupported(npm)) {
            (Some(_), None) => assert!(provider::SUPPORTED_NPM.contains(&npm), "{npm}"),
            (None, Some(_)) => missing.push(npm),
            other => panic!("{npm}: {other:?}"),
        }
    }
    assert_eq!(missing, ["@ai-sdk/cohere", "gitlab-ai-provider", "ai-gateway-provider", "watsonx-ai-provider", "@ai-sdk/gateway", "@jerome-benoit/sap-ai-provider-v2"]);
    for npm in missing {
        let provider = one("p", npm, json!({"m": {}}), json!({"apiKey": "k"}));
        let error = provider::resolve(&provider, &provider.models[0]).unwrap_err();
        assert!(matches!(&error, Error::Config(m) if m.starts_with(&format!("p/m: not supported yet: {npm} ("))), "{error}");
    }
    // An unknown package is named as well.
    let provider = one("p", "some-new-sdk", json!({"m": {}}), json!({"apiKey": "k"}));
    assert!(matches!(provider::resolve(&provider, &provider.models[0]), Err(Error::Config(m)) if m == "p/m: not supported yet: some-new-sdk"));
    let gateway = one("cloudflare-ai-gateway", "ai-gateway-provider", json!({"openai/gpt-5": {"provider": {"npm": "@ai-sdk/openai"}}}), json!({"apiKey": "k"}));
    assert!(matches!(provider::resolve(&gateway, &gateway.models[0]), Err(Error::Config(m)) if m.contains("not supported yet: ai-gateway-provider")));
}

#[test]
fn wrapper_packages_resolve_with_their_endpoints() {
    let key = json!({"apiKey": "k"});
    for (npm, expected) in [
        ("@ai-sdk/groq", "https://api.groq.com/openai/v1/chat/completions"),
        ("@ai-sdk/cerebras", "https://api.cerebras.ai/v1/chat/completions"),
        ("@ai-sdk/deepinfra", "https://api.deepinfra.com/v1/openai/chat/completions"),
        ("@ai-sdk/togetherai", "https://api.together.xyz/v1/chat/completions"),
        ("@ai-sdk/vercel", "https://api.v0.dev/v1/chat/completions"),
        ("@ai-sdk/mistral", "https://api.mistral.ai/v1/chat/completions"),
        ("@ai-sdk/perplexity", "https://api.perplexity.ai/chat/completions"),
        ("@ai-sdk/alibaba", "https://dashscope-intl.aliyuncs.com/compatible-mode/v1/chat/completions"),
        ("@openrouter/ai-sdk-provider", "https://openrouter.ai/api/v1/chat/completions"),
        ("venice-ai-sdk-provider", "https://api.venice.ai/api/v1/chat/completions"),
        ("@saladtechnologies-oss/ai-sdk-provider", "https://ai.salad.cloud/v1/chat/completions"),
        ("merge-gateway-ai-sdk-provider", "https://api-gateway.merge.dev/v1/ai-sdk/chat/completions"),
        ("@ai-sdk/xai", "https://api.x.ai/v1/responses"),
    ] {
        assert_eq!(url_of("p", npm, "m", key.clone()), expected, "{npm}");
    }
    // Only the packages that ask for it get a usage chunk; the local qvac server takes a fixed key.
    let usage = |npm: &str| {
        let provider = one("p", npm, json!({"m": {}}), json!({"apiKey": "k", "baseURL": "https://gw.test/v1"}));
        prepare(&provider::resolve(&provider, &provider.models[0]).unwrap(), &ask(), None).unwrap().body.get("stream_options").is_some()
    };
    assert_eq!([usage("@ai-sdk/groq"), usage("@ai-sdk/togetherai"), usage("@ai-sdk/alibaba"), usage("@ai-sdk/mistral"), usage("@ai-sdk/openai-compatible")], [false, true, true, false, true]);
    let qvac = one("qvac", "@qvac/ai-sdk-provider", json!({"m": {}}), json!({}));
    let target = provider::resolve(&qvac, &qvac.models[0]).unwrap();
    assert_eq!((target.url.as_str(), target.api_key.as_deref()), ("http://127.0.0.1:11435/v1/chat/completions", Some("qvac")));

    // AIHubMix picks the protocol from the model id.
    let hub = one("aihubmix", provider::AIHUBMIX_NPM, json!({"claude-sonnet-4-5": {}, "gemini-2.5-pro": {}, "gemini-2.5-pro-search": {}, "gpt-5": {}}), key.clone());
    let targets: Vec<_> = hub.models.iter().map(|m| provider::resolve(&hub, m).unwrap()).collect();
    assert_eq!(
        targets.iter().map(|t| (t.url.as_str(), t.protocol)).collect::<Vec<_>>(),
        [
            ("https://aihubmix.com/v1/messages", Protocol::Anthropic),
            ("https://aihubmix.com/gemini/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse", Protocol::Gemini),
            ("https://aihubmix.com/v1/chat/completions", Protocol::Chat),
            ("https://aihubmix.com/v1/chat/completions", Protocol::Chat),
        ]
    );
    let claude = prepare(&targets[0], &ask(), None).unwrap();
    assert_eq!((header(&claude.headers, "x-api-key"), header(&claude.headers, "app-code"), header(&claude.headers, "anthropic-version")), (Some("k"), Some("WHVL9885"), None));
    assert_eq!(header(&prepare(&targets[1], &ask(), None).unwrap().headers, "x-goog-api-key"), Some("k"));
}

#[test]
fn azure_vertex_snowflake_and_mantle_targets() {
    let azure = one("azure", azure::NPM, json!({"gpt-5": {}}), json!({"apiKey": "az", "resourceName": "res"}));
    let target = provider::resolve(&azure, &azure.models[0]).unwrap();
    assert_eq!((target.url.as_str(), target.protocol), ("https://res.openai.azure.com/openai/v1/responses?api-version=v1", Protocol::Responses));
    let prepared = prepare(&target, &ask(), None).unwrap();
    assert_eq!((header(&prepared.headers, "api-key"), header(&prepared.headers, "authorization")), (Some("az"), None));
    let chat = one("azure", azure::NPM, json!({"dep": {}}), json!({"apiKey": "az", "resourceName": "res", "useCompletionUrls": true, "useDeploymentBasedUrls": true, "apiVersion": "2025-04-01"}));
    let target = provider::resolve(&chat, &chat.models[0]).unwrap();
    assert_eq!((target.url.as_str(), target.protocol), ("https://res.openai.azure.com/openai/deployments/dep/chat/completions?api-version=2025-04-01", Protocol::Chat));
    // An Entra ID login replaces the key with a CLI token, for every package under the provider.
    let catalog = json!({"azure": {"id": "azure", "env": [], "npm": azure::NPM, "models": {"gpt-5": {}, "claude": {"provider": {"npm": "@ai-sdk/anthropic", "api": "https://${AZURE_RESOURCE_NAME}.services.ai.azure.com/anthropic/v1"}}}}});
    let entra = provider::build(&catalog, &json!({}), &vec![("azure".to_string(), azure::login_entry("entra-res"))]).remove(0);
    let target = provider::resolve(&entra, &entra.models[0]).unwrap();
    assert!(matches!(target.credential, Some(Credential::Azure(_))) && target.api_key.is_none() && target.url.starts_with("https://entra-res.openai.azure.com/openai/v1/responses"));
    let claude = provider::resolve(&entra, &entra.models[1]).unwrap();
    assert_eq!(claude.url, "https://entra-res.services.ai.azure.com/anthropic/v1/messages");
    assert_eq!(azure::scope_for(&claude.url), azure::FOUNDRY_SCOPE);

    let express = one("google-vertex", vertex::NPM, json!({"gemini-2.5-pro": {}}), json!({"apiKey": "vk"}));
    let target = provider::resolve(&express, &express.models[0]).unwrap();
    assert_eq!(target.url, "https://aiplatform.googleapis.com/v1/publishers/google/models/gemini-2.5-pro:streamGenerateContent?alt=sse");
    assert_eq!(header(&prepare(&target, &ask(), None).unwrap().headers, "x-goog-api-key"), Some("vk"));
    let partner = json!({"claude-sonnet-4-5@20250929": {"provider": {"npm": vertex::ANTHROPIC_NPM}}, "llama": {"provider": {"npm": "@ai-sdk/openai-compatible", "api": "https://${GOOGLE_VERTEX_ENDPOINT}/v1/projects/${GOOGLE_VERTEX_PROJECT}/locations/${GOOGLE_VERTEX_LOCATION}/endpoints/openapi"}}});
    let adc = one("google-vertex", vertex::NPM, partner, json!({"project": "proj", "location": "global"}));
    let claude = provider::resolve(&adc, &adc.models[0]).unwrap();
    assert_eq!(claude.url, "https://aiplatform.googleapis.com/v1/projects/proj/locations/global/publishers/anthropic/models/claude-sonnet-4-5@20250929:streamRawPredict");
    assert!(matches!(claude.credential, Some(Credential::Vertex(_))));
    let prepared = prepare(&claude, &ask(), None).unwrap();
    assert_eq!((prepared.body.get("model"), &prepared.body["anthropic_version"], header(&prepared.headers, "anthropic-version")), (None, &json!("vertex-2023-10-16"), None));
    let llama = provider::resolve(&adc, &adc.models[1]).unwrap();
    assert_eq!(llama.url, "https://aiplatform.googleapis.com/v1/projects/proj/locations/global/endpoints/openapi/chat/completions");
    assert!(matches!(llama.credential, Some(Credential::Vertex(_))) && llama.api_key.is_none());

    let cortex = one(snowflake::PROVIDER_ID, "@ai-sdk/openai-compatible", json!({"claude-4-sonnet": {}}), json!({"account": "acct", "token": "pat"}));
    let target = provider::resolve(&cortex, &cortex.models[0]).unwrap();
    assert_eq!((target.url.as_str(), target.api_key.as_deref()), ("https://acct.snowflakecomputing.com/api/v2/cortex/v1/chat/completions", Some("pat")));
    let body = prepare(&target, &ask(), None).unwrap().body;
    assert_eq!((&body["max_completion_tokens"], body.get("max_tokens")), (&json!(64), None));

    let models = json!({"anthropic.claude-sonnet-4-5-v1:0": {}, "openai.gpt-oss-120b": {"provider": {"npm": bedrock::MANTLE_NPM}}, "openai.gpt-oss-safeguard-20b": {"provider": {"npm": bedrock::MANTLE_NPM}}});
    let aws = one(bedrock::PROVIDER_ID, bedrock::NPM, models, json!({"apiKey": "bk", "region": "eu-west-1"}));
    let urls: Vec<String> = aws.models.iter().map(|m| provider::resolve(&aws, m).unwrap().url).collect();
    assert_eq!(
        urls,
        [
            "https://bedrock-runtime.eu-west-1.amazonaws.com/model/eu.anthropic.claude-sonnet-4-5-v1%3A0/converse-stream",
            "https://bedrock-mantle.eu-west-1.api.aws/v1/responses",
            "https://bedrock-mantle.eu-west-1.api.aws/v1/chat/completions",
        ]
    );
    let Some(Credential::Bedrock(config)) = provider::resolve(&aws, &aws.models[1]).unwrap().credential else { panic!("no credential") };
    assert_eq!((config.service, config.bearer.as_deref(), config.region.as_str()), ("bedrock-mantle", Some("bk"), "eu-west-1"));
}

struct Fake {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    seen: Mutex<Vec<HttpRequest>>,
}

impl Executor for Fake {
    fn execute<'a>(&'a self, request: HttpRequest) -> BoxFuture<'a, Result<HttpResponse, Error>> {
        self.seen.lock().unwrap().push(request);
        Box::pin(async move { Ok(HttpResponse::buffered(self.status, vec![("content-type".into(), self.content_type.into())], self.body.clone())) })
    }
}

#[tokio::test]
async fn bedrock_signs_and_streams_the_recorded_exchange() {
    let Some(c) = cassette("llm/test/fixtures/recordings/bedrock-converse/streams-text.json") else { return };
    let aws = one(bedrock::PROVIDER_ID, bedrock::NPM, json!({"amazon.nova-micro-v1:0": {}}), json!({"apiKey": "bedrock-key", "region": "us-east-1"}));
    let mut target = provider::resolve(&aws, &aws.models[0]).unwrap();
    assert_eq!((target.url.as_str(), target.protocol), (c["interactions"][0]["request"]["url"].as_str().unwrap(), Protocol::Bedrock));
    let request = Request {
        model: "amazon.nova-micro-v1:0".into(),
        system: vec!["Reply with the single word 'Hello'.".into()],
        messages: vec![Message::User("Say hello.".into())],
        max_tokens: Some(16),
        temperature: Some(0.0),
        options: json!({}),
        ..Default::default()
    };
    let mut recorded = request_body(&c, 0);
    recorded.as_object_mut().unwrap().remove("modelId");

    // SigV4 with fixed credentials and clock: the body hash and the double-encoded path are part of the signature.
    let mut prepared = prepare(&target, &request, None).unwrap();
    assert_eq!(prepared.body, recorded);
    let body = prepared.body.to_string().into_bytes();
    let credentials = sigv4::Credentials { access_key_id: "AKIDEXAMPLE".into(), secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(), session_token: None };
    let config = bedrock::Config { time: Some(1_440_938_160), ..bedrock::Config::new("us-east-1") };
    client::authorize_bedrock(&mut prepared, &body, &config, &bedrock::Auth::SigV4(credentials.clone())).unwrap();
    let signed = header(&prepared.headers, "authorization").unwrap().to_string();
    assert!(signed.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/bedrock/aws4_request, SignedHeaders=host;x-amz-date, Signature="), "{signed}");
    assert_eq!((header(&prepared.headers, "x-amz-date"), header(&prepared.headers, "content-type")), (Some("20150830T123600Z"), Some("application/json")));
    assert_eq!(signed.rsplit('=').next().unwrap(), SIGNATURE);
    // Any change to the payload changes the signature.
    let mut other = prepare(&target, &request, None).unwrap();
    client::authorize_bedrock(&mut other, b"{}", &config, &bedrock::Auth::SigV4(credentials)).unwrap();
    assert_ne!(header(&other.headers, "authorization"), Some(signed.as_str()));

    // The bearer path, streamed through a custom executor with the recorded binary body.
    let frames = base64::engine::general_purpose::STANDARD.decode(c["interactions"][0]["response"]["body"].as_str().unwrap()).unwrap();
    let fake = Fake { status: 200, content_type: "application/vnd.amazon.eventstream", body: frames.clone(), seen: Mutex::new(Vec::new()) };
    target.credential = Some(Credential::Bedrock(bedrock::Config { bearer: Some("bedrock-key".into()), ..bedrock::Config::new("us-east-1") }));
    let mut events = Vec::new();
    client::stream_with(&fake, &http(), &target, &request, |e| events.push(e)).await.unwrap();
    assert_eq!(text(&events), "Hello");
    assert!(matches!(events.last(), Some(Event::Finish { reason, usage }) if reason == "stop" && usage.total == 14));
    {
        let seen = fake.seen.lock().unwrap();
        assert_eq!((seen[0].method.as_str(), seen[0].url.as_str()), ("POST", target.url.as_str()));
        assert!(header(&seen[0].headers, "authorization").is_some_and(|v| v.starts_with("Bearer ")));
        assert_eq!(serde_json::from_slice::<Value>(&seen[0].body).unwrap(), recorded);
    }

    // A truncated stream and an HTTP error are both reported.
    let cut = Fake { status: 200, content_type: "application/vnd.amazon.eventstream", body: frames[..frames.len() - 9].to_vec(), seen: Mutex::new(Vec::new()) };
    assert!(matches!(client::stream_with(&cut, &http(), &target, &request, |_| {}).await, Err(Error::Stream(m)) if m.contains("Incomplete")));
    let denied = Fake { status: 403, content_type: "application/json", body: br#"{"message":"The security token included in the request is invalid."}"#.to_vec(), seen: Mutex::new(Vec::new()) };
    assert!(matches!(client::stream_with(&denied, &http(), &target, &request, |_| {}).await, Err(Error::Status { status: 403, body, .. }) if body.contains("security token")));
}

// Computed independently (Node `crypto`) over the same canonical request.
const SIGNATURE: &str = "159e7edd85137f763437f6ea190c1cb1b475d011a849650e4e7600c074d110a6";

#[tokio::test]
async fn copilot_models_are_discovered_with_catalog_fallback() {
    let item = |id: &str, picker: bool, endpoints: Value, supports: Value| {
        json!({"id": id, "name": format!("Remote {id}"), "version": format!("{id}-2026-01-02"), "model_picker_enabled": picker, "supported_endpoints": endpoints,
            "capabilities": {"family": "fam", "limits": {"max_context_window_tokens": 200000, "max_prompt_tokens": 128000, "max_output_tokens": 16000, "vision": {"max_prompt_image_size": 1, "max_prompt_images": 1, "supported_media_types": ["image/png", "application/pdf"]}}, "supports": supports}})
    };
    let mut priced = item("gpt-5.2", true, json!(["/responses", "/chat/completions"]), json!({"tool_calls": true, "reasoning_effort": ["low", "high"]}));
    priced["billing"] = json!({"token_prices": {"batch_size": 1000, "default": {"input_price": 0.25, "output_price": 2, "cache_price": 0.025}}});
    let mut blocked = item("blocked", true, json!(["/chat/completions"]), json!({"tool_calls": true}));
    blocked["policy"] = json!({"state": "disabled"});
    let mut unlimited = item("unlimited", true, json!(["/chat/completions"]), json!({"tool_calls": true}));
    unlimited["capabilities"]["limits"] = json!({});
    let listing = json!({"data": [
        item("gpt-4.1", true, json!(["/chat/completions"]), json!({"tool_calls": true, "vision": true})),
        item("claude-sonnet-4.5", true, json!(["/v1/messages", "/chat/completions"]), json!({"tool_calls": true, "adaptive_thinking": true, "reasoning_effort": ["medium"], "vision": true})),
        item("hidden", false, json!(["/chat/completions"]), json!({"tool_calls": true})),
        priced, blocked, unlimited, "garbage"
    ]});
    let (base, seen) = serve(vec![("/models", 200, JSON, listing.to_string())]).await;
    let catalog = json!({"github-copilot": {"id": "github-copilot", "env": [], "npm": "@ai-sdk/openai-compatible", "api": "https://api.githubcopilot.com", "models": {
        "gpt-4.1": {"id": "gpt-4.1", "name": "GPT 4.1", "family": "gpt", "release_date": "2025-04-14", "attachment": true},
        "retired": {"id": "retired"}
    }}});
    let dir = tempfile::tempdir().unwrap();
    let entries = vec![("github-copilot".to_string(), auth::Info::Oauth { refresh: "gho_x".into(), access: "gho_x".into(), expires: 0, account_id: None, enterprise_url: Some("company.ghe.com".into()) })];
    let endpoints = hooks::Endpoints { copilot: Some(base.clone()), ..Default::default() };
    let found = hooks::discover(&http(), &catalog, &json!({}), &entries, &dir.path().join("auth.json"), &endpoints).await;
    let providers = provider::build_with(&catalog, &json!({}), &entries, &found);
    let models = &providers[0].models;
    assert_eq!(models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["gpt-4.1", "claude-sonnet-4.5", "gpt-5.2"]);
    // Catalog facts survive; the API supplies endpoint, limits and prices.
    assert_eq!((models[0].name.as_str(), models[0].release_date.as_str(), models[0].npm.as_str(), models[0].endpoint.as_deref()), ("GPT 4.1", "2025-04-14", copilot::NPM, Some("chat")));
    assert_eq!((models[0].limit_context, models[0].limit_input, models[0].limit_output, models[0].input.image, models[0].input.pdf), (200_000, 128_000, 16_000, true, true));
    let claude = &models[1];
    assert_eq!((claude.npm.as_str(), claude.api_url.clone(), claude.endpoint.as_deref(), claude.release_date.as_str(), claude.reasoning), ("@ai-sdk/anthropic", format!("{base}/v1"), Some("messages"), "2026-01-02", true));
    assert_eq!(claude.variants["medium"], json!({"thinking": {"type": "adaptive", "display": "summarized"}, "effort": "medium"}));
    let target = provider::resolve(&providers[0], claude).unwrap();
    assert_eq!((target.url.clone(), target.protocol), (format!("{base}/v1/messages"), Protocol::Anthropic));
    let gpt5 = &models[2];
    assert_eq!((gpt5.cost.base.input, gpt5.cost.base.output, gpt5.cost.base.cache_read), (2.5, 20.0, 0.25));
    assert_eq!(gpt5.variants["high"], json!({"reasoningEffort": "high", "reasoningSummary": "auto", "include": ["reasoning.encrypted_content"]}));
    assert_eq!(provider::resolve(&providers[0], gpt5).unwrap().protocol, Protocol::Responses);
    {
        let seen = seen.lock().unwrap();
        assert_eq!((seen[0].header("authorization"), seen[0].header("x-github-api-version")), (Some("Bearer gho_x"), Some(copilot::API_VERSION)));
    }

    // When discovery fails the catalog list is kept, pointed at the login's host.
    let broken = hooks::Endpoints { copilot: Some(format!("{base}/missing")), ..Default::default() };
    let none = hooks::discover(&http(), &catalog, &json!({}), &entries, &dir.path().join("auth.json"), &broken).await;
    assert!(none.models.is_empty());
    let fallback = provider::build_with(&catalog, &json!({}), &entries, &none);
    assert_eq!(fallback[0].models.iter().map(|m| (m.id.as_str(), m.npm.as_str(), m.api_url.as_str())).collect::<Vec<_>>(), [("gpt-4.1", copilot::NPM, "https://copilot-api.company.ghe.com"), ("retired", copilot::NPM, "https://copilot-api.company.ghe.com")]);
}

#[tokio::test]
async fn chatgpt_browser_and_headless_logins() {
    let access = codex::tests::jwt(&json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acct_1"}}));
    let tokens = json!({"access_token": access, "refresh_token": "rt", "id_token": "", "expires_in": 120}).to_string();
    let (issuer, seen) = serve(vec![
        ("/oauth/token", 200, JSON, tokens.clone()),
        ("/api/accounts/deviceauth/usercode", 200, JSON, json!({"device_auth_id": "dev1", "user_code": "ABCD-1234", "interval": "7"}).to_string()),
        ("/api/accounts/deviceauth/token", 403, JSON, "{}".into()),
        ("/api/accounts/deviceauth/token", 200, JSON, json!({"authorization_code": "authcode", "code_verifier": "server-verifier"}).to_string()),
        ("/api/accounts/deviceauth/token", 500, JSON, "{}".into()),
    ])
    .await;
    let login = codex::login_start_on(&issuer, 0).await.unwrap();
    assert!(login.url.starts_with(&format!("{issuer}/oauth/authorize?response_type=code&client_id={}&redirect_uri=http%3A%2F%2Flocalhost%3A", codex::CLIENT_ID)));
    for (name, value) in [("scope", codex::SCOPE), ("code_challenge_method", "S256"), ("id_token_add_organizations", "true"), ("codex_cli_simplified_flow", "true"), ("originator", "opencode")] {
        assert_eq!(param(&login.url, name), value);
    }
    let (state, challenge, redirect) = (param(&login.url, "state"), param(&login.url, "code_challenge"), login.redirect_uri.clone());
    let callback = redirect.replace("localhost", "127.0.0.1");
    let waiting = tokio::spawn(async move { login.wait(&http()).await });
    // A forged state is rejected and ends the login; so a second attempt is needed.
    assert_eq!(http().get(format!("{callback}?code=c&state=forged")).send().await.unwrap().status().as_u16(), 400);
    assert!(matches!(waiting.await.unwrap(), Err(Error::Config(m)) if m.contains("CSRF")));

    let login = codex::login_start_on(&issuer, 0).await.unwrap();
    let (state2, challenge2, redirect) = (param(&login.url, "state"), param(&login.url, "code_challenge"), login.redirect_uri.clone());
    assert!(state2 != state && challenge.len() == 43 && challenge2 != challenge);
    let waiting = tokio::spawn(async move { login.wait(&http()).await });
    let page = http().get(format!("{}?code=the-code&state={state2}", redirect.replace("localhost", "127.0.0.1"))).send().await.unwrap();
    assert!(page.status().is_success() && page.text().await.unwrap().contains("Authorization successful"));
    let info = waiting.await.unwrap().unwrap();
    assert!(matches!(&info, auth::Info::Oauth { refresh, access: a, account_id: Some(account), expires, .. } if refresh == "rt" && *a == access && account == "acct_1" && *expires > oauth::now_ms()));
    let dir = tempfile::tempdir().unwrap();
    oauth::store(&dir.path().join("auth.json"), codex::PROVIDER_ID, info).unwrap();
    assert!(matches!(&auth::read_all(&dir.path().join("auth.json"))[0], (id, auth::Info::Oauth { .. }) if id == "openai"));
    {
        let seen = seen.lock().unwrap();
        let form = oauth::parse_query(&seen[0].body);
        let field = |name: &str| form.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
        assert_eq!((field("grant_type"), field("code"), field("client_id"), field("redirect_uri")), (Some("authorization_code"), Some("the-code"), Some(codex::CLIENT_ID), Some(redirect.as_str())));
        // The verifier sent to the token endpoint is the one whose challenge went to the browser.
        assert_eq!(oauth::challenge(field("code_verifier").unwrap()), challenge2);
        assert_eq!(seen[0].header("content-type"), Some("application/x-www-form-urlencoded"));
    }

    // Headless: 403 means "not yet", then the server hands over a code and its own verifier.
    let (issuer, seen) = serve(vec![
        ("/api/accounts/deviceauth/usercode", 200, JSON, json!({"device_auth_id": "dev1", "user_code": "ABCD-1234", "interval": "7"}).to_string()),
        ("/api/accounts/deviceauth/token", 403, JSON, "{}".into()),
        ("/api/accounts/deviceauth/token", 200, JSON, json!({"authorization_code": "authcode", "code_verifier": "server-verifier"}).to_string()),
        ("/api/accounts/deviceauth/token", 500, JSON, "{}".into()),
        ("/oauth/token", 200, JSON, tokens),
    ])
    .await;
    let device = codex::device_start(&http(), &issuer).await.unwrap();
    assert_eq!((device.url.clone(), device.instructions(), device.interval_ms), (format!("{issuer}/codex/device"), "Enter code: ABCD-1234".to_string(), 7000));
    assert_eq!(codex::device_poll(&http(), &device).await.unwrap(), Poll::Pending { wait_ms: 10_000 });
    assert!(matches!(codex::device_poll(&http(), &device).await.unwrap(), Poll::Success(auth::Info::Oauth { account_id: Some(a), .. }) if a == "acct_1"));
    assert_eq!(codex::device_poll(&http(), &device).await.unwrap(), Poll::Failed);
    let seen = seen.lock().unwrap();
    assert_eq!(serde_json::from_str::<Value>(&seen[0].body).unwrap(), json!({"client_id": codex::CLIENT_ID}));
    assert_eq!(serde_json::from_str::<Value>(&seen[1].body).unwrap(), json!({"device_auth_id": "dev1", "user_code": "ABCD-1234"}));
    let exchange = seen.iter().find(|s| s.path == "/oauth/token").unwrap();
    let callback = oauth::form(&[("u", &format!("{issuer}/deviceauth/callback"))]);
    assert_eq!(exchange.body, format!("grant_type=authorization_code&code=authcode&redirect_uri={}&client_id={}&code_verifier=server-verifier", &callback[2..], codex::CLIENT_ID));
}

#[tokio::test]
async fn xai_device_login_and_token_refresh() {
    let device = json!({"device_code": "dc", "user_code": "WXYZ", "verification_uri": "https://accounts.x.ai/device", "verification_uri_complete": "https://accounts.x.ai/device?user_code=WXYZ", "interval": 2, "expires_in": 600});
    let fresh = codex::tests::jwt(&json!({"exp": oauth::now_ms() / 1000 + 7200}));
    let (base, seen) = serve(vec![
        ("/oauth2/device/code", 200, JSON, device.to_string()),
        ("/oauth2/token", 400, JSON, r#"{"error":"authorization_pending"}"#.into()),
        ("/oauth2/token", 400, JSON, r#"{"error":"slow_down"}"#.into()),
        ("/oauth2/token", 200, JSON, json!({"access_token": "at1", "refresh_token": "rt1", "expires_in": 60}).to_string()),
        ("/oauth2/token", 200, JSON, json!({"access_token": fresh, "expires_in": 7200}).to_string()),
        ("/oauth2/token", 400, JSON, r#"{"error":"access_denied"}"#.into()),
    ])
    .await;
    let token_url = format!("{base}/oauth2/token");
    let mut login = xai::device_start_at(&http(), &format!("{base}/oauth2/device/code"), &token_url).await.unwrap();
    assert_eq!((login.url(), login.instructions().as_str(), login.interval_ms), ("https://accounts.x.ai/device?user_code=WXYZ", "Open https://accounts.x.ai/device on any device and enter code: WXYZ", 2000));
    assert_eq!(xai::device_poll(&http(), &mut login).await.unwrap(), Poll::Pending { wait_ms: 5000 });
    // `slow_down` lengthens every later wait by five seconds.
    assert_eq!(xai::device_poll(&http(), &mut login).await.unwrap(), Poll::Pending { wait_ms: 10_000 });
    let Poll::Success(info) = xai::device_poll(&http(), &mut login).await.unwrap() else { panic!("no token") };
    assert!(matches!(&info, auth::Info::Oauth { refresh, access, .. } if refresh == "rt1" && access == "at1"));

    // The stored token expires within the skew, so the next request refreshes it and keeps the old refresh token.
    let dir = tempfile::tempdir().unwrap();
    let config = xai::Config { auth_file: dir.path().join("auth.json"), token_url: token_url.clone() };
    oauth::store(&config.auth_file, xai::PROVIDER_ID, info).unwrap();
    assert_eq!(xai::token(&http(), &config).await.unwrap(), fresh);
    assert_eq!(xai::token(&http(), &config).await.unwrap(), fresh);
    assert!(matches!(&auth::read_all(&config.auth_file)[0].1, auth::Info::Oauth { refresh, access, .. } if refresh == "rt1" && *access == fresh));
    assert_eq!(xai::device_poll(&http(), &mut login).await.unwrap(), Poll::Failed);

    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].body, format!("client_id={}&scope=openid+profile+email+offline_access+grok-cli%3Aaccess+api%3Aaccess&referrer=opencode", xai::CLIENT_ID));
    assert_eq!((seen[0].header("accept"), seen[0].header("content-type")), (Some("application/json"), Some("application/x-www-form-urlencoded")));
    assert_eq!(seen[1].body, format!("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&client_id={}&device_code=dc", xai::CLIENT_ID));
    assert_eq!(seen[4].body, format!("grant_type=refresh_token&refresh_token=rt1&client_id={}", xai::CLIENT_ID));
    assert_eq!(seen.len(), 6);
}

#[tokio::test]
async fn snowflake_login_refresh_and_stream_fixes() {
    let chunk = r#"data: {"choices":[{"delta":{"role":"","content":"hi"},"finish_reason":"stop"}]}"#;
    let (base, seen) = serve(vec![
        ("/oauth/token-request", 200, JSON, json!({"access_token": "sf1", "refresh_token": "sfr", "expires_in": 60}).to_string()),
        ("/oauth/token-request", 200, JSON, json!({"access_token": "sf2", "expires_in": 600}).to_string()),
        ("/oauth/token-request", 200, JSON, json!({"access_token": "sf3", "expires_in": 600}).to_string()),
        ("/api/v2/cortex/v1/chat/completions", 401, JSON, "{}".into()),
        ("/api/v2/cortex/v1/chat/completions", 200, SSE, format!("{chunk}\n\ndata: [DONE]\n\n")),
        ("/api/v2/cortex/v1/chat/completions", 400, JSON, r#"{"message":"Conversation complete"}"#.into()),
    ])
    .await;
    let login = snowflake::authorize_at("acct", Some("PUBLIC"), &base).await.unwrap();
    assert!(login.url.starts_with(&format!("{base}/oauth/authorize?client_id=LOCAL_APPLICATION&response_type=code&redirect_uri=http%3A%2F%2F127.0.0.1%3A")));
    assert_eq!((param(&login.url, "scope"), param(&login.url, "code_challenge_method"), param(&login.url, "state").len()), ("refresh_token session:role:PUBLIC".to_string(), "S256".to_string(), 64));
    let (state, redirect) = (param(&login.url, "state"), login.redirect_uri.clone());
    let waiting = tokio::spawn(async move { login.wait(&http()).await });
    assert_eq!(http().get(format!("{redirect}other")).send().await.unwrap().status().as_u16(), 404);
    assert!(http().get(format!("{redirect}?code=sfcode&state={state}")).send().await.unwrap().status().is_success());
    let info = waiting.await.unwrap().unwrap();
    assert!(matches!(&info, auth::Info::Oauth { refresh, access, account_id: Some(account), .. } if refresh == "sfr" && access == "sf1" && account == "acct"));

    let dir = tempfile::tempdir().unwrap();
    let config = snowflake::Config { auth_file: dir.path().join("auth.json"), origin: Some(base.clone()) };
    oauth::store(&config.auth_file, snowflake::PROVIDER_ID, info).unwrap();
    // Expiring within two minutes: refreshed, the refresh token kept.
    assert_eq!(snowflake::token(&http(), &config, false).await.unwrap(), "sf2");
    assert_eq!(snowflake::token(&http(), &config, false).await.unwrap(), "sf2");

    // A 401 forces one more refresh and a retry; the empty role in the stream is repaired.
    let entries = auth::read_all(&config.auth_file);
    let catalog = json!({"snowflake-cortex": {"id": "snowflake-cortex", "env": [], "npm": "@ai-sdk/openai-compatible", "models": {"claude-4-sonnet": {}}}});
    let cortex = provider::build(&catalog, &json!({"provider": {"snowflake-cortex": {}}}), &entries).remove(0);
    let mut target = provider::resolve(&cortex, &cortex.models[0]).unwrap();
    assert!(matches!(target.credential, Some(Credential::Snowflake(_))) && target.api_key.is_none());
    assert_eq!(target.url, "https://acct.snowflakecomputing.com/api/v2/cortex/v1/chat/completions");
    target.url = target.url.replace("https://acct.snowflakecomputing.com", &base);
    target.credential = Some(Credential::Snowflake(config.clone()));
    let mut events = Vec::new();
    client::stream_resolved(&http(), &target, &ask(), |e| events.push(e)).await.unwrap();
    assert_eq!(text(&events), "hi");
    let mut events = Vec::new();
    client::stream_resolved(&http(), &target, &ask(), |e| events.push(e)).await.unwrap();
    assert_eq!(events, [Event::Finish { reason: "stop".into(), usage: Default::default() }]);

    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].header("authorization"), Some("Basic TE9DQUxfQVBQTElDQVRJT046TE9DQUxfQVBQTElDQVRJT04="));
    assert!(seen[0].body.starts_with("grant_type=authorization_code&code=sfcode&redirect_uri=http%3A%2F%2F127.0.0.1%3A") && seen[0].body.contains("&client_id=LOCAL_APPLICATION&code_verifier="));
    assert_eq!(seen[1].body, "grant_type=refresh_token&refresh_token=sfr&client_id=LOCAL_APPLICATION");
    let calls: Vec<_> = seen.iter().filter(|s| s.path.ends_with("/chat/completions")).collect();
    assert_eq!(calls.iter().map(|c| c.header("authorization").unwrap()).collect::<Vec<_>>(), ["Bearer sf2", "Bearer sf3", "Bearer sf3"]);
    assert!(calls[0].body.contains("\"max_completion_tokens\":64") && !calls[0].body.contains("max_tokens"));
}

#[tokio::test]
async fn digitalocean_login_and_inference_routers() {
    let routers = json!({"model_routers": [{"name": "fast", "uuid": "u1", "description": "d"}, {"uuid": "nameless"}]}).to_string();
    let (api, seen) = serve(vec![("/v2/gen-ai/models/routers", 200, JSON, routers.clone()), ("/v2/gen-ai/models/routers", 200, JSON, json!({"model_routers": [{"name": "fast"}, {"name": "smart"}]}).to_string())]).await;
    let login = digitalocean::authorize_on(0, digitalocean::OAUTH_PORT).await.unwrap();
    assert!(login.url.starts_with(&format!("https://cloud.digitalocean.com/v1/oauth/authorize?response_type=token&client_id={}&redirect_uri=http%3A%2F%2Flocalhost%3A1456%2Fauth%2Fcallback&scope=genai%3Aread+inference%3Aquery&state=", digitalocean::CLIENT_ID)));
    let (state, local) = (param(&login.url, "state"), format!("http://127.0.0.1:{}", login.port()));
    assert_eq!(state.len(), 64);
    let api_base = api.clone();
    let waiting = tokio::spawn(async move { login.wait(&http(), &api_base).await });
    // The redirect lands on a page that relays the URL fragment to the token route.
    let page = http().get(format!("{local}/auth/callback")).send().await.unwrap().text().await.unwrap();
    assert!(page.contains("\"/auth/token\"") && page.contains("access_token"));
    let relay = http().post(format!("{local}/auth/token")).body(json!({"access_token": "do_tok", "expires_in": "3600", "state": state}).to_string()).send().await.unwrap();
    assert_eq!(relay.text().await.unwrap(), r#"{"ok":true}"#);
    let info = waiting.await.unwrap().unwrap();
    let auth::Info::Api { key, metadata: Some(metadata) } = &info else { panic!("not an api entry") };
    assert_eq!((key.as_str(), &metadata["oauth_access"], &metadata["oauth_scopes"]), ("do_tok", &json!("do_tok"), &json!("genai:read inference:query")));
    assert_eq!(digitalocean::parse_routers(metadata["routers"].as_str()), [digitalocean::Router { name: "fast".into(), uuid: Some("u1".into()), description: Some("d".into()) }]);

    // A wrong state is refused.
    let login = digitalocean::authorize_on(0, digitalocean::OAUTH_PORT).await.unwrap();
    let local = format!("http://127.0.0.1:{}", login.port());
    let api_base = api.clone();
    let waiting = tokio::spawn(async move { login.wait(&http(), &api_base).await });
    assert_eq!(http().post(format!("{local}/auth/token")).body(r#"{"access_token":"t","state":"forged"}"#).send().await.unwrap().status().as_u16(), 400);
    assert!(matches!(waiting.await.unwrap(), Err(Error::Config(m)) if m.contains("CSRF")));

    // Fresh router metadata is served from auth.json; stale metadata is refreshed and written back.
    let dir = tempfile::tempdir().unwrap();
    let auth_file = dir.path().join("auth.json");
    let catalog = json!({"digitalocean": {"id": "digitalocean", "env": [], "api": "https://inference.do-ai.run/v1", "models": {"llama": {}}}});
    let endpoints = hooks::Endpoints { digitalocean: api.clone(), ..Default::default() };
    let entries = vec![("digitalocean".to_string(), info.clone())];
    let found = hooks::discover(&http(), &catalog, &json!({}), &entries, &auth_file, &endpoints).await;
    let providers = provider::build_with(&catalog, &json!({}), &entries, &found);
    assert_eq!(providers[0].models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["llama", "router:fast"]);
    let target = provider::resolve(&providers[0], &providers[0].models[1]).unwrap();
    assert_eq!((target.url.as_str(), target.api_key.as_deref()), ("https://inference.do-ai.run/v1/chat/completions", Some("do_tok")));
    assert_eq!(seen.lock().unwrap().len(), 1);
    let mut stale = metadata.clone();
    stale.insert("routers_fetched_at".into(), "1".into());
    let entries = vec![("digitalocean".to_string(), auth::Info::Api { key: key.clone(), metadata: Some(stale) })];
    let found = hooks::discover(&http(), &catalog, &json!({}), &entries, &auth_file, &endpoints).await;
    assert_eq!(found.models_for("digitalocean").unwrap().iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["llama", "router:fast", "router:smart"]);
    assert!(matches!(&auth::read_all(&auth_file)[0].1, auth::Info::Api { metadata: Some(m), .. } if m["routers"].as_str().unwrap().contains("smart")));
    let seen = seen.lock().unwrap();
    assert_eq!((seen.len(), seen[1].header("authorization")), (2, Some("Bearer do_tok")));
}

#[tokio::test]
async fn vertex_access_token_from_user_credentials() {
    let (base, seen) = serve(vec![("/token", 200, JSON, json!({"access_token": "ya29.x", "expires_in": 3599}).to_string())]).await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("adc.json");
    std::fs::write(&file, json!({"type": "authorized_user", "client_id": "cid", "client_secret": "cs", "refresh_token": "1//r"}).to_string()).unwrap();
    let config = vertex::Config { credentials_file: Some(file.clone()), token_url: format!("{base}/token"), metadata_url: format!("{base}/none") };
    assert_eq!(vertex::token(&http(), &config).await.unwrap(), "ya29.x");
    // Cached until shortly before expiry.
    assert_eq!(vertex::token(&http(), &config).await.unwrap(), "ya29.x");
    {
        let seen = seen.lock().unwrap();
        assert_eq!((seen.len(), seen[0].body.as_str()), (1, "refresh_token=1%2F%2Fr&client_id=cid&client_secret=cs&grant_type=refresh_token"));
    }
    std::fs::write(&file, json!({"type": "service_account", "client_email": "x"}).to_string()).unwrap();
    let other = vertex::Config { token_url: format!("{base}/token2"), ..config };
    assert!(matches!(vertex::token(&http(), &other).await, Err(Error::Config(m)) if m.starts_with("not supported yet: Google credentials of type \"service_account\"")));
}

struct Plugin {
    fetched: Mutex<Vec<(String, HttpRequest)>>,
}

impl AuthLoader for Plugin {
    fn options<'a>(&'a self, provider_id: &'a str, stored: &'a auth::Info, info: &'a Value) -> BoxFuture<'a, Option<Loaded>> {
        Box::pin(async move {
            let known = provider_id == "acme" && matches!(stored, auth::Info::Oauth { .. }) && info["models"].get("small").is_some();
            known.then(|| Loaded { options: json!({"apiKey": "", "headers": {"x-plugin": "1"}}), fetch: Some("fetch-7".into()) })
        })
    }

    fn fetch<'a>(&'a self, handle: &'a str, request: HttpRequest) -> BoxFuture<'a, Result<HttpResponse, Error>> {
        self.fetched.lock().unwrap().push((handle.to_string(), request));
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"via plugin\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        Box::pin(async move { Ok(HttpResponse::buffered(200, vec![("content-type".into(), "text/event-stream".into())], body.as_bytes().to_vec())) })
    }

    fn models<'a>(&'a self, provider_id: &'a str, info: &'a Value, _stored: Option<&'a auth::Info>) -> BoxFuture<'a, Option<Value>> {
        Box::pin(async move {
            let mut small = info["models"].get("small")?.clone();
            small["limit"]["context"] = 4242.into();
            (provider_id == "acme").then(|| json!({"small": small, "extra": {"id": "extra", "api": {"id": "extra-v2", "url": "https://api.acme.test/v2", "npm": "@ai-sdk/openai-compatible"}, "capabilities": {"toolcall": false}}}))
        })
    }
}

#[tokio::test]
async fn plugin_models_options_and_fetch_are_injected() {
    let catalog = json!({
        "acme": {"id": "acme", "name": "Acme", "env": [], "api": "https://api.acme.test/v1", "models": {"small": {"id": "small", "limit": {"context": 1, "output": 2}}, "big": {"id": "big"}}},
        "plain": {"id": "plain", "env": [], "api": "https://plain.test/v1", "models": {"m": {}}}
    });
    let entries = vec![
        ("acme".to_string(), auth::Info::Oauth { refresh: "r".into(), access: "a".into(), expires: 0, account_id: None, enterprise_url: None }),
        ("plain".to_string(), auth::Info::Api { key: "pk".into(), metadata: None }),
    ];
    // Without a loader an unknown OAuth login cannot be used.
    assert!(provider::build(&catalog, &json!({}), &entries).iter().all(|p| p.id != "acme"));

    let plugin = Plugin { fetched: Mutex::new(Vec::new()) };
    let overrides = hooks::load(&plugin, &catalog, &json!({}), &entries, Overrides::default()).await;
    assert_eq!((overrides.fetch.clone(), overrides.options.len()), (vec![("acme".to_string(), "fetch-7".to_string())], 1));
    let providers = provider::build_with(&catalog, &json!({}), &entries, &overrides);
    assert_eq!(providers.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["acme", "plain"]);
    let acme = &providers[0];
    assert_eq!(acme.models.iter().map(|m| (m.id.as_str(), m.api_id.as_str(), m.limit_context, m.toolcall)).collect::<Vec<_>>(), [("small", "small", 4242, true), ("extra", "extra-v2", 0, false)]);
    assert_eq!((acme.fetch.as_deref(), provider::resolve(acme, &acme.models[1]).unwrap().url.as_str()), (Some("fetch-7"), "https://api.acme.test/v2/chat/completions"));
    assert!(providers[1].fetch.is_none());
    // The round trip through `model_info` keeps what the registry knows.
    let again = provider::model_from_info("acme", "small", &provider::model_info(&acme.models[0]));
    assert_eq!((again.limit_context, again.limit_output, again.api_url.as_str(), again.npm.as_str()), (4242, 2, "https://api.acme.test/v1", "@ai-sdk/openai-compatible"));

    let mut events = Vec::new();
    client::stream_via(&http(), &plugin, acme, &acme.models[0], &ask(), |e| events.push(e)).await.unwrap();
    assert_eq!(text(&events), "via plugin");
    let fetched = plugin.fetched.lock().unwrap();
    let (handle, request) = &fetched[0];
    assert_eq!((handle.as_str(), request.method.as_str(), request.url.as_str()), ("fetch-7", "POST", "https://api.acme.test/v1/chat/completions"));
    // The loader's headers ride along; the plugin's fetch adds the credential itself.
    assert_eq!((header(&request.headers, "x-plugin"), header(&request.headers, "authorization")), (Some("1"), None));
    assert_eq!(serde_json::from_slice::<Value>(&request.body).unwrap()["messages"][1], json!({"role": "user", "content": "q"}));
}
