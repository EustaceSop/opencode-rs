use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use opencode_lsp::{FileDiffStatus, Patch, Snapshot};
use tempfile::TempDir;

struct Fixture {
    _tmp: TempDir,
    data: PathBuf,
    work: PathBuf,
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git on PATH");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write(root: &Path, rel: &str, content: impl AsRef<[u8]>) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn read(root: &Path, rel: &str) -> String {
    std::fs::read_to_string(root.join(rel)).unwrap()
}

fn fixture(repo: bool) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    if repo {
        git(&work, &["init", "-q"]);
        write(&work, ".gitignore", "ignored.txt\n");
        write(&work, "a.txt", "one\n");
        write(&work, "gone.txt", "bye\n");
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "init"]);
    }
    Fixture {
        _tmp: tmp,
        data,
        work,
    }
}

fn snapshot(f: &Fixture) -> Snapshot {
    Snapshot::new(&f.data, "proj", &f.work)
}

fn abs(f: &Fixture, rel: &str) -> String {
    f.work.join(rel).to_string_lossy().replace('\\', "/")
}

fn set(patch: &Patch) -> BTreeSet<String> {
    patch.files.iter().cloned().collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn track_patch_diff_revert_restore() {
    let f = fixture(true);
    let snap = snapshot(&f);
    assert!(snap.enabled());

    let h1 = snap.track().await.expect("tree hash");
    assert_eq!(h1.len(), 40);
    assert!(h1.bytes().all(|b| b.is_ascii_hexdigit()));
    assert!(
        snap.gitdir()
            .starts_with(f.data.join("snapshot-rs").join("proj"))
    );
    assert!(snap.gitdir().join("HEAD").exists());
    assert!(!f.data.join("snapshot").exists());
    assert_eq!(snap.track().await.as_deref(), Some(h1.as_str()));
    assert!(snap.patch(&h1).await.files.is_empty());
    assert_eq!(snap.diff(&h1).await, "");

    write(&f.work, "a.txt", "two\n");
    write(&f.work, "b b.txt", "new\n");
    write(&f.work, "sub/c.txt", "deep\n");
    write(&f.work, "ignored.txt", "secret\n");
    write(&f.work, "bin.dat", [0u8, 1, 2, 0, 255]);
    std::fs::remove_file(f.work.join("gone.txt")).unwrap();

    let patch = snap.patch(&h1).await;
    assert_eq!(patch.hash, h1);
    let expected: BTreeSet<String> = ["a.txt", "b b.txt", "sub/c.txt", "bin.dat", "gone.txt"]
        .iter()
        .map(|rel| abs(&f, rel))
        .collect();
    assert_eq!(set(&patch), expected);

    let diff = snap.diff(&h1).await;
    assert!(diff.contains("-one") && diff.contains("+two"), "{diff}");
    assert!(diff.contains("b b.txt") && !diff.contains("ignored.txt"));

    let h2 = snap.track().await.unwrap();
    assert_ne!(h1, h2);
    let full = snap.diff_full(&h1, &h2).await;
    let by = |name: &str| {
        full.iter()
            .find(|d| d.file.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("{name}"))
    };
    assert_eq!(full.len(), 5);
    let a = by("a.txt");
    assert_eq!(
        (a.additions, a.deletions, a.status),
        (1.0, 1.0, Some(FileDiffStatus::Modified))
    );
    let head = format!(
        "Index: a.txt\n{}\n--- a.txt\t\n+++ a.txt\t\n",
        "=".repeat(67)
    );
    assert_eq!(
        a.patch.as_deref(),
        Some(format!("{head}@@ -1,1 +1,1 @@\n-one\n+two\n").as_str())
    );
    let b = by("b b.txt");
    assert_eq!(
        (b.additions, b.deletions, b.status),
        (1.0, 0.0, Some(FileDiffStatus::Added))
    );
    assert!(
        b.patch
            .as_deref()
            .unwrap()
            .ends_with("@@ -0,0 +1,1 @@\n+new\n")
    );
    let gone = by("gone.txt");
    assert_eq!(
        (gone.additions, gone.deletions, gone.status),
        (0.0, 1.0, Some(FileDiffStatus::Deleted))
    );
    assert!(
        gone.patch
            .as_deref()
            .unwrap()
            .ends_with("@@ -1,1 +0,0 @@\n-bye\n")
    );
    let bin = by("bin.dat");
    assert_eq!(
        (bin.additions, bin.deletions, bin.patch.as_deref()),
        (0.0, 0.0, Some(""))
    );
    assert_eq!(by("sub/c.txt").status, Some(FileDiffStatus::Added));
    assert!(snap.diff_full(&h2, &h2).await.is_empty());

    snap.revert(&[patch]).await;
    assert_eq!(read(&f.work, "a.txt"), "one\n");
    assert_eq!(read(&f.work, "gone.txt"), "bye\n");
    assert!(!f.work.join("b b.txt").exists());
    assert!(!f.work.join("sub/c.txt").exists());
    assert!(!f.work.join("bin.dat").exists());
    assert_eq!(read(&f.work, "ignored.txt"), "secret\n");

    assert!(snap.restore(&h2).await);
    assert_eq!(read(&f.work, "a.txt"), "two\n");
    assert_eq!(read(&f.work, "b b.txt"), "new\n");
    assert_eq!(read(&f.work, "sub/c.txt"), "deep\n");
    assert!(
        !snap
            .restore("0000000000000000000000000000000000000000")
            .await
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn revert_uses_first_patch_per_file() {
    let f = fixture(true);
    let snap = snapshot(&f);
    let h1 = snap.track().await.unwrap();
    write(&f.work, "a.txt", "two\n");
    let p1 = snap.patch(&h1).await;
    let h2 = snap.track().await.unwrap();
    write(&f.work, "a.txt", "three\n");
    write(&f.work, "late.txt", "x\n");
    let p2 = snap.patch(&h2).await;
    assert_eq!(
        set(&p2),
        [abs(&f, "a.txt"), abs(&f, "late.txt")]
            .into_iter()
            .collect()
    );

    snap.revert(&[p1, p2]).await;
    assert_eq!(read(&f.work, "a.txt"), "one\n");
    assert!(!f.work.join("late.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn no_vcs_is_inert() {
    let f = fixture(false);
    let snap = snapshot(&f);
    write(&f.work, "a.txt", "one\n");
    assert!(!snap.enabled());
    assert_eq!(snap.track().await, None);
    assert!(snap.patch("abc").await.files.is_empty());
    assert_eq!(snap.diff("abc").await, "");
    assert!(snap.diff_full("abc", "def").await.is_empty());
    assert!(!snap.restore("abc").await);
    snap.revert(&[Patch {
        hash: "abc".into(),
        files: vec![abs(&f, "a.txt")],
    }])
    .await;
    snap.cleanup().await;
    assert_eq!(read(&f.work, "a.txt"), "one\n");
    assert!(!f.data.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn config_false_disables() {
    let f = fixture(true);
    let snap = snapshot(&f).with_config(Some(false));
    assert_eq!(snap.track().await, None);
    assert!(!f.data.exists());
    assert!(snapshot(&f).with_config(Some(true)).enabled());
    assert!(snapshot(&f).with_config(None).enabled());
    assert!(!snapshot(&f).with_vcs(false).enabled());
}

#[tokio::test(flavor = "multi_thread")]
async fn large_untracked_files_are_excluded() {
    let f = fixture(true);
    let snap = snapshot(&f);
    let h1 = snap.track().await.unwrap();
    write(&f.work, "big.bin", vec![b'x'; 2 * 1024 * 1024 + 1]);
    write(&f.work, "small.txt", "s\n");
    let patch = snap.patch(&h1).await;
    assert_eq!(patch.files, vec![abs(&f, "small.txt")]);
    let exclude = std::fs::read_to_string(snap.gitdir().join("info").join("exclude")).unwrap();
    assert!(exclude.lines().any(|line| line == "/big.bin"), "{exclude}");
}

#[tokio::test(flavor = "multi_thread")]
async fn newly_ignored_files_leave_the_snapshot() {
    let f = fixture(true);
    let snap = snapshot(&f);
    write(&f.work, "tmp.log", "x\n");
    let h1 = snap.track().await.unwrap();
    write(&f.work, ".gitignore", "ignored.txt\n*.log\n");
    write(&f.work, "tmp.log", "y\n");
    let patch = snap.patch(&h1).await;
    assert_eq!(patch.files, vec![abs(&f, ".gitignore")]);
}

#[tokio::test(flavor = "multi_thread")]
async fn instance_directory_scopes_tracking() {
    let f = fixture(true);
    write(&f.work, "sub/in.txt", "1\n");
    let snap = snapshot(&f).with_directory(&f.work.join("sub"));
    let h1 = snap.track().await.unwrap();
    write(&f.work, "sub/in.txt", "2\n");
    write(&f.work, "a.txt", "outside\n");
    assert_eq!(snap.patch(&h1).await.files, vec![abs(&f, "sub/in.txt")]);
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_repository_and_cleanup() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q"]);
    let snap = Snapshot::new(&tmp.path().join("data"), "global", &work);
    let h1 = snap.track().await.unwrap();
    write(&work, "first.txt", "hello\n");
    let h2 = snap.track().await.unwrap();
    assert_ne!(h1, h2);
    snap.cleanup().await;
    assert_eq!(snap.track().await.as_deref(), Some(h2.as_str()));
    let full = snap.diff_full(&h1, &h2).await;
    assert_eq!(full.len(), 1);
    assert_eq!(full[0].file.as_deref(), Some("first.txt"));
}
