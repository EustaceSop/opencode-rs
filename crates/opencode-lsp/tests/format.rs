use std::path::{Path, PathBuf};

use opencode_lsp::{FormatOptions, FormatResult, Formatter};
use serde_json::json;

fn script(dir: &Path, name: &str, windows: &str, unix: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    if cfg!(windows) {
        let path = dir.join(format!("{name}.cmd"));
        std::fs::write(&path, format!("@echo off\r\n{windows}\r\n")).unwrap();
        return path;
    }
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{unix}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

// Appends `<token><OC_SUFFIX>` as a line to the file given as first argument.
fn appender(dir: &Path) -> String {
    script(
        dir,
        "append",
        "echo %~2%OC_SUFFIX%>>\"%~1\"",
        "echo \"$2$OC_SUFFIX\" >> \"$1\"",
    )
    .to_string_lossy()
    .into_owned()
}

fn opts(dir: &Path) -> FormatOptions {
    FormatOptions {
        directory: dir.to_path_buf(),
        worktree: dir.to_path_buf(),
        ..Default::default()
    }
}

fn lines(file: &Path) -> Vec<String> {
    std::fs::read_to_string(file)
        .unwrap()
        .lines()
        .map(String::from)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn custom_formatters_run_in_order() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let append = appender(&dir.join("bin"));
    let config = json!({
        "first": {"command": [append, "$FILE", "one"], "extensions": [".txt"]},
        "second": {"command": [append, "$FILE", "two"], "extensions": [".txt", ".md"], "environment": {"OC_SUFFIX": "!"}},
        "broken": {"command": [dir.join("missing-binary").to_string_lossy(), "$FILE"], "extensions": [".txt"]},
        "idle": {"extensions": [".txt"]},
        "off": {"command": [append, "$FILE", "never"], "extensions": [".txt"], "disabled": true}
    });
    let formatter = Formatter::new(Some(&config), opts(dir));
    let file = dir.join("note.txt");
    std::fs::write(&file, "start\n").unwrap();

    assert_eq!(
        formatter.run(&file).await,
        FormatResult {
            ran: true,
            changed: true
        }
    );
    assert_eq!(lines(&file), ["start", "one", "two!"]);
    assert!(formatter.file(&file).await);
    assert_eq!(lines(&file).len(), 5);

    let other = dir.join("doc.md");
    std::fs::write(&other, "").unwrap();
    assert!(formatter.run(&other).await.changed);
    assert_eq!(lines(&other), ["two!"]);

    let unmatched = dir.join("code.unknownext");
    std::fs::write(&unmatched, "fn main() {}\n").unwrap();
    assert_eq!(formatter.run(&unmatched).await, FormatResult::default());

    let status = formatter.status().await;
    let enabled = |name: &str| status.iter().find(|s| s.name == name).map(|s| s.enabled);
    assert_eq!(enabled("first"), Some(true));
    assert_eq!(enabled("broken"), Some(true));
    assert_eq!(enabled("idle"), Some(false));
    assert_eq!(enabled("off"), None);
    assert_eq!(
        status
            .iter()
            .find(|s| s.name == "second")
            .unwrap()
            .extensions,
        [".txt", ".md"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unchanged_and_failing_formatters() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let noop = script(&dir.join("bin"), "noop", "exit /b 3", "exit 3")
        .to_string_lossy()
        .into_owned();
    let config = json!({ "noop": {"command": [noop, "$FILE"], "extensions": [".txt"]} });
    let formatter = Formatter::new(Some(&config), opts(dir));
    let file = dir.join("a.txt");
    std::fs::write(&file, "same\n").unwrap();
    assert_eq!(
        formatter.run(&file).await,
        FormatResult {
            ran: true,
            changed: false
        }
    );
    assert_eq!(lines(&file), ["same"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn absent_config_does_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("a.rs");
    std::fs::write(&file, "fn  main(){}\n").unwrap();
    for config in [None, Some(json!(false))] {
        let formatter = Formatter::new(config.as_ref(), opts(tmp.path()));
        assert!(formatter.is_empty());
        assert_eq!(formatter.run(&file).await, FormatResult::default());
        assert!(formatter.status().await.is_empty());
    }
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "fn  main(){}\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn project_files_enable_builtins() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let dir = root.join("packages").join("app");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{"devDependencies": {"prettier": "^3"}}"#,
    )
    .unwrap();
    std::fs::write(dir.join("package.json"), r#"{"dependencies": {}}"#).unwrap();
    std::fs::write(
        root.join("composer.json"),
        r#"{"require-dev": {"laravel/pint": "^1"}}"#,
    )
    .unwrap();
    script(
        &root.join("node_modules").join(".bin"),
        "prettier",
        "echo pretty>\"%~2\"",
        "echo pretty > \"$2\"",
    );

    // `clang-format` and `uv` are display names, not export identifiers: the built-ins stay untouched.
    let config = json!({ "biome": {"disabled": true}, "oxfmt": {"disabled": true} });
    let options = FormatOptions {
        directory: dir.clone(),
        worktree: root.to_path_buf(),
        ..Default::default()
    };
    let formatter = Formatter::new(Some(&config), options);
    let status = formatter.status().await;
    let enabled = |name: &str| status.iter().find(|s| s.name == name).map(|s| s.enabled);
    assert_eq!(enabled("prettier"), Some(true));
    assert_eq!(enabled("pint"), Some(true));
    assert_eq!(enabled("biome"), None);
    assert_eq!(enabled("clang-format"), Some(false));
    assert_eq!(enabled("ocamlformat"), Some(false));
    assert_eq!(status.len(), 24);

    let file = dir.join("data.graphql");
    std::fs::write(&file, "query{a}\n").unwrap();
    assert_eq!(
        formatter.run(&file).await,
        FormatResult {
            ran: true,
            changed: true
        }
    );
    assert_eq!(lines(&file), ["pretty"]);

    // Without the dependency the same project has no prettier.
    std::fs::write(root.join("package.json"), "{}").unwrap();
    let options = FormatOptions {
        directory: dir.clone(),
        worktree: root.to_path_buf(),
        ..Default::default()
    };
    let formatter = Formatter::new(Some(&json!(true)), options);
    let status = formatter.status().await;
    assert_eq!(
        status
            .iter()
            .find(|s| s.name == "prettier")
            .map(|s| s.enabled),
        Some(false)
    );
    assert_eq!(status.len(), 26);
}
