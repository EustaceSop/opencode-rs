// One entry point for every wire protocol: build the HTTP request, stream it, decode to events.
use crate::hooks::{AuthLoader, Executor, HttpRequest, PluginFetch};
use crate::provider::{self, Credential, Model, Protocol, Provider, Resolved};
use crate::{Completion, Error, Event, Request, anthropic, azure, bedrock, chat, codex, copilot, dialect, eventstream, gemini, responses, shape, sigv4, snowflake, sse, transform, vertex, xai};
use futures_util::StreamExt;
use serde_json::Value;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

fn set(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    headers.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
    headers.push((name.to_string(), value.to_string()));
}

// The exact request sent for `request`; `token` is the ChatGPT credential when the target uses one.
pub fn prepare(target: &Resolved, request: &Request, token: Option<&codex::Token>) -> Result<Prepared, Error> {
    let mut request = request.clone();
    if request.options.is_null() {
        request.options = target.options.clone();
        request.cache = target.cache;
    }
    let openai = target.provider_id == codex::PROVIDER_ID;
    if openai && target.protocol == Protocol::Responses {
        request.max_tokens = None;
    }
    if token.is_some() && target.copilot.is_none() && !request.system.is_empty() {
        // The ChatGPT backend takes the system prompt as `instructions`, never as input messages.
        if !request.options.is_object() {
            request.options = Value::Object(Default::default());
        }
        if request.options.get("instructions").is_none() {
            request.options["instructions"] = request.system.join("\n").into();
        }
        request.system.clear();
    }
    let npm = target.model.as_ref().map(|m| m.npm.as_str()).unwrap_or_default();
    let via_copilot = npm == copilot::NPM;
    if let Some(model) = &target.model {
        request.messages = transform::messages(std::mem::take(&mut request.messages), model);
        for tool in &mut request.tools {
            tool.parameters = transform::schema(model, &tool.parameters);
        }
        if copilot::is_provider(&model.provider_id) && request.tools.is_empty() && transform::has_tool_history(&request.messages) {
            request.tools.push(transform::noop_tool());
        }
    }
    let mut body = match target.protocol {
        Protocol::Chat => {
            // Only the generic package (and Copilot's) replays reasoning in the catalog's interleaved field.
            let field = target.model.as_ref().filter(|m| !dialect::own_builder(&m.npm)).and_then(|m| m.interleaved.clone());
            let mut body = chat::encode_with(&request, &chat::Config { include_usage: target.include_usage, reasoning_field: field, copilot: via_copilot })?;
            dialect::adapt(&mut body, npm, &request);
            body
        }
        Protocol::Anthropic => anthropic::encode(&request)?,
        Protocol::Responses => responses::encode_with(&request, &responses::Config { copilot: via_copilot, ungated: npm == "@ai-sdk/xai" })?,
        Protocol::Gemini => gemini::encode(&request)?,
        Protocol::Bedrock => bedrock::encode(&request)?,
    };
    if npm == vertex::ANTHROPIC_NPM {
        vertex::anthropic_body(&mut body);
    }
    if target.provider_id == snowflake::PROVIDER_ID {
        snowflake::rewrite_body(&mut body);
    }

    let mut headers = Vec::new();
    set(&mut headers, "User-Agent", &shape::user_agent());
    // Only the Anthropic SDK's own provider sends the version header; Vertex and AIHubMix reuse just its model class.
    if target.protocol == Protocol::Anthropic && npm != vertex::ANTHROPIC_NPM && npm != provider::AIHUBMIX_NPM {
        set(&mut headers, "anthropic-version", anthropic::VERSION);
    }
    match (&target.api_key, target.protocol) {
        (Some(key), _) if npm == azure::NPM => set(&mut headers, azure::API_KEY_HEADER, key),
        (Some(key), Protocol::Anthropic) => set(&mut headers, "x-api-key", key),
        (Some(key), Protocol::Gemini) => set(&mut headers, gemini::API_KEY_HEADER, key),
        (Some(key), _) => set(&mut headers, "Authorization", &format!("Bearer {key}")),
        (None, _) => {}
    }
    if openai {
        for (name, value) in codex::headers(None) {
            set(&mut headers, &name, &value);
        }
    }
    for (name, value) in target.headers.iter().chain(&request.headers) {
        set(&mut headers, name, value);
    }
    if target.protocol == Protocol::Anthropic {
        // The SDK lists the betas the body needs first, then the configured ones, lowercased and deduplicated.
        let existing = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("anthropic-beta")).map(|(_, v)| v.to_lowercase()).unwrap_or_default();
        let mut betas: Vec<&str> = anthropic::betas(&request);
        for beta in existing.split(',').map(str::trim).filter(|b| !b.is_empty()) {
            if !betas.contains(&beta) {
                betas.push(beta);
            }
        }
        if !betas.is_empty() {
            set(&mut headers, "anthropic-beta", &betas.join(","));
        }
    }
    let mut url = target.url.clone();
    if let Some(token) = token.filter(|_| target.copilot.is_some()) {
        copilot::apply_headers(&mut headers, &url, &body, &token.access);
    } else if let Some(token) = token {
        set(&mut headers, "authorization", &format!("Bearer {}", token.access));
        if let Some(account) = &token.account_id {
            set(&mut headers, "ChatGPT-Account-Id", account);
        }
        let endpoint = target.codex.as_ref().map(|c| c.endpoint.as_str()).unwrap_or(codex::API_ENDPOINT);
        if let Some(rewritten) = codex::rewrite_url(&url, endpoint) {
            url = rewritten;
            if let Some(residency) = codex::residency(&token.access) {
                set(&mut headers, "x-openai-internal-codex-residency", &residency);
            }
        }
    }
    set(&mut headers, "content-type", "application/json");
    Ok(Prepared { url, headers, body })
}

enum Decoder {
    Chat(chat::Decoder),
    Anthropic(anthropic::Decoder),
    Responses(responses::Decoder),
    Gemini(gemini::Decoder),
    Bedrock(bedrock::Decoder),
}

// Response bytes to events: SSE lines for most protocols, AWS event-stream frames for Bedrock.
struct Pipeline {
    sse: sse::Decoder,
    frames: eventstream::Decoder,
    decoder: Decoder,
    // Snowflake streams deltas with an empty role.
    fix_roles: bool,
}

impl Pipeline {
    fn new(target: &Resolved) -> Self {
        let decoder = match target.protocol {
            Protocol::Chat => Decoder::Chat(chat::Decoder::default()),
            Protocol::Anthropic => Decoder::Anthropic(anthropic::Decoder::default()),
            Protocol::Responses => Decoder::Responses(responses::Decoder::default()),
            Protocol::Gemini => Decoder::Gemini(gemini::Decoder::default()),
            Protocol::Bedrock => Decoder::Bedrock(bedrock::Decoder::default()),
        };
        Pipeline { sse: sse::Decoder::default(), frames: eventstream::Decoder::default(), decoder, fix_roles: target.provider_id == snowflake::PROVIDER_ID }
    }

    fn push(&mut self, data: &str) -> Result<Vec<Event>, Error> {
        match &mut self.decoder {
            Decoder::Chat(d) => d.push(data),
            Decoder::Anthropic(d) => d.push(data),
            Decoder::Responses(d) => d.push(data),
            Decoder::Gemini(d) => d.push(data),
            Decoder::Bedrock(_) => Ok(Vec::new()),
        }
    }

    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Event>, Error> {
        let mut events = Vec::new();
        if let Decoder::Bedrock(decoder) = &mut self.decoder {
            for frame in self.frames.feed(chunk)? {
                events.extend(decoder.push(&frame)?);
            }
            return Ok(events);
        }
        let fixed;
        let chunk = if self.fix_roles {
            fixed = snowflake::fix_roles(&String::from_utf8_lossy(chunk));
            fixed.as_bytes()
        } else {
            chunk
        };
        for data in self.sse.feed(chunk) {
            events.extend(self.push(&data)?);
        }
        Ok(events)
    }

    fn finish(mut self) -> Result<(Vec<Event>, Completion), Error> {
        let mut events = Vec::new();
        for data in self.sse.finish() {
            events.extend(self.push(&data)?);
        }
        if self.frames.pending() > 0 {
            return Err(Error::Stream("Incomplete Amazon Bedrock event-stream frame".into()));
        }
        let (tail, completion) = match self.decoder {
            Decoder::Chat(d) => d.finish_with(),
            Decoder::Anthropic(d) => (Vec::new(), d.finish()?),
            Decoder::Responses(d) => (Vec::new(), d.finish()?),
            Decoder::Gemini(d) => d.finish()?,
            Decoder::Bedrock(d) => d.finish()?,
        };
        events.extend(tail);
        Ok((events, completion))
    }
}

// Streams one model turn against an already resolved target, within its header, chunk and total timeouts.
pub async fn stream_resolved(http: &reqwest::Client, target: &Resolved, request: &Request, on_event: impl FnMut(Event)) -> Result<Completion, Error> {
    stream_with(http, http, target, request, on_event).await
}

// `stream_resolved` with the HTTP exchange delegated to `executor` (a plugin `fetch`, a proxy, a test double). Token
// refreshes of the built-in logins still use `http`.
pub async fn stream_with(executor: &dyn Executor, http: &reqwest::Client, target: &Resolved, request: &Request, mut on_event: impl FnMut(Event)) -> Result<Completion, Error> {
    let run = run(executor, http, target, request, &mut on_event);
    match target.timeouts.total {
        Some(ms) => tokio::time::timeout(Duration::from_millis(ms), run).await.unwrap_or_else(|_| Err(Error::total_timeout())),
        None => run.await,
    }
}

// Signs a Bedrock request with SigV4, or attaches the bearer token; `body` is the exact payload sent.
pub fn authorize_bedrock(prepared: &mut Prepared, body: &[u8], config: &bedrock::Config, auth: &bedrock::Auth) -> Result<(), Error> {
    match auth {
        bedrock::Auth::Bearer(token) => set(&mut prepared.headers, "Authorization", &format!("Bearer {token}")),
        bedrock::Auth::SigV4(credentials) => {
            let signing = sigv4::Signing { method: "POST", url: &prepared.url, body, region: &config.region, service: config.service, time: config.time.unwrap_or_else(sigv4::now) };
            sigv4::sign(&mut prepared.headers, credentials, &signing)?;
        }
    }
    Ok(())
}

// Attaches the per-request credential of `target`; `refresh` forces a new token where the login supports it.
async fn authorize(http: &reqwest::Client, target: &Resolved, prepared: &mut Prepared, body: &[u8], refresh: bool) -> Result<(), Error> {
    let bearer = match &target.credential {
        None => return Ok(()),
        Some(Credential::Bedrock(config)) => return authorize_bedrock(prepared, body, config, &bedrock::resolve(config, &bedrock::Source::process())?),
        Some(Credential::Vertex(config)) => vertex::token(http, config).await?,
        Some(Credential::Azure(config)) => {
            prepared.headers.retain(|(k, _)| !k.eq_ignore_ascii_case("api-key") && !k.eq_ignore_ascii_case("x-api-key"));
            azure::token(config, azure::scope_for(&prepared.url)).await?
        }
        Some(Credential::Xai(config)) => xai::token(http, config).await?,
        Some(Credential::Snowflake(config)) => snowflake::token(http, config, refresh).await?,
    };
    set(&mut prepared.headers, "authorization", &format!("Bearer {bearer}"));
    Ok(())
}

async fn run(executor: &dyn Executor, http: &reqwest::Client, target: &Resolved, request: &Request, on_event: &mut impl FnMut(Event)) -> Result<Completion, Error> {
    let token = match (&target.codex, &target.copilot) {
        (Some(config), _) => Some(codex::token(http, config).await?),
        (None, Some(config)) => Some(codex::Token { access: copilot::token(config)?, account_id: None }),
        (None, None) => None,
    };
    let mut prepared = prepare(target, request, token.as_ref())?;
    let body = prepared.body.to_string().into_bytes();
    authorize(http, target, &mut prepared, &body, false).await?;
    let send = |prepared: &Prepared| {
        let exchange = executor.execute(HttpRequest { method: "POST".into(), url: prepared.url.clone(), headers: prepared.headers.clone(), body: body.clone() });
        async move {
            match target.timeouts.header {
                Some(ms) => tokio::time::timeout(Duration::from_millis(ms), exchange).await.map_err(|_| Error::header_timeout(ms))?,
                None => exchange.await,
            }
        }
    };
    let mut response = send(&prepared).await?;
    // A Snowflake login retries once with a fresh token.
    if response.status == 401 && matches!(target.credential, Some(Credential::Snowflake(_))) {
        authorize(http, target, &mut prepared, &body, true).await?;
        response = send(&prepared).await?;
    }
    let status = response.status;
    if !(200..300).contains(&status) {
        let headers = response.headers.clone();
        let body = response.text().await.unwrap_or_default();
        if target.provider_id == snowflake::PROVIDER_ID && snowflake::conversation_complete(status, &body) {
            on_event(Event::Finish { reason: "stop".into(), usage: Default::default() });
            return Ok(Completion::default());
        }
        return Err(Error::Status { status, body, headers });
    }
    // The idle timer only guards event streams.
    let event_stream = response.header("content-type").is_some_and(|v| v.contains("text/event-stream"));
    let idle = target.timeouts.chunk.filter(|_| event_stream);
    let mut pipeline = Pipeline::new(target);
    let mut body = response.body;
    loop {
        let next = match idle {
            Some(ms) => tokio::time::timeout(Duration::from_millis(ms), body.next()).await.map_err(|_| Error::chunk_timeout())?,
            None => body.next().await,
        };
        let Some(chunk) = next else { break };
        pipeline.feed(&chunk?)?.into_iter().for_each(&mut *on_event);
    }
    let (events, completion) = pipeline.finish()?;
    events.into_iter().for_each(&mut *on_event);
    Ok(completion)
}

// Streams one model turn, picking the wire protocol from the model's SDK package.
pub async fn stream(http: &reqwest::Client, provider: &Provider, model: &Model, request: &Request, on_event: impl FnMut(Event)) -> Result<Completion, Error> {
    let target = provider::resolve(provider, model)?;
    stream_resolved(http, &target, request, on_event).await
}

// `stream` for a provider whose requests may go through a plugin `fetch`: when `provider.fetch` names a handle, the
// exchange is delegated to `loader.fetch`; otherwise it behaves exactly like `stream`.
pub async fn stream_via(http: &reqwest::Client, loader: &dyn AuthLoader, provider: &Provider, model: &Model, request: &Request, on_event: impl FnMut(Event)) -> Result<Completion, Error> {
    let target = provider::resolve(provider, model)?;
    match &target.fetch {
        Some(handle) => stream_with(&PluginFetch { loader, handle }, http, &target, request, on_event).await,
        None => stream_resolved(http, &target, request, on_event).await,
    }
}

pub async fn stream_chat(http: &reqwest::Client, target: &Resolved, request: &Request, on_event: impl FnMut(Event)) -> Result<(), Error> {
    stream_resolved(http, target, request, on_event).await.map(|_| ())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::testutil::{cassette, request_body, response_body};
    use crate::{Message, Tool, ToolCall, catalog, retry};
    use opencode_core::auth;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Debug, Clone)]
    pub(crate) struct Seen {
        pub path: String,
        pub headers: Vec<(String, String)>,
        pub body: String,
    }

    impl Seen {
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
        }
    }

    pub(crate) type Route = (&'static str, u16, &'static str, String);

    // Minimal HTTP/1.1 server: each route answers its path once per request, in order of registration.
    pub(crate) async fn serve(routes: Vec<Route>) -> (String, Arc<Mutex<Vec<Seen>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let routes = Arc::new(Mutex::new(routes));
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let (log, routes) = (log.clone(), routes.clone());
                tokio::spawn(async move {
                    let mut raw = Vec::new();
                    let mut buffer = [0u8; 8192];
                    let (head_end, length) = loop {
                        let n = socket.read(&mut buffer).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        raw.extend_from_slice(&buffer[..n]);
                        if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&raw[..end]).to_lowercase();
                            let length = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0);
                            break (end + 4, length);
                        }
                    };
                    while raw.len() < head_end + length {
                        let n = socket.read(&mut buffer).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        raw.extend_from_slice(&buffer[..n]);
                    }
                    let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
                    let mut lines = head.lines();
                    let path = lines.next().unwrap_or_default().split(' ').nth(1).unwrap_or_default().to_string();
                    let headers = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
                    let body = String::from_utf8_lossy(&raw[head_end..]).to_string();
                    log.lock().unwrap().push(Seen { path: path.clone(), headers, body });
                    let route = {
                        let mut routes = routes.lock().unwrap();
                        let index = routes.iter().position(|r| r.0 == path);
                        index.map(|i| if routes.iter().filter(|r| r.0 == path).count() > 1 { routes.remove(i) } else { routes[i].clone() })
                    };
                    let (status, extra, payload) = match route {
                        Some((_, status, extra, payload)) => (status, extra, payload),
                        None => (404, "", String::from("not found")),
                    };
                    let response = format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n{extra}\r\n{payload}", payload.len());
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (base, seen)
    }

    pub(crate) fn http() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    pub(crate) const SSE: &str = "content-type: text/event-stream\r\n";

    pub(crate) fn target(protocol: Protocol, provider_id: &str, url: String) -> Resolved {
        Resolved {
            url,
            api_key: Some("sk-test".into()),
            headers: Vec::new(),
            max_tokens: 32_000,
            protocol,
            provider_id: provider_id.into(),
            options: json!({}),
            cache: false,
            codex: None,
            copilot: None,
            model: None,
            timeouts: provider::Timeouts::default(),
            include_usage: true,
            credential: None,
            fetch: None,
        }
    }

    // Serves one scripted exchange: wait, send the head, wait, send the rest, then keep the socket open.
    async fn slow(head_delay: u64, head: &'static str, body_delay: u64, rest: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                tokio::spawn(async move {
                    let mut buffer = [0u8; 8192];
                    let _ = socket.read(&mut buffer).await;
                    tokio::time::sleep(Duration::from_millis(head_delay)).await;
                    let _ = socket.write_all(head.as_bytes()).await;
                    tokio::time::sleep(Duration::from_millis(body_delay)).await;
                    let _ = socket.write_all(rest.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        base
    }

    #[tokio::test]
    async fn header_chunk_and_total_timeouts() {
        const HEAD: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
        const JSON_HEAD: &str = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n";
        const DONE: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let request = Request { model: "m".into(), messages: vec![Message::User("q".into())], ..Default::default() };
        let run = |base: String, timeouts: provider::Timeouts| {
            let request = request.clone();
            async move {
                let mut events = Vec::new();
                let target = Resolved { timeouts, ..target(Protocol::Chat, "x", format!("{base}/chat/completions")) };
                stream_resolved(&http(), &target, &request, |e| events.push(e)).await.map(|_| text(&events))
            }
        };
        let limits = |header, chunk, total| provider::Timeouts { header, chunk, total };

        let error = run(slow(400, HEAD, 0, DONE).await, limits(Some(60), None, None)).await.unwrap_err();
        assert!(matches!(&error, Error::Timeout { code: "HeaderTimeoutError", message } if message == "Provider response headers timed out after 60ms"), "{error:?}");
        assert!(retry::classify(&error, "x").retryable);
        assert_eq!(run(slow(100, HEAD, 0, DONE).await, limits(None, Some(2000), None)).await.unwrap(), "hi");

        let error = run(slow(0, HEAD, 400, DONE).await, limits(Some(2000), Some(60), None)).await.unwrap_err();
        assert!(matches!(&error, Error::Timeout { code: "ResponseStreamError", message } if message == "SSE read timed out"), "{error:?}");
        assert!(retry::classify(&error, "x").retryable);
        // The idle timer ignores bodies that are not event streams.
        assert_eq!(run(slow(0, JSON_HEAD, 200, DONE).await, limits(None, Some(60), None)).await.unwrap(), "hi");

        let error = run(slow(0, HEAD, 400, DONE).await, limits(None, None, Some(80))).await.unwrap_err();
        assert!(matches!(&error, Error::Timeout { code: "TimeoutError", .. }) && !retry::classify(&error, "x").retryable, "{error:?}");

        let options = provider::Timeouts::from_options;
        assert_eq!(options(&json!({})), limits(Some(300_000), Some(300_000), None));
        assert_eq!(options(&json!({"headerTimeout": false, "chunkTimeout": false, "timeout": 1000})), limits(None, None, Some(1000)));
        assert_eq!(options(&json!({"headerTimeout": 5, "chunkTimeout": 0, "timeout": false})), limits(Some(5), None, None));
    }

    #[tokio::test]
    async fn catalog_fetch_caches_locks_and_refreshes() {
        let api = r#"{"p":{"id":"p","models":{}}}"#;
        let (base, seen) = serve(vec![("/api.json", 503, "", "busy".into()), ("/api.json", 200, "", api.into()), ("/api.json", 200, "", r#"{"q":{}}"#.into()), ("/api.json", 200, "", r#"{"q":{}}"#.into())]).await;
        let dir = tempfile::tempdir().unwrap();
        let mut source = catalog::Source { url: base, file: dir.path().join("cache").join("models.json"), locks: dir.path().join("state").join("locks"), override_file: None, disabled: true };
        assert_eq!(catalog::load(&http(), &source).await, json!({}));
        assert!(!catalog::refresh(&http(), &source, true).await.unwrap() && seen.lock().unwrap().is_empty());

        source.disabled = false;
        // The first answer is a transient failure and is retried.
        assert_eq!(catalog::load(&http(), &source).await["p"]["id"], "p");
        assert_eq!(std::fs::read_to_string(&source.file).unwrap(), api);
        assert_eq!(seen.lock().unwrap().len(), 2);
        assert_eq!(seen.lock().unwrap()[0].header("user-agent"), Some(catalog::user_agent().as_str()));
        let leftovers = |path: &std::path::Path| std::fs::read_dir(path).unwrap().count();
        assert_eq!((leftovers(source.file.parent().unwrap()), leftovers(&source.locks)), (1, 0));

        // A fresh cache is served from disk and not refreshed; a stale or forced one is fetched again.
        assert_eq!(catalog::load(&http(), &source).await["p"]["id"], "p");
        assert!(!catalog::refresh(&http(), &source, false).await.unwrap());
        assert_eq!(seen.lock().unwrap().len(), 2);
        let old = std::time::SystemTime::now() - Duration::from_secs(600);
        std::fs::OpenOptions::new().write(true).open(&source.file).unwrap().set_modified(old).unwrap();
        assert!(catalog::refresh(&http(), &source, false).await.unwrap());
        assert_eq!(provider::build(&catalog::read(&source).unwrap(), &json!({}), &Vec::new()).len(), 0);
        assert_eq!(catalog::read(&source).unwrap(), json!({"q": {}}));
        assert!(catalog::refresh(&http(), &source, true).await.unwrap());
        assert_eq!((seen.lock().unwrap().len(), leftovers(&source.locks)), (4, 0));
    }

    #[tokio::test]
    async fn copilot_device_login_and_request_headers() {
        let device = json!({"verification_uri": "https://github.com/login/device", "user_code": "AB12-CD34", "device_code": "dev", "interval": 5}).to_string();
        let Some(recording) = cassette("llm/test/fixtures/recordings/openai-compatible-chat/deepseek-streams-text.json") else { return };
        let Some(responses) = cassette("llm/test/fixtures/recordings/openai-responses/gpt-5-5-streams-text.json") else { return };
        let json_type = "content-type: application/json\r\n";
        let (base, seen) = serve(vec![
            ("/login/device/code", 200, json_type, device),
            ("/login/oauth/access_token", 200, json_type, r#"{"error":"authorization_pending"}"#.into()),
            ("/login/oauth/access_token", 200, json_type, r#"{"error":"slow_down","interval":12}"#.into()),
            ("/login/oauth/access_token", 200, json_type, r#"{"access_token":"gho_secret"}"#.into()),
            ("/login/oauth/access_token", 200, json_type, r#"{"error":"access_denied"}"#.into()),
            ("/chat/completions", 200, SSE, response_body(&recording, 0).to_string()),
            ("/responses", 200, SSE, response_body(&responses, 0).to_string()),
        ])
        .await;
        let started = copilot::start_at(&http(), &format!("{base}/login/device/code"), &format!("{base}/login/oauth/access_token"), Some("company.ghe.com".into())).await.unwrap();
        assert_eq!((started.verification_uri.as_str(), started.instructions().as_str(), started.interval), ("https://github.com/login/device", "Enter code: AB12-CD34", 5));
        assert_eq!(copilot::device_poll(&http(), &started).await.unwrap(), copilot::Poll::Pending { wait_ms: 8000 });
        assert_eq!(copilot::device_poll(&http(), &started).await.unwrap(), copilot::Poll::Pending { wait_ms: 15_000 });
        let copilot::Poll::Success(info) = copilot::device_poll(&http(), &started).await.unwrap() else { panic!("no token") };
        assert_eq!(info, auth::Info::Oauth { refresh: "gho_secret".into(), access: "gho_secret".into(), expires: 0, account_id: None, enterprise_url: Some("company.ghe.com".into()) });
        assert_eq!(copilot::device_poll(&http(), &started).await.unwrap(), copilot::Poll::Failed);
        {
            let seen = seen.lock().unwrap();
            assert_eq!(serde_json::from_str::<Value>(&seen[0].body).unwrap(), json!({"client_id": "Ov23li8tweQw6odWQebz", "scope": "read:user"}));
            assert_eq!((seen[0].header("accept"), seen[0].header("content-type"), seen[0].header("user-agent")), (Some("application/json"), Some("application/json"), Some(shape::user_agent().as_str())));
            assert_eq!(serde_json::from_str::<Value>(&seen[1].body).unwrap(), json!({"client_id": "Ov23li8tweQw6odWQebz", "device_code": "dev", "grant_type": "urn:ietf:params:oauth:grant-type:device_code"}));
        }

        // The stored GitHub token is the bearer; nothing is exchanged or refreshed.
        let dir = tempfile::tempdir().unwrap();
        let auth_file = dir.path().join("auth.json");
        auth::set(&auth_file, copilot::PROVIDER_ID, info).unwrap();
        let catalog = json!({"github-copilot": {"id": "github-copilot", "env": ["OC_TEST_UNSET_KEY8"], "npm": "@ai-sdk/openai-compatible", "api": "https://api.githubcopilot.com", "models": {
            "gpt-4.1": {"id": "gpt-4.1", "limit": {"context": 1, "output": 16000}, "modalities": {"input": ["text", "image"], "output": ["text"]}},
            "gpt-5.2": {"id": "gpt-5.2", "reasoning": true, "provider": {"npm": "@ai-sdk/openai"}},
            "gpt-5-mini": {"id": "gpt-5-mini"}
        }}});
        let providers = provider::build(&catalog, &json!({}), &auth::read_all(&auth_file));
        let copilot_provider = &providers[0];
        assert!(copilot_provider.oauth && copilot_provider.models.iter().all(|m| m.npm == copilot::NPM && m.api_url == "https://copilot-api.company.ghe.com"));
        let mut chat_target = provider::resolve(copilot_provider, &copilot_provider.models[0]).unwrap();
        assert_eq!((chat_target.url.as_str(), chat_target.protocol, &chat_target.api_key, chat_target.include_usage), ("https://copilot-api.company.ghe.com/chat/completions", Protocol::Chat, &None, false));
        let mut responses_target = provider::resolve(copilot_provider, &copilot_provider.models[1]).unwrap();
        assert_eq!((responses_target.url.as_str(), responses_target.protocol), ("https://copilot-api.company.ghe.com/responses", Protocol::Responses));
        assert_eq!(provider::resolve(copilot_provider, &copilot_provider.models[2]).unwrap().protocol, Protocol::Chat);
        for target in [&mut chat_target, &mut responses_target] {
            target.url = target.url.replace("https://copilot-api.company.ghe.com", &base);
            target.copilot = Some(copilot::Config { auth_file: auth_file.clone() });
        }

        let session = shape::Session { session_id: "ses_1", parent_session_id: Some("ses_0"), ..Default::default() };
        let mut request = Request {
            model: "gpt-4.1".into(),
            system: vec!["sys".into()],
            messages: vec![
                Message::User("q".into()),
                Message::assistant("", vec![ToolCall { id: "c1".into(), name: "shot".into(), arguments: "{}".into() }]),
                Message::tool("c1", "done"),
                Message::UserParts(vec![crate::Part::Text(copilot::SYNTHETIC_ATTACHMENT_PROMPT.into()), crate::Part::Media { mime: "image/png".into(), data: "AAAA".into(), filename: None }]),
            ],
            ..Default::default()
        };
        shape::apply(&mut request, copilot_provider, &copilot_provider.models[0], &session);
        assert_eq!(request.max_tokens, None);
        let mut events = Vec::new();
        stream_resolved(&http(), &chat_target, &request, |e| events.push(e)).await.unwrap();
        assert_eq!(text(&events), "Hello!");
        let mut plain = Request { model: "gpt-5.2".into(), system: vec!["sys".into()], messages: vec![Message::User("q".into())], ..Default::default() };
        shape::apply(&mut plain, copilot_provider, &copilot_provider.models[1], &shape::Session { session_id: "ses_2", ..Default::default() });
        stream_resolved(&http(), &responses_target, &plain, |_| {}).await.unwrap();

        let seen = seen.lock().unwrap();
        let call = &seen[5];
        assert_eq!((call.header("authorization"), call.header("openai-intent"), call.header("copilot-vision-request")), (Some("Bearer gho_secret"), Some("conversation-edits"), Some("true")));
        // A sub-agent session is agent-initiated whatever the last message is.
        assert_eq!((call.header("x-initiator"), call.header("x-github-api-version"), call.header("x-interaction-id")), (Some("agent"), Some("2026-06-01"), Some("ses_1")));
        assert_eq!((call.header("user-agent"), call.header("copilot-integration-id"), call.header("x-api-key")), (Some(shape::user_agent().as_str()), None, None));
        let body: Value = serde_json::from_str(&call.body).unwrap();
        assert!(body.get("stream_options").is_none() && body.get("max_tokens").is_none() && body.get("store").is_none());
        // Tool history without tools gets the placeholder tool.
        assert_eq!(body["tools"][0]["function"]["name"], "_noop");
        assert_eq!(body["messages"][1], json!({"role": "user", "content": "q"}));
        let call = &seen[6];
        assert_eq!((call.header("authorization"), call.header("x-initiator"), call.header("copilot-vision-request")), (Some("Bearer gho_secret"), Some("user"), None));
        let body: Value = serde_json::from_str(&call.body).unwrap();
        assert_eq!((&body["input"][0], &body["store"], &body["reasoning"]), (&json!({"role": "developer", "content": "sys"}), &json!(false), &json!({"effort": "medium", "summary": "auto"})));
    }

    #[tokio::test]
    async fn gemini_dispatch_and_request_shaping() {
        let Some(c) = cassette("llm/test/fixtures/recordings/gemini/streams-tool-call.json") else { return };
        let (base, seen) = serve(vec![("/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse", 200, SSE, response_body(&c, 0).to_string())]).await;
        let catalog = json!({"google": {"id": "google", "env": ["OC_TEST_UNSET_A", "OC_TEST_UNSET_B"], "npm": "@ai-sdk/google", "models": {
            "gemini-2.5-flash": {"id": "gemini-2.5-flash", "reasoning": true, "temperature": true, "limit": {"context": 1000000, "output": 65536}}
        }}});
        let config = json!({"provider": {"google": {"options": {"baseURL": format!("{base}/v1beta"), "apiKey": "g-key"}}}});
        let providers = provider::build(&catalog, &config, &Vec::new());
        let (google, model) = (&providers[0], &providers[0].models[0]);
        assert_eq!(provider::resolve(google, model).unwrap().protocol, Protocol::Gemini);
        let mut request = Request {
            model: model.api_id.clone(),
            system: vec!["Call tools exactly as requested.".into()],
            messages: vec![Message::User("Call get_weather with city exactly Paris.".into())],
            tools: vec![Tool {
                name: "get_weather".into(),
                description: "Get current weather for a city.".into(),
                parameters: json!({"type": "object", "properties": {"city": {"type": "string"}, "unit": {"type": "integer", "enum": [1, 2]}}, "required": ["city", "ghost"], "additionalProperties": false}),
            }],
            variant: Some("max".into()),
            ..Default::default()
        };
        shape::apply(&mut request, google, model, &shape::Session { session_id: "ses_1", ..Default::default() });
        assert_eq!((request.temperature, request.top_p, request.top_k, request.max_tokens, &request.variant), (Some(1.0), Some(0.95), Some(64), Some(32_000), &None));
        let mut events = Vec::new();
        let completion = stream(&http(), google, model, &request, |e| events.push(e)).await.unwrap();
        assert!(matches!(&events[0], Event::ToolCall(call) if call.name == "get_weather" && call.id == "tool_0"));
        assert_eq!(completion.reasoning.len(), 1);

        let seen = seen.lock().unwrap();
        assert_eq!((seen[0].header("x-goog-api-key"), seen[0].header("authorization")), (Some("g-key"), None));
        let body: Value = serde_json::from_str(&seen[0].body).unwrap();
        // The `max` variant replaces the default thinking level; the schema passes the Gemini sanitizer and the SDK projection.
        assert_eq!(
            body["generationConfig"],
            json!({"maxOutputTokens": 32000, "temperature": 1, "topK": 64, "topP": 0.95, "thinkingConfig": {"includeThoughts": true, "thinkingBudget": 24576}})
        );
        assert_eq!(
            body["tools"][0]["functionDeclarations"][0]["parameters"],
            json!({"required": ["city"], "type": "object", "properties": {"city": {"type": "string"}, "unit": {"type": "string", "enum": ["1", "2"]}}})
        );
    }

    pub(crate) fn text(events: &[Event]) -> String {
        events.iter().filter_map(|e| if let Event::TextDelta(t) = e { Some(t.as_str()) } else { None }).collect()
    }

    #[tokio::test]
    async fn chatgpt_login_refreshes_rewrites_and_streams() {
        let Some(c) = cassette("opencode/test/fixtures/recordings/session/native-openai-oauth-tool-loop.json") else { return };
        let access = codex::tests::jwt(&json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acct_new", "chatgpt_compute_residency": "eu"}}));
        let tokens = json!({"access_token": access, "refresh_token": "refresh-new", "expires_in": 600}).to_string();
        let (base, seen) = serve(vec![
            ("/oauth/token", 200, "content-type: application/json\r\n", tokens),
            ("/backend-api/codex/responses", 200, SSE, response_body(&c, 0).to_string()),
            ("/backend-api/codex/responses", 200, SSE, response_body(&c, 1).to_string()),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let auth_file = dir.path().join("auth.json");
        std::fs::write(&auth_file, r#"{"zai":{"type":"api","key":"keep"},"openai":{"type":"oauth","refresh":"refresh-old","access":"access-old","expires":1,"accountId":"acct_old"}}"#).unwrap();

        let catalog = json!({"openai": {"id": "openai", "env": [], "npm": "@ai-sdk/openai", "models": {"gpt-5.5": {"id": "gpt-5.5", "reasoning": true, "limit": {"context": 1, "output": 128000}}}}});
        let entries = auth::read_all(&auth_file);
        let providers = provider::build(&catalog, &json!({}), &entries);
        let mut target = provider::resolve(&providers[0], &providers[0].models[0]).unwrap();
        target.codex = Some(codex::Config { auth_file: auth_file.clone(), issuer: base.clone(), endpoint: format!("{base}/backend-api/codex/responses") });

        let system = "Answer using tools when appropriate.\nUse the get_weather tool exactly once to look up Paris, then reply with exactly: Paris is sunny.";
        let session = shape::Session { session_id: "session-recorded-openai-oauth-loop", ..Default::default() };
        let mut request = Request {
            model: "gpt-5.5".into(),
            system: vec![system.into()],
            messages: vec![Message::User("What is the weather in Paris?".into())],
            tools: vec![Tool {
                name: "get_weather".into(),
                description: "Get the current weather for a city.".into(),
                parameters: json!({"$schema": "http://json-schema.org/draft-07/schema#", "type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"], "additionalProperties": false}),
            }],
            ..Default::default()
        };
        shape::apply(&mut request, &providers[0], &providers[0].models[0], &session);

        // Recorded by the native runtime; the SDK path additionally lowers tool schemas for OpenAI.
        let recorded = |index: usize| {
            let mut body = request_body(&c, index);
            body["tools"][0]["parameters"] = transform::sanitize_openai(&body["tools"][0]["parameters"]);
            body
        };
        let mut events = Vec::new();
        let completion = stream_resolved(&http(), &target, &request, |e| events.push(e)).await.unwrap();
        let calls: Vec<ToolCall> = events.iter().filter_map(|e| if let Event::ToolCall(t) = e { Some(t.clone()) } else { None }).collect();
        assert_eq!(calls.len(), 1);
        assert!(matches!(events.last(), Some(Event::Finish { reason, .. }) if reason == "tool-calls"));

        {
            let seen = seen.lock().unwrap();
            assert_eq!(seen.iter().map(|s| s.path.as_str()).collect::<Vec<_>>(), ["/oauth/token", "/backend-api/codex/responses"]);
            assert_eq!(seen[0].body, format!("grant_type=refresh_token&refresh_token=refresh-old&client_id={}", codex::CLIENT_ID));
            assert_eq!(seen[0].header("content-type"), Some("application/x-www-form-urlencoded"));
            let call = &seen[1];
            assert_eq!(call.header("authorization"), Some(format!("Bearer {access}").as_str()));
            assert_eq!(call.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("authorization")).count(), 1);
            assert_eq!((call.header("chatgpt-account-id"), call.header("x-openai-internal-codex-residency")), (Some("acct_new"), Some("eu")));
            assert_eq!((call.header("originator"), call.header("session-id")), (Some("opencode"), Some("session-recorded-openai-oauth-loop")));
            assert_eq!(serde_json::from_str::<Value>(&call.body).unwrap(), recorded(0));
            assert!(call.body.starts_with(r#"{"model":"gpt-5.5","input":[{"role":"user","#) && call.body.ends_with(r#""text":{"verbosity":"low"},"stream":true}"#));
        }
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&auth_file).unwrap()).unwrap();
        assert_eq!(saved["zai"], json!({"type": "api", "key": "keep"}));
        assert_eq!((&saved["openai"]["type"], &saved["openai"]["refresh"], &saved["openai"]["access"], &saved["openai"]["accountId"]), (&json!("oauth"), &json!("refresh-new"), &json!(access), &json!("acct_new")));
        assert!(saved["openai"]["expires"].as_u64().unwrap() > 1_700_000_000_000);

        // Second turn reuses the stored token and replays the encrypted reasoning item.
        let replayed = ToolCall { arguments: "{\"city\":{}}".into(), ..calls[0].clone() };
        request.messages.push(Message::Assistant { text: String::new(), reasoning: String::new(), tool_calls: vec![replayed], reasoning_parts: completion.reasoning });
        request.messages.push(Message::tool(&calls[0].id, "{\"temperature\":22,\"condition\":\"sunny\"}"));
        let mut events = Vec::new();
        stream_resolved(&http(), &target, &request, |e| events.push(e)).await.unwrap();
        assert_eq!(text(&events), "Paris is sunny.");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(serde_json::from_str::<Value>(&seen[2].body).unwrap(), recorded(1));
    }

    #[tokio::test]
    async fn refresh_failure_is_reported() {
        let (base, _) = serve(vec![("/oauth/token", 401, "", "{}".into())]).await;
        let dir = tempfile::tempdir().unwrap();
        let auth_file = dir.path().join("auth.json");
        std::fs::write(&auth_file, r#"{"openai":{"type":"oauth","refresh":"r","access":"","expires":0}}"#).unwrap();
        let config = codex::Config { auth_file: auth_file.clone(), issuer: base, endpoint: codex::API_ENDPOINT.into() };
        let error = codex::token(&http(), &config).await.unwrap_err();
        assert!(matches!(&error, Error::Config(m) if m == "Token refresh failed: 401"), "{error}");
        std::fs::write(&auth_file, r#"{"openai":{"type":"api","key":"sk"}}"#).unwrap();
        assert!(matches!(codex::token(&http(), &config).await, Err(Error::Config(_))));
    }

    #[tokio::test]
    async fn anthropic_dispatch_headers_and_errors() {
        let Some(c) = cassette("llm/test/fixtures/recordings/anthropic-messages/streams-text.json") else { return };
        let (base, seen) = serve(vec![
            ("/v1/messages", 200, SSE, response_body(&c, 0).to_string()),
            ("/v1/messages", 429, "retry-after: 3\r\ncontent-type: application/json\r\n", r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#.into()),
        ])
        .await;
        let catalog = json!({"anthropic": {"id": "anthropic", "env": [], "npm": "@ai-sdk/anthropic", "models": {"claude-haiku-4-5-20251001": {"temperature": true, "limit": {"context": 200000, "output": 64000}}}}});
        let config = json!({"provider": {"anthropic": {"options": {"baseURL": format!("{base}/v1")}}}});
        let providers = provider::build(&catalog, &config, &vec![("anthropic".to_string(), auth::Info::Api { key: "sk-ant".into(), metadata: None })]);
        let request = Request {
            model: "claude-haiku-4-5-20251001".into(),
            system: vec!["You are concise.".into()],
            messages: vec![Message::User("Reply with exactly: Hello!".into())],
            max_tokens: Some(20),
            temperature: Some(0.0),
            options: json!({"thinking": {"type": "adaptive", "blockBinding": {"prefixMismatchBehavior": "drop_block"}}}),
            headers: vec![("x-session-affinity".into(), "ses_1".into())],
            ..Default::default()
        };
        let mut events = Vec::new();
        stream(&http(), &providers[0], &providers[0].models[0], &request, |e| events.push(e)).await.unwrap();
        assert_eq!(text(&events), "Hello!");
        let error = stream(&http(), &providers[0], &providers[0].models[0], &request, |_| {}).await.unwrap_err();
        let (retry, wait) = retry::next(&error, "anthropic", 1, 0.0).unwrap();
        assert_eq!((retry.message.as_str(), wait), ("slow down", 3000));

        let seen = seen.lock().unwrap();
        let call = &seen[0];
        assert_eq!((call.header("x-api-key"), call.header("anthropic-version"), call.header("authorization")), (Some("sk-ant"), Some("2023-06-01"), None));
        assert_eq!(call.header("anthropic-beta"), Some("thinking-binding-controls-2026-08-01,interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14"));
        assert_eq!((call.header("x-session-affinity"), call.header("content-type")), (Some("ses_1"), Some("application/json")));
        assert_eq!(call.header("user-agent"), Some(shape::user_agent().as_str()));
        let body: Value = serde_json::from_str(&call.body).unwrap();
        assert_eq!(body["thinking"], json!({"type": "adaptive", "block_binding": {"prefix_mismatch_behavior": "drop_block"}}));
    }

    #[tokio::test]
    async fn chat_and_responses_dispatch_with_bearer_keys() {
        let Some(chat) = cassette("llm/test/fixtures/recordings/openai-compatible-chat/deepseek-streams-text.json") else { return };
        let Some(resp) = cassette("llm/test/fixtures/recordings/openai-responses/gpt-5-5-streams-text.json") else { return };
        let (base, seen) = serve(vec![("/chat/completions", 200, SSE, response_body(&chat, 0).to_string()), ("/v1/responses", 200, SSE, response_body(&resp, 0).to_string())]).await;
        let request = Request {
            model: "deepseek-chat".into(),
            system: vec!["You are concise.".into()],
            messages: vec![Message::User("Reply with exactly: Hello!".into())],
            max_tokens: Some(20),
            temperature: Some(0.0),
            ..Default::default()
        };
        let mut events = Vec::new();
        stream_chat(&http(), &target(Protocol::Chat, "deepseek", format!("{base}/chat/completions")), &request, |e| events.push(e)).await.unwrap();
        assert_eq!(text(&events), "Hello!");

        // An API-key OpenAI target keeps its URL and key, and still drops the output cap.
        let mut openai = target(Protocol::Responses, "openai", format!("{base}/v1/responses"));
        openai.options = json!({"store": false});
        let mut events = Vec::new();
        let completion = stream_resolved(&http(), &openai, &Request { model: "gpt-5.5".into(), ..request.clone() }, |e| events.push(e)).await.unwrap();
        assert_eq!(text(&events), "Hello!");
        assert!(completion.response_id.is_some());

        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].body, chat["interactions"][0]["request"]["body"].as_str().unwrap());
        assert_eq!(seen[0].header("authorization"), Some("Bearer sk-test"));
        let body: Value = serde_json::from_str(&seen[1].body).unwrap();
        assert_eq!((seen[1].header("authorization"), seen[1].header("originator")), (Some("Bearer sk-test"), Some("opencode")));
        // GPT-5 reasons, so the system prompt travels as a developer message and sampling parameters are dropped.
        assert_eq!((&body["store"], body.get("max_output_tokens"), &body["input"][0]["role"], body.get("temperature")), (&json!(false), None, &json!("developer"), None));
    }

    #[test]
    fn unsupported_package_is_a_config_error() {
        let catalog = json!({"cohere": {"id": "cohere", "env": [], "npm": "@ai-sdk/cohere", "models": {"command-a": {}, "compat": {"provider": {"npm": "@ai-sdk/openai-compatible", "api": "https://api.cohere.ai/compatibility/v1"}}}}});
        let providers = provider::build(&catalog, &json!({}), &vec![("cohere".to_string(), auth::Info::Api { key: "k".into(), metadata: None })]);
        let error = provider::resolve(&providers[0], &providers[0].models[0]).unwrap_err();
        assert!(matches!(&error, Error::Config(m) if m.starts_with("cohere/command-a: not supported yet: @ai-sdk/cohere (Cohere v2 chat")), "{error}");
        assert_eq!(provider::resolve(&providers[0], &providers[0].models[1]).unwrap().url, "https://api.cohere.ai/compatibility/v1/chat/completions");
        assert_eq!(provider::protocol("@ai-sdk/openai-compatible"), Some(Protocol::Chat));
        assert_eq!(provider::protocol("@ai-sdk/anthropic"), Some(Protocol::Anthropic));
        assert_eq!(provider::protocol("@ai-sdk/openai"), Some(Protocol::Responses));
        assert_eq!(provider::protocol("@ai-sdk/google"), Some(Protocol::Gemini));
        assert_eq!(provider::protocol("@ai-sdk/amazon-bedrock"), Some(Protocol::Bedrock));
        assert_eq!(provider::protocol("@ai-sdk/gateway"), None);
    }
}
