// ACP end to end over in-memory pipes: an engine on an in-memory store, tempdir paths, a scripted provider on a local
// socket and no catalog download.
use opencode_acp::{AcpOptions, serve};
use opencode_core::paths::Paths;
use opencode_session::Engine;
use opencode_storage::Store;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines};
use tokio::net::TcpListener;

fn sse(chunks: &[Value]) -> String {
    let mut out: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    out.push_str("data: [DONE]\n\n");
    out
}

fn text_reply(text: &str) -> Reply {
    Reply::Full(sse(&[
        json!({"choices": [{"index": 0, "delta": {"content": text}}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 20, "completion_tokens": 2, "total_tokens": 22}}),
    ]))
}

fn tool_reply(name: &str, arguments: &Value) -> Reply {
    Reply::Full(sse(&[
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": name, "arguments": arguments.to_string()}}]}}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}], "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}}),
    ]))
}

enum Reply {
    Full(String),
    // The first chunk, then the stream stays open.
    Stall(String),
}

// Serves the scripted replies in order; title requests are answered on the side.
async fn provider(responses: Vec<Reply>) -> u16 {
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
                    if n == 0 {
                        return;
                    }
                    buffer.extend_from_slice(&chunk[..n]);
                    let Some(split) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                    let head = String::from_utf8_lossy(&buffer[..split]).to_lowercase();
                    let length: usize = head.lines().find_map(|l| l.strip_prefix("content-length:")).unwrap().trim().parse().unwrap();
                    if buffer.len() >= split + 4 + length {
                        break String::from_utf8_lossy(&buffer[split + 4..]).into_owned();
                    }
                };
                let reply = if body.contains("Generate a title for this conversation") {
                    text_reply("Mock title")
                } else {
                    script.lock().unwrap().pop_front().expect("unscripted provider request")
                };
                match reply {
                    Reply::Full(text) => {
                        let head = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\ncontent-length: {}\r\n\r\n", text.len());
                        socket.write_all(format!("{head}{text}").as_bytes()).await.unwrap();
                        socket.shutdown().await.ok();
                    }
                    Reply::Stall(first) => {
                        let chunk = format!("data: {}\n\n", json!({"choices": [{"index": 0, "delta": {"content": first}}]}));
                        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
                        socket.write_all(format!("{head}{chunk}").as_bytes()).await.unwrap();
                        let mut rest = [0u8; 64];
                        let _ = tokio::time::timeout(Duration::from_secs(60), socket.read(&mut rest)).await;
                    }
                }
            });
        }
    });
    port
}

// An engine on an in-memory store whose model catalog is an empty local file and is never downloaded.
fn offline_engine(paths: Paths) -> Engine {
    std::fs::create_dir_all(&paths.cache).unwrap();
    let file = paths.cache.join("models.json");
    std::fs::write(&file, "{}").unwrap();
    let source = opencode_llm::catalog::Source { url: "http://127.0.0.1:9".into(), file: file.clone(), locks: paths.state.join("locks"), override_file: Some(file), disabled: true };
    let engine = Engine::with_store(paths, Store::open_in_memory().unwrap());
    engine.set_catalog_source(source);
    engine
}

struct Client {
    writer: DuplexStream,
    lines: Lines<BufReader<DuplexStream>>,
    next: u64,
    seen: Vec<Value>,
}

impl Client {
    fn connect(engine: Arc<Engine>, options: AcpOptions) -> Self {
        let (writer, agent_in) = tokio::io::duplex(1 << 20);
        let (agent_out, reader) = tokio::io::duplex(1 << 20);
        tokio::spawn(serve(agent_in, agent_out, options, engine));
        Client { writer, lines: BufReader::new(reader).lines(), next: 1, seen: Vec::new() }
    }

    async fn send(&mut self, message: Value) {
        self.writer.write_all(format!("{message}\n").as_bytes()).await.unwrap();
    }

    async fn request(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next;
        self.next += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await;
        id
    }

    async fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params})).await;
    }

    async fn respond(&mut self, id: &Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result})).await;
    }

    async fn next(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(30), self.lines.next_line()).await.expect("timed out waiting for a message").unwrap().expect("stream ended");
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        self.seen.push(value.clone());
        value
    }

    async fn until(&mut self, test: impl Fn(&Value) -> bool) -> Value {
        loop {
            let message = self.next().await;
            if test(&message) {
                return message;
            }
        }
    }

    // The response to request `id`; everything before it stays in `seen`.
    async fn response(&mut self, id: u64) -> Value {
        self.until(|m| m.get("method").is_none() && m["id"] == id).await
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.request(method, params).await;
        let response = self.response(id).await;
        assert!(response.get("error").is_none(), "{method} failed: {response}");
        response["result"].clone()
    }

    async fn new_session(&mut self, cwd: &Path) -> String {
        let result = self.call("session/new", json!({"cwd": cwd, "mcpServers": []})).await;
        result["sessionId"].as_str().unwrap().to_string()
    }

    // The `session/update` payloads seen so far, for one session.
    fn updates(&self, session: &str) -> Vec<Value> {
        self.seen.iter().filter(|m| m["method"] == "session/update" && m["params"]["sessionId"] == session).map(|m| m["params"]["update"].clone()).collect()
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    project: PathBuf,
    engine: Arc<Engine>,
}

impl Fixture {
    // `responses` sees the project directory; `extra` is merged over the fixture's opencode.json.
    async fn new(responses: impl FnOnce(&Path) -> Vec<Reply>, extra: Value) -> Self {
        let root = tempfile::tempdir().unwrap();
        let canonical = root.path().canonicalize().unwrap();
        let base = PathBuf::from(canonical.to_str().unwrap().trim_start_matches(r"\\?\"));
        let project = base.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let paths = Paths { home: base.join("home"), data: base.join("data"), cache: base.join("cache"), config: base.join("config"), state: base.join("state"), tmp: base.join("tmp") };
        std::fs::create_dir_all(&paths.data).unwrap();
        let port = provider(responses(&project)).await;
        let model = json!({"name": "Mock", "limit": {"context": 1000, "output": 100}});
        let mut config = json!({"model": "mock/m1", "provider": {"mock": {"name": "Mock Provider", "options": {"baseURL": format!("http://127.0.0.1:{port}/v1"), "apiKey": "k"}, "models": {"m1": model}}}});
        opencode_core::config::merge_deep(&mut config, extra);
        std::fs::write(project.join("opencode.json"), config.to_string()).unwrap();
        Fixture { _root: root, project, engine: Arc::new(offline_engine(paths)) }
    }

    fn client(&self) -> Client {
        Client::connect(self.engine.clone(), AcpOptions { cwd: Some(self.project.clone()), version: "9.9.9".into() })
    }

}

fn file(project: &Path, name: &str) -> String {
    project.join(name).to_string_lossy().into_owned()
}

fn text(prompt: &str) -> Value {
    json!([{"type": "text", "text": prompt}])
}

#[tokio::test]
async fn initialize_and_authenticate_match_the_original() {
    let fixture = Fixture::new(|_| Vec::new(), json!({})).await;
    let mut client = fixture.client();
    let result = client.call("initialize", json!({"protocolVersion": 1, "clientCapabilities": {"fs": {"readTextFile": true}, "_meta": {"terminal-auth": true}}})).await;
    let expected = json!({
        "protocolVersion": 1,
        "agentCapabilities": {
            "loadSession": true,
            "mcpCapabilities": {"http": true, "sse": true},
            "promptCapabilities": {"embeddedContext": true, "image": true},
            "sessionCapabilities": {"close": {}, "fork": {}, "list": {}, "resume": {}},
        },
        "authMethods": [{
            "description": "Run `opencode auth login` in the terminal",
            "name": "Login with opencode",
            "id": "opencode-login",
            "_meta": {"terminal-auth": {"command": "opencode", "args": ["auth", "login"], "label": "OpenCode Login"}},
        }],
        "agentInfo": {"name": "OpenCode", "version": "9.9.9"},
    });
    assert_eq!(result, expected);
    // Key order is the original's too.
    assert_eq!(serde_json::to_string(&result).unwrap(), serde_json::to_string(&expected).unwrap());

    let plain = client.call("initialize", json!({"protocolVersion": 1})).await;
    assert!(plain["authMethods"][0].get("_meta").is_none());
    assert_eq!(client.call("authenticate", json!({"methodId": "opencode-login"})).await, json!({}));
    let id = client.request("authenticate", json!({"methodId": "nope"})).await;
    let rejected = client.response(id).await;
    assert_eq!(rejected["error"], json!({"code": -32602, "message": "Invalid params: unknown auth method: nope", "data": {"methodId": "nope"}}));
    let id = client.request("providers/list", json!({})).await;
    assert_eq!(client.response(id).await["error"], json!({"code": -32601, "message": "\"Method not found\": providers/list", "data": {"method": "providers/list"}}));
    let id = client.request("initialize", json!({})).await;
    assert_eq!(client.response(id).await["error"]["code"], -32602);
}

#[tokio::test]
async fn new_session_answers_config_options_then_announces_commands() {
    let fixture = Fixture::new(|_| Vec::new(), json!({})).await;
    let mut client = fixture.client();
    let id = client.request("session/new", json!({"cwd": fixture.project, "mcpServers": []})).await;
    let response = client.next().await;
    assert_eq!(response["id"], id, "the response comes before the commands update");
    let session = response["result"]["sessionId"].as_str().unwrap().to_string();
    assert!(session.starts_with("ses_"));
    let options = response["result"]["configOptions"].as_array().unwrap();
    assert_eq!(options[0], json!({"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": "mock/m1", "options": [{"value": "mock/m1", "name": "Mock Provider/Mock"}]}));
    let mode = options.iter().find(|o| o["id"] == "mode").unwrap();
    assert_eq!(mode["currentValue"], "build");
    let modes: Vec<&str> = mode["options"].as_array().unwrap().iter().map(|o| o["value"].as_str().unwrap()).collect();
    assert!(modes.contains(&"build") && modes.contains(&"plan"), "{modes:?}");

    let commands = client.next().await;
    assert_eq!(commands["method"], "session/update");
    assert_eq!(commands["params"]["sessionId"], session);
    assert_eq!(commands["params"]["update"]["sessionUpdate"], "available_commands_update");
    let names: Vec<&str> = commands["params"]["update"]["availableCommands"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"init") && names.contains(&"review"), "{names:?}");

    let stored = fixture.engine.store.get_session(&session).unwrap().unwrap();
    assert_eq!((stored.agent.as_deref(), stored.model.as_ref().map(|m| m.id.as_str())), (Some("build"), Some("m1")));

    let listed = client.call("session/list", json!({"cwd": fixture.project})).await;
    assert!(listed["sessions"].as_array().unwrap().iter().any(|s| s["sessionId"] == session), "{listed}");
    assert!(listed["sessions"][0]["updatedAt"].as_str().unwrap().ends_with('Z'));

    let mode = client.call("session/set_config_option", json!({"sessionId": session, "configId": "mode", "value": "plan"})).await;
    assert_eq!(mode["configOptions"].as_array().unwrap().iter().find(|o| o["id"] == "mode").unwrap()["currentValue"], "plan");
    let id = client.request("session/set_mode", json!({"sessionId": session, "modeId": "nope"})).await;
    assert_eq!(client.response(id).await["error"], json!({"code": -32602, "message": "Invalid params: mode not found: nope", "data": {"mode": "nope"}}));
    let id = client.request("session/set_model", json!({"sessionId": session, "modelId": "mock/zzz"})).await;
    assert_eq!(client.response(id).await["error"]["message"], "Invalid params: model not found: mock/zzz");
    assert_eq!(client.call("session/set_model", json!({"sessionId": session, "modelId": "mock/m1"})).await, json!({}));
    let id = client.request("session/prompt", json!({"sessionId": "ses_missing", "prompt": text("hi")})).await;
    assert_eq!(client.response(id).await["error"]["message"], "Invalid params: session not found: ses_missing");
    assert_eq!(client.call("session/close", json!({"sessionId": session})).await, json!({}));
}

#[tokio::test]
async fn prompt_streams_text_and_a_tool_call() {
    let fixture = Fixture::new(|project| vec![tool_reply("read", &json!({"filePath": file(project, "notes.txt")})), text_reply("All done")], json!({})).await;
    let notes = file(&fixture.project, "notes.txt");
    std::fs::write(&notes, "remember the milk\n").unwrap();

    let mut client = fixture.client();
    let session = client.new_session(&fixture.project).await;
    let id = client.request("session/prompt", json!({"sessionId": session, "prompt": text("read my notes"), "messageId": "client-1"})).await;
    let response = client.response(id).await;
    assert_eq!(response["result"], json!({"stopReason": "end_turn", "usage": {"inputTokens": 20, "outputTokens": 2, "totalTokens": 22}, "userMessageId": "client-1", "_meta": {}}));

    let updates = client.updates(&session);
    let kinds: Vec<&str> = updates.iter().map(|u| u["sessionUpdate"].as_str().unwrap()).collect();
    let at = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap_or_else(|| panic!("no {kind} in {kinds:?}"));
    let call = &updates[at("tool_call")];
    assert_eq!(call["toolCallId"], "call_1");
    assert_eq!(call["kind"], "read");
    assert_eq!(call["status"], "pending");
    assert_eq!(call["locations"], json!([{"path": notes}]));
    assert_eq!(call["rawInput"]["filePath"], notes);
    let done = updates.iter().position(|u| u["sessionUpdate"] == "tool_call_update" && u["status"] == "completed").expect("tool completed");
    assert!(done > at("tool_call"));
    let completed = &updates[done];
    assert!(completed["content"][0]["content"]["text"].as_str().unwrap().contains("remember the milk"), "{completed}");
    assert!(completed["rawOutput"]["output"].is_string());

    let chunk = at("agent_message_chunk");
    assert!(chunk > done);
    let text: String = updates.iter().filter(|u| u["sessionUpdate"] == "agent_message_chunk").map(|u| u["content"]["text"].as_str().unwrap()).collect();
    assert_eq!(text, "All done");
    assert!(updates[chunk]["messageId"].as_str().unwrap().starts_with("msg_"));
    let usage = &updates[at("usage_update")];
    assert!(at("usage_update") > chunk);
    assert_eq!(usage, &json!({"sessionUpdate": "usage_update", "used": 20, "size": 1000, "cost": {"amount": 0, "currency": "USD"}}));
}

#[tokio::test]
async fn permission_requests_round_trip_to_the_engine() {
    let script = |project: &Path| {
        vec![
            tool_reply("write", &json!({"filePath": file(project, "new.txt"), "content": "hello\n"})),
            text_reply("written"),
            tool_reply("write", &json!({"filePath": file(project, "no.txt"), "content": "nope\n"})),
            text_reply("fine"),
        ]
    };
    let fixture = Fixture::new(script, json!({"permission": {"edit": "ask"}})).await;
    let (created, rejected) = (file(&fixture.project, "new.txt"), file(&fixture.project, "no.txt"));

    let mut client = fixture.client();
    let session = client.new_session(&fixture.project).await;
    let id = client.request("session/prompt", json!({"sessionId": session, "prompt": text("write it")})).await;
    let ask = client.until(|m| m["method"] == "session/request_permission").await;
    let params = &ask["params"];
    assert_eq!(params["sessionId"], session);
    assert_eq!(params["options"], json!([
        {"optionId": "once", "kind": "allow_once", "name": "Allow once"},
        {"optionId": "always", "kind": "allow_always", "name": "Always allow"},
        {"optionId": "reject", "kind": "reject_once", "name": "Reject"},
    ]));
    let call = &params["toolCall"];
    assert_eq!((call["toolCallId"].as_str(), call["kind"].as_str(), call["status"].as_str()), (Some("call_1"), Some("edit"), Some("pending")));
    let shown = call["rawInput"]["filepath"].as_str().unwrap().to_string();
    assert_eq!(call["title"], shown);
    assert_eq!(call["locations"], json!([{"path": shown}]));
    assert_eq!(call["content"], json!([{"type": "diff", "path": shown, "oldText": "", "newText": "hello\n"}]));
    assert!(!Path::new(&created).exists(), "nothing is written before the reply");
    client.respond(&ask["id"], json!({"outcome": {"outcome": "selected", "optionId": "once"}})).await;

    let write = client.until(|m| m["method"] == "fs/write_text_file").await;
    assert_eq!(write["params"], json!({"sessionId": session, "path": shown, "content": "hello\n"}));
    client.respond(&write["id"], Value::Null).await;
    let response = client.response(id).await;
    assert_eq!(response["result"]["stopReason"], "end_turn");
    assert_eq!(std::fs::read_to_string(&created).unwrap(), "hello\n");
    let updates = client.updates(&session);
    assert!(updates.iter().any(|u| u["sessionUpdate"] == "tool_call_update" && u["toolCallId"] == "call_1" && u["status"] == "completed"), "{updates:?}");

    client.seen.clear();
    let id = client.request("session/prompt", json!({"sessionId": session, "prompt": text("write another")})).await;
    let ask = client.until(|m| m["method"] == "session/request_permission").await;
    client.respond(&ask["id"], json!({"outcome": {"outcome": "selected", "optionId": "reject"}})).await;
    let response = client.response(id).await;
    assert!(response.get("result").is_some(), "{response}");
    assert!(!Path::new(&rejected).exists());
    let updates = client.updates(&session);
    let failed = updates.iter().find(|u| u["sessionUpdate"] == "tool_call_update" && u["status"] == "failed").unwrap_or_else(|| panic!("{updates:?}"));
    assert_eq!(failed["kind"], "edit");
    assert!(failed["content"][0]["content"]["text"].as_str().unwrap().contains("rejected"), "{failed}");
    assert!(!client.seen.iter().any(|m| m["method"] == "fs/write_text_file"));
}

#[tokio::test]
async fn cancel_ends_the_running_turn_as_cancelled() {
    let fixture = Fixture::new(|_| vec![Reply::Stall("Hel".into())], json!({})).await;
    let mut client = fixture.client();
    let session = client.new_session(&fixture.project).await;
    let id = client.request("session/prompt", json!({"sessionId": session, "prompt": text("talk")})).await;
    let chunk = client.until(|m| m["params"]["update"]["sessionUpdate"] == "agent_message_chunk").await;
    assert_eq!(chunk["params"]["update"]["content"], json!({"type": "text", "text": "Hel"}));
    client.notify("session/cancel", json!({"sessionId": session})).await;
    let response = client.response(id).await;
    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");
    assert_eq!(response["result"]["_meta"], json!({}));
}

#[tokio::test]
async fn load_replays_history_and_fork_and_resume_restore_it() {
    let fixture = Fixture::new(|_| vec![text_reply("Hi there")], json!({})).await;
    let mut first = fixture.client();
    let session = first.new_session(&fixture.project).await;
    let reply = first.call("session/prompt", json!({"sessionId": session, "prompt": text("hello agent")})).await;
    assert_eq!(reply["stopReason"], "end_turn");
    fixture.engine.settle().await;

    let mut second = fixture.client();
    second.call("initialize", json!({"protocolVersion": 1})).await;
    let loaded = second.call("session/load", json!({"cwd": fixture.project, "sessionId": session, "mcpServers": []})).await;
    assert_eq!(loaded["configOptions"][0]["currentValue"], "mock/m1");
    let updates = second.updates(&session);
    let user: String = updates.iter().filter(|u| u["sessionUpdate"] == "user_message_chunk").map(|u| u["content"]["text"].as_str().unwrap()).collect();
    let agent: String = updates.iter().filter(|u| u["sessionUpdate"] == "agent_message_chunk").map(|u| u["content"]["text"].as_str().unwrap()).collect();
    assert_eq!((user.as_str(), agent.as_str()), ("hello agent", "Hi there"));
    assert!(updates.iter().any(|u| u["sessionUpdate"] == "available_commands_update"));

    let resumed = second.call("session/resume", json!({"cwd": fixture.project, "sessionId": session})).await;
    assert_eq!(resumed["configOptions"][0]["category"], "model");
    second.seen.clear();
    let forked = second.call("session/fork", json!({"cwd": fixture.project, "sessionId": session})).await;
    let fork = forked["sessionId"].as_str().unwrap().to_string();
    assert_ne!(fork, session);
    let replayed: String = second.updates(&fork).iter().filter(|u| u["sessionUpdate"] == "agent_message_chunk").map(|u| u["content"]["text"].as_str().unwrap()).collect();
    assert_eq!(replayed, "Hi there");
    let listed = second.call("session/list", json!({"cwd": fixture.project})).await;
    let ids: Vec<&str> = listed["sessions"].as_array().unwrap().iter().map(|s| s["sessionId"].as_str().unwrap()).collect();
    assert!(ids.contains(&session.as_str()) && ids.contains(&fork.as_str()), "{ids:?}");
    let titled = listed["sessions"].as_array().unwrap().iter().find(|s| s["sessionId"] == session).unwrap();
    assert!(titled["title"].is_string());
}
