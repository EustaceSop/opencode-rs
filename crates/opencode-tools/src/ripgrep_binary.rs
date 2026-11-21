//! Managed ripgrep binary (`core/src/ripgrep/binary.ts`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};

use crate::ToolError;

pub const VERSION: &str = "15.1.0";
pub const RELEASES_URL: &str = "https://github.com/BurntSushi/ripgrep/releases/download";

static CACHE_DIR: RwLock<Option<PathBuf>> = RwLock::new(None);
static RESOLVED: Mutex<Option<PathBuf>> = Mutex::new(None);
static INSTALL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Sets the opencode cache directory (`Global.Path.cache`); the managed binary lives in `<cache>/bin`.
pub fn set_cache_dir(dir: PathBuf) {
    *CACHE_DIR.write().unwrap() = Some(dir);
    *RESOLVED.lock().unwrap() = None;
}

/// The configured cache directory, else `$XDG_CACHE_HOME/opencode`, else `~/.cache/opencode`.
pub fn cache_dir() -> PathBuf {
    if let Some(dir) = CACHE_DIR.read().unwrap().clone() {
        return dir;
    }
    let base = match std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        Some(v) => PathBuf::from(v),
        None => std::env::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".cache"),
    };
    base.join("opencode")
}

/// `Global.Path.bin`.
pub fn bin_dir() -> PathBuf {
    cache_dir().join("bin")
}

/// `util/which.ts`: `path` followed by `bin`.
pub fn which_in(cmd: &str, path: Option<OsString>, bin: &Path) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = path
        .filter(|p| !p.is_empty())
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    dirs.push(bin.to_path_buf());
    let joined = std::env::join_paths(dirs).ok()?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    which::which_in(cmd, Some(joined), cwd).ok()
}

/// `which(cmd)` over the process PATH plus `<cache>/bin`.
pub fn which(cmd: &str) -> Option<PathBuf> {
    which_in(cmd, std::env::var_os("PATH"), &bin_dir())
}

fn platform() -> Option<(&'static str, &'static str)> {
    Some(match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => ("aarch64-apple-darwin", "tar.gz"),
        ("aarch64", "linux") => ("aarch64-unknown-linux-gnu", "tar.gz"),
        ("x86_64", "macos") => ("x86_64-apple-darwin", "tar.gz"),
        ("x86_64", "linux") => ("x86_64-unknown-linux-musl", "tar.gz"),
        ("aarch64", "windows") => ("aarch64-pc-windows-msvc", "zip"),
        ("x86", "windows") => ("i686-pc-windows-msvc", "zip"),
        ("x86_64", "windows") => ("x86_64-pc-windows-msvc", "zip"),
        _ => return None,
    })
}

fn platform_key() -> String {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    };
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    format!("{arch}-{os}")
}

fn exe() -> &'static str {
    if cfg!(windows) { "rg.exe" } else { "rg" }
}

fn is_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file())
}

fn default_base() -> String {
    if cfg!(test) {
        return "http://127.0.0.1:9".to_string();
    }
    RELEASES_URL.to_string()
}

/// Resolves `rg`: PATH + `<bin>` lookup, then `<bin>/rg[.exe]`, then a download into `<bin>`.
#[derive(Debug, Clone)]
pub struct Installer {
    pub bin: PathBuf,
    pub base_url: String,
    pub path: Option<OsString>,
}

impl Installer {
    pub fn system() -> Self {
        Self {
            bin: bin_dir(),
            base_url: default_base(),
            path: std::env::var_os("PATH"),
        }
    }

    pub fn target(&self) -> PathBuf {
        self.bin.join(exe())
    }

    pub fn archive_name() -> Option<String> {
        platform().map(|(target, ext)| format!("ripgrep-{VERSION}-{target}.{ext}"))
    }

    /// The existing binary, without downloading.
    pub fn locate(&self) -> Option<PathBuf> {
        if let Some(system) = which_in(exe(), self.path.clone(), &self.bin)
            && is_file(&system)
        {
            return Some(system);
        }
        Some(self.target()).filter(|t| is_file(t))
    }

    pub async fn filepath(&self) -> Result<PathBuf, ToolError> {
        if let Some(found) = self.locate() {
            return Ok(found);
        }
        let _guard = INSTALL.lock().await;
        if let Some(found) = self.locate() {
            return Ok(found);
        }
        let (target, ext) = platform().ok_or_else(|| {
            ToolError::Other(format!(
                "unsupported platform for ripgrep: {}",
                platform_key()
            ))
        })?;
        let filename = format!("ripgrep-{VERSION}-{target}.{ext}");
        let url = format!(
            "{}/{VERSION}/{filename}",
            self.base_url.trim_end_matches('/')
        );
        let bytes = download(&url).await?;

        let io = |e: std::io::Error| ToolError::Other(format!("failed to install ripgrep: {e}"));
        tokio::fs::create_dir_all(&self.bin).await.map_err(io)?;
        let dir = self.bin.join(format!(
            "ripgrep-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        tokio::fs::create_dir_all(&dir).await.map_err(io)?;
        let result = self.unpack(&dir, &filename, target, ext, &bytes).await;
        let _ = tokio::fs::remove_dir_all(&dir).await;
        result?;
        Ok(self.target())
    }

    async fn unpack(
        &self,
        dir: &Path,
        filename: &str,
        target: &str,
        ext: &str,
        bytes: &[u8],
    ) -> Result<(), ToolError> {
        let io = |e: std::io::Error| ToolError::Other(format!("failed to install ripgrep: {e}"));
        let archive = dir.join(filename);
        tokio::fs::write(&archive, bytes).await.map_err(io)?;
        extract(&archive, ext, dir).await?;

        let extracted = dir.join(format!("ripgrep-{VERSION}-{target}")).join(exe());
        if !is_file(&extracted) {
            return Err(ToolError::Other(format!(
                "ripgrep archive did not contain executable: {}",
                extracted.display()
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&extracted, std::fs::Permissions::from_mode(0o755))
                .await
                .map_err(io)?;
        }
        let dest = self.target();
        match tokio::fs::rename(&extracted, &dest).await {
            Ok(()) => Ok(()),
            Err(_) if is_file(&dest) => Ok(()),
            Err(e) => Err(io(e)),
        }
    }
}

async fn download(url: &str) -> Result<Vec<u8>, ToolError> {
    let failed = |e: reqwest::Error| {
        let mut detail = e.to_string();
        let mut source = std::error::Error::source(&e);
        while let Some(inner) = source {
            detail.push_str(&format!(": {inner}"));
            source = inner.source();
        }
        ToolError::Other(format!(
            "failed to download ripgrep from {url}: {detail}. Check your network connection, or install ripgrep and add it to PATH."
        ))
    };
    let bytes = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(failed)?
        .bytes()
        .await
        .map_err(failed)?;
    if bytes.is_empty() {
        return Err(ToolError::Other(format!(
            "failed to download ripgrep from {url}"
        )));
    }
    Ok(bytes.to_vec())
}

async fn extract(archive: &Path, ext: &str, dir: &Path) -> Result<(), ToolError> {
    let mut command = if ext == "zip" {
        let shell = which("powershell.exe")
            .or_else(|| which("pwsh.exe"))
            .unwrap_or_else(|| PathBuf::from("powershell.exe"));
        let quote = |p: &Path| p.to_string_lossy().replace('\'', "''");
        let mut command = tokio::process::Command::new(shell);
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!(
                "$global:ProgressPreference = 'SilentlyContinue'; Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
                quote(archive),
                quote(dir)
            ),
        ]);
        command
    } else {
        let mut command = tokio::process::Command::new("tar");
        command.arg("-xzf").arg(archive).arg("-C").arg(dir);
        command
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    let output = command
        .output()
        .await
        .map_err(|e| ToolError::Other(format!("ripgrep extraction failed: {e}")))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let message = [stderr.trim(), stdout.trim()]
        .into_iter()
        .find(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            format!(
                "ripgrep extraction failed with code {}",
                output
                    .status
                    .code()
                    .map_or("null".to_string(), |c| c.to_string())
            )
        });
    Err(ToolError::Other(message))
}

/// `rg` from PATH or `<cache>/bin`, without downloading.
pub fn find_rg() -> Option<PathBuf> {
    Installer::system().locate()
}

/// `RipgrepBinary.filepath`: `rg` from PATH or `<cache>/bin`, downloaded on first need.
pub async fn filepath() -> Result<PathBuf, ToolError> {
    if let Some(found) = RESOLVED.lock().unwrap().clone() {
        return Ok(found);
    }
    let found = Installer::system().filepath().await?;
    *RESOLVED.lock().unwrap() = Some(found.clone());
    Ok(found)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    const FAKE: &[u8] = b"fake ripgrep binary";

    struct Server {
        base: String,
        hits: Arc<AtomicUsize>,
        paths: Arc<Mutex<Vec<String>>>,
    }

    async fn serve(status: &'static str, body: Vec<u8>) -> Server {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let paths = Arc::new(Mutex::new(Vec::new()));
        let (count, seen) = (hits.clone(), paths.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let (body, count, seen) = (body.clone(), count.clone(), seen.clone());
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => request.extend_from_slice(&buf[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&request);
                    let path = text.split_whitespace().nth(1).unwrap_or("").to_string();
                    seen.lock().unwrap().push(path);
                    count.fetch_add(1, Ordering::SeqCst);
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
        Server { base, hits, paths }
    }

    fn archive() -> Vec<u8> {
        let (target, ext) = platform().unwrap();
        let stage = tempfile::tempdir().unwrap();
        let name = format!("ripgrep-{VERSION}-{target}");
        std::fs::create_dir(stage.path().join(&name)).unwrap();
        std::fs::write(stage.path().join(&name).join(exe()), FAKE).unwrap();
        let out = stage.path().join(format!("{name}.{ext}"));
        let status = if ext == "zip" {
            std::process::Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command"])
                .arg(format!(
                    "$global:ProgressPreference = 'SilentlyContinue'; Compress-Archive -Path '{}' -DestinationPath '{}'",
                    stage.path().join(&name).display(),
                    out.display()
                ))
                .status()
                .unwrap()
        } else {
            std::process::Command::new("tar")
                .arg("-czf")
                .arg(&out)
                .arg("-C")
                .arg(stage.path())
                .arg(&name)
                .status()
                .unwrap()
        };
        assert!(status.success());
        std::fs::read(out).unwrap()
    }

    fn installer(server: &Server, cache: &Path, path: &Path) -> Installer {
        Installer {
            bin: cache.join("bin"),
            base_url: server.base.clone(),
            path: Some(path.as_os_str().to_os_string()),
        }
    }

    fn fake_rg(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let file = dir.join(exe());
        std::fs::write(&file, FAKE).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        file
    }

    fn entries(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[tokio::test]
    async fn downloads_extracts_and_reuses() {
        let server = serve("200 OK", archive()).await;
        let cache = tempfile::tempdir().unwrap();
        let empty = tempfile::tempdir().unwrap();
        let installer = installer(&server, cache.path(), empty.path());
        assert_eq!(installer.locate(), None);

        let found = installer.filepath().await.unwrap();
        assert_eq!(found, cache.path().join("bin").join(exe()));
        assert_eq!(std::fs::read(&found).unwrap(), FAKE);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&found).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
        assert_eq!(entries(&cache.path().join("bin")), vec![exe().to_string()]);
        assert_eq!(
            *server.paths.lock().unwrap(),
            vec![format!(
                "/{VERSION}/{}",
                Installer::archive_name().unwrap()
            )]
        );

        assert_eq!(installer.filepath().await.unwrap(), found);
        assert_eq!(installer.locate(), Some(found));
        assert_eq!(server.hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_callers_download_once() {
        let server = serve("200 OK", archive()).await;
        let cache = tempfile::tempdir().unwrap();
        let empty = tempfile::tempdir().unwrap();
        let installer = installer(&server, cache.path(), empty.path());
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let installer = installer.clone();
                tokio::spawn(async move { installer.filepath().await })
            })
            .collect();
        for task in tasks {
            assert_eq!(
                task.await.unwrap().unwrap(),
                cache.path().join("bin").join(exe())
            );
        }
        assert_eq!(server.hits.load(Ordering::SeqCst), 1);
        assert_eq!(entries(&cache.path().join("bin")), vec![exe().to_string()]);
    }

    #[tokio::test]
    async fn path_is_preferred_over_the_managed_copy() {
        let server = serve("200 OK", archive()).await;
        let cache = tempfile::tempdir().unwrap();
        let system = tempfile::tempdir().unwrap();
        let empty = tempfile::tempdir().unwrap();
        let on_path = fake_rg(system.path());
        let managed = fake_rg(&cache.path().join("bin"));

        let with_path = installer(&server, cache.path(), system.path());
        assert_eq!(with_path.filepath().await.unwrap(), on_path);
        let without = installer(&server, cache.path(), empty.path());
        assert_eq!(without.filepath().await.unwrap(), managed);
        assert_eq!(server.hits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn which_appends_bin_after_path() {
        let first = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let managed = fake_rg(bin.path());
        assert_eq!(which_in(exe(), None, bin.path()), Some(managed.clone()));
        assert_eq!(
            which_in(exe(), Some(OsString::new()), bin.path()),
            Some(managed.clone())
        );
        let path = Some(first.path().as_os_str().to_os_string());
        assert_eq!(which_in(exe(), path.clone(), bin.path()), Some(managed));
        let system = fake_rg(first.path());
        assert_eq!(which_in(exe(), path, bin.path()), Some(system));
    }

    #[tokio::test]
    async fn reports_download_failures() {
        let cache = tempfile::tempdir().unwrap();
        let empty = tempfile::tempdir().unwrap();
        let name = Installer::archive_name().unwrap();

        let missing = serve("404 Not Found", b"nope".to_vec()).await;
        let err = installer(&missing, cache.path(), empty.path())
            .filepath()
            .await
            .unwrap_err()
            .to_string();
        let url = format!("{}/{VERSION}/{name}", missing.base);
        assert!(
            err.starts_with(&format!("failed to download ripgrep from {url}: ")),
            "{err}"
        );
        assert!(err.contains("404"), "{err}");
        assert!(err.ends_with("install ripgrep and add it to PATH."), "{err}");

        let blank = serve("200 OK", Vec::new()).await;
        let err = installer(&blank, cache.path(), empty.path())
            .filepath()
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            format!(
                "failed to download ripgrep from {}/{VERSION}/{name}",
                blank.base
            )
        );

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let offline = Server {
            base: format!("http://{}", listener.local_addr().unwrap()),
            hits: Arc::default(),
            paths: Arc::default(),
        };
        drop(listener);
        let err = installer(&offline, cache.path(), empty.path())
            .filepath()
            .await
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("failed to download ripgrep from "), "{err}");
        assert!(!cache.path().join("bin").exists());
    }

    #[tokio::test]
    async fn rejects_archives_without_the_executable() {
        let server = serve("200 OK", b"not an archive".to_vec()).await;
        let cache = tempfile::tempdir().unwrap();
        let empty = tempfile::tempdir().unwrap();
        let installer = installer(&server, cache.path(), empty.path());
        assert!(installer.filepath().await.is_err());
        assert_eq!(entries(&cache.path().join("bin")), Vec::<String>::new());
    }
}
