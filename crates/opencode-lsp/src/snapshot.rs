use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

pub use opencode_storage::{FileDiff, FileDiffStatus};
use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};

use crate::util::{self, Output, RunOptions};

const PRUNE: &str = "7.days";
const LIMIT: u64 = 2 * 1024 * 1024;
const CORE: [&str; 4] = ["-c", "core.longpaths=true", "-c", "core.symlinks=true"];
const CFG: [&str; 6] = [
    "-c",
    "core.autocrlf=false",
    "-c",
    "core.longpaths=true",
    "-c",
    "core.symlinks=true",
];
const QUOTE: [&str; 8] = [
    "-c",
    "core.autocrlf=false",
    "-c",
    "core.longpaths=true",
    "-c",
    "core.symlinks=true",
    "-c",
    "core.quotepath=false",
];
// The TypeScript build owns `<data>/snapshot`; this build keeps its own history.
const ROOT: &str = "snapshot-rs";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Patch {
    pub hash: String,
    pub files: Vec<String>,
}

fn lock_for(gitdir: &Path) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut map = LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    map.entry(gitdir.to_path_buf()).or_default().clone()
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    directory: PathBuf,
    worktree: PathBuf,
    gitdir: PathBuf,
    git: bool,
    config_enabled: bool,
    lock: Arc<tokio::sync::Mutex<()>>,
}

struct Row {
    file: String,
    status: FileDiffStatus,
    binary: bool,
    additions: f64,
    deletions: f64,
}

#[derive(Clone, Copy, PartialEq)]
enum Side {
    Before,
    After,
}

impl Snapshot {
    /// `vcs` defaults to "git" when `<worktree>/.git` exists; the instance directory defaults to the worktree.
    pub fn new(data_dir: &Path, project_id: &str, worktree: &Path) -> Self {
        let gitdir = data_dir
            .join(ROOT)
            .join(project_id)
            .join(util::sha1_hex(&worktree.to_string_lossy()));
        Self {
            directory: worktree.to_path_buf(),
            worktree: worktree.to_path_buf(),
            lock: lock_for(&gitdir),
            gitdir,
            git: worktree.join(".git").exists(),
            config_enabled: true,
        }
    }

    pub fn with_directory(mut self, directory: &Path) -> Self {
        self.directory = directory.to_path_buf();
        self
    }

    /// `project.vcs === "git"`.
    pub fn with_vcs(mut self, git: bool) -> Self {
        self.git = git;
        self
    }

    /// Config `snapshot !== false`.
    pub fn with_config(mut self, snapshot: Option<bool>) -> Self {
        self.config_enabled = snapshot != Some(false);
        self
    }

    pub fn enabled(&self) -> bool {
        self.git && self.config_enabled
    }

    pub fn gitdir(&self) -> &Path {
        &self.gitdir
    }

    pub fn worktree(&self) -> &Path {
        &self.worktree
    }

    fn args(&self, cmd: &[&str]) -> Vec<String> {
        let mut out = vec![
            "--git-dir".to_string(),
            self.gitdir.to_string_lossy().into_owned(),
            "--work-tree".to_string(),
            self.worktree.to_string_lossy().into_owned(),
        ];
        out.extend(cmd.iter().map(|s| s.to_string()));
        out
    }

    fn with(&self, flags: &[&str], cmd: &[&str]) -> Vec<String> {
        let mut out: Vec<String> = flags.iter().map(|s| s.to_string()).collect();
        out.extend(self.args(cmd));
        out
    }

    async fn git(&self, args: &[String], cwd: Option<&Path>, stdin: Option<&[u8]>) -> Output {
        util::run(
            "git",
            args,
            RunOptions {
                cwd,
                stdin,
                env: None,
            },
        )
        .await
    }

    async fn ignore(&self, files: &[String]) -> HashSet<String> {
        if files.is_empty() {
            return HashSet::new();
        }
        // check-ignore treats a leading colon as pathspec magic but accepts and echoes a protective ./ prefix.
        let input = nul_terminated(files.iter().map(|f| {
            if f.starts_with(':') {
                format!("./{f}")
            } else {
                f.clone()
            }
        }));
        let mut args: Vec<String> = QUOTE.iter().map(|s| s.to_string()).collect();
        args.extend([
            "--git-dir".to_string(),
            self.worktree.join(".git").to_string_lossy().into_owned(),
            "--work-tree".to_string(),
            self.worktree.to_string_lossy().into_owned(),
            "check-ignore".to_string(),
            "--no-index".to_string(),
            "--stdin".to_string(),
            "-z".to_string(),
        ]);
        let check = self
            .git(&args, Some(&self.worktree), Some(input.as_bytes()))
            .await;
        if check.code != 0 && check.code != 1 {
            return HashSet::new();
        }
        check
            .text()
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(|item| {
                if item.starts_with("./:") {
                    item[2..].to_string()
                } else {
                    item.to_string()
                }
            })
            .collect()
    }

    async fn drop_files(&self, files: &[String]) {
        if files.is_empty() {
            return;
        }
        let args = self.with(
            &CFG,
            &[
                "rm",
                "--cached",
                "-f",
                "--ignore-unmatch",
                "--pathspec-from-file=-",
                "--pathspec-file-nul",
            ],
        );
        self.git(
            &args,
            Some(&self.worktree),
            Some(top_literal(files).as_bytes()),
        )
        .await;
    }

    async fn stage(&self, files: &[String]) {
        if files.is_empty() {
            return;
        }
        let args = self.with(
            &CFG,
            &[
                "add",
                "--all",
                "--sparse",
                "--pathspec-from-file=-",
                "--pathspec-file-nul",
            ],
        );
        self.git(
            &args,
            Some(&self.worktree),
            Some(top_literal(files).as_bytes()),
        )
        .await;
    }

    async fn excludes(&self) -> Option<PathBuf> {
        let args = [
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "info/exclude",
        ]
        .map(String::from);
        let result = self.git(&args, Some(&self.worktree), None).await;
        let file = result.text().trim().to_string();
        if file.is_empty() {
            return None;
        }
        let file = PathBuf::from(file);
        file.exists().then_some(file)
    }

    async fn sync(&self, list: &[String]) {
        let source = match self.excludes().await {
            Some(file) => std::fs::read_to_string(file)
                .unwrap_or_default()
                .trim_end()
                .to_string(),
            None => String::new(),
        };
        let text = std::iter::once(source)
            .chain(
                list.iter()
                    .map(|item| format!("/{}", item.replace('\\', "/"))),
            )
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        let info = self.gitdir.join("info");
        let _ = std::fs::create_dir_all(&info);
        let _ = std::fs::write(
            info.join("exclude"),
            if text.is_empty() {
                String::new()
            } else {
                format!("{text}\n")
            },
        );
    }

    // Share the source object database and index so the first add reuses existing hashes.
    async fn seed(&self) {
        if !self.git {
            return;
        }
        let args = ["rev-parse", "--path-format=absolute", "--git-common-dir"].map(String::from);
        let common = self.git(&args, Some(&self.worktree), None).await;
        if common.code != 0 {
            return;
        }
        let source = common.text().trim().to_string();
        if source.is_empty() || !Path::new(&source).exists() {
            return;
        }
        let source = PathBuf::from(source);
        let objects = source.join("objects");
        let chained =
            std::fs::read_to_string(objects.join("info").join("alternates")).unwrap_or_default();
        let alternates: Vec<String> = std::iter::once(objects.to_string_lossy().into_owned())
            .chain(
                chained
                    .lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty()),
            )
            .filter(|candidate| Path::new(candidate).exists())
            .collect();
        if alternates.is_empty() {
            return;
        }
        let info = self.gitdir.join("objects").join("info");
        let _ = std::fs::create_dir_all(&info);
        let _ = std::fs::write(info.join("alternates"), alternates.join("\n") + "\n");
        let index = source.join("index");
        if index.exists() {
            let _ = std::fs::copy(index, self.gitdir.join("index"));
        }
    }

    async fn add(&self) {
        self.sync(&[]).await;
        let diff_args = self.with(&QUOTE, &["diff-files", "--name-only", "-z", "--", "."]);
        let other_args = self.with(
            &QUOTE,
            &[
                "ls-files",
                "--full-name",
                "--others",
                "--exclude-standard",
                "-z",
                "--",
                ".",
            ],
        );
        let (diff, other) = tokio::join!(
            self.git(&diff_args, Some(&self.directory), None),
            self.git(&other_args, Some(&self.directory), None)
        );
        if diff.code != 0 || other.code != 0 {
            return;
        }
        let split = |text: String| {
            text.split('\0')
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect::<Vec<_>>()
        };
        let tracked = split(diff.text());
        let untracked = split(other.text());
        let mut seen = HashSet::new();
        let all: Vec<String> = tracked
            .iter()
            .chain(&untracked)
            .filter(|f| seen.insert(f.as_str()))
            .cloned()
            .collect();
        if all.is_empty() {
            return;
        }

        let ignored = self.ignore(&all).await;
        if !ignored.is_empty() {
            self.drop_files(&ignored.iter().cloned().collect::<Vec<_>>())
                .await;
        }
        let allow: Vec<String> = all
            .into_iter()
            .filter(|item| !ignored.contains(item))
            .collect();
        if allow.is_empty() {
            return;
        }

        let large: HashSet<&String> = allow
            .iter()
            .filter(|item| {
                std::fs::metadata(self.worktree.join(item.as_str()))
                    .is_ok_and(|meta| meta.is_file() && meta.len() > LIMIT)
            })
            .collect();
        let block: Vec<String> = untracked
            .into_iter()
            .filter(|item| large.contains(item))
            .collect();
        self.sync(&block).await;
        let block: HashSet<&String> = block.iter().collect();
        let staged: Vec<String> = allow
            .iter()
            .filter(|item| !block.contains(item))
            .cloned()
            .collect();
        self.stage(&staged).await;
    }

    /// `git gc --prune=7.days` on the snapshot repo.
    pub async fn cleanup(&self) {
        let _guard = self.lock.lock().await;
        if !self.enabled() || !self.gitdir.exists() {
            return;
        }
        let prune = format!("--prune={PRUNE}");
        self.git(&self.args(&["gc", &prune]), Some(&self.directory), None)
            .await;
    }

    /// The source's background loop: first run after one minute, then hourly.
    pub fn spawn_cleanup(&self) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            loop {
                this.cleanup().await;
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        })
    }

    /// Stage the worktree into the snapshot index and return the tree hash; `None` when disabled.
    pub async fn track(&self) -> Option<String> {
        let _guard = self.lock.lock().await;
        if !self.enabled() {
            return None;
        }
        let existed = self.gitdir.exists();
        let _ = std::fs::create_dir_all(&self.gitdir);
        if !existed {
            let env = HashMap::from([
                (
                    "GIT_DIR".to_string(),
                    self.gitdir.to_string_lossy().into_owned(),
                ),
                (
                    "GIT_WORK_TREE".to_string(),
                    self.worktree.to_string_lossy().into_owned(),
                ),
            ]);
            util::run(
                "git",
                &["init"],
                RunOptions {
                    env: Some(&env),
                    ..Default::default()
                },
            )
            .await;
            let gitdir = self.gitdir.to_string_lossy().into_owned();
            for (key, value) in [
                ("core.autocrlf", "false"),
                ("core.longpaths", "true"),
                ("core.symlinks", "true"),
                ("core.fsmonitor", "false"),
                ("feature.manyFiles", "true"),
                ("index.version", "4"),
                ("index.threads", "true"),
                ("core.untrackedCache", "true"),
            ] {
                util::run(
                    "git",
                    &["--git-dir", &gitdir, "config", key, value],
                    RunOptions::default(),
                )
                .await;
            }
            self.seed().await;
        }
        self.add().await;
        let result = self
            .git(&self.args(&["write-tree"]), Some(&self.directory), None)
            .await;
        let hash = result.text().trim().to_string();
        (!hash.is_empty()).then_some(hash)
    }

    /// Files changed since `hash` (absolute, forward slashes).
    pub async fn patch(&self, hash: &str) -> Patch {
        let empty = Patch {
            hash: hash.to_string(),
            files: Vec::new(),
        };
        if !self.enabled() {
            return empty;
        }
        let _guard = self.lock.lock().await;
        self.add().await;
        let args = self.with(
            &QUOTE,
            &[
                "diff",
                "--cached",
                "--no-ext-diff",
                "--name-only",
                hash,
                "--",
                ".",
            ],
        );
        let result = self.git(&args, Some(&self.directory), None).await;
        if result.code != 0 {
            return empty;
        }
        let files: Vec<String> = result
            .text()
            .trim()
            .split('\n')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        let ignored = self.ignore(&files).await;
        Patch {
            hash: hash.to_string(),
            files: files
                .iter()
                .filter(|item| !ignored.contains(*item))
                .map(|item| util::slash(&self.worktree.join(item)))
                .collect(),
        }
    }

    /// `read-tree` + `checkout-index -a -f`: put the worktree back to `snapshot`.
    pub async fn restore(&self, snapshot: &str) -> bool {
        if !self.enabled() {
            return false;
        }
        let _guard = self.lock.lock().await;
        let read = self
            .git(
                &self.with(&CORE, &["read-tree", snapshot]),
                Some(&self.worktree),
                None,
            )
            .await;
        if read.code != 0 {
            return false;
        }
        self.git(
            &self.with(&CORE, &["checkout-index", "-a", "-f"]),
            Some(&self.worktree),
            None,
        )
        .await
        .code
            == 0
    }

    async fn revert_single(&self, hash: &str, file: &str, rel: &str) {
        let checkout = self
            .git(
                &self.with(&CORE, &["checkout", hash, "--", file]),
                Some(&self.worktree),
                None,
            )
            .await;
        if checkout.code == 0 {
            return;
        }
        let tree = self
            .git(
                &self.with(&CORE, &["ls-tree", hash, "--", rel]),
                Some(&self.worktree),
                None,
            )
            .await;
        if tree.code == 0 && !tree.text().trim().is_empty() {
            return;
        }
        remove(file);
    }

    /// Undo `patches`: each file goes back to its content in the first patch that lists it, or is deleted.
    pub async fn revert(&self, patches: &[Patch]) {
        if !self.enabled() {
            return;
        }
        let _guard = self.lock.lock().await;
        let mut seen = HashSet::new();
        let mut ops: Vec<(&str, &str, String)> = Vec::new();
        for item in patches {
            for file in &item.files {
                if !seen.insert(file.as_str()) {
                    continue;
                }
                ops.push((&item.hash, file, self.relative(file)));
            }
        }

        let clash = |a: &str, b: &str| {
            a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
        };
        let mut i = 0;
        while i < ops.len() {
            let hash = ops[i].0;
            let mut j = i + 1;
            // Only batch adjacent files when their paths cannot affect each other.
            while j < ops.len() && j - i < 100 {
                if ops[j].0 != hash || ops[i..j].iter().any(|op| clash(&op.2, &ops[j].2)) {
                    break;
                }
                j += 1;
            }
            let run = &ops[i..j];
            i = j;
            if run.len() == 1 {
                self.revert_single(hash, run[0].1, &run[0].2).await;
                continue;
            }

            let mut cmd = vec!["ls-tree", "--name-only", hash, "--"];
            cmd.extend(run.iter().map(|op| op.2.as_str()));
            let tree = self
                .git(&self.with(&CORE, &cmd), Some(&self.worktree), None)
                .await;
            if tree.code != 0 {
                for op in run {
                    self.revert_single(op.0, op.1, &op.2).await;
                }
                continue;
            }
            let text = tree.text();
            let have: HashSet<&str> = text
                .trim()
                .split('\n')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            let list: Vec<&str> = run
                .iter()
                .filter(|op| have.contains(op.2.as_str()))
                .map(|op| op.1)
                .collect();
            if !list.is_empty() {
                let mut cmd = vec!["checkout", hash, "--"];
                cmd.extend(&list);
                if self
                    .git(&self.with(&CORE, &cmd), Some(&self.worktree), None)
                    .await
                    .code
                    != 0
                {
                    for op in run {
                        self.revert_single(op.0, op.1, &op.2).await;
                    }
                    continue;
                }
            }
            for op in run.iter().filter(|op| !have.contains(op.2.as_str())) {
                remove(op.1);
            }
        }
    }

    fn relative(&self, file: &str) -> String {
        let path = Path::new(file);
        util::slash(path.strip_prefix(&self.worktree).unwrap_or(path))
    }

    /// Unified diff of the worktree against `hash`.
    pub async fn diff(&self, hash: &str) -> String {
        if !self.enabled() {
            return String::new();
        }
        let _guard = self.lock.lock().await;
        self.add().await;
        let args = self.with(
            &QUOTE,
            &["diff", "--cached", "--no-ext-diff", hash, "--", "."],
        );
        let result = self.git(&args, Some(&self.worktree), None).await;
        if result.code != 0 {
            return String::new();
        }
        result.text().trim().to_string()
    }

    async fn show(&self, rev: &str, file: &str) -> String {
        let spec = format!("{rev}:{file}");
        self.git(&self.with(&CFG, &["show", &spec]), None, None)
            .await
            .text()
    }

    async fn load(
        &self,
        rows: &[&Row],
        from: &str,
        to: &str,
    ) -> Option<HashMap<String, (String, String)>> {
        let mut refs: Vec<(&str, Side, String)> = Vec::new();
        for row in rows.iter().filter(|row| !row.binary) {
            if row.status != FileDiffStatus::Added {
                refs.push((&row.file, Side::Before, format!("{from}:{}", row.file)));
            }
            if row.status != FileDiffStatus::Deleted {
                refs.push((&row.file, Side::After, format!("{to}:{}", row.file)));
            }
        }
        let mut map = HashMap::new();
        if refs.is_empty() {
            return Some(map);
        }
        let stdin = refs
            .iter()
            .map(|r| r.2.as_str())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let args = self.with(&CFG, &["cat-file", "--batch"]);
        let batch = self
            .git(&args, Some(&self.directory), Some(stdin.as_bytes()))
            .await;
        if batch.code != 0 {
            return None;
        }
        let out = &batch.stdout;
        let mut i = 0;
        for (file, side, _) in &refs {
            let end = i + out[i..].iter().position(|b| *b == b'\n')?;
            let head = String::from_utf8_lossy(&out[i..end]).into_owned();
            i = end + 1;
            let hit: &mut (String, String) = map.entry(file.to_string()).or_default();
            if head.ends_with(" missing") {
                continue;
            }
            let mut parts = head.split(' ');
            let (Some(oid), Some("blob"), Some(size), None) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                return None;
            };
            if oid.is_empty() || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            let size: usize = size.parse().ok()?;
            if i + size >= out.len() || out[i + size] != b'\n' {
                return None;
            }
            let text = String::from_utf8_lossy(&out[i..i + size]).into_owned();
            match side {
                Side::Before => hit.0 = text,
                Side::After => hit.1 = text,
            }
            i += size + 1;
        }
        (i == out.len()).then_some(map)
    }

    /// Per-file diff between two snapshot trees, in the shape `summary.diffs` and `/session/:id/diff` use.
    pub async fn diff_full(&self, from: &str, to: &str) -> Vec<FileDiff> {
        if !self.enabled() {
            return Vec::new();
        }
        let _guard = self.lock.lock().await;
        let args = self.with(
            &QUOTE,
            &[
                "diff",
                "--no-ext-diff",
                "--name-status",
                "--no-renames",
                from,
                to,
                "--",
                ".",
            ],
        );
        let statuses = self.git(&args, Some(&self.directory), None).await;
        let mut status = HashMap::new();
        for line in statuses.text().trim().split('\n') {
            let mut parts = line.split('\t');
            let (Some(code), Some(file)) = (parts.next(), parts.next()) else {
                continue;
            };
            if code.is_empty() || file.is_empty() {
                continue;
            }
            let kind = match code.as_bytes()[0] {
                b'A' => FileDiffStatus::Added,
                b'D' => FileDiffStatus::Deleted,
                _ => FileDiffStatus::Modified,
            };
            status.insert(file.to_string(), kind);
        }

        let args = self.with(
            &QUOTE,
            &[
                "diff",
                "--no-ext-diff",
                "--no-renames",
                "--numstat",
                from,
                to,
                "--",
                ".",
            ],
        );
        let numstat = self.git(&args, Some(&self.directory), None).await;
        let mut rows: Vec<Row> = Vec::new();
        for line in numstat.text().trim().split('\n').filter(|l| !l.is_empty()) {
            let mut parts = line.split('\t');
            let (Some(adds), Some(dels), Some(file)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if file.is_empty() {
                continue;
            }
            let binary = adds == "-" && dels == "-";
            let count = |s: &str| {
                if binary {
                    0.0
                } else {
                    s.parse::<f64>().unwrap_or(0.0)
                }
            };
            rows.push(Row {
                file: file.to_string(),
                status: status
                    .get(file)
                    .copied()
                    .unwrap_or(FileDiffStatus::Modified),
                binary,
                additions: count(adds),
                deletions: count(dels),
            });
        }

        let ignored = self
            .ignore(&rows.iter().map(|r| r.file.clone()).collect::<Vec<_>>())
            .await;
        rows.retain(|row| !ignored.contains(&row.file));

        let mut result = Vec::with_capacity(rows.len());
        for run in rows.chunks(100) {
            let text = self.load(&run.iter().collect::<Vec<_>>(), from, to).await;
            for row in run {
                let (before, after) = if row.binary {
                    (String::new(), String::new())
                } else if let Some(map) = &text {
                    map.get(&row.file).cloned().unwrap_or_default()
                } else {
                    let before = if row.status == FileDiffStatus::Added {
                        String::new()
                    } else {
                        self.show(from, &row.file).await
                    };
                    let after = if row.status == FileDiffStatus::Deleted {
                        String::new()
                    } else {
                        self.show(to, &row.file).await
                    };
                    (before, after)
                };
                result.push(FileDiff {
                    file: Some(row.file.clone()),
                    patch: Some(if row.binary {
                        String::new()
                    } else {
                        full_patch(&row.file, &before, &after)
                    }),
                    additions: row.additions,
                    deletions: row.deletions,
                    status: Some(row.status),
                });
            }
        }
        result
    }
}

fn nul_terminated(files: impl Iterator<Item = String>) -> String {
    files.map(|f| f + "\0").collect()
}

fn top_literal(files: &[String]) -> String {
    nul_terminated(files.iter().map(|file| format!(":(top,literal){file}")))
}

fn remove(file: &str) {
    let path = Path::new(file);
    if std::fs::remove_file(path).is_err() && path.is_dir() {
        let _ = std::fs::remove_dir_all(path);
    }
}

/// jsdiff `formatPatch(structuredPatch(file, file, before, after, "", "", { context: MAX }))`: one whole-file hunk.
pub fn full_patch(file: &str, before: &str, after: &str) -> String {
    let mut out = format!(
        "Index: {file}\n{}\n--- {file}\t\n+++ {file}\t\n",
        "=".repeat(67)
    );
    let diff = TextDiff::from_lines(before, after);
    if before == after {
        return out;
    }
    let (mut old_lines, mut new_lines) = (0usize, 0usize);
    let mut body = String::new();
    for change in diff.iter_all_changes() {
        body.push(match change.tag() {
            ChangeTag::Delete => {
                old_lines += 1;
                '-'
            }
            ChangeTag::Insert => {
                new_lines += 1;
                '+'
            }
            ChangeTag::Equal => {
                old_lines += 1;
                new_lines += 1;
                ' '
            }
        });
        let value: &str = change.value();
        body.push_str(value);
        if !value.ends_with('\n') {
            body.push_str("\n\\ No newline at end of file\n");
        }
    }
    let start = |n: usize| usize::from(n > 0);
    out.push_str(&format!(
        "@@ -{},{old_lines} +{},{new_lines} @@\n",
        start(old_lines),
        start(new_lines)
    ));
    out.push_str(&body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_patch_shapes() {
        let head = format!(
            "Index: f.txt\n{}\n--- f.txt\t\n+++ f.txt\t\n",
            "=".repeat(67)
        );
        assert_eq!(
            full_patch("f.txt", "a\nb\n", "a\nB\n"),
            format!("{head}@@ -1,2 +1,2 @@\n a\n-b\n+B\n")
        );
        assert_eq!(
            full_patch("f.txt", "", "x\ny"),
            format!("{head}@@ -0,0 +1,2 @@\n+x\n+y\n\\ No newline at end of file\n")
        );
        assert_eq!(
            full_patch("f.txt", "x\n", ""),
            format!("{head}@@ -1,1 +0,0 @@\n-x\n")
        );
        assert_eq!(full_patch("f.txt", "same\n", "same\n"), head);
    }

    #[test]
    fn gitdir_layout() {
        let snap = Snapshot::new(Path::new("/data"), "proj", Path::new("/work/tree"));
        let expected = Path::new("/data")
            .join("snapshot-rs")
            .join("proj")
            .join(util::sha1_hex("/work/tree"));
        assert_eq!(snap.gitdir(), expected);
    }
}
