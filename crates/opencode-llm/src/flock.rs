// Cross-process lock shared with the TypeScript build: a directory `<locks>/<sha1(key)>.lock/` holding
// `meta.json` and a `heartbeat` file whose mtime the owner keeps fresh.
use sha1::{Digest, Sha1};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{fs, io, thread};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Options {
    pub stale_ms: u64,
    pub timeout_ms: u64,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for Options {
    fn default() -> Self {
        Options { stale_ms: 60_000, timeout_ms: 5 * 60_000, base_delay_ms: 100, max_delay_ms: 2_000 }
    }
}

// `Hash.fast`: lowercase SHA-1 hex.
pub fn hash(input: &str) -> String {
    Sha1::digest(input.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

pub fn lock_dir(locks: &Path, key: &str) -> PathBuf {
    locks.join(format!("{}.lock", hash(key)))
}

static SEED: AtomicU64 = AtomicU64::new(0);

// Small non-cryptographic generator for jitter and owner tokens.
pub(crate) fn random() -> u64 {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
    let mut x = SEED.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed) ^ now ^ (u64::from(std::process::id()) << 32);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

fn jitter(ms: u64) -> u64 {
    let spread = ms * 3 / 10;
    (ms + random() % (2 * spread + 1)).saturating_sub(spread)
}

fn age(path: &Path) -> io::Result<Option<Duration>> {
    match fs::metadata(path) {
        Ok(meta) => Ok(Some(SystemTime::now().duration_since(meta.modified()?).unwrap_or_default())),
        Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => Ok(None),
        Err(e) => Err(e),
    }
}

// A lock is judged by its heartbeat, then its metadata, then the directory itself.
fn stale(dir: &Path, stale_ms: u64) -> io::Result<bool> {
    for path in [dir.join("heartbeat"), dir.join("meta.json"), dir.to_path_buf()] {
        if let Some(age) = age(&path)? {
            return Ok(age.as_millis() as u64 > stale_ms);
        }
    }
    Ok(false)
}

fn mkdir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(dir)
    }
    #[cfg(not(unix))]
    fs::create_dir(dir)
}

fn remove(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
}

// 1970-01-01T00:00:00.000Z style timestamp.
fn iso_now() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let (days, rest) = ((now.as_secs() / 86_400) as i64, now.as_secs() % 86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (day, month) = (doy - (153 * mp + 2) / 5 + 1, if mp < 10 { mp + 3 } else { mp - 9 });
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z", rest / 3600, rest % 3600 / 60, rest % 60, now.subsec_millis())
}

fn hostname() -> String {
    ["COMPUTERNAME", "HOSTNAME"].iter().find_map(|name| std::env::var(name).ok().filter(|v| !v.is_empty())).unwrap_or_else(|| "localhost".into())
}

fn create_new(path: &Path, content: &str) -> io::Result<()> {
    use io::Write;
    fs::OpenOptions::new().write(true).create_new(true).open(path)?.write_all(content.as_bytes())
}

// One attempt: the token of the acquired lock, or None while another owner is alive.
fn try_acquire(dir: &Path, options: &Options) -> io::Result<Option<String>> {
    if let Err(error) = mkdir(dir) {
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
        if !stale(dir, options.stale_ms)? {
            return Ok(None);
        }
        // Only one contender may break a stale lock.
        let breaker = PathBuf::from(format!("{}.breaker", dir.display()));
        if let Err(claim) = mkdir(&breaker) {
            return match claim.kind() {
                io::ErrorKind::AlreadyExists => {
                    if age(&breaker)?.is_some_and(|a| a.as_millis() as u64 > options.stale_ms) {
                        remove(&breaker);
                    }
                    Ok(None)
                }
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => Ok(None),
                _ => Err(claim),
            };
        }
        let retaken = (|| {
            if !stale(dir, options.stale_ms)? {
                return Ok(false);
            }
            fs::remove_dir_all(dir)?;
            match mkdir(dir) {
                Ok(()) => Ok(true),
                Err(e) if matches!(e.kind(), io::ErrorKind::AlreadyExists | io::ErrorKind::DirectoryNotEmpty) => Ok(false),
                Err(e) => Err(e),
            }
        })();
        remove(&breaker);
        if !retaken? {
            return Ok(None);
        }
    }
    let token = format!("{:016x}{:016x}", random(), random());
    let meta = serde_json::json!({"token": token, "pid": std::process::id(), "hostname": hostname(), "createdAt": iso_now()});
    let written = create_new(&dir.join("heartbeat"), "").and_then(|()| create_new(&dir.join("meta.json"), &serde_json::to_string_pretty(&meta).unwrap_or_default()));
    if let Err(error) = written {
        remove(dir);
        return Err(io::Error::new(error.kind(), "Lock acquired but its files already existed (possible compromise)."));
    }
    Ok(Some(token))
}

pub struct Lease {
    dir: PathBuf,
    token: String,
    stop: Arc<AtomicBool>,
    heartbeat: Option<thread::JoinHandle<()>>,
}

impl Lease {
    // Removes the lock; refuses when the directory no longer belongs to this owner.
    pub fn release(mut self) -> io::Result<()> {
        self.stop_heartbeat();
        let raw = fs::read_to_string(self.dir.join("meta.json")).map_err(|e| io::Error::new(e.kind(), "Refusing to release: lock is compromised (metadata missing)."))?;
        let meta: serde_json::Value = serde_json::from_str(&raw).map_err(|_| io::Error::other("Refusing to release: lock is compromised (metadata invalid)."))?;
        if meta.get("token").and_then(|t| t.as_str()) != Some(self.token.as_str()) {
            return Err(io::Error::other("Refusing to release: lock token mismatch (not the owner)."));
        }
        let dir = std::mem::take(&mut self.dir);
        fs::remove_dir_all(dir)
    }

    fn stop_heartbeat(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.heartbeat.take() {
            handle.thread().unpark();
            let _ = handle.join();
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.stop_heartbeat();
        if !self.dir.as_os_str().is_empty() {
            let owned = fs::read_to_string(self.dir.join("meta.json")).is_ok_and(|raw| raw.contains(&self.token));
            if owned {
                remove(&self.dir);
            }
        }
    }
}

// Waits for the lock named `key` under `locks`, breaking locks whose owner stopped heartbeating.
pub async fn acquire(locks: &Path, key: &str, options: Options) -> io::Result<Lease> {
    fs::create_dir_all(locks)?;
    let dir = lock_dir(locks, key);
    let deadline = Instant::now() + Duration::from_millis(options.timeout_ms);
    let mut delay = options.base_delay_ms;
    let token = loop {
        if let Some(token) = try_acquire(&dir, &options)? {
            break token;
        }
        if Instant::now() > deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, format!("Timed out waiting for lock: {key}")));
        }
        tokio::time::sleep(Duration::from_millis(jitter(delay))).await;
        delay = options.max_delay_ms.min(delay * 17 / 10);
    };
    let stop = Arc::new(AtomicBool::new(false));
    let (flag, heartbeat, interval) = (stop.clone(), dir.join("heartbeat"), Duration::from_millis((options.stale_ms / 3).max(100)));
    let handle = thread::Builder::new().name("opencode-lock-heartbeat".into()).spawn(move || {
        loop {
            thread::park_timeout(interval);
            if flag.load(Ordering::Relaxed) {
                return;
            }
            if let Ok(file) = fs::OpenOptions::new().write(true).open(&heartbeat) {
                let _ = file.set_modified(SystemTime::now());
            }
        }
    })?;
    Ok(Lease { dir, token, stop, heartbeat: Some(handle) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quick(stale_ms: u64, timeout_ms: u64) -> Options {
        Options { stale_ms, timeout_ms, base_delay_ms: 5, max_delay_ms: 20 }
    }

    #[test]
    fn hash_is_sha1_hex() {
        assert_eq!(hash("abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert!(lock_dir(Path::new("locks"), "models-dev:x").ends_with(format!("{}.lock", hash("models-dev:x"))));
        let stamp = iso_now();
        assert!(stamp.len() == 24 && stamp.ends_with('Z') && &stamp[4..5] == "-" && &stamp[10..11] == "T", "{stamp}");
        assert!((70..=130).contains(&jitter(100)));
    }

    #[tokio::test]
    async fn acquires_excludes_and_releases() {
        let tmp = tempfile::tempdir().unwrap();
        let locks = tmp.path().join("locks");
        let lease = acquire(&locks, "k", quick(60_000, 1000)).await.unwrap();
        let dir = lock_dir(&locks, "k");
        let meta: serde_json::Value = serde_json::from_str(&fs::read_to_string(dir.join("meta.json")).unwrap()).unwrap();
        assert!(meta["token"].is_string() && meta["pid"] == std::process::id() && meta["hostname"].is_string() && meta["createdAt"].is_string());
        assert_eq!(fs::read(dir.join("heartbeat")).unwrap(), b"");
        let blocked = acquire(&locks, "k", quick(60_000, 60)).await.err().unwrap();
        assert_eq!((blocked.kind(), blocked.to_string().as_str()), (io::ErrorKind::TimedOut, "Timed out waiting for lock: k"));
        // Another key is independent.
        acquire(&locks, "other", quick(60_000, 1000)).await.unwrap().release().unwrap();
        lease.release().unwrap();
        assert!(!dir.exists());
        drop(acquire(&locks, "k", quick(60_000, 1000)).await.unwrap());
        assert!(!dir.exists());
    }

    #[tokio::test]
    async fn breaks_stale_locks_and_refuses_foreign_release() {
        let tmp = tempfile::tempdir().unwrap();
        let locks = tmp.path().join("locks");
        let dir = lock_dir(&locks, "k");
        // A lock left behind by a process that died: its heartbeat stopped long ago.
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("heartbeat"), "").unwrap();
        fs::write(dir.join("meta.json"), r#"{"token":"theirs"}"#).unwrap();
        let old = SystemTime::now() - Duration::from_secs(120);
        fs::OpenOptions::new().write(true).open(dir.join("heartbeat")).unwrap().set_modified(old).unwrap();
        assert!(matches!(acquire(&locks, "k", Options { stale_ms: 600_000, ..quick(0, 40) }).await, Err(e) if e.kind() == io::ErrorKind::TimedOut));
        let lease = acquire(&locks, "k", quick(60_000, 1000)).await.unwrap();
        assert!(!PathBuf::from(format!("{}.breaker", dir.display())).exists());
        // The owner changed underneath us: release must not delete someone else's lock.
        fs::write(dir.join("meta.json"), r#"{"token":"theirs"}"#).unwrap();
        assert!(lease.release().unwrap_err().to_string().contains("token mismatch"));
        assert!(dir.exists());
    }
}
