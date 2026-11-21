// End-to-end: a scripted fake provider drives the loop; everything lives in temp dirs and an in-memory store.
use opencode_core::paths::Paths;
use opencode_llm::catalog::Source;
use opencode_session::{
    CommandInput, CreateSession, Delivery, Engine, Error, Host, PermissionAnswer, PermissionAsk, PromptInput, QuestionRequest, Reply, ShellInput,
    Status, Update,
};
use opencode_storage::{AssistantError, FilePart, Message, Part, PartBody, PermissionAction, PermissionRule, Session, Store, ToolState};
use opencode_tools::PermissionRequest;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

enum Response {
    Sse(String),
    Status(u16, String),
}

#[derive(Default)]
struct Script {
    responses: Mutex<VecDeque<Response>>,
    // Bodies of every non-title model request, in arrival order.
    seen: Mutex<Vec<Value>>,
    // Request heads (lower-cased) of the same requests.
    heads: Mutex<Vec<String>>,
    titles: Mutex<Vec<Value>>,
    title_heads: Mutex<Vec<String>>,
    title: Mutex<String>,
    remote: Mutex<String>,
}

fn usage(prompt: u64, completion: u64) -> Value {
    json!({"prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": prompt + completion})
}

fn sse(chunks: &[Value]) -> Response {
    let mut out: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    out.push_str("data: [DONE]\n\n");
    Response::Sse(out)
}

fn text_with(content: &str, usage: Value) -> Response {
    sse(&[
        json!({"choices": [{"index": 0, "delta": {"content": content}}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": usage}),
    ])
}

fn text(content: &str) -> Response {
    text_with(content, usage(20, 2))
}

fn calls(items: &[(&str, &str, Value)]) -> Response {
    let list: Vec<Value> = items
        .iter()
        .enumerate()
        .map(|(index, (id, name, args))| {
            let arguments = if args.is_string() { args.as_str().unwrap().to_string() } else { args.to_string() };
            json!({"index": index, "id": id, "type": "function", "function": {"name": name, "arguments": arguments}})
        })
        .collect();
    sse(&[
        json!({"choices": [{"index": 0, "delta": {"tool_calls": list}}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}], "usage": usage(10, 5)}),
    ])
}

fn system_of(body: &Value) -> &str {
    body["messages"][0]["content"].as_str().unwrap_or("")
}

async fn serve(script: Arc<Script>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let script = script.clone();
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let (head, body) = loop {
                    let mut chunk = [0u8; 8192];
                    let n = socket.read(&mut chunk).await.unwrap();
                    buffer.extend_from_slice(&chunk[..n]);
                    let Some(split) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                    let head = String::from_utf8_lossy(&buffer[..split]).to_lowercase();
                    let length: usize = head.lines().find_map(|l| l.strip_prefix("content-length:")).map_or(0, |v| v.trim().parse().unwrap());
                    if buffer.len() >= split + 4 + length {
                        break (head, buffer[split + 4..split + 4 + length].to_vec());
                    }
                };
                let response = if head.starts_with("get ") {
                    Response::Status(200, script.remote.lock().unwrap().clone())
                } else {
                    let body: Value = serde_json::from_slice(&body).unwrap();
                    if system_of(&body).contains("You are a title generator") {
                        script.titles.lock().unwrap().push(body);
                        script.title_heads.lock().unwrap().push(head.clone());
                        text(&script.title.lock().unwrap())
                    } else {
                        script.seen.lock().unwrap().push(body);
                        script.heads.lock().unwrap().push(head.clone());
                        let next = script.responses.lock().unwrap().pop_front();
                        match next {
                            Some(response) => response,
                            // Out of script: keep the request open.
                            None => return std::future::pending().await,
                        }
                    }
                };
                let (status, kind, payload) = match response {
                    Response::Sse(payload) => (200, "text/event-stream", payload),
                    Response::Status(status, payload) => (status, "application/json", payload),
                };
                let reply = format!("HTTP/1.1 {status} X\r\ncontent-type: {kind}\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{payload}", payload.len());
                socket.write_all(reply.as_bytes()).await.unwrap();
                socket.shutdown().await.ok();
            });
        }
    });
    port
}

#[derive(Default)]
struct TestHost {
    text: Mutex<String>,
    asked: Mutex<Vec<String>>,
    events: Mutex<Vec<String>>,
    tools: Mutex<Vec<Part>>,
    answers: Mutex<Option<Vec<Vec<String>>>>,
    questions: Mutex<Vec<QuestionRequest>>,
    // Both questions of one step must be pending before either is answered.
    barrier: Option<tokio::sync::Barrier>,
    hang: bool,
    asked_question: tokio::sync::Notify,
    reject: bool,
    // Answer every permission request with "always".
    always: bool,
    // Reject with this feedback instead.
    feedback: Option<String>,
    asks: Mutex<Vec<PermissionAsk>>,
    // (session, message, part, delta)
    deltas: Mutex<Vec<(String, String, String, String)>>,
    errors: Mutex<Vec<(String, Value)>>,
    // (kind, properties) of every `Update::Event`.
    bus: Mutex<Vec<(String, Value)>>,
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

#[async_trait::async_trait]
impl Host for TestHost {
    fn update(&self, update: Update) {
        let tag = match update {
            Update::Text(delta) => {
                self.text.lock().unwrap().push_str(&delta);
                return;
            }
            Update::Tool(part) => {
                self.tools.lock().unwrap().push(*part);
                return;
            }
            Update::Title { title, .. } => format!("title:{title}"),
            Update::Compacted { .. } => "compacted".to_string(),
            Update::Todos { todos, .. } => format!("todos:{}", todos.len()),
            Update::Child { update, .. } => match *update {
                Update::Text(delta) => format!("child-text:{delta}"),
                other @ (Update::Delta { .. } | Update::SessionError { .. }) => return self.update(other),
                _ => return,
            },
            Update::Delta { session_id, message_id, part_id, field, delta } => {
                assert_eq!(field, "text");
                self.deltas.lock().unwrap().push((session_id, message_id, part_id, delta));
                return;
            }
            Update::SessionError { session_id, error } => {
                self.errors.lock().unwrap().push((session_id, serde_json::to_value(error).unwrap()));
                return;
            }
            Update::Error { message, .. } => format!("error:{message}"),
            Update::Event { kind, properties, .. } => {
                self.bus.lock().unwrap().push((kind, properties));
                return;
            }
            _ => return,
        };
        self.events.lock().unwrap().push(tag);
    }

    async fn ask(&self, ask: &PermissionAsk) -> PermissionAnswer {
        self.asks.lock().unwrap().push(ask.clone());
        match &self.feedback {
            Some(feedback) => PermissionAnswer::Corrected(feedback.clone()),
            None => self.permission(&ask.request).await.into(),
        }
    }

    async fn permission(&self, request: &PermissionRequest) -> Reply {
        self.asked.lock().unwrap().push(request.permission.clone());
        if self.reject {
            Reply::Reject
        } else if self.always {
            Reply::Always
        } else {
            Reply::Once
        }
    }

    async fn question(&self, request: &QuestionRequest) -> Option<Vec<Vec<String>>> {
        self.questions.lock().unwrap().push(request.clone());
        self.asked_question.notify_one();
        if self.hang {
            return std::future::pending().await;
        }
        if let Some(barrier) = &self.barrier {
            barrier.wait().await;
        }
        self.answers.lock().unwrap().clone()
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    project: PathBuf,
    paths: Paths,
    script: Arc<Script>,
    engine: Arc<Engine>,
    port: u16,
}

impl Fixture {
    // `config` is merged over a config that points provider `mock` at the fake server.
    async fn new(config: Value, responses: Vec<Response>) -> Self {
        Self::build(config, responses, |_| {}).await
    }

    async fn build(config: Value, responses: Vec<Response>, setup: impl FnOnce(&PathBuf)) -> Self {
        let root = tempfile::tempdir().unwrap();
        // Plain long-form path, as a model would write it (no verbatim prefix).
        let canonical = root.path().canonicalize().unwrap();
        let root_path = PathBuf::from(canonical.to_str().unwrap().trim_start_matches(r"\\?\"));
        let project = root_path.join("project");
        // The marker makes the project its own worktree, so nothing above the temp dir is searched.
        std::fs::create_dir_all(project.join(".git")).unwrap();
        let paths = Paths {
            home: root_path.join("home"),
            data: root_path.join("data"),
            cache: root_path.join("cache"),
            config: root_path.join("config"),
            state: root_path.join("state"),
            tmp: root_path.join("tmp"),
        };
        std::fs::create_dir_all(&paths.data).unwrap();
        setup(&project);

        let script = Arc::new(Script { responses: Mutex::new(responses.into()), title: Mutex::new("A Title".into()), ..Default::default() });
        let port = serve(script.clone()).await;
        let mut merged = json!({
            "model": "mock/m1",
            "provider": {"mock": {"options": {"baseURL": format!("http://127.0.0.1:{port}/v1"), "apiKey": "k"}, "models": {"m1": {"name": "Mock"}}}}
        });
        let text = config.to_string().replace("{port}", &port.to_string());
        opencode_core::config::merge_deep(&mut merged, serde_json::from_str(&text).unwrap());
        std::fs::write(project.join("opencode.json"), merged.to_string()).unwrap();
        let engine = Arc::new(Engine::with_store(paths.clone(), Store::open_in_memory().unwrap()));
        engine.attach();
        // The catalog is whatever a test writes to the temp cache; nothing is ever downloaded.
        let file = paths.cache.join("models.json");
        engine.set_catalog_source(Source { url: "http://127.0.0.1:9".into(), file, locks: paths.state.join("locks"), override_file: None, disabled: true });
        Self { _root: root, project, paths, script, engine, port }
    }

    fn input(&self, text: &str) -> PromptInput {
        PromptInput { directory: self.project.clone(), text: text.into(), ..Default::default() }
    }

    async fn prompt(&self, text: &str, host: &Arc<TestHost>) -> Session {
        self.engine.prompt(self.input(text), host.clone(), CancellationToken::new()).await.unwrap()
    }

    async fn follow_up(&self, session: &Session, text: &str, host: &Arc<TestHost>) -> Session {
        let input = PromptInput { session_id: Some(session.id.clone()), ..self.input(text) };
        self.engine.prompt(input, host.clone(), CancellationToken::new()).await.unwrap()
    }

    fn seen(&self) -> Vec<Value> {
        self.script.seen.lock().unwrap().clone()
    }

    fn rows(&self, session: &Session) -> Vec<(Message, Vec<Part>)> {
        self.engine.store.list_messages(&session.id).unwrap()
    }

    fn tool(&self, session: &Session, index: usize) -> opencode_storage::ToolPart {
        let tools: Vec<_> = self
            .rows(session)
            .into_iter()
            .flat_map(|(_, parts)| parts)
            .filter_map(|p| match p.body {
                PartBody::Tool(tool) => Some(tool),
                _ => None,
            })
            .collect();
        tools[index].clone()
    }
}

fn completed(tool: &opencode_storage::ToolPart) -> &opencode_storage::ToolStateCompleted {
    match &tool.state {
        ToolState::Completed(state) => state,
        other => panic!("expected completed, got {other:?}"),
    }
}

fn failed(tool: &opencode_storage::ToolPart) -> &opencode_storage::ToolStateError {
    match &tool.state {
        ToolState::Error(state) => state,
        other => panic!("expected error, got {other:?}"),
    }
}

fn kinds(rows: &[(Message, Vec<Part>)]) -> Vec<Vec<&'static str>> {
    rows.iter().map(|(_, parts)| parts.iter().map(|p| p.kind()).collect()).collect()
}

fn texts(parts: &[Part]) -> Vec<String> {
    parts
        .iter()
        .filter_map(|p| match &p.body {
            PartBody::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect()
}

fn tool_names(body: &Value) -> Vec<String> {
    body["tools"].as_array().into_iter().flatten().map(|t| t["function"]["name"].as_str().unwrap().to_string()).collect()
}

fn last_user(body: &Value) -> String {
    let messages = body["messages"].as_array().unwrap();
    messages.iter().rev().find(|m| m["role"] == "user").unwrap()["content"].as_str().unwrap().to_string()
}

fn tool_def<'a>(body: &'a Value, name: &str) -> &'a Value {
    body["tools"].as_array().unwrap().iter().find(|t| t["function"]["name"] == name).unwrap_or_else(|| panic!("no tool {name}"))
}

// The default "agent" profile: exposed names and snake_case arguments for the model, internal names everywhere else.
#[tokio::test]
async fn runs_a_tool_call_then_answers() {
    let fx = Fixture::new(json!({"permission": {"edit": "ask"}}), vec![]).await;
    let target = fx.project.join("hello.txt");
    let arguments = json!({"file_path": target.to_string_lossy(), "content": "hi\n"});
    fx.script.responses.lock().unwrap().extend([calls(&[("call_1", "Write", arguments.clone())]), text("Done.")]);
    let host = Arc::new(TestHost::default());
    let session = fx.prompt("create hello.txt", &host).await;

    assert_eq!(std::fs::read_to_string(&target).unwrap(), "hi\n");
    assert_eq!(*host.text.lock().unwrap(), "Done.");
    // Permission keys stay internal.
    assert_eq!(*host.asked.lock().unwrap(), ["edit"]);

    let requests = fx.seen();
    assert_eq!(requests.len(), 2);
    let tools = tool_names(&requests[0]);
    // Bash where the session shell is bash (Git Bash on Windows), PowerShell or Cmd otherwise.
    let shell = opencode_tools::shell::tool_name(&opencode_tools::shell::acceptable(None));
    let mut exposed = vec![shell, "Edit", "Glob", "Grep", "Read", "Task", "TodoWrite", "WebFetch", "Write", "question", "skill"];
    exposed.sort();
    assert_eq!(tools, exposed, "{tools:?}");
    let system = system_of(&requests[0]);
    assert!(system.starts_with("You are a coding agent working inside the user's project through tools."), "{system}");
    assert!(system.contains("\n<env>\n  Working directory: "), "{system}");
    let write = tool_def(&requests[0], "Write");
    assert!(write["function"]["description"].as_str().unwrap().starts_with("Creates a file, or replaces an existing file entirely."));
    assert_eq!(write["function"]["parameters"]["required"], json!(["file_path", "content"]));
    let bash = tool_def(&requests[0], shell)["function"]["description"].as_str().unwrap();
    assert!(bash.starts_with("Runs a command in a ") && !bash.contains("{shell}") && !bash.contains("<!--"), "{bash}");
    let task = tool_def(&requests[0], "Task")["function"]["description"].as_str().unwrap();
    assert!(task.contains("\n\nAvailable agents:\n- explore: "), "{task}");
    // History replays with the exposed name and arguments.
    let replayed = requests[1]["messages"].as_array().unwrap();
    let roles: Vec<&str> = replayed.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["system", "user", "assistant", "tool"]);
    assert_eq!(replayed[3]["tool_call_id"], "call_1");
    let call = &replayed[2]["tool_calls"][0]["function"];
    assert_eq!(call["name"], "Write");
    assert_eq!(serde_json::from_str::<Value>(call["arguments"].as_str().unwrap()).unwrap(), arguments);

    // The stored part keeps the internal tool name and arguments.
    let stored = fx.tool(&session, 0);
    assert_eq!(stored.tool, "write");
    assert_eq!(*stored.state.input(), *json!({"filePath": target.to_string_lossy(), "content": "hi\n"}).as_object().unwrap());

    let rows = fx.rows(&session);
    assert_eq!(kinds(&rows), [vec!["text"], vec!["step-start", "tool", "step-finish", "patch"], vec!["step-start", "text", "step-finish"]]);
    assert!(matches!(&rows[0].0, Message::User(_)));
    assert!(matches!(fx.tool(&session, 0).state, ToolState::Completed(_)));
    let stored = fx.engine.store.get_session(&session.id).unwrap().unwrap();
    assert_eq!(stored.tokens.unwrap().output, 7.0);
    assert_eq!(stored.agent.as_deref(), Some("build"));
    assert_eq!(stored.model.unwrap().id, "m1");
    assert_eq!(fx.engine.status(&session.id), Status::Idle);
}

// `tool_profile: "opencode"` restores the original's names, descriptions and provider-family prompt.
#[tokio::test]
async fn original_profile_keeps_the_original_tools() {
    let fx = Fixture::new(json!({"tool_profile": "opencode", "permission": {"edit": "ask"}}), vec![]).await;
    let target = fx.project.join("hello.txt");
    let arguments = json!({"filePath": target.to_string_lossy(), "content": "hi\n"});
    fx.script.responses.lock().unwrap().extend([calls(&[("call_1", "write", arguments.clone())]), text("Done.")]);
    let host = Arc::new(TestHost::default());
    let session = fx.prompt("create hello.txt", &host).await;

    assert_eq!(std::fs::read_to_string(&target).unwrap(), "hi\n");
    assert_eq!(*host.asked.lock().unwrap(), ["edit"]);
    let requests = fx.seen();
    let tools = tool_names(&requests[0]);
    assert!(tools.contains(&"write".into()) && tools.contains(&"bash".into()) && !tools.contains(&"apply_patch".into()), "{tools:?}");
    assert!(!tools.contains(&"invalid".into()) && tools.is_sorted(), "{tools:?}");
    assert!(["task", "skill", "question", "todowrite"].iter().all(|t| tools.contains(&t.to_string())), "{tools:?}");
    let original = include_str!("../../../../opencode-ts/packages/opencode/src/tool/write.txt");
    assert_eq!(tool_def(&requests[0], "write")["function"]["description"], original);
    assert!(!system_of(&requests[0]).starts_with("You are a coding agent working inside the user's project"));
    let default = include_str!("../../../../opencode-ts/packages/opencode/src/session/prompt/default.txt");
    assert!(system_of(&requests[0]).starts_with(default.trim_end()), "{}", system_of(&requests[0]));
    let task = tool_def(&requests[0], "task")["function"]["description"].as_str().unwrap();
    assert!(task.contains("\nAvailable agent types and the tools they have access to:\n- explore: "), "{task}");
    let call = &requests[1]["messages"][2]["tool_calls"][0]["function"];
    assert_eq!((call["name"].as_str(), serde_json::from_str::<Value>(call["arguments"].as_str().unwrap()).unwrap()), (Some("write"), arguments));
    assert_eq!(fx.tool(&session, 0).tool, "write");
}

// "agent" profile: an existing file must be Read in the same session before Edit or Write; Edit errors say what to do.
#[tokio::test]
async fn edit_and_write_need_a_read_in_the_same_session() {
    let fx = Fixture::build(json!({}), vec![], |project| std::fs::write(project.join("f.txt"), "a\nb\na\n").unwrap()).await;
    let file = fx.project.join("f.txt").to_string_lossy().into_owned();
    let edit = |old: &str| json!({"file_path": file, "old_string": old, "new_string": "z"});
    fx.script.responses.lock().unwrap().extend([
        calls(&[("c1", "Edit", edit("b")), ("c2", "Write", json!({"file_path": file, "content": "x"}))]),
        calls(&[("c3", "Read", json!({"file_path": file, "limit": 2}))]),
        calls(&[("c4", "Edit", edit("a")), ("c5", "Edit", edit("nope"))]),
        calls(&[("c6", "Edit", edit("b"))]),
        text("done"),
    ]);
    let host = Arc::new(TestHost::default());
    let session = fx.prompt("change it", &host).await;

    let unread = failed(&fx.tool(&session, 0)).error.clone();
    assert!(unread.ends_with("f.txt has not been Read in this session — Read it first, then edit it."), "{unread}");
    assert!(failed(&fx.tool(&session, 1)).error.ends_with("then overwrite it."));
    assert_eq!(completed(&fx.tool(&session, 2)).output, "1\ta\n2\tb\n\n(Showing lines 1-2 of 3. Use offset=3 to continue.)");
    let many = failed(&fx.tool(&session, 3)).error.clone();
    assert!(many.starts_with("found 2 matches of old_string in ") && many.contains("set replace_all to true"), "{many}");
    let missing = failed(&fx.tool(&session, 4)).error.clone();
    assert!(missing.starts_with("old_string not found in ") && missing.contains("re-Read the file"), "{missing}");
    assert!(matches!(fx.tool(&session, 5).state, ToolState::Completed(_)));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "a\nz\na\n");

    // Another session has not read the file.
    fx.script.responses.lock().unwrap().extend([calls(&[("c7", "Edit", edit("z"))]), text("no")]);
    let other = fx.prompt("again", &host).await;
    assert!(failed(&fx.tool(&other, 0)).error.contains("has not been Read in this session"));
}

// An agent with its own prompt keeps it in both profiles.
#[tokio::test]
async fn an_agent_prompt_wins_in_both_profiles() {
    for profile in ["agent", "opencode"] {
        let config = json!({"tool_profile": profile, "agent": {"build": {"prompt": "My own prompt."}}});
        let fx = Fixture::new(config, vec![text("ok")]).await;
        let host = Arc::new(TestHost::default());
        fx.prompt("hi", &host).await;
        let system = system_of(&fx.seen()[0]).to_string();
        assert!(system.starts_with("My own prompt.\n"), "{profile}: {system}");
        assert!(!system.contains("You are a coding agent working inside"), "{profile}");
    }
}

#[tokio::test]
async fn cost_follows_model_pricing() {
    let pricing = json!({"provider": {"mock": {"models": {"m1": {"cost": {"input": 3, "output": 15, "cache_read": 0.3}}}}}});
    let cached = json!({"prompt_tokens": 1000, "completion_tokens": 100, "total_tokens": 1100, "prompt_tokens_details": {"cached_tokens": 200}});
    let fx = Fixture::new(pricing, vec![text_with("priced", cached)]).await;
    let host = Arc::new(TestHost::default());
    let session = fx.prompt("hi", &host).await;

    // (800 * 3 + 100 * 15 + 200 * 0.3) / 1e6
    let rows = fx.rows(&session);
    let assistant = rows[1].0.as_assistant().unwrap();
    assert_eq!(assistant.cost, 0.00396);
    assert_eq!((assistant.tokens.input, assistant.tokens.cache.read, assistant.tokens.total), (800.0, 200.0, Some(1100.0)));
    let PartBody::StepFinish(step) = &rows[1].1.last().unwrap().body else { panic!("expected step-finish") };
    assert_eq!(step.cost, 0.00396);
    assert_eq!(fx.engine.store.get_session(&session.id).unwrap().unwrap().cost, Some(0.00396));
}

#[tokio::test]
async fn titles_a_new_session_in_the_background() {
    let fx = Fixture::new(json!({}), vec![text("first"), text("second")]).await;
    *fx.script.title.lock().unwrap() = "<think>hmm\nplan</think>\n\n   Fixing the Parser  \nsecond line".into();
    let host = Arc::new(TestHost::default());
    let session = fx.prompt("please fix the parser", &host).await;
    fx.engine.settle().await;

    assert_eq!(fx.engine.store.get_session(&session.id).unwrap().unwrap().title, "Fixing the Parser");
    assert!(host.events.lock().unwrap().contains(&"title:Fixing the Parser".to_string()));
    let request = fx.script.titles.lock().unwrap()[0].clone();
    assert!(tool_names(&request).is_empty());
    let messages = request["messages"].as_array().unwrap();
    assert_eq!(messages[1]["content"], "Generate a title for this conversation:\n");
    assert_eq!(messages[2]["content"], "please fix the parser");

    // Only the first real user message names the session.
    fx.follow_up(&session, "and now the lexer", &host).await;
    fx.engine.settle().await;
    assert_eq!(fx.script.titles.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn compacts_when_the_context_fills_up() {
    let limits = json!({"provider": {"mock": {"models": {"m1": {"limit": {"context": 1000, "output": 100}}}}}});
    let responses = vec![text_with("long answer", usage(900, 50)), text("## Objective\n- keep going"), text("continued")];
    let fx = Fixture::new(limits, responses).await;
    let host = Arc::new(TestHost::default());
    let session = fx.prompt("do a lot", &host).await;

    let rows = fx.rows(&session);
    let roles: Vec<&str> = rows.iter().map(|(m, _)| m.role()).collect();
    assert_eq!(roles, ["user", "assistant", "user", "assistant", "user", "assistant"]);
    assert_eq!(kinds(&rows)[2], ["compaction"]);
    let summary = rows[3].0.as_assistant().unwrap();
    assert_eq!((summary.summary, summary.agent.as_str(), summary.mode.as_str()), (Some(true), "compaction", "compaction"));
    let PartBody::Text(follow) = &rows[4].1[0].body else { panic!("expected text") };
    assert_eq!(follow.text, "Continue if you have next steps, or stop and ask for clarification if you are unsure how to proceed.");
    assert_eq!(follow.synthetic, Some(true));
    assert_eq!(follow.metadata.as_ref().unwrap()["compaction_continue"], true);
    assert!(host.events.lock().unwrap().contains(&"compacted".to_string()));

    let requests = fx.seen();
    assert_eq!(requests.len(), 3);
    assert!(system_of(&requests[1]).starts_with("You are a context summarization agent."));
    assert!(tool_names(&requests[1]).is_empty());
    let prompt = last_user(&requests[1]);
    // A 1000-token window leaves no room beside the summary prompt, so the transcript keeps only the user's text.
    assert!(prompt.starts_with("[User]: do a lot\n\nYou MUST summarize the conversation above"), "{prompt}");
    assert!(prompt.contains("## Objective\n") && prompt.contains("## Next Move\n") && prompt.contains("## Important Context\n"));
    assert!(prompt.ends_with("Do not include a preamble, explanation, or other commentary."));
    // After compaction the model sees the summary instead of the old turns.
    let after: Vec<(&str, &str)> = requests[2]["messages"].as_array().unwrap()[1..].iter().map(|m| (m["role"].as_str().unwrap(), m["content"].as_str().unwrap())).collect();
    assert_eq!(after, [("user", "What did we do so far?"), ("assistant", "## Objective\n- keep going"), ("user", follow.text.as_str())]);

    // A manual summarize folds the previous summary in and adds no follow-up message.
    fx.script.responses.lock().unwrap().push_back(text("## Objective\n- newer"));
    fx.engine.summarize(&session.id, false, host.clone(), CancellationToken::new()).await.unwrap();
    let prompt = last_user(&fx.seen()[3]);
    assert!(prompt.contains("<summary>\n## Objective\n- keep going\n</summary>\n</conversation-checkpoint>\n\n[User]: Continue if you have next steps"), "{prompt}");
    assert!(prompt.contains("\n\nUpdate the existing checkpoint in the conversation above into one consolidated summary."), "{prompt}");
    let rows = fx.rows(&session);
    assert_eq!(rows.len(), 8);
    let PartBody::Compaction(manual) = &rows[6].1[0].body else { panic!("expected compaction") };
    assert_eq!((manual.auto, manual.overflow), (false, None));
    assert_eq!(rows[7].0.as_assistant().unwrap().summary, Some(true));
}

#[tokio::test]
async fn provider_overflow_error_triggers_compaction() {
    let overflow = json!({"error": {"message": "This model's maximum context length is 10 tokens", "code": "context_length_exceeded"}}).to_string();
    let responses = vec![Response::Status(400, overflow.clone()), text("summary"), text("recovered")];
    let fx = Fixture::new(json!({}), responses).await;
    let host = Arc::new(TestHost::default());
    let session = fx.prompt("too big", &host).await;
    let rows = fx.rows(&session);
    let PartBody::Compaction(marker) = &rows[2].1[0].body else { panic!("expected compaction, got {:?}", kinds(&rows)) };
    assert_eq!((marker.auto, marker.overflow), (true, Some(true)));
    assert!(rows[1].0.as_assistant().unwrap().error.is_none());
    assert!(texts(&rows[4].1)[0].starts_with("The previous request exceeded the provider's size limit due to large media attachments."));
    assert_eq!(*host.text.lock().unwrap(), "summaryrecovered");

    // With auto compaction off the overflow is a plain failure recorded on the message.
    let fx = Fixture::new(json!({"compaction": {"auto": false}}), vec![Response::Status(400, overflow)]).await;
    let session = fx.engine.prompt(PromptInput { no_reply: true, ..fx.input("setup") }, host.clone(), CancellationToken::new()).await.unwrap();
    let input = PromptInput { session_id: Some(session.id.clone()), ..fx.input("too big") };
    let error = fx.engine.prompt(input, host.clone(), CancellationToken::new()).await.unwrap_err();
    assert!(matches!(error, Error::Llm(_)), "{error:?}");
    let rows = fx.rows(&session)[1..].to_vec();
    let assistant = rows[1].0.as_assistant().unwrap();
    assert!(matches!(assistant.error, Some(AssistantError::ContextOverflowError { .. })), "{:?}", assistant.error);
    assert_eq!(assistant.finish.as_deref(), Some("error"));
}

#[tokio::test]
async fn system_prompt_carries_instructions_skills_and_reminders() {
    let config = json!({
        "instructions": ["http://127.0.0.1:{port}/rules.md"],
        "agent": {"plan": {"steps": 2}}
    });
    let fx = Fixture::build(config, vec![calls(&[("c1", "skill", json!({"name": "demo"}))]), text("plan"), text("built")], |project| {
        std::fs::write(project.join("AGENTS.md"), "Project rules").unwrap();
        let skill = project.join(".opencode").join("skill").join("demo");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "---\nname: demo\ndescription: Demo skill\n---\n\nDo the demo.\n").unwrap();
        std::fs::write(skill.join("helper.sh"), "echo").unwrap();
    })
    .await;
    *fx.script.remote.lock().unwrap() = "Remote rules".into();
    let host = Arc::new(TestHost::default());
    let input = PromptInput { system: Some("Extra system".into()), agent: Some("plan".into()), ..fx.input("go") };
    let session = fx.engine.prompt(input, host.clone(), CancellationToken::new()).await.unwrap();

    let requests = fx.seen();
    let system = system_of(&requests[0]);
    let agents = fx.project.join("AGENTS.md");
    let skill_dir = fx.project.join(".opencode").join("skill").join("demo");
    assert!(system.contains("\n<env>\n  Working directory: "), "{system}");
    assert!(system.contains(&format!("</env>\nInstructions from: {}\nProject rules\nInstructions from: http://127.0.0.1:", agents.display())), "{system}");
    assert!(system.contains("/rules.md\nRemote rules\nSkills provide specialized instructions and workflows for specific tasks.\nUse the skill tool to load a skill when a task matches its description.\n<available_skills>"));
    assert!(system.contains(&format!("    <name>demo</name>\n    <description>Demo skill</description>\n    <location>{}</location>", skill_dir.join("SKILL.md").display())));
    assert!(system.ends_with("</available_skills>\nExtra system"), "{system}");
    assert_eq!(requests[0]["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "system").count(), 1);

    // The plan agent gets its reminder on the last user message, in memory only.
    let reminder = include_str!("../../../../opencode-ts/packages/opencode/src/session/prompt/plan.txt");
    assert_eq!(last_user(&requests[0]), format!("go\n{reminder}"));
    assert_eq!(kinds(&fx.rows(&session))[0], ["text"]);

    let loaded = fx.tool(&session, 0);
    let state = completed(&loaded);
    assert_eq!(state.title, "Loaded skill: demo");
    let expected = format!(
        "<skill_content name=\"demo\">\n# Skill: demo\n\nDo the demo.\n\nBase directory for this skill: {dir}\nRelative paths in this skill (e.g., scripts/, reference/) are relative to this base directory.\nNote: file list is sampled.\n\n<skill_files>\n<file>{file}</file>\n</skill_files>\n</skill_content>",
        dir = skill_dir.display(),
        file = skill_dir.join("helper.sh").display()
    );
    assert_eq!(state.output, expected);

    // Second step of an agent limited to two steps: tools stay, the model is told to stop.
    let last = requests[1]["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["role"], "assistant");
    assert!(last["content"].as_str().unwrap().starts_with("CRITICAL - MAXIMUM STEPS REACHED\n\nThe maximum number of steps allowed"));
    assert!(!tool_names(&requests[1]).is_empty());

    // Switching back to build after a plan turn adds the build-switch reminder.
    fx.follow_up(&session, "build it", &host).await;
    let switch = include_str!("../../../../opencode-ts/packages/opencode/src/session/prompt/build-switch.txt");
    assert_eq!(last_user(&fx.seen()[2]), format!("build it\n{switch}"));
}

#[tokio::test]
async fn read_attaches_nearby_instruction_files_once() {
    let fx = Fixture::build(json!({}), vec![], |project| {
        std::fs::create_dir_all(project.join("pkg")).unwrap();
        std::fs::write(project.join("pkg").join("AGENTS.md"), "Package rules").unwrap();
        std::fs::write(project.join("pkg").join("a.txt"), "alpha").unwrap();
    })
    .await;
    let file = fx.project.join("pkg").join("a.txt").to_string_lossy().into_owned();
    let read = || calls(&[("c1", "read", json!({"filePath": file}))]);
    fx.script.responses.lock().unwrap().extend([read(), read(), text("ok")]);
    let host = Arc::new(TestHost::default());
    let session = fx.prompt("read it", &host).await;

    let nested = fx.project.join("pkg").join("AGENTS.md");
    let first = fx.tool(&session, 0);
    let reminder = format!("\n\n<system-reminder>\nInstructions from: {}\nPackage rules\n</system-reminder>", nested.display());
    assert!(completed(&first).output.ends_with(&reminder), "{}", completed(&first).output);
    assert_eq!(completed(&first).metadata["loaded"], json!([nested.to_string_lossy()]));
    let second = fx.tool(&session, 1);
    assert!(!completed(&second).output.contains("<system-reminder>"));
}

#[tokio::test]
async fn task_runs_a_subagent_in_a_child_session() {
    let task = json!({"description": "Find things", "prompt": "look around", "subagent_type": "general"});
    let nested = json!({"description": "Deeper", "prompt": "again", "subagent_type": "general"});
    let responses = vec![calls(&[("c1", "Task", task)]), calls(&[("c2", "Task", nested)]), text("child says hi"), text("parent done")];
    let fx = Fixture::new(json!({"agent": {"general": {"permission": {"task": "allow"}}}}), responses).await;
    let host = Arc::new(TestHost::default());
    let session = fx.prompt("delegate", &host).await;

    let children = fx.engine.store.list_children(&session.id).unwrap();
    assert_eq!(children.len(), 1);
    let child = &children[0];
    assert_eq!(child.title, "Find things (@general subagent)");
    assert_eq!(child.agent.as_deref(), Some("general"));
    // `general` names both todowrite and task in its own rules, so nothing extra is denied.
    assert_eq!(child.permission.clone().unwrap(), []);

    let tool = fx.tool(&session, 0);
    let state = completed(&tool);
    assert_eq!(state.output, format!("<task id=\"{}\" state=\"completed\">\n<task_result>\nchild says hi\n</task_result>\n</task>", child.id));
    assert_eq!(state.title, "Find things");
    assert_eq!(state.metadata["sessionId"], child.id.as_str());
    assert_eq!(state.metadata["parentSessionId"], session.id.as_str());
    assert_eq!(state.metadata["model"], json!({"modelID": "m1", "providerID": "mock"}));

    // The child ran the subagent's prompt; its own task call hit the depth limit.
    let requests = fx.seen();
    assert_eq!(last_user(&requests[1]), "look around");
    let nested = fx.tool(child, 0);
    assert_eq!(failed(&nested).error, "Subagent depth limit reached (1). Increase \"subagent_depth\" to allow nested subagents.");
    assert!(host.events.lock().unwrap().contains(&"child-text:child says hi".to_string()));
    assert_eq!(*host.text.lock().unwrap(), "parent done");
    let description = tool_def(&requests[0], "Task")["function"]["description"].as_str().unwrap().to_string();
    assert!(description.contains("\n\nAvailable agents:\n- explore: "), "{description}");
    assert_eq!((tool.tool.as_str(), nested.tool.as_str()), ("task", "task"));
}

#[tokio::test]
async fn todowrite_persists_and_unknown_or_malformed_calls_go_to_invalid() {
    let todos = json!({"todos": [{"content": "one", "status": "pending", "priority": "high"}, {"content": "two", "status": "completed", "priority": "low"}]});
    let responses = vec![
        calls(&[("c1", "todowrite", todos), ("c2", "nope", json!({"a": 1})), ("c3", "WRITE", json!({"filePath": "made.txt", "content": "x"})), ("c4", "read", json!("{not json"))]),
        text("ok"),
    ];
    let fx = Fixture::new(json!({}), responses).await;
    let host = Arc::new(TestHost::default());
    let session = fx.prompt("plan", &host).await;

    let saved = fx.engine.todos(&session.id).unwrap();
    assert_eq!(saved.iter().map(|t| t.content.as_str()).collect::<Vec<_>>(), ["one", "two"]);
    assert!(host.events.lock().unwrap().contains(&"todos:2".to_string()));
    assert_eq!(completed(&fx.tool(&session, 0)).title, "1 todos");

    let unknown = fx.tool(&session, 1);
    assert_eq!(unknown.tool, "invalid");
    assert_eq!(unknown.state.input()["tool"], "nope");
    let output = &completed(&unknown).output;
    let shell = opencode_tools::shell::tool_name(&opencode_tools::shell::acceptable(None));
    let first = if shell == "Bash" { "Bash, Edit, " } else { "Edit, " };
    assert!(output.starts_with(&format!("The arguments provided to the tool are invalid: Model tried to call unavailable tool 'nope'. Available tools: {first}")), "{output}");

    // A name that only differs by case is repaired instead.
    assert_eq!(fx.tool(&session, 2).tool, "write");
    assert!(matches!(fx.tool(&session, 2).state, ToolState::Completed(_)));
    assert!(fx.project.join("made.txt").exists());

    let malformed = fx.tool(&session, 3);
    assert_eq!(malformed.tool, "invalid");
    assert!(completed(&malformed).output.starts_with("The arguments provided to the tool are invalid: Invalid input for tool read: "));

    // Every call got a result in call order, so the next request replays all four.
    let replayed: Vec<String> = fx.seen()[1]["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "tool").map(|m| m["tool_call_id"].as_str().unwrap().to_string()).collect();
    assert_eq!(replayed, ["c1", "c2", "c3", "c4"]);
}

fn question(text: &str) -> Value {
    json!({"questions": [{"question": text, "header": "Pick", "options": [{"label": "Red", "description": "warm"}, {"label": "Blue", "description": "cold"}]}]})
}

#[tokio::test]
async fn questions_run_in_parallel_and_feed_answers_back() {
    let responses = vec![calls(&[("c1", "question", question("First?")), ("c2", "question", question("Second?"))]), text("thanks")];
    let fx = Fixture::new(json!({}), responses).await;
    let host = Arc::new(TestHost { answers: Mutex::new(Some(vec![vec!["Red".into(), "Blue".into()]])), barrier: Some(tokio::sync::Barrier::new(2)), ..Default::default() });
    let session = fx.prompt("ask me", &host).await;

    let first = fx.tool(&session, 0);
    assert_eq!(first.call_id, "c1");
    assert_eq!(completed(&first).output, "User has answered your questions: \"First?\"=\"Red, Blue\". You can now continue with the user's answers in mind.");
    assert_eq!(completed(&first).title, "Asked 1 question");
    assert_eq!(completed(&first).metadata["answers"], json!([["Red", "Blue"]]));
    assert_eq!(fx.tool(&session, 1).call_id, "c2");
    let asked = host.questions.lock().unwrap();
    assert_eq!(asked.len(), 2);
    assert!(asked[0].id.starts_with("que_") && asked[0].tool.is_some());
}

#[tokio::test]
async fn a_dismissed_question_stops_the_loop() {
    let fx = Fixture::new(json!({}), vec![calls(&[("c1", "question", question("Sure?"))]), text("never sent")]).await;
    // The default `Host::question` rejects.
    struct Plain;
    #[async_trait::async_trait]
    impl Host for Plain {
        fn update(&self, _update: Update) {}
        async fn permission(&self, _request: &PermissionRequest) -> Reply {
            Reply::Once
        }
    }
    let session = fx.engine.prompt(fx.input("ask"), Arc::new(Plain), CancellationToken::new()).await.unwrap();
    assert_eq!(failed(&fx.tool(&session, 0)).error, "The user dismissed this question");
    assert_eq!(fx.seen().len(), 1);
}

#[tokio::test]
async fn abort_marks_running_tools_and_the_message() {
    let fx = Fixture::new(json!({}), vec![calls(&[("c1", "question", question("Wait?"))])]).await;
    let host = Arc::new(TestHost { hang: true, ..Default::default() });
    let session = fx.engine.prompt(PromptInput { no_reply: true, ..fx.input("setup") }, host.clone(), CancellationToken::new()).await.unwrap();
    assert!(fx.seen().is_empty());
    assert_eq!(fx.engine.status(&session.id), Status::Idle);

    let (engine, input, runner) = (fx.engine.clone(), PromptInput { session_id: Some(session.id.clone()), ..fx.input("go") }, host.clone());
    let running = tokio::spawn(async move { engine.prompt(input, runner, CancellationToken::new()).await });
    host.asked_question.notified().await;
    assert_eq!(fx.engine.status(&session.id), Status::Busy);
    assert_eq!(fx.engine.busy(), std::slice::from_ref(&session.id));
    assert!(matches!(fx.engine.revert(&session.id, "msg_x", None), Err(Error::Busy(_))));
    assert!(fx.engine.abort(&session.id));
    assert!(matches!(running.await.unwrap(), Err(Error::Aborted)));
    assert!(!fx.engine.abort(&session.id));
    assert_eq!(fx.engine.status(&session.id), Status::Idle);

    let tool = fx.tool(&session, 0);
    let state = failed(&tool);
    assert_eq!(state.error, "Tool execution aborted");
    assert_eq!(state.metadata.as_ref().unwrap()["interrupted"], true);
    let rows = fx.rows(&session);
    let assistant = rows.last().unwrap().0.as_assistant().unwrap();
    assert_eq!(assistant.error, Some(AssistantError::MessageAbortedError { message: "Aborted".into() }));
    assert!(assistant.time.completed.is_some());
    let aborted = json!({"name": "MessageAbortedError", "data": {"message": "Aborted"}});
    assert_eq!(serde_json::to_value(&assistant.error).unwrap(), aborted);
    // An abort is reported as the error object only.
    assert_eq!(*host.errors.lock().unwrap(), [(session.id.clone(), aborted)]);
    assert!(!host.events.lock().unwrap().iter().any(|event| event.starts_with("error:")));

    // The interrupted call is an orphan: the next prompt starts a fresh step and still gives it a result.
    fx.script.responses.lock().unwrap().push_back(text("resumed"));
    let quiet = Arc::new(TestHost::default());
    fx.follow_up(&session, "continue", &quiet).await;
    let request = fx.seen().last().unwrap().clone();
    let tool_message = request["messages"].as_array().unwrap().iter().find(|m| m["role"] == "tool").unwrap().clone();
    assert_eq!(tool_message["content"], "Tool execution aborted");
}

#[tokio::test]
async fn command_expands_template_shell_and_file_references() {
    let config = json!({"command": {
        "greet": {"template": "Say $1 to @notes.txt then $2 !`echo marker`", "description": "greets"},
        "dig": {"template": "Investigate $ARGUMENTS", "description": "digs", "agent": "explore"}
    }});
    let fx = Fixture::build(config, vec![text("greeted"), text("found it"), text("summarized")], |project| {
        std::fs::write(project.join("notes.txt"), "remember this").unwrap();
    })
    .await;
    let host = Arc::new(TestHost::default());
    let input = CommandInput { directory: fx.project.clone(), command: "greet".into(), arguments: "hello \"big world\" again".into(), ..Default::default() };
    let session = fx.engine.command(input, host.clone(), CancellationToken::new()).await.unwrap();

    let rows = fx.rows(&session);
    let notes = fx.project.join("notes.txt");
    let user = texts(&rows[0].1);
    assert!(user[0].starts_with("Say hello to @notes.txt then big world again marker"), "{}", user[0]);
    assert_eq!(user[1], format!("Called the Read tool with the following input: {}", json!({"file_path": notes.to_string_lossy()})));
    assert_eq!(user[2], "1\tremember this\n\n(End of file - total 1 lines)");
    assert_eq!(kinds(&rows)[0], ["text", "text", "text", "file"]);
    let PartBody::File(file) = &rows[0].1[3].body else { panic!("expected file") };
    assert_eq!((file.mime.as_str(), file.filename.as_deref()), ("text/plain", Some("notes.txt")));
    assert!(file.url.starts_with("file:///"));
    assert!(last_user(&fx.seen()[0]).contains("remember this"));

    let missing = CommandInput { directory: fx.project.clone(), command: "nope".into(), ..Default::default() };
    let error = fx.engine.command(missing, host.clone(), CancellationToken::new()).await.unwrap_err().to_string();
    assert!(error.starts_with("Command not found: \"nope\". Available commands: init, review, greet, dig"), "{error}");

    // A command bound to a subagent becomes a subtask run through the task tool.
    let input = CommandInput { directory: fx.project.clone(), session_id: Some(session.id.clone()), command: "dig".into(), arguments: "the cache".into(), ..Default::default() };
    fx.engine.command(input, host.clone(), CancellationToken::new()).await.unwrap();
    let rows = fx.rows(&session);
    let PartBody::Subtask(subtask) = &rows[2].1[0].body else { panic!("expected subtask, got {:?}", kinds(&rows)) };
    assert_eq!((subtask.agent.as_str(), subtask.prompt.as_str(), subtask.command.as_deref()), ("explore", "Investigate the cache", Some("dig")));
    assert_eq!(rows[2].0.as_user().unwrap().agent, "build");
    let runner = rows[3].0.as_assistant().unwrap();
    assert_eq!((runner.agent.as_str(), runner.finish.as_deref()), ("explore", Some("tool-calls")));
    let child = &fx.engine.store.list_children(&session.id).unwrap()[0];
    let task = fx.tool(&session, 0);
    assert_eq!(task.tool, "task");
    assert_eq!(task.state.input()["subagent_type"], "explore");
    assert!(completed(&task).output.contains(&format!("<task id=\"{}\" state=\"completed\">\n<task_result>\nfound it", child.id)));
    assert_eq!(texts(&rows[4].1), ["Summarize the task tool output above and continue with your task."]);
    let request = fx.seen()[2].clone();
    let sent: Vec<&str> = request["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "user").map(|m| m["content"].as_str().unwrap()).collect();
    assert_eq!(sent[1..], ["The following tool was executed by the user", "Summarize the task tool output above and continue with your task."]);
}

#[tokio::test]
async fn attachments_become_read_results_or_media() {
    // The model has to accept images, or the image part is replaced by an error text.
    let config = json!({"provider": {"mock": {"models": {"m1": {"modalities": {"input": ["text", "image"], "output": ["text"]}}}}}});
    let fx = Fixture::build(config, vec![text("seen")], |project| {
        std::fs::write(project.join("a b.txt"), "line one\nline two\nline three\n").unwrap();
        std::fs::write(project.join("pixel.png"), [0x89, b'P', b'N', b'G']).unwrap();
    })
    .await;
    let url = |name: &str| format!("file:///{}", fx.project.join(name).to_string_lossy().replace('\\', "/").trim_start_matches('/').replace(' ', "%20"));
    let files = vec![
        FilePart { mime: "text/plain".into(), filename: Some("a b.txt".into()), url: format!("{}?start=2&end=3", url("a b.txt")), source: None },
        FilePart { mime: "image/png".into(), filename: Some("pixel.png".into()), url: url("pixel.png"), source: None },
        FilePart { mime: "text/plain".into(), filename: Some("inline.txt".into()), url: "data:text/plain;base64,aGVsbG8=".into(), source: None },
    ];
    let host = Arc::new(TestHost::default());
    let session = fx.engine.prompt(PromptInput { files, ..fx.input("look") }, host.clone(), CancellationToken::new()).await.unwrap();

    let rows = fx.rows(&session);
    assert_eq!(kinds(&rows)[0], ["text", "text", "text", "file", "text", "file", "text", "text", "file"]);
    let user = texts(&rows[0].1);
    let path = fx.project.join("a b.txt");
    assert_eq!(user[1], format!("Called the Read tool with the following input: {}", json!({"file_path": path.to_string_lossy(), "offset": 2, "limit": 2})));
    assert!(user[2].starts_with("2\tline two\n3\tline three\n\n(") && !user[2].contains("line one"), "{}", user[2]);
    assert_eq!(user[3], format!("Called the Read tool with the following input: {{\"file_path\":\"{}\"}}", fx.project.join("pixel.png").display()));
    assert_eq!(user[4..], ["Called the Read tool with the following input: {\"file_path\":\"inline.txt\"}", "hello"]);
    let PartBody::File(image) = &rows[0].1[5].body else { panic!("expected file") };
    assert_eq!(image.url, "data:image/png;base64,iVBORw==");

    // Text files are inlined as text; the image travels as an image part.
    let content = fx.seen()[0]["messages"][1]["content"].clone();
    let kinds: Vec<&str> = content.as_array().unwrap().iter().map(|p| p["type"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["text", "text", "text", "text", "image_url", "text", "text"]);
    assert_eq!(content[4]["image_url"]["url"], "data:image/png;base64,iVBORw==");
}

#[tokio::test]
async fn revert_fork_and_permission_overrides() {
    let fx = Fixture::new(json!({}), vec![text("one"), text("two"), text("three")]).await;
    let host = Arc::new(TestHost::default());
    let session = fx.prompt("first", &host).await;
    fx.follow_up(&session, "second", &host).await;
    fx.engine.settle().await;
    let rows = fx.rows(&session);
    assert_eq!(rows.len(), 4);

    // Reverting an assistant message reverts to the user message that caused it.
    let reverted = fx.engine.revert(&session.id, rows[3].0.id(), None).unwrap();
    assert_eq!(reverted.revert.as_ref().unwrap().message_id, rows[2].0.id());
    assert_eq!(fx.rows(&session).len(), 4);
    assert!(fx.engine.unrevert(&session.id).unwrap().revert.is_none());
    fx.engine.revert(&session.id, rows[2].0.id(), None).unwrap();

    let fork = fx.engine.fork(&session.id, Some(rows[2].0.id())).unwrap();
    assert_eq!(fork.title, "A Title (fork #1)");
    assert_eq!(fx.engine.fork(&fork.id, None).unwrap().title, "A Title (fork #2)");
    let copied = fx.rows(&fork);
    assert_eq!(copied.len(), 2);
    assert_eq!(copied[1].0.as_assistant().unwrap().parent_id, copied[0].0.id());
    assert!(copied[0].0.id() != rows[0].0.id() && copied[1].1.iter().all(|p| p.session_id == fork.id));
    assert_eq!(fork.tokens.unwrap().output, 2.0);

    // The next prompt drops the reverted turn; session rules hide denied tools and legacy switches replace them.
    let deny = PermissionRule { permission: "bash".into(), pattern: "*".into(), action: PermissionAction::Deny };
    assert_eq!(fx.engine.set_permission(&session.id, Some(vec![deny.clone()])).unwrap().permission, Some(vec![deny]));
    let after = fx.follow_up(&session, "third", &host).await;
    assert!(after.revert.is_none());
    let rows = fx.rows(&session);
    assert_eq!(rows.iter().map(|(_, parts)| texts(parts).concat()).collect::<Vec<_>>(), ["first", "one", "third", "three"]);
    // Rules and switches use the internal ids; they hide the exposed tools.
    let shell = opencode_tools::shell::tool_name(&opencode_tools::shell::acceptable(None)).to_string();
    let request = fx.seen().last().unwrap().clone();
    assert!(!tool_names(&request).contains(&shell) && tool_names(&request).contains(&"Read".into()));
    assert_eq!(request["messages"].as_array().unwrap().len(), 4);

    fx.script.responses.lock().unwrap().push_back(text("four"));
    let switches = [("read".to_string(), false)].into_iter().collect();
    let input = PromptInput { session_id: Some(session.id.clone()), tools: Some(switches), ..fx.input("fourth") };
    let updated = fx.engine.prompt(input, host.clone(), CancellationToken::new()).await.unwrap();
    assert_eq!(updated.permission.unwrap()[0].permission, "read");
    let names = tool_names(fx.seen().last().unwrap());
    assert!(names.contains(&shell) && !names.contains(&"Read".into()), "{names:?}");
    assert!(fx.paths.data.exists());
}

#[tokio::test]
async fn a_rejected_permission_fails_queued_requests_and_stops() {
    let write = |name: &str| json!({"filePath": name, "content": "x"});
    let responses = vec![calls(&[("c1", "write", write("a.txt")), ("c2", "write", write("b.txt"))]), text("never sent")];
    let fx = Fixture::new(json!({"permission": {"edit": "ask"}}), responses).await;
    let host = Arc::new(TestHost { reject: true, ..Default::default() });
    let session = fx.prompt("write twice", &host).await;

    assert_eq!(*host.asked.lock().unwrap(), ["edit"]);
    for index in 0..2 {
        assert_eq!(failed(&fx.tool(&session, index)).error, "The user rejected permission to use this specific tool call.");
    }
    assert!(!fx.project.join("a.txt").exists() && !fx.project.join("b.txt").exists());
    assert_eq!(fx.seen().len(), 1);
    let updates = host.tools.lock().unwrap();
    assert_eq!(updates.len(), 4);
    assert!(updates.iter().all(|part| part.session_id == session.id));
}

#[tokio::test]
async fn three_identical_calls_ask_about_a_doom_loop() {
    let fx = Fixture::build(json!({}), vec![], |project| std::fs::write(project.join("same.txt"), "same").unwrap()).await;
    let read = json!({"filePath": fx.project.join("same.txt").to_string_lossy()});
    let other = json!({"filePath": fx.project.join("same.txt").to_string_lossy(), "limit": 1});
    fx.script.responses.lock().unwrap().extend([
        calls(&[("c1", "read", read.clone()), ("c2", "read", other), ("c3", "read", read.clone()), ("c4", "read", read.clone())]),
        calls(&[("c5", "read", read.clone()), ("c6", "read", read.clone()), ("c7", "read", read)]),
        text("done"),
    ]);
    let host = Arc::new(TestHost::default());
    let session = fx.prompt("loop", &host).await;
    // Only the second step has three identical calls in a row.
    assert_eq!(*host.asked.lock().unwrap(), ["doom_loop"]);
    assert!((0..7).all(|index| matches!(fx.tool(&session, index).state, ToolState::Completed(_))));
}

#[tokio::test]
async fn a_prompt_during_a_run_joins_it() {
    let responses = vec![calls(&[("c1", "question", question("Ready?"))]), text("both handled")];
    let fx = Fixture::new(json!({}), responses).await;
    let host = Arc::new(TestHost { answers: Mutex::new(Some(vec![vec!["Red".into()]])), barrier: Some(tokio::sync::Barrier::new(2)), ..Default::default() });
    let (engine, input, runner) = (fx.engine.clone(), fx.input("first"), host.clone());
    let first = tokio::spawn(async move { engine.prompt(input, runner, CancellationToken::new()).await });
    host.asked_question.notified().await;
    let session_id = host.questions.lock().unwrap()[0].session_id.clone();

    let (engine, runner) = (fx.engine.clone(), host.clone());
    let input = PromptInput { session_id: Some(session_id.clone()), ..fx.input("second") };
    let second = tokio::spawn(async move { engine.prompt(input, runner, CancellationToken::new()).await });
    while fx.engine.store.list_messages(&session_id).unwrap().iter().filter(|(m, _)| m.as_user().is_some()).count() < 2 {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    host.barrier.as_ref().unwrap().wait().await;
    let (first, second) = (first.await.unwrap().unwrap(), second.await.unwrap().unwrap());
    assert_eq!(first.id, second.id);

    // The running loop picked the second message up: one more model call answers both.
    let requests = fx.seen();
    assert_eq!(requests.len(), 2);
    let roles: Vec<&str> = requests[1]["messages"].as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["system", "user", "assistant", "tool", "user"]);
    assert_eq!(last_user(&requests[1]), "second");
    let rows = fx.rows(&first);
    assert_eq!(rows.last().unwrap().0.as_assistant().unwrap().parent_id, rows[2].0.id());
}

#[tokio::test]
async fn mcp_servers_feed_tools_resources_prompts_and_instructions() {
    let config = json!({
        "mcp": {"demo": {"type": "local", "command": [stdio_server()]}, "off": {"type": "local", "command": [stdio_server()], "enabled": false}},
        "permission": {"demo_echo": "ask", "demo_fail": "deny"}
    });
    let responses = vec![
        calls(&[
            ("c1", "demo_echo", json!({"text": "hi"})),
            ("c2", "list_mcp_resources", json!({})),
            ("c3", "read_mcp_resource", json!({"server": "demo", "uri": "file:///readme.md"})),
            ("c4", "list_mcp_resource_templates", json!({"server": "nope"})),
        ]),
        text("done"),
        text("greeted"),
    ];
    let fx = Fixture::new(config, responses).await;
    let host = Arc::new(TestHost::default());
    let session = fx.prompt("use mcp", &host).await;

    let requests = fx.seen();
    let names = tool_names(&requests[0]);
    for expected in ["demo_echo", "demo_weird_name", "list_mcp_resources", "list_mcp_resource_templates", "read_mcp_resource", "Read"] {
        assert!(names.contains(&expected.to_string()), "{expected} missing from {names:?}");
    }
    assert!(!names.contains(&"demo_fail".to_string()) && !names.iter().any(|name| name.starts_with("off_")));
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);
    let echo = requests[0]["tools"].as_array().unwrap().iter().find(|t| t["function"]["name"] == "demo_echo").unwrap();
    assert_eq!(echo["function"]["description"], "Echo text");
    assert_eq!(echo["function"]["parameters"]["additionalProperties"], false);
    let block = "<mcp_instructions>\n  <server name=\"demo\">\n    Use the test tools.\n  </server>\n</mcp_instructions>";
    assert!(system_of(&requests[0]).contains(block), "{}", system_of(&requests[0]));

    // Only the MCP tool is set to ask; the request names the session and the call.
    let rows = fx.rows(&session);
    let asks = host.asks.lock().unwrap().clone();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!((asks[0].request.permission.as_str(), asks[0].request.patterns.clone()), ("demo_echo", vec!["*".to_string()]));
    assert_eq!((asks[0].session_id.as_str(), asks[0].tool.clone()), (session.id.as_str(), Some((rows[1].0.id().to_string(), "c1".to_string()))));

    assert_eq!(completed(&fx.tool(&session, 0)).output, "echo: hi");
    let listed = fx.tool(&session, 1);
    let state = completed(&listed);
    assert_eq!(state.title, "MCP resources");
    let expected = json!({"resources": [{"name": "readme", "uri": "file:///readme.md", "mimeType": "text/markdown", "server": "demo"}]});
    assert_eq!(state.output, serde_json::to_string_pretty(&expected).unwrap());
    assert_eq!(Value::Object(state.metadata.clone()), json!({"count": 1, "servers": ["demo"], "truncated": false}));
    let read = fx.tool(&session, 2);
    let state = completed(&read);
    assert_eq!(state.title, "MCP resource: file:///readme.md");
    assert_eq!(state.output, "Resource: file:///readme.md\nMIME: application/octet-stream\nhello resource");
    assert_eq!(
        Value::Object(state.metadata.clone()),
        json!({"server": "demo", "uri": "file:///readme.md", "contents": 1, "attachments": 0, "truncated": false})
    );
    assert_eq!(failed(&fx.tool(&session, 3)).error, "MCP server \"nope\" does not support resources. Available resource servers: demo");

    // Prompts are slash commands; the server process is reused across prompts.
    let commands = fx.engine.commands(&fx.project).await.unwrap();
    let greet = commands.iter().find(|c| c.name == "demo:greet_user").unwrap();
    assert_eq!((greet.template.as_str(), greet.description.as_deref()), ("Hello $1", Some("Greets")));
    assert_eq!(greet.hints, ["$1"]);
    assert!(commands.iter().any(|c| c.name == "init"));
    let manager = fx.engine.mcp(&fx.project).await.unwrap();
    let pid = manager.clients()["demo"].pid();
    assert!(pid.is_some());
    let status = serde_json::to_value(manager.status()).unwrap();
    assert_eq!(status, json!({"demo": {"status": "connected"}, "off": {"status": "disabled"}}));
    let input = CommandInput {
        directory: fx.project.clone(),
        session_id: Some(session.id.clone()),
        command: "demo:greet_user".into(),
        arguments: "World".into(),
        ..Default::default()
    };
    fx.engine.command(input, host.clone(), CancellationToken::new()).await.unwrap();
    assert_eq!(last_user(fx.seen().last().unwrap()), "Hello World");
    assert_eq!(fx.engine.mcp(&fx.project).await.unwrap().clients()["demo"].pid(), pid);

    fx.engine.shutdown().await;
    assert!(manager.clients().is_empty());
}

#[tokio::test]
async fn streamed_chunks_name_the_stored_parts() {
    let delta = |delta: Value| json!({"choices": [{"index": 0, "delta": delta}]});
    let stream = sse(&[
        delta(json!({"reasoning_content": "think "})),
        delta(json!({"reasoning_content": "hard"})),
        delta(json!({"content": "Hel"})),
        delta(json!({"content": "lo"})),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": usage(5, 2)}),
    ]);
    let fx = Fixture::new(json!({}), vec![stream]).await;
    let host = Arc::new(TestHost::default());
    let session = fx.prompt("hi", &host).await;

    let rows = fx.rows(&session);
    assert_eq!(kinds(&rows)[1], ["step-start", "reasoning", "text", "step-finish"]);
    let (message, parts) = &rows[1];
    let (reasoning, text) = (&parts[1], &parts[2]);
    match (&reasoning.body, &text.body) {
        (PartBody::Reasoning(r), PartBody::Text(t)) => {
            assert_eq!((r.text.as_str(), t.text.as_str()), ("think hard", "Hello"));
            assert!(r.time.end.is_some() && t.time.is_some_and(|time| time.end.is_some()));
        }
        other => panic!("{other:?}"),
    }
    let chunk = |part: &Part, delta: &str| (session.id.clone(), message.id().to_string(), part.id.clone(), delta.to_string());
    assert_eq!(*host.deltas.lock().unwrap(), [chunk(reasoning, "think "), chunk(reasoning, "hard"), chunk(text, "Hel"), chunk(text, "lo")]);
    assert_eq!(*host.text.lock().unwrap(), "Hello");
}

#[tokio::test]
async fn feedback_on_a_rejection_reaches_the_model_and_the_loop_goes_on() {
    let write = json!({"filePath": "a.txt", "content": "x"});
    let fx = Fixture::new(json!({"permission": {"edit": "ask"}}), vec![calls(&[("c1", "write", write)]), text("ok, b.txt then")]).await;
    let host = Arc::new(TestHost { feedback: Some("use b.txt".into()), ..Default::default() });
    let session = fx.prompt("write", &host).await;

    let error = "The user rejected permission to use this specific tool call with the following feedback: use b.txt";
    assert_eq!(failed(&fx.tool(&session, 0)).error, error);
    assert!(!fx.project.join("a.txt").exists());
    let requests = fx.seen();
    assert_eq!(requests.len(), 2);
    let tool_message = requests[1]["messages"].as_array().unwrap().iter().find(|m| m["role"] == "tool").unwrap();
    assert_eq!(tool_message["content"], error);
    let asks = host.asks.lock().unwrap().clone();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].tool, Some((fx.rows(&session)[1].0.id().to_string(), "c1".to_string())));
}

#[tokio::test]
async fn subagent_requests_and_chunks_belong_to_the_child_session() {
    let task = json!({"description": "Write it", "prompt": "write the file", "subagent_type": "general"});
    let write = json!({"filePath": "child.txt", "content": "x"});
    let responses = vec![calls(&[("c1", "task", task)]), calls(&[("c2", "write", write)]), text("child done"), text("parent done")];
    let fx = Fixture::new(json!({"permission": {"edit": "ask"}}), responses).await;
    let host = Arc::new(TestHost::default());
    let session = fx.prompt("delegate", &host).await;

    let children = fx.engine.store.list_children(&session.id).unwrap();
    let child = &children[0];
    assert_eq!(child.path, session.path);
    assert!(fx.project.join("child.txt").exists());
    let asks: Vec<PermissionAsk> = host.asks.lock().unwrap().iter().filter(|ask| ask.request.permission == "edit").cloned().collect();
    assert_eq!(asks.len(), 1);
    let child_rows = fx.rows(child);
    let asking = child_rows.iter().find(|(m, _)| m.as_assistant().is_some()).unwrap().0.id().to_string();
    assert_eq!((asks[0].session_id.as_str(), asks[0].tool.clone()), (child.id.as_str(), Some((asking, "c2".to_string()))));
    let deltas = host.deltas.lock().unwrap();
    let said = |session_id: &str, text: &str| deltas.iter().any(|(s, _, _, delta)| s == session_id && delta == text);
    assert!(said(&child.id, "child done") && said(&session.id, "parent done"));
}

#[tokio::test]
async fn failures_carry_the_error_object() {
    let body = json!({"error": {"message": "bad request body"}}).to_string();
    let fx = Fixture::new(json!({}), vec![Response::Status(400, body)]).await;
    let host = Arc::new(TestHost::default());
    let result = fx.engine.prompt(fx.input("hi"), host.clone(), CancellationToken::new()).await;
    assert!(matches!(result, Err(Error::Llm(_))));

    let errors = host.errors.lock().unwrap().clone();
    assert_eq!(errors.len(), 1, "{errors:?}");
    let (session_id, error) = &errors[0];
    assert_eq!((&error["name"], &error["data"]["statusCode"]), (&json!("APIError"), &json!(400)));
    let rows = fx.engine.store.list_messages(session_id).unwrap();
    let assistant = rows.last().unwrap().0.as_assistant().unwrap();
    assert_eq!(&serde_json::to_value(&assistant.error).unwrap(), error);
    let message = error["data"]["message"].as_str().unwrap();
    assert!(host.events.lock().unwrap().contains(&format!("error:{message}")));

    // An unknown command fails before any session work; nothing is reported through the host.
    let input = CommandInput { directory: fx.project.clone(), command: "nope".into(), ..Default::default() };
    let missing = fx.engine.command(input, host.clone(), CancellationToken::new()).await;
    assert!(matches!(missing, Err(Error::Config(message)) if message.starts_with("Command not found: \"nope\". Available commands: init, review")));
}

#[tokio::test]
async fn shell_records_a_user_command_as_a_bash_call() {
    let shell = if cfg!(windows) { "cmd" } else { "sh" };
    let fx = Fixture::new(json!({"shell": shell}), vec![]).await;
    let host = Arc::new(TestHost::default());
    let session = fx.engine.create_session(&fx.project, CreateSession { title: Some("Shell".into()), ..Default::default() }).unwrap();
    assert_eq!((session.title.as_str(), session.directory.as_str()), ("Shell", fx.project.to_str().unwrap()));
    assert_eq!(fx.engine.store.get_session(&session.id).unwrap(), Some(session.clone()));
    assert_eq!(fx.engine.default_model(&fx.project).unwrap(), Some(("mock".to_string(), "m1".to_string())));

    let input = ShellInput { session_id: session.id.clone(), agent: "build".into(), command: "echo shell-marker".into(), ..Default::default() };
    let (message, parts) = fx.engine.shell(input.clone(), host.clone(), CancellationToken::new()).await.unwrap();
    let rows = fx.rows(&session);
    assert_eq!(kinds(&rows), [vec!["text"], vec!["tool"]]);
    assert_eq!(texts(&rows[0].1), ["The following tool was executed by the user"]);
    assert_eq!((message.id(), parts.len()), (rows[1].0.id(), 1));
    let assistant = rows[1].0.as_assistant().unwrap();
    assert_eq!((assistant.parent_id.as_str(), assistant.finish.as_deref(), assistant.agent.as_str()), (rows[0].0.id(), None, "build"));
    assert!(assistant.time.completed.is_some());
    let tool = fx.tool(&session, 0);
    let state = completed(&tool);
    assert_eq!((tool.tool.as_str(), state.title.as_str(), &state.input["command"]), ("bash", "", &json!("echo shell-marker")));
    assert!(state.output.contains("shell-marker"), "{}", state.output);
    assert_eq!(state.metadata["output"], state.output.as_str());
    assert!(fx.seen().is_empty());
    assert_eq!(fx.engine.status(&session.id), Status::Idle);
    assert!(host.tools.lock().unwrap().len() >= 2);

    // Aborting kills the command and keeps what it printed, marked as aborted.
    let slow = if cfg!(windows) { "echo started& ping -n 60 127.0.0.1 >nul" } else { "echo started; sleep 60" };
    let (engine, runner) = (fx.engine.clone(), host.clone());
    let long = ShellInput { command: slow.into(), ..input.clone() };
    let started = std::time::Instant::now();
    let running = tokio::spawn(async move { engine.shell(long, runner, CancellationToken::new()).await });
    loop {
        let printed = fx.rows(&session).last().is_some_and(|(_, parts)| {
            parts.iter().any(|p| matches!(&p.body, PartBody::Tool(t) if matches!(&t.state, ToolState::Running(s) if s.metadata.is_some())))
        });
        if printed {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(fx.engine.status(&session.id), Status::Busy);
    let busy = fx.engine.shell(input.clone(), host.clone(), CancellationToken::new()).await;
    assert!(matches!(busy, Err(Error::Busy(_))));
    assert!(fx.engine.abort(&session.id));
    let (_, parts) = running.await.unwrap().unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
    let PartBody::Tool(aborted) = &parts[0].body else { panic!("{parts:?}") };
    let output = &completed(aborted).output;
    assert!(output.contains("started") && output.ends_with("\n\n<metadata>\nUser aborted the command\n</metadata>"), "{output}");
    assert_eq!(fx.engine.status(&session.id), Status::Idle);

    let unknown = fx.engine.shell(ShellInput { agent: "nope".into(), ..input }, host.clone(), CancellationToken::new()).await;
    assert!(matches!(unknown, Err(Error::Config(message)) if message.starts_with("Agent not found: \"nope\".")));
    assert_eq!(host.errors.lock().unwrap().last().unwrap().1["name"], "UnknownError");
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(["-c", "user.name=test", "-c", "user.email=test@example.com", "-c", "commit.gpgsign=false", "-c", "core.autocrlf=false"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

// A real repository with one committed file.
fn repository(project: &std::path::Path) {
    git(project, &["init", "-q"]);
    std::fs::write(project.join("keep.txt"), "one\ntwo\n").unwrap();
    git(project, &["add", "keep.txt"]);
    git(project, &["commit", "-q", "-m", "init"]);
}

fn slashed(path: &std::path::Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn snapshots(parts: &[Part]) -> (Option<String>, Option<String>) {
    let start = parts.iter().find_map(|p| match &p.body {
        PartBody::StepStart(step) => step.snapshot.clone(),
        _ => None,
    });
    let finish = parts.iter().find_map(|p| match &p.body {
        PartBody::StepFinish(step) => step.snapshot.clone(),
        _ => None,
    });
    (start, finish)
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshots_patches_diffs_and_revert() {
    let fx = Fixture::build(json!({}), vec![], |project| repository(project)).await;
    let (new, keep) = (fx.project.join("new.txt"), fx.project.join("keep.txt"));
    fx.script.responses.lock().unwrap().extend([
        calls(&[("call_0", "Read", json!({"file_path": keep.to_string_lossy()})), ("call_1", "Write", json!({"file_path": new.to_string_lossy(), "content": "hi\n"}))]),
        calls(&[("call_2", "Edit", json!({"file_path": keep.to_string_lossy(), "old_string": "two", "new_string": "TWO"}))]),
        text("Done."),
    ]);
    let seen: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    let sink = seen.clone();
    fx.engine.watch_events(move |event| sink.lock().unwrap().push((event.kind.clone(), event.properties.clone())));
    let host = Arc::new(TestHost::default());
    let session = fx.prompt("change files", &host).await;
    fx.engine.settle().await;

    let rows = fx.rows(&session);
    let patch = vec!["step-start", "tool", "step-finish", "patch"];
    assert_eq!(kinds(&rows), [vec!["text"], vec!["step-start", "tool", "tool", "step-finish", "patch"], patch, vec!["step-start", "text", "step-finish"]]);
    let (first, second, third) = (snapshots(&rows[1].1), snapshots(&rows[2].1), snapshots(&rows[3].1));
    let hash = |value: &Option<String>| value.clone().filter(|h| h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit())).expect("tree hash");
    assert_ne!(hash(&first.0), hash(&first.1));
    assert_eq!((hash(&first.1), hash(&second.1)), (hash(&second.0), hash(&third.0)));
    assert_eq!(third.0, third.1);
    let patches: Vec<(String, Vec<String>)> = rows
        .iter()
        .flat_map(|(_, parts)| parts)
        .filter_map(|p| match &p.body {
            PartBody::Patch(patch) => Some((patch.hash.clone(), patch.files.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(patches, [(hash(&first.0), vec![slashed(&new)]), (hash(&second.0), vec![slashed(&keep)])]);

    // The user message carries the diff of everything its answer changed; the session summary is reset to zero.
    let user = rows[0].0.as_user().unwrap().clone();
    let diffs = user.summary.clone().expect("summary").diffs;
    let brief: Vec<(Option<&str>, f64, f64, Option<&str>)> = diffs
        .iter()
        .map(|d| (d.file.as_deref(), d.additions, d.deletions, serde_json::to_value(d.status).ok().and_then(|s| s.as_str().map(|_| ())).map(|_| "")))
        .collect();
    assert_eq!(brief, [(Some("keep.txt"), 1.0, 1.0, Some("")), (Some("new.txt"), 1.0, 0.0, Some(""))]);
    assert_eq!(serde_json::to_value(&diffs[0]).unwrap()["status"], "modified");
    assert_eq!(serde_json::to_value(&diffs[1]).unwrap()["status"], "added");
    assert!(diffs[0].patch.as_deref().is_some_and(|p| p.contains("-two") && p.contains("+TWO")), "{:?}", diffs[0].patch);
    let stored = fx.engine.store.get_session(&session.id).unwrap().unwrap();
    assert_eq!(serde_json::to_value(stored.summary).unwrap(), json!({"additions": 0, "deletions": 0, "files": 0}));
    assert_eq!(fx.engine.message_diff(&session.id, Some(&user.id)).unwrap(), diffs);
    assert!(fx.engine.message_diff(&session.id, None).unwrap().is_empty());

    // Tool edits and summaries are announced on the bus, to the host and to engine listeners alike.
    let bus = host.bus.lock().unwrap().clone();
    let shown = |path: &PathBuf| path.to_string_lossy().into_owned();
    let expected = [
        ("file.edited".to_string(), json!({"file": shown(&new)})),
        ("file.watcher.updated".to_string(), json!({"file": shown(&new), "event": "add"})),
        ("file.edited".to_string(), json!({"file": shown(&keep)})),
        ("file.watcher.updated".to_string(), json!({"file": shown(&keep), "event": "change"})),
    ];
    let files: Vec<(String, Value)> = bus.iter().filter(|(kind, _)| kind.starts_with("file.")).cloned().collect();
    assert_eq!(files, expected);
    let diff_events = bus.iter().filter(|(kind, _)| kind == "session.diff").count();
    assert_eq!(diff_events, 4, "{bus:?}");
    assert!(bus.iter().all(|(kind, properties)| kind != "session.diff" || *properties == json!({"sessionID": session.id, "diff": []})));
    assert_eq!(*seen.lock().unwrap(), bus);

    // Revert restores the files and records what it undid; unrevert brings the changes back.
    let reverted = fx.engine.revert(&session.id, &user.id, None).unwrap();
    assert!(!new.exists());
    assert_eq!(std::fs::read_to_string(&keep).unwrap(), "one\ntwo\n");
    let revert = reverted.revert.clone().expect("revert");
    assert_eq!(revert.message_id, user.id);
    let taken = hash(&revert.snapshot);
    assert!(revert.diff.as_deref().is_some_and(|d| d.contains("new.txt") && d.contains("-TWO") && d.contains("+two")), "{:?}", revert.diff);
    assert_eq!(serde_json::to_value(reverted.summary).unwrap(), json!({"additions": 2, "deletions": 1, "files": 2}));
    let last = seen.lock().unwrap().last().cloned().unwrap();
    assert_eq!((last.0.as_str(), last.1["diff"].as_array().map(Vec::len)), ("session.diff", Some(2)));
    assert_eq!(fx.rows(&session).len(), 4);

    // A second revert keeps the first snapshot.
    let again = fx.engine.revert(&session.id, &user.id, None).unwrap();
    assert_eq!(again.revert.unwrap().snapshot, Some(taken));

    let restored = fx.engine.unrevert(&session.id).unwrap();
    assert!(restored.revert.is_none());
    assert_eq!(std::fs::read_to_string(&new).unwrap(), "hi\n");
    assert_eq!(std::fs::read_to_string(&keep).unwrap(), "one\nTWO\n");
    assert!(fx.engine.unrevert(&session.id).unwrap().revert.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshots_can_be_switched_off() {
    let fx = Fixture::build(json!({"snapshot": false}), vec![], |project| repository(project)).await;
    let new = fx.project.join("new.txt");
    fx.script.responses.lock().unwrap().extend([
        calls(&[("call_1", "write", json!({"filePath": new.to_string_lossy(), "content": "hi\n"}))]),
        text("Done."),
    ]);
    let host = Arc::new(TestHost::default());
    let session = fx.prompt("change files", &host).await;
    fx.engine.settle().await;
    let rows = fx.rows(&session);
    assert_eq!(kinds(&rows), [vec!["text"], vec!["step-start", "tool", "step-finish"], vec!["step-start", "text", "step-finish"]]);
    assert_eq!(snapshots(&rows[1].1), (None, None));
    assert!(rows[0].0.as_user().unwrap().summary.is_none());
    assert!(!fx.paths.data.join("snapshot-rs").exists());
    // Without snapshots a revert only hides messages.
    let reverted = fx.engine.revert(&session.id, rows[0].0.id(), None).unwrap();
    assert!(new.exists());
    assert_eq!(reverted.revert.map(|r| (r.snapshot, r.diff)), Some((None, None)));
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
    assert!(path.exists(), "missing fake language server at {}", path.display());
    path.to_string_lossy().into_owned()
}

// A formatter that appends its second argument as a line to the file named by its first.
fn fake_formatter(dir: &std::path::Path) -> String {
    let path = if cfg!(windows) {
        let path = dir.join("append.cmd");
        std::fs::write(&path, "@echo off\r\necho %~2>>\"%~1\"\r\n").unwrap();
        path
    } else {
        let path = dir.join("append");
        std::fs::write(&path, "#!/bin/sh\necho \"$2\" >> \"$1\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    };
    path.to_string_lossy().into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn edits_are_formatted_and_checked() {
    let scripts = tempfile::tempdir().unwrap();
    let config = json!({
        "formatter": {"stamp": {"command": [fake_formatter(scripts.path()), "$FILE", "formatted"], "extensions": [".fake"]}},
        "lsp": {"fake": {"command": [fake_lsp()], "extensions": [".fake"]}}
    });
    let fx = Fixture::new(config, vec![]).await;
    let (target, other) = (fx.project.join("a.fake"), fx.project.join("b.txt"));
    let shown = target.to_string_lossy().into_owned();
    fx.script.responses.lock().unwrap().extend([
        calls(&[("call_1", "Write", json!({"file_path": shown, "content": "ok\nERR here\n"}))]),
        calls(&[("call_2", "Edit", json!({"file_path": shown, "old_string": "ERR here", "new_string": "fine"}))]),
        calls(&[("call_3", "Write", json!({"file_path": other.to_string_lossy(), "content": "plain\n"}))]),
        calls(&[("call_4", "Read", json!({"file_path": shown}))]),
        text("Done."),
    ]);
    let updates: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = updates.clone();
    fx.engine.watch_events(move |event| sink.lock().unwrap().push(event.kind.clone()));
    let host = Arc::new(TestHost::default());
    let session = fx.prompt("write a.fake", &host).await;

    // The formatter ran after each write; the language server saw the formatted text.
    let line = if cfg!(windows) { "\r\n" } else { "\n" };
    assert_eq!(std::fs::read_to_string(&target).unwrap(), format!("ok\nfine\nformatted{line}formatted{line}"));
    assert_eq!(std::fs::read_to_string(&other).unwrap(), "plain\n");
    let written = fx.tool(&session, 0);
    let written = completed(&written);
    assert_eq!(
        written.output,
        format!(
            "Wrote file successfully.\n\nLSP errors detected in this file, please fix:\n<diagnostics file=\"{shown}\">\nERROR [2:1] bad line\n</diagnostics>"
        )
    );
    let diagnostics = written.metadata["diagnostics"].as_object().unwrap();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics.values().next().unwrap().as_array().unwrap().len(), 2);

    let edited = fx.tool(&session, 1);
    let edited = completed(&edited);
    assert_eq!(edited.output, "Edit applied successfully.");
    assert_eq!(edited.metadata["diagnostics"].as_object().unwrap().values().next().unwrap().as_array().unwrap().len(), 1);
    let diff = edited.metadata["diff"].as_str().unwrap();
    assert!(diff.contains("-ERR here") && diff.contains("+fine") && diff.contains("+formatted"), "{diff}");
    assert_eq!(edited.metadata["filediff"]["additions"], 2);

    // A file no formatter or server matches is left alone; errors elsewhere are not reported once fixed.
    let plain = fx.tool(&session, 2);
    assert_eq!(completed(&plain).output, "Wrote file successfully.");
    let read = fx.tool(&session, 3);
    assert!(completed(&read).output.starts_with("1\tok\n2\tfine\n"), "{}", completed(&read).output);

    assert_eq!(updates.lock().unwrap().iter().filter(|kind| *kind == "lsp.updated").count(), 1);
    let workspace = fx.engine.workspace(&fx.project).unwrap();
    assert_eq!(serde_json::to_value(workspace.lsp_status()).unwrap(), json!([{"id": "fake", "name": "fake", "root": "", "status": "connected"}]));
    let formatters = serde_json::to_value(workspace.formatter_status().await).unwrap();
    let formatters = formatters.as_array().unwrap();
    assert!(formatters.contains(&json!({"name": "stamp", "extensions": [".fake"], "enabled": true})) && formatters.iter().any(|f| f["name"] == "gofmt"));

    // Disposing the instance stops the language server; the next use starts from the config again.
    fx.engine.dispose(&fx.project).await;
    assert!(fx.engine.workspace(&fx.project).unwrap().lsp_status().is_empty());
    fx.engine.shutdown().await;
}

// `bun` the way the plugin host looks it up; npm's Windows shim (`bun.cmd`) is replaced by the executable it wraps.
fn bun_path() -> Option<PathBuf> {
    static FOUND: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    FOUND
        .get_or_init(|| {
            let found = opencode_plugin::find_bun(None).ok()?;
            let wrapped = found.parent()?.join("node_modules").join("bun").join("bin").join(format!("bun{}", std::env::consts::EXE_SUFFIX));
            let path = if wrapped.is_file() { wrapped } else { found };
            let runs = std::process::Command::new(&path).arg("--version").stdin(std::process::Stdio::null()).output();
            runs.is_ok_and(|out| out.status.success()).then_some(path)
        })
        .clone()
}

fn bun() -> bool {
    let found = bun_path().is_some();
    if !found {
        eprintln!("SKIPPED (bun-dependent test): `bun` is not on PATH");
    }
    found
}

const HOOKS_PLUGIN: &str = r#"
import fs from "node:fs"
import path from "node:path"

export const TestPlugin = async ({ directory, serverUrl, client }) => {
  const log = (line) => fs.appendFileSync(path.join(directory, "..", "plugin.log"), line + "\n")
  log(`init ${serverUrl.href} ${client.config.headers?.Authorization ? "auth" : "open"}`)
  return {
    config: async (cfg) => {
      cfg.instructions = [...(cfg.instructions ?? []), "PLUGIN.md"]
    },
    event: async ({ event }) => log(`event ${event.type}`),
    "chat.message": async (input, output) => {
      log(`chat.message ${input.sessionID === output.message.sessionID} ${input.agent ?? "-"} ${output.parts.length}`)
      output.parts[0].text += " [seen]"
    },
    "chat.params": async (input, output) => {
      log(`chat.params ${input.agent} ${input.model.id} ${input.provider.info.id} ${input.message.role}`)
      output.temperature = 0.25
    },
    "chat.headers": async (_input, output) => {
      output.headers["x-plugin"] = "yes"
    },
    "tool.definition": async (input, output) => {
      if (input.toolID === "read") output.description = "PLUGGED " + output.description
    },
    "tool.execute.before": async (input, output) => {
      if (input.tool === "write") output.args.content += "+before"
    },
    "tool.execute.after": async (input, output) => {
      if (input.tool !== "write") return
      output.output += " [after]"
      output.title = "retitled"
    },
    "shell.env": async (_input, output) => {
      output.env.PLUGIN_VAR = "from-plugin"
    },
    "experimental.chat.system.transform": async (_input, output) => {
      output.system.push("EXTRA SYSTEM")
    },
    "experimental.text.complete": async (_input, output) => {
      output.text += "!"
    },
    "command.execute.before": async (input, output) => {
      output.parts.push({ type: "text", text: `hooked ${input.command} ${input.arguments}` })
    },
    tool: {
      shout: {
        description: "Shouts the text",
        args: { text: { type: "string", description: "what to shout" } },
        async execute(args) {
          return args.text.toUpperCase()
        },
      },
    },
  }
}
"#;

const HELLO_TOOL: &str = r#"
export default {
  description: "Says hello",
  args: {},
  async execute(_args, ctx) {
    return `hello from ${ctx.agent}`
  },
}
"#;

struct FakeServer {
    launches: Mutex<u32>,
}

#[async_trait::async_trait]
impl opencode_session::ServerLauncher for FakeServer {
    async fn launch(&self) -> Result<opencode_session::ServerInfo, String> {
        *self.launches.lock().unwrap() += 1;
        Ok(opencode_session::ServerInfo::with_password("http://127.0.0.1:9".into(), None, "secret"))
    }
}

// Points the sidecar at a stub SDK so nothing is resolved from the machine or the network.
fn plugin_settings(fx: &Fixture) {
    let stub = fx.project.parent().unwrap().join("sdk-stub.mjs");
    std::fs::write(&stub, "export function createOpencodeClient(config) { return { stub: true, config } }\n").unwrap();
    let settings = opencode_session::PluginSettings { sdk_path: Some(stub.to_string_lossy().into_owned()), bun_path: bun_path(), ..Default::default() };
    fx.engine.set_plugin_settings(settings);
}

async fn logged(fx: &Fixture, needle: &str) -> String {
    let file = fx.project.parent().unwrap().join("plugin.log");
    for _ in 0..100 {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        if text.contains(needle) {
            return text;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("`{needle}` never showed up in {}", std::fs::read_to_string(&file).unwrap_or_default());
}

#[tokio::test(flavor = "multi_thread")]
async fn plugins_hook_into_the_loop() {
    if !bun() {
        return;
    }
    let mut config = json!({"command": {"greet": {"template": "Say hi to $ARGUMENTS"}}});
    // `%VAR%` below needs cmd; the Windows default would be PowerShell or `$SHELL`.
    if cfg!(windows) {
        config["shell"] = "cmd.exe".into();
    }
    let fx = Fixture::build(config, vec![], |project| {
        let write = |rel: &str, text: &str| {
            let file = project.join(rel);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, text).unwrap();
        };
        write(".opencode/plugin/hooks.js", HOOKS_PLUGIN);
        write(".opencode/tool/hello.js", HELLO_TOOL);
        write("PLUGIN.md", "PLUGIN RULES");
    })
    .await;
    plugin_settings(&fx);
    let launcher = Arc::new(FakeServer { launches: Mutex::new(0) });
    fx.engine.set_server_launcher(launcher.clone());
    assert!(fx.engine.plugin_status(&fx.project).is_none());

    let target = fx.project.join("out.txt");
    let (print, shell) = if cfg!(windows) { ("echo %PLUGIN_VAR%", "Cmd") } else { ("echo $PLUGIN_VAR", "Bash") };
    // The model uses the exposed names; hooks still see `read`, `write` and `bash`.
    fx.script.responses.lock().unwrap().extend([
        calls(&[("call_1", "Write", json!({"file_path": target.to_string_lossy(), "content": "body"}))]),
        calls(&[("call_2", "shout", json!({"text": "hey"})), ("call_3", "hello", json!({}))]),
        calls(&[("call_4", shell, json!({"command": print, "description": "print"}))]),
        text("Done."),
        text("Hi bob."),
    ]);
    let host = Arc::new(TestHost::default());
    let session = fx.prompt("go", &host).await;

    // chat.message, tool.definition, chat.params, chat.headers, system.transform and the config hook shape the request.
    let first = &fx.seen()[0];
    assert_eq!(last_user(first), "go [seen]");
    let tools = tool_names(first);
    assert!(tools.contains(&"shout".to_string()) && tools.contains(&"hello".to_string()), "{tools:?}");
    let described = |name: &str| first["tools"].as_array().unwrap().iter().find(|t| t["function"]["name"] == name).unwrap()["function"]["description"].as_str().unwrap().to_string();
    assert!(described("Read").starts_with("PLUGGED Reads a file from the local filesystem"));
    assert!(tools.contains(&shell.to_string()), "{tools:?}");
    assert_eq!(fx.tool(&session, 3).tool, "bash");
    assert_eq!(described("shout"), "Shouts the text");
    assert_eq!(first["temperature"], 0.25);
    let systems: Vec<&str> = first["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "system").filter_map(|m| m["content"].as_str()).collect();
    assert!(systems.iter().any(|s| s.contains("PLUGIN RULES")), "{systems:?}");
    assert!(systems.iter().any(|s| s.contains("EXTRA SYSTEM")), "{systems:?}");
    assert!(fx.script.heads.lock().unwrap()[0].contains("x-plugin: yes"));

    // tool.execute.before/after wrap built-ins; plugin and custom tools run in the sidecar; shell.env reaches bash.
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "body+before");
    let written = fx.tool(&session, 0);
    assert_eq!((completed(&written).output.as_str(), completed(&written).title.as_str()), ("Wrote file successfully. [after]", "retitled"));
    assert_eq!(completed(&fx.tool(&session, 1)).output, "HEY");
    assert_eq!(completed(&fx.tool(&session, 1)).metadata["truncated"], false);
    assert_eq!(completed(&fx.tool(&session, 2)).output, "hello from build");
    assert!(completed(&fx.tool(&session, 3)).output.contains("from-plugin"));
    // The same hook through the public trigger, as the server's terminals run it.
    let env = fx.engine.shell_env(&fx.project, &fx.project.to_string_lossy()).await.unwrap();
    assert_eq!(env, [("PLUGIN_VAR".to_string(), "from-plugin".to_string())]);
    let untouched = fx.engine.trigger(&fx.project, opencode_session::HookName::PermissionAsk, json!({}), json!({"status": "ask"})).await.unwrap();
    assert_eq!(untouched, json!({"status": "ask"}));
    // experimental.text.complete rewrote the stored text.
    assert_eq!(texts(&fx.rows(&session).last().unwrap().1), ["Done.!"]);

    let log = logged(&fx, "chat.params").await;
    assert!(log.starts_with("init http://127.0.0.1:9/ auth\n"), "{log}");
    assert!(log.contains("chat.message true - 1\n") && log.contains("chat.params build m1 mock user\n"), "{log}");
    assert_eq!(*launcher.launches.lock().unwrap(), 1);
    let status = fx.engine.plugin_status(&fx.project).unwrap();
    assert_eq!(status.tools, ["hello", "shout"]);
    assert_eq!(status.plugins.len(), 1);
    assert!(status.plugins[0].1.iter().any(|hook| hook == "chat.params") && status.notices.is_empty(), "{status:?}");

    // command.execute.before adds to the command's parts.
    let input = CommandInput { directory: fx.project.clone(), session_id: Some(session.id.clone()), command: "greet".into(), arguments: "bob".into(), ..Default::default() };
    fx.engine.command(input, host.clone(), CancellationToken::new()).await.unwrap();
    let rows = fx.rows(&session);
    let asked = rows.iter().rev().find(|(m, _)| m.as_user().is_some()).unwrap();
    assert_eq!(texts(&asked.1), ["Say hi to bob [seen]", "hooked greet bob"]);

    // Bus events of the directory reach the `event` hook; other directories do not.
    fx.engine.plugin_event(&fx.project.to_string_lossy(), "evt_1", "custom.event", &json!({}));
    fx.engine.plugin_event("/somewhere/else", "evt_2", "other.event", &json!({}));
    let log = logged(&fx, "event custom.event").await;
    assert!(!log.contains("other.event"));

    // Disposing the instance stops the sidecar; the next use starts a new one.
    fx.engine.dispose(&fx.project).await;
    assert!(fx.engine.plugin_status(&fx.project).is_none());
    let reloaded = fx.engine.load_plugins(&fx.project).await.unwrap().unwrap();
    assert_eq!(reloaded.tools, ["hello", "shout"]);
    fx.engine.shutdown().await;
    assert!(fx.engine.plugin_status(&fx.project).is_none());
}

const FAILING_PLUGIN: &str = r#"
export const Failing = async () => ({
  "tool.execute.before": async (input) => {
    if (input.tool === "write") throw new Error("write is forbidden here")
  },
  "chat.params": async (input) => {
    if (input.message.id && globalThis.__armed) process.exit(3)
  },
  "chat.message": async (_input, output) => {
    if (output.parts[0].text === "crash") globalThis.__armed = true
  },
})
"#;

#[tokio::test(flavor = "multi_thread")]
async fn plugin_failures_fail_the_operation_and_a_crash_drops_the_host() {
    if !bun() {
        return;
    }
    let fx = Fixture::build(json!({}), vec![], |project| {
        std::fs::create_dir_all(project.join(".opencode/plugin")).unwrap();
        std::fs::write(project.join(".opencode/plugin/failing.js"), FAILING_PLUGIN).unwrap();
    })
    .await;
    plugin_settings(&fx);
    let target = fx.project.join("out.txt");
    fx.script.responses.lock().unwrap().extend([
        calls(&[("call_1", "write", json!({"filePath": target.to_string_lossy(), "content": "body"}))]),
        text("Understood."),
        text("Still here."),
    ]);
    let host = Arc::new(TestHost::default());
    let session = fx.prompt("go", &host).await;

    // A throwing hook fails the tool call it guards; the loop goes on.
    assert!(!target.exists());
    let error = failed(&fx.tool(&session, 0)).error.clone();
    assert!(error.contains("tool.execute.before") && error.contains("write is forbidden here"), "{error}");
    assert_eq!(*host.text.lock().unwrap(), "Understood.");

    // The sidecar dying mid-hook fails that prompt, is reported once, and later prompts run without plugins.
    let crashed = fx.engine.prompt(PromptInput { session_id: Some(session.id.clone()), ..fx.input("crash") }, host.clone(), CancellationToken::new()).await;
    assert!(matches!(&crashed, Err(Error::Config(message)) if message.contains("chat.params")), "{crashed:?}");
    let status = fx.engine.plugin_status(&fx.project).unwrap();
    assert!(status.plugins.is_empty() && status.notices.len() == 1 && status.notices[0].starts_with("Plugins are disabled for this instance: "), "{status:?}");
    assert_eq!(fx.engine.status(&session.id), Status::Idle);

    fx.script.responses.lock().unwrap().push_front(text("Still here."));
    let before = fx.seen().len();
    fx.follow_up(&session, "again", &host).await;
    assert_eq!(fx.seen().len(), before + 1);
    assert!(host.text.lock().unwrap().ends_with("Still here."));
    fx.engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn uninstalled_npm_plugins_are_reported_not_installed() {
    let config = json!({"plugin": ["opencode-not-installed-plugin@1.2.3", "opencode-openai-codex-auth"]});
    let fx = Fixture::new(config, vec![text("Hello.")]).await;
    let seen: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    let sink = seen.clone();
    fx.engine.watch_events(move |event| sink.lock().unwrap().push((event.kind.clone(), event.properties.clone())));
    let host = Arc::new(TestHost::default());
    fx.prompt("hi", &host).await;
    assert_eq!(*host.text.lock().unwrap(), "Hello.");

    let notices: Vec<String> = seen.lock().unwrap().iter().filter(|(kind, _)| kind == "session.error").map(|(_, p)| p["error"]["data"]["message"].as_str().unwrap().to_string()).collect();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].starts_with("Failed to load plugin opencode-not-installed-plugin@1.2.3: package opencode-not-installed-plugin is not installed"), "{notices:?}");
    assert!(notices[0].contains("opencode_plugin::install") && notices[0].contains("bun add opencode-not-installed-plugin@1.2.3"), "{notices:?}");
    assert_eq!(seen.lock().unwrap().iter().find(|(kind, _)| kind == "session.error").unwrap().1["error"]["name"], "UnknownError");
    assert!(host.bus.lock().unwrap().iter().any(|(kind, _)| kind == "session.error"));
    assert_eq!(fx.engine.plugin_status(&fx.project).unwrap().notices, notices);
    // Nothing was fetched or created for it.
    assert!(!fx.paths.cache.join("packages").exists() && !fx.paths.cache.join("plugin-host").exists());
}


const AUTH_PLUGIN: &str = r#"
export const AuthPlugin = async () => ({
  auth: {
    provider: "mock",
    methods: [],
    loader: async (getAuth, provider) => {
      const auth = await getAuth()
      return {
        headers: { "x-loader-option": provider.id },
        async fetch(input, init) {
          const headers = new Headers(init?.headers)
          headers.set("x-plugin-fetch", auth.key)
          return fetch(input, { ...init, headers })
        },
      }
    },
  },
  provider: {
    id: "cat",
    models: async (provider, { auth }) => ({
      plugged: { ...provider.models.orig, name: `Plugged ${auth?.key}` },
    }),
  },
  "chat.headers": async (input, output) => {
    output.headers["x-agent"] = input.agent
  },
  "experimental.chat.system.transform": async (_input, output) => {
    output.system.push("TRANSFORMED")
  },
})
"#;

#[tokio::test(flavor = "multi_thread")]
async fn plugin_auth_loaders_and_model_lists_reach_the_provider() {
    if !bun() {
        return;
    }
    let delta = |content: &str| json!({"choices": [{"index": 0, "delta": {"content": content}}]});
    let stream = sse(&[delta("Hello "), delta("there."), json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": usage(20, 2)})]);
    let fx = Fixture::build(json!({}), vec![stream, text("Cat says hi.")], |project| {
        std::fs::create_dir_all(project.join(".opencode/plugin")).unwrap();
        std::fs::write(project.join(".opencode/plugin/auth.js"), AUTH_PLUGIN).unwrap();
    })
    .await;
    plugin_settings(&fx);
    let api = format!("http://127.0.0.1:{}/v1", fx.port);
    let orig = json!({"id": "orig", "name": "Original", "tool_call": true, "limit": {"context": 100000, "output": 1000}});
    let catalog = json!({"cat": {"id": "cat", "name": "Cat", "env": [], "npm": "@ai-sdk/openai-compatible", "api": api, "models": {"orig": orig}}});
    std::fs::create_dir_all(&fx.paths.cache).unwrap();
    std::fs::write(fx.paths.cache.join("models.json"), catalog.to_string()).unwrap();
    let auth = json!({"mock": {"type": "api", "key": "stored-mock"}, "cat": {"type": "api", "key": "stored-cat"}});
    std::fs::write(fx.paths.auth_file(), auth.to_string()).unwrap();

    // The loader's options and its fetch (holding the key it read through `getAuth`) carry the request; the answer streams back.
    let host = Arc::new(TestHost::default());
    let session = fx.prompt("hi", &host).await;
    fx.engine.settle().await;
    assert_eq!(*host.text.lock().unwrap(), "Hello there.");
    assert_eq!(host.deltas.lock().unwrap().len(), 2);
    let head = fx.script.heads.lock().unwrap()[0].clone();
    assert!(head.contains("x-plugin-fetch: stored-mock") && head.contains("x-loader-option: mock") && head.contains("x-agent: build"), "{head}");

    // The title request is prepared like the loop's (system transform, chat hooks) and goes through the same fetch.
    let title_head = fx.script.title_heads.lock().unwrap()[0].clone();
    assert!(title_head.contains("x-plugin-fetch: stored-mock") && title_head.contains("x-agent: title"), "{title_head}");
    let title = fx.script.titles.lock().unwrap()[0].clone();
    let systems: Vec<&str> = title["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "system").filter_map(|m| m["content"].as_str()).collect();
    assert!(systems.contains(&"TRANSFORMED"), "{systems:?}");
    assert_eq!(fx.engine.store.get_session(&session.id).unwrap().unwrap().title, "A Title");

    // `provider.models` replaced the catalog's list of `cat`; requests of providers without a loader fetch directly.
    let providers = fx.engine.providers(&fx.project).await.unwrap();
    let cat = providers.iter().find(|p| p.id == "cat").unwrap();
    assert_eq!(cat.models.iter().map(|m| (m.id.as_str(), m.name.as_str())).collect::<Vec<_>>(), [("plugged", "Plugged stored-cat")]);
    assert!(providers.iter().any(|p| p.id == "mock" && p.fetch.is_some()));
    let input = |model: &str, text: &str| PromptInput { model: Some(model.into()), ..fx.input(text) };
    let missing = fx.engine.prompt(input("cat/orig", "x"), host.clone(), CancellationToken::new()).await;
    assert!(matches!(&missing, Err(Error::Config(message)) if message.contains("Model not found: cat/orig")), "{missing:?}");
    let cat = fx.engine.prompt(input("cat/plugged", "meow"), host.clone(), CancellationToken::new()).await.unwrap();
    assert_eq!(cat.model.as_ref().map(|m| m.id.as_str()), Some("plugged"));
    assert_eq!(fx.seen().last().unwrap()["model"], "orig");
    let head = fx.script.heads.lock().unwrap().last().unwrap().clone();
    assert!(head.contains("authorization: bearer stored-cat") && !head.contains("x-plugin-fetch"), "{head}");
    fx.engine.settle().await;
    fx.engine.shutdown().await;
}

const MCP_AFTER_PLUGIN: &str = r#"
export const McpAfter = async () => ({
  "tool.execute.after": async (input, output) => {
    if (input.tool !== "demo_echo") return
    output.content.push({ type: "text", text: `raw ${Array.isArray(output.content)} ${input.args.text}` })
    output.metadata = { seen: true }
  },
})
"#;

#[tokio::test(flavor = "multi_thread")]
async fn mcp_tool_results_reach_tool_execute_after_raw() {
    if !bun() {
        return;
    }
    let config = json!({"mcp": {"demo": {"type": "local", "command": [stdio_server()]}}});
    let responses = vec![calls(&[("c1", "demo_echo", json!({"text": "hi"}))]), text("done")];
    let fx = Fixture::build(config, responses, |project| {
        std::fs::create_dir_all(project.join(".opencode/plugin")).unwrap();
        std::fs::write(project.join(".opencode/plugin/after.js"), MCP_AFTER_PLUGIN).unwrap();
    })
    .await;
    plugin_settings(&fx);
    let host = Arc::new(TestHost::default());
    let session = fx.prompt("echo", &host).await;
    // The hook saw `CallToolResult.content`; its edits shape the stored output.
    let state = completed(&fx.tool(&session, 0)).clone();
    assert_eq!(state.output, "echo: hi\n\nraw true hi");
    assert_eq!(Value::Object(state.metadata.clone()), json!({"seen": true, "truncated": false}));
    fx.engine.shutdown().await;
}

fn user_texts(body: &Value) -> Vec<String> {
    body["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "user").map(|m| m["content"].as_str().map_or_else(|| m["content"].to_string(), str::to_string)).collect()
}

#[tokio::test]
async fn copy_names_come_from_the_small_model() {
    let config = json!({"small_model": "mock/small", "provider": {"mock": {"models": {"small": {"name": "Small"}}}}});
    let fx = Fixture::new(config, vec![text("  Fix Login Bug quickly\n"), Response::Status(400, "{}".into())]).await;
    assert_eq!(fx.engine.small_model(&fx.project).await.unwrap(), Some(("mock".into(), "small".into())));

    let name = fx.engine.generate_name(&fx.project, Some("  repair the login form ")).await;
    assert_eq!(name, "fix-login-bug");
    let body = fx.seen()[0].clone();
    assert_eq!(body["model"], "small");
    assert_eq!(user_texts(&body), ["Generate a short 2-3 word name that describes this task:\nrepair the login form"]);
    // An agent without a prompt sends the provider's default system prompt.
    assert!(system_of(&body).starts_with("You are opencode, an interactive CLI tool"), "{}", system_of(&body));

    // No context asks nothing; a failed request falls back to a random slug.
    let random = fx.engine.generate_name(&fx.project, Some(" \n")).await;
    assert!(random.contains('-') && fx.seen().len() == 1, "{random}");
    let fallback = fx.engine.generate_name(&fx.project, Some("anything")).await;
    assert!(fallback.contains('-') && fx.seen().len() == 2, "{fallback}");
    assert!(fx.engine.store.list_projects().unwrap().iter().all(|p| fx.engine.store.list_sessions(&p.id).unwrap().is_empty()));
}

#[tokio::test]
async fn generate_text_retries_and_takes_an_explicit_model() {
    let fx = Fixture::new(json!({}), vec![Response::Status(503, "{}".into()), text("pong")]).await;
    let request = opencode_session::TextRequest {
        model: Some("mock/m1".into()),
        agent: "probe".into(),
        prompt: Some("Be terse.".into()),
        system: vec!["Extra.".into()],
        messages: vec!["ping".into()],
        retries: 1,
    };
    assert_eq!(fx.engine.generate_text(&fx.project, request.clone()).await.unwrap(), "pong");
    let seen = fx.seen();
    assert_eq!((seen.len(), system_of(&seen[1])), (2, "Be terse.\nExtra."));
    assert_eq!(user_texts(&seen[1]), ["ping"]);
    let missing = fx.engine.generate_text(&fx.project, opencode_session::TextRequest { model: Some("mock/none".into()), ..request }).await;
    assert!(matches!(&missing, Err(Error::Config(message)) if message.contains("Model not found")), "{missing:?}");
}

#[tokio::test]
async fn always_approvals_can_be_revoked() {
    let write = |n: u8| calls(&[("call_1", "write", json!({"filePath": format!("out{n}.txt"), "content": "x"}))]);
    let fx = Fixture::new(json!({"permission": {"edit": "ask"}}), vec![write(1), text("one"), write(2), text("two"), write(3), text("three")]).await;
    let host = Arc::new(TestHost { always: true, ..Default::default() });
    let session = fx.prompt("first", &host).await;
    assert_eq!(host.asked.lock().unwrap().len(), 1);
    assert_eq!(fx.engine.approvals(), [opencode_core::permission::Rule::new("edit", "*", opencode_core::permission::Action::Allow)]);
    // Covered by the approval: no question.
    fx.follow_up(&session, "second", &host).await;
    assert_eq!(host.asked.lock().unwrap().len(), 1);
    assert!(fx.engine.revoke_approval("edit", "*") && !fx.engine.revoke_approval("edit", "*"));
    assert!(fx.engine.approvals().is_empty());
    fx.follow_up(&session, "third", &host).await;
    assert_eq!(host.asked.lock().unwrap().len(), 2);
    fx.engine.settle().await;
}

#[tokio::test]
async fn revert_cleanup_catalog_and_directories() {
    let fx = Fixture::build(json!({}), vec![text("one"), text("two")], |project| repository(project)).await;
    let host = Arc::new(TestHost::default());
    let session = fx.prompt("first", &host).await;
    fx.follow_up(&session, "second", &host).await;
    fx.engine.settle().await;
    let rows = fx.rows(&session);
    let reverted = fx.engine.revert(&session.id, rows[2].0.id(), None).unwrap();
    fx.engine.revert_cleanup(&reverted).unwrap();
    assert_eq!(fx.rows(&session).len(), 2);
    assert!(fx.engine.store.get_session(&session.id).unwrap().unwrap().revert.is_none());

    // The directory's worktree root is among its project's directories.
    let project = fx.engine.project(&fx.project).unwrap();
    assert_ne!(project.id, "global");
    let listed = fx.engine.store.list_project_directories(&project.id).unwrap();
    assert_eq!(listed.iter().map(|d| PathBuf::from(&d.directory)).collect::<Vec<_>>(), std::slice::from_ref(&fx.project));

    // The catalog getter serves what the providers were built from.
    assert_eq!(*fx.engine.catalog_value().await, json!({}));
    // Without plugins a trigger hands its output back.
    assert!(fx.engine.shell_env(&fx.project, "x").await.unwrap().is_empty());
}

// Rust build: v2's tool input repair runs before the tool parses its arguments.
#[tokio::test]
async fn tool_input_is_repaired_before_validation() {
    let todos = json!([{"content": "ship it", "status": "pending", "priority": "high"}]).to_string();
    let responses = vec![calls(&[("c1", "todowrite", json!({"todos": todos}))]), text("done")];
    let fx = Fixture::new(json!({}), responses).await;
    let host = Arc::new(TestHost::default());
    let session = fx.prompt("plan", &host).await;
    assert!(completed(&fx.tool(&session, 0)).output.contains("ship it"));
    assert_eq!(fx.engine.todos(&session.id).unwrap()[0].content, "ship it");
}

// Rust build: a queued prompt waits for the running loop to end instead of steering it.
#[tokio::test]
async fn a_queued_prompt_waits_for_the_run_to_end() {
    let responses = vec![calls(&[("c1", "question", question("Ready?"))]), text("first done"), text("queued done")];
    let fx = Fixture::new(json!({}), responses).await;
    let host = Arc::new(TestHost { answers: Mutex::new(Some(vec![vec!["Red".into()]])), barrier: Some(tokio::sync::Barrier::new(2)), ..Default::default() });
    let (engine, input, runner) = (fx.engine.clone(), fx.input("first"), host.clone());
    let first = tokio::spawn(async move { engine.prompt(input, runner, CancellationToken::new()).await });
    host.asked_question.notified().await;
    let session_id = host.questions.lock().unwrap()[0].session_id.clone();

    let (engine, runner) = (fx.engine.clone(), host.clone());
    let input = PromptInput { session_id: Some(session_id.clone()), delivery: Delivery::Queue, ..fx.input("second") };
    let second = tokio::spawn(async move { engine.prompt(input, runner, CancellationToken::new()).await });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let users = |fx: &Fixture| fx.engine.store.list_messages(&session_id).unwrap().iter().filter(|(m, _)| m.as_user().is_some()).count();
    assert_eq!(users(&fx), 1, "a queued prompt is not stored while the session runs");
    host.barrier.as_ref().unwrap().wait().await;
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();

    let requests = fx.seen();
    assert_eq!(requests.len(), 3);
    assert_eq!(last_user(&requests[1]), "first");
    assert_eq!(last_user(&requests[2]), "second");
    assert_eq!(users(&fx), 2);
}

// Rust build: a background shell returns at once; when it ends, its notice wakes the idle session.
#[tokio::test]
async fn a_background_shell_notifies_the_session_when_it_ends() {
    // Slow enough that the run has ended before it does; a fast one is picked up by the running loop instead.
    let command = if cfg!(windows) { "ping -n 3 127.0.0.1; echo bg-hello" } else { "sleep 2; echo bg-hello" };
    let responses = vec![calls(&[("c1", "bash", json!({"command": command, "background": true}))]), text("started"), text("seen it")];
    let fx = Fixture::new(json!({}), responses).await;
    let host = Arc::new(TestHost::default());
    let session = fx.prompt("start it", &host).await;
    let output = completed(&fx.tool(&session, 0)).output.clone();
    assert!(output.starts_with("Command moved to the background (shell ID: shell_"), "{output}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while fx.seen().len() < 3 {
        assert!(std::time::Instant::now() < deadline, "the session was not woken: {:?}", fx.rows(&session).iter().map(|(_, p)| texts(p)).collect::<Vec<_>>());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let notice = last_user(&fx.seen()[2]);
    assert!(notice.starts_with("<shell id=\"shell_") && notice.contains(&format!("state=\"completed\" command=\"{command}\">")), "{notice}");
    assert!(notice.contains("bg-hello") && notice.ends_with("\n</shell>"), "{notice}");
    while fx.engine.status(&session.id) != Status::Idle || fx.rows(&session).last().unwrap().0.as_assistant().is_none_or(|a| a.finish.is_none()) {
        assert!(std::time::Instant::now() < deadline, "the woken run did not end");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(texts(&fx.rows(&session).last().unwrap().1), ["seen it"]);
}

// Rust build: `/goal` keeps the session working until the reviewer says the objective is met.
#[tokio::test]
async fn goal_continues_until_the_reviewer_is_satisfied() {
    let responses = vec![
        text("working on it"),
        text("{\"verdict\":\"continue\",\"reason\":\"tests not run\",\"next_step\":\"run the tests\"}"),
        text("tests pass"),
        text("Verdict: {\"verdict\":\"done\",\"reason\":\"tests pass in the output\"}"),
    ];
    let fx = Fixture::new(json!({}), responses).await;
    let host = Arc::new(TestHost::default());
    let command = |arguments: &str, session_id: Option<String>| CommandInput {
        directory: fx.project.clone(),
        session_id,
        command: "goal".into(),
        arguments: arguments.into(),
        ..Default::default()
    };
    let commands = fx.engine.commands(&fx.project).await.unwrap();
    assert!(commands.iter().any(|c| c.name == "goal"));
    let session = fx.engine.command(command("\"ship <it>\"", None), host.clone(), CancellationToken::new()).await.unwrap();
    assert_eq!(session.title, "ship <it>");

    let requests = fx.seen();
    assert_eq!(requests.len(), 4);
    assert!(last_user(&requests[0]).starts_with("A goal is now active for this session."));
    assert!(last_user(&requests[0]).contains("<untrusted_objective>\nship &lt;it&gt;\n</untrusted_objective>"));
    let judge = last_user(&requests[1]);
    assert!(judge.starts_with("Stop acting as the assistant. You are now the reviewer") && judge.contains("No background shells are running."), "{judge}");
    let next = last_user(&requests[2]);
    assert!(next.starts_with("Continue working toward the active goal.") && next.contains("tests not run\nNext step: run the tests"), "{next}");
    let goal = fx.engine.goal(&session.id).unwrap();
    assert_eq!((goal.status, goal.rounds, goal.reason.as_deref()), (opencode_session::GoalStatus::Complete, 1, Some("tests pass in the output")));

    // Controls: usage without arguments, pause needs an active goal, clear removes it.
    let usage = fx.engine.command(command("", Some(session.id.clone())), host.clone(), CancellationToken::new()).await.unwrap_err();
    assert_eq!(usage.to_string(), "Goal achieved: ship <it>\ntests pass in the output\nUsage: /goal <objective> | pause | resume | clear");
    let pause = fx.engine.command(command("pause", Some(session.id.clone())), host.clone(), CancellationToken::new()).await.unwrap_err();
    assert_eq!(pause.to_string(), "No active goal to pause.");
    fx.engine.command(command("clear", Some(session.id.clone())), host.clone(), CancellationToken::new()).await.unwrap();
    assert!(fx.engine.goal(&session.id).is_none());
    assert_eq!(fx.seen().len(), 4);
}

// Rust build: v2 `session.generate` (the TUI's `/btw`): an answer from the session's context that is not stored.
#[tokio::test]
async fn generate_in_session_answers_without_storing() {
    let fx = Fixture::new(json!({}), vec![text("hello"), text("a side answer")]).await;
    let host = Arc::new(TestHost::default());
    let session = fx.prompt("hi", &host).await;
    let before = fx.rows(&session).len();
    let answer = fx.engine.generate_in_session(&fx.project, &session.id, "what did I say?").await.unwrap();
    assert_eq!(answer, "a side answer");
    assert_eq!(fx.rows(&session).len(), before);
    let request = &fx.seen()[1];
    let roles: Vec<&str> = request["messages"].as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["system", "user", "assistant", "user"]);
    assert_eq!(last_user(request), "what did I say?");
}
