use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use sha1::{Digest, Sha1};

use crate::types::GLOBAL_PROJECT_ID;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    pub worktree: PathBuf,
    pub git_directory: PathBuf,
    pub common_directory: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProject {
    pub id: String,
    pub previous: Option<String>,
    pub directory: PathBuf,
    // Git common directory holding the cached id file; `None` when not in a repository.
    pub store: Option<PathBuf>,
}

impl ResolvedProject {
    pub fn is_git(&self) -> bool {
        self.store.is_some()
    }
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !result.pop() {
                    result.push("..");
                }
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

fn resolve_path(cwd: &Path, value: &str) -> PathBuf {
    let trimmed = value.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        return cwd.to_path_buf();
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        return normalize(path);
    }
    normalize(&cwd.join(path))
}

pub fn discover(directory: &Path) -> Option<Repository> {
    let cwd = directory
        .ancestors()
        .find(|dir| dir.join(".git").exists())?;
    let top_level = git(cwd, &["rev-parse", "--show-toplevel"]);
    let git_dir = git(cwd, &["rev-parse", "--git-dir"])?;
    let common_dir = git(cwd, &["rev-parse", "--git-common-dir"])?;
    Some(Repository {
        worktree: top_level.map_or_else(|| cwd.to_path_buf(), |text| resolve_path(cwd, &text)),
        git_directory: resolve_path(cwd, &git_dir),
        common_directory: resolve_path(cwd, &common_dir),
    })
}

fn remote_parts(host: &str, name: &str) -> Option<String> {
    let path = name.trim_start_matches('/');
    let path = path
        .strip_suffix(".git/")
        .or_else(|| path.strip_suffix(".git"))
        .unwrap_or(path);
    let path = path.trim_end_matches('/');
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some(format!("{}/{path}", host.to_lowercase()))
}

// scp-like form `^([^@/:]+@)?([^/:]+):(.+)$`.
fn scp_parts(value: &str) -> Option<(&str, &str)> {
    let rest = match value.find('@') {
        Some(at) if at > 0 && !value[..at].contains(['/', ':']) => {
            let tail = &value[at + 1..];
            let host_len = tail.find(['/', ':']).unwrap_or(tail.len());
            if host_len > 0 && tail[host_len..].starts_with(':') && tail.len() > host_len + 1 {
                tail
            } else {
                value
            }
        }
        _ => value,
    };
    let host_len = rest.find(['/', ':'])?;
    if host_len == 0 || !rest[host_len..].starts_with(':') {
        return None;
    }
    let path = &rest[host_len + 1..];
    if path.is_empty() || path.contains(['\n', '\r']) {
        return None;
    }
    Some((&rest[..host_len], path))
}

pub fn normalize_remote(input: &str) -> Option<String> {
    let value = input.trim();
    if value.is_empty() {
        return None;
    }
    match url::Url::parse(value) {
        Ok(parsed) => {
            if parsed.scheme() == "file" {
                return None;
            }
            remote_parts(parsed.host_str().unwrap_or(""), parsed.path())
        }
        Err(_) => {
            let (host, path) = scp_parts(value)?;
            remote_parts(host, path)
        }
    }
}

pub fn remote_id(origin: &str) -> Option<String> {
    let normalized = normalize_remote(origin)?;
    let digest = Sha1::digest(format!("git-remote:{normalized}").as_bytes());
    Some(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub fn cached_id(common_directory: &Path) -> Option<String> {
    let text = fs::read_to_string(common_directory.join("opencode")).ok()?;
    let value = text.trim();
    (!value.is_empty()).then(|| value.to_string())
}

pub fn root_commit(repository: &Repository) -> Option<String> {
    let text = git(
        &repository.worktree,
        &["rev-list", "--max-parents=0", "HEAD"],
    )?;
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .min()
        .map(str::to_string)
}

pub fn resolve(directory: &Path) -> ResolvedProject {
    let Some(repository) = discover(directory) else {
        let root = directory.ancestors().last().unwrap_or(directory);
        return ResolvedProject {
            id: GLOBAL_PROJECT_ID.to_string(),
            previous: None,
            directory: root.to_path_buf(),
            store: None,
        };
    };
    let previous = cached_id(&repository.common_directory);
    let remote = git(&repository.worktree, &["remote", "get-url", "origin"])
        .as_deref()
        .and_then(remote_id);
    let id = remote
        .or_else(|| previous.clone())
        .or_else(|| root_commit(&repository))
        .unwrap_or_else(|| GLOBAL_PROJECT_ID.to_string());
    ResolvedProject {
        id,
        previous,
        directory: repository.worktree,
        store: Some(repository.common_directory),
    }
}

// Persists the id to `<git-common-dir>/opencode`; failures are ignored as in the original.
pub fn commit(store: &Path, id: &str) {
    let _ = fs::write(store.join("opencode"), id);
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPENCODE_REMOTE_ID: &str = "476fbfe6befa9e63ddd7f9ae71655999a0104256";

    fn run(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
            ])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .output()
            .expect("git must be installed for these tests");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn repo_with_commit() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "-q"]);
        fs::write(dir.path().join("a.txt"), "a").unwrap();
        run(dir.path(), &["add", "."]);
        run(dir.path(), &["commit", "-q", "-m", "root"]);
        let root = run(dir.path(), &["rev-list", "--max-parents=0", "HEAD"]);
        (dir, root)
    }

    fn same_path(left: &Path, right: &Path) -> bool {
        fs::canonicalize(left).unwrap() == fs::canonicalize(right).unwrap()
    }

    #[test]
    fn remote_normalisation_matches_reference() {
        let cases = [
            (
                "https://GitHub.com/sst/opencode.git",
                Some("github.com/sst/opencode"),
            ),
            (
                "ssh://git@github.com:22/sst/opencode.git/",
                Some("github.com/sst/opencode"),
            ),
            (
                "git@github.com:sst/opencode.git",
                Some("github.com/sst/opencode"),
            ),
            (
                "git@GitHub.com:/sst/opencode.git/",
                Some("github.com/sst/opencode"),
            ),
            (
                "https://github.com//sst/opencode///",
                Some("github.com/sst/opencode"),
            ),
            (
                "https://user:pw@example.com:8443/a/b.git",
                Some("example.com/a/b"),
            ),
            ("git://[::1]/a/b", Some("[::1]/a/b")),
            (
                "  git@github.com:sst/opencode.git\n",
                Some("github.com/sst/opencode"),
            ),
            ("github.com:sst/opencode", None),
            ("file:///tmp/x", None),
            ("C:\\repo\\x", None),
            ("/tmp/local/repo", None),
            ("../relative/repo", None),
            ("", None),
        ];
        for (input, expected) in cases {
            assert_eq!(normalize_remote(input).as_deref(), expected, "{input}");
        }
    }

    #[test]
    fn remote_id_is_sha1_of_normalised_remote() {
        assert_eq!(
            remote_id("git@github.com:sst/opencode.git").as_deref(),
            Some(OPENCODE_REMOTE_ID)
        );
        assert_eq!(
            remote_id("https://user:pw@example.com:8443/a/b.git").as_deref(),
            Some("040ceb89ec73905500f1df19cfe83b6a592d796a")
        );
    }

    #[test]
    fn directory_without_repository_is_global() {
        let dir = tempfile::tempdir().unwrap();
        if discover(dir.path()).is_some() {
            return;
        }
        let resolved = resolve(dir.path());
        assert_eq!(resolved.id, "global");
        assert_eq!(resolved.previous, None);
        assert!(!resolved.is_git());
        assert_eq!(resolved.directory.parent(), None);
    }

    #[test]
    fn root_commit_identifies_repository_without_remote() {
        let (dir, root) = repo_with_commit();
        let nested = dir.path().join("a").join("b");
        fs::create_dir_all(&nested).unwrap();
        let resolved = resolve(&nested);
        assert_eq!(resolved.id, root);
        assert_eq!(resolved.previous, None);
        assert!(same_path(&resolved.directory, dir.path()));
        assert!(same_path(
            resolved.store.as_deref().unwrap(),
            &dir.path().join(".git")
        ));
    }

    #[test]
    fn cached_id_wins_over_root_commit_and_remote_wins_over_cache() {
        let (dir, root) = repo_with_commit();
        let store = resolve(dir.path()).store.unwrap();
        commit(&store, "cached-project-id\n");
        let resolved = resolve(dir.path());
        assert_eq!(resolved.id, "cached-project-id");
        assert_eq!(resolved.previous.as_deref(), Some("cached-project-id"));
        assert_ne!(resolved.id, root);

        run(
            dir.path(),
            &["remote", "add", "origin", "git@github.com:sst/opencode.git"],
        );
        let resolved = resolve(dir.path());
        assert_eq!(resolved.id, OPENCODE_REMOTE_ID);
        assert_eq!(resolved.previous.as_deref(), Some("cached-project-id"));

        commit(&store, &resolved.id);
        assert_eq!(cached_id(&store).as_deref(), Some(OPENCODE_REMOTE_ID));
    }

    #[test]
    fn empty_repository_falls_back_to_global() {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "-q"]);
        let resolved = resolve(dir.path());
        assert_eq!(resolved.id, "global");
        assert!(resolved.is_git());
    }

    #[test]
    fn local_path_remote_is_ignored() {
        let (dir, root) = repo_with_commit();
        run(dir.path(), &["remote", "add", "origin", "../elsewhere"]);
        assert_eq!(resolve(dir.path()).id, root);
    }

    #[test]
    fn multiple_roots_pick_lexicographically_first() {
        let (dir, first) = repo_with_commit();
        run(dir.path(), &["checkout", "-q", "--orphan", "other"]);
        fs::write(dir.path().join("b.txt"), "b").unwrap();
        run(dir.path(), &["add", "."]);
        run(dir.path(), &["commit", "-q", "-m", "second root"]);
        let second = run(dir.path(), &["rev-list", "--max-parents=0", "HEAD"]);
        run(
            dir.path(),
            &[
                "merge",
                "-q",
                "--allow-unrelated-histories",
                "-m",
                "merge",
                "main",
            ],
        );
        let expected = first.clone().min(second.clone());
        assert_ne!(first, second);
        assert_eq!(resolve(dir.path()).id, expected);
    }
}
