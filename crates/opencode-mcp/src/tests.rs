use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use futures_util::FutureExt;
use indexmap::IndexMap;
use opencode_core::config::{McpConfig, McpOAuth, McpOAuthSetting, McpServer};
use opencode_tools::{PermissionAsker, PermissionRequest, Tool, ToolContext, ToolError};
use reqwest::Url;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::auth::{AuthStore, ClientInfo, Entry, Tokens, number};
use crate::test_server::{ServerState, Step, handle, response};
use crate::{AuthStatus, BrowserOpener, ManagerError, McpEvent, McpManager, McpOptions, Status};

// ---------------------------------------------------------------- fixtures

fn stdio_server() -> String {
    let exe = std::env::current_exe().unwrap();
    let debug = exe.parent().unwrap().parent().unwrap();
    let path = debug
        .join("examples")
        .join(format!("mcp_test_server{}", std::env::consts::EXE_SUFFIX));
    assert!(path.exists(), "missing test server at {}", path.display());
    path.to_string_lossy().into_owned()
}

fn local(args: &[&str]) -> McpServer {
    let mut command = vec![stdio_server()];
    command.extend(args.iter().map(|a| a.to_string()));
    McpServer::Local {
        command,
        cwd: None,
        environment: IndexMap::new(),
        enabled: None,
        timeout: None,
    }
}

fn remote(url: &str) -> McpServer {
    McpServer::Remote {
        url: url.to_string(),
        enabled: None,
        headers: IndexMap::new(),
        oauth: None,
        timeout: None,
    }
}

fn servers(entries: Vec<(&str, McpServer)>) -> IndexMap<String, McpConfig> {
    entries
        .into_iter()
        .map(|(name, server)| (name.to_string(), McpConfig::Server(server)))
        .collect()
}

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("work/sub")).unwrap();
        std::fs::create_dir_all(dir.path().join("data")).unwrap();
        Self { dir }
    }

    fn work(&self) -> PathBuf {
        self.dir.path().join("work")
    }

    fn data(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    fn options(&self) -> McpOptions {
        let env: HashMap<String, String> = std::env::vars().collect();
        McpOptions::new(self.work(), env, self.data())
    }

    fn auth_file(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.data().join("mcp-auth.json")).unwrap()).unwrap()
    }
}

#[derive(Default)]
struct Recorder {
    asked: Mutex<Vec<PermissionRequest>>,
    reject: bool,
}

#[async_trait::async_trait]
impl PermissionAsker for Recorder {
    async fn ask(&self, req: PermissionRequest) -> Result<(), ToolError> {
        self.asked.lock().unwrap().push(req);
        if self.reject {
            return Err(ToolError::Rejected);
        }
        Ok(())
    }
}

fn context(sandbox: &Sandbox, permissions: Arc<Recorder>) -> ToolContext {
    ToolContext {
        session_id: "ses".into(),
        message_id: "msg".into(),
        call_id: "call".into(),
        agent: "build".into(),
        cwd: sandbox.work(),
        worktree: sandbox.work(),
        data_dir: sandbox.data(),
        abort: CancellationToken::new(),
        permissions,
        hooks: None,
    }
}

fn find(tools: &[Arc<dyn Tool>], id: &str) -> Arc<dyn Tool> {
    tools
        .iter()
        .find(|tool| tool.id() == id)
        .unwrap_or_else(|| panic!("tool {id} not found"))
        .clone()
}

async fn wait_event(
    rx: &mut broadcast::Receiver<McpEvent>,
    matches: impl Fn(&McpEvent) -> bool,
) -> McpEvent {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = rx.recv().await.unwrap();
            if matches(&event) {
                return event;
            }
        }
    })
    .await
    .expect("event not published")
}

fn failed(status: &Status) -> &str {
    match status {
        Status::Failed { error } => error,
        other => panic!("expected failed, got {other:?}"),
    }
}

// ---------------------------------------------------------------- HTTP test server

#[derive(Clone, Default)]
struct RemoteConfig {
    legacy_sse: bool,
    sse_responses: bool,
    require_header: Option<(String, String)>,
    auth: bool,
    plain_401: bool,
    registration_unsupported: bool,
}

struct RemoteShared {
    config: RemoteConfig,
    base: String,
    hits: Mutex<Vec<String>>,
    state: Mutex<ServerState>,
    valid_tokens: Mutex<Vec<String>>,
    challenge: Mutex<Option<String>>,
    legacy: Mutex<Option<mpsc::UnboundedSender<String>>>,
    token_requests: Mutex<Vec<HashMap<String, String>>>,
    registrations: Mutex<Vec<Value>>,
}

struct Remote {
    shared: Arc<RemoteShared>,
    cancel: CancellationToken,
}

impl Drop for Remote {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

struct Request {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: String,
}

async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(position) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break position;
        }
        match stream.read(&mut chunk).await {
            Ok(n) if n > 0 => buffer.extend_from_slice(&chunk[..n]),
            _ => return None,
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    let headers: HashMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < length {
        match stream.read(&mut chunk).await {
            Ok(n) if n > 0 => body.extend_from_slice(&chunk[..n]),
            _ => return None,
        }
    }
    Some(Request {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

async fn respond(stream: &mut TcpStream, status: u16, headers: &[(&str, String)], body: &str) {
    let mut head = format!(
        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body.as_bytes()).await;
    let _ = stream.shutdown().await;
}

async fn respond_json(stream: &mut TcpStream, status: u16, body: Value) {
    respond(
        stream,
        status,
        &[("Content-Type", "application/json".into())],
        &body.to_string(),
    )
    .await
}

const SSE_HEAD: &str =
    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n";

fn sse_event(event: &str, data: &str) -> String {
    format!("event: {event}\r\ndata: {data}\r\n\r\n")
}

impl RemoteShared {
    fn authorized(&self, request: &Request) -> bool {
        if self.config.plain_401 {
            return false;
        }
        if !self.config.auth {
            return true;
        }
        let token = request
            .headers
            .get("authorization")
            .and_then(|value| value.strip_prefix("Bearer "))
            .unwrap_or_default();
        self.valid_tokens
            .lock()
            .unwrap()
            .iter()
            .any(|valid| valid == token)
    }

    async fn reject(&self, stream: &mut TcpStream) {
        let mut headers = Vec::new();
        if self.config.auth {
            headers.push((
                "WWW-Authenticate",
                format!(
                    "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource/mcp\"",
                    self.base
                ),
            ));
        }
        respond(stream, 401, &headers, "unauthorized").await
    }

    fn token(&self, form: &HashMap<String, String>) -> Result<Value, &'static str> {
        let field = |key: &str| form.get(key).map(String::as_str).unwrap_or_default();
        match field("grant_type") {
            "authorization_code" => {
                let expected = self
                    .challenge
                    .lock()
                    .unwrap()
                    .clone()
                    .ok_or("invalid_grant")?;
                let digest = Sha256::digest(field("code_verifier").as_bytes());
                let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
                if field("code") != "good-code"
                    || challenge != expected
                    || field("client_id") != "dyn-client"
                {
                    return Err("invalid_grant");
                }
                self.valid_tokens.lock().unwrap().push("tok-1".into());
                Ok(
                    json!({"access_token": "tok-1", "token_type": "Bearer", "refresh_token": "ref-1", "expires_in": 3600}),
                )
            }
            "refresh_token" if field("refresh_token") == "ref-1" => {
                self.valid_tokens.lock().unwrap().push("tok-2".into());
                Ok(json!({"access_token": "tok-2", "token_type": "Bearer", "expires_in": 3600}))
            }
            _ => Err("invalid_grant"),
        }
    }

    async fn serve(self: Arc<Self>, mut stream: TcpStream) {
        let Some(request) = read_request(&mut stream).await else {
            return;
        };
        let path = request
            .path
            .split('?')
            .next()
            .unwrap_or_default()
            .to_string();
        self.hits
            .lock()
            .unwrap()
            .push(format!("{} {path}", request.method));
        match (request.method.as_str(), path.as_str()) {
            ("GET", "/.well-known/oauth-protected-resource/mcp") if self.config.auth => {
                let body = json!({"resource": format!("{}/mcp", self.base), "authorization_servers": [self.base]});
                respond_json(&mut stream, 200, body).await
            }
            ("GET", "/.well-known/oauth-authorization-server") if self.config.auth => {
                let body = json!({
                    "issuer": self.base,
                    "authorization_endpoint": format!("{}/authorize", self.base),
                    "token_endpoint": format!("{}/token", self.base),
                    "registration_endpoint": format!("{}/register", self.base),
                    "response_types_supported": ["code"],
                    "code_challenge_methods_supported": ["S256"],
                });
                respond_json(&mut stream, 200, body).await
            }
            ("POST", "/register") if self.config.auth => {
                if self.config.registration_unsupported {
                    return respond(&mut stream, 404, &[], "dynamic registration not supported")
                        .await;
                }
                let metadata: Value = serde_json::from_str(&request.body).unwrap_or(Value::Null);
                self.registrations.lock().unwrap().push(metadata);
                let body = json!({"client_id": "dyn-client", "client_id_issued_at": 1700000000, "redirect_uris": []});
                respond_json(&mut stream, 201, body).await
            }
            ("POST", "/token") if self.config.auth => {
                let form: HashMap<String, String> =
                    Url::parse(&format!("http://x/?{}", request.body))
                        .unwrap()
                        .query_pairs()
                        .map(|(k, v)| (k.into_owned(), v.into_owned()))
                        .collect();
                self.token_requests.lock().unwrap().push(form.clone());
                match self.token(&form) {
                    Ok(tokens) => respond_json(&mut stream, 200, tokens).await,
                    Err(code) => respond_json(&mut stream, 400, json!({"error": code})).await,
                }
            }
            ("POST", "/mcp") => self.streamable(&mut stream, request).await,
            ("GET", "/mcp") if self.config.legacy_sse => {
                self.legacy_stream(&mut stream, request).await
            }
            ("POST", "/messages") if self.config.legacy_sse => {
                self.legacy_post(&mut stream, request).await
            }
            ("GET", "/mcp") => respond(&mut stream, 405, &[], "no stream").await,
            _ => respond(&mut stream, 404, &[], "not found").await,
        }
    }

    async fn streamable(&self, stream: &mut TcpStream, request: Request) {
        if self.config.legacy_sse {
            return respond(stream, 405, &[], "Method Not Allowed").await;
        }
        if !self.authorized(&request) {
            return self.reject(stream).await;
        }
        if let Some((name, value)) = &self.config.require_header
            && request.headers.get(name) != Some(value)
        {
            return respond(stream, 403, &[], "missing header").await;
        }
        let message: Value = serde_json::from_str(&request.body).unwrap_or(Value::Null);
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let Some(id) = message.get("id").cloned() else {
            return respond(stream, 202, &[], "").await;
        };
        let mut headers = Vec::new();
        if method == "initialize" {
            headers.push(("Mcp-Session-Id", "sess-1".to_string()));
        } else if request.headers.get("mcp-session-id").map(String::as_str) != Some("sess-1")
            || !request.headers.contains_key("mcp-protocol-version")
        {
            return respond(stream, 400, &[], "missing session or protocol version").await;
        }
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let outcome = handle(&mut self.state.lock().unwrap(), &method, &params);
        if !self.config.sse_responses {
            for step in &outcome.steps {
                if let Step::Sleep(ms) = step {
                    tokio::time::sleep(Duration::from_millis(*ms)).await;
                }
            }
            headers.push(("Content-Type", "application/json".to_string()));
            return respond(
                stream,
                200,
                &headers,
                &response(&id, outcome.reply).to_string(),
            )
            .await;
        }
        let mut head = SSE_HEAD.to_string();
        for (name, value) in &headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let _ = stream.write_all(head.as_bytes()).await;
        for step in outcome.steps {
            match step {
                Step::Sleep(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
                Step::Notify(notification) => {
                    let _ = stream
                        .write_all(sse_event("message", &notification.to_string()).as_bytes())
                        .await;
                    let _ = stream.flush().await;
                }
                Step::AskRoots | Step::Exit => {}
            }
        }
        let event = sse_event("message", &response(&id, outcome.reply).to_string());
        let _ = stream.write_all(event.as_bytes()).await;
        let _ = stream.shutdown().await;
    }

    async fn legacy_stream(&self, stream: &mut TcpStream, request: Request) {
        if !self.authorized(&request) {
            return self.reject(stream).await;
        }
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        *self.legacy.lock().unwrap() = Some(tx);
        let head = format!(
            "{SSE_HEAD}\r\n{}",
            sse_event("endpoint", "/messages?session=1")
        );
        if stream.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        while let Some(data) = rx.recv().await {
            if stream
                .write_all(sse_event("message", &data).as_bytes())
                .await
                .is_err()
            {
                return;
            }
            let _ = stream.flush().await;
        }
    }

    async fn legacy_post(&self, stream: &mut TcpStream, request: Request) {
        let message: Value = serde_json::from_str(&request.body).unwrap_or(Value::Null);
        let sender = self.legacy.lock().unwrap().clone();
        if let (Some(id), Some(sender)) = (message.get("id"), sender) {
            let method = message
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            let outcome = handle(&mut self.state.lock().unwrap(), method, &params);
            for step in outcome.steps {
                if let Step::Notify(notification) = step {
                    let _ = sender.send(notification.to_string());
                }
            }
            let _ = sender.send(response(id, outcome.reply).to_string());
        }
        respond(stream, 202, &[], "Accepted").await
    }
}

impl Remote {
    async fn start(config: RemoteConfig) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let shared = Arc::new(RemoteShared {
            config,
            base,
            hits: Mutex::new(Vec::new()),
            state: Mutex::new(ServerState::default()),
            valid_tokens: Mutex::new(Vec::new()),
            challenge: Mutex::new(None),
            legacy: Mutex::new(None),
            token_requests: Mutex::new(Vec::new()),
            registrations: Mutex::new(Vec::new()),
        });
        let cancel = CancellationToken::new();
        let (accepting, stop) = (shared.clone(), cancel.clone());
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    _ = stop.cancelled() => return,
                    accepted = listener.accept() => accepted,
                };
                if let Ok((stream, _)) = accepted {
                    tokio::spawn(accepting.clone().serve(stream));
                }
            }
        });
        Self { shared, cancel }
    }

    fn url(&self) -> String {
        format!("{}/mcp", self.shared.base)
    }

    fn hits(&self) -> Vec<String> {
        self.shared.hits.lock().unwrap().clone()
    }
}

async fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

// ---------------------------------------------------------------- stdio

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_tools_round_trip() {
    let sandbox = Sandbox::new();
    let mut server = local(&[]);
    if let McpServer::Local {
        cwd, environment, ..
    } = &mut server
    {
        *cwd = Some("sub".into());
        environment.insert("MCP_TEST_VALUE".into(), "from-config".into());
    }
    let config = servers(vec![("my server", server)]);
    let manager = McpManager::connect(config, sandbox.options()).await;
    assert_eq!(manager.status()["my server"], Status::Connected);

    let tools = manager.tools();
    let ids: Vec<&str> = tools.iter().map(|tool| tool.id()).collect();
    assert_eq!(
        ids,
        [
            "my_server_echo",
            "my_server_fail",
            "my_server_image",
            "my_server_slow",
            "my_server_slow_progress",
            "my_server_structured",
            "my_server_grow",
            "my_server_env",
            "my_server_roots",
            "my_server_die",
            "my_server_weird_name",
        ]
    );
    let echo = find(&tools, "my_server_echo");
    assert_eq!(echo.description(), "Echo text");
    assert_eq!(
        echo.parameters(),
        json!({"type": "object", "properties": {"text": {"type": "string"}}, "additionalProperties": false})
    );
    let weird = find(&tools, "my_server_weird_name");
    assert_eq!(weird.description(), "");
    assert_eq!(
        weird.parameters(),
        json!({"type": "object", "required": ["x"], "properties": {}, "additionalProperties": false})
    );

    let permissions = Arc::new(Recorder::default());
    let ctx = context(&sandbox, permissions.clone());
    let out = echo.execute(json!({"text": "hi"}), &ctx).await.unwrap();
    assert_eq!(out.output, "echo: hi");
    assert_eq!(out.title, "");
    assert!(out.attachments.is_empty());
    assert_eq!(
        permissions.asked.lock().unwrap()[0],
        PermissionRequest {
            permission: "my_server_echo".into(),
            patterns: vec!["*".into()],
            always: vec!["*".into()],
            metadata: json!({}),
        }
    );

    let error = find(&tools, "my_server_fail")
        .execute(json!({}), &ctx)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "boom\n\nagain");

    let out = find(&tools, "my_server_image")
        .execute(json!({}), &ctx)
        .await
        .unwrap();
    assert_eq!(out.output, "caption\n\nnote");
    let urls: Vec<&str> = out.attachments.iter().map(|a| a.url.as_str()).collect();
    assert_eq!(
        urls,
        [
            "data:image/png;base64,QUJD",
            "data:application/pdf;base64,QUI="
        ]
    );

    let out = find(&tools, "my_server_structured")
        .execute(Value::Null, &ctx)
        .await
        .unwrap();
    assert_eq!(out.output, r#"{"a":1}"#);

    let out = find(&tools, "my_server_env")
        .execute(json!({"name": "MCP_TEST_VALUE"}), &ctx)
        .await
        .unwrap();
    let (value, cwd) = out.output.split_once('|').unwrap();
    assert_eq!(value, "from-config");
    assert!(cwd.ends_with("sub"), "{cwd}");

    let out = find(&tools, "my_server_roots")
        .execute(json!({}), &ctx)
        .await
        .unwrap();
    assert!(
        out.output
            .starts_with(r#"roots: {"roots":[{"uri":"file:///"#),
        "{}",
        out.output
    );
    assert!(out.output.contains("/work"), "{}", out.output);

    let rejecting = context(
        &sandbox,
        Arc::new(Recorder {
            reject: true,
            ..Recorder::default()
        }),
    );
    let error = echo
        .execute(json!({"text": "x"}), &rejecting)
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::Rejected));

    assert_eq!(
        manager.instructions(),
        vec![crate::ServerInstructions {
            name: "my server".into(),
            instructions: "Use the test tools.".into(),
            tools: ids.iter().map(|id| id.to_string()).collect(),
        }]
    );

    manager.shutdown().await;
    assert!(manager.tools().is_empty());
    assert!(manager.clients().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_resources_prompts_and_list_changed() {
    let sandbox = Sandbox::new();
    let manager = McpManager::connect(servers(vec![("a:b", local(&[]))]), sandbox.options()).await;
    assert_eq!(manager.status()["a:b"], Status::Connected);
    assert_eq!(manager.resource_servers(), ["a:b"]);

    let resources = manager.resources(None).await;
    assert_eq!(
        resources["a%3Ab:file:///readme.md"],
        json!({"name": "readme", "uri": "file:///readme.md", "mimeType": "text/markdown", "client": "a:b"})
    );
    assert!(manager.resources(Some("other")).await.is_empty());
    let templates = manager.resource_templates(Some("a:b")).await;
    assert_eq!(templates["a%3Ab:file:///{path}"]["name"], "file");
    let read = manager
        .read_resource("a:b", "file:///readme.md")
        .await
        .unwrap();
    assert_eq!(read["contents"][0]["text"], "hello resource");
    assert!(
        manager
            .read_resource("missing", "file:///readme.md")
            .await
            .is_none()
    );

    let prompts = manager.prompts().await;
    assert_eq!(prompts["a_b:greet_user"]["client"], "a:b");
    assert_eq!(prompts["a_b:greet_user"]["name"], "greet user");
    let args: IndexMap<String, String> = [("who".to_string(), "$1".to_string())].into();
    let prompt = manager
        .get_prompt("a:b", "greet user", Some(args))
        .await
        .unwrap();
    assert_eq!(prompt["messages"][0]["content"]["text"], "Hello $1");

    let mut events = manager.subscribe();
    let tools = manager.mcp_tools();
    assert!(!tools.contains_key("a_b_added"));
    tools["a_b_grow"].call(json!({}), None).await.unwrap();
    let event = wait_event(&mut events, |e| matches!(e, McpEvent::ToolsChanged { .. })).await;
    assert_eq!(
        event,
        McpEvent::ToolsChanged {
            server: "a:b".into()
        }
    );
    assert!(manager.mcp_tools().contains_key("a_b_added"));
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_failure_statuses() {
    let sandbox = Sandbox::new();
    let mut hang = local(&["--hang"]);
    if let McpServer::Local { timeout, .. } = &mut hang {
        *timeout = Some(400);
    }
    let mut disabled = local(&[]);
    if let McpServer::Local { enabled, .. } = &mut disabled {
        *enabled = Some(false);
    }
    let missing = McpServer::Local {
        command: vec!["opencode-mcp-definitely-missing-binary".into()],
        cwd: None,
        environment: IndexMap::new(),
        enabled: None,
        timeout: None,
    };
    let mut config = servers(vec![
        ("hang", hang),
        ("exit", local(&["--exit"])),
        ("missing", missing),
        ("disabled", disabled),
    ]);
    config.insert("toggle".into(), McpConfig::Toggle { enabled: true });
    let manager = McpManager::connect(config, sandbox.options()).await;
    let status = manager.status();
    assert_eq!(
        status.keys().collect::<Vec<_>>(),
        ["hang", "exit", "missing", "disabled"]
    );
    assert_eq!(failed(&status["hang"]), "Operation timed out after 400ms");
    assert!(
        ["MCP error -32000: Connection closed", "Not connected"].contains(&failed(&status["exit"])),
        "{:?}",
        status["exit"]
    );
    assert!(failed(&status["missing"]).contains("opencode-mcp-definitely-missing-binary"));
    assert_eq!(status["disabled"], Status::Disabled);
    assert!(manager.tools().is_empty());

    assert_eq!(
        serde_json::to_value(&status).unwrap()["hang"],
        json!({"status": "failed", "error": "Operation timed out after 400ms"})
    );
    assert_eq!(
        serde_json::to_value(Status::NeedsAuth).unwrap(),
        json!({"status": "needs_auth"})
    );
    assert_eq!(
        serde_json::to_value(Status::NeedsClientRegistration { error: "e".into() }).unwrap(),
        json!({"status": "needs_client_registration", "error": "e"})
    );
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_timeout_progress_and_abort() {
    let sandbox = Sandbox::new();
    let mut options = sandbox.options();
    options.default_timeout = Some(600);
    let manager = McpManager::connect(servers(vec![("s", local(&[]))]), options).await;
    assert_eq!(manager.status()["s"], Status::Connected);
    let tools = manager.tools();
    let ctx = context(&sandbox, Arc::new(Recorder::default()));

    let error = find(&tools, "s_slow")
        .execute(json!({"ms": 1500}), &ctx)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "MCP error -32001: Request timed out");

    // 750 ms of work, kept alive by progress notifications every 250 ms.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let out = find(&tools, "s_slow_progress")
        .execute(json!({}), &ctx)
        .await
        .unwrap();
    assert_eq!(out.output, "progressed");

    let aborting = context(&sandbox, Arc::new(Recorder::default()));
    let abort = aborting.abort.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        abort.cancel();
    });
    let error = find(&tools, "s_slow")
        .execute(json!({"ms": 300}), &aborting)
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::Aborted));
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_operations_and_reconnect() {
    let sandbox = Sandbox::new();
    let mut disabled = local(&[]);
    if let McpServer::Local { enabled, .. } = &mut disabled {
        *enabled = Some(false);
    }
    let manager = McpManager::connect(servers(vec![("cfg", disabled)]), sandbox.options()).await;
    assert_eq!(manager.status()["cfg"], Status::Disabled);

    let not_found = ManagerError::NotFound {
        name: "nope".into(),
    };
    assert_eq!(manager.connect_server("nope").await.unwrap_err(), not_found);
    assert_eq!(manager.disconnect("nope").await.unwrap_err(), not_found);
    assert_eq!(manager.remove("nope").await.unwrap_err(), not_found);

    assert_eq!(
        manager.connect_server("cfg").await.unwrap(),
        Status::Connected
    );
    let statuses = manager.add("extra", local(&[])).await;
    assert_eq!(statuses["extra"], Status::Connected);
    assert_eq!(
        manager.status().keys().collect::<Vec<_>>(),
        ["cfg", "extra"]
    );
    assert!(manager.mcp_tools().contains_key("extra_echo"));
    assert!(manager.mcp_tools().contains_key("cfg_echo"));

    // The server dies: status flips to failed and a tools-changed event is published.
    let mut events = manager.subscribe();
    let die = manager.mcp_tools()["extra_die"].clone();
    let error = die.call(json!({}), None).await.unwrap_err();
    assert_eq!(error.to_string(), "MCP error -32000: Connection closed");
    wait_event(&mut events, |e| {
        *e == McpEvent::ToolsChanged {
            server: "extra".into(),
        }
    })
    .await;
    assert_eq!(failed(&manager.status()["extra"]), "Connection closed");
    assert!(!manager.mcp_tools().contains_key("extra_echo"));

    assert_eq!(
        manager.connect_server("extra").await.unwrap(),
        Status::Connected
    );
    assert!(manager.mcp_tools().contains_key("extra_echo"));

    manager.disconnect("cfg").await.unwrap();
    assert_eq!(manager.status()["cfg"], Status::Disabled);
    assert!(!manager.mcp_tools().contains_key("cfg_echo"));

    manager.remove("extra").await.unwrap();
    assert_eq!(manager.status().keys().collect::<Vec<_>>(), ["cfg"]);
    manager.remove("cfg").await.unwrap();
    assert_eq!(manager.status()["cfg"], Status::Disabled);
    assert!(manager.clients().is_empty());
    assert!(!manager.supports_oauth("cfg").unwrap());
    manager.shutdown().await;
}

// ---------------------------------------------------------------- remote

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streamable_http_round_trip() {
    let sandbox = Sandbox::new();
    let server = Remote::start(RemoteConfig {
        require_header: Some(("x-api-key".into(), "secret".into())),
        ..RemoteConfig::default()
    })
    .await;
    let mut config = remote(&server.url());
    if let McpServer::Remote { headers, oauth, .. } = &mut config {
        headers.insert("X-Api-Key".into(), "secret".into());
        *oauth = Some(McpOAuthSetting::Toggle(false));
    }
    let manager = McpManager::connect(servers(vec![("http", config)]), sandbox.options()).await;
    assert_eq!(manager.status()["http"], Status::Connected);
    assert!(!manager.supports_oauth("http").unwrap());

    let ctx = context(&sandbox, Arc::new(Recorder::default()));
    let tools = manager.tools();
    assert_eq!(tools.len(), 11);
    let out = find(&tools, "http_echo")
        .execute(json!({"text": "remote"}), &ctx)
        .await
        .unwrap();
    assert_eq!(out.output, "echo: remote");
    let error = find(&tools, "http_fail")
        .execute(json!({}), &ctx)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "boom\n\nagain");
    assert_eq!(manager.resources(None).await.len(), 1);

    let hits = server.hits();
    assert_eq!(hits[0], "POST /mcp");
    assert!(hits.contains(&"GET /mcp".to_string()), "{hits:?}");
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streamable_http_event_stream_responses() {
    let sandbox = Sandbox::new();
    let server = Remote::start(RemoteConfig {
        sse_responses: true,
        ..RemoteConfig::default()
    })
    .await;
    let manager = McpManager::connect(
        servers(vec![("ev", remote(&server.url()))]),
        sandbox.options(),
    )
    .await;
    assert_eq!(manager.status()["ev"], Status::Connected);

    let mut events = manager.subscribe();
    let tools = manager.mcp_tools();
    let result = tools["ev_grow"].call(json!({}), None).await.unwrap();
    assert_eq!(result["content"][0]["text"], "grown");
    wait_event(&mut events, |e| {
        *e == McpEvent::ToolsChanged {
            server: "ev".into(),
        }
    })
    .await;
    assert!(manager.mcp_tools().contains_key("ev_added"));
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn falls_back_to_legacy_sse() {
    let sandbox = Sandbox::new();
    let server = Remote::start(RemoteConfig {
        legacy_sse: true,
        ..RemoteConfig::default()
    })
    .await;
    let manager = McpManager::connect(
        servers(vec![("old", remote(&server.url()))]),
        sandbox.options(),
    )
    .await;
    assert_eq!(manager.status()["old"], Status::Connected);
    let hits = server.hits();
    assert_eq!(hits[0], "POST /mcp");
    assert!(hits.contains(&"POST /messages".to_string()), "{hits:?}");

    let ctx = context(&sandbox, Arc::new(Recorder::default()));
    let out = find(&manager.tools(), "old_echo")
        .execute(json!({"text": "sse"}), &ctx)
        .await
        .unwrap();
    assert_eq!(out.output, "echo: sse");

    let mut events = manager.subscribe();
    manager.mcp_tools()["old_grow"]
        .call(json!({}), None)
        .await
        .unwrap();
    wait_event(&mut events, |e| {
        *e == McpEvent::ToolsChanged {
            server: "old".into(),
        }
    })
    .await;
    assert!(manager.mcp_tools().contains_key("old_added"));
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_failure_statuses() {
    let sandbox = Sandbox::new();
    let denied = Remote::start(RemoteConfig {
        plain_401: true,
        ..RemoteConfig::default()
    })
    .await;
    let mut no_oauth = remote(&denied.url());
    if let McpServer::Remote { oauth, .. } = &mut no_oauth {
        *oauth = Some(McpOAuthSetting::Toggle(false));
    }
    let unsupported = Remote::start(RemoteConfig {
        auth: true,
        registration_unsupported: true,
        ..RemoteConfig::default()
    })
    .await;
    let dead = format!("http://127.0.0.1:{}/mcp", free_port().await);
    let config = servers(vec![
        ("bad url", remote("not a url")),
        ("dead", remote(&dead)),
        ("denied", no_oauth),
        ("unsupported", remote(&unsupported.url())),
    ]);
    let manager = McpManager::connect(config, sandbox.options()).await;
    let status = manager.status();
    assert_eq!(
        failed(&status["bad url"]),
        "Invalid MCP URL for \"bad url\""
    );
    assert!(
        failed(&status["dead"]).starts_with("SSE error: "),
        "{:?}",
        status["dead"]
    );
    assert_eq!(
        failed(&status["denied"]),
        "SSE error: Non-200 status code (405)"
    );
    assert_eq!(
        status["unsupported"],
        Status::NeedsClientRegistration {
            error: "Server does not support dynamic client registration. Please provide clientId in config.".into()
        }
    );
    assert_eq!(
        manager.auth_status("denied").await,
        AuthStatus::NotAuthenticated
    );
    assert!(
        manager
            .start_auth("denied")
            .await
            .unwrap_err()
            .to_string()
            .contains("OAuth explicitly disabled")
    );
    manager.shutdown().await;
}

fn oauth_remote(url: &str, callback_port: Option<u16>) -> McpServer {
    McpServer::Remote {
        url: url.to_string(),
        enabled: None,
        headers: IndexMap::new(),
        oauth: callback_port.map(|port| {
            McpOAuthSetting::Config(McpOAuth {
                callback_port: Some(port),
                scope: Some("mcp:read".into()),
                ..McpOAuth::default()
            })
        }),
        timeout: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unauthorized_server_needs_auth() {
    let sandbox = Sandbox::new();
    let server = Remote::start(RemoteConfig {
        auth: true,
        ..RemoteConfig::default()
    })
    .await;
    let options = sandbox.options();
    let store = AuthStore::new(&options.data_dir);
    // Credentials issued for another URL must be ignored.
    let foreign = Entry {
        tokens: Some(Tokens {
            access_token: "tok-1".into(),
            ..Tokens::default()
        }),
        ..Entry::default()
    };
    store
        .set("locked", foreign, Some("http://elsewhere.invalid/mcp"))
        .await;
    server
        .shared
        .valid_tokens
        .lock()
        .unwrap()
        .push("tok-1".into());

    let manager = McpManager::connect(
        servers(vec![("locked", oauth_remote(&server.url(), None))]),
        options,
    )
    .await;
    assert_eq!(manager.status()["locked"], Status::NeedsAuth);
    assert!(manager.supports_oauth("locked").unwrap());
    assert!(manager.tools().is_empty());

    // Dynamic registration happened during the plain connect and was persisted for this URL.
    let registration = server.shared.registrations.lock().unwrap()[0].clone();
    assert_eq!(
        registration,
        json!({
            "redirect_uris": ["http://127.0.0.1:19876/mcp/oauth/callback"],
            "client_name": "OpenCode",
            "client_uri": "https://opencode.ai",
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        })
    );
    let file = sandbox.auth_file();
    assert_eq!(
        file["locked"]["clientInfo"],
        json!({"clientId": "dyn-client", "clientIdIssuedAt": 1700000000})
    );
    assert_eq!(file["locked"]["serverUrl"], server.url());
    assert_eq!(file["locked"]["codeVerifier"].as_str().unwrap().len(), 43);
    assert_eq!(file["locked"]["oauthState"].as_str().unwrap().len(), 64);
    // Streamable HTTP answered 401, so the SSE fallback must not have been attempted.
    assert!(!server.hits().contains(&"GET /mcp".to_string()));
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stored_tokens_are_sent_and_refreshed() {
    let sandbox = Sandbox::new();
    let server = Remote::start(RemoteConfig {
        auth: true,
        ..RemoteConfig::default()
    })
    .await;
    let options = sandbox.options();
    let store = AuthStore::new(&options.data_dir);
    let entry = Entry {
        tokens: Some(Tokens {
            access_token: "stale".into(),
            refresh_token: Some("ref-1".into()),
            expires_at: number(1.0),
            scope: None,
        }),
        client_info: Some(ClientInfo {
            client_id: "dyn-client".into(),
            ..ClientInfo::default()
        }),
        ..Entry::default()
    };
    store.set("api", entry, Some(&server.url())).await;

    let mut config = oauth_remote(&server.url(), None);
    if let McpServer::Remote { enabled, .. } = &mut config {
        *enabled = Some(false);
    }
    let manager = McpManager::connect(servers(vec![("api", config)]), options).await;
    assert_eq!(manager.status()["api"], Status::Disabled);
    assert_eq!(manager.auth_status("api").await, AuthStatus::Expired);
    assert!(manager.has_stored_tokens("api").await);

    assert_eq!(
        manager.connect_server("api").await.unwrap(),
        Status::Connected
    );
    let refresh = server.shared.token_requests.lock().unwrap()[0].clone();
    assert_eq!(refresh["grant_type"], "refresh_token");
    assert_eq!(refresh["refresh_token"], "ref-1");
    assert_eq!(refresh["client_id"], "dyn-client");
    assert_eq!(refresh["resource"], server.url());
    let file = sandbox.auth_file();
    assert_eq!(file["api"]["tokens"]["accessToken"], "tok-2");
    assert_eq!(file["api"]["tokens"]["refreshToken"], "ref-1");
    assert!(file["api"]["tokens"]["expiresAt"].as_f64().unwrap() > 1_700_000_000.0);
    assert_eq!(manager.auth_status("api").await, AuthStatus::Authenticated);

    let ctx = context(&sandbox, Arc::new(Recorder::default()));
    let out = find(&manager.tools(), "api_echo")
        .execute(json!({"text": "authed"}), &ctx)
        .await
        .unwrap();
    assert_eq!(out.output, "echo: authed");

    assert_eq!(
        manager
            .finish_auth("api", "x")
            .await
            .unwrap_err()
            .to_string(),
        "No pending OAuth flow for MCP server: api"
    );
    manager.remove_auth("api").await;
    assert!(!manager.has_stored_tokens("api").await);
    assert_eq!(sandbox.auth_file(), json!({}));
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_oauth_flow() {
    let sandbox = Sandbox::new();
    let server = Remote::start(RemoteConfig {
        auth: true,
        ..RemoteConfig::default()
    })
    .await;
    let port = free_port().await;
    let redirect = format!("http://127.0.0.1:{port}/mcp/oauth/callback");

    let shared = server.shared.clone();
    let opened: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = opened.clone();
    let browser: BrowserOpener = Arc::new(move |url: String| {
        let (shared, seen) = (shared.clone(), seen.clone());
        async move {
            seen.lock().unwrap().push(url.clone());
            let url = Url::parse(&url).map_err(|e| e.to_string())?;
            let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
            *shared.challenge.lock().unwrap() = query.get("code_challenge").cloned();
            let callback = format!(
                "{}?code=good-code&state={}",
                query["redirect_uri"], query["state"]
            );
            tokio::spawn(async move {
                let page = reqwest::get(callback).await.unwrap();
                assert_eq!(page.status(), 200);
            });
            Ok(())
        }
        .boxed()
    });
    let mut options = sandbox.options();
    options.browser = Some(browser);

    let config = servers(vec![("locked", oauth_remote(&server.url(), Some(port)))]);
    let manager = McpManager::connect(config, options).await;
    assert_eq!(manager.status()["locked"], Status::NeedsAuth);

    let announced: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = announced.clone();
    let on_authorization = move |url: &str| sink.lock().unwrap().push(url.to_string());
    let status = manager
        .authenticate("locked", Some(&on_authorization))
        .await
        .unwrap();
    assert_eq!(status, Status::Connected);
    assert_eq!(manager.status()["locked"], Status::Connected);
    assert!(!crate::callback::is_running());

    let authorization = Url::parse(&opened.lock().unwrap()[0]).unwrap();
    assert_eq!(announced.lock().unwrap()[0], authorization.as_str());
    assert!(
        authorization
            .as_str()
            .starts_with(&format!("{}/authorize?", server.shared.base))
    );
    let query: HashMap<String, String> = authorization.query_pairs().into_owned().collect();
    assert_eq!(query["response_type"], "code");
    assert_eq!(query["client_id"], "dyn-client");
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(query["redirect_uri"], redirect);
    assert_eq!(query["scope"], "mcp:read");
    assert_eq!(query["resource"], server.url());
    assert_eq!(query["state"].len(), 64);

    let exchange = server
        .shared
        .token_requests
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(exchange["grant_type"], "authorization_code");
    assert_eq!(exchange["redirect_uri"], redirect);

    let file = sandbox.auth_file();
    let entry = file["locked"].as_object().unwrap();
    assert_eq!(
        entry.keys().collect::<Vec<_>>(),
        ["tokens", "clientInfo", "serverUrl"]
    );
    assert_eq!(entry["tokens"]["accessToken"], "tok-1");
    assert_eq!(entry["tokens"]["refreshToken"], "ref-1");
    assert_eq!(entry["clientInfo"]["clientId"], "dyn-client");
    assert_eq!(entry["serverUrl"], server.url());
    assert_eq!(
        manager.auth_status("locked").await,
        AuthStatus::Authenticated
    );

    let ctx = context(&sandbox, Arc::new(Recorder::default()));
    let out = find(&manager.tools(), "locked_echo")
        .execute(json!({"text": "oauth"}), &ctx)
        .await
        .unwrap();
    assert_eq!(out.output, "echo: oauth");

    // The pending provider starts from scratch, so a second start yields a fresh redirect.
    let again = manager.start_auth("locked").await.unwrap();
    assert!(
        again
            .authorization_url
            .starts_with(&format!("{}/authorize?", server.shared.base))
    );
    assert!(
        again
            .authorization_url
            .contains(&format!("state={}", again.oauth_state))
    );
    assert_eq!(
        sandbox.auth_file()["locked"]["oauthState"],
        again.oauth_state.as_str()
    );
    let status = manager.finish_auth("locked", "bad-code").await.unwrap();
    assert!(
        failed(&status).starts_with("OAuth completion failed: "),
        "{status:?}"
    );
    manager.remove_auth("locked").await;
    crate::callback::stop();
    manager.shutdown().await;
}

// ---------------------------------------------------------------- auth store

#[tokio::test]
async fn auth_file_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let store = AuthStore::new(dir.path());
    assert_eq!(store.path(), dir.path().join("mcp-auth.json"));
    assert!(store.all().await.is_empty());

    let tokens = Tokens {
        access_token: "a".into(),
        refresh_token: Some("r".into()),
        expires_at: number(1700000000.5),
        scope: None,
    };
    store
        .update_tokens("srv", tokens.clone(), Some("http://x/mcp"))
        .await;
    let info = ClientInfo {
        client_id: "c".into(),
        client_id_issued_at: number(10.0),
        ..ClientInfo::default()
    };
    store.update_client_info("srv", info, None).await;
    let expected = r#"{
  "srv": {
    "tokens": {
      "accessToken": "a",
      "refreshToken": "r",
      "expiresAt": 1700000000.5
    },
    "clientInfo": {
      "clientId": "c",
      "clientIdIssuedAt": 10
    },
    "serverUrl": "http://x/mcp"
  }
}"#;
    assert_eq!(std::fs::read_to_string(store.path()).unwrap(), expected);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    assert_eq!(
        store
            .get_for_url("srv", "http://x/mcp")
            .await
            .unwrap()
            .tokens,
        Some(tokens)
    );
    assert!(store.get_for_url("srv", "http://y/mcp").await.is_none());
    assert!(store.get_for_url("none", "http://x/mcp").await.is_none());

    store.update_code_verifier("srv", "verifier").await;
    store.update_oauth_state("srv", "state").await;
    assert_eq!(store.get_oauth_state("srv").await.as_deref(), Some("state"));
    store.clear_oauth_state("srv").await;
    store.clear_code_verifier("srv").await;
    assert_eq!(std::fs::read_to_string(store.path()).unwrap(), expected);

    // Clearing a field of an unknown entry must not create it.
    store.clear_oauth_state("ghost").await;
    assert!(store.get("ghost").await.is_none());

    // A file written by the original, including unknown keys, is read back.
    let foreign = json!({"ts": {"tokens": {"accessToken": "t", "expiresAt": 1700000123.456}, "serverUrl": "u", "extra": 1}});
    std::fs::write(store.path(), foreign.to_string()).unwrap();
    let entry = store.get("ts").await.unwrap();
    assert_eq!(
        entry.tokens.unwrap().expires_at.unwrap().as_f64(),
        Some(1700000123.456)
    );

    store.remove("ts").await;
    assert_eq!(std::fs::read_to_string(store.path()).unwrap(), "{}");
    std::fs::write(store.path(), "not json").unwrap();
    assert!(store.all().await.is_empty());
}

#[tokio::test]
async fn auth_falls_back_to_v2_database_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let store = AuthStore::new(dir.path());
    let url = "https://mcp.example/mcp";
    assert_eq!(crate::auth::v2_integration_id("remote", url), "mcp_5fa5262afdf6fa5f");
    let db_file = dir.path().join("opencode.db");
    {
        // `credential` as created by migration `20260611192811_lush_chimera`.
        let db = rusqlite::Connection::open(&db_file).unwrap();
        db.execute_batch("CREATE TABLE `credential` (`id` text PRIMARY KEY, `integration_id` text, `label` text NOT NULL, `value` text NOT NULL, `connector_id` text, `method_id` text, `active` integer, `time_created` integer NOT NULL, `time_updated` integer NOT NULL);").unwrap();
        let value = json!({"type": "oauth", "methodID": "mcp_5fa5262afdf6fa5f", "access": "acc", "refresh": "ref", "expires": 1_700_000_123_456u64,
            "metadata": {"serverUrl": url, "tokenType": "Bearer", "scope": "read", "client": {"client_id": "cid", "client_secret": "sec", "client_secret_expires_at": 0}}});
        db.execute("INSERT INTO credential (id, integration_id, label, value, time_created, time_updated) VALUES ('cred_a', 'mcp_5fa5262afdf6fa5f', 'remote', ?1, 1, 1)", [value.to_string()]).unwrap();
        db.execute("INSERT INTO credential (id, integration_id, label, value, time_created, time_updated) VALUES ('cred_b', ?1, 'k', '{\"type\":\"key\",\"key\":\"x\"}', 1, 1)", [crate::auth::v2_integration_id("keyed", url)]).unwrap();
    }
    let before = std::fs::read(&db_file).unwrap();
    let entry = store.get_for_url("remote", url).await.unwrap();
    let tokens = entry.tokens.unwrap();
    assert_eq!((tokens.access_token.as_str(), tokens.refresh_token.as_deref(), tokens.scope.as_deref()), ("acc", Some("ref"), Some("read")));
    assert_eq!(tokens.expires_at, number(1_700_000_123.0));
    let client = entry.client_info.unwrap();
    assert_eq!((client.client_id.as_str(), client.client_secret.as_deref()), ("cid", Some("sec")));
    assert_eq!(entry.server_url.as_deref(), Some(url));
    // The id covers name and URL, and only OAuth rows are used.
    assert!(store.get_for_url("remote", "https://other/mcp").await.is_none());
    assert!(store.get_for_url("keyed", url).await.is_none());
    assert!(store.all().await.is_empty() && !store.path().exists());

    // A server with an entry of its own never reads the database.
    store.update_tokens("remote", Tokens { access_token: "mine".into(), ..Tokens::default() }, Some(url)).await;
    assert_eq!(store.get_for_url("remote", url).await.unwrap().tokens.unwrap().access_token, "mine");
    store.update_oauth_state("remote2", "s").await;
    assert!(store.get_for_url("remote2", url).await.is_none());
    assert_eq!(std::fs::read(&db_file).unwrap(), before);
}
