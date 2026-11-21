// End to end: the real engine talks to a scripted provider on a local socket while the TUI driver reduces its
// updates and draws frames to an off-screen terminal. Temp dirs and an in-memory store only.
use opencode_core::paths::Paths;
use opencode_storage::Store;
use opencode_tui::app::{Action, App, Route};
use opencode_tui::runtime::{Driver, TuiOptions};
use opencode_tui::view;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

enum Chunk {
    Data(Value),
    // The stream stalls here until the test releases it.
    Gate,
}

struct Script {
    responses: Mutex<VecDeque<Vec<Chunk>>>,
    gate: tokio::sync::Mutex<UnboundedReceiver<()>>,
    seen: Mutex<Vec<Value>>,
}

fn delta(content: &str) -> Chunk {
    Chunk::Data(json!({"choices": [{"index": 0, "delta": {"content": content}}]}))
}

fn finish(reason: &str) -> Chunk {
    Chunk::Data(json!({"choices": [{"index": 0, "delta": {}, "finish_reason": reason}], "usage": {"prompt_tokens": 20, "completion_tokens": 4, "total_tokens": 24}}))
}

fn call(id: &str, name: &str, arguments: Value) -> Chunk {
    Chunk::Data(json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": id, "type": "function", "function": {"name": name, "arguments": arguments.to_string()}}]}}]}))
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
                let body = loop {
                    let mut chunk = [0u8; 8192];
                    let n = socket.read(&mut chunk).await.unwrap();
                    buffer.extend_from_slice(&chunk[..n]);
                    let Some(split) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                    let head = String::from_utf8_lossy(&buffer[..split]).to_lowercase();
                    let length: usize = head.lines().find_map(|l| l.strip_prefix("content-length:")).map_or(0, |v| v.trim().parse().unwrap());
                    if buffer.len() >= split + 4 + length {
                        break buffer[split + 4..split + 4 + length].to_vec();
                    }
                };
                let body: Value = serde_json::from_slice(&body).unwrap();
                let title = body["messages"][0]["content"].as_str().unwrap_or("").contains("You are a title generator");
                let chunks = if title {
                    vec![delta("Streamed Title"), finish("stop")]
                } else {
                    script.seen.lock().unwrap().push(body);
                    let next = script.responses.lock().unwrap().pop_front();
                    match next {
                        Some(chunks) => chunks,
                        None => return std::future::pending().await,
                    }
                };
                // No content length: the body ends when the connection closes, so chunks reach the client as written.
                socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n").await.unwrap();
                for chunk in chunks {
                    match chunk {
                        Chunk::Data(value) => {
                            socket.write_all(format!("data: {value}\n\n").as_bytes()).await.unwrap();
                            socket.flush().await.unwrap();
                        }
                        Chunk::Gate => {
                            script.gate.lock().await.recv().await;
                        }
                    }
                }
                socket.write_all(b"data: [DONE]\n\n").await.unwrap();
                socket.shutdown().await.ok();
            });
        }
    });
    port
}

struct Fixture {
    _root: tempfile::TempDir,
    project: PathBuf,
    script: Arc<Script>,
    release: UnboundedSender<()>,
    driver: Driver,
    rx: UnboundedReceiver<Action>,
    terminal: Terminal<TestBackend>,
}

impl Fixture {
    async fn new(config: Value, responses: Vec<Vec<Chunk>>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let canonical = root.path().canonicalize().unwrap();
        let base = PathBuf::from(canonical.to_str().unwrap().trim_start_matches(r"\\?\"));
        let project = base.join("project");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        let paths = Paths { home: base.join("home"), data: base.join("data"), cache: base.join("cache"), config: base.join("config"), state: base.join("state"), tmp: base.join("tmp") };
        std::fs::create_dir_all(&paths.data).unwrap();
        // These scenarios check the original's tool rows; the Rust build's default draws them Claude-style.
        std::fs::create_dir_all(&paths.state).unwrap();
        std::fs::write(paths.state.join("kv.json"), r#"{"rs.style":"opencode"}"#).unwrap();
        let (release, gate) = unbounded_channel();
        let script = Arc::new(Script { responses: Mutex::new(responses.into()), gate: tokio::sync::Mutex::new(gate), seen: Mutex::new(Vec::new()) });
        let port = serve(script.clone()).await;
        let mut merged = json!({
            "model": "mock/m1",
            "provider": {"mock": {"options": {"baseURL": format!("http://127.0.0.1:{port}/v1"), "apiKey": "k"}, "models": {"m1": {"name": "Mock"}}}}
        });
        opencode_core::config::merge_deep(&mut merged, config);
        std::fs::write(project.join("opencode.json"), merged.to_string()).unwrap();
        let options = TuiOptions { directory: project.clone(), ..Default::default() };
        // Only a terminal type, so glyph and colour fallbacks stay off on every platform.
        let env = |key: &str| (key == "TERM").then(|| "xterm-direct".to_string());
        let (mut driver, rx) = Driver::new(options, paths, Some(Store::open_in_memory().unwrap()), &env).unwrap();
        driver.dispatch(Action::Resize(100, 40));
        Self { _root: root, project, script, release, driver, rx, terminal: Terminal::new(TestBackend::new(100, 40)).unwrap() }
    }

    fn app(&self) -> &App {
        &self.driver.app
    }

    fn key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.driver.dispatch(Action::Key(KeyEvent::new(code, modifiers)));
    }

    fn press(&mut self, code: KeyCode) {
        self.key(code, KeyModifiers::NONE);
    }

    fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            self.press(KeyCode::Char(c));
        }
    }

    fn frame(&mut self) -> String {
        let app = &self.driver.app;
        self.terminal.draw(|frame| view::render(frame, app)).unwrap();
        let buffer = self.terminal.backend().buffer();
        let area = buffer.area;
        let text = (0..area.height).map(|y| (0..area.width).map(|x| buffer[(x, y)].symbol()).collect::<String>().trim_end().to_string()).collect::<Vec<_>>().join("\n");
        // `TUI_DUMP=1 cargo test -p opencode-tui --test stream -- --nocapture` shows every asserted frame.
        if std::env::var_os("TUI_DUMP").is_some() {
            println!("{}\n{text}\n", "=".repeat(100));
        }
        text
    }

    // Feeds engine updates into the reducer until `done` holds.
    async fn pump(&mut self, what: &str, done: impl Fn(&App) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while !done(&self.driver.app) {
            let action = tokio::time::timeout_at(deadline, self.rx.recv()).await.unwrap_or_else(|_| panic!("timed out waiting for {what}")).expect("driver channel closed");
            self.driver.dispatch(action);
        }
    }

    async fn settle(&mut self) {
        self.pump("the run to finish", |app| app.running.is_empty()).await;
        // The reload asked for by the last update has run synchronously; nothing is left in flight.
        assert!(!self.app().session().unwrap().busy());
    }
}

fn live_text(app: &App) -> String {
    app.session().map(|view| view.live.iter().map(|live| format!("{live:?}")).collect::<String>()).unwrap_or_default()
}

#[tokio::test]
async fn streams_text_asks_permission_and_completes() {
    let write = json!({"filePath": "a.txt", "content": "streamed"});
    let responses = vec![vec![delta("Hel"), Chunk::Gate, delta("lo **there**"), call("c1", "write", write), finish("tool_calls")], vec![delta("All "), delta("done."), finish("stop")]];
    let mut fx = Fixture::new(json!({"permission": {"edit": "ask"}}), responses).await;
    let home = fx.frame();
    assert!(home.contains("Build · Mock"), "{home}");

    fx.type_text("write it");
    fx.press(KeyCode::Enter);
    assert!(matches!(fx.app().route, Route::Session(_)));
    let sent = fx.frame();
    assert!(sent.contains("┃  write it") && sent.contains("esc interrupt"), "{sent}");

    // Mid-stream: the first delta is on screen while the provider is still holding the rest back.
    fx.pump("the first delta", |app| live_text(app).contains("Hel")).await;
    let streaming = fx.frame();
    assert!(streaming.contains("   Hel") && !streaming.contains("Hello"), "{streaming}");
    assert!(streaming.contains("┃  write it") && streaming.contains("esc interrupt"), "{streaming}");
    assert_eq!(streaming.matches("write it").count(), 1, "the prompt is drawn once\n{streaming}");
    fx.release.send(()).unwrap();

    // The tool call stops at the permission prompt, which replaces the input.
    fx.pump("the permission prompt", |app| app.permission().is_some()).await;
    let asking = fx.frame();
    assert!(asking.contains("   Hello there"), "{asking}");
    assert!(asking.contains("△ Permission required") && asking.contains("→ Edit a.txt"), "{asking}");
    assert!(asking.contains("1 + streamed"), "{asking}");
    assert!(asking.contains("Allow once   Allow always   Reject") && !asking.contains("ctrl+p commands"), "{asking}");
    assert!(!fx.project.join("a.txt").exists());
    assert_eq!(fx.app().permission().unwrap().session_id, fx.app().session().unwrap().info.id);
    fx.press(KeyCode::Enter);
    assert!(fx.app().permission().is_none());

    fx.settle().await;
    assert_eq!(std::fs::read_to_string(fx.project.join("a.txt")).unwrap(), "streamed");
    // The title is generated beside the loop and may land after it.
    fx.pump("the title", |app| app.session().unwrap().info.title == "Streamed Title").await;
    let done = fx.frame();
    for needle in ["# Streamed Title", "┃  write it", "   Hello there", "a.txt", "   All done.", "▣  Build · Mock", "ctrl+p commands"] {
        assert!(done.contains(needle), "missing {needle:?}\n{done}");
    }
    assert!(done.contains("# Wrote a.txt") || done.contains("← Write a.txt"), "{done}");
    assert!(!done.contains("Permission required") && !done.contains("esc interrupt"), "{done}");
    assert_eq!(done.matches("Hello there").count(), 1, "{done}");
    assert_eq!(done.matches("All done.").count(), 1, "{done}");
    let view = fx.app().session().unwrap();
    assert!(view.live.is_empty() && view.live_parts.is_empty() && view.pending_user.is_none());
    assert!(view.messages.len() >= 3, "{}", view.messages.len());
    assert_eq!(fx.script.seen.lock().unwrap().len(), 2);

    // A second turn in the same session streams under the first.
    fx.script.responses.lock().unwrap().push_back(vec![delta("Second "), Chunk::Gate, delta("answer"), finish("stop")]);
    fx.type_text("again");
    fx.press(KeyCode::Enter);
    fx.pump("the second stream", |app| live_text(app).contains("Second")).await;
    let streaming = fx.frame();
    assert!(streaming.contains("   All done.") && streaming.contains("┃  again") && streaming.contains("   Second"), "{streaming}");
    fx.release.send(()).unwrap();
    fx.settle().await;
    let done = fx.frame();
    assert!(done.contains("   Second answer") && done.matches("again").count() == 1, "{done}");
}

#[tokio::test]
async fn rejecting_a_permission_and_interrupting_a_stream() {
    let write = json!({"filePath": "b.txt", "content": "nope"});
    // A rejection ends the loop: the provider is not asked again for that turn.
    let responses = vec![vec![call("c1", "write", write), finish("tool_calls")], vec![delta("Thinking"), Chunk::Gate, delta(" forever"), finish("stop")]];
    let mut fx = Fixture::new(json!({"permission": {"edit": "ask"}}), responses).await;
    fx.type_text("write b");
    fx.press(KeyCode::Enter);
    fx.pump("the permission prompt", |app| app.permission().is_some()).await;
    fx.press(KeyCode::Esc);
    fx.settle().await;
    assert!(!fx.project.join("b.txt").exists());
    assert_eq!(fx.script.seen.lock().unwrap().len(), 1);
    let rejected = fx.frame();
    assert!(rejected.contains("Write b.txt") && !rejected.contains("Permission required"), "{rejected}");

    fx.type_text("think");
    fx.press(KeyCode::Enter);
    fx.pump("the stream", |app| live_text(app).contains("Thinking")).await;
    fx.press(KeyCode::Esc);
    assert!(fx.frame().contains("esc again to interrupt"));
    fx.press(KeyCode::Esc);
    fx.settle().await;
    let stopped = fx.frame();
    assert!(stopped.contains("interrupted") && !stopped.contains("forever") && stopped.contains("ctrl+p commands"), "{stopped}");
}

#[tokio::test]
async fn shell_mode_runs_a_command_through_the_engine() {
    let mut fx = Fixture::new(json!({}), Vec::new()).await;
    fx.type_text("!echo shell-ok");
    assert!(fx.app().shell_mode && fx.app().editor.text == "echo shell-ok");
    let typing = fx.frame();
    assert!(typing.contains("┃  echo shell-ok") && typing.contains("Shell · Mock") && typing.contains("esc exit shell mode"), "{typing}");
    fx.press(KeyCode::Enter);
    assert!(matches!(fx.app().route, Route::Session(_)) && !fx.app().shell_mode);
    fx.settle().await;
    let done = fx.frame();
    assert!(done.contains("$ echo shell-ok") && done.matches("shell-ok").count() >= 2, "{done}");
    assert!(done.contains("ctrl+p commands") && !done.contains("esc interrupt"), "{done}");
    // The provider was never asked: a shell command is not a model turn.
    assert!(fx.script.seen.lock().unwrap().is_empty());
    let state = fx.project.parent().unwrap().join("state");
    assert_eq!(std::fs::read_to_string(state.join("prompt-history.jsonl")).unwrap(), "{\"input\":\"echo shell-ok\",\"parts\":[],\"mode\":\"shell\"}\n");
}

#[tokio::test]
async fn forks_reverts_and_restores_through_the_engine() {
    let responses = vec![vec![delta("first answer"), finish("stop")], vec![delta("second answer"), finish("stop")]];
    let mut fx = Fixture::new(json!({}), responses).await;
    for prompt in ["one", "two"] {
        fx.type_text(prompt);
        fx.press(KeyCode::Enter);
        fx.settle().await;
    }
    let original = fx.app().session().unwrap().info.id.clone();
    assert!(fx.frame().contains("   second answer"));

    // Timeline -> newest message -> Revert: the turn disappears and its text returns to the prompt.
    fx.key(KeyCode::Char('x'), KeyModifiers::CONTROL);
    fx.press(KeyCode::Char('g'));
    let timeline = fx.frame();
    assert!(timeline.contains("Timeline") && timeline.contains("two") && timeline.contains("one"), "{timeline}");
    fx.press(KeyCode::Enter);
    assert!(fx.frame().contains("Message Actions"));
    fx.press(KeyCode::Enter);
    fx.pump("the revert", |app| app.session().unwrap().info.revert.is_some()).await;
    let reverted = fx.frame();
    assert!(!reverted.contains("second answer") && reverted.contains("   first answer") && reverted.contains("┃  two"), "{reverted}");
    assert!(reverted.contains("reverted"), "{reverted}");
    assert_eq!(fx.app().editor.text, "two");
    // <leader>r brings it back.
    fx.key(KeyCode::Char('u'), KeyModifiers::CONTROL);
    fx.key(KeyCode::Char('x'), KeyModifiers::CONTROL);
    fx.press(KeyCode::Char('r'));
    fx.pump("the restore", |app| app.session().unwrap().info.revert.is_none()).await;
    assert!(fx.frame().contains("   second answer"));

    // Fork from the second message: a new session holding the first turn, with the prompt prefilled.
    fx.type_text("/fork");
    fx.press(KeyCode::Enter);
    let forking = fx.frame();
    assert!(forking.contains("Fork session") && forking.contains("Full session"), "{forking}");
    fx.press(KeyCode::Down);
    fx.press(KeyCode::Enter);
    let forked = fx.app().session().unwrap();
    assert_ne!(forked.info.id, original);
    assert_eq!(fx.app().editor.text, "two");
    let frame = fx.frame();
    assert!(frame.contains("   first answer") && !frame.contains("second answer") && frame.contains("┃  two"), "{frame}");
}

#[tokio::test]
async fn answers_a_question_and_follows_a_subagent() {
    let question = json!({"questions": [{"question": "Which color?", "header": "Pick", "options": [{"label": "Red", "description": "warm"}, {"label": "Blue", "description": "cold"}]}]});
    let task = json!({"description": "Find things", "prompt": "look around", "subagent_type": "general"});
    let responses = vec![
        vec![call("c1", "question", question), finish("tool_calls")],
        vec![call("c2", "task", task), finish("tool_calls")],
        vec![delta("child "), Chunk::Gate, delta("says hi"), finish("stop")],
        vec![delta("parent done"), finish("stop")],
    ];
    let mut fx = Fixture::new(json!({}), responses).await;
    fx.type_text("ask then delegate");
    fx.press(KeyCode::Enter);

    fx.pump("the question", |app| app.question_ask().is_some()).await;
    let asking = fx.frame();
    for needle in ["Which color?", "1. Red", "warm", "2. Blue", "3. Type your own answer", "↑↓ select", "enter submit", "esc dismiss"] {
        assert!(asking.contains(needle), "missing {needle:?}\n{asking}");
    }
    assert!(!asking.contains("ctrl+p commands"), "{asking}");
    fx.press(KeyCode::Down);
    fx.press(KeyCode::Enter);
    assert!(fx.app().question_ask().is_none());

    // The subagent stalls mid-answer: its session exists and the parent's task row is running.
    fx.pump("the subagent", |app| app.session().is_some_and(|view| !view.children.is_empty())).await;
    let parent_id = fx.app().session().unwrap().info.id.clone();
    let child_id = fx.app().session().unwrap().children[0].info.id.clone();
    // Its first words arrive while the parent is on screen; they are not the parent's text.
    let seen = fx.script.clone();
    fx.pump("the child request", move |_| seen.seen.lock().unwrap().len() == 3).await;
    let delegating = fx.frame();
    assert!(delegating.contains("# Questions") && delegating.contains("Which color?") && delegating.contains("Blue"), "{delegating}");
    assert!(delegating.contains("│ General Task — Find things"), "{delegating}");
    assert!(!delegating.contains("   child"), "{delegating}");

    // <leader>down opens the child session while it runs, with what it has streamed so far.
    fx.key(KeyCode::Char('x'), KeyModifiers::CONTROL);
    fx.press(KeyCode::Down);
    assert_eq!(fx.app().session().unwrap().info.id, child_id);
    fx.pump("the child stream", |app| live_text(app).contains("child")).await;
    let child = fx.frame();
    assert!(child.contains("   child") && child.contains("General (1 of 1)") && child.contains("Parent up"), "{child}");
    assert!(!child.contains("ctrl+p commands"), "{child}");
    fx.type_text("typed into nothing");
    assert!(fx.app().editor.is_empty());
    fx.release.send(()).unwrap();
    fx.pump("the child to finish", |app| {
        let view = app.session().unwrap();
        view.live.is_empty() && view.messages.iter().any(|(_, parts)| parts.iter().any(|part| matches!(&part.body, opencode_storage::PartBody::Text(text) if text.text == "child says hi")))
    })
    .await;
    assert!(fx.frame().contains("   child says hi"));

    fx.press(KeyCode::Up);
    assert_eq!(fx.app().session().unwrap().info.id, parent_id);
    fx.settle().await;
    let done = fx.frame();
    for needle in ["# Questions", "✓ General Task — Find things", "↳ ", "   parent done", "▣  Build · Mock"] {
        assert!(done.contains(needle), "missing {needle:?}\n{done}");
    }
    assert_eq!(fx.script.seen.lock().unwrap().len(), 4);
}
