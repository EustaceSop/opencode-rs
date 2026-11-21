use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use opencode_lsp::lsp::{Context, Lsp};
use opencode_lsp::{AfterEdit, DiagnosticsMode, Location, Workspace, WorkspaceOptions};
use serde_json::{Value, json};

// Cargo builds examples next to the test binaries: target/<profile>/examples/fake_lsp.
fn fake_lsp() -> String {
    let exe = std::env::current_exe().unwrap();
    let profile = exe.parent().and_then(Path::parent).unwrap();
    let name = if cfg!(windows) {
        "fake_lsp.exe"
    } else {
        "fake_lsp"
    };
    let path = profile.join("examples").join(name);
    assert!(path.exists(), "missing {}", path.display());
    path.to_string_lossy().into_owned()
}

fn command(flags: &[&str]) -> Value {
    let mut command = vec![fake_lsp()];
    command.extend(flags.iter().map(|s| s.to_string()));
    json!(command)
}

fn config(flags: &[&str]) -> Value {
    json!({ "fake": { "command": command(flags), "extensions": [".fake"], "initialization": { "fake": { "value": 7 } } } })
}

struct Fixture {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    // Long-name form, so paths compare equal to what the server echoes back.
    let dir = std::fs::canonicalize(tmp.path()).unwrap();
    let dir = PathBuf::from(
        dir.to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_string(),
    );
    Fixture { _tmp: tmp, dir }
}

fn lsp(f: &Fixture, config: &Value) -> Lsp {
    let ctx = Context {
        directory: f.dir.clone(),
        worktree: f.dir.clone(),
        ..Default::default()
    };
    Lsp::new(Some(config), ctx)
}

fn write(f: &Fixture, name: &str, text: &str) -> PathBuf {
    let path = f.dir.join(name);
    std::fs::write(&path, text).unwrap();
    path
}

fn at(file: &Path, line: u32, character: u32) -> Location {
    Location {
        file: file.to_path_buf(),
        line,
        character,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn push_diagnostics_and_operations() {
    let f = fixture();
    let lsp = lsp(&f, &config(&[]));
    let mut updates = lsp.subscribe_updates();
    let file = write(&f, "a.fake", "ok\nERR here\nfine\nERR again\n");
    assert!(lsp.has_clients(&file));
    assert!(lsp.status().is_empty());

    let started = Instant::now();
    lsp.touch_file(&file, Some(DiagnosticsMode::Document)).await;
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "push wait took {:?}",
        started.elapsed()
    );
    assert!(updates.try_recv().is_ok() && updates.try_recv().is_err());

    let status = lsp.status();
    assert_eq!(status.len(), 1);
    assert_eq!(
        (status[0].id.as_str(), status[0].name.as_str()),
        ("fake", "fake")
    );
    assert_eq!(
        (status[0].root.as_str(), status[0].status.as_str()),
        ("", "connected")
    );
    assert_eq!(
        serde_json::to_value(&status[0]).unwrap(),
        json!({"id": "fake", "name": "fake", "root": "", "status": "connected"})
    );

    let diagnostics = lsp.diagnostics();
    let items = &diagnostics[&*file.to_string_lossy()];
    assert_eq!(items.len(), 3);
    let name = file.to_string_lossy();
    assert_eq!(
        lsp.edit_report(&file),
        format!(
            "\n\nLSP errors detected in this file, please fix:\n<diagnostics file=\"{name}\">\nERROR [2:1] bad line\nERROR [4:1] bad line\n</diagnostics>"
        )
    );

    let hover = lsp.hover(&at(&file, 3, 9)).await;
    assert_eq!(
        hover,
        vec![json!({ "contents": "hover 3:9 config=[7] change=none" })]
    );
    assert_eq!(lsp.definition(&at(&file, 0, 0)).await.len(), 1);
    assert_eq!(lsp.references(&at(&file, 0, 0)).await.len(), 2);
    assert!(lsp.implementation(&at(&file, 0, 0)).await.is_empty());
    assert!(
        lsp.prepare_call_hierarchy(&at(&file, 0, 0))
            .await
            .is_empty()
    );
    assert!(lsp.incoming_calls(&at(&file, 0, 0)).await.is_empty());
    let uri = url::Url::from_file_path(&file).unwrap().to_string();
    let symbols = lsp.document_symbol(&uri).await;
    assert_eq!(symbols.len(), 1);
    assert_eq!(symbols[0]["name"], "main");
    let workspace = lsp.workspace_symbol("sym").await;
    assert_eq!(workspace.len(), 10);
    assert!(workspace.iter().all(|s| s["kind"] == 12));
    assert_eq!(workspace[0]["name"], "sym1");

    // Fixing the file clears the report on the next touch (didChange, version 1).
    std::fs::write(&file, "ok\nfine\n").unwrap();
    lsp.touch_file(&file, Some(DiagnosticsMode::Document)).await;
    assert_eq!(lsp.edit_report(&file), "");
    assert_eq!(lsp.diagnostics()[&*file.to_string_lossy()].len(), 1);
    assert_eq!(
        lsp.hover(&at(&file, 0, 0)).await[0]["contents"],
        "hover 0:0 config=[7] change=full"
    );
    assert_eq!(lsp.status().len(), 1);

    lsp.shutdown().await;
    assert!(lsp.status().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn pull_diagnostics() {
    let f = fixture();
    let lsp = lsp(&f, &config(&["--pull"]));
    let file = write(&f, "a.fake", "ERR\n");
    let started = Instant::now();
    lsp.touch_file(&file, Some(DiagnosticsMode::Document)).await;
    assert!(started.elapsed() < Duration::from_secs(4));
    assert!(
        lsp.edit_report(&file)
            .ends_with("ERROR [1:1] pull error\n</diagnostics>")
    );

    std::fs::write(&file, "clean\n").unwrap();
    lsp.touch_file(&file, Some(DiagnosticsMode::Full)).await;
    assert_eq!(lsp.edit_report(&file), "");
    lsp.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn incremental_sync_sends_one_range_edit() {
    let f = fixture();
    let lsp = lsp(&f, &config(&["--incremental"]));
    let file = write(&f, "a.fake", "one\n");
    lsp.touch_file(&file, None).await;
    std::fs::write(&file, "two\nERR\n").unwrap();
    lsp.touch_file(&file, Some(DiagnosticsMode::Document)).await;
    assert_eq!(
        lsp.hover(&at(&file, 0, 0)).await[0]["contents"],
        "hover 0:0 config=[7] change=range"
    );
    assert!(lsp.edit_report(&file).contains("ERROR [2:1] bad line"));
    lsp.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn write_report_lists_other_files() {
    let f = fixture();
    let lsp = lsp(&f, &config(&[]));
    let current = write(&f, "current.fake", "ERR\n");
    let mut others = Vec::new();
    for index in 0..7 {
        others.push(write(&f, &format!("other{index}.fake"), "x\nERR\n"));
    }
    let clean = write(&f, "clean.fake", "fine\n");
    for file in others.iter().chain([&clean, &current]) {
        lsp.touch_file(file, Some(DiagnosticsMode::Document)).await;
    }
    let report = lsp.write_report(&current);
    assert_eq!(
        report
            .matches("LSP errors detected in this file, please fix:")
            .count(),
        1
    );
    assert_eq!(
        report
            .matches("LSP errors detected in other files:")
            .count(),
        5
    );
    assert!(report.contains(&format!(
        "<diagnostics file=\"{}\">\nERROR [2:1] bad line",
        others[0].to_string_lossy()
    )));
    assert!(!report.contains("clean.fake"));
    assert_eq!(lsp.status().len(), 1);
    lsp.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_touches_share_one_client() {
    let f = fixture();
    let lsp = lsp(&f, &config(&[]));
    let a = write(&f, "a.fake", "ERR\n");
    let b = write(&f, "b.fake", "ok\n");
    tokio::join!(
        lsp.touch_file(&a, Some(DiagnosticsMode::Document)),
        lsp.touch_file(&b, Some(DiagnosticsMode::Document)),
        lsp.touch_file(&a, None)
    );
    assert_eq!(lsp.status().len(), 1);
    assert_eq!(lsp.diagnostics().len(), 2);
    lsp.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_servers_are_marked_broken() {
    let f = fixture();
    let config = json!({
        "dies": { "command": command(&["--die"]), "extensions": [".fake"] },
        "missing": { "command": [f.dir.join("no-such-server").to_string_lossy()], "extensions": [".fake"] },
        "empty": { "extensions": [".fake"] }
    });
    let lsp = lsp(&f, &config);
    let file = write(&f, "a.fake", "ERR\n");
    assert!(lsp.has_clients(&file));
    lsp.touch_file(&file, Some(DiagnosticsMode::Document)).await;
    assert!(!lsp.has_clients(&file));
    assert!(lsp.status().is_empty());
    assert!(lsp.diagnostics().is_empty());
    assert!(lsp.hover(&at(&file, 0, 0)).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn unmatched_and_outside_files_get_no_server() {
    let f = fixture();
    let lsp = lsp(&f, &config(&[]));
    let other = write(&f, "a.txt", "ERR\n");
    assert!(!lsp.has_clients(&other));
    lsp.touch_file(&other, Some(DiagnosticsMode::Document))
        .await;

    let outside = tempfile::tempdir().unwrap();
    let stray = outside.path().join("x.fake");
    std::fs::write(&stray, "ERR\n").unwrap();
    lsp.touch_file(&stray, Some(DiagnosticsMode::Document))
        .await;
    assert!(lsp.status().is_empty());
    assert!(lsp.workspace_symbol("q").await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn builtin_roots_spawn_per_root() {
    let f = fixture();
    // Override a built-in: its root strategy (nearest Cargo.toml) is kept, the command is replaced.
    let config = json!({ "rust": { "command": command(&[]) }, "pyright": { "disabled": true } });
    let lsp = lsp(&f, &config);
    for name in ["one", "two"] {
        std::fs::create_dir_all(f.dir.join(name).join("src")).unwrap();
        write(&f, &format!("{name}/Cargo.toml"), "[package]\n");
        write(&f, &format!("{name}/src/lib.rs"), "ERR\n");
    }
    lsp.touch_file(
        &f.dir.join("one/src/lib.rs"),
        Some(DiagnosticsMode::Document),
    )
    .await;
    lsp.touch_file(
        &f.dir.join("two/src/lib.rs"),
        Some(DiagnosticsMode::Document),
    )
    .await;
    let mut roots: Vec<String> = lsp
        .status()
        .into_iter()
        .filter(|s| s.id == "rust")
        .map(|s| s.root)
        .collect();
    roots.sort();
    assert_eq!(roots, ["one", "two"]);
    assert!(!lsp.server_ids().contains(&"pyright"));
    lsp.shutdown().await;
}

fn workspace(f: &Fixture, config: Value) -> Workspace {
    Workspace::new(WorkspaceOptions {
        data_dir: f.dir.join(".data"),
        cache_dir: None,
        project_id: "proj".into(),
        directory: f.dir.clone(),
        worktree: f.dir.clone(),
        git: false,
        config,
        ..Default::default()
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn workspace_is_inert_without_config() {
    let f = fixture();
    let ws = workspace(&f, json!({}));
    let file = write(&f, "a.fake", "ERR\n");
    assert_eq!(ws.after_edit(&file).await, AfterEdit::default());
    assert_eq!(ws.after_write(&file).await, AfterEdit::default());
    ws.warm(&file).await;
    assert!(ws.lsp_status().is_empty());
    assert!(ws.formatter_status().await.is_empty());
    assert_eq!(ws.snapshot().track().await, None);
    assert!(!f.dir.join(".data").exists());
    ws.dispose().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn workspace_after_edit_formats_then_reports() {
    let f = fixture();
    // The server binary doubles as a formatter that fails without touching the file.
    let mut config = config(&[]);
    config = json!({ "lsp": config, "formatter": { "fake": { "command": [fake_lsp(), "--die"], "extensions": [".fake"] } } });
    let ws = workspace(&f, config);
    let file = write(&f, "a.fake", "ERR\n");
    let result = ws.after_edit(&file).await;
    assert!(result.formatted && !result.changed);
    assert!(
        result
            .diagnostics_text
            .starts_with("\n\nLSP errors detected in this file, please fix:\n<diagnostics file=")
    );
    assert!(
        result
            .diagnostics_text
            .ends_with("ERROR [1:1] bad line\n</diagnostics>")
    );
    assert_eq!(
        ws.after_write(&file).await.diagnostics_text,
        result.diagnostics_text
    );
    assert_eq!(ws.lsp_status().len(), 1);
    assert_eq!(ws.formatter_status().await.len(), 27);
    ws.dispose().await;
    assert!(ws.lsp_status().is_empty());
}
