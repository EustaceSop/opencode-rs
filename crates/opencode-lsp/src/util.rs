use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::task::Poll;

use sha1::{Digest, Sha1};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub fn sha1_hex(input: &str) -> String {
    Sha1::digest(input.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut cmd = Command::new(program);
    cmd.kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    cmd
}

#[derive(Debug, Default)]
pub struct Output {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Output {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

#[derive(Default)]
pub struct RunOptions<'a> {
    pub cwd: Option<&'a Path>,
    pub env: Option<&'a HashMap<String, String>>,
    pub stdin: Option<&'a [u8]>,
}

// Spawn failures are folded into `code = 1`, as the callers in the source do.
pub async fn run(
    program: impl AsRef<OsStr>,
    args: &[impl AsRef<OsStr>],
    opts: RunOptions<'_>,
) -> Output {
    let mut cmd = command(program);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.stdin(if opts.stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    if let Some(cwd) = opts.cwd {
        cmd.current_dir(cwd);
    }
    if let Some(env) = opts.env {
        cmd.envs(env);
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            return Output {
                code: 1,
                stdout: Vec::new(),
                stderr: err.to_string(),
            };
        }
    };
    let stdin = child.stdin.take();
    let input = opts.stdin.map(<[u8]>::to_vec);
    let feed = async move {
        if let (Some(mut stdin), Some(input)) = (stdin, input) {
            let _ = stdin.write_all(&input).await;
            let _ = stdin.shutdown().await;
        }
    };
    let (_, result) = tokio::join!(feed, child.wait_with_output());
    match result {
        Ok(out) => Output {
            code: out.status.code().unwrap_or(1),
            stdout: out.stdout,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        },
        Err(err) => Output {
            code: 1,
            stdout: Vec::new(),
            stderr: err.to_string(),
        },
    }
}

// core/src/util/which.ts: PATH followed by the opencode bin directory.
pub fn which(name: &str, bin_dir: Option<&Path>) -> Option<PathBuf> {
    which_in(name, bin_dir, None)
}

// `path` replaces the process PATH when given.
pub fn which_in(name: &str, bin_dir: Option<&Path>, path: Option<&OsStr>) -> Option<PathBuf> {
    let mut paths: Vec<PathBuf> = path
        .map(OsStr::to_os_string)
        .or_else(|| std::env::var_os("PATH"))
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(bin) = bin_dir {
        paths.push(bin.to_path_buf());
    }
    let joined: OsString = std::env::join_paths(paths).ok()?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    which::which_in(name, Some(joined), cwd).ok()
}

// Drive every future concurrently on the current task; results keep the input order.
pub async fn join_all<'a, T>(
    mut futures: Vec<Pin<Box<dyn Future<Output = T> + Send + 'a>>>,
) -> Vec<T> {
    let mut out: Vec<Option<T>> = futures.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (slot, future) in out.iter_mut().zip(futures.iter_mut()) {
            if slot.is_some() {
                continue;
            }
            match future.as_mut().poll(cx) {
                Poll::Ready(value) => *slot = Some(value),
                Poll::Pending => pending = true,
            }
        }
        if pending {
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    })
    .await;
    out.into_iter().flatten().collect()
}

// Filesystem.up: first hit walking from `start` up to and including `stop`; targets are literal names.
pub fn up(targets: &[&str], start: &Path, stop: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        for target in targets {
            let search = current.join(target);
            if search.exists() {
                return Some(search);
            }
        }
        if current == stop {
            return None;
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => return None,
        }
    }
}

// Filesystem.findUp: every hit, nearest first.
pub fn find_up(targets: &[&str], start: &Path, stop: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut current = start.to_path_buf();
    loop {
        for target in targets {
            let search = current.join(target);
            if search.exists() {
                out.push(search);
            }
        }
        if current == stop {
            break;
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => break,
        }
    }
    out
}

pub fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub fn absolute(base: &Path, path: &Path) -> PathBuf {
    lexical_normalize(&if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    })
}

// Filesystem.normalizePath: identity off Windows; on Windows resolve + realpath (long names, real case).
pub fn normalize_path(path: &Path) -> PathBuf {
    if !cfg!(windows) {
        return path.to_path_buf();
    }
    let resolved = absolute(&std::env::current_dir().unwrap_or_default(), path);
    match std::fs::canonicalize(&resolved) {
        Ok(real) => strip_verbatim(real),
        Err(_) => resolved,
    }
}

pub fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest.to_string());
    }
    path
}

pub fn path_to_uri(path: &Path) -> String {
    url::Url::from_file_path(path)
        .map(String::from)
        .unwrap_or_else(|_| format!("file://{}", path.display()))
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    if !uri.starts_with("file://") {
        return None;
    }
    url::Url::parse(uri).ok()?.to_file_path().ok()
}

// FSUtil.contains: `child` is `parent` or below it.
pub fn contains(parent: &Path, child: &Path) -> bool {
    lexical_normalize(child).starts_with(lexical_normalize(parent))
}

// Node `path.extname`: last dot of the basename, ignoring a leading dot.
pub fn extname(file: &str) -> &str {
    let base = file.rsplit(['/', '\\']).next().unwrap_or(file);
    match base.rfind('.') {
        Some(idx) if idx > 0 => &base[idx..],
        _ => "",
    }
}

pub fn slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

// Module.resolve stand-in: node_modules lookup walking up from `dir`.
pub fn resolve_module(relative: &str, dir: &Path) -> Option<PathBuf> {
    let mut current = Some(dir);
    while let Some(dir) = current {
        let candidate = relative
            .split('/')
            .fold(dir.join("node_modules"), |p, s| p.join(s));
        if candidate.exists() {
            return Some(candidate);
        }
        current = dir.parent();
    }
    None
}

// Npm.which without the install step: reuse `<cache>/packages/<pkg>/node_modules/.bin/<bin>`.
pub fn npm_cached_bin(cache_dir: Option<&Path>, pkg: &str, bin: Option<&str>) -> Option<PathBuf> {
    let sanitized: String = if cfg!(windows) {
        pkg.chars()
            .map(|c| {
                if matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || (c as u32) < 32 {
                    '_'
                } else {
                    c
                }
            })
            .collect()
    } else {
        pkg.to_string()
    };
    let bin_dir = cache_dir?
        .join("packages")
        .join(sanitized)
        .join("node_modules")
        .join(".bin");
    let unscoped = pkg.rsplit('/').next().unwrap_or(pkg);
    let name = bin.unwrap_or(unscoped);
    let exts: &[&str] = if cfg!(windows) {
        &[".cmd", ".exe", ""]
    } else {
        &[""]
    };
    exts.iter()
        .map(|ext| bin_dir.join(format!("{name}{ext}")))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extname_matches_node() {
        assert_eq!(extname("a/b/c.ts"), ".ts");
        assert_eq!(extname("a/.gitignore"), "");
        assert_eq!(extname("Dockerfile"), "");
        assert_eq!(extname(r"C:\x\y.html.erb"), ".erb");
    }

    #[test]
    fn up_and_find_up() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("m.txt"), "").unwrap();
        std::fs::write(root.join("a/m.txt"), "").unwrap();
        assert_eq!(
            up(&["m.txt"], &root.join("a/b"), root),
            Some(root.join("a").join("m.txt"))
        );
        assert_eq!(find_up(&["m.txt"], &root.join("a/b"), root).len(), 2);
        assert_eq!(up(&["m.txt"], &root.join("a/b"), &root.join("a/b")), None);
    }

    #[test]
    fn uri_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let file = normalize_path(tmp.path()).join("x y.rs");
        let uri = path_to_uri(&file);
        assert!(uri.starts_with("file:///"));
        assert_eq!(uri_to_path(&uri).unwrap(), file);
        assert_eq!(uri_to_path("untitled:1"), None);
    }

    #[test]
    fn sha1_known() {
        assert_eq!(sha1_hex("abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }
}
