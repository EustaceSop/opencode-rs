// The client against the Rust server in-process: in-memory store, tempdir paths, a scripted provider on a local
// socket, no model catalog download.
use opencode_client::{Client, CreateSession, Credentials, Error, Event, PermissionReply, PromptBody, PromptPart, Reconnect, SessionPatch, SessionQuery, SessionStatus};
use opencode_core::paths::Paths;
use opencode_server::{Server, ServerOptions, serve_on};
use opencode_session::Engine;
use opencode_storage::{Message, PartBody, Store};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn sse(chunks: &[Value]) -> String {
    let mut out: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    out.push_str("data: [DONE]\n\n");
    out
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

// Scripted replies in order; title requests are answered on the side.
async fn provider(responses: Vec<String>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let script = Arc::new(Mutex::new(VecDeque::from(responses)));
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let script = script.clone();
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let body = loop {
                    let mut chunk = [0u8; 8192];
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buffer.extend_from_slice(&chunk[..n]);
                    let Some(split) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                    let head = String::from_utf8_lossy(&buffer[..split]).to_lowercase();
                    let length: usize = head.lines().find_map(|l| l.strip_prefix("content-length:")).map_or(0, |v| v.trim().parse().unwrap());
                    if buffer.len() >= split + 4 + length {
                        break String::from_utf8_lossy(&buffer[split + 4..]).into_owned();
                    }
                };
                let response = if body.contains("Generate a title for this conversation") || body.contains("You are a title generator") {
                    text_reply("Mock title")
                } else {
                    let next = script.lock().unwrap().pop_front();
                    match next {
                        Some(response) => response,
                        None => return,
                    }
                };
                let reply = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{response}", response.len());
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    port
}

struct Fixture {
    _root: tempfile::TempDir,
    project: PathBuf,
    other: PathBuf,
    server: Server,
}

impl Fixture {
    async fn new(responses: Vec<String>, password: Option<&str>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let canonical = root.path().canonicalize().unwrap();
        let base = PathBuf::from(canonical.to_str().unwrap().trim_start_matches(r"\\?\"));
        let project = base.join("project");
        let other = base.join("other");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let paths = Paths { home: base.join("home"), data: base.join("data"), cache: base.join("cache"), config: base.join("config"), state: base.join("state"), tmp: base.join("tmp") };
        std::fs::create_dir_all(&paths.cache).unwrap();
        let port = provider(responses).await;
        let config = json!({"model": "mock/m1", "permission": {"edit": "ask"}, "provider": {"mock": {"options": {"baseURL": format!("http://127.0.0.1:{port}/v1"), "apiKey": "k"}, "models": {"m1": {"name": "Mock"}}}}});
        std::fs::write(project.join("opencode.json"), config.to_string()).unwrap();
        let catalog = paths.cache.join("models.json");
        std::fs::write(&catalog, "{}").unwrap();
        let source = opencode_llm::catalog::Source { url: "http://127.0.0.1:9".into(), file: catalog.clone(), locks: paths.state.join("locks"), override_file: Some(catalog), disabled: true };
        let engine = Engine::with_store(paths, Store::open_in_memory().unwrap());
        engine.set_catalog_source(source);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let options = ServerOptions { directory: Some(project.to_string_lossy().into_owned()), password: password.map(str::to_string), ..ServerOptions::default() };
        let server = serve_on(listener, options, Arc::new(engine)).await.unwrap();
        Fixture { _root: root, project, other, server }
    }

    fn client(&self) -> Client {
        Client::new(&self.server.url()).with_directory(Some(self.project.to_string_lossy().into_owned()))
    }
}

async fn until(events: &mut opencode_client::EventStream, what: &str, done: impl Fn(&Event) -> bool) -> Vec<Event> {
    let mut seen = Vec::new();
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(20), events.next()).await.unwrap_or_else(|_| panic!("timed out waiting for {what}")).expect("stream ended");
        let finished = done(&frame.event);
        seen.push(frame.event);
        if finished {
            return seen;
        }
    }
}

#[tokio::test]
async fn sessions_prompt_and_stream_events() {
    let fx = Fixture::new(vec![text_reply("Hello there")], None).await;
    let client = fx.client();
    assert!(client.health().await.unwrap().healthy);
    assert_eq!(client.path().await.unwrap().directory, fx.project.to_string_lossy());
    let providers = client.config_providers().await.unwrap();
    assert!(providers.providers.iter().any(|p| p.id == "mock" && p.models.contains_key("m1")));
    assert!(client.agents().await.unwrap().iter().any(|a| a.name == "build" && a.mode == "primary"));

    let mut events = client.events().await.unwrap();
    assert_eq!(events.next().await.unwrap().event, Event::ServerConnected);
    let session = client.create_session(&CreateSession { title: Some("First".into()), ..Default::default() }).await.unwrap();
    assert_eq!(session.directory, fx.project.to_string_lossy());
    let created = until(&mut events, "session.created", |e| matches!(e, Event::SessionCreated(_))).await;
    assert!(matches!(created.last(), Some(Event::SessionCreated(info)) if info.id == session.id));

    let body = PromptBody { parts: vec![PromptPart::Text { text: "hi".into() }], ..Default::default() };
    let answer = client.prompt(&session.id, &body).await.unwrap();
    assert!(matches!(answer.info, Message::Assistant(_)));
    let streamed = until(&mut events, "idle", |e| matches!(e, Event::SessionStatus { status: SessionStatus::Idle, .. })).await;
    let deltas: String = streamed.iter().filter_map(|e| match e { Event::PartDelta { delta, field, .. } if field == "text" => Some(delta.as_str()), _ => None }).collect();
    assert_eq!(deltas, "Hello there");
    assert!(streamed.iter().any(|e| matches!(e, Event::SessionStatus { status: SessionStatus::Busy, .. })));
    assert!(streamed.iter().any(|e| matches!(e, Event::MessageUpdated(m) if matches!(**m, Message::User(_)))));
    assert!(streamed.iter().any(|e| matches!(e, Event::PartUpdated(p) if matches!(&p.body, PartBody::StepFinish(_)))));

    let messages = client.messages(&session.id, None).await.unwrap();
    assert_eq!(messages.len(), 2);
    assert!(messages[1].parts.iter().any(|p| matches!(&p.body, PartBody::Text(t) if t.text == "Hello there")));
    assert_eq!(client.messages(&session.id, Some(1)).await.unwrap().len(), 1);

    let renamed = client.update_session(&session.id, &SessionPatch { title: Some("Renamed".into()) }).await.unwrap();
    assert_eq!(renamed.title, "Renamed");
    let forked = client.fork(&session.id, None).await.unwrap();
    assert_ne!(forked.id, session.id);
    let roots = client.sessions(&SessionQuery { roots: Some(true), ..Default::default() }).await.unwrap();
    assert_eq!(roots.len(), 2);
    assert!(client.status().await.unwrap().is_empty());
    assert!(client.todos(&session.id).await.unwrap().is_empty());
    assert!(client.delete_session(&forked.id).await.unwrap());
    let missing = client.session(&forked.id).await.unwrap_err();
    assert!(opencode_client::is_not_found(&missing), "{missing:?}");
    assert!(missing.to_string().contains("Session not found"), "{missing}");

    // Reads of another directory see none of this project's sessions.
    let elsewhere = Client::new(&fx.server.url()).with_directory(Some(fx.other.to_string_lossy().into_owned()));
    assert!(elsewhere.sessions(&SessionQuery::default()).await.unwrap().is_empty());
    assert_eq!(elsewhere.path().await.unwrap().directory, fx.other.to_string_lossy());
}

#[tokio::test]
async fn permission_requests_round_trip() {
    let write = json!({"filePath": "a.txt", "content": "remote"});
    let fx = Fixture::new(vec![tool_reply("write", &write), text_reply("Written")], None).await;
    let client = fx.client();
    let mut events = client.events().await.unwrap();
    let session = client.create_session(&CreateSession::default()).await.unwrap();
    let body = PromptBody { parts: vec![PromptPart::Text { text: "write it".into() }], ..Default::default() };
    client.prompt_async(&session.id, &body).await.unwrap();

    let asked = until(&mut events, "permission.asked", |e| matches!(e, Event::PermissionAsked(_))).await;
    let Some(Event::PermissionAsked(request)) = asked.last() else { unreachable!() };
    assert_eq!((request.session_id.as_str(), request.permission.as_str()), (session.id.as_str(), "edit"));
    assert!(request.tool.is_some());
    assert_eq!(client.permissions().await.unwrap(), vec![request.clone()]);
    assert!(!fx.project.join("a.txt").exists());
    assert!(client.reply_permission(&request.id, PermissionReply::Once, None).await.unwrap());

    let rest = until(&mut events, "idle", |e| matches!(e, Event::SessionStatus { status: SessionStatus::Idle, .. })).await;
    assert!(rest.iter().any(|e| matches!(e, Event::PermissionReplied { reply, .. } if reply == "once")));
    assert_eq!(std::fs::read_to_string(fx.project.join("a.txt")).unwrap(), "remote");
    assert!(client.permissions().await.unwrap().is_empty());
    let unknown = client.reply_permission("per_missing", PermissionReply::Reject, None).await.unwrap_err();
    assert_eq!(unknown.status(), Some(404));
}

#[tokio::test]
async fn basic_auth_guards_every_route() {
    let fx = Fixture::new(Vec::new(), Some("s3cret")).await;
    let anonymous = fx.client();
    assert!(matches!(anonymous.health().await, Err(Error::Status { status: 401, .. })));
    assert!(anonymous.events().await.is_err());
    let wrong = fx.client().with_credentials(Some(Credentials::new("opencode", "nope")));
    assert_eq!(wrong.sessions(&SessionQuery::default()).await.unwrap_err().status(), Some(401));
    let authed = fx.client().with_credentials(Some(Credentials::new("opencode", "s3cret")));
    assert!(authed.health().await.unwrap().healthy);
    let mut events = authed.events().await.unwrap();
    assert_eq!(events.next().await.unwrap().event, Event::ServerConnected);
}

// A stand-in SSE server: the first connection sends one event and closes, the second stays open.
#[tokio::test]
async fn event_streams_reconnect_after_a_drop() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(Mutex::new(Vec::<String>::new()));
    let log = accepted.clone();
    tokio::spawn(async move {
        for round in 0.. {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let mut head = vec![0u8; 4096];
            let n = socket.read(&mut head).await.unwrap_or(0);
            log.lock().unwrap().push(String::from_utf8_lossy(&head[..n]).lines().next().unwrap_or_default().to_string());
            let event = json!({"id": format!("evt_{round}"), "type": "server.connected", "properties": {}});
            let todo = json!({"type": "todo.updated", "properties": {"sessionID": "ses_1", "todos": [{"content": "x", "status": "pending", "priority": "high"}]}});
            let body = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\ndata: {event}\n\ndata: {todo}\n\n");
            let _ = socket.write_all(body.as_bytes()).await;
            if round == 0 {
                let _ = socket.shutdown().await;
                continue;
            }
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(30)).await;
                drop(socket);
            });
        }
    });
    let client = Client::new(&format!("http://127.0.0.1:{port}")).with_directory(Some("/w s".into()));
    let mut events = client.events_with(Reconnect::after(Duration::from_millis(20), Duration::from_millis(100))).await.unwrap();
    let mut ids = Vec::new();
    for _ in 0..4 {
        let frame = tokio::time::timeout(Duration::from_secs(10), events.next()).await.unwrap().unwrap();
        ids.push((frame.id.clone(), matches!(frame.event, Event::TodoUpdated { .. })));
    }
    assert_eq!(ids, vec![(Some("evt_0".into()), false), (None, true), (Some("evt_1".into()), false), (None, true)]);
    let requests = accepted.lock().unwrap().clone();
    assert_eq!(requests, vec!["GET /event?directory=%2Fw+s HTTP/1.1".to_string(); 2]);
    drop(events);

    let mut once = client.events_with(Reconnect::never()).await.unwrap();
    assert!(once.next().await.is_some());
}
