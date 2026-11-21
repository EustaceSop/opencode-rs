// Workspaces, sync and session moves over HTTP with `OPENCODE_EXPERIMENTAL_WORKSPACES` on (set once, before anything
// else in this binary reads the environment); temp git repos, tempdir paths, catalog fetching disabled.
use opencode_core::paths::Paths;
use opencode_server::{Server, ServerOptions, serve_on};
use opencode_session::Engine;
use opencode_storage::Store;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

fn init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: every test calls this before it starts a runtime thread or reads the environment.
    ONCE.call_once(|| unsafe { std::env::set_var("OPENCODE_EXPERIMENTAL_WORKSPACES", "true") });
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

fn encode(value: &str) -> String {
    value.bytes().map(|b| if b.is_ascii_alphanumeric() { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

struct Root {
    _dir: tempfile::TempDir,
    base: PathBuf,
}

// A temp dir (long, plain path) holding a git repository `project` with one commit.
fn root() -> Root {
    let dir = tempfile::tempdir().unwrap();
    let canonical = dir.path().canonicalize().unwrap();
    let base = PathBuf::from(canonical.to_str().unwrap().trim_start_matches(r"\\?\"));
    let project = base.join("project");
    std::fs::create_dir_all(&project).unwrap();
    git(&project, &["init", "-q"]);
    std::fs::write(project.join("keep.txt"), "one\n").unwrap();
    git(&project, &["add", "keep.txt"]);
    git(&project, &["commit", "-q", "-m", "init"]);
    Root { _dir: dir, base }
}

struct Fixture {
    project: PathBuf,
    engine: Arc<Engine>,
    server: Server,
    http: reqwest::Client,
}

async fn serve(root: &Root, name: &str, store: Store) -> Fixture {
    let base = root.base.join(name);
    let paths = Paths {
        home: base.join("home"),
        data: base.join("data"),
        cache: base.join("cache"),
        config: base.join("config"),
        state: base.join("state"),
        tmp: base.join("tmp"),
    };
    std::fs::create_dir_all(&paths.cache).unwrap();
    std::fs::create_dir_all(&paths.data).unwrap();
    let file = paths.cache.join("models.json");
    std::fs::write(&file, "{}").unwrap();
    let source = opencode_llm::catalog::Source { url: "http://127.0.0.1:9".into(), file: file.clone(), locks: paths.state.join("locks"), override_file: Some(file), disabled: true };
    let engine = Arc::new(Engine::with_store(paths, store));
    engine.set_catalog_source(source);
    let project = root.base.join("project");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let options = ServerOptions { directory: Some(project.to_string_lossy().into_owned()), ..ServerOptions::default() };
    let server = serve_on(listener, options, engine.clone()).await.unwrap();
    Fixture { project, engine, server, http: reqwest::Client::new() }
}

impl Fixture {
    fn url(&self, path: &str, directory: &Path) -> String {
        let separator = if path.contains('?') { '&' } else { '?' };
        format!("{}{path}{separator}directory={}", self.server.url(), encode(&directory.to_string_lossy()))
    }

    async fn send_in(&self, method: reqwest::Method, path: &str, body: Option<Value>, directory: &Path) -> (u16, Value) {
        let mut request = self.http.request(method, self.url(path, directory));
        if let Some(body) = body {
            request = request.header("content-type", "application/json").body(body.to_string());
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        (status, serde_json::from_str(&response.text().await.unwrap()).unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        self.send_in(reqwest::Method::GET, path, None, &self.project).await
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        self.send_in(reqwest::Method::POST, path, Some(body), &self.project).await
    }

    async fn delete(&self, path: &str) -> (u16, Value) {
        self.send_in(reqwest::Method::DELETE, path, None, &self.project).await
    }

    async fn session(&self, directory: &Path) -> String {
        let (status, created) = self.send_in(reqwest::Method::POST, "/session", Some(json!({})), directory).await;
        assert_eq!(status, 200, "{created}");
        created["id"].as_str().unwrap().to_string()
    }

    async fn stop(self) {
        self.server.shutdown().await;
    }
}

async fn eventually(test: impl Fn() -> bool) {
    for _ in 0..200 {
        if test() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("condition never held");
}

#[tokio::test(flavor = "multi_thread")]
async fn workspaces_follow_the_worktree_adapter() {
    init();
    let root = root();
    let f = serve(&root, "a", Store::open_in_memory().unwrap()).await;
    assert_eq!(f.get("/experimental/workspace/adapter").await.1, json!([{"type": "worktree", "name": "Worktree", "description": "Create a git worktree"}]));
    let (status, body) = f.post("/experimental/workspace", json!({"type": "nope"})).await;
    assert_eq!((status, body), (400, json!({"name": "WorkspaceCreateError", "data": {"message": "Unknown workspace adapter: nope"}})));
    assert_eq!(f.post("/experimental/workspace", json!({})).await.0, 400);

    let (status, info) = f.post("/experimental/workspace", json!({"type": "worktree", "extra": {"k": 1}})).await;
    assert_eq!(status, 200, "{info}");
    let id = info["id"].as_str().unwrap().to_string();
    let directory = PathBuf::from(info["directory"].as_str().unwrap());
    assert!(id.starts_with("wrk_") && info["timeUsed"].is_u64() && info["branch"].is_null() && info["extra"] == json!({"k": 1}), "{info}");
    assert!(directory.starts_with(f.engine.paths.data.join("worktree")) && directory.ends_with(info["name"].as_str().unwrap()), "{info}");
    assert_eq!(f.get("/experimental/workspace").await.1, json!([info]));
    assert_eq!(f.get("/experimental/workspace/status").await.1, json!([{"workspaceID": id, "status": "connected"}]));
    assert_eq!(f.post("/sync/start", json!({})).await, (200, json!(true)));
    eventually(|| directory.join("keep.txt").exists()).await;

    // A session warps into the workspace and back, taking the workspace's changes with it.
    let session = f.session(&f.project).await;
    assert_eq!(f.post("/experimental/workspace/warp", json!({"id": id, "sessionID": session})).await.0, 204);
    assert_eq!(f.get(&format!("/session/{session}")).await.1["workspaceID"], json!(id));
    std::fs::write(directory.join("keep.txt"), "one\ntwo\n").unwrap();
    assert_eq!(f.post("/experimental/workspace/warp", json!({"id": null, "sessionID": session, "copyChanges": true})).await.0, 204);
    assert_eq!(std::fs::read_to_string(f.project.join("keep.txt")).unwrap().replace("\r\n", "\n"), "one\ntwo\n");
    assert!(f.get(&format!("/session/{session}")).await.1.get("workspaceID").is_none());
    let (status, body) = f.post("/experimental/workspace/warp", json!({"id": "wrk_missing", "sessionID": session})).await;
    assert_eq!((status, body), (404, json!({"name": "NotFoundError", "data": {"message": "Workspace not found: wrk_missing"}})));
    assert_eq!(f.post("/experimental/workspace/warp", json!({})).await.0, 400);

    // Worktrees made elsewhere become workspaces once.
    let manual = root.base.join("manual");
    git(&f.project, &["worktree", "add", "--detach", &manual.to_string_lossy(), "HEAD"]);
    assert_eq!(f.post("/experimental/workspace/sync-list", json!({})).await.0, 204);
    assert_eq!(f.post("/experimental/workspace/sync-list", json!({})).await.0, 204);
    let listed = f.get("/experimental/workspace").await.1;
    let names: Vec<&str> = listed.as_array().unwrap().iter().map(|w| w["name"].as_str().unwrap()).collect();
    assert_eq!((names.len(), names.contains(&"manual")), (2, true), "{listed}");

    // Removing a workspace removes its sessions and its worktree.
    assert_eq!(f.post("/experimental/workspace/warp", json!({"id": id, "sessionID": session})).await.0, 204);
    let (status, removed) = f.delete(&format!("/experimental/workspace/{id}")).await;
    assert_eq!((status, &removed["id"]), (200, &json!(id)));
    assert_eq!(f.get(&format!("/session/{session}")).await.0, 404);
    assert!(!directory.exists());
    assert_eq!(f.get("/experimental/workspace").await.1.as_array().unwrap().len(), 1);
    assert_eq!(f.delete("/experimental/workspace/wrk_gone").await, (200, Value::Null));
    assert_eq!(f.delete("/experimental/workspace/nope").await.0, 400);
    f.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_history_replays_into_another_server_and_survives_restarts() {
    init();
    let root = root();
    let database = root.base.join("events.db");
    let a = serve(&root, "a", Store::open(&database).unwrap()).await;
    let session = a.session(&a.project).await;
    assert_eq!(a.send_in(reqwest::Method::PATCH, &format!("/session/{session}"), Some(json!({"title": "Synced"})), &a.project).await.0, 200);
    assert_eq!(a.post(&format!("/api/session/{session}/model"), json!({"model": {"id": "m1", "providerID": "mock"}})).await.0, 204);

    let (status, history) = a.post("/sync/history", json!({})).await;
    assert_eq!(status, 200);
    let rows = history.as_array().unwrap().clone();
    let kinds: Vec<&str> = rows.iter().map(|row| row["type"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["session.created.1", "session.updated.1", "session.next.model.switched.1"]);
    assert_eq!(rows.iter().map(|row| row["seq"].as_u64().unwrap()).collect::<Vec<_>>(), [0, 1, 2]);
    assert_eq!(rows[0]["aggregate_id"], json!(session));
    assert_eq!(a.post("/sync/history", json!({&session: 1})).await.1.as_array().unwrap().len(), 1);
    assert_eq!(a.post("/sync/history", json!({&session: -1})).await.0, 400);

    // Another server (same project) rebuilds the session from the history.
    let b = serve(&root, "b", Store::open_in_memory().unwrap()).await;
    b.get("/project/current").await;
    let events: Vec<Value> = rows.iter().map(|row| json!({"id": row["id"], "aggregateID": row["aggregate_id"], "seq": row["seq"], "type": row["type"], "data": row["data"]})).collect();
    let replay = json!({"directory": b.project.to_string_lossy(), "events": events});
    assert_eq!(b.post("/sync/replay?workspace=wrk_a", replay.clone()).await, (200, json!({"sessionID": session})));
    let copied = b.get(&format!("/session/{session}")).await.1;
    assert_eq!((&copied["title"], &copied["model"]["id"]), (&json!("Synced"), &json!("m1")));
    assert_eq!(b.post("/sync/replay?workspace=wrk_a", replay.clone()).await.0, 200);
    let mut diverged = replay.clone();
    diverged["events"][1]["data"]["info"]["title"] = json!("Other");
    assert_eq!(b.post("/sync/replay?workspace=wrk_a", diverged).await.0, 500);
    // A different owner may not write into an owned aggregate.
    assert_eq!(b.post("/sync/replay?workspace=wrk_b", replay).await.0, 500);
    b.stop().await;

    // The v2 history comes back from the log after a restart.
    a.stop().await;
    let a = serve(&root, "a", Store::open(&database).unwrap()).await;
    let (status, history) = a.get(&format!("/api/session/{session}/history")).await;
    assert_eq!(status, 200);
    assert_eq!(history["data"][0]["type"], "session.next.model.switched");
    assert_eq!(history["data"][0]["durable"], json!({"aggregateID": session, "seq": 2, "version": 1}));
    a.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_move_with_their_changes() {
    init();
    let root = root();
    let f = serve(&root, "a", Store::open_in_memory().unwrap()).await;
    let linked = root.base.join("linked");
    git(&f.project, &["worktree", "add", "--detach", &linked.to_string_lossy(), "HEAD"]);
    let session = f.session(&f.project).await;
    std::fs::write(f.project.join("keep.txt"), "one\nmoved\n").unwrap();
    std::fs::write(f.project.join("new.txt"), "fresh\n").unwrap();

    let path = "/experimental/control-plane/move-session";
    let moved = json!({"sessionID": session, "destination": {"directory": linked.to_string_lossy()}, "moveChanges": true});
    assert_eq!(f.post(path, moved).await.0, 204);
    let info = f.get(&format!("/session/{session}")).await.1;
    assert_eq!((&info["directory"], &info["path"]), (&json!(linked.to_string_lossy()), &json!("")));
    assert_eq!(std::fs::read_to_string(linked.join("keep.txt")).unwrap().replace("\r\n", "\n"), "one\nmoved\n");
    assert_eq!(std::fs::read_to_string(linked.join("new.txt")).unwrap().replace("\r\n", "\n"), "fresh\n");
    assert_eq!(std::fs::read_to_string(f.project.join("keep.txt")).unwrap().replace("\r\n", "\n"), "one\n");
    assert!(!f.project.join("new.txt").exists());
    let history = f.get(&format!("/api/session/{session}/history")).await.1;
    assert_eq!(history["data"][0]["data"]["location"], json!({"directory": linked.to_string_lossy()}));

    let missing = json!({"sessionID": "ses_missing", "destination": {"directory": linked.to_string_lossy()}});
    assert_eq!(f.post(path, missing).await, (400, json!({"name": "MoveSessionError", "data": {"message": "Session not found: ses_missing"}})));
    let elsewhere = root.base.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let foreign = json!({"sessionID": session, "destination": {"directory": elsewhere.to_string_lossy()}});
    assert_eq!(f.post(path, foreign).await.1["data"]["message"], "Destination directory belongs to another project");
    assert_eq!(f.post(path, json!({})).await.0, 400);
    f.stop().await;
}
