// Remote mode end to end: the Rust server runs in-process on a local socket with an in-memory store and a scripted
// provider; the TUI driver talks to it over HTTP and SSE only, and draws to an off-screen terminal.
use opencode_client::{Client, CreateSession, SessionPatch};
use opencode_core::paths::Paths;
use opencode_server::{Server, ServerOptions, serve_on};
use opencode_session::Engine;
use opencode_storage::Store;
use opencode_tui::app::{Action, App, Effect, Route};
use opencode_tui::dialog::Dialog;
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
use tokio::net::TcpListener;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

enum Chunk {
    Data(Value),
    Gate,
}

struct Script {
    responses: Mutex<VecDeque<Vec<Chunk>>>,
    gate: tokio::sync::Mutex<UnboundedReceiver<()>>,
    seen: Mutex<usize>,
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

async fn provider(script: Arc<Script>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
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
                        break buffer[split + 4..split + 4 + length].to_vec();
                    }
                };
                let body: Value = serde_json::from_slice(&body).unwrap_or_default();
                let title = body["messages"][0]["content"].as_str().unwrap_or("").contains("You are a title generator");
                let chunks = if title {
                    vec![delta("Remote Title"), finish("stop")]
                } else {
                    *script.seen.lock().unwrap() += 1;
                    let next = script.responses.lock().unwrap().pop_front();
                    match next {
                        Some(chunks) => chunks,
                        None => return,
                    }
                };
                let _ = socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n").await;
                for chunk in chunks {
                    match chunk {
                        Chunk::Data(value) => {
                            let _ = socket.write_all(format!("data: {value}\n\n").as_bytes()).await;
                            let _ = socket.flush().await;
                        }
                        Chunk::Gate => {
                            script.gate.lock().await.recv().await;
                        }
                    }
                }
                let _ = socket.write_all(b"data: [DONE]\n\n").await;
                let _ = socket.shutdown().await;
            });
        }
    });
    port
}

struct Fixture {
    _root: tempfile::TempDir,
    project: PathBuf,
    tui_paths: Paths,
    server: Server,
    release: UnboundedSender<()>,
    script: Arc<Script>,
}

impl Fixture {
    async fn new(config: Value, responses: Vec<Vec<Chunk>>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let canonical = root.path().canonicalize().unwrap();
        let base = PathBuf::from(canonical.to_str().unwrap().trim_start_matches(r"\\?\"));
        let project = base.join("project");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        let dirs = |name: &str| {
            let at = base.join(name);
            Paths { home: at.join("home"), data: at.join("data"), cache: at.join("cache"), config: at.join("config"), state: at.join("state"), tmp: at.join("tmp") }
        };
        let (server_paths, tui_paths) = (dirs("server"), dirs("tui"));
        std::fs::create_dir_all(&server_paths.cache).unwrap();
        // These scenarios check the original layout; the Rust build's default is the Claude look.
        std::fs::create_dir_all(&tui_paths.state).unwrap();
        std::fs::write(tui_paths.state.join("kv.json"), r#"{"rs.style":"opencode"}"#).unwrap();
        let (release, gate) = unbounded_channel();
        let script = Arc::new(Script { responses: Mutex::new(responses.into()), gate: tokio::sync::Mutex::new(gate), seen: Mutex::new(0) });
        let port = provider(script.clone()).await;
        let mut merged = json!({"model": "mock/m1", "provider": {"mock": {"options": {"baseURL": format!("http://127.0.0.1:{port}/v1"), "apiKey": "k"}, "models": {"m1": {"name": "Mock"}}}}});
        opencode_core::config::merge_deep(&mut merged, config);
        std::fs::write(project.join("opencode.json"), merged.to_string()).unwrap();
        let catalog = server_paths.cache.join("models.json");
        std::fs::write(&catalog, "{}").unwrap();
        let source = opencode_llm::catalog::Source { url: "http://127.0.0.1:9".into(), file: catalog.clone(), locks: server_paths.state.join("locks"), override_file: Some(catalog), disabled: true };
        let engine = Engine::with_store(server_paths, Store::open_in_memory().unwrap());
        engine.set_catalog_source(source);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let options = ServerOptions { directory: Some(project.to_string_lossy().into_owned()), ..ServerOptions::default() };
        let server = serve_on(listener, options, Arc::new(engine)).await.unwrap();
        Fixture { _root: root, project, tui_paths, server, release, script }
    }

    fn client(&self) -> Client {
        Client::new(&self.server.url()).with_directory(Some(self.project.to_string_lossy().into_owned()))
    }

    async fn tui(&self, options: TuiOptions) -> Tui {
        let env = |key: &str| (key == "TERM").then(|| "xterm-direct".to_string());
        let options = TuiOptions { directory: self.project.clone(), ..options };
        let (mut driver, rx) = Driver::connect(options, self.tui_paths.clone(), self.client(), &env).await.unwrap();
        driver.dispatch(Action::Resize(100, 40));
        Tui { driver, rx, terminal: Terminal::new(TestBackend::new(100, 40)).unwrap() }
    }
}

struct Tui {
    driver: Driver,
    rx: UnboundedReceiver<Action>,
    terminal: Terminal<TestBackend>,
}

impl Tui {
    fn app(&self) -> &App {
        &self.driver.app
    }

    fn press(&mut self, code: KeyCode) {
        self.driver.dispatch(Action::Key(KeyEvent::new(code, KeyModifiers::NONE)));
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
        (0..area.height).map(|y| (0..area.width).map(|x| buffer[(x, y)].symbol()).collect::<String>().trim_end().to_string()).collect::<Vec<_>>().join("\n")
    }

    async fn pump(&mut self, what: &str, done: impl Fn(&App) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while !done(&self.driver.app) {
            let action = tokio::time::timeout_at(deadline, self.rx.recv()).await.unwrap_or_else(|_| panic!("timed out waiting for {what}")).expect("driver channel closed");
            self.driver.dispatch(action);
        }
    }

    async fn settle(&mut self) {
        self.pump("the run to finish", |app| app.running.is_empty() && app.session().is_some_and(|view| !view.busy() && view.live.is_empty())).await;
    }
}

fn live_text(app: &App) -> String {
    app.session().map(|view| view.live.iter().map(|live| format!("{live:?}")).collect::<String>()).unwrap_or_default()
}

#[tokio::test]
async fn streams_text_and_answers_a_permission_over_http() {
    let write = json!({"filePath": "a.txt", "content": "over http"});
    let responses = vec![vec![delta("Hel"), Chunk::Gate, delta("lo **there**"), call("c1", "write", write), finish("tool_calls")], vec![delta("All done."), finish("stop")]];
    let fx = Fixture::new(json!({"permission": {"edit": "ask"}}), responses).await;
    let mut tui = fx.tui(TuiOptions::default()).await;
    let home = tui.frame();
    assert!(home.contains("Build · Mock"), "{home}");

    tui.type_text("write it");
    tui.press(KeyCode::Enter);
    // The session is created on the server first; the prompt shows as sent once it exists.
    tui.pump("the new session", |app| matches!(app.route, Route::Session(_))).await;
    let sent = tui.frame();
    assert!(sent.contains("┃  write it") && sent.contains("esc interrupt"), "{sent}");

    tui.pump("the first delta", |app| live_text(app).contains("Hel")).await;
    let streaming = tui.frame();
    assert!(streaming.contains("   Hel") && !streaming.contains("Hello"), "{streaming}");
    fx.release.send(()).unwrap();

    tui.pump("the permission prompt", |app| app.permission().is_some()).await;
    let asking = tui.frame();
    assert!(asking.contains("△ Permission required") && asking.contains("→ Edit a.txt"), "{asking}");
    assert!(asking.contains("Allow once   Allow always   Reject"), "{asking}");
    assert_eq!(tui.app().permission().unwrap().session_id, tui.app().session().unwrap().info.id);
    assert!(!fx.project.join("a.txt").exists());
    tui.press(KeyCode::Enter);
    assert!(tui.app().permission().is_none());

    tui.settle().await;
    assert_eq!(std::fs::read_to_string(fx.project.join("a.txt")).unwrap(), "over http");
    tui.pump("the title", |app| app.session().unwrap().info.title == "Remote Title").await;
    let done = tui.frame();
    for needle in ["# Remote Title", "┃  write it", "   Hello there", "a.txt", "   All done.", "▣  Build · Mock"] {
        assert!(done.contains(needle), "missing {needle:?}\n{done}");
    }
    assert!(!done.contains("Permission required") && !done.contains("esc interrupt"), "{done}");
    assert_eq!(done.matches("Hello there").count(), 1, "{done}");
    assert_eq!(*fx.script.seen.lock().unwrap(), 2);
    drop(tui);
}

#[tokio::test]
async fn auto_mode_and_rejections_go_to_the_server() {
    let first = json!({"filePath": "auto.txt", "content": "approved"});
    let second = json!({"filePath": "nope.txt", "content": "rejected"});
    let responses = vec![vec![call("c1", "write", first), finish("tool_calls")], vec![delta("ok"), finish("stop")], vec![call("c2", "write", second), finish("tool_calls")]];
    let fx = Fixture::new(json!({"permission": {"edit": "ask"}}), responses).await;
    let mut tui = fx.tui(TuiOptions { auto: true, ..Default::default() }).await;
    assert!(tui.frame().contains("Build auto · Mock"));
    tui.type_text("auto write");
    tui.press(KeyCode::Enter);
    tui.pump("the new session", |app| matches!(app.route, Route::Session(_))).await;
    tui.settle().await;
    assert_eq!(std::fs::read_to_string(fx.project.join("auto.txt")).unwrap(), "approved");
    assert!(tui.app().permissions.is_empty());

    tui.driver.app.run("permission_mode");
    tui.type_text("write again");
    tui.press(KeyCode::Enter);
    tui.pump("the permission prompt", |app| app.permission().is_some()).await;
    tui.press(KeyCode::Esc);
    tui.settle().await;
    assert!(!fx.project.join("nope.txt").exists());
    assert!(fx.client().permissions().await.unwrap().is_empty());
}

#[tokio::test]
async fn lists_opens_forks_and_renames_sessions_on_the_server() {
    let fx = Fixture::new(json!({}), vec![vec![delta("first answer"), finish("stop")]]).await;
    let client = fx.client();
    let older = client.create_session(&CreateSession { title: Some("Older".into()), ..Default::default() }).await.unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    let newer = client.create_session(&CreateSession { title: Some("Newer".into()), ..Default::default() }).await.unwrap();
    client.create_session(&CreateSession { parent_id: Some(newer.id.clone()), title: Some("Child".into()), ..Default::default() }).await.unwrap();

    let mut tui = fx.tui(TuiOptions::default()).await;
    tui.driver.execute(Effect::ListSessions);
    tui.pump("the session list", |app| matches!(&app.dialog, Some(Dialog::Select(_)))).await;
    let Some(Dialog::Select(select)) = &tui.app().dialog else { unreachable!() };
    assert_eq!(select.items.iter().map(|item| item.value.clone()).collect::<Vec<_>>(), vec![newer.id.clone(), older.id.clone()]);
    let listing = tui.frame();
    assert!(listing.contains("Sessions") && listing.contains("Newer") && listing.contains("Older") && !listing.contains("Child"), "{listing}");
    tui.press(KeyCode::Down);
    tui.press(KeyCode::Enter);
    tui.pump("the opened session", |app| app.session().is_some_and(|view| view.info.id == older.id)).await;
    assert!(tui.frame().contains("# Older"));

    tui.type_text("hi");
    tui.press(KeyCode::Enter);
    tui.settle().await;
    assert!(tui.frame().contains("   first answer"));

    // Renamed by another client: the title follows through the event stream.
    client.update_session(&older.id, &SessionPatch { title: Some("From elsewhere".into()) }).await.unwrap();
    tui.pump("the rename", |app| app.session().unwrap().info.title == "From elsewhere").await;

    // `--continue --fork`: a copy of the newest root session opens.
    let mut forked = fx.tui(TuiOptions { continue_last: true, fork: true, ..Default::default() }).await;
    forked.pump("the fork", |app| app.session().is_some()).await;
    let view = forked.app().session().unwrap();
    assert!(view.info.id != older.id && view.info.id != newer.id);
    assert!(view.messages.iter().any(|(_, parts)| parts.iter().any(|p| matches!(&p.body, opencode_storage::PartBody::Text(t) if t.text == "first answer"))));

    let mut missing = fx.tui(TuiOptions { session: Some("ses_missing".into()), ..Default::default() }).await;
    missing.pump("the error", |app| app.toast.is_some()).await;
    assert!(missing.app().toast.as_ref().unwrap().message.contains("Session not found"), "{:?}", missing.app().toast);
}
