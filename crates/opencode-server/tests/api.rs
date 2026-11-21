// End-to-end over HTTP: in-memory store, tempdir paths, a scripted provider on a local socket.
use futures_util::StreamExt;
use opencode_core::paths::Paths;
use opencode_server::{Server, ServerOptions, listening_line, openapi, serve_on};
use opencode_session::Engine;
use opencode_storage::Store;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

fn sse(chunks: &[Value]) -> String {
    let mut out: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    out.push_str("data: [DONE]\n\n");
    out
}

// Serves the scripted replies in order; title requests are answered on the side so they never consume the script.
async fn provider(responses: Vec<String>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let script = Arc::new(Mutex::new(VecDeque::from(responses)));
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let script = script.clone();
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let body = loop {
                    let mut chunk = [0u8; 8192];
                    let n = socket.read(&mut chunk).await.unwrap();
                    buffer.extend_from_slice(&chunk[..n]);
                    let Some(split) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                    let head = String::from_utf8_lossy(&buffer[..split]).to_lowercase();
                    let length: usize = head.lines().find_map(|l| l.strip_prefix("content-length:")).unwrap().trim().parse().unwrap();
                    if buffer.len() >= split + 4 + length {
                        break String::from_utf8_lossy(&buffer[split + 4..]).into_owned();
                    }
                };
                let response = if body.contains("Generate a title for this conversation") {
                    text_reply("Mock title")
                } else {
                    script.lock().unwrap().pop_front().expect("unscripted provider request")
                };
                let reply = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{response}", response.len());
                socket.write_all(reply.as_bytes()).await.unwrap();
                socket.shutdown().await.ok();
            });
        }
    });
    port
}

fn text_reply(text: &str) -> String {
    sse(&[
        json!({"choices": [{"index": 0, "delta": {"content": text}}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 20, "completion_tokens": 2, "total_tokens": 22}}),
    ])
}

fn tool_reply(name: &str, arguments: &Value) -> String {
    sse(&[
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": name, "arguments": arguments.to_string()}}]}}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}], "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}}),
    ])
}

// An engine on an in-memory store whose model catalog is an empty local file and is never downloaded.
fn offline_engine(paths: Paths) -> Engine {
    std::fs::create_dir_all(&paths.cache).unwrap();
    let file = paths.cache.join("models.json");
    std::fs::write(&file, "{}").unwrap();
    let source = opencode_llm::catalog::Source {
        url: "http://127.0.0.1:9".into(),
        file: file.clone(),
        locks: paths.state.join("locks"),
        override_file: Some(file),
        disabled: true,
    };
    let engine = Engine::with_store(paths, Store::open_in_memory().unwrap());
    engine.set_catalog_source(source);
    engine
}

struct Fixture {
    _root: tempfile::TempDir,
    project: PathBuf,
    engine: Arc<Engine>,
    server: Server,
    http: reqwest::Client,
}

fn encode(value: &str) -> String {
    value.bytes().map(|b| if b.is_ascii_alphanumeric() { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

impl Fixture {
    async fn new(responses: Vec<String>, options: ServerOptions) -> Self {
        Self::scripted(|_| responses, options).await
    }

    async fn scripted(responses: impl FnOnce(&Path) -> Vec<String>, options: ServerOptions) -> Self {
        Self::configured(json!({}), responses, options).await
    }

    // `extra` is merged over the fixture's opencode.json.
    async fn configured(extra: Value, responses: impl FnOnce(&Path) -> Vec<String>, options: ServerOptions) -> Self {
        let root = tempfile::tempdir().unwrap();
        let canonical = root.path().canonicalize().unwrap();
        let base = PathBuf::from(canonical.to_str().unwrap().trim_start_matches(r"\\?\"));
        let project = base.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let paths = Paths {
            home: base.join("home"),
            data: base.join("data"),
            cache: base.join("cache"),
            config: base.join("config"),
            state: base.join("state"),
            tmp: base.join("tmp"),
        };
        std::fs::create_dir_all(&paths.data).unwrap();
        let port = provider(responses(&project)).await;
        let mut config = json!({"model": "mock/m1", "permission": {"edit": "ask"}, "provider": {"mock": {"options": {"baseURL": format!("http://127.0.0.1:{port}/v1"), "apiKey": "k"}, "models": {"m1": {"name": "Mock"}}}}});
        opencode_core::config::merge_deep(&mut config, extra);
        std::fs::write(project.join("opencode.json"), config.to_string()).unwrap();
        let engine = Arc::new(offline_engine(paths));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let options = ServerOptions { directory: Some(project.to_string_lossy().into_owned()), ..options };
        let server = serve_on(listener, options, engine.clone()).await.unwrap();
        Fixture { _root: root, project, engine, server, http: reqwest::Client::new() }
    }

    fn directory(&self) -> String {
        self.project.to_string_lossy().into_owned()
    }

    fn url(&self, path: &str) -> String {
        let separator = if path.contains('?') { '&' } else { '?' };
        format!("{}{path}{separator}directory={}", self.server.url(), encode(&self.directory()))
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        let response = self.http.get(self.url(path)).send().await.unwrap();
        let status = response.status().as_u16();
        (status, serde_json::from_str(&response.text().await.unwrap()).unwrap_or(Value::Null))
    }

    async fn send(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut request = self.http.request(method, self.url(path));
        if let Some(body) = body {
            request = request.header("content-type", "application/json").body(body.to_string());
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        (status, serde_json::from_str(&response.text().await.unwrap()).unwrap_or(Value::Null))
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        self.send(reqwest::Method::POST, path, Some(body)).await
    }

    // Opens an event stream and returns once `server.connected` has arrived.
    async fn events(&self, path: &str) -> Events {
        let response = self.http.get(self.url(path)).send().await.unwrap();
        assert_eq!(response.status(), 200);
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut stream = response.bytes_stream();
            let mut buffer = String::new();
            while let Some(Ok(chunk)) = stream.next().await {
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(end) = buffer.find("\n\n") {
                    let frame: String = buffer.drain(..end + 2).collect();
                    if tx.send(frame).is_err() {
                        return;
                    }
                }
            }
        });
        let mut events = Events { rx, seen: Vec::new() };
        let first = events.next().await;
        let payload = first.get("payload").unwrap_or(&first);
        assert_eq!(payload["type"], "server.connected");
        events
    }
}

struct Events {
    rx: mpsc::UnboundedReceiver<String>,
    seen: Vec<Value>,
}

impl Events {
    async fn raw(&mut self) -> String {
        tokio::time::timeout(Duration::from_secs(20), self.rx.recv()).await.expect("timed out waiting for an event").expect("stream ended")
    }

    async fn next(&mut self) -> Value {
        let frame = self.raw().await;
        // One `data:` line per event and nothing else.
        let json = frame.strip_prefix("data: ").and_then(|rest| rest.strip_suffix("\n\n")).unwrap_or_else(|| panic!("bad frame {frame:?}"));
        assert!(!json.contains('\n'));
        let value: Value = serde_json::from_str(json).unwrap();
        self.seen.push(value.clone());
        value
    }

    async fn until(&mut self, kind: &str) -> Value {
        loop {
            let event = self.next().await;
            if event.get("payload").unwrap_or(&event)["type"] == kind {
                return event;
            }
        }
    }

    fn kinds(&self) -> Vec<String> {
        self.seen.iter().map(|e| e["type"].as_str().unwrap().to_string()).collect()
    }

    fn position(&self, test: impl Fn(&Value) -> bool) -> usize {
        self.seen.iter().position(test).unwrap_or_else(|| panic!("event not found in {:?}", self.kinds()))
    }
}

// Checks `value` against an openapi component schema: types, enums, required keys, no undeclared keys.
fn validate(document: &Value, schema: &Value, value: &Value, at: &str) -> Result<(), String> {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let name = reference.rsplit('/').next().unwrap();
        return validate(document, &document["components"]["schemas"][name], value, &format!("{at}<{name}>"));
    }
    if let Some(options) = schema.get("anyOf").or(schema.get("oneOf")).and_then(Value::as_array) {
        let errors: Vec<String> = options.iter().filter_map(|option| validate(document, option, value, at).err()).collect();
        return if errors.len() < options.len() { Ok(()) } else { Err(format!("{at}: no anyOf arm matched: {errors:?}")) };
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        return Err(format!("{at}: {value} not in {allowed:?}"));
    }
    let ok = match schema.get("type").and_then(Value::as_str) {
        Some("object") => value.is_object(),
        Some("array") => value.is_array(),
        Some("string") => value.is_string(),
        Some("number") => value.is_number(),
        Some("integer") => value.is_i64() || value.is_u64(),
        Some("boolean") => value.is_boolean(),
        Some("null") => value.is_null(),
        _ => true,
    };
    if !ok {
        return Err(format!("{at}: expected {}, got {value}", schema["type"]));
    }
    if let (Some(pattern), Some(text)) = (schema.get("pattern").and_then(Value::as_str), value.as_str())
        && let Some(prefix) = pattern.strip_prefix('^')
        && prefix.chars().all(char::is_alphanumeric)
        && !text.starts_with(prefix)
    {
        return Err(format!("{at}: {text} does not match {pattern}"));
    }
    if let Some(object) = value.as_object() {
        let properties = schema.get("properties").and_then(Value::as_object);
        for key in schema.get("required").and_then(Value::as_array).into_iter().flatten() {
            if !object.contains_key(key.as_str().unwrap()) {
                return Err(format!("{at}: missing required {key}"));
            }
        }
        for (key, item) in object {
            match (properties.and_then(|p| p.get(key)), schema.get("additionalProperties")) {
                (Some(property), _) => validate(document, property, item, &format!("{at}.{key}"))?,
                (None, Some(Value::Bool(false))) => return Err(format!("{at}: undeclared key {key}")),
                (None, Some(extra @ Value::Object(_))) => validate(document, extra, item, &format!("{at}.{key}"))?,
                _ => {}
            }
        }
    }
    if let (Some(items), Some(list)) = (schema.get("items"), value.as_array()) {
        for (index, item) in list.iter().enumerate() {
            validate(document, items, item, &format!("{at}[{index}]"))?;
        }
    }
    Ok(())
}

fn document() -> Value {
    serde_json::from_str(openapi()).unwrap()
}

fn check(document: &Value, component: &str, value: &Value) {
    let schema = json!({"$ref": format!("#/components/schemas/{component}")});
    if let Err(error) = validate(document, &schema, value, "") {
        panic!("{component}: {error}\n{value:#}");
    }
}

// The 200 response schema of an operation.
fn check_response(document: &Value, method: &str, path: &str, value: &Value) {
    let schema = &document["paths"][path][method]["responses"]["200"]["content"]["application/json"]["schema"];
    assert!(schema.is_object(), "no 200 schema for {method} {path}");
    if let Err(error) = validate(document, schema, value, "") {
        panic!("{method} {path}: {error}\n{value:#}");
    }
}

#[tokio::test]
async fn health_doc_and_listening_line() {
    let f = Fixture::new(vec![], ServerOptions::default()).await;
    let (status, body) = f.get("/global/health").await;
    assert_eq!(status, 200);
    assert_eq!(body, json!({"healthy": true, "version": env!("CARGO_PKG_VERSION")}));
    let doc = document();
    check_response(&doc, "get", "/global/health", &body);

    let response = f.http.get(f.url("/doc")).send().await.unwrap();
    assert_eq!(response.headers()["content-type"], "application/json");
    let served: Value = response.json().await.unwrap();
    assert_eq!(served["openapi"], "3.1.0");
    let operations: usize = served["paths"].as_object().unwrap().values().map(|p| p.as_object().unwrap().len()).sum();
    assert_eq!(operations, 188);
    assert!(!openapi().contains("x-codeSamples"));

    assert_eq!(listening_line("127.0.0.1", 4096), "opencode server listening on http://127.0.0.1:4096");
    assert_eq!(f.server.listening_line(), format!("opencode server listening on http://127.0.0.1:{}", f.server.addr.port()));
}

#[tokio::test]
async fn session_crud_and_events() {
    let f = Fixture::new(vec![], ServerOptions::default()).await;
    let doc = document();
    let mut global = f.events("/global/event").await;

    // An empty body is `{}`.
    let response = f.http.post(f.url("/session")).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let first: Value = response.json().await.unwrap();
    check(&doc, "Session", &first);
    assert_eq!(first["directory"], f.directory());
    assert!(first["title"].as_str().unwrap().starts_with("New session - "));
    let created = global.until("session.created").await;
    assert_eq!(created["directory"], f.directory());
    assert_eq!(created["project"], first["projectID"]);
    assert_eq!(created["payload"]["properties"], json!({"sessionID": first["id"], "info": first}));
    check(&doc, "EventSessionCreated", &created["payload"]);

    let (_, child) = f.post("/session", json!({"parentID": first["id"], "title": "kid"})).await;
    check(&doc, "Session", &child);
    assert_eq!(child["parentID"], first["id"]);

    let (status, list) = f.get("/session").await;
    assert_eq!(status, 200);
    check_response(&doc, "get", "/session", &list);
    assert_eq!(list.as_array().unwrap().len(), 2);
    let (_, roots) = f.get("/session?roots=true").await;
    assert_eq!(roots.as_array().unwrap().iter().map(|s| &s["id"]).collect::<Vec<_>>(), [&first["id"]]);
    let (_, found) = f.get("/session?search=KID").await;
    assert_eq!(found[0]["id"], child["id"]);
    let (status, bad) = f.get("/session?roots=yes").await;
    assert_eq!((status, &bad["name"], &bad["data"]["kind"]), (400, &json!("BadRequest"), &json!("Query")));

    let (_, children) = f.get(&format!("/session/{}/children", first["id"].as_str().unwrap())).await;
    assert_eq!(children, json!([child]));
    let (_, fetched) = f.get(&format!("/session/{}", first["id"].as_str().unwrap())).await;
    assert_eq!(fetched, first);

    let path = format!("/session/{}", first["id"].as_str().unwrap());
    let (status, renamed) = f.send(reqwest::Method::PATCH, &path, Some(json!({"title": "renamed", "time": {"archived": 5}}))).await;
    assert_eq!(status, 200);
    assert_eq!((&renamed["title"], &renamed["time"]["archived"]), (&json!("renamed"), &json!(5)));
    check(&doc, "Session", &renamed);
    let updated = global.until("session.updated").await;
    assert_eq!(updated["payload"]["properties"]["info"]["title"], "renamed");
    check(&doc, "EventSessionUpdated", &updated["payload"]);

    let (_, listed) = f.get("/experimental/session").await;
    check_response(&doc, "get", "/experimental/session", &listed);
    assert_eq!(listed.as_array().unwrap().len(), 1, "archived sessions are hidden");
    assert_eq!(listed[0]["project"]["id"], first["projectID"]);

    let (_, statuses) = f.get("/session/status").await;
    assert_eq!(statuses, json!({}));

    let (status, deleted) = f.send(reqwest::Method::DELETE, &path, None).await;
    assert_eq!((status, deleted), (200, json!(true)));
    // Children go first.
    assert_eq!(global.until("session.deleted").await["payload"]["properties"]["sessionID"], child["id"]);
    assert_eq!(global.until("session.deleted").await["payload"]["properties"]["sessionID"], first["id"]);
    let (status, gone) = f.get(&path).await;
    assert_eq!(status, 404);
    assert_eq!(gone, json!({"name": "NotFoundError", "data": {"message": format!("Session not found: {}", first["id"].as_str().unwrap())}}));
    assert_eq!(f.get("/session").await.1, json!([]));
}

#[tokio::test]
async fn prompt_streams_events_in_order() {
    let f = Fixture::new(vec![text_reply("Done.")], ServerOptions::default()).await;
    let doc = document();
    let (_, session) = f.post("/session", json!({})).await;
    let id = session["id"].as_str().unwrap();
    let mut events = f.events("/event").await;
    let mut global = f.events("/global/event").await;

    let body = json!({"parts": [{"type": "text", "text": "say done"}], "model": {"providerID": "mock", "modelID": "m1"}});
    let (status, reply) = f.post(&format!("/session/{id}/message"), body).await;
    assert_eq!(status, 200, "{reply}");
    check_response(&doc, "post", "/session/{sessionID}/message", &reply);
    assert_eq!(reply["info"]["role"], "assistant");
    assert_eq!(reply["info"]["finish"], "stop");
    let texts: Vec<&str> = reply["parts"].as_array().unwrap().iter().filter(|p| p["type"] == "text").map(|p| p["text"].as_str().unwrap()).collect();
    assert_eq!(texts, ["Done."]);

    events.until("session.idle").await;
    for event in &events.seen {
        assert!(event["id"].as_str().unwrap().starts_with("evt_"));
        let component = match event["type"].as_str().unwrap() {
            "session.status" => "EventSessionStatus",
            "session.idle" => "EventSessionIdle",
            "session.updated" => "EventSessionUpdated",
            "message.updated" => "EventMessageUpdated",
            "message.part.updated" => "EventMessagePartUpdated",
            "message.part.delta" => "EventMessagePartDelta",
            "server.connected" => "EventServerConnected",
            "session.diff" => "EventSessionDiff",
            other => panic!("unexpected event {other}"),
        };
        check(&doc, component, event);
    }
    let kind = |name: &'static str| move |e: &Value| e["type"] == name;
    let role = |name: &'static str| move |e: &Value| e["type"] == "message.updated" && e["properties"]["info"]["role"] == name;
    let busy = events.position(|e| e["type"] == "session.status" && e["properties"]["status"]["type"] == "busy");
    let user = events.position(role("user"));
    let user_text = events.position(|e| e["type"] == "message.part.updated" && e["properties"]["part"]["text"] == "say done");
    let assistant = events.position(role("assistant"));
    let started = events.position(|e| e["type"] == "message.part.updated" && e["properties"]["part"]["text"] == "");
    let delta = events.position(kind("message.part.delta"));
    let text = events.position(|e| e["type"] == "message.part.updated" && e["properties"]["part"]["text"] == "Done.");
    let finish = events.position(|e| e["properties"]["part"]["type"] == "step-finish");
    let idle_status = events.position(|e| e["type"] == "session.status" && e["properties"]["status"]["type"] == "idle");
    let idle = events.position(kind("session.idle"));
    let order = [0, user, user_text, busy, assistant, started, delta, text, finish, idle_status, idle];
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{order:?} {:?}", events.kinds());

    // The part is announced empty, the delta targets it, and the same part is then stored complete.
    let part_id = &events.seen[started]["properties"]["part"]["id"];
    assert_eq!(events.seen[delta]["properties"], json!({"sessionID": id, "messageID": reply["info"]["id"], "partID": part_id, "field": "text", "delta": "Done."}));
    assert_eq!(&events.seen[text]["properties"]["part"]["id"], part_id);
    assert!(!events.kinds().contains(&"message.part.removed".to_string()));

    // Replaying the events reproduces what the API lists.
    let (status, messages) = f.get(&format!("/session/{id}/message")).await;
    assert_eq!(status, 200);
    check_response(&doc, "get", "/session/{sessionID}/message", &messages);
    assert_eq!(messages.as_array().unwrap().len(), 2);
    assert_eq!(messages[1], reply);
    let mut replayed: Vec<Value> = Vec::new();
    for event in &events.seen {
        let properties = &event["properties"];
        match event["type"].as_str().unwrap() {
            "message.updated" => {
                let info = &properties["info"];
                match replayed.iter_mut().find(|m| m["info"]["id"] == info["id"]) {
                    Some(existing) => existing["info"] = info.clone(),
                    None => replayed.push(json!({"info": info, "parts": []})),
                }
            }
            "message.part.updated" => {
                let part = &properties["part"];
                let parts = replayed.iter_mut().find(|m| m["info"]["id"] == part["messageID"]).unwrap()["parts"].as_array_mut().unwrap();
                match parts.iter_mut().find(|p| p["id"] == part["id"]) {
                    Some(existing) => *existing = part.clone(),
                    None => parts.push(part.clone()),
                }
            }
            "message.part.removed" => {
                let parts = replayed.iter_mut().find(|m| m["info"]["id"] == properties["messageID"]).unwrap()["parts"].as_array_mut().unwrap();
                parts.retain(|p| p["id"] != properties["partID"]);
            }
            _ => {}
        }
    }
    assert_eq!(Value::Array(replayed), messages);

    let message_id = reply["info"]["id"].as_str().unwrap();
    let (_, single) = f.get(&format!("/session/{id}/message/{message_id}")).await;
    assert_eq!(single, reply);
    let (status, missing) = f.get(&format!("/session/{id}/message/msg_nope")).await;
    assert_eq!((status, missing), (404, json!({"name": "NotFoundError", "data": {"message": "Message not found: msg_nope"}})));

    // Paging: newest first window, cursor headers, then the rest.
    let response = f.http.get(f.url(&format!("/session/{id}/message?limit=1"))).send().await.unwrap();
    let cursor = response.headers()["x-next-cursor"].to_str().unwrap().to_string();
    assert_eq!(response.headers()["access-control-expose-headers"], "Link, X-Next-Cursor");
    let link = response.headers()["link"].to_str().unwrap().to_string();
    assert!(link.starts_with(&format!("<{}/session/{id}/message?", f.server.url())) && link.ends_with(&format!("limit=1&before={cursor}>; rel=\"next\"")), "{link}");
    assert_eq!(response.json::<Value>().await.unwrap(), json!([messages[1]]));
    let (_, older) = f.get(&format!("/session/{id}/message?limit=1&before={cursor}")).await;
    assert_eq!(older, json!([messages[0]]));
    let (status, body) = f.get(&format!("/session/{id}/message?before={cursor}")).await;
    assert_eq!((status, body), (400, json!({"_tag": "BadRequest"})));
    let (status, body) = f.get(&format!("/session/{id}/message?limit=1&before=zzz")).await;
    assert_eq!((status, body), (400, json!({"_tag": "BadRequest"})));

    // The global stream wraps the same payloads with their location.
    let wrapped = global.until("session.idle").await;
    assert_eq!(wrapped, json!({"directory": f.directory(), "project": session["projectID"], "payload": events.seen[idle]}));
    let stored = f.engine.store.get_session(id).unwrap().unwrap();
    assert_eq!(stored.tokens.unwrap().output, 2.0);

    // The generated title lands after the turn and is announced like any other session change.
    f.engine.settle().await;
    let (_, titled) = f.get(&format!("/session/{id}")).await;
    assert_eq!(titled["title"], "Mock title");
    let retitled = loop {
        let event = events.until("session.updated").await;
        if event["properties"]["info"]["title"] == "Mock title" {
            break event;
        }
    };
    check(&doc, "EventSessionUpdated", &retitled);

    assert_eq!(f.get(&format!("/session/{id}/todo")).await, (200, json!([])));
    let (status, reverted) = f.post(&format!("/session/{id}/revert"), json!({"messageID": messages[0]["info"]["id"]})).await;
    assert_eq!(status, 200, "{reverted}");
    check(&doc, "Session", &reverted);
    assert_eq!(reverted["revert"]["messageID"], messages[0]["info"]["id"]);
    let (status, restored) = f.post(&format!("/session/{id}/unrevert"), json!({})).await;
    assert_eq!((status, restored.get("revert")), (200, None));
    assert_eq!(f.post(&format!("/session/{id}/revert"), json!({})).await.0, 400);
}

#[tokio::test]
async fn permission_round_trip() {
    // Two turns: the first is approved over /permission, the second rejected over the session route.
    let script = |project: &Path| {
        let arguments = json!({"filePath": project.join("hello.txt").to_string_lossy(), "content": "hi
"});
        vec![tool_reply("write", &arguments), text_reply("Done."), tool_reply("write", &arguments)]
    };
    let f = Fixture::scripted(script, ServerOptions::default()).await;
    let doc = document();
    let target = f.project.join("hello.txt");
    let (_, session) = f.post("/session", json!({})).await;
    let id = session["id"].as_str().unwrap();
    let mut events = f.events("/event").await;

    let response = f.http.post(f.url(&format!("/session/{id}/prompt_async"))).header("content-type", "application/json").body(json!({"parts": [{"type": "text", "text": "create hello.txt"}]}).to_string()).send().await.unwrap();
    assert_eq!(response.status(), 204);
    assert!(response.text().await.unwrap().is_empty());

    let asked = events.until("permission.asked").await;
    check(&doc, "EventPermissionAsked", &asked);
    let request = &asked["properties"];
    assert_eq!((&request["sessionID"], &request["permission"]), (&json!(id), &json!("edit")));
    let (_, pending) = f.get("/permission").await;
    check_response(&doc, "get", "/permission", &pending);
    assert_eq!(pending, json!([request]));
    assert_eq!(f.get("/session/status").await.1, json!({id: {"type": "busy"}}));
    assert!(!target.exists());

    let request_id = request["id"].as_str().unwrap();
    let (status, body) = f.post(&format!("/permission/{request_id}/reply"), json!({"reply": "maybe"})).await;
    assert_eq!((status, &body["name"], &body["data"]["kind"]), (400, &json!("BadRequest"), &json!("Payload")));
    let (status, body) = f.post(&format!("/permission/{request_id}/reply"), json!({"reply": "once"})).await;
    assert_eq!((status, body), (200, json!(true)));
    let replied = events.until("permission.replied").await;
    check(&doc, "EventPermissionReplied", &replied);
    assert_eq!(replied["properties"], json!({"sessionID": id, "requestID": request_id, "reply": "once"}));
    events.until("session.idle").await;
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "hi\n");
    assert_eq!(f.get("/permission").await.1, json!([]));
    let tool = events.position(|e| e["properties"]["part"]["type"] == "tool" && e["properties"]["part"]["state"]["status"] == "completed");
    check(&doc, "EventMessagePartUpdated", &events.seen[tool]);

    // Answering twice is a tagged 404.
    let (status, body) = f.post(&format!("/permission/{request_id}/reply"), json!({"reply": "once"})).await;
    assert_eq!(status, 404);
    assert_eq!(body, json!({"_tag": "PermissionNotFoundError", "requestID": request_id, "message": format!("Permission request not found: {request_id}")}));

    // Second turn, rejected through the older session-scoped route.
    std::fs::remove_file(&target).unwrap();
    let blocking = tokio::spawn({
        let (http, url) = (f.http.clone(), f.url(&format!("/session/{id}/message")));
        async move { http.post(url).header("content-type", "application/json").body(json!({"parts": [{"type": "text", "text": "again"}]}).to_string()).send().await.unwrap().json::<Value>().await.unwrap() }
    });
    let asked = events.until("permission.asked").await;
    let request_id = asked["properties"]["id"].as_str().unwrap();
    let (status, body) = f.post(&format!("/session/{id}/permissions/{request_id}"), json!({"response": "reject"})).await;
    assert_eq!((status, body), (200, json!(true)));
    assert_eq!(events.until("permission.replied").await["properties"]["reply"], "reject");
    let reply = blocking.await.unwrap();
    // A rejection ends the turn; the reply is the assistant message holding the failed call.
    let tool = reply["parts"].as_array().unwrap().iter().find(|p| p["type"] == "tool").unwrap_or_else(|| panic!("{reply}"));
    assert_eq!(tool["state"]["status"], "error");
    assert!(!target.exists());
    let (_, messages) = f.get(&format!("/session/{id}/message")).await;
    check_response(&doc, "get", "/session/{sessionID}/message", &messages);
    let failed = messages.as_array().unwrap().iter().flat_map(|m| m["parts"].as_array().unwrap()).filter(|p| p["state"]["status"] == "error").count();
    assert_eq!(failed, 1);
}

#[tokio::test]
async fn failed_turn_reports_session_error() {
    let f = Fixture::new(vec![], ServerOptions::default()).await;
    let (_, session) = f.post("/session", json!({})).await;
    let id = session["id"].as_str().unwrap();
    let mut events = f.events("/event").await;
    let body = json!({"parts": [{"type": "text", "text": "hi"}], "model": {"providerID": "nope", "modelID": "m"}});
    let (status, reply) = f.post(&format!("/session/{id}/message"), body).await;
    assert_eq!((status, reply), (400, json!({"_tag": "BadRequest"})));
    let error = events.until("session.error").await;
    check(&document(), "EventSessionError", &error);
    assert_eq!(error["properties"]["error"]["name"], "UnknownError");
    assert_eq!(f.post(&format!("/session/{id}/abort"), json!({})).await, (200, json!(true)));
}

#[tokio::test]
async fn auth_and_cors() {
    let open = Fixture::new(vec![], ServerOptions::default()).await;
    assert_eq!(open.get("/session").await.0, 200);

    let options = ServerOptions { password: Some("s3:cret".into()), cors: vec!["https://example.test".into()], ..Default::default() };
    let f = Fixture::new(vec![], options).await;
    let url = f.url("/global/health");
    let denied = f.http.get(&url).header("origin", "http://localhost:3000").send().await.unwrap();
    assert_eq!(denied.status(), 401);
    assert_eq!(denied.headers()["www-authenticate"], "Basic realm=\"Secure Area\"");
    assert_eq!(denied.headers()["access-control-allow-origin"], "http://localhost:3000");
    assert_eq!(denied.headers()["vary"], "Origin");
    assert!(denied.text().await.unwrap().is_empty());

    assert_eq!(f.http.get(&url).basic_auth("opencode", Some("s3:cret")).send().await.unwrap().status(), 200);
    assert_eq!(f.http.get(&url).basic_auth("opencode", Some("wrong")).send().await.unwrap().status(), 401);
    assert_eq!(f.http.get(&url).basic_auth("other", Some("s3:cret")).send().await.unwrap().status(), 401);
    assert_eq!(f.http.get(&url).header("authorization", "Basic !!!").send().await.unwrap().status(), 401);
    // base64("opencode:s3:cret"); the query token wins over a bad header.
    let token = "b3BlbmNvZGU6czM6Y3JldA==";
    let with_token = f.http.get(format!("{url}&auth_token={token}")).basic_auth("opencode", Some("wrong")).send().await.unwrap();
    assert_eq!(with_token.status(), 200);
    assert_eq!(f.http.get(format!("{url}&auth_token=bm9wZQ==")).basic_auth("opencode", Some("s3:cret")).send().await.unwrap().status(), 401);
    for path in ["/doc", "/event", "/nope"] {
        assert_eq!(f.http.get(f.url(path)).send().await.unwrap().status(), 401, "{path}");
    }

    // Preflights skip auth; only allowed origins are echoed.
    let preflight = |origin: &'static str| f.http.request(reqwest::Method::OPTIONS, f.url("/config")).header("origin", origin).header("access-control-request-method", "PATCH").header("access-control-request-headers", "x-opencode-directory,content-type").send();
    for origin in ["http://127.0.0.1:5173", "https://app.opencode.ai", "https://opencode.ai", "tauri://localhost", "oc://renderer", "https://example.test"] {
        let response = preflight(origin).await.unwrap();
        assert_eq!(response.status(), 204, "{origin}");
        assert_eq!(response.headers()["access-control-allow-origin"], origin);
        assert_eq!(response.headers()["access-control-allow-headers"], "x-opencode-directory,content-type");
        assert_eq!(response.headers()["access-control-max-age"], "86400");
    }
    for origin in ["https://evil.test", "https://opencode.ai.evil.test", "https://xopencode.ai", "http://localhost.evil.test"] {
        let response = preflight(origin).await.unwrap();
        assert_eq!(response.status(), 204, "{origin}");
        assert!(!response.headers().contains_key("access-control-allow-origin"), "{origin}");
    }

    let custom = ServerOptions { password: Some("pw".into()), username: Some("me".into()), ..Default::default() };
    let f = Fixture::new(vec![], custom).await;
    assert_eq!(f.http.get(f.url("/path")).basic_auth("me", Some("pw")).send().await.unwrap().status(), 200);
    assert_eq!(f.http.get(f.url("/path")).basic_auth("opencode", Some("pw")).send().await.unwrap().status(), 401);
    let empty = ServerOptions { password: Some(String::new()), ..Default::default() };
    assert_eq!(Fixture::new(vec![], empty).await.get("/path").await.0, 200);
}

#[tokio::test]
async fn error_shapes() {
    let f = Fixture::new(vec![], ServerOptions::default()).await;
    let (status, body) = f.get("/session/ses_missing").await;
    assert_eq!((status, body), (404, json!({"name": "NotFoundError", "data": {"message": "Session not found: ses_missing"}})));
    let (status, body) = f.get("/session/nope").await;
    assert_eq!(status, 400);
    assert_eq!((&body["name"], &body["data"]["kind"]), (&json!("BadRequest"), &json!("Params")));
    assert!(body["data"]["message"].is_string());
    let (status, body) = f.post("/session", json!("nope")).await;
    assert_eq!((status, body), (400, json!({"_tag": "BadRequest"})));
    let response = f.http.post(f.url("/session")).body("{oops").send().await.unwrap();
    assert_eq!((response.status().as_u16(), response.json::<Value>().await.unwrap()), (400, json!({"_tag": "BadRequest"})));
    let (_, session) = f.post("/session", json!({})).await;
    let id = session["id"].as_str().unwrap();
    // Payload routes need a body.
    let response = f.http.post(f.url(&format!("/session/{id}/message"))).send().await.unwrap();
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.unwrap();
    assert_eq!((&body["name"], &body["data"]["kind"]), (&json!("BadRequest"), &json!("Payload")));
    let (status, body) = f.post("/session/ses_missing/message", json!({"parts": []})).await;
    assert_eq!((status, &body["name"]), (404, &json!("NotFoundError")));
    let (status, body) = f.post("/permission/per_missing/reply", json!({"reply": "once"})).await;
    assert_eq!(status, 404);
    assert_eq!(body, json!({"_tag": "PermissionNotFoundError", "requestID": "per_missing", "message": "Permission request not found: per_missing"}));
    let (status, body) = f.post("/mcp/nothing/connect", json!({})).await;
    assert_eq!((status, body), (404, json!({"_tag": "McpServerNotFoundError", "name": "nothing", "message": "MCP server not found: nothing"})));
    let (status, body) = f.get("/pty/pty_1").await;
    assert_eq!((status, &body["_tag"], &body["ptyID"]), (404, &json!("PtyNotFoundError"), &json!("pty_1")));

    // Sharing was discontinued: sharing fails as the original's does, unsharing clears the local link.
    let (status, body) = f.post(&format!("/session/{id}/share"), json!({})).await;
    assert_eq!((status, body), (500, json!({"_tag": "InternalServerError"})));
    let (status, body) = f.send(reqwest::Method::DELETE, &format!("/session/{id}/share"), None).await;
    assert_eq!((status, &body["id"], body.get("share")), (200, &json!(id), None));
    // A replay needs at least one event.
    let (status, body) = f.post("/sync/replay", json!({"directory": "x", "events": []})).await;
    assert_eq!((status, &body["name"]), (400, &json!("BadRequest")));
    assert_eq!(f.post("/session/ses_missing/shell", json!({"agent": "build", "command": "ls"})).await.0, 404);
    assert_eq!(f.post("/session/ses_missing/shell", json!({})).await.0, 400);
    let (status, body) = f.post("/question/que_missing/reply", json!({"answers": []})).await;
    assert_eq!((status, &body["_tag"], &body["requestID"]), (404, &json!("QuestionNotFoundError"), &json!("que_missing")));
    // No `pid`: the web app keeps classifying this server as v1 (`/global/health` is probed first anyway).
    let (status, body) = f.get("/api/health").await;
    assert_eq!((status, body), (200, json!({"healthy": true})));
    // Unknown paths answer JSON unless a browser navigates there: the v2 SDK treats a `text/html` answer as "not
    // supported by this version".
    let ui = opencode_server::web_ui_available();
    let unknown: &[&str] = if ui { &["/api/nothing"] } else { &["/api/nothing", "/nothing", "/session/ses_x/nothing"] };
    for path in unknown.iter().copied() {
        let probe = f.http.get(f.url(path)).send().await.unwrap();
        assert_eq!(probe.status(), 404, "{path}");
        assert!(probe.headers()["content-type"].to_str().unwrap().contains("application/json"), "{path}");
    }
    let (_, body) = f.get("/api/nothing").await;
    assert_eq!(body, json!({"_tag": "NotFoundError", "message": "Route not found: /api/nothing"}));
    let page = f.http.get(f.url("/nothing")).header("accept", "text/html,application/xhtml+xml").send().await.unwrap();
    assert_eq!((page.status().as_u16(), page.headers()["content-type"].to_str().unwrap()), (200, "text/html; charset=utf-8"));
    let html = page.text().await.unwrap();
    // With the web UI embedded, unknown paths serve the app (client-side routes); otherwise a short API-only page.
    assert!(if ui { html.contains("<title>OpenCode</title>") } else { html.contains("href=\"/doc\"") });
    let api_paths: &[&str] = if ui { &["/api/nothing", "/global/health"] } else { &["/api/nothing", "/session/ses_x/nothing", "/global/health"] };
    for path in api_paths.iter().copied() {
        let api = f.http.get(f.url(path)).header("accept", "text/html").send().await.unwrap();
        assert!(api.headers()["content-type"].to_str().unwrap().contains("application/json"), "{path}");
    }
    // The original's UI route takes every method.
    let posted = f.http.post(f.url("/nothing")).header("accept", "text/html").send().await.unwrap();
    assert_eq!(posted.status(), if ui { 200 } else { 404 });
    let health = f.http.get(f.url("/global/health")).send().await.unwrap();
    assert!(health.status().is_success() && health.headers()["content-type"].to_str().unwrap().contains("application/json"));
    assert_eq!(health.json::<Value>().await.unwrap()["healthy"], true);
    let (status, body) = f.post("/global/upgrade", json!({"target": "1.0.0"})).await;
    assert_eq!((status, body), (400, json!({"success": false, "error": "Unknown installation method"})));

    // A long reason is capped at 1024 UTF-16 units plus a count.
    let long = "x".repeat(3000);
    let (_, body) = f.get(&format!("/session/{long}")).await;
    let message = body["data"]["message"].as_str().unwrap();
    assert!(message.ends_with(" more chars)") && message.contains('\u{2026}') && message.chars().count() < 1100, "{message}");
}

#[tokio::test]
async fn directory_selection() {
    use opencode_server::{decode_component, select_directory, session_id_from_path};
    assert_eq!(select_directory(Some("/s"), Some("/q"), Some("/h"), "/cwd"), "/s");
    assert_eq!(select_directory(None, Some("/q"), Some("/h"), "/cwd"), "/q");
    assert_eq!(select_directory(None, Some(""), Some("/h"), "/cwd"), "/h");
    assert_eq!(select_directory(None, None, None, "/cwd"), "/cwd");
    // The chosen value is decoded once more, falling back to the raw text when malformed.
    assert_eq!(select_directory(None, None, Some("%2Ftmp%2Fa%20b"), "/cwd"), "/tmp/a b");
    assert_eq!(select_directory(None, Some("/tmp/100%"), None, "/cwd"), "/tmp/100%");
    assert_eq!(decode_component("%E4%B8%AD"), "中");
    assert_eq!(decode_component("%FF"), "%FF");
    assert_eq!(session_id_from_path("/session/ses_1/message"), Some("ses_1"));
    assert_eq!(session_id_from_path("/session/ses_1"), Some("ses_1"));
    assert_eq!(session_id_from_path("/session/status"), None);
    assert_eq!(session_id_from_path("/session"), None);
    assert_eq!(session_id_from_path("/experimental/session/ses_2/background"), Some("ses_2"));
    assert_eq!(session_id_from_path("/experimental/session"), None);

    let f = Fixture::new(vec![], ServerOptions::default()).await;
    let other = f.project.parent().unwrap().join("other dir");
    std::fs::create_dir_all(&other).unwrap();
    let other = other.to_string_lossy().into_owned();
    let base = f.server.url();
    // The header value is percent-encoded by SDKs; the query value ends up decoded twice.
    let by_header: Value = f.http.get(format!("{base}/path")).header("x-opencode-directory", encode(&other)).send().await.unwrap().json().await.unwrap();
    assert_eq!(by_header["directory"], other);
    let twice = encode(&encode(&other));
    let by_query: Value = f.http.get(format!("{base}/path?directory={twice}")).header("x-opencode-directory", "/ignored").send().await.unwrap().json().await.unwrap();
    assert_eq!(by_query["directory"], other);
    let fallback: Value = f.http.get(format!("{base}/path")).send().await.unwrap().json().await.unwrap();
    assert_eq!(fallback["directory"], f.directory());
    check_response(&document(), "get", "/path", &fallback);

    // A session route runs in the session's directory whatever the request says.
    let (_, session) = f.post("/session", json!({})).await;
    let id = session["id"].as_str().unwrap();
    let forked: Value = f.http.post(format!("{base}/session/{id}/fork?directory={}", encode(&other))).send().await.unwrap().json().await.unwrap();
    assert_eq!(forked["directory"], f.directory());
    assert!(forked["title"].as_str().unwrap().ends_with(" (fork #1)"));

    // `GET /session` filters by directory only when the query names one.
    let created: Value = f.http.post(format!("{base}/session?directory={}", encode(&other))).send().await.unwrap().json().await.unwrap();
    assert_eq!(created["directory"], other);
    let all: Value = f.http.get(format!("{base}/session")).send().await.unwrap().json().await.unwrap();
    let here = f.get("/session").await.1;
    assert_eq!((all.as_array().unwrap().len(), here.as_array().unwrap().len()), (3, 2));
}

#[tokio::test]
async fn event_stream_framing_and_heartbeat() {
    let options = ServerOptions { heartbeat: Duration::from_millis(150), ..Default::default() };
    let f = Fixture::new(vec![], options).await;
    let response = f.http.get(f.url("/event")).send().await.unwrap();
    let headers = response.headers().clone();
    assert_eq!(headers["content-type"], "text/event-stream");
    assert_eq!(headers["cache-control"], "no-cache, no-transform");
    assert_eq!(headers["x-accel-buffering"], "no");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    let mut stream = response.bytes_stream();
    let mut raw = String::new();
    while raw.matches("\n\n").count() < 2 {
        raw.push_str(&String::from_utf8_lossy(&stream.next().await.unwrap().unwrap()));
    }
    let frames: Vec<&str> = raw.split_inclusive("\n\n").collect();
    let parse = |frame: &str| serde_json::from_str::<Value>(frame.strip_prefix("data: ").unwrap().strip_suffix("\n\n").unwrap()).unwrap();
    let (connected, beat) = (parse(frames[0]), parse(frames[1]));
    assert_eq!(frames[0], format!("data: {{\"id\":\"{}\",\"type\":\"server.connected\",\"properties\":{{}}}}\n\n", connected["id"].as_str().unwrap()));
    assert_eq!((&beat["type"], &beat["properties"]), (&json!("server.heartbeat"), &json!({})));
    assert!(beat["id"].as_str().unwrap().starts_with("evt_"));

    let mut global = f.events("/global/event").await;
    assert_eq!(global.seen[0].as_object().unwrap().keys().collect::<Vec<_>>(), ["payload"]);
    let beat = global.until("server.heartbeat").await;
    assert_eq!(beat.as_object().unwrap().len(), 1);

    // TUI control events and instance disposal, which ends `/event` but not the global stream.
    let mut events = f.events("/event").await;
    assert_eq!(f.post("/tui/show-toast", json!({"message": "hi", "variant": "info"})).await, (200, json!(true)));
    let toast = events.until("tui.toast.show").await;
    check(&document(), "Event.tui.toast.show", &toast);
    assert_eq!(f.post("/tui/execute-command", json!({"command": "session_new"})).await.1, json!(true));
    assert_eq!(events.until("tui.command.execute").await["properties"], json!({"command": "session.new"}));
    assert_eq!(f.post("/instance/dispose", json!({})).await, (200, json!(true)));
    let disposed = events.until("server.instance.disposed").await;
    assert_eq!(disposed["properties"], json!({"directory": f.directory()}));
    assert!(tokio::time::timeout(Duration::from_secs(5), events.rx.recv()).await.unwrap().is_none(), "stream should end");
    assert_eq!(global.until("server.instance.disposed").await["directory"], f.directory());
    global.until("server.heartbeat").await;
}

#[tokio::test]
async fn catalog_routes_match_their_schemas() {
    let f = Fixture::new(vec![], ServerOptions::default()).await;
    let doc = document();
    for path in ["/config/providers", "/provider", "/provider/auth", "/agent", "/command", "/skill", "/path", "/project", "/project/current", "/experimental/tool/ids", "/experimental/capabilities", "/permission", "/question", "/lsp", "/formatter", "/mcp", "/pty", "/vcs", "/vcs/status", "/experimental/worktree", "/experimental/resource", "/session/status", "/config", "/global/config"] {
        let (status, body) = f.get(path).await;
        assert_eq!(status, 200, "{path}: {body}");
        check_response(&doc, "get", path, &body);
    }
    let (_, providers) = f.get("/config/providers").await;
    assert_eq!(providers["default"], json!({"mock": "m1"}));
    let mock = &providers["providers"][0];
    assert_eq!((&mock["id"], &mock["source"], &mock["models"]["m1"]["name"]), (&json!("mock"), &json!("config"), &json!("Mock")));
    let (_, listed) = f.get("/provider").await;
    assert_eq!(listed["connected"], json!(["mock"]));

    let (_, agents) = f.get("/agent").await;
    let names: Vec<&str> = agents.as_array().unwrap().iter().map(|a| a["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"build") && names.contains(&"plan"), "{names:?}");
    let (_, commands) = f.get("/command").await;
    assert!(commands.as_array().unwrap().iter().any(|c| c["name"] == "init"));
    let (_, tools) = f.get("/experimental/tool?provider=mock&model=m1").await;
    check_response(&doc, "get", "/experimental/tool", &tools);
    assert!(tools.as_array().unwrap().iter().any(|t| t["id"] == "bash"));
    assert_eq!(f.get("/experimental/tool").await.0, 400);

    let (_, project) = f.get("/project/current").await;
    assert_eq!((&project["id"], &project["worktree"]), (&json!("global"), &json!("/")));
    let mut global = f.events("/global/event").await;
    let (status, updated) = f.send(reqwest::Method::PATCH, "/project/global", Some(json!({"name": "mine", "icon": {"color": "red"}}))).await;
    assert_eq!(status, 200);
    check(&doc, "Project", &updated);
    assert_eq!((&updated["name"], &updated["icon"]), (&json!("mine"), &json!({"color": "red"})));
    let event = global.until("project.updated").await;
    assert_eq!((&event["directory"], &event["project"], &event["payload"]["properties"]), (&json!("global"), &json!("global"), &updated));
    let (status, body) = f.send(reqwest::Method::PATCH, "/project/nope", Some(json!({}))).await;
    assert_eq!((status, body), (404, json!({"_tag": "ProjectNotFoundError", "projectID": "nope", "message": "Project not found: nope"})));

    assert_eq!(f.post("/log", json!({"service": "t", "level": "info", "message": "m"})).await, (200, json!(true)));
    assert_eq!(f.post("/log", json!({"service": "t", "level": "loud", "message": "m"})).await.0, 400);
    assert_eq!(f.send(reqwest::Method::PUT, "/auth/mock", Some(json!({"type": "api", "key": "abc"}))).await, (200, json!(true)));
    assert!(std::fs::read_to_string(f.engine.paths.auth_file()).unwrap().contains("abc"));
    assert_eq!(f.send(reqwest::Method::DELETE, "/auth/mock", None).await, (200, json!(true)));
    assert!(!std::fs::read_to_string(f.engine.paths.auth_file()).unwrap().contains("abc"));
}

#[tokio::test]
async fn every_legacy_operation_is_routed() {
    let f = Fixture::new(vec![], ServerOptions::default()).await;
    let doc = document();
    let mut count = 0;
    for (path, operations) in doc["paths"].as_object().unwrap() {
        // The v2 operations have their own test.
        if v2_operation(path) {
            continue;
        }
        let concrete = path
            .replace("{sessionID}", "ses_none")
            .replace("{messageID}", "msg_none")
            .replace("{partID}", "prt_none")
            .replace("{ptyID}", "pty_none")
            .replace("{permissionID}", "per_none")
            .replace("{requestID}", if path.starts_with("/question") { "que_none" } else { "per_none" })
            .replace("{providerID}", "none")
            .replace("{projectID}", "none")
            .replace("{name}", "none")
            .replace("{id}", "wrk_none");
        assert!(!concrete.contains('{'), "{concrete}");
        for method in operations.as_object().unwrap().keys() {
            let method = reqwest::Method::from_bytes(method.to_uppercase().as_bytes()).unwrap();
            count += 1;
            // The control queue long-polls until the TUI is asked something.
            let sent = f.http.request(method.clone(), f.url(&concrete)).timeout(Duration::from_secs(if path == "/tui/control/next" { 1 } else { 30 })).send().await;
            let response = match sent {
                Ok(response) => response,
                Err(error) => {
                    assert!(error.is_timeout() && path == "/tui/control/next", "{method} {path}: {error}");
                    continue;
                }
            };
            let status = response.status().as_u16();
            // WebSocket pre-checks answer with an empty body.
            if status == 404 && path == "/pty/{ptyID}/connect" {
                assert!(response.text().await.unwrap().is_empty());
                continue;
            }
            count -= 1;
            if status == 404 && !response.headers()["content-type"].to_str().unwrap().contains("event-stream") {
                let body: Value = response.json().await.unwrap();
                let message = body["data"]["message"].as_str().unwrap_or_default();
                assert!(!message.starts_with("Route not found"), "{method} {path} is not routed");
            }
            count += 1;
        }
    }
    assert_eq!(count, 127);
}

fn stdio_server() -> String {
    static BUILT: std::sync::Once = std::sync::Once::new();
    let exe = std::env::current_exe().unwrap();
    let debug = exe.parent().unwrap().parent().unwrap();
    let path = debug.join("examples").join(format!("mcp_test_server{}", std::env::consts::EXE_SUFFIX));
    // A workspace test run builds it; a single-crate run has to ask for it.
    BUILT.call_once(|| {
        if !path.exists() {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let status = std::process::Command::new(cargo).args(["build", "-p", "opencode-mcp", "--example", "mcp_test_server"]).status();
            assert!(status.is_ok_and(|s| s.success()), "could not build the MCP test server");
        }
    });
    assert!(path.exists(), "missing test server at {}", path.display());
    path.to_string_lossy().into_owned()
}

fn reasoning_reply(reasoning: &str, text: &str) -> String {
    sse(&[
        json!({"choices": [{"index": 0, "delta": {"reasoning_content": reasoning}}]}),
        json!({"choices": [{"index": 0, "delta": {"content": text}}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 20, "completion_tokens": 2, "total_tokens": 22}}),
    ])
}

impl Fixture {
    async fn session(&self) -> String {
        self.post("/session", json!({})).await.1["id"].as_str().unwrap().to_string()
    }

    async fn prompt_async(&self, id: &str, text: &str) {
        let body = json!({"parts": [{"type": "text", "text": text}]});
        let response = self.http.post(self.url(&format!("/session/{id}/prompt_async"))).header("content-type", "application/json").body(body.to_string()).send().await.unwrap();
        assert_eq!(response.status(), 204);
    }

    async fn messages(&self, id: &str) -> Vec<Value> {
        self.get(&format!("/session/{id}/message")).await.1.as_array().unwrap().clone()
    }
}

fn tool_parts(messages: &[Value]) -> Vec<Value> {
    messages.iter().flat_map(|m| m["parts"].as_array().unwrap().clone()).filter(|p| p["type"] == "tool").collect()
}

#[tokio::test]
async fn question_round_trip() {
    let ask = json!({"questions": [{"question": "Pick?", "header": "Pick", "options": [{"label": "Red", "description": "r"}, {"label": "Blue", "description": "b"}]}]});
    let f = Fixture::new(vec![tool_reply("question", &ask), text_reply("Red it is."), tool_reply("question", &ask)], ServerOptions::default()).await;
    let doc = document();
    let id = f.session().await;
    let mut events = f.events("/event").await;

    f.prompt_async(&id, "ask me").await;
    let asked = events.until("question.asked").await;
    check(&doc, "EventQuestionAsked", &asked);
    let request = asked["properties"].clone();
    let request_id = request["id"].as_str().unwrap();
    assert!(request_id.starts_with("que"));
    assert_eq!((&request["sessionID"], &request["questions"], &request["tool"]["callID"]), (&json!(id), &ask["questions"], &json!("call_1")));
    let (_, pending) = f.get("/question").await;
    check_response(&doc, "get", "/question", &pending);
    assert_eq!(pending, json!([request]));
    let asking = f.messages(&id).await;
    assert_eq!(request["tool"]["messageID"], asking.last().unwrap()["info"]["id"]);

    let (status, body) = f.post(&format!("/question/{request_id}/reply"), json!({"answers": "Red"})).await;
    assert_eq!((status, &body["data"]["kind"]), (400, &json!("Payload")));
    assert_eq!(f.post(&format!("/question/{request_id}/reply"), json!({"answers": [["Red"]]})).await, (200, json!(true)));
    let replied = events.until("question.replied").await;
    check(&doc, "EventQuestionReplied", &replied);
    assert_eq!(replied["properties"], json!({"sessionID": id, "requestID": request_id, "answers": [["Red"]]}));
    events.until("session.idle").await;
    assert_eq!(f.get("/question").await.1, json!([]));
    let tool = &tool_parts(&f.messages(&id).await)[0];
    assert_eq!(tool["state"]["status"], "completed");
    assert_eq!(tool["state"]["output"], "User has answered your questions: \"Pick?\"=\"Red\". You can now continue with the user's answers in mind.");
    assert_eq!(f.post(&format!("/question/{request_id}/reply"), json!({"answers": [["Red"]]})).await.0, 404);

    // Dismissing a question ends the turn with the failed call.
    f.prompt_async(&id, "again").await;
    let asked = events.until("question.asked").await;
    let request_id = asked["properties"]["id"].as_str().unwrap();
    assert_eq!(f.post(&format!("/question/{request_id}/reject"), json!({})).await, (200, json!(true)));
    let rejected = events.until("question.rejected").await;
    check(&doc, "EventQuestionRejected", &rejected);
    assert_eq!(rejected["properties"], json!({"sessionID": id, "requestID": request_id}));
    events.until("session.idle").await;
    let tools = tool_parts(&f.messages(&id).await);
    assert_eq!((&tools[1]["state"]["status"], &tools[1]["state"]["error"]), (&json!("error"), &json!("The user dismissed this question")));
}

#[tokio::test]
async fn command_summarize_and_init() {
    let extra = json!({"command": {"greet": {"template": "Say hello to $1", "description": "Greets"}}});
    let responses = vec![text_reply("Hello Bob."), text_reply("Summary of things."), text_reply("Created AGENTS.md")];
    let f = Fixture::configured(extra, |_| responses, ServerOptions::default()).await;
    let doc = document();
    let id = f.session().await;
    let mut events = f.events("/event").await;

    let (status, reply) = f.post(&format!("/session/{id}/command"), json!({"command": "greet", "arguments": "Bob"})).await;
    assert_eq!(status, 200, "{reply}");
    check_response(&doc, "post", "/session/{sessionID}/command", &reply);
    assert_eq!((&reply["info"]["role"], &reply["parts"][1]["text"]), (&json!("assistant"), &json!("Hello Bob.")));
    let messages = f.messages(&id).await;
    assert_eq!(messages[0]["parts"][0]["text"], "Say hello to Bob");
    let executed = events.until("command.executed").await;
    check(&doc, "EventCommandExecuted", &executed);
    assert_eq!(executed["properties"], json!({"name": "greet", "sessionID": id, "arguments": "Bob", "messageID": reply["info"]["id"]}));
    let (status, body) = f.post(&format!("/session/{id}/command"), json!({"command": "nope", "arguments": ""})).await;
    assert_eq!((status, body), (400, json!({"_tag": "BadRequest"})));
    let error = events.until("session.error").await;
    assert!(error["properties"]["error"]["data"]["message"].as_str().unwrap().starts_with("Command not found: \"nope\"."));

    let (status, body) = f.post(&format!("/session/{id}/summarize"), json!({"providerID": "mock"})).await;
    assert_eq!((status, &body["data"]["kind"]), (400, &json!("Payload")));
    assert_eq!(f.post(&format!("/session/{id}/summarize"), json!({"providerID": "mock", "modelID": "m1"})).await, (200, json!(true)));
    let compacted = events.until("session.compacted").await;
    check(&doc, "EventSessionCompacted", &compacted);
    assert_eq!(compacted["properties"], json!({"sessionID": id}));
    let messages = f.messages(&id).await;
    check_response(&doc, "get", "/session/{sessionID}/message", &json!(messages));
    let summary = messages.last().unwrap();
    assert_eq!((&summary["info"]["summary"], &summary["info"]["agent"]), (&json!(true), &json!("compaction")));
    assert!(summary["parts"].as_array().unwrap().iter().any(|p| p["text"] == "Summary of things."));
    assert!(messages.iter().any(|m| m["parts"].as_array().unwrap().iter().any(|p| p["type"] == "compaction")));

    let message_id = opencode_storage::id::message();
    let init = json!({"providerID": "mock", "modelID": "m1", "messageID": message_id});
    assert_eq!(f.post(&format!("/session/{id}/init"), json!({"providerID": "mock"})).await.0, 400);
    assert_eq!(f.post(&format!("/session/{id}/init"), init).await, (200, json!(true)));
    let messages = f.messages(&id).await;
    let asked = messages.iter().find(|m| m["info"]["id"] == message_id.as_str()).expect("the init message keeps the given id");
    assert!(asked["parts"][0]["text"].as_str().unwrap().contains("AGENTS.md"));
    assert!(messages.last().unwrap()["parts"].as_array().unwrap().iter().any(|p| p["text"] == "Created AGENTS.md"));
}

#[tokio::test]
async fn subagent_events_and_reasoning_deltas() {
    let task = json!({"description": "Look", "prompt": "look around", "subagent_type": "general"});
    let responses = vec![tool_reply("task", &task), reasoning_reply("child thinks", "child says hi"), text_reply("parent done")];
    let f = Fixture::new(responses, ServerOptions::default()).await;
    let doc = document();
    let id = f.session().await;
    let mut events = f.events("/event").await;

    let (status, reply) = f.post(&format!("/session/{id}/message"), json!({"parts": [{"type": "text", "text": "delegate"}]})).await;
    assert_eq!(status, 200, "{reply}");
    let (_, children) = f.get(&format!("/session/{id}/children")).await;
    let child = children[0]["id"].as_str().unwrap();
    loop {
        let event = events.until("session.idle").await;
        if event["properties"]["sessionID"] == id.as_str() {
            break;
        }
    }
    for event in &events.seen {
        let component = match event["type"].as_str().unwrap() {
            "session.created" => "EventSessionCreated",
            "session.updated" => "EventSessionUpdated",
            "session.status" => "EventSessionStatus",
            "session.idle" => "EventSessionIdle",
            "message.updated" => "EventMessageUpdated",
            "message.part.updated" => "EventMessagePartUpdated",
            "message.part.delta" => "EventMessagePartDelta",
            "server.connected" => "EventServerConnected",
            "session.diff" => "EventSessionDiff",
            other => panic!("unexpected event {other}"),
        };
        check(&doc, component, event);
    }

    // The child session is announced, streams under its own id and goes idle before the parent does.
    let of = |session: &str, kind: &'static str| {
        let session = session.to_string();
        move |e: &Value| e["type"] == kind && e["properties"]["sessionID"] == session.as_str()
    };
    let created = events.position(of(child, "session.created"));
    assert_eq!(events.seen[created]["properties"]["info"]["parentID"], id.as_str());
    let reasoning = events.position(|e| e["type"] == "message.part.updated" && e["properties"]["part"]["type"] == "reasoning");
    let reasoning_part = events.seen[reasoning]["properties"]["part"].clone();
    assert_eq!((&reasoning_part["sessionID"], &reasoning_part["text"]), (&json!(child), &json!("")));
    let thought = events.position(|e| e["type"] == "message.part.delta" && e["properties"]["delta"] == "child thinks");
    assert_eq!(
        events.seen[thought]["properties"],
        json!({"sessionID": child, "messageID": reasoning_part["messageID"], "partID": reasoning_part["id"], "field": "text", "delta": "child thinks"})
    );
    let said = events.position(|e| e["type"] == "message.part.delta" && e["properties"]["delta"] == "child says hi");
    assert_eq!(events.seen[said]["properties"]["sessionID"], child);
    let child_idle = events.position(of(child, "session.idle"));
    let parent_said = events.position(|e| e["type"] == "message.part.delta" && e["properties"]["delta"] == "parent done");
    assert_eq!(events.seen[parent_said]["properties"]["sessionID"], id.as_str());
    let parent_idle = events.position(of(&id, "session.idle"));
    let order = [created, reasoning, thought, said, child_idle, parent_said, parent_idle];
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{order:?} {:?}", events.kinds());

    let stored = f.messages(child).await;
    let finished = stored.last().unwrap()["parts"].as_array().unwrap().iter().find(|p| p["type"] == "reasoning").unwrap().clone();
    assert_eq!((&finished["id"], &finished["text"]), (&reasoning_part["id"], &json!("child thinks")));
    let tool = &tool_parts(&f.messages(&id).await)[0];
    assert_eq!((&tool["state"]["status"], &tool["state"]["metadata"]["sessionId"]), (&json!("completed"), &json!(child)));
}

#[tokio::test]
async fn permission_feedback_and_subagent_attribution() {
    let script = |project: &Path| {
        let write = |name: &str| json!({"filePath": project.join(name).to_string_lossy(), "content": "x"});
        let task = json!({"description": "Write", "prompt": "write it", "subagent_type": "general"});
        vec![tool_reply("write", &write("a.txt")), text_reply("Understood."), tool_reply("task", &task), tool_reply("write", &write("child.txt")), text_reply("child done"), text_reply("parent done")]
    };
    let f = Fixture::scripted(script, ServerOptions::default()).await;
    let doc = document();
    let id = f.session().await;
    let mut events = f.events("/event").await;

    f.prompt_async(&id, "write a.txt").await;
    let asked = events.until("permission.asked").await;
    check(&doc, "EventPermissionAsked", &asked);
    let request = &asked["properties"];
    let asking = f.messages(&id).await;
    assert_eq!(request["tool"], json!({"messageID": asking.last().unwrap()["info"]["id"], "callID": "call_1"}));
    let request_id = request["id"].as_str().unwrap();
    let (status, body) = f.post(&format!("/permission/{request_id}/reply"), json!({"reply": "reject", "message": 5})).await;
    assert_eq!((status, &body["data"]["kind"]), (400, &json!("Payload")));
    assert_eq!(f.post(&format!("/permission/{request_id}/reply"), json!({"reply": "reject", "message": "use b.txt"})).await, (200, json!(true)));
    assert_eq!(events.until("permission.replied").await["properties"], json!({"sessionID": id, "requestID": request_id, "reply": "reject"}));
    events.until("session.idle").await;
    let messages = f.messages(&id).await;
    let feedback = "The user rejected permission to use this specific tool call with the following feedback: use b.txt";
    assert_eq!(tool_parts(&messages)[0]["state"]["error"], feedback);
    // Feedback does not stop the loop: the model answered it.
    assert!(messages.last().unwrap()["parts"].as_array().unwrap().iter().any(|p| p["text"] == "Understood."));
    assert!(!f.project.join("a.txt").exists());

    // A subagent's request names the child session and its own tool call.
    f.prompt_async(&id, "delegate").await;
    let asked = events.until("permission.asked").await;
    let request = asked["properties"].clone();
    let (_, children) = f.get(&format!("/session/{id}/children")).await;
    let child = children[0]["id"].as_str().unwrap();
    assert_eq!((&request["sessionID"], &request["permission"]), (&json!(child), &json!("edit")));
    let child_messages = f.messages(child).await;
    assert_eq!(request["tool"]["messageID"], child_messages.last().unwrap()["info"]["id"]);
    assert_eq!(f.get("/permission").await.1, json!([request]));
    assert_eq!(f.post(&format!("/permission/{}/reply", request["id"].as_str().unwrap()), json!({"reply": "once"})).await, (200, json!(true)));
    loop {
        let event = events.until("session.idle").await;
        if event["properties"]["sessionID"] == id.as_str() {
            break;
        }
    }
    assert!(f.project.join("child.txt").exists());
}

#[tokio::test]
async fn mcp_routes_reflect_the_engine_connections() {
    let server = stdio_server();
    let extra = json!({"mcp": {"demo": {"type": "local", "command": [server]}, "off": {"type": "local", "command": [server], "enabled": false}}});
    let responses = vec![tool_reply("demo_grow", &json!({})), text_reply("grown")];
    let f = Fixture::configured(extra, |_| responses, ServerOptions::default()).await;
    let doc = document();

    let (status, listed) = f.get("/mcp").await;
    assert_eq!((status, &listed), (200, &json!({"demo": {"status": "connected"}, "off": {"status": "disabled"}})));
    check_response(&doc, "get", "/mcp", &listed);
    let (status, resources) = f.get("/experimental/resource").await;
    assert_eq!(status, 200);
    check_response(&doc, "get", "/experimental/resource", &resources);
    assert_eq!(resources, json!({"demo:file:///readme.md": {"name": "readme", "uri": "file:///readme.md", "mimeType": "text/markdown", "client": "demo"}}));
    let (_, commands) = f.get("/command").await;
    check_response(&doc, "get", "/command", &commands);
    let greet = commands.as_array().unwrap().iter().find(|c| c["name"] == "demo:greet_user").unwrap();
    assert_eq!(greet, &json!({"name": "demo:greet_user", "description": "Greets", "source": "mcp", "template": "Hello $1", "hints": ["$1"]}));

    assert_eq!(f.post("/mcp/off/connect", json!({})).await, (200, json!(true)));
    assert_eq!(f.get("/mcp").await.1["off"], json!({"status": "connected"}));
    assert_eq!(f.post("/mcp/off/disconnect", json!({})).await, (200, json!(true)));
    assert_eq!(f.get("/mcp").await.1["off"], json!({"status": "disabled"}));
    let (status, added) = f.post("/mcp", json!({"name": "extra", "config": {"type": "local", "command": [server]}})).await;
    assert_eq!((status, &added["extra"]), (200, &json!({"status": "connected"})));
    check_response(&doc, "post", "/mcp", &added);
    let (status, body) = f.post("/mcp", json!({"name": "bad", "config": {"type": "weird"}})).await;
    assert_eq!((status, &body["data"]["kind"]), (400, &json!("Payload")));

    let missing = json!({"_tag": "McpServerNotFoundError", "name": "nothing", "message": "MCP server not found: nothing"});
    assert_eq!(f.post("/mcp/demo/auth", json!({})).await, (400, json!({"error": "MCP server demo does not support OAuth"})));
    assert_eq!(f.post("/mcp/demo/auth/authenticate", json!({})).await.0, 400);
    for path in ["/mcp/nothing/auth", "/mcp/nothing/auth/authenticate", "/mcp/nothing/disconnect"] {
        assert_eq!(f.post(path, json!({})).await, (404, missing.clone()), "{path}");
    }
    assert_eq!(f.post("/mcp/nothing/auth/callback", json!({"code": "c"})).await, (404, missing.clone()));
    assert_eq!(f.send(reqwest::Method::DELETE, "/mcp/nothing/auth", None).await, (404, missing));
    assert_eq!(f.send(reqwest::Method::DELETE, "/mcp/demo/auth", None).await, (200, json!({"success": true})));
    assert_eq!(f.post("/mcp/demo/auth/callback", json!({})).await.0, 400);

    // A prompt uses the same connections; the server's tool-list change reaches the event stream.
    let id = f.session().await;
    let mut events = f.events("/event").await;
    let (status, reply) = f.post(&format!("/session/{id}/message"), json!({"parts": [{"type": "text", "text": "grow"}]})).await;
    assert_eq!(status, 200, "{reply}");
    let tool = &tool_parts(&f.messages(&id).await)[0];
    assert_eq!((&tool["tool"], &tool["state"]["output"]), (&json!("demo_grow"), &json!("grown")));
    let changed = events.until("mcp.tools.changed").await;
    check(&doc, "EventMcpToolsChanged", &changed);
    assert_eq!(changed["properties"], json!({"server": "demo"}));

    // Disposing the instance closes its servers; the next request connects again from the config.
    let before = f.engine.mcp(&f.project).await.unwrap();
    assert_eq!(f.post("/instance/dispose", json!({})).await, (200, json!(true)));
    let mut global = f.events("/global/event").await;
    while !before.clients().is_empty() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(f.get("/mcp").await.1, json!({"demo": {"status": "connected"}, "off": {"status": "disabled"}}));
    assert_eq!(f.post("/global/dispose", json!({})).await, (200, json!(true)));
    global.until("global.disposed").await;
    f.engine.shutdown().await;
}

#[tokio::test]
async fn file_routes_search_list_and_read() {
    let f = Fixture::new(vec![], ServerOptions::default()).await;
    let doc = document();
    std::fs::create_dir_all(f.project.join("src")).unwrap();
    std::fs::create_dir_all(f.project.join("target")).unwrap();
    std::fs::write(f.project.join("src").join("main.rs"), "fn main() { needle(); }\n").unwrap();
    std::fs::write(f.project.join("target").join("out.bin"), [1u8, 0, 2]).unwrap();
    std::fs::write(f.project.join(".gitignore"), "target/\n").unwrap();
    std::fs::write(f.project.join("notes.txt"), "  hello  \n").unwrap();
    let separator = std::path::MAIN_SEPARATOR;

    let (status, entries) = f.get("/file?path=.").await;
    assert_eq!(status, 200, "{entries}");
    check_response(&doc, "get", "/file", &entries);
    let names: Vec<&str> = entries.as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["src", "target", ".gitignore", "notes.txt", "opencode.json"]);
    let src = json!({"name": "src", "path": format!("src{separator}"), "absolute": f.project.join("src").to_string_lossy(), "type": "directory", "ignored": false});
    assert_eq!(entries[0], src);
    assert_eq!((&entries[1]["ignored"], &entries[3]["type"], &entries[3]["ignored"]), (&json!(true), &json!("file"), &json!(false)));
    let (_, nested) = f.get("/file?path=src").await;
    assert_eq!(nested[0]["path"], format!("src{separator}main.rs"));
    assert_eq!(f.get("/file").await.0, 400);
    assert_eq!(f.get("/file?path=..").await.0, 500);
    assert_eq!(f.get("/file?path=notes.txt").await.0, 500);

    let (status, text) = f.get("/file/content?path=notes.txt").await;
    assert_eq!((status, &text), (200, &json!({"type": "text", "content": "hello"})));
    check_response(&doc, "get", "/file/content", &text);
    let (_, binary) = f.get("/file/content?path=target/out.bin").await;
    check_response(&doc, "get", "/file/content", &binary);
    assert_eq!(binary, json!({"type": "binary", "content": "AQAC", "encoding": "base64", "mimeType": "application/octet-stream"}));
    assert_eq!(f.get("/file/content?path=missing.txt").await, (200, json!({"type": "text", "content": ""})));
    assert_eq!(f.get("/file/content?path=..%2Foutside.txt").await.0, 500);
    assert_eq!(f.get("/file/status").await, (200, json!([])));
    assert_eq!(f.get("/find").await.0, 400);
    assert_eq!(f.get("/find/file?query=main&limit=0").await.0, 400);

    if opencode_tools::ripgrep::find_rg().is_none() {
        eprintln!("ripgrep is not installed; skipping the search half of this test");
        return;
    }
    let (status, matches) = f.get("/find?pattern=needle").await;
    assert_eq!(status, 200, "{matches}");
    check_response(&doc, "get", "/find", &matches);
    let expected = json!([{
        "path": {"text": "src/main.rs"},
        "lines": {"text": "fn main() { needle(); }\n"},
        "line_number": 1,
        "absolute_offset": 0,
        "submatches": [{"match": {"text": "needle"}, "start": 12, "end": 18}]
    }]);
    assert_eq!(matches, expected);
    let (status, files) = f.get("/find/file?query=main").await;
    assert_eq!(status, 200);
    check_response(&doc, "get", "/find/file", &files);
    assert_eq!(files[0], "src/main.rs");
    assert_eq!(f.get("/find/file?query=src&type=directory").await.1, json!([format!("src{separator}")]));
    assert_eq!(f.get("/find/file?query=notes&dirs=false").await.1, json!(["notes.txt"]));
    assert_eq!(f.get("/find/file?query=zzzz").await.1, json!([]));
}

#[tokio::test]
async fn config_patches_write_the_files_the_original_writes() {
    let f = Fixture::new(vec![], ServerOptions::default()).await;
    let doc = document();
    let mut global = f.events("/global/event").await;

    let patch = json!({"username": "tester", "agent": {"build": {"temperature": 0.5}}});
    let (status, body) = f.send(reqwest::Method::PATCH, "/config", Some(patch.clone())).await;
    assert_eq!((status, &body), (200, &patch));
    check_response(&doc, "patch", "/config", &body);
    let file = f.project.join("config.json");
    assert_eq!(serde_json::from_str::<Value>(&std::fs::read_to_string(&file).unwrap()).unwrap(), patch);
    let disposed = global.until("server.instance.disposed").await;
    assert_eq!(disposed["payload"]["properties"], json!({"directory": f.directory()}));
    let (status, _) = f.send(reqwest::Method::PATCH, "/config", Some(json!({"agent": {"build": {"steps": 3}}}))).await;
    assert_eq!(status, 200);
    let merged: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(merged, json!({"username": "tester", "agent": {"build": {"temperature": 0.5, "steps": 3}}}));
    let (status, body) = f.send(reqwest::Method::PATCH, "/config", Some(json!(["nope"]))).await;
    assert_eq!((status, &body["data"]["kind"]), (400, &json!("Payload")));
    assert_eq!(f.send(reqwest::Method::PATCH, "/config", Some(json!({"agent": "nope"}))).await.0, 400);

    let (status, body) = f.send(reqwest::Method::PATCH, "/global/config", Some(json!({"username": "me"}))).await;
    assert_eq!((status, &body["username"]), (200, &json!("me")));
    check_response(&doc, "patch", "/global/config", &body);
    let written = f.engine.paths.config.join("opencode.jsonc");
    assert_eq!(std::fs::read_to_string(&written).unwrap(), "{\n  \"username\": \"me\"\n}");
    global.until("global.disposed").await;
    assert_eq!(f.get("/global/config").await.1["username"], "me");
    assert_eq!(f.get("/config").await.1["username"], "me");
}

#[tokio::test]
async fn shell_runs_a_user_command() {
    let shell = if cfg!(windows) { "cmd" } else { "sh" };
    let f = Fixture::configured(json!({"shell": shell}), |_| vec![], ServerOptions::default()).await;
    let doc = document();
    let id = f.session().await;
    let mut events = f.events("/event").await;

    let (status, reply) = f.post(&format!("/session/{id}/shell"), json!({"agent": "build", "command": "echo shell-marker"})).await;
    assert_eq!(status, 200, "{reply}");
    check_response(&doc, "post", "/session/{sessionID}/shell", &reply);
    let part = &reply["parts"][0];
    assert_eq!((&reply["info"]["role"], &part["tool"], &part["state"]["status"], &part["state"]["title"]), (&json!("assistant"), &json!("bash"), &json!("completed"), &json!("")));
    assert!(part["state"]["output"].as_str().unwrap().contains("shell-marker"));
    assert_eq!(part["state"]["metadata"]["output"], part["state"]["output"]);
    events.until("session.idle").await;
    let running = events.position(|e| e["properties"]["part"]["state"]["status"] == "running");
    let completed = events.position(|e| e["properties"]["part"]["state"]["status"] == "completed");
    assert!(running < completed);
    let messages = f.messages(&id).await;
    assert_eq!(messages[0]["parts"][0]["text"], "The following tool was executed by the user");
    assert_eq!(messages[1], reply);
    assert_eq!(f.get("/session/status").await.1, json!({}));

    let (status, body) = f.post(&format!("/session/{id}/shell"), json!({"agent": "nope", "command": "echo x"})).await;
    assert_eq!((status, body), (400, json!({"_tag": "BadRequest"})));
    let error = events.until("session.error").await;
    check(&doc, "EventSessionError", &error);
    assert!(error["properties"]["error"]["data"]["message"].as_str().unwrap().starts_with("Agent not found: \"nope\"."));
}

fn git(dir: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(["-c", "user.name=test", "-c", "user.email=test@example.com", "-c", "commit.gpgsign=false", "-c", "core.autocrlf=false"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

#[tokio::test(flavor = "multi_thread")]
async fn diff_revert_and_workspace_routes() {
    let responses = |project: &Path| {
        git(project, &["init", "-q"]);
        std::fs::write(project.join("keep.txt"), "one\n").unwrap();
        git(project, &["add", "keep.txt"]);
        git(project, &["commit", "-q", "-m", "init"]);
        let arguments = json!({"filePath": project.join("new.txt").to_string_lossy(), "content": "hi\n"});
        vec![tool_reply("write", &arguments), text_reply("Done.")]
    };
    let f = Fixture::configured(json!({"permission": {"edit": "allow"}}), responses, ServerOptions::default()).await;
    let doc = document();
    let id = f.session().await;
    let mut events = f.events("/event").await;
    let new = f.project.join("new.txt");

    let (status, reply) = f.post(&format!("/session/{id}/message"), json!({"parts": [{"type": "text", "text": "write it"}]})).await;
    assert_eq!(status, 200, "{reply}");
    events.until("session.idle").await;
    assert_eq!(std::fs::read_to_string(&new).unwrap(), "hi\n");

    // Tool edits and diff summaries are published like the original's.
    let edited = events.position(|e| e["type"] == "file.edited");
    assert_eq!(events.seen[edited]["properties"], json!({"file": new.to_string_lossy()}));
    check(&doc, "EventFileEdited", &events.seen[edited]);
    let watched = events.position(|e| e["type"] == "file.watcher.updated");
    assert_eq!(events.seen[watched]["properties"], json!({"file": new.to_string_lossy(), "event": "add"}));
    check(&doc, "EventFileWatcherUpdated", &events.seen[watched]);
    let summarized = events.position(|e| e["type"] == "session.diff");
    assert_eq!(events.seen[summarized]["properties"], json!({"sessionID": id, "diff": []}));
    check(&doc, "EventSessionDiff", &events.seen[summarized]);

    // Steps carry snapshots and the changed files; the user message carries the diff once the summary has run.
    let messages = f.messages(&id).await;
    let user = messages[0]["info"]["id"].as_str().unwrap().to_string();
    let parts = messages[1]["parts"].as_array().unwrap();
    let patch = parts.iter().find(|p| p["type"] == "patch").expect("patch part");
    assert_eq!(patch["files"], json!([new.to_string_lossy().replace('\\', "/")]));
    assert_eq!(parts.iter().find(|p| p["type"] == "step-start").unwrap()["snapshot"], patch["hash"]);
    f.engine.settle().await;
    let path = format!("/session/{id}/diff");
    let (status, diff) = f.get(&format!("{path}?messageID={user}")).await;
    assert_eq!(status, 200);
    check_response(&doc, "get", "/session/{sessionID}/diff", &diff);
    assert_eq!(diff.as_array().unwrap().len(), 1, "{diff}");
    assert_eq!((&diff[0]["file"], &diff[0]["additions"], &diff[0]["deletions"], &diff[0]["status"]), (&json!("new.txt"), &json!(1), &json!(0), &json!("added")));
    assert_eq!(f.get(&path).await, (200, json!([])));
    let (status, bad) = f.get(&format!("{path}?messageID=nope")).await;
    assert_eq!((status, &bad["data"]["kind"]), (400, &json!("Params")));

    // Revert puts the files back and reports what it undid; unrevert restores the change.
    let (status, reverted) = f.post(&format!("/session/{id}/revert"), json!({"messageID": user})).await;
    assert_eq!(status, 200, "{reverted}");
    check(&doc, "Session", &reverted);
    assert!(!new.exists());
    assert_eq!(reverted["revert"]["messageID"], user.as_str());
    assert!(reverted["revert"]["snapshot"].as_str().is_some_and(|hash| hash.len() == 40));
    assert!(reverted["revert"]["diff"].as_str().is_some_and(|d| d.contains("new.txt")));
    assert_eq!(reverted["summary"], json!({"additions": 1, "deletions": 0, "files": 1}));
    let announced = loop {
        let event = events.until("session.diff").await;
        if event["properties"]["diff"].as_array().is_some_and(|d| !d.is_empty()) {
            break event;
        }
    };
    check(&doc, "EventSessionDiff", &announced);
    assert_eq!(announced["properties"]["diff"][0]["file"], "new.txt");

    let (status, restored) = f.post(&format!("/session/{id}/unrevert"), json!({})).await;
    assert_eq!(status, 200, "{restored}");
    assert!(restored.get("revert").is_none());
    assert_eq!(std::fs::read_to_string(&new).unwrap(), "hi\n");

    // Nothing is configured, so no language server or formatter is listed; symbol search is empty as in the original.
    for route in ["/lsp", "/formatter", "/find/symbol?query=main"] {
        let (status, body) = f.get(route).await;
        assert_eq!((status, body), (200, json!([])), "{route}");
    }
    let (status, missing) = f.get("/find/symbol").await;
    assert_eq!((status, &missing["data"]["kind"]), (400, &json!("Query")));
}

fn fake_lsp() -> String {
    static BUILT: std::sync::Once = std::sync::Once::new();
    let exe = std::env::current_exe().unwrap();
    let debug = exe.parent().unwrap().parent().unwrap();
    let path = debug.join("examples").join(format!("fake_lsp{}", std::env::consts::EXE_SUFFIX));
    BUILT.call_once(|| {
        if !path.exists() {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let status = std::process::Command::new(cargo).args(["build", "-p", "opencode-lsp", "--example", "fake_lsp"]).status();
            assert!(status.is_ok_and(|s| s.success()), "could not build the fake language server");
        }
    });
    path.to_string_lossy().into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn lsp_and_formatter_status_routes() {
    let extra = json!({
        "permission": {"edit": "allow"},
        "formatter": {"stamp": {"command": ["definitely-not-a-real-formatter", "$FILE"], "extensions": [".fake"]}},
        "lsp": {"fake": {"command": [fake_lsp()], "extensions": [".fake"]}}
    });
    let responses = |project: &Path| {
        let arguments = json!({"filePath": project.join("a.fake").to_string_lossy(), "content": "ok\nERR here\n"});
        vec![tool_reply("write", &arguments), text_reply("Done.")]
    };
    let f = Fixture::configured(extra, responses, ServerOptions::default()).await;
    let doc = document();
    assert_eq!(f.get("/lsp").await, (200, json!([])));
    let (status, formatters) = f.get("/formatter").await;
    assert_eq!(status, 200);
    check_response(&doc, "get", "/formatter", &formatters);
    assert!(formatters.as_array().unwrap().iter().any(|item| item["name"] == "stamp" && item["extensions"] == json!([".fake"])), "{formatters}");

    let id = f.session().await;
    let mut events = f.events("/event").await;
    let (status, reply) = f.post(&format!("/session/{id}/message"), json!({"parts": [{"type": "text", "text": "write it"}]})).await;
    assert_eq!(status, 200, "{reply}");
    events.until("session.idle").await;
    let updated = events.position(|e| e["type"] == "lsp.updated");
    check(&doc, "EventLspUpdated", &events.seen[updated]);
    let tool = &tool_parts(&f.messages(&id).await)[0];
    assert!(tool["state"]["output"].as_str().unwrap().contains("LSP errors detected in this file, please fix:"), "{tool}");

    let (status, servers) = f.get("/lsp").await;
    assert_eq!((status, &servers), (200, &json!([{"id": "fake", "name": "fake", "root": "", "status": "connected"}])));
    check_response(&doc, "get", "/lsp", &servers);

    // Disposing the instance stops its language servers.
    let (status, _) = f.post("/instance/dispose", json!({})).await;
    assert_eq!(status, 200);
    for _ in 0..100 {
        if f.get("/lsp").await.1 == json!([]) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("language server still listed after dispose");
}

const AUTH_PLUGIN: &str = r#"
export const AuthPlugin = async () => ({
  auth: {
    provider: "acme",
    methods: [
      {
        type: "oauth",
        label: "Login",
        prompts: [{ type: "text", key: "org", message: "Org", validate: (value) => (value === "bad" ? "bad org" : undefined) }],
        authorize: async (inputs) => ({
          url: "https://acme.test/auth?org=" + (inputs?.org ?? ""),
          method: "code",
          instructions: "Paste the code",
          callback: async (code) => (code === "good" ? { type: "success", refresh: "r", access: "a", expires: 123 } : { type: "failed" }),
        }),
      },
      { type: "api", label: "Key" },
    ],
  },
})
"#;

#[tokio::test(flavor = "multi_thread")]
async fn plugin_auth_methods_authorize_and_callback() {
    if !std::process::Command::new("bun").arg("--version").output().is_ok_and(|out| out.status.success()) {
        eprintln!("SKIPPED (bun-dependent test): `bun` is not on PATH");
        return;
    }
    let responses = |project: &Path| {
        std::fs::create_dir_all(project.join(".opencode/plugin")).unwrap();
        std::fs::write(project.join(".opencode/plugin/auth.js"), AUTH_PLUGIN).unwrap();
        std::fs::write(project.join("../sdk-stub.mjs"), "export function createOpencodeClient(config) { return { config } }\n").unwrap();
        vec![]
    };
    let f = Fixture::configured(json!({}), responses, ServerOptions::default()).await;
    let stub = f.project.parent().unwrap().join("sdk-stub.mjs");
    f.engine.set_plugin_settings(opencode_session::PluginSettings { sdk_path: Some(stub.to_string_lossy().into_owned()), ..Default::default() });
    let doc = document();

    let (status, methods) = f.get("/provider/auth").await;
    assert_eq!(status, 200, "{methods}");
    check_response(&doc, "get", "/provider/auth", &methods);
    assert_eq!(methods, json!({"acme": [{"type": "oauth", "label": "Login", "prompts": [{"type": "text", "key": "org", "message": "Org"}]}, {"type": "api", "label": "Key"}]}));

    let (authorize, callback) = ("/provider/acme/oauth/authorize", "/provider/acme/oauth/callback");
    assert_eq!(f.post(callback, json!({"method": 0, "code": "good"})).await, (400, json!({"name": "ProviderAuthOauthMissing", "data": {"providerID": "acme"}})));
    assert_eq!(f.post(authorize, json!({"method": 1})).await, (200, Value::Null));
    assert_eq!(f.post(authorize, json!({"nope": true})).await, (400, json!({"name": "BadRequest", "data": {}})));
    let invalid = f.post(authorize, json!({"method": 0, "inputs": {"org": "bad"}})).await;
    assert_eq!(invalid, (400, json!({"name": "ProviderAuthValidationFailed", "data": {"field": "org", "message": "bad org"}})));

    let (status, started) = f.post(authorize, json!({"method": 0, "inputs": {"org": "x"}})).await;
    assert_eq!((status, &started), (200, &json!({"url": "https://acme.test/auth?org=x", "method": "code", "instructions": "Paste the code"})));
    check_response(&doc, "post", "/provider/{providerID}/oauth/authorize", &started);
    assert_eq!(f.post(callback, json!({"method": 0})).await, (400, json!({"name": "ProviderAuthOauthCodeMissing", "data": {"providerID": "acme"}})));
    assert_eq!(f.post(callback, json!({"method": 0, "code": "good"})).await, (200, json!(true)));
    let stored: Value = serde_json::from_str(&std::fs::read_to_string(f.engine.paths.auth_file()).unwrap()).unwrap();
    assert_eq!(stored, json!({"acme": {"type": "oauth", "refresh": "r", "access": "a", "expires": 123}}));

    f.engine.shutdown().await;
}

const PROBE_PLUGIN: &str = r#"
import fs from "node:fs"
import path from "node:path"

export const Probe = async ({ serverUrl, client, directory }) => {
  const headers = client.config.headers ?? {}
  const authed = await fetch(new URL("/global/health", serverUrl), { headers })
  const open = await fetch(new URL("/global/health", serverUrl))
  const report = { url: serverUrl.href, authed: authed.status, open: open.status, body: await authed.json() }
  fs.writeFileSync(path.join(directory, "..", "probe.json"), JSON.stringify(report))
  return {
    event: async ({ event }) => fs.appendFileSync(path.join(directory, "..", "events.log"), event.type + "\n"),
  }
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn plugins_get_a_private_server_when_none_runs() {
    if !std::process::Command::new("bun").arg("--version").output().is_ok_and(|out| out.status.success()) {
        eprintln!("SKIPPED (bun-dependent test): `bun` is not on PATH");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let canonical = root.path().canonicalize().unwrap();
    let base = PathBuf::from(canonical.to_str().unwrap().trim_start_matches(r"\\?\"));
    let project = base.join("project");
    std::fs::create_dir_all(project.join(".opencode/plugin")).unwrap();
    std::fs::write(project.join(".opencode/plugin/probe.js"), PROBE_PLUGIN).unwrap();
    std::fs::write(base.join("sdk-stub.mjs"), "export function createOpencodeClient(config) { return { config } }\n").unwrap();
    let paths = Paths { home: base.join("home"), data: base.join("data"), cache: base.join("cache"), config: base.join("config"), state: base.join("state"), tmp: base.join("tmp") };
    std::fs::create_dir_all(&paths.data).unwrap();
    let engine = Arc::new(offline_engine(paths));
    engine.set_plugin_settings(opencode_session::PluginSettings { sdk_path: Some(base.join("sdk-stub.mjs").to_string_lossy().into_owned()), ..Default::default() });
    opencode_server::attach_plugin_server(&engine);
    assert!(engine.server().is_none());

    let status = engine.load_plugins(&project).await.unwrap().expect("plugin status");
    assert!(status.notices.is_empty() && status.plugins.len() == 1, "{status:?}");
    let server = engine.server().expect("a private server");
    assert!(server.url.starts_with("http://127.0.0.1:") && server.headers.iter().any(|(name, value)| name == "Authorization" && value.starts_with("Basic ")), "{server:?}");
    let probe: Value = serde_json::from_str(&std::fs::read_to_string(base.join("probe.json")).unwrap()).unwrap();
    assert_eq!(probe["url"], format!("{}/", server.url));
    assert_eq!((&probe["authed"], &probe["open"], &probe["body"]["healthy"]), (&json!(200), &json!(401), &json!(true)));

    // Store changes reach the plugin's `event` hook through that server's bus.
    engine.create_session(&project, Default::default()).unwrap();
    let mut seen = String::new();
    for _ in 0..100 {
        seen = std::fs::read_to_string(base.join("events.log")).unwrap_or_default();
        if seen.contains("session.created") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(seen.contains("session.created"), "{seen}");

    engine.shutdown().await;
    assert!(engine.server().is_none());
    let refused = reqwest::Client::new().get(format!("{}/global/health", server.url)).timeout(Duration::from_secs(2)).send().await;
    assert!(refused.is_err());
}

fn v2_operation(path: &str) -> bool {
    path.starts_with("/api/") || path.starts_with("/experimental/project/{projectID}/copy") && !path.ends_with("generate-name")
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git").args(args).current_dir(dir).output().unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn vcs_routes_report_and_apply_changes() {
    let prepare = |project: &Path| {
        git(project, &["init", "-q", "-b", "main"]);
        std::fs::write(project.join("keep.txt"), "one\ntwo\n").unwrap();
        std::fs::write(project.join("gone.txt"), "bye\n").unwrap();
        git(project, &["add", "."]);
        git(project, &["commit", "-q", "-m", "init"]);
        std::fs::write(project.join("keep.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::remove_file(project.join("gone.txt")).unwrap();
        std::fs::write(project.join("new.txt"), "a\nb\n").unwrap();
        vec![]
    };
    let f = Fixture::scripted(prepare, ServerOptions::default()).await;
    let doc = document();

    let (status, body) = f.get("/vcs").await;
    assert_eq!((status, &body), (200, &json!({"branch": "main", "default_branch": "main"})));
    check_response(&doc, "get", "/vcs", &body);

    let (_, listed) = f.get("/vcs/status").await;
    check_response(&doc, "get", "/vcs/status", &listed);
    let row = |file: &str| listed.as_array().unwrap().iter().find(|item| item["file"] == file).cloned().unwrap_or_else(|| panic!("{file} in {listed}"));
    assert_eq!(row("keep.txt"), json!({"file": "keep.txt", "additions": 1, "deletions": 0, "status": "modified"}));
    assert_eq!(row("gone.txt"), json!({"file": "gone.txt", "additions": 0, "deletions": 1, "status": "deleted"}));
    assert_eq!(row("new.txt"), json!({"file": "new.txt", "additions": 2, "deletions": 0, "status": "added"}));

    let (status, diff) = f.get("/vcs/diff?mode=git&context=1").await;
    assert_eq!(status, 200);
    check_response(&doc, "get", "/vcs/diff", &diff);
    let files: Vec<&str> = diff.as_array().unwrap().iter().map(|item| item["file"].as_str().unwrap()).collect();
    assert!(files.windows(2).all(|pair| pair[0] <= pair[1]) && files.contains(&"new.txt"), "{files:?}");
    let keep = diff.as_array().unwrap().iter().find(|item| item["file"] == "keep.txt").unwrap();
    let patch = keep["patch"].as_str().unwrap();
    assert!(patch.contains("\n two\n+three") && !patch.contains("\n one"), "{patch}");
    let new = diff.as_array().unwrap().iter().find(|item| item["file"] == "new.txt").unwrap();
    assert!(new["patch"].as_str().unwrap().contains("+a\n+b"), "{new}");
    // On the default branch there is nothing to compare against.
    assert_eq!(f.get("/vcs/diff?mode=branch").await, (200, json!([])));
    let (status, body) = f.get("/vcs/diff").await;
    assert_eq!((status, &body["name"], &body["data"]["kind"]), (400, &json!("BadRequest"), &json!("Query")));
    assert_eq!(f.get("/vcs/diff?mode=nope").await.0, 400);

    let raw = f.http.get(f.url("/vcs/diff/raw")).send().await.unwrap();
    assert_eq!(raw.headers()["content-type"], "text/x-diff; charset=utf-8");
    let raw = raw.text().await.unwrap();
    assert!(raw.contains("diff --git a/keep.txt b/keep.txt") && raw.contains("+three") && raw.contains("new.txt"), "{raw}");

    let patch = "diff --git a/keep.txt b/keep.txt\n--- a/keep.txt\n+++ b/keep.txt\n@@ -1,3 +1,4 @@\n one\n two\n three\n+four\n";
    let (status, body) = f.post("/vcs/apply", json!({"patch": patch})).await;
    assert_eq!((status, &body), (200, &json!({"applied": true})));
    check_response(&doc, "post", "/vcs/apply", &body);
    assert_eq!(std::fs::read_to_string(f.project.join("keep.txt")).unwrap(), "one\ntwo\nthree\nfour\n");
    let (status, body) = f.post("/vcs/apply", json!({"patch": patch})).await;
    assert_eq!((status, body), (400, json!({"name": "VcsApplyError", "data": {"message": "Patch can't be applied", "reason": "not-clean"}})));
    assert_eq!(f.post("/vcs/apply", json!({})).await.0, 400);
}

#[tokio::test(flavor = "multi_thread")]
async fn git_init_and_worktrees() {
    let f = Fixture::new(vec![text_reply("Fix the login bug please")], ServerOptions::default()).await;
    let doc = document();
    // Outside a repository everything VCS-shaped is empty and worktrees are refused.
    assert_eq!(f.get("/vcs").await, (200, json!({})));
    assert_eq!(f.get("/vcs/diff?mode=git").await, (200, json!([])));
    let (status, body) = f.post("/vcs/apply", json!({"patch": "x"})).await;
    assert_eq!((status, &body["data"]["reason"]), (400, &json!("non-git")));
    let (status, body) = f.post("/experimental/worktree", json!({})).await;
    assert_eq!((status, body), (400, json!({"name": "WorktreeNotGitError", "data": {"message": "Worktrees are only supported for git projects"}})));

    let mut events = f.events("/global/event").await;
    let (status, project) = f.post("/project/git/init", json!({})).await;
    assert_eq!((status, &project["vcs"]), (200, &json!("git")), "{project}");
    check_response(&doc, "post", "/project/git/init", &project);
    assert!(f.project.join(".git").is_dir());
    events.until("server.instance.disposed").await;
    assert_eq!(f.get("/project/current").await.1["id"], project["id"]);
    assert_eq!(f.post("/project/git/init", json!({})).await.1, project);

    std::fs::write(f.project.join("keep.txt"), "one\n").unwrap();
    git(&f.project, &["add", "keep.txt"]);
    git(&f.project, &["commit", "-q", "-m", "init"]);
    let base = git_out(&f.project, &["symbolic-ref", "--short", "HEAD"]);
    let project_id = f.get("/project/current").await.1["id"].as_str().unwrap().to_string();

    let (status, created) = f.post("/experimental/worktree", json!({"name": "My Feature"})).await;
    assert_eq!(status, 200, "{created}");
    check_response(&doc, "post", "/experimental/worktree", &created);
    assert_eq!((&created["name"], &created["branch"]), (&json!("my-feature"), &json!("opencode/my-feature")));
    let directory = created["directory"].as_str().unwrap().to_string();
    assert!(Path::new(&directory).starts_with(f.engine.paths.data.join("worktree")), "{directory}");
    let ready = events.until("worktree.ready").await;
    assert_eq!(ready["payload"]["properties"], json!({"name": "my-feature", "branch": "opencode/my-feature"}));
    assert_eq!(ready["directory"], json!(directory));
    // The checkout runs with the user's git settings, line endings included.
    let kept = || std::fs::read_to_string(Path::new(&directory).join("keep.txt")).unwrap().replace("\r\n", "\n");
    assert_eq!(kept(), "one\n");
    assert_eq!(f.get("/experimental/worktree").await, (200, json!([directory])));
    assert_eq!(f.get("/project/current").await.1["sandboxes"], json!([directory]));
    // The same name again gets a suffix instead of colliding.
    let (_, second) = f.post("/experimental/worktree", json!({"name": "my feature"})).await;
    assert!(second["name"].as_str().unwrap().starts_with("my-feature-"), "{second}");
    events.until("worktree.ready").await;

    std::fs::write(Path::new(&directory).join("scratch.txt"), "x").unwrap();
    std::fs::write(Path::new(&directory).join("keep.txt"), "changed\n").unwrap();
    let (status, body) = f.post("/experimental/worktree/reset", json!({"directory": directory})).await;
    assert_eq!((status, &body), (200, &json!(true)), "{body}");
    assert!(!Path::new(&directory).join("scratch.txt").exists());
    assert_eq!(kept(), "one\n");
    let (status, body) = f.post("/experimental/worktree/reset", json!({"directory": f.directory()})).await;
    assert_eq!((status, &body["name"], &body["data"]["message"]), (400, &json!("WorktreeResetFailedError"), &json!("Cannot reset the primary workspace")));

    for target in [directory.clone(), second["directory"].as_str().unwrap().to_string()] {
        let (status, body) = f.send(reqwest::Method::DELETE, "/experimental/worktree", Some(json!({"directory": target}))).await;
        assert_eq!((status, &body), (200, &json!(true)), "{body}");
        assert!(!Path::new(&target).exists());
    }
    assert_eq!(f.get("/experimental/worktree").await, (200, json!([])));
    assert_eq!(git_out(&f.project, &["branch", "--format=%(refname:short)"]), base);

    // Opening the project remembered its worktree root.
    let (status, listed) = f.get(&format!("/project/{project_id}/directories")).await;
    assert_eq!((status, &listed), (200, &json!([{"directory": f.directory()}])));
    check_response(&doc, "get", "/project/{projectID}/directories", &listed);
    let (status, named) = f.post(&format!("/experimental/project/{project_id}/copy/generate-name"), json!({})).await;
    assert_eq!(status, 200);
    check_response(&doc, "post", "/experimental/project/{projectID}/copy/generate-name", &named);
    assert!(!named["name"].as_str().unwrap().is_empty());
    // With a context the model names the copy: its first three words, slugified.
    let (_, named) = f.post(&format!("/experimental/project/{project_id}/copy/generate-name"), json!({"context": "fix the login form"})).await;
    assert_eq!(named, json!({"name": "fix-the-login"}));
    assert_eq!(f.post(&format!("/experimental/project/{project_id}/copy/generate-name"), json!({"context": 1})).await.0, 400);

    // Project copies are detached worktrees recorded with their strategy.
    let copies = f.project.parent().unwrap().join("copies");
    let copy_path = format!("/experimental/project/{project_id}/copy");
    let create = |name: &str| json!({"strategy": "git_worktree", "directory": copies.to_string_lossy(), "name": name});
    let (status, copy) = f.post(&copy_path, create("c1")).await;
    assert_eq!(status, 200, "{copy}");
    check_response(&doc, "post", "/experimental/project/{projectID}/copy", &copy);
    let first = copies.join("c1").to_string_lossy().into_owned();
    assert_eq!(copy, json!({"directory": first}));
    assert_eq!(std::fs::read_to_string(copies.join("c1").join("keep.txt")).unwrap().replace("\r\n", "\n"), "one\n");
    assert_eq!(f.post(&copy_path, create("c1")).await.1["directory"], json!(copies.join("c1-2").to_string_lossy()));
    let listed = f.get(&format!("/project/{project_id}/directories")).await.1;
    let strategies: Vec<&Value> = listed.as_array().unwrap().iter().map(|d| &d["strategy"]).collect();
    assert_eq!(strategies.iter().filter(|s| **s == "git_worktree").count(), 2, "{listed}");
    let (status, body) = f.post(&copy_path, json!({"strategy": "rsync", "directory": copies.to_string_lossy()})).await;
    assert_eq!((status, body), (400, json!({"name": "ProjectCopyError", "data": {"message": "Project copy strategy unavailable: rsync"}})));
    let (status, body) = f.send(reqwest::Method::DELETE, &copy_path, Some(json!({"directory": f.directory(), "force": false}))).await;
    assert_eq!((status, &body["data"]["message"]), (400, &json!(format!("Invalid project copy directory: {}", f.directory()))));
    // A dirty copy needs `force`.
    std::fs::write(copies.join("c1").join("dirty.txt"), "x").unwrap();
    let (status, body) = f.send(reqwest::Method::DELETE, &copy_path, Some(json!({"directory": first, "force": false}))).await;
    assert_eq!((status, &body["name"], &body["data"]["forceRequired"]), (400, &json!("ProjectCopyError"), &json!(true)), "{body}");
    assert_eq!(f.send(reqwest::Method::DELETE, &copy_path, Some(json!({"directory": first, "force": true}))).await.0, 204);
    assert!(!copies.join("c1").exists());
    // Refresh drops vanished copies and records worktrees made behind its back.
    let outside = copies.join("outside");
    git(&f.project, &["worktree", "add", "--detach", &outside.to_string_lossy(), "HEAD"]);
    assert_eq!(f.post(&format!("{copy_path}/refresh"), json!({})).await.0, 204);
    let listed = f.get(&format!("/project/{project_id}/directories")).await.1;
    let mut found: Vec<(String, Value)> = listed.as_array().unwrap().iter().map(|d| (d["directory"].as_str().unwrap().to_string(), d["strategy"].clone())).collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    let mut expected = vec![(f.directory(), Value::Null), (copies.join("c1-2").to_string_lossy().into_owned(), json!("git_worktree")), (outside.to_string_lossy().into_owned(), json!("git_worktree"))];
    expected.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(found, expected);
}

#[tokio::test]
async fn tui_control_sync_and_console_routes() {
    let f = Fixture::new(vec![], ServerOptions::default()).await;
    let doc = document();
    f.server.tui_request("/tui/open-help", json!({"n": 1}));
    f.server.tui_request("/tui/open-models", json!({}));
    let (status, next) = f.get("/tui/control/next").await;
    assert_eq!((status, &next), (200, &json!({"path": "/tui/open-help", "body": {"n": 1}})));
    check_response(&doc, "get", "/tui/control/next", &next);
    assert_eq!(f.get("/tui/control/next").await.1["path"], "/tui/open-models");
    assert_eq!(f.post("/tui/control/response", json!({"ok": true})).await, (200, json!(true)));
    assert_eq!(tokio::time::timeout(Duration::from_secs(5), f.server.tui_response()).await.unwrap(), json!({"ok": true}));
    // Nothing queued: the poll stays open until something is.
    let poll = tokio::spawn(f.http.get(f.url("/tui/control/next")).send());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!poll.is_finished());
    f.server.tui_request("/tui/clear-prompt", json!(null));
    let answered: Value = poll.await.unwrap().unwrap().json().await.unwrap();
    assert_eq!(answered, json!({"path": "/tui/clear-prompt", "body": null}));

    assert_eq!(f.post("/sync/start", json!({})).await, (200, json!(true)));
    let id = f.session().await;
    // Stealing needs a workspace to steal into, and this server is none.
    assert_eq!(f.post("/sync/steal", json!({"sessionID": id})).await, (400, json!({"_tag": "BadRequest"})));
    assert_eq!(f.post("/sync/steal", json!({})).await.1["name"], "BadRequest");
    assert_eq!(f.post("/experimental/workspace/sync-list", json!({})).await.0, 204);
    // Without `OPENCODE_EXPERIMENTAL_WORKSPACES` nothing is listed; the built-in adapter always is.
    let (_, workspaces) = f.get("/experimental/workspace").await;
    assert_eq!(workspaces, json!([]));
    check_response(&doc, "get", "/experimental/workspace", &workspaces);
    let (_, adapters) = f.get("/experimental/workspace/adapter").await;
    check_response(&doc, "get", "/experimental/workspace/adapter", &adapters);
    assert_eq!(adapters[0]["type"], "worktree");
    assert_eq!(f.get("/experimental/workspace/status").await.1, json!([]));
    let (status, history) = f.post("/sync/history", json!({})).await;
    assert_eq!(status, 200);
    check_response(&doc, "post", "/sync/history", &history);
    assert_eq!(history[0]["type"], "session.created.1");

    let (status, console) = f.get("/experimental/console").await;
    assert_eq!((status, &console), (200, &json!({"consoleManagedProviders": [], "switchableOrgCount": 0})));
    check_response(&doc, "get", "/experimental/console", &console);
    let (_, orgs) = f.get("/experimental/console/orgs").await;
    check_response(&doc, "get", "/experimental/console/orgs", &orgs);
    assert_eq!(f.post("/experimental/console/switch", json!({"accountID": "a", "orgID": "o"})).await, (400, json!({"_tag": "BadRequest"})));
}

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

// What one attachment has shown: the text before the cursor frame, the cursor, and the text since.
#[derive(Default)]
struct Terminal {
    replay: String,
    cursor: Option<u64>,
    live: String,
    closed: Option<(u16, String)>,
}

impl Terminal {
    // The client-side cursor: the server's, advanced by every UTF-16 unit received after it.
    fn position(&self) -> u64 {
        self.cursor.unwrap() + self.live.encode_utf16().count() as u64
    }

    async fn read(&mut self, socket: &mut Socket) -> bool {
        use tokio_tungstenite::tungstenite::Message;
        let message = tokio::time::timeout(Duration::from_secs(30), socket.next())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for a frame: replay {:?} cursor {:?} live {:?}", self.replay, self.cursor, self.live));
        match message {
            Some(Ok(Message::Text(text))) => {
                // ConPTY asks where the cursor is before it lets the shell run; a terminal emulator answers.
                if self.cursor.is_some() && text.as_str().contains("\u{1b}[6n") {
                    use futures_util::SinkExt;
                    socket.send(Message::Text("\u{1b}[1;1R".into())).await.unwrap();
                }
                if self.cursor.is_none() { self.replay.push_str(text.as_str()) } else { self.live.push_str(text.as_str()) }
            }
            Some(Ok(Message::Binary(bytes))) => {
                assert_eq!(bytes[0], 0, "control frames start with a zero byte");
                assert!(self.cursor.is_none(), "one cursor frame per attachment");
                let meta: Value = serde_json::from_slice(&bytes[1..]).unwrap();
                assert_eq!(meta.as_object().unwrap().len(), 1);
                self.cursor = Some(meta["cursor"].as_u64().unwrap());
            }
            Some(Ok(Message::Close(frame))) => {
                self.closed = Some(frame.map_or((1005, String::new()), |frame| (u16::from(frame.code), frame.reason.to_string())));
                return false;
            }
            Some(Ok(_)) => {}
            Some(Err(_)) | None => return false,
        }
        true
    }

    async fn until(&mut self, socket: &mut Socket, done: impl Fn(&Terminal) -> bool) {
        while !done(self) {
            assert!(self.read(socket).await, "socket ended early: replay {:?} live {:?} closed {:?}", self.replay, self.live, self.closed);
        }
    }

    async fn closed(mut self, socket: &mut Socket) -> (u16, String) {
        while self.read(socket).await {}
        self.closed.expect("no close frame")
    }
}

impl Fixture {
    fn socket_url(&self, path: &str) -> String {
        self.url(path).replacen("http://", "ws://", 1)
    }

    async fn attach(&self, path: &str) -> Socket {
        tokio_tungstenite::connect_async(self.socket_url(path)).await.unwrap_or_else(|error| panic!("{path}: {error}")).0
    }

    // The HTTP status a refused upgrade answers with.
    async fn refused(&self, path: &str) -> u16 {
        match tokio_tungstenite::connect_async(self.socket_url(path)).await {
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => response.status().as_u16(),
            other => panic!("expected a refused upgrade, got {:?}", other.map(|_| ())),
        }
    }
}

async fn type_line(socket: &mut Socket, line: &str) {
    use futures_util::SinkExt;
    socket.send(tokio_tungstenite::tungstenite::Message::Text(format!("{line}\r").into())).await.unwrap();
}

fn shell_command() -> Value {
    if cfg!(windows) { json!({"command": "cmd.exe", "title": "Test shell", "env": {"PTY_TEST": "from-env"}}) } else { json!({"command": "/bin/sh", "title": "Test shell", "env": {"PTY_TEST": "from-env"}}) }
}

#[tokio::test(flavor = "multi_thread")]
async fn pty_crud_attach_resume_and_resize() {
    let f = Fixture::new(vec![], ServerOptions::default()).await;
    let doc = document();
    let mut events = f.events("/global/event").await;

    let (status, shells) = f.get("/pty/shells").await;
    assert_eq!(status, 200);
    check_response(&doc, "get", "/pty/shells", &shells);
    assert!(!shells.as_array().unwrap().is_empty());
    assert_eq!(f.post("/pty", json!({"args": "x"})).await.0, 400);

    let (status, info) = f.post("/pty", shell_command()).await;
    assert_eq!(status, 200, "{info}");
    check_response(&doc, "post", "/pty", &info);
    let id = info["id"].as_str().unwrap().to_string();
    assert_eq!((&info["title"], &info["status"], &info["cwd"]), (&json!("Test shell"), &json!("running"), &json!(f.directory())));
    assert!(info["pid"].as_u64().unwrap() > 0);
    assert_eq!(events.until("pty.created").await["payload"]["properties"]["info"], info);
    assert_eq!(f.get("/pty").await.1, json!([info]));
    assert_eq!(f.get(&format!("/pty/{id}")).await.1, info);
    // Terminals belong to their directory.
    let elsewhere = format!("{}/pty/{id}?directory=%2Fnowhere", f.server.url());
    assert_eq!(f.http.get(elsewhere).send().await.unwrap().status(), 404);

    let mut socket = f.attach(&format!("/pty/{id}/connect")).await;
    let mut first = Terminal::default();
    first.until(&mut socket, |t| t.cursor.is_some()).await;
    let variable = if cfg!(windows) { "%PTY_TEST%" } else { "$PTY_TEST" };
    type_line(&mut socket, &format!("echo marker-one-{variable}")).await;
    first.until(&mut socket, |t| t.live.contains("marker-one-from-env")).await;
    let resume = first.position();
    drop(socket);

    // A full replay ends with the cursor frame and covers what the first attachment saw.
    let mut socket = f.attach(&format!("/pty/{id}/connect")).await;
    let mut full = Terminal::default();
    full.until(&mut socket, |t| t.cursor.is_some()).await;
    assert!(full.replay.contains("marker-one-from-env"), "{:?}", full.replay);
    assert!(full.cursor.unwrap() >= resume);
    assert_eq!(full.replay.encode_utf16().count() as u64, full.cursor.unwrap());
    drop(socket);

    // Resuming from the client's cursor replays only what came after it.
    let mut socket = f.attach(&format!("/pty/{id}/connect?cursor={resume}")).await;
    let mut resumed = Terminal::default();
    resumed.until(&mut socket, |t| t.cursor.is_some()).await;
    assert!(!resumed.replay.contains("marker-one"), "{:?}", resumed.replay);
    assert_eq!(resumed.replay.encode_utf16().count() as u64, resumed.cursor.unwrap() - resume);

    let (status, updated) = f.send(reqwest::Method::PUT, &format!("/pty/{id}"), Some(json!({"title": "Renamed", "size": {"rows": 31, "cols": 97}}))).await;
    assert_eq!((status, &updated["title"]), (200, &json!("Renamed")));
    check_response(&doc, "put", "/pty/{ptyID}", &updated);
    assert_eq!(events.until("pty.updated").await["payload"]["properties"]["info"]["title"], "Renamed");
    assert_eq!(f.send(reqwest::Method::PUT, &format!("/pty/{id}"), Some(json!({"size": {"rows": 0, "cols": 1}}))).await.0, 400);
    type_line(&mut socket, if cfg!(windows) { "mode con" } else { "stty size" }).await;
    resumed.until(&mut socket, |t| t.live.contains("97")).await;
    drop(socket);

    // -1 tails: nothing is replayed.
    let mut socket = f.attach(&format!("/pty/{id}/connect?cursor=-1")).await;
    let mut tail = Terminal::default();
    tail.until(&mut socket, |t| t.cursor.is_some()).await;
    assert_eq!(tail.replay, "");
    // Binary input is accepted when it is UTF-8 and dropped otherwise.
    {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::Message;
        socket.send(Message::Binary(vec![0xff, 0xfe].into())).await.unwrap();
        socket.send(Message::Binary(b"echo marker-two\r".to_vec().into())).await.unwrap();
    }
    tail.until(&mut socket, |t| t.live.contains("marker-two")).await;
    type_line(&mut socket, "exit").await;
    assert_eq!(tail.closed(&mut socket).await.0, 1000);
    assert_eq!(events.until("pty.exited").await["payload"]["properties"], json!({"id": id, "exitCode": 0}));

    // Legacy hides an exited terminal; v2 still shows it and reports the exit on the socket.
    assert_eq!(f.get("/pty").await.1, json!([]));
    assert_eq!(f.get(&format!("/pty/{id}")).await.0, 404);
    assert_eq!(f.refused(&format!("/pty/{id}/connect")).await, 404);
    assert_eq!(f.refused("/pty/pty_missing/connect").await, 404);
    let (status, body) = f.get(&format!("/api/pty/{id}")).await;
    assert_eq!((status, &body["data"]["status"], &body["data"]["exitCode"]), (200, &json!("exited"), &json!(0)));
    check_response(&doc, "get", "/api/pty/{ptyID}", &body);
    let mut socket = f.attach(&format!("/api/pty/{id}/connect")).await;
    assert_eq!(Terminal::default().closed(&mut socket).await, (4404, "session exited".to_string()));
    assert_eq!(f.refused("/api/pty/pty_missing/connect").await, 404);

    assert_eq!(f.send(reqwest::Method::DELETE, &format!("/api/pty/{id}"), None).await.0, 204);
    assert_eq!(events.until("pty.deleted").await["payload"]["properties"], json!({"id": id}));
    let (status, body) = f.send(reqwest::Method::DELETE, &format!("/api/pty/{id}"), None).await;
    assert_eq!((status, body), (404, json!({"_tag": "PtyNotFoundError", "ptyID": id, "message": format!("PTY session not found: {id}")})));
}

#[tokio::test(flavor = "multi_thread")]
async fn pty_tickets_auth_and_shutdown() {
    let options = ServerOptions { password: Some("secret".into()), ..ServerOptions::default() };
    let f = Fixture::new(vec![], options).await;
    let basic = "Basic b3BlbmNvZGU6c2VjcmV0";
    let auth_token = "b3BlbmNvZGU6c2VjcmV0";
    let request = |method: reqwest::Method, path: &str| f.http.request(method, f.url(path)).header("authorization", basic);
    let created: Value = request(reqwest::Method::POST, "/pty").header("content-type", "application/json").body(shell_command().to_string()).send().await.unwrap().json().await.unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    let doc = document();

    // Minting needs the header that forces a preflight, and an origin the CORS policy allows.
    let token_path = format!("/pty/{id}/connect-token");
    let plain = request(reqwest::Method::POST, &token_path).send().await.unwrap();
    assert_eq!(plain.status(), 403);
    assert_eq!(plain.json::<Value>().await.unwrap(), json!({"_tag": "PtyForbiddenError", "message": "Invalid PTY connect token request"}));
    let foreign = request(reqwest::Method::POST, &token_path).header("x-opencode-ticket", "1").header("origin", "https://evil.example").send().await.unwrap();
    assert_eq!(foreign.status(), 403);
    let v2 = request(reqwest::Method::POST, &format!("/api/pty/{id}/connect-token")).send().await.unwrap();
    assert_eq!(v2.json::<Value>().await.unwrap(), json!({"_tag": "ForbiddenError", "message": "Invalid PTY connect token request"}));
    let mint = || async {
        let response = request(reqwest::Method::POST, &token_path).header("x-opencode-ticket", "1").header("origin", "http://localhost:3000").send().await.unwrap();
        assert_eq!(response.status(), 200);
        response.json::<Value>().await.unwrap()
    };
    let token = mint().await;
    check_response(&doc, "post", "/pty/{ptyID}/connect-token", &token);
    assert_eq!(token["expires_in"], 60);
    let ticket = token["ticket"].as_str().unwrap().to_string();
    assert_eq!(ticket.len(), 36);

    // Without credentials or a ticket the upgrade is refused; a ticket is good for exactly one connection.
    assert_eq!(f.refused(&format!("/pty/{id}/connect")).await, 401);
    assert_eq!(f.refused(&format!("/pty/{id}/connect?ticket=wrong")).await, 403);
    let mut socket = f.attach(&format!("/pty/{id}/connect?ticket={ticket}")).await;
    let mut terminal = Terminal::default();
    terminal.until(&mut socket, |t| t.cursor.is_some()).await;
    assert_eq!(f.refused(&format!("/pty/{id}/connect?ticket={ticket}")).await, 403);
    // A ticket is bound to its terminal.
    let other: Value = request(reqwest::Method::POST, "/pty").header("content-type", "application/json").body(shell_command().to_string()).send().await.unwrap().json().await.unwrap();
    let scoped = mint().await["ticket"].as_str().unwrap().to_string();
    assert_eq!(f.refused(&format!("/pty/{}/connect?ticket={scoped}", other["id"].as_str().unwrap())).await, 403);
    // The query credential works where a browser cannot set headers; a foreign origin is refused before anything else.
    let mut second = f.attach(&format!("/pty/{id}/connect?auth_token={auth_token}&cursor=-1")).await;
    let mut tail = Terminal::default();
    tail.until(&mut second, |t| t.cursor.is_some()).await;
    {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut upgrade = f.socket_url(&format!("/pty/{id}/connect?auth_token={auth_token}")).into_client_request().unwrap();
        upgrade.headers_mut().insert("origin", "https://evil.example".parse().unwrap());
        match tokio_tungstenite::connect_async(upgrade).await {
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => assert_eq!(response.status(), 403),
            other => panic!("expected 403, got {:?}", other.map(|_| ())),
        }
    }

    // Shutting down closes open sockets with 1001 and ends the shells.
    let Fixture { server, .. } = f;
    let closing = tokio::spawn(async move { (terminal.closed(&mut socket).await, tail.closed(&mut second).await) });
    let started = std::time::Instant::now();
    server.shutdown().await;
    assert!(started.elapsed() < Duration::from_secs(20), "shutdown took {:?}", started.elapsed());
    let (first, tailed) = tokio::time::timeout(Duration::from_secs(10), closing).await.unwrap().unwrap();
    assert_eq!((first.0, first.1.as_str()), (1001, "server closing"));
    assert_eq!(tailed.0, 1001);
}

// Reads a v2 stream: `data:` frames as JSON, comment frames counted.
struct V2Events {
    events: Events,
    heartbeats: usize,
}

impl V2Events {
    async fn open(f: &Fixture) -> Self {
        let events = f.events("/api/event").await;
        assert_eq!(events.seen[0]["data"], json!({}));
        assert!(events.seen[0]["id"].as_str().unwrap().starts_with("evt_") && events.seen[0].get("location").is_none());
        V2Events { events, heartbeats: 0 }
    }

    async fn next(&mut self) -> Value {
        loop {
            let frame = self.events.raw().await;
            if frame == ": heartbeat\n\n" {
                self.heartbeats += 1;
                continue;
            }
            let json = frame.strip_prefix("data: ").and_then(|rest| rest.strip_suffix("\n\n")).unwrap_or_else(|| panic!("bad frame {frame:?}"));
            let value: Value = serde_json::from_str(json).unwrap();
            self.events.seen.push(value.clone());
            return value;
        }
    }

    async fn until(&mut self, kind: &str) -> Value {
        loop {
            let event = self.next().await;
            if event["type"] == kind {
                return event;
            }
        }
    }
}

// The openapi component of a v2 event type.
fn event_component<'a>(doc: &'a Value, kind: &str) -> &'a str {
    let schemas = doc["components"]["schemas"].as_object().unwrap();
    let found = schemas.iter().find(|(name, schema)| {
        !name.starts_with("Event") && !name.starts_with("SyncEvent") && schema["properties"]["type"]["enum"] == json!([kind]) && schema["properties"].get("data").is_some()
    });
    found.unwrap_or_else(|| panic!("no component for {kind}")).0
}

#[tokio::test]
async fn v2_operations_are_routed_and_documented() {
    let f = Fixture::new(vec![], ServerOptions::default()).await;
    let doc = document();
    let served: Value = f.http.get(f.url("/openapi.json")).send().await.unwrap().json().await.unwrap();
    let paths = served["paths"].as_object().unwrap();
    assert!(paths.keys().all(|path| v2_operation(path)));
    assert_eq!(paths.values().map(|operations| operations.as_object().unwrap().len()).sum::<usize>(), 61);

    let mut count = 0;
    for (path, operations) in doc["paths"].as_object().unwrap().iter().filter(|(path, _)| v2_operation(path)) {
        let concrete = path
            .replace("{sessionID}", "ses_none")
            .replace("{messageID}", "msg_none")
            .replace("{ptyID}", "pty_none")
            .replace("{requestID}", if path.contains("/question/") { "que_none" } else { "per_none" })
            .replace("/api/fs/read/*", "/api/fs/read/none.txt");
        let concrete = ["{providerID}", "{integrationID}", "{attemptID}", "{credentialID}", "{projectID}", "{id}"].iter().fold(concrete, |path, name| path.replace(name, "none"));
        assert!(!concrete.contains('{'), "{concrete}");
        for method in operations.as_object().unwrap().keys() {
            let method = reqwest::Method::from_bytes(method.to_uppercase().as_bytes()).unwrap();
            let response = f.http.request(method.clone(), f.url(&concrete)).send().await.unwrap();
            let status = response.status().as_u16();
            let streamed = response.headers().get("content-type").is_some_and(|kind| kind.to_str().unwrap().contains("event-stream"));
            if !streamed {
                let text = response.text().await.unwrap();
                assert!(!text.contains("Route not found"), "{method} {path} is not routed: {text}");
                // Every v2 failure is a tagged error.
                if status >= 400 && !text.is_empty() {
                    let body: Value = serde_json::from_str(&text).unwrap_or_else(|_| panic!("{method} {path}: {text}"));
                    assert!(body["_tag"].is_string(), "{method} {path}: {body}");
                }
            }
            count += 1;
        }
    }
    assert_eq!(count, 61);
}

#[tokio::test(flavor = "multi_thread")]
async fn v2_catalog_routes_match_their_schemas() {
    let prepare = |project: &Path| {
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(project.join("src").join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(project.join("notes.md"), "# notes\n").unwrap();
        vec![]
    };
    let f = Fixture::scripted(prepare, ServerOptions::default()).await;
    let doc = document();
    let directory = f.directory();
    // `location[directory]` selects the location; the header is the fallback.
    let located = |path: &str| {
        let separator = if path.contains('?') { '&' } else { '?' };
        format!("{}{path}{separator}location%5Bdirectory%5D={}", f.server.url(), encode(&directory))
    };
    let fetch = |path: &str| {
        let url = located(path);
        let http = f.http.clone();
        async move {
            let response = http.get(url).send().await.unwrap();
            (response.status().as_u16(), response.json::<Value>().await.unwrap())
        }
    };

    let (status, location) = fetch("/api/location").await;
    assert_eq!((status, &location["directory"]), (200, &json!(directory)));
    check_response(&doc, "get", "/api/location", &location);
    let by_header: Value = f.http.get(format!("{}/api/location", f.server.url())).header("x-opencode-directory", encode(&directory)).send().await.unwrap().json().await.unwrap();
    assert_eq!(by_header, location);

    for path in ["/api/agent", "/api/model", "/api/provider", "/api/provider/mock", "/api/integration", "/api/integration/mock", "/api/command", "/api/skill", "/api/pty", "/api/permission/request", "/api/question/request", "/api/reference", "/api/fs/list", "/api/fs/list?path=src", "/api/fs/find?query=main"] {
        let (status, body) = fetch(path).await;
        assert_eq!(status, 200, "{path}: {body}");
        let template = path.split('?').next().unwrap().replace("/provider/mock", "/provider/{providerID}").replace("/integration/mock", "/integration/{integrationID}");
        check_response(&doc, "get", &template, &body);
        assert_eq!(body["location"], location, "{path}");
    }
    let (_, agents) = fetch("/api/agent").await;
    let build = agents["data"].as_array().unwrap().iter().find(|agent| agent["id"] == "build").unwrap();
    assert_eq!((&build["mode"], &build["hidden"]), (&json!("primary"), &json!(false)));
    assert!(build["permissions"].as_array().unwrap().iter().any(|rule| rule["action"] == "edit" && rule["effect"] == "ask"));
    let (_, models) = fetch("/api/model").await;
    assert_eq!((&models["data"][0]["id"], &models["data"][0]["providerID"], &models["data"][0]["name"]), (&json!("m1"), &json!("mock"), &json!("Mock")));
    assert_eq!(fetch("/api/provider/mock").await.1["data"]["id"], "mock");
    let (status, body) = fetch("/api/provider/absent").await;
    assert_eq!((status, body), (404, json!({"_tag": "ProviderNotFoundError", "providerID": "absent", "message": "Provider not found: absent"})));

    let separator = std::path::MAIN_SEPARATOR;
    let (_, listed) = fetch("/api/fs/list").await;
    let entries: Vec<&str> = listed["data"].as_array().unwrap().iter().map(|entry| entry["path"].as_str().unwrap()).collect();
    assert_eq!(entries, [format!("src{separator}").as_str(), "notes.md", "opencode.json"]);
    assert_eq!(fetch("/api/fs/list?path=src").await.1["data"], json!([{"path": format!("src{separator}main.rs"), "type": "file"}]));
    assert_eq!(fetch("/api/fs/find?query=main&type=file").await.1["data"], json!([{"path": "src/main.rs", "type": "file"}]));
    let (status, body) = fetch("/api/fs/find").await;
    assert_eq!((status, &body["_tag"], &body["kind"]), (400, &json!("InvalidRequestError"), &json!("Query")));
    let read = f.http.get(located("/api/fs/read/src/main.rs")).send().await.unwrap();
    assert_eq!((read.status().as_u16(), read.headers()["content-type"].to_str().unwrap()), (200, "application/octet-stream"));
    assert_eq!(read.text().await.unwrap(), "fn main() {}\n");
    let escaped = f.http.get(located("/api/fs/read/..%2Foutside.txt")).send().await.unwrap();
    assert_eq!(escaped.status(), 500);
    assert_eq!(escaped.json::<Value>().await.unwrap()["_tag"], "UnknownError");

    // A stored key connects an integration and shows up as its credential.
    let mut stream = V2Events::open(&f).await;
    let connect = f.http.post(located("/api/integration/mock/connect/key")).header("content-type", "application/json").body(json!({"key": "sk-test"}).to_string()).send().await.unwrap();
    assert_eq!(connect.status(), 204);
    let changed = stream.until("integration.connection.updated").await;
    check(&doc, event_component(&doc, "integration.connection.updated"), &changed);
    assert_eq!((&changed["data"], &changed["location"]), (&json!({"integrationID": "mock"}), &json!({"directory": directory})));
    let (_, integration) = fetch("/api/integration/mock").await;
    assert_eq!(integration["data"]["connections"], json!([{"type": "credential", "id": "mock", "label": "mock"}]));
    assert!(integration["data"]["methods"].as_array().unwrap().contains(&json!({"type": "key"})));
    assert_eq!(f.http.delete(located("/api/credential/mock")).send().await.unwrap().status(), 204);
    assert_eq!(fetch("/api/integration/mock").await.1["data"]["connections"], json!([]));
    let (status, body) = fetch("/api/integration/attempt/none").await;
    assert_eq!((status, &body["_tag"]), (400, &json!("InvalidRequestError")));
    let (status, saved) = fetch("/api/permission/saved").await;
    assert_eq!((status, &saved), (200, &json!({"data": []})));
    check_response(&doc, "get", "/api/permission/saved", &saved);

    // v2 terminals answer in the location envelope.
    let made = f.http.post(located("/api/pty")).header("content-type", "application/json").body(shell_command().to_string()).send().await.unwrap();
    let made: Value = made.json().await.unwrap();
    check_response(&doc, "post", "/api/pty", &made);
    let created = stream.until("pty.created").await;
    check(&doc, event_component(&doc, "pty.created"), &created);
    let pty = made["data"]["id"].as_str().unwrap();
    let token = f.http.post(located(&format!("/api/pty/{pty}/connect-token"))).header("x-opencode-ticket", "1").send().await.unwrap();
    let token: Value = token.json().await.unwrap();
    check_response(&doc, "post", "/api/pty/{ptyID}/connect-token", &token);
    let ticket = token["data"]["ticket"].as_str().unwrap();
    let url = located(&format!("/api/pty/{pty}/connect?ticket={ticket}")).replacen("http://", "ws://", 1);
    let mut socket = tokio_tungstenite::connect_async(url).await.unwrap().0;
    let mut terminal = Terminal::default();
    terminal.until(&mut socket, |t| t.cursor.is_some()).await;
    assert_eq!(f.http.delete(located(&format!("/api/pty/{pty}"))).send().await.unwrap().status(), 204);
    assert_eq!(terminal.closed(&mut socket).await.0, 1000);
    check(&doc, event_component(&doc, "pty.deleted"), &stream.until("pty.deleted").await);
}

#[tokio::test(flavor = "multi_thread")]
async fn v2_sessions_prompt_and_events() {
    let responses = |project: &Path| {
        let arguments = json!({"filePath": project.join("new.txt").to_string_lossy(), "content": "hi\n"});
        vec![tool_reply("write", &arguments), text_reply("All done.")]
    };
    let options = ServerOptions { heartbeat: Duration::from_millis(200), ..ServerOptions::default() };
    let f = Fixture::configured(json!({"permission": {"edit": "allow"}}), responses, options).await;
    let doc = document();
    let directory = f.directory();
    let mut stream = V2Events::open(&f).await;

    let (status, created) = f.post("/api/session", json!({"location": {"directory": directory}, "agent": "build"})).await;
    assert_eq!(status, 200, "{created}");
    check_response(&doc, "post", "/api/session", &created);
    let id = created["data"]["id"].as_str().unwrap().to_string();
    assert_eq!((&created["data"]["location"], &created["data"]["agent"], &created["data"]["cost"]), (&json!({"directory": directory}), &json!("build"), &json!(0)));
    let announced = stream.until("session.created").await;
    check(&doc, event_component(&doc, "session.created"), &announced);
    assert_eq!(announced["location"], json!({"directory": directory}));
    let (status, got) = f.get(&format!("/api/session/{id}")).await;
    assert_eq!((status, &got), (200, &created));
    check_response(&doc, "get", "/api/session/{sessionID}", &got);

    // Malformed ids are rejected before lookup; unknown ones are tagged not-found.
    assert_eq!(f.get("/api/session/nope").await, (400, json!({"_tag": "InvalidRequestError", "message": "Invalid session ID", "field": "sessionID"})));
    assert_eq!(f.get("/api/session/ses_missing").await, (404, json!({"_tag": "SessionNotFoundError", "sessionID": "ses_missing", "message": "Session not found: ses_missing"})));
    let (status, body) = f.post(&format!("/api/session/{id}/prompt"), json!({})).await;
    assert_eq!((status, &body["_tag"], &body["kind"]), (400, &json!("InvalidRequestError"), &json!("Payload")));

    assert_eq!(f.post(&format!("/api/session/{id}/model"), json!({"model": {"id": "m1", "providerID": "mock"}})).await.0, 204);
    let switched = stream.until("session.next.model.switched").await;
    check(&doc, event_component(&doc, "session.next.model.switched"), &switched);
    assert_eq!((&switched["durable"], &switched["data"]["model"]), (&json!({"aggregateID": id, "seq": 1, "version": 1}), &json!({"id": "m1", "providerID": "mock"})));
    assert_eq!(f.post(&format!("/api/session/{id}/agent"), json!({"agent": "build"})).await.0, 204);
    assert_eq!(f.get(&format!("/api/session/{id}")).await.1["data"]["model"], json!({"id": "m1", "providerID": "mock"}));

    let (status, admitted) = f.post(&format!("/api/session/{id}/prompt"), json!({"prompt": {"text": "make a file"}})).await;
    assert_eq!(status, 200, "{admitted}");
    check_response(&doc, "post", "/api/session/{sessionID}/prompt", &admitted);
    let message_id = admitted["data"]["id"].as_str().unwrap().to_string();
    assert_eq!((&admitted["data"]["admittedSeq"], &admitted["data"]["delivery"], &admitted["data"]["prompt"]), (&json!(3), &json!("steer"), &json!({"text": "make a file"})));
    let (status, conflict) = f.post(&format!("/api/session/{id}/prompt"), json!({"id": "bad", "prompt": {"text": "x"}})).await;
    assert_eq!((status, &conflict["_tag"]), (400, &json!("InvalidRequestError")));

    assert_eq!(f.post(&format!("/api/session/{id}/wait"), json!({})).await.0, 204);
    f.engine.settle().await;
    assert_eq!(std::fs::read_to_string(f.project.join("new.txt")).unwrap(), "hi\n");
    assert_eq!(f.get("/api/session/active").await.1, json!({"data": {}}));
    let (status, repeated) = f.post(&format!("/api/session/{id}/prompt"), json!({"id": message_id, "prompt": {"text": "again"}})).await;
    assert_eq!((status, &repeated["_tag"], &repeated["resource"]), (409, &json!("ConflictError"), &json!(message_id)));

    // Every frame of the turn validates against its component, in an order a client can follow.
    let mut kinds = Vec::new();
    loop {
        let event = stream.next().await;
        let kind = event["type"].as_str().unwrap().to_string();
        check(&doc, event_component(&doc, &kind), &event);
        assert!(opencode_server_event_types().contains(&kind.as_str()), "{kind} is not a v2 stream type");
        if kind.starts_with("session.next.") {
            assert_eq!((&event["data"]["sessionID"], &event["location"]), (&json!(id), &json!({"directory": directory})));
        }
        kinds.push(kind);
        if kinds.iter().filter(|kind| *kind == "session.next.step.ended").count() == 2 {
            break;
        }
    }
    let at = |kind: &str| kinds.iter().position(|seen| seen == kind).unwrap_or_else(|| panic!("{kind} missing from {kinds:?}"));
    let order = ["session.next.prompt.admitted", "session.next.prompted", "session.next.step.started", "session.next.tool.input.started", "session.next.tool.called", "session.next.tool.success", "session.next.step.ended", "session.next.text.started", "session.next.text.delta", "session.next.text.ended"];
    assert!(order.windows(2).all(|pair| at(pair[0]) < at(pair[1])), "{kinds:?}");
    assert!(kinds.iter().any(|kind| kind == "message.part.updated") && kinds.iter().any(|kind| kind == "file.edited"));
    assert!(!kinds.iter().any(|kind| matches!(kind.as_str(), "message.part.delta" | "session.status" | "session.idle")), "{kinds:?}");
    tokio::time::sleep(Duration::from_millis(700)).await;
    f.post("/tui/show-toast", json!({"message": "x", "variant": "info"})).await;
    assert_eq!(f.post(&format!("/api/session/{id}/agent"), json!({"agent": "plan"})).await.0, 204);
    assert_eq!(stream.until("session.next.agent.switched").await["data"]["agent"], "plan");
    assert!(stream.heartbeats >= 2, "{}", stream.heartbeats);

    let (status, messages) = f.get(&format!("/api/session/{id}/message?order=asc")).await;
    assert_eq!(status, 200, "{messages}");
    check_response(&doc, "get", "/api/session/{sessionID}/message", &messages);
    let list = messages["data"].as_array().unwrap();
    assert_eq!(list.iter().map(|message| message["type"].as_str().unwrap()).collect::<Vec<_>>(), ["user", "assistant", "assistant"]);
    assert_eq!((&list[0]["id"], &list[0]["text"]), (&json!(message_id), &json!("make a file")));
    let tool = &list[1]["content"].as_array().unwrap().iter().find(|part| part["type"] == "tool").unwrap();
    assert_eq!((&tool["name"], &tool["state"]["status"], &tool["id"]), (&json!("write"), &json!("completed"), &json!("call_1")));
    assert_eq!(list[2]["content"], json!([{"type": "text", "id": list[2]["content"][0]["id"], "text": "All done."}]));
    assert_eq!((&list[2]["model"], &list[2]["finish"]), (&json!({"id": "m1", "providerID": "mock"}), &json!("stop")));
    let (_, context) = f.get(&format!("/api/session/{id}/context")).await;
    check_response(&doc, "get", "/api/session/{sessionID}/context", &context);
    assert_eq!(&context["data"], &messages["data"]);
    let (status, single) = f.get(&format!("/api/session/{id}/message/{message_id}")).await;
    assert_eq!((status, &single["data"]), (200, &list[0]));
    check_response(&doc, "get", "/api/session/{sessionID}/message/{messageID}", &single);
    let (status, body) = f.get(&format!("/api/session/{id}/message/msg_missing")).await;
    assert_eq!((status, &body["_tag"], &body["messageID"]), (404, &json!("MessageNotFoundError"), &json!("msg_missing")));

    // Pages follow the opaque cursors in both directions.
    let (_, newest) = f.get(&format!("/api/session/{id}/message?limit=1")).await;
    assert_eq!(newest["data"][0]["id"], list[2]["id"]);
    let next = newest["cursor"]["next"].as_str().unwrap();
    let (_, older) = f.get(&format!("/api/session/{id}/message?limit=1&cursor={next}")).await;
    assert_eq!(older["data"][0]["id"], list[1]["id"]);
    let previous = older["cursor"]["previous"].as_str().unwrap();
    assert_eq!(f.get(&format!("/api/session/{id}/message?limit=5&cursor={previous}")).await.1["data"][0]["id"], list[2]["id"]);
    let (status, body) = f.get(&format!("/api/session/{id}/message?cursor={next}&order=asc")).await;
    assert_eq!((status, body), (400, json!({"_tag": "InvalidCursorError", "message": "Cursor cannot be combined with order"})));
    assert_eq!(f.get(&format!("/api/session/{id}/message?cursor=zzz")).await, (400, json!({"_tag": "InvalidCursorError", "message": "Invalid cursor"})));
    assert_eq!(f.get(&format!("/api/session/{id}/message?limit=0")).await.0, 400);

    let (status, history) = f.get(&format!("/api/session/{id}/history?limit=100")).await;
    assert_eq!(status, 200);
    check_response(&doc, "get", "/api/session/{sessionID}/history", &history);
    let durable = history["data"].as_array().unwrap();
    // One sequence per session, counted from 0 (`session.created`) and shared with the v1 events of its row writes.
    let seqs: Vec<u64> = durable.iter().map(|event| event["durable"]["seq"].as_u64().unwrap()).collect();
    assert!(seqs[0] == 1 && seqs.windows(2).all(|pair| pair[0] < pair[1]), "{seqs:?}");
    assert!(durable.iter().all(|event| event["type"].as_str().unwrap().starts_with("session.next.") && event.get("location").is_none()));
    assert!(!durable.iter().any(|event| event["type"].as_str().unwrap().ends_with(".delta")) && history["hasMore"] == false);
    let (_, tail) = f.get(&format!("/api/session/{id}/history?after=2&limit=2")).await;
    assert_eq!((tail["data"][0]["durable"]["seq"].as_u64(), tail["data"].as_array().unwrap().len(), &tail["hasMore"]), (Some(3), 2, &json!(true)));
    assert_eq!(f.get(&format!("/api/session/{id}/history?limit=101")).await.0, 400);
    let replay = f.http.get(f.url(&format!("/api/session/{id}/event?after=2"))).send().await.unwrap();
    assert_eq!(replay.headers()["content-type"], "text/event-stream");
    let mut frames = replay.bytes_stream();
    let first = String::from_utf8(frames.next().await.unwrap().unwrap().to_vec()).unwrap();
    assert!(first.starts_with("id: 3\nevent: session.next.prompt.admitted\ndata: {"), "{first}");

    let (status, sessions) = f.get(&format!("/api/session?directory={}", encode(&directory))).await;
    assert_eq!(status, 200);
    check_response(&doc, "get", "/api/session", &sessions);
    let row = &sessions["data"][0];
    assert_eq!(&row["id"], &json!(id));
    assert!(row["tokens"]["input"].as_f64().unwrap() > 0.0 && row["title"].is_string());
    let (_, other) = f.post("/api/session", json!({"location": {"directory": directory}})).await;
    let (_, page) = f.get("/api/session?limit=1").await;
    assert_eq!(page["data"][0]["id"], other["data"]["id"]);
    let (_, rest) = f.get(&format!("/api/session?cursor={}", page["cursor"]["next"].as_str().unwrap())).await;
    assert_eq!(rest["data"].as_array().unwrap().iter().map(|s| &s["id"]).collect::<Vec<_>>(), [&json!(id)]);
    assert_eq!(f.get("/api/session?cursor=zzz").await, (400, json!({"_tag": "InvalidCursorError", "message": "Invalid cursor"})));
    assert_eq!(f.get("/api/session?search=zzzz-nothing").await.1["data"], json!([]));
    assert_eq!(f.post(&format!("/api/session/{id}/interrupt"), json!({})).await.0, 204);
}

fn opencode_server_event_types() -> Vec<&'static str> {
    let doc: Value = serde_json::from_str(openapi()).unwrap();
    let union = doc["components"]["schemas"]["V2Event"]["anyOf"].as_array().unwrap().len();
    assert_eq!(union, 88);
    // The stream carries the server subset of that union.
    let listed = include_str!("../src/v2/mod.rs");
    let start = listed.find("pub const EVENT_TYPES").unwrap();
    let block = &listed[start..start + listed[start..].find("];").unwrap()];
    block.split('"').skip(1).step_by(2).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn v2_permissions_questions_and_reverts() {
    let responses = |project: &Path| {
        git(project, &["init", "-q"]);
        std::fs::write(project.join("keep.txt"), "one\n").unwrap();
        git(project, &["add", "keep.txt"]);
        git(project, &["commit", "-q", "-m", "init"]);
        let arguments = json!({"filePath": project.join("new.txt").to_string_lossy(), "content": "hi\n"});
        vec![tool_reply("write", &arguments), text_reply("Done.")]
    };
    let f = Fixture::scripted(responses, ServerOptions::default()).await;
    let doc = document();
    let id = f.session().await;
    let mut stream = V2Events::open(&f).await;
    let (status, admitted) = f.post(&format!("/api/session/{id}/prompt"), json!({"prompt": {"text": "write it"}})).await;
    assert_eq!(status, 200, "{admitted}");
    let message_id = admitted["data"]["id"].as_str().unwrap().to_string();

    let asked = stream.until("permission.v2.asked").await;
    check(&doc, event_component(&doc, "permission.v2.asked"), &asked);
    let request = asked["data"]["id"].as_str().unwrap().to_string();
    assert_eq!((&asked["data"]["action"], &asked["data"]["source"]["type"]), (&json!("edit"), &json!("tool")));
    let (_, pending) = f.get(&format!("/api/session/{id}/permission")).await;
    check_response(&doc, "get", "/api/session/{sessionID}/permission", &pending);
    assert_eq!(pending["data"], json!([asked["data"]]));
    let (_, one) = f.get(&format!("/api/session/{id}/permission/{request}")).await;
    check_response(&doc, "get", "/api/session/{sessionID}/permission/{requestID}", &one);
    let located: Value = f.http.get(f.url("/api/permission/request")).header("x-opencode-directory", encode(&f.directory())).send().await.unwrap().json().await.unwrap();
    assert_eq!(located["data"], pending["data"]);
    assert_eq!(f.get("/api/session/active").await.1["data"][&id], json!({"type": "running"}));
    let (status, body) = f.post(&format!("/api/session/{id}/permission/per_missing/reply"), json!({"reply": "once"})).await;
    assert_eq!((status, body), (404, json!({"_tag": "PermissionNotFoundError", "requestID": "per_missing", "message": "Permission request not found: per_missing"})));
    assert_eq!(f.post(&format!("/api/session/{id}/permission/{request}/reply"), json!({"reply": "sometimes"})).await.0, 400);
    assert_eq!(f.post(&format!("/api/session/{id}/permission/{request}/reply"), json!({"reply": "always"})).await.0, 204);
    let replied = stream.until("permission.v2.replied").await;
    check(&doc, event_component(&doc, "permission.v2.replied"), &replied);
    assert_eq!(replied["data"], json!({"sessionID": id, "requestID": request, "reply": "always"}));
    assert_eq!(f.post(&format!("/api/session/{id}/wait"), json!({})).await.0, 204);
    assert!(f.project.join("new.txt").exists());

    // The "always" answer is listed as saved and can be dropped again.
    let (_, saved) = f.get("/api/permission/saved").await;
    check_response(&doc, "get", "/api/permission/saved", &saved);
    let entry = &saved["data"][0];
    assert_eq!(&entry["action"], &json!("edit"));
    assert_eq!(f.engine.approvals().len(), 1);
    assert_eq!(f.send(reqwest::Method::DELETE, &format!("/api/permission/saved/{}", entry["id"].as_str().unwrap()), None).await.0, 204);
    assert_eq!(f.get("/api/permission/saved").await.1["data"], json!([]));
    // The engine asks again from now on.
    assert!(f.engine.approvals().is_empty());

    // A client can ask on its own; the answer is the effect of the session's rules.
    let (status, effect) = f.post(&format!("/api/session/{id}/permission"), json!({"action": "bash", "resources": ["rm -rf /"]})).await;
    assert_eq!((status, &effect["data"]["effect"]), (200, &json!("ask")), "{effect}");
    check_response(&doc, "post", "/api/session/{sessionID}/permission", &effect);
    let own = effect["data"]["id"].as_str().unwrap();
    assert_eq!(f.get(&format!("/api/session/{id}/permission/{own}")).await.1["data"]["resources"], json!(["rm -rf /"]));
    assert_eq!(f.post(&format!("/api/session/{id}/permission/{own}/reply"), json!({"reply": "reject"})).await.0, 204);
    assert_eq!(f.get(&format!("/api/session/{id}/permission")).await.1["data"], json!([]));

    let (_, questions) = f.get(&format!("/api/session/{id}/question")).await;
    check_response(&doc, "get", "/api/session/{sessionID}/question", &questions);
    let (status, body) = f.post(&format!("/api/session/{id}/question/que_missing/reply"), json!({"answers": []})).await;
    assert_eq!((status, &body["_tag"]), (404, &json!("QuestionNotFoundError")));
    assert_eq!(f.post(&format!("/api/session/{id}/question/que_missing/reject"), json!({})).await.0, 404);

    // Staging hides the turn and restores the files; clearing brings both back; committing deletes for good.
    let (status, staged) = f.post(&format!("/api/session/{id}/revert/stage"), json!({"messageID": message_id})).await;
    assert_eq!(status, 200, "{staged}");
    check_response(&doc, "post", "/api/session/{sessionID}/revert/stage", &staged);
    assert_eq!(staged["data"]["messageID"], json!(message_id));
    assert!(!f.project.join("new.txt").exists());
    check(&doc, event_component(&doc, "session.next.revert.staged"), &stream.until("session.next.revert.staged").await);
    assert_eq!(f.get(&format!("/api/session/{id}")).await.1["data"]["revert"]["messageID"], json!(message_id));
    assert_eq!(f.post(&format!("/api/session/{id}/revert/clear"), json!({})).await.0, 204);
    assert!(f.project.join("new.txt").exists());
    assert!(f.get(&format!("/api/session/{id}")).await.1["data"].get("revert").is_none());
    let (status, body) = f.post(&format!("/api/session/{id}/revert/stage"), json!({"messageID": "msg_missing"})).await;
    assert_eq!((status, &body["_tag"]), (404, &json!("MessageNotFoundError")));
    assert_eq!(f.post(&format!("/api/session/{id}/revert/stage"), json!({"messageID": message_id})).await.0, 200);
    assert_eq!(f.post(&format!("/api/session/{id}/revert/commit"), json!({})).await.0, 204);
    assert_eq!(f.get(&format!("/api/session/{id}/message")).await.1["data"], json!([]));
    check(&doc, event_component(&doc, "session.next.revert.committed"), &stream.until("session.next.revert.committed").await);
}

#[tokio::test]
async fn v2_auth_compression_limits_and_idle_disposal() {
    let options = ServerOptions { password: Some("secret".into()), ..ServerOptions::default() };
    let f = Fixture::new(vec![], options).await;
    let basic = "Basic b3BlbmNvZGU6c2VjcmV0";
    // v2 paths answer 401 with a tagged body, legacy ones with none; both challenge.
    for path in ["/api/health", "/api/session", "/experimental/project/p/copy"] {
        let method = if path.ends_with("copy") { reqwest::Method::POST } else { reqwest::Method::GET };
        let denied = f.http.request(method, f.url(path)).send().await.unwrap();
        assert_eq!((denied.status().as_u16(), denied.headers()["www-authenticate"].to_str().unwrap()), (401, "Basic realm=\"Secure Area\""));
        assert_eq!(denied.json::<Value>().await.unwrap(), json!({"_tag": "UnauthorizedError", "message": "Authentication required"}), "{path}");
    }
    let legacy = f.http.get(f.url("/session")).send().await.unwrap();
    assert_eq!(legacy.status(), 401);
    assert!(legacy.text().await.unwrap().is_empty());
    let allowed = f.http.get(f.url("/api/health")).header("authorization", basic).send().await.unwrap();
    assert_eq!(allowed.json::<Value>().await.unwrap(), json!({"healthy": true}));

    // Large buffered JSON is compressed on request; small bodies and streams never are.
    let get = |path: &str, encoding: &'static str| f.http.get(f.url(path)).header("authorization", basic).header("accept-encoding", encoding).send();
    let packed = get("/doc", "br, gzip, deflate").await.unwrap();
    assert_eq!(packed.headers()["content-encoding"], "gzip");
    let bytes = packed.bytes().await.unwrap();
    assert_eq!(&bytes[..2], &[0x1f, 0x8b]);
    assert!(bytes.len() < openapi().len() / 4);
    assert_eq!(get("/doc", "deflate").await.unwrap().headers()["content-encoding"], "deflate");
    assert!(get("/doc", "br").await.unwrap().headers().get("content-encoding").is_none());
    assert!(get("/global/health", "gzip").await.unwrap().headers().get("content-encoding").is_none());
    let stream = get("/api/event", "gzip").await.unwrap();
    assert!(stream.headers().get("content-encoding").is_none() && stream.headers()["content-type"] == "text/event-stream");
    drop(stream);

    // Bodies beyond the limit are refused instead of buffered.
    let huge = vec![b' '; opencode_server::MAX_BODY_BYTES + 1];
    let refused = f.http.post(f.url("/log")).header("authorization", basic).header("content-type", "application/json").body(huge).send().await.unwrap();
    assert_eq!(refused.status(), 413);

    // An instance nobody uses is disposed; one with an open event stream is not.
    let watched = f.http.get(f.url("/event")).header("authorization", basic).send().await.unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    let mut global = {
        let response = f.http.get(f.url("/global/event")).header("authorization", basic).send().await.unwrap();
        let mut stream = response.bytes_stream();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(Ok(chunk)) = stream.next().await {
                if tx.send(String::from_utf8_lossy(&chunk).into_owned()).is_err() {
                    return;
                }
            }
        });
        rx
    };
    f.server.dispose_idle(Duration::from_millis(50)).await;
    drop(watched);
    tokio::time::sleep(Duration::from_millis(120)).await;
    let mut seen = String::new();
    while let Ok(chunk) = global.try_recv() {
        seen.push_str(&chunk);
    }
    assert!(!seen.contains("server.instance.disposed"), "{seen}");
    f.server.dispose_idle(Duration::from_millis(50)).await;
    let disposed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let chunk = global.recv().await.unwrap();
            if chunk.contains("server.instance.disposed") {
                return chunk;
            }
        }
    })
    .await
    .unwrap();
    assert!(disposed.contains(&serde_json::to_string(&f.directory()).unwrap()), "{disposed}");
}

