use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::release::{self, Asset};
use super::*;
use crate::lsp::{Lsp, Status};

// Cargo builds examples next to the test binaries: target/<profile>/examples/<name>.
fn example(name: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let profile = exe.parent().and_then(Path::parent).unwrap();
    let path = profile.join("examples").join(Platform::current().exe(name));
    assert!(path.exists(), "missing {}", path.display());
    path
}

struct Sandbox {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    cache: PathBuf,
    tools: PathBuf,
    log: PathBuf,
}

// A project, an empty cache and a PATH directory holding only the named fake tools.
fn sandbox(tools: &[&str], extra: Value) -> Sandbox {
    let tmp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let base = PathBuf::from(
        base.to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_string(),
    );
    let sb = Sandbox {
        dir: base.join("project"),
        cache: base.join("cache"),
        tools: base.join("tools"),
        log: base.join("tools.log"),
        _tmp: tmp,
    };
    for dir in [&sb.dir, &sb.cache, &sb.tools] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let fake = example("fake_tool");
    for tool in tools {
        std::fs::copy(&fake, sb.tools.join(Platform::current().exe(tool))).unwrap();
    }
    let mut config = json!({ "lsp": example("fake_lsp"), "log": sb.log });
    for (key, value) in extra.as_object().into_iter().flatten() {
        config[key] = value.clone();
    }
    std::fs::write(sb.tools.join("fake_tool.json"), config.to_string()).unwrap();
    sb
}

impl Sandbox {
    fn installer(&self, http: Option<&Http>) -> Installer {
        let base = http.map_or("http://127.0.0.1:9".to_string(), |h| h.base.clone());
        let mut installer = Installer::new(false);
        installer.path = Some(self.tools.clone().into_os_string());
        installer.endpoints = Endpoints {
            github_api: base.clone(),
            github: base.clone(),
            hashicorp: base.clone(),
            eclipse: base.clone(),
            jetbrains: base,
        };
        installer
    }

    fn ctx(&self) -> Context {
        Context {
            directory: self.dir.clone(),
            worktree: self.dir.clone(),
            cache_dir: Some(self.cache.clone()),
            experimental_ty: false,
        }
    }

    fn lsp(&self, installer: Installer) -> Lsp {
        Lsp::with_installer(Some(&json!(true)), self.ctx(), installer)
    }

    fn file(&self, name: &str) -> PathBuf {
        let path = self.dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "content\n").unwrap();
        path
    }

    fn bin(&self) -> PathBuf {
        self.cache.join("bin")
    }

    fn calls(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

fn connected(id: &str, root: &str) -> Status {
    Status {
        id: id.into(),
        name: id.into(),
        root: root.into(),
        status: "connected".into(),
    }
}

fn state_of(lsp: &Lsp, id: &str) -> Option<(InstallState, String)> {
    lsp.install_status()
        .into_iter()
        .find(|item| item.id == id)
        .map(|item| (item.state, item.reason.unwrap_or_default()))
}

type Routes = Arc<Mutex<HashMap<String, Vec<u8>>>>;

// Local stand-in for the release hosts; unknown paths answer 404.
struct Http {
    base: String,
    routes: Routes,
    hits: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Http {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Http {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let routes: Routes = Arc::default();
        let hits: Arc<Mutex<Vec<String>>> = Arc::default();
        let (shared, seen) = (routes.clone(), hits.clone());
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (routes, hits) = (shared.clone(), seen.clone());
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut chunk = [0u8; 2048];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut chunk).await {
                            Ok(n) if n > 0 => request.extend_from_slice(&chunk[..n]),
                            _ => return,
                        }
                    }
                    let head = String::from_utf8_lossy(&request);
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                    hits.lock().unwrap().push(path.clone());
                    let body = routes.lock().unwrap().get(&path).cloned();
                    let (status, body) = match body {
                        Some(body) => ("200 OK", body),
                        None => ("404 Not Found", Vec::new()),
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self {
            base,
            routes,
            hits,
            task,
        }
    }

    fn route(&self, path: &str, body: impl Into<Vec<u8>>) {
        self.routes
            .lock()
            .unwrap()
            .insert(path.to_string(), body.into());
    }

    // A GitHub "latest release" document with one downloadable asset.
    fn release(&self, repo: &str, tag: &str, asset: &str, archive: Vec<u8>) {
        let release = json!({
            "tag_name": tag,
            "name": tag,
            "assets": [{ "name": asset, "browser_download_url": format!("{}/dl/{asset}", self.base) }],
        });
        self.route(
            &format!("/repos/{repo}/releases/latest"),
            release.to_string(),
        );
        self.route(&format!("/dl/{asset}"), archive);
    }

    fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (!(crc & 1)).wrapping_add(1));
        }
    }
    !crc
}

// Stored (uncompressed) zip with unix executable modes.
fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let (mut out, mut central) = (Vec::new(), Vec::new());
    for (name, data) in entries {
        let offset = out.len() as u32;
        let (crc, size) = (crc32(data), data.len() as u32);
        let mut common = Vec::new();
        for field in [20u16, 0, 0, 0, 33] {
            common.extend(field.to_le_bytes());
        }
        for field in [crc, size, size] {
            common.extend(field.to_le_bytes());
        }
        common.extend((name.len() as u16).to_le_bytes());
        common.extend(0u16.to_le_bytes());
        out.extend(0x0403_4b50u32.to_le_bytes());
        out.extend(&common);
        out.extend(name.as_bytes());
        out.extend(*data);
        central.extend(0x0201_4b50u32.to_le_bytes());
        central.extend(((3u16 << 8) | 20).to_le_bytes());
        central.extend(&common);
        central.extend([0u8; 6]);
        central.extend((0o100755u32 << 16).to_le_bytes());
        central.extend(offset.to_le_bytes());
        central.extend(name.as_bytes());
    }
    let (start, count) = (out.len() as u32, entries.len() as u16);
    out.extend(&central);
    out.extend(0x0605_4b50u32.to_le_bytes());
    out.extend([0u8; 4]);
    out.extend(count.to_le_bytes());
    out.extend(count.to_le_bytes());
    out.extend((central.len() as u32).to_le_bytes());
    out.extend(start.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out
}

fn tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let mut tops: Vec<&str> = Vec::new();
    for (name, data) in entries {
        let path = src.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, data).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let top = name.split('/').next().unwrap();
        if !tops.contains(&top) {
            tops.push(top);
        }
    }
    let status = std::process::Command::new("tar")
        .args(["-czf", "../out.tar.gz"])
        .args(&tops)
        .current_dir(&src)
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::read(tmp.path().join("out.tar.gz")).unwrap()
}

fn archive(name: &str, entries: &[(&str, &[u8])]) -> Vec<u8> {
    if name.ends_with(".zip") {
        zip(entries)
    } else {
        tar_gz(entries)
    }
}

fn no_staging_left(bin: &Path) {
    let left: Vec<String> = std::fs::read_dir(bin)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".tmp-") || name.ends_with(".zip") || name.ends_with(".gz"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn npm_package_installs_with_bun_once_and_connects() {
    let sb = sandbox(&["bun"], json!({}));
    let lsp = sb.lsp(sb.installer(None));
    let file = sb.file("a.yaml");
    assert!(lsp.status().is_empty());
    lsp.touch_file(&file, None).await;
    assert_eq!(lsp.status(), vec![connected("yaml-ls", "")]);
    assert_eq!(
        state_of(&lsp, "yaml-ls").unwrap().0,
        InstallState::Installed
    );

    let package = sb.cache.join("packages").join("yaml-language-server");
    let calls = sb.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0]["tool"], "bun");
    assert_eq!(
        calls[0]["args"],
        json!([
            "add",
            "yaml-language-server",
            "--exact",
            "--ignore-scripts",
            "--no-progress"
        ])
    );
    assert_eq!(PathBuf::from(calls[0]["cwd"].as_str().unwrap()), package);
    let shim = Platform::current().exe("yaml-language-server");
    assert!(
        package
            .join("node_modules")
            .join(".bin")
            .join(shim)
            .is_file()
    );

    lsp.touch_file(&sb.file("b.yml"), None).await;
    lsp.shutdown().await;
    // A later instance reuses the cached package.
    let again = sb.lsp(sb.installer(None));
    again.touch_file(&file, None).await;
    assert_eq!(again.status(), vec![connected("yaml-ls", "")]);
    assert!(again.install_status().is_empty());
    again.shutdown().await;
    assert_eq!(sb.calls().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn npm_is_the_fallback_and_scoped_packages_keep_their_layout() {
    let bins = json!({ "bins": { "@vue/language-server": ["vue-language-server"] } });
    let sb = sandbox(&["npm"], bins);
    let lsp = sb.lsp(sb.installer(None));
    lsp.touch_file(&sb.file("a.vue"), None).await;
    assert_eq!(lsp.status(), vec![connected("vue", "")]);
    let calls = sb.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0]["tool"], "npm");
    assert_eq!(
        calls[0]["args"],
        json!([
            "install",
            "@vue/language-server",
            "--save-prod",
            "--save-exact",
            "--ignore-scripts",
            "--no-progress"
        ])
    );
    let package = sb
        .cache
        .join("packages")
        .join("@vue")
        .join("language-server");
    assert_eq!(PathBuf::from(calls[0]["cwd"].as_str().unwrap()), package);
    assert!(package.join("package.json").is_file());
    lsp.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn windows_prefers_npm_when_both_are_present() {
    let sb = sandbox(&["bun", "npm"], json!({}));
    let lsp = sb.lsp(sb.installer(None));
    lsp.touch_file(&sb.file("a.yaml"), None).await;
    let calls = sb.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0]["tool"], if cfg!(windows) { "npm" } else { "bun" });
    lsp.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn npm_server_is_unavailable_without_a_package_manager() {
    let sb = sandbox(&[], json!({}));
    let lsp = sb.lsp(sb.installer(None));
    let file = sb.file("a.yaml");
    lsp.touch_file(&file, None).await;
    assert!(lsp.status().is_empty());
    assert!(!lsp.has_clients(&file));
    let (state, reason) = state_of(&lsp, "yaml-ls").unwrap();
    assert_eq!(state, InstallState::Unavailable);
    assert_eq!(reason, "neither bun nor npm was found on PATH");
    assert!(!sb.cache.join("packages").exists());
}

#[test]
fn npm_pick_follows_the_package_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path();
    let dir = npm::directory(cache, "@scope/tool");
    let bin_dir = dir.join("node_modules").join(".bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    assert_eq!(npm::pick(cache, "@scope/tool", None), None);
    let shim = |name: &str| {
        let ext = if cfg!(windows) { ".cmd" } else { "" };
        let path = bin_dir.join(format!("{name}{ext}"));
        std::fs::write(&path, "").unwrap();
        // npm also writes the bare and PowerShell launchers on Windows.
        if cfg!(windows) {
            std::fs::write(bin_dir.join(name), "").unwrap();
            std::fs::write(bin_dir.join(format!("{name}.ps1")), "").unwrap();
        }
        path
    };
    let only = shim("solo");
    assert_eq!(npm::pick(cache, "@scope/tool", None), Some(only.clone()));
    assert_eq!(npm::pick(cache, "@scope/tool", Some("other")), None);

    let other = shim("other");
    let tool = shim("tool");
    let manifest = dir.join("node_modules").join("@scope").join("tool");
    std::fs::create_dir_all(&manifest).unwrap();
    let write = |bin: Value| {
        std::fs::write(
            manifest.join("package.json"),
            json!({ "bin": bin }).to_string(),
        )
        .unwrap()
    };
    // No manifest entry: the first file.
    assert_eq!(npm::pick(cache, "@scope/tool", None), Some(other.clone()));
    write(json!("cli.js"));
    assert_eq!(npm::pick(cache, "@scope/tool", None), Some(tool.clone()));
    write(json!({ "solo": "a.js" }));
    assert_eq!(npm::pick(cache, "@scope/tool", None), Some(only.clone()));
    write(json!({ "solo": "a.js", "tool": "b.js" }));
    assert_eq!(npm::pick(cache, "@scope/tool", None), Some(tool));
    write(json!({ "solo": "a.js", "other": "b.js" }));
    assert_eq!(npm::pick(cache, "@scope/tool", None), Some(only));
    assert_eq!(npm::pick(cache, "@scope/tool", Some("other")), Some(other));
}

#[tokio::test(flavor = "multi_thread")]
async fn github_release_in_the_native_format_installs_and_connects() {
    let sb = sandbox(&[], json!({}));
    let http = Http::start().await;
    let platform = Platform::current();
    let asset = release::texlab_asset(platform);
    let server = std::fs::read(example("fake_lsp")).unwrap();
    let exe = platform.exe("texlab");
    http.release(
        "latex-lsp/texlab",
        "v5.25.1",
        &asset,
        archive(&asset, &[(&exe, &server), ("README.md", b"docs")]),
    );
    let lsp = sb.lsp(sb.installer(Some(&http)));
    let file = sb.file("paper.tex");
    lsp.touch_file(&file, None).await;
    assert_eq!(
        lsp.status(),
        vec![connected("texlab", "")],
        "{:?}",
        lsp.install_status()
    );
    assert!(sb.bin().join(&exe).is_file());
    no_staging_left(&sb.bin());
    lsp.touch_file(&sb.file("refs.bib"), None).await;
    assert_eq!(
        http.hits(),
        vec![
            "/repos/latex-lsp/texlab/releases/latest".to_string(),
            format!("/dl/{asset}")
        ]
    );
    lsp.shutdown().await;
}

// The archive format the host does not use natively: zip on unix, tar.gz on Windows.
#[tokio::test(flavor = "multi_thread")]
async fn github_release_in_the_other_format_extracts_to_the_same_layout() {
    let sb = sandbox(&[], json!({}));
    let http = Http::start().await;
    let platform = Platform {
        os: if cfg!(windows) { "linux" } else { "win32" },
        arch: "x64",
    };
    let asset = release::texlab_asset(platform);
    let exe = platform.exe("texlab");
    http.release(
        "latex-lsp/texlab",
        "v5.25.1",
        &asset,
        archive(&asset, &[(&exe, b"binary")]),
    );
    let mut installer = sb.installer(Some(&http));
    installer.platform = platform;
    let result = installer.install("texlab", &sb.dir, &sb.ctx()).await;
    assert_eq!(result, Ok(()));
    assert_eq!(std::fs::read(sb.bin().join(&exe)).unwrap(), b"binary");
    no_staging_left(&sb.bin());
}

#[tokio::test(flavor = "multi_thread")]
async fn lua_ls_keeps_its_tree_in_a_dedicated_directory() {
    let platform = Platform::current();
    let Some(asset) = release::lua_asset(platform, "3.15.0") else {
        return;
    };
    let sb = sandbox(&[], json!({}));
    let http = Http::start().await;
    let server = std::fs::read(example("fake_lsp")).unwrap();
    let exe = format!("bin/{}", platform.exe("lua-language-server"));
    http.release(
        "LuaLS/lua-language-server",
        "3.15.0",
        &asset,
        archive(&asset, &[(&exe, &server), ("meta/template.lua", b"--")]),
    );
    let lsp = sb.lsp(sb.installer(Some(&http)));
    lsp.touch_file(&sb.file("init.lua"), None).await;
    assert_eq!(
        lsp.status(),
        vec![connected("lua-ls", "")],
        "{:?}",
        lsp.install_status()
    );
    let dir = sb.bin().join(format!(
        "lua-language-server-{}-{}",
        platform.arch, platform.os
    ));
    assert!(dir.join(&exe).is_file());
    assert!(dir.join("meta").join("template.lua").is_file());
    no_staging_left(&sb.bin());
    lsp.shutdown().await;
    // The dedicated directory is found again without a second download.
    let again = sb.lsp(sb.installer(Some(&http)));
    again.touch_file(&sb.file("other.lua"), None).await;
    assert_eq!(again.status(), vec![connected("lua-ls", "")]);
    assert_eq!(http.hits().len(), 2);
    again.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn toolchain_install_runs_go_with_gobin() {
    let sb = sandbox(&["go"], json!({}));
    let lsp = sb.lsp(sb.installer(None));
    lsp.touch_file(&sb.file("main.go"), None).await;
    assert_eq!(lsp.status(), vec![connected("gopls", "")]);
    let calls = sb.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0]["tool"], "go");
    assert_eq!(
        calls[0]["args"],
        json!(["install", "golang.org/x/tools/gopls@latest"])
    );
    assert_eq!(PathBuf::from(calls[0]["gobin"].as_str().unwrap()), sb.bin());
    assert!(sb.bin().join(Platform::current().exe("gopls")).is_file());
    lsp.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn toolchain_install_needs_the_toolchain() {
    let sb = sandbox(&[], json!({}));
    let lsp = sb.lsp(sb.installer(None));
    lsp.touch_file(&sb.file("main.go"), None).await;
    lsp.touch_file(&sb.file("build.zig"), None).await;
    assert!(lsp.status().is_empty());
    let unavailable = |reason: &str| Some((InstallState::Unavailable, reason.to_string()));
    assert_eq!(
        state_of(&lsp, "gopls"),
        unavailable("go was not found on PATH")
    );
    assert_eq!(
        state_of(&lsp, "zls"),
        unavailable("zig was not found on PATH")
    );
    assert!(sb.calls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn disable_flag_gates_downloads_but_not_typescript() {
    let sb = sandbox(&["bun", "go"], json!({}));
    let http = Http::start().await;
    let mut installer = sb.installer(Some(&http));
    installer.disabled = true;
    let lsp = sb.lsp(installer);
    let files = ["a.yaml", "main.go", "paper.tex", "init.lua", "main.c"].map(|name| sb.file(name));
    for file in &files {
        lsp.touch_file(file, None).await;
        assert!(!lsp.has_clients(file));
    }
    assert!(lsp.status().is_empty());
    assert!(sb.calls().is_empty());
    assert!(http.hits().is_empty());
    for id in ["yaml-ls", "gopls", "texlab", "lua-ls", "clangd"] {
        let (state, reason) = state_of(&lsp, id).unwrap();
        assert_eq!(state, InstallState::Unavailable, "{id}");
        assert_eq!(
            reason,
            "downloads are disabled by OPENCODE_DISABLE_LSP_DOWNLOAD"
        );
    }
    assert!(!sb.bin().exists());

    // `Typescript.spawn` calls `Npm.which` without looking at the flag.
    let tsserver = sb.file("node_modules/typescript/lib/tsserver.js");
    lsp.touch_file(&sb.file("index.ts"), None).await;
    assert_eq!(lsp.status(), vec![connected("typescript", "")]);
    let calls = sb.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0]["args"][1], "typescript-language-server");
    assert!(tsserver.is_file());
    lsp.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_installs_are_reported_and_not_retried() {
    let sb = sandbox(&["go"], json!({ "fail": ["go"] }));
    let http = Http::start().await;
    let lsp = sb.lsp(sb.installer(Some(&http)));
    let (go, tex) = (sb.file("main.go"), sb.file("paper.tex"));
    for _ in 0..3 {
        lsp.touch_file(&go, None).await;
        lsp.touch_file(&tex, None).await;
    }
    let error = |id: &str| Status {
        status: "error".into(),
        ..connected(id, "")
    };
    assert_eq!(lsp.status(), vec![error("gopls"), error("texlab")]);
    assert!(!lsp.has_clients(&go) && !lsp.has_clients(&tex));

    let (state, reason) = state_of(&lsp, "gopls").unwrap();
    assert_eq!(state, InstallState::Failed);
    assert!(
        reason.starts_with("`go install golang.org/x/tools/gopls@latest` exited with code 1")
            && reason.ends_with("go: simulated failure"),
        "{reason}"
    );
    let (state, reason) = state_of(&lsp, "texlab").unwrap();
    assert_eq!(state, InstallState::Failed);
    assert!(reason.ends_with("HTTP 404 Not Found"), "{reason}");

    assert_eq!(sb.calls().len(), 1);
    assert_eq!(http.hits(), vec!["/repos/latex-lsp/texlab/releases/latest"]);
    assert!(!sb.bin().join(Platform::current().exe("gopls")).exists());
}

// A canonicalized cache directory arrives in the `\\?\` form, which Expand-Archive cannot take.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn zip_extraction_accepts_a_verbatim_cache_path() {
    let sb = sandbox(&[], json!({}));
    let http = Http::start().await;
    let asset = release::texlab_asset(Platform::current());
    http.release(
        "latex-lsp/texlab",
        "v1.0.0",
        &asset,
        zip(&[("texlab.exe", b"binary")]),
    );
    let mut ctx = sb.ctx();
    ctx.cache_dir = Some(std::fs::canonicalize(&sb.cache).unwrap());
    let installer = sb.installer(Some(&http));
    assert_eq!(installer.install("texlab", &sb.dir, &ctx).await, Ok(()));
    assert!(sb.bin().join("texlab.exe").is_file());
}

#[tokio::test(flavor = "multi_thread")]
async fn broken_archives_leave_nothing_behind() {
    let sb = sandbox(&[], json!({}));
    let http = Http::start().await;
    let asset = release::texlab_asset(Platform::current());
    http.release(
        "latex-lsp/texlab",
        "v1.0.0",
        &asset,
        b"not an archive".to_vec(),
    );
    let installer = sb.installer(Some(&http));
    let result = installer.install("texlab", &sb.dir, &sb.ctx()).await;
    assert!(
        matches!(&result, Err(Stop::Failed(reason)) if reason.starts_with("extracting")),
        "{result:?}"
    );
    assert_eq!(std::fs::read_dir(sb.bin()).unwrap().count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_touches_install_once() {
    let sb = sandbox(&["go"], json!({ "delay_ms": 400 }));
    let lsp = Arc::new(sb.lsp(sb.installer(None)));
    // Two module roots: separate clients, one shared install.
    let mut files = Vec::new();
    for module in ["a", "b"] {
        sb.file(&format!("{module}/go.work"));
        for name in ["x.go", "y.go", "z.go", "w.go"] {
            files.push(sb.file(&format!("{module}/{name}")));
        }
    }
    let tasks: Vec<_> = files
        .into_iter()
        .map(|file| {
            let lsp = lsp.clone();
            tokio::spawn(async move { lsp.touch_file(&file, None).await })
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }
    let mut status = lsp.status();
    status.sort_by(|a, b| a.root.cmp(&b.root));
    assert_eq!(
        status,
        vec![connected("gopls", "a"), connected("gopls", "b")]
    );
    assert_eq!(sb.calls().len(), 1, "{:?}", sb.calls());
    lsp.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn slow_install_does_not_hold_back_other_servers() {
    let sb = sandbox(&["go"], json!({ "delay_ms": 3000 }));
    let fake = example("fake_lsp").to_string_lossy().into_owned();
    let config = json!({ "fake": { "command": [fake], "extensions": [".go"] } });
    let lsp = Arc::new(Lsp::with_installer(
        Some(&config),
        sb.ctx(),
        sb.installer(None),
    ));
    let file = sb.file("main.go");
    let touch = {
        let lsp = lsp.clone();
        tokio::spawn(async move { lsp.touch_file(&file, None).await })
    };
    let started = Instant::now();
    let installing =
        || state_of(&lsp, "gopls").map(|(state, _)| state) == Some(InstallState::Installing);
    while lsp.status() != vec![connected("fake", "")] || !installing() {
        assert!(
            started.elapsed() < Duration::from_millis(2500),
            "blocked by the install"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    touch.await.unwrap();
    assert_eq!(lsp.status().len(), 2);
    assert_eq!(state_of(&lsp, "gopls").unwrap().0, InstallState::Installed);
    lsp.shutdown().await;
}

#[test]
fn asset_names_follow_the_source() {
    let on = |os, arch| Platform { os, arch };
    assert_eq!(
        release::zls_asset(on("win32", "x64")).unwrap(),
        "zls-x86_64-windows.zip"
    );
    assert_eq!(
        release::zls_asset(on("darwin", "arm64")).unwrap(),
        "zls-aarch64-macos.tar.xz"
    );
    assert_eq!(
        release::zls_asset(on("linux", "ia32")).unwrap(),
        "zls-x86-linux.tar.xz"
    );
    assert_eq!(release::zls_asset(on("darwin", "ia32")), None);
    assert_eq!(release::zls_asset(on("freebsd", "x64")), None);

    assert_eq!(
        release::lua_asset(on("win32", "x64"), "3.15.0").unwrap(),
        "lua-language-server-3.15.0-win32-x64.zip"
    );
    assert_eq!(
        release::lua_asset(on("linux", "arm64"), "3.15.0").unwrap(),
        "lua-language-server-3.15.0-linux-arm64.tar.gz"
    );
    assert_eq!(release::lua_asset(on("win32", "arm64"), "3.15.0"), None);

    assert_eq!(
        release::texlab_asset(on("darwin", "arm64")),
        "texlab-aarch64-macos.tar.gz"
    );
    assert_eq!(
        release::texlab_asset(on("win32", "x64")),
        "texlab-x86_64-windows.zip"
    );
    assert_eq!(
        release::texlab_asset(on("linux", "x64")),
        "texlab-x86_64-linux.tar.gz"
    );

    assert_eq!(
        release::tinymist_asset(on("darwin", "x64")),
        "tinymist-x86_64-apple-darwin.tar.gz"
    );
    assert_eq!(
        release::tinymist_asset(on("win32", "arm64")),
        "tinymist-aarch64-pc-windows-msvc.zip"
    );
    assert_eq!(
        release::tinymist_asset(on("linux", "x64")),
        "tinymist-x86_64-unknown-linux-gnu.tar.gz"
    );

    assert_eq!(
        release::terraform_build(on("win32", "x64")),
        ("amd64", "windows")
    );
    assert_eq!(
        release::terraform_build(on("darwin", "arm64")),
        ("arm64", "darwin")
    );

    assert_eq!(
        release::kotlin_asset(on("darwin", "arm64"), "0.253.10629").unwrap(),
        "kotlin-lsp-0.253.10629-mac-aarch64.zip"
    );
    assert_eq!(
        release::kotlin_asset(on("win32", "x64"), "0.253.10629").unwrap(),
        "kotlin-lsp-0.253.10629-win-x64.zip"
    );
    assert_eq!(release::kotlin_asset(on("linux", "ia32"), "1"), None);
}

#[test]
fn clangd_asset_prefers_zip_then_tar_xz() {
    let asset = |name: &str| Asset {
        name: name.into(),
        url: format!("https://example.invalid/{name}"),
    };
    let assets = [
        asset("clangd-linux-21.1.0.tar.xz"),
        asset("clangd-linux-21.1.0.zip"),
        asset("clangd-mac-21.1.0.tar.xz"),
        asset("clangd-windows-20.0.0.zip"),
        asset("clangd_indexing_tools-windows-21.1.0.7z"),
    ];
    let pick = |os| {
        let platform = Platform { os, arch: "x64" };
        release::clangd_asset(platform, "21.1.0", &assets).map(|asset| asset.name)
    };
    assert_eq!(pick("linux").unwrap(), "clangd-linux-21.1.0.zip");
    assert_eq!(pick("darwin").unwrap(), "clangd-mac-21.1.0.tar.xz");
    assert_eq!(
        pick("win32").unwrap(),
        "clangd_indexing_tools-windows-21.1.0.7z"
    );
    assert_eq!(pick("freebsd"), None);
}

#[test]
fn disable_flag_name_and_default() {
    assert_eq!(DISABLE_FLAG, "OPENCODE_DISABLE_LSP_DOWNLOAD");
    assert!(!Installer::new(false).disabled);
    assert_eq!(unit("csharp"), unit("razor"));
    assert_eq!(unit("rust"), None);
    assert_eq!(
        SERVERS.iter().filter(|def| unit(def.id).is_some()).count(),
        25
    );
}
