//! Managed portable bash (Rust build only): the official MinGit zip of the latest Git for Windows release, unpacked
//! into `<cache>/opencode-rs/mingit/<version>/`. Nothing is installed; it is downloaded only after the user agreed.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ToolError;

pub const RELEASES_API: &str = "https://api.github.com/repos/git-for-windows/git/releases/latest";
pub const RELEASES_PAGE: &str = "https://github.com/git-for-windows/git/releases";
/// The consent question, shared by every UI.
pub const OFFER: &str = "Download the official portable bash (Git for Windows MinGit, ~38 MB) so the AI can run commands reliably. No install, no admin rights, nothing outside opencode's cache is changed.";
/// The one-line CLI notice while the question is unanswered.
pub const HINT: &str = "No bash found: run `opencode setup bash` to download a portable one (Git for Windows MinGit, ~38 MB) so the AI can run commands reliably.";

static INSTALL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// `<cache>/../opencode-rs/mingit` for an opencode cache dir such as `~/.cache/opencode`.
pub fn root_in(cache: &Path) -> PathBuf {
    cache.with_file_name("opencode-rs").join("mingit")
}

/// The managed copies' root, beside the cache dir of the managed ripgrep.
pub fn root() -> PathBuf {
    root_in(&crate::ripgrep_binary::cache_dir())
}

/// The shell of one unpacked copy: `usr/bin/bash.exe`, else `usr/bin/sh.exe` (MinGit's sh is GNU bash).
pub fn shell_in(dir: &Path) -> Option<PathBuf> {
    let bin = dir.join("usr").join("bin");
    ["bash.exe", "sh.exe"].iter().map(|name| bin.join(name)).find(|file| std::fs::metadata(file).is_ok_and(|m| m.is_file() && m.len() > 0))
}

fn version_key(version: &str) -> Vec<u64> {
    version.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).filter_map(|s| s.parse().ok()).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub version: String,
    pub dir: PathBuf,
    pub shell: PathBuf,
}

/// The newest unpacked copy under `root`.
pub fn installed_in(root: &Path) -> Option<Installed> {
    std::fs::read_dir(root)
        .ok()?
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|entry| {
            let version = entry.file_name().to_string_lossy().into_owned();
            if version.starts_with('.') {
                return None;
            }
            let dir = entry.path();
            let shell = shell_in(&dir)?;
            Some(Installed { version, dir, shell })
        })
        .max_by(|a, b| version_key(&a.version).cmp(&version_key(&b.version)).then_with(|| a.version.cmp(&b.version)))
}

/// The managed shell when one was downloaded before.
pub fn installed() -> Option<Installed> {
    if !cfg!(windows) {
        return None;
    }
    installed_in(&root())
}

/// PATH for a shell process started from `<dir>/usr/bin/{bash,sh}.exe`: `<dir>\usr\bin` and `<dir>\cmd` first.
pub fn path_env(shell: &Path) -> Option<String> {
    let bin = shell.parent()?;
    let usr = bin.parent()?;
    let named = |p: &Path, name: &str| p.file_name().is_some_and(|n| n.eq_ignore_ascii_case(name));
    if !named(bin, "bin") || !named(usr, "usr") {
        return None;
    }
    let stem = shell.file_stem()?.to_string_lossy().to_lowercase();
    if stem != "bash" && stem != "sh" {
        return None;
    }
    let dir = usr.parent()?;
    let mut dirs: Vec<PathBuf> = vec![bin.to_path_buf()];
    let cmd = dir.join("cmd");
    if cmd.is_dir() {
        dirs.push(cmd);
    }
    dirs.extend(std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default());
    std::env::join_paths(dirs).ok().map(|p| p.to_string_lossy().into_owned())
}

fn asset_suffix() -> Option<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Some("-64-bit.zip"),
        "aarch64" => Some("-arm64.zip"),
        "x86" => Some("-32-bit.zip"),
        _ => None,
    }
}

/// One downloadable MinGit zip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub asset: String,
    pub url: String,
    pub size: u64,
    /// Lower-case hex from the asset's `digest` field, when GitHub provides one.
    pub sha256: Option<String>,
}

/// The plain (non-busybox) MinGit zip whose name ends with `suffix`, from a `releases/latest` response.
pub fn pick(release: &serde_json::Value, suffix: &str) -> Result<Release, ToolError> {
    let tag = release["tag_name"].as_str().ok_or_else(|| ToolError::other("Git for Windows release has no tag_name"))?;
    let version = tag.trim_start_matches('v').to_string();
    if version.is_empty() || !version.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')) {
        return Err(ToolError::Other(format!("unexpected Git for Windows release tag: {tag}")));
    }
    let asset = release["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|asset| asset["name"].as_str().is_some_and(|name| name.starts_with("MinGit-") && name.ends_with(suffix) && !name.contains("busybox")))
        .ok_or_else(|| ToolError::Other(format!("Git for Windows {tag} has no MinGit-*{suffix} asset")))?;
    let field = |key: &str| asset[key].as_str().map(str::to_string).ok_or_else(|| ToolError::Other(format!("MinGit asset has no {key}")));
    let sha256 = asset["digest"].as_str().and_then(|d| d.strip_prefix("sha256:")).map(str::to_lowercase).filter(|h| h.len() == 64);
    Ok(Release { version, asset: field("name")?, url: field("browser_download_url")?, size: asset["size"].as_u64().unwrap_or(0), sha256 })
}

/// How the downloaded zip was checked against the release metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verified {
    Sha256,
    /// The release carried no digest; only the byte size matched.
    Size,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub installed: Installed,
    pub release: Release,
    /// `None` when that version was already unpacked and nothing was downloaded.
    pub verified: Option<Verified>,
}

/// Size and digest checks of a downloaded zip.
pub fn verify(bytes: &[u8], release: &Release) -> Result<Verified, ToolError> {
    if release.size > 0 && bytes.len() as u64 != release.size {
        return Err(ToolError::Other(format!("MinGit download is {} bytes, the release lists {}", bytes.len(), release.size)));
    }
    let Some(expected) = &release.sha256 else {
        if release.size == 0 {
            return Err(ToolError::other("the MinGit release lists neither a digest nor a size; refusing to unpack it"));
        }
        return Ok(Verified::Size);
    };
    let actual = format!("{:x}", Sha256::digest(bytes));
    if &actual != expected {
        return Err(ToolError::Other(format!("MinGit sha256 mismatch: expected {expected}, got {actual}")));
    }
    Ok(Verified::Sha256)
}

fn default_api() -> String {
    if cfg!(test) {
        return "http://127.0.0.1:9/latest".to_string();
    }
    RELEASES_API.to_string()
}

fn failure(what: &str, url: &str, error: reqwest::Error) -> ToolError {
    let mut detail = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(inner) = source {
        detail.push_str(&format!(": {inner}"));
        source = inner.source();
    }
    ToolError::Other(format!("failed to {what} {url}: {detail}"))
}

#[derive(Debug, Clone)]
pub struct Installer {
    pub root: PathBuf,
    pub api_url: String,
}

impl Installer {
    pub fn system() -> Self {
        Self { root: root(), api_url: default_api() }
    }

    pub fn new(root: PathBuf) -> Self {
        Self { root, api_url: default_api() }
    }

    pub fn installed(&self) -> Option<Installed> {
        installed_in(&self.root)
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder().user_agent(concat!("opencode-rs/", env!("CARGO_PKG_VERSION"))).build().unwrap_or_default()
    }

    /// The MinGit zip of the latest release for this machine's architecture.
    pub async fn latest(&self) -> Result<Release, ToolError> {
        let suffix = asset_suffix().ok_or_else(|| ToolError::Other(format!("no MinGit build for {}", std::env::consts::ARCH)))?;
        let mut request = Self::client().get(&self.api_url).header("accept", "application/vnd.github+json");
        // Anonymous API calls are limited to 60 an hour per address; a token lifts that.
        let token = ["GITHUB_TOKEN", "GH_TOKEN"].iter().find_map(|key| std::env::var(key).ok().filter(|t| !t.trim().is_empty()));
        if let Some(token) = &token {
            request = request.bearer_auth(token.trim());
        }
        let response = request.send().await.map_err(|e| failure("query", &self.api_url, e))?;
        let limited = response.headers().get("x-ratelimit-remaining").is_some_and(|v| v.as_bytes() == b"0");
        if limited || (token.is_none() && matches!(response.status().as_u16(), 403 | 429)) {
            let reset = response.headers().get("x-ratelimit-reset").and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let wait = reset.map(|at| format!(" in about {} min", at.saturating_sub(now).div_ceil(60).max(1))).unwrap_or_default();
            return Err(ToolError::Other(format!(
                "GitHub's limit for anonymous requests from this network was reached ({}). Try again{wait}, or set GITHUB_TOKEN to a GitHub token.",
                response.status()
            )));
        }
        let response = response.error_for_status().map_err(|e| failure("query", &self.api_url, e))?;
        let body = response.bytes().await.map_err(|e| failure("read", &self.api_url, e))?;
        let value: serde_json::Value = serde_json::from_slice(&body).map_err(|e| ToolError::Other(format!("invalid release JSON from {}: {e}", self.api_url)))?;
        pick(&value, suffix)
    }

    async fn download(&self, release: &Release, progress: &(dyn Fn(u64, u64) + Send + Sync)) -> Result<Vec<u8>, ToolError> {
        let mut response = Self::client().get(&release.url).send().await.and_then(|r| r.error_for_status()).map_err(|e| failure("download", &release.url, e))?;
        let total = response.content_length().unwrap_or(release.size);
        let mut bytes = Vec::with_capacity(total as usize);
        while let Some(chunk) = response.chunk().await.map_err(|e| failure("download", &release.url, e))? {
            bytes.extend_from_slice(&chunk);
            progress(bytes.len() as u64, total);
        }
        Ok(bytes)
    }

    /// Downloads, verifies and unpacks the latest MinGit, once per process at a time; an unpacked copy of the same
    /// version is reused.
    pub async fn install(&self, progress: &(dyn Fn(u64, u64) + Send + Sync)) -> Result<Outcome, ToolError> {
        let _guard = INSTALL.lock().await;
        let release = self.latest().await?;
        let dest = self.root.join(&release.version);
        if let Some(shell) = shell_in(&dest) {
            return Ok(Outcome { installed: Installed { version: release.version.clone(), dir: dest, shell }, release, verified: None });
        }
        let bytes = self.download(&release, progress).await?;
        let verified = verify(&bytes, &release)?;
        let io = |e: std::io::Error| ToolError::Other(format!("failed to unpack MinGit: {e}"));
        tokio::fs::create_dir_all(&self.root).await.map_err(io)?;
        let stage = self.root.join(format!(".unpack-{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
        let target = stage.clone();
        let unpacked = tokio::task::spawn_blocking(move || unpack(&bytes, &target)).await.map_err(|e| ToolError::Other(e.to_string()))?;
        let moved = match unpacked {
            Ok(()) => match std::fs::rename(&stage, &dest) {
                Ok(()) => Ok(()),
                Err(_) if shell_in(&dest).is_some() => Ok(()),
                Err(e) => Err(io(e)),
            },
            Err(e) => Err(e),
        };
        let _ = tokio::fs::remove_dir_all(&stage).await;
        moved?;
        let shell = shell_in(&dest).ok_or_else(|| ToolError::other("MinGit was unpacked but its shell is missing"))?;
        Ok(Outcome { installed: Installed { version: release.version.clone(), dir: dest, shell }, release, verified: Some(verified) })
    }
}

fn unpack(bytes: &[u8], dir: &Path) -> Result<(), ToolError> {
    let fail = |e: &dyn std::fmt::Display| ToolError::Other(format!("failed to unpack MinGit: {e}"));
    std::fs::create_dir_all(dir).map_err(|e| fail(&e))?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| fail(&e))?;
    archive.extract(dir).map_err(|e| fail(&e))?;
    if shell_in(dir).is_none() {
        return Err(ToolError::other("the MinGit archive has no usr/bin/sh.exe"));
    }
    Ok(())
}

/// The answer to the download question, kept in `<state>/setup-rs.json` as `{"bash": "accepted"|"declined"|null}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Consent {
    Accepted,
    Declined,
}

pub fn setup_file(state: &Path) -> PathBuf {
    state.join("setup-rs.json")
}

pub fn consent(state: &Path) -> Option<Consent> {
    let text = std::fs::read_to_string(setup_file(state)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    serde_json::from_value(value.get("bash")?.clone()).ok()
}

/// Records the answer; other keys of the file are kept.
pub fn set_consent(state: &Path, answer: Option<Consent>) -> std::io::Result<()> {
    let file = setup_file(state);
    let mut value = std::fs::read_to_string(&file).ok().and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok()).filter(|v| v.is_object()).unwrap_or_else(|| serde_json::json!({}));
    value["bash"] = serde_json::to_value(answer).map_err(std::io::Error::other)?;
    std::fs::create_dir_all(state)?;
    std::fs::write(&file, serde_json::to_string_pretty(&value).map_err(std::io::Error::other)?)
}

/// Windows without Git Bash and without a managed copy under `root`.
pub fn bash_missing_in(root: &Path) -> bool {
    cfg!(windows) && crate::shell::gitbash().is_none() && installed_in(root).is_none()
}

/// Whether a UI should ask about the download: no bash on this machine and no answer recorded yet.
pub fn should_offer(state: &Path, cache: &Path) -> bool {
    consent(state).is_none() && bash_missing_in(&root_in(cache))
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::sync::{Arc, Mutex};

    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    const SHELL: &[u8] = b"MZ fake bash";

    fn mingit_zip(with_shell: bool) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        if with_shell {
            writer.start_file("usr/bin/sh.exe", options).unwrap();
            writer.write_all(SHELL).unwrap();
        }
        writer.start_file("cmd/git.exe", options).unwrap();
        writer.write_all(b"MZ fake git").unwrap();
        writer.start_file("etc/gitconfig", options).unwrap();
        writer.write_all(b"[core]\n").unwrap();
        writer.finish().unwrap().into_inner()
    }

    struct Server {
        base: String,
        paths: Arc<Mutex<Vec<String>>>,
    }

    // Serves `/latest` (release JSON whose assets point back here) and `/MinGit-*` (the zip).
    async fn serve(zip: Vec<u8>, digest: Option<String>, size: Option<u64>) -> Server {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let paths = Arc::new(Mutex::new(Vec::new()));
        let seen = paths.clone();
        let size = size.unwrap_or(zip.len() as u64);
        let asset = |name: &str| json!({"name": name, "size": size, "digest": digest.as_ref().map(|d| format!("sha256:{d}")), "browser_download_url": format!("{base}/{name}")});
        let suffix = asset_suffix().unwrap();
        let release = json!({
            "tag_name": "v2.56.0.windows.4",
            "assets": [asset(&format!("MinGit-2.56.0.4-busybox{suffix}")), asset("Git-2.56.0.4-64-bit.exe"), asset(&format!("MinGit-2.56.0.4{suffix}"))]
        })
        .to_string();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (zip, release, seen) = (zip.clone(), release.clone(), seen.clone());
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => request.extend_from_slice(&buf[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&request).to_string();
                    let path = text.split_whitespace().nth(1).unwrap_or("").to_string();
                    seen.lock().unwrap().push(path.clone());
                    let body = if path == "/latest" { release.into_bytes() } else { zip };
                    let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Server { base, paths }
    }

    fn installer(server: &Server, root: &Path) -> Installer {
        Installer { root: root.to_path_buf(), api_url: format!("{}/latest", server.base) }
    }

    fn sha(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn none(_: u64, _: u64) {}

    #[tokio::test]
    async fn rate_limited_queries_say_when_to_retry() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_url = format!("http://{}/latest", listener.local_addr().unwrap());
        let reset = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 600;
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = socket.read(&mut buf).await;
            let head = format!("HTTP/1.1 403 Forbidden\r\nx-ratelimit-remaining: 0\r\nx-ratelimit-reset: {reset}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = socket.write_all(head.as_bytes()).await;
        });
        let root = tempfile::tempdir().unwrap();
        let error = Installer { root: root.path().to_path_buf(), api_url }.latest().await.unwrap_err().to_string();
        assert!(error.contains("limit for anonymous requests") && error.contains("in about 10 min") && error.contains("GITHUB_TOKEN"), "{error}");
    }

    #[tokio::test]
    async fn downloads_verifies_unpacks_and_reuses() {
        let zip = mingit_zip(true);
        let server = serve(zip.clone(), Some(sha(&zip)), None).await;
        let root = tempfile::tempdir().unwrap();
        let installer = installer(&server, root.path());
        assert_eq!(installer.installed(), None);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        let outcome = installer.install(&move |done, total| record.lock().unwrap().push((done, total))).await.unwrap();
        assert_eq!(outcome.verified, Some(Verified::Sha256));
        assert_eq!(outcome.release.version, "2.56.0.windows.4");
        assert!(!outcome.release.asset.contains("busybox"), "{}", outcome.release.asset);
        let dir = root.path().join("2.56.0.windows.4");
        assert_eq!(outcome.installed, Installed { version: "2.56.0.windows.4".into(), dir: dir.clone(), shell: dir.join("usr").join("bin").join("sh.exe") });
        assert_eq!(std::fs::read(&outcome.installed.shell).unwrap(), SHELL);
        assert!(dir.join("cmd").join("git.exe").is_file());
        assert_eq!(seen.lock().unwrap().last().map(|p| p.0), Some(zip.len() as u64));
        // Only the version directory remains; the staging directory is gone.
        let names: Vec<String> = std::fs::read_dir(root.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["2.56.0.windows.4"]);
        assert_eq!(installer.installed(), Some(outcome.installed.clone()));

        let again = installer.install(&none).await.unwrap();
        assert_eq!((again.verified, again.installed), (None, outcome.installed));
        let fetched: Vec<String> = server.paths.lock().unwrap().iter().filter(|p| p.starts_with("/MinGit")).cloned().collect();
        assert_eq!(fetched.len(), 1, "{fetched:?}");
    }

    #[tokio::test]
    async fn digest_mismatch_and_size_only_releases() {
        let zip = mingit_zip(true);
        let root = tempfile::tempdir().unwrap();
        let bad = serve(zip.clone(), Some("0".repeat(64)), None).await;
        let err = installer(&bad, root.path()).install(&none).await.unwrap_err().to_string();
        assert!(err.starts_with("MinGit sha256 mismatch"), "{err}");
        assert!(installed_in(root.path()).is_none() && std::fs::read_dir(root.path()).map_or(0, |d| d.count()) == 0);

        let short = serve(zip.clone(), None, Some(zip.len() as u64 + 1)).await;
        let err = installer(&short, root.path()).install(&none).await.unwrap_err().to_string();
        assert!(err.contains("bytes, the release lists"), "{err}");

        let sized = serve(zip, None, None).await;
        let outcome = installer(&sized, root.path()).install(&none).await.unwrap();
        assert_eq!(outcome.verified, Some(Verified::Size));
    }

    #[tokio::test]
    async fn archives_without_a_shell_are_rejected() {
        let zip = mingit_zip(false);
        let server = serve(zip.clone(), Some(sha(&zip)), None).await;
        let root = tempfile::tempdir().unwrap();
        let err = installer(&server, root.path()).install(&none).await.unwrap_err().to_string();
        assert_eq!(err, "the MinGit archive has no usr/bin/sh.exe");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_installs_download_once() {
        let zip = mingit_zip(true);
        let server = serve(zip.clone(), Some(sha(&zip)), None).await;
        let root = tempfile::tempdir().unwrap();
        let installer = installer(&server, root.path());
        let tasks: Vec<_> = (0..6)
            .map(|_| {
                let installer = installer.clone();
                tokio::spawn(async move { installer.install(&none).await.map(|o| o.installed) })
            })
            .collect();
        for task in tasks {
            assert!(task.await.unwrap().is_ok());
        }
        let fetched = server.paths.lock().unwrap().iter().filter(|p| p.starts_with("/MinGit")).count();
        assert_eq!(fetched, 1);
    }

    #[test]
    fn picks_the_plain_zip_and_reads_the_digest() {
        let release = json!({"tag_name": "v2.56.0.windows.4", "assets": [
            {"name": "MinGit-2.56.0.4-busybox-64-bit.zip", "size": 1, "browser_download_url": "u1"},
            {"name": "MinGit-2.56.0.4-64-bit.zip", "size": 39846926, "digest": "sha256:E3BF78351FE777FBA47DDEE5950082F8A98BB0F02604FDC7BA3EE2CB68FFA877", "browser_download_url": "u2"},
            {"name": "MinGit-2.56.0.4-arm64.zip", "size": 3, "browser_download_url": "u3"}
        ]});
        let x64 = pick(&release, "-64-bit.zip").unwrap();
        assert_eq!((x64.asset.as_str(), x64.url.as_str(), x64.size), ("MinGit-2.56.0.4-64-bit.zip", "u2", 39846926));
        assert_eq!(x64.sha256.as_deref(), Some("e3bf78351fe777fba47ddee5950082f8a98bb0f02604fdc7ba3ee2cb68ffa877"));
        let arm = pick(&release, "-arm64.zip").unwrap();
        assert_eq!((arm.url.as_str(), arm.sha256), ("u3", None));
        assert!(pick(&json!({"tag_name": "v1", "assets": []}), "-64-bit.zip").is_err());
        assert!(pick(&json!({"tag_name": "../x", "assets": []}), "-64-bit.zip").is_err());
    }

    #[test]
    fn newest_unpacked_copy_and_its_path() {
        let root = tempfile::tempdir().unwrap();
        for (version, file) in [("2.9.0.windows.1", "sh.exe"), ("2.56.0.windows.4", "bash.exe"), ("2.60.0.windows.1", "none"), (".unpack-1-0", "sh.exe")] {
            let bin = root.path().join(version).join("usr").join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::create_dir_all(root.path().join(version).join("cmd")).unwrap();
            std::fs::write(bin.join(file), "x").unwrap();
        }
        let found = installed_in(root.path()).unwrap();
        assert_eq!(found.version, "2.56.0.windows.4");
        assert_eq!(found.shell, root.path().join("2.56.0.windows.4").join("usr").join("bin").join("bash.exe"));
        let path = path_env(&found.shell).unwrap();
        let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
        assert_eq!(dirs[..2], [found.dir.join("usr").join("bin"), found.dir.join("cmd")]);
        assert_eq!(path_env(Path::new("C:/Program Files/Git/bin/bash.exe")), None);
        assert_eq!(path_env(&found.dir.join("usr").join("bin").join("ls.exe")), None);
        assert_eq!(root_in(Path::new("/c/opencode")), Path::new("/c/opencode-rs/mingit"));
    }

    #[test]
    fn consent_round_trips_and_keeps_other_keys() {
        let state = tempfile::tempdir().unwrap();
        assert_eq!(consent(state.path()), None);
        std::fs::write(setup_file(state.path()), r#"{"other": 1, "bash": null}"#).unwrap();
        assert_eq!(consent(state.path()), None);
        set_consent(state.path(), Some(Consent::Declined)).unwrap();
        assert_eq!(consent(state.path()), Some(Consent::Declined));
        set_consent(state.path(), Some(Consent::Accepted)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(setup_file(state.path())).unwrap()).unwrap();
        assert_eq!(value, json!({"other": 1, "bash": "accepted"}));
        set_consent(state.path(), None).unwrap();
        assert_eq!(consent(state.path()), None);
        // An answer, or a non-Windows machine, means there is nothing to offer.
        let cache = tempfile::tempdir().unwrap();
        set_consent(state.path(), Some(Consent::Declined)).unwrap();
        assert!(!should_offer(state.path(), &cache.path().join("opencode")));
        if !cfg!(windows) {
            set_consent(state.path(), None).unwrap();
            assert!(!should_offer(state.path(), &cache.path().join("opencode")));
        }
    }
}
