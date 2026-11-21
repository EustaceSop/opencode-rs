// models.dev catalog cache: read from disk, fetch when missing or stale, share the file with the TypeScript build.
use crate::{Error, flock};
use opencode_core::paths::Paths;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, fs};

pub const DEFAULT_URL: &str = "https://models.opencode.ai";
pub const TTL: Duration = Duration::from_secs(5 * 60);
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const RETRIES: u32 = 2;

#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    // Base URL; the catalog is `<url>/api.json`.
    pub url: String,
    pub file: PathBuf,
    // Directory of cross-process locks (`<state>/locks`).
    pub locks: PathBuf,
    // `OPENCODE_MODELS_PATH`: read instead of the cache, never written or deleted.
    pub override_file: Option<PathBuf>,
    // `OPENCODE_DISABLE_MODELS_FETCH`.
    pub disabled: bool,
}

fn flag(name: &str) -> bool {
    env::var(name).is_ok_and(|v| matches!(v.to_lowercase().as_str(), "true" | "1"))
}

// Cache file for a source: `models.json` for the default, `models-<sha1(url)>.json` otherwise.
pub fn cache_file(cache: &Path, url: &str) -> PathBuf {
    cache.join(if url == DEFAULT_URL { "models.json".to_string() } else { format!("models-{}.json", flock::hash(url)) })
}

impl Source {
    pub fn from_env(paths: &Paths) -> Self {
        let url = env::var("OPENCODE_MODELS_URL").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| DEFAULT_URL.to_string());
        Source {
            file: cache_file(&paths.cache, &url),
            url,
            locks: paths.state.join("locks"),
            override_file: env::var_os("OPENCODE_MODELS_PATH").filter(|v| !v.is_empty()).map(PathBuf::from),
            disabled: flag("OPENCODE_DISABLE_MODELS_FETCH"),
        }
    }

    fn lock_key(&self) -> String {
        format!("models-dev:{}", self.file.display())
    }
}

pub fn user_agent() -> String {
    let client = env::var("OPENCODE_CLIENT").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "cli".into());
    format!("opencode/local/{}/{client}", env!("CARGO_PKG_VERSION"))
}

// Whether the cache file was written within the TTL.
pub fn fresh(file: &Path) -> bool {
    let modified = fs::metadata(file).and_then(|m| m.modified());
    modified.is_ok_and(|at| SystemTime::now().duration_since(at).unwrap_or_default() < TTL)
}

// The catalog on disk, whatever its age. A cache file that is not JSON is deleted.
pub fn read(source: &Source) -> Option<Value> {
    let file = source.override_file.as_ref().unwrap_or(&source.file);
    let text = fs::read_to_string(file).ok()?;
    match serde_json::from_str(&text) {
        Ok(value) => Some(value),
        Err(_) => {
            if source.override_file.is_none() {
                let _ = fs::remove_file(&source.file);
            }
            None
        }
    }
}

async fn fetch(http: &reqwest::Client, source: &Source) -> Result<String, Error> {
    let url = format!("{}/api.json", source.url);
    let mut attempt = 0;
    loop {
        let result = async {
            let response = http.get(&url).header("User-Agent", user_agent()).timeout(FETCH_TIMEOUT).send().await?;
            let status = response.status();
            if !status.is_success() {
                let headers = response.headers().iter().filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string()))).collect();
                return Err(Error::Status { status: status.as_u16(), body: response.text().await.unwrap_or_default(), headers });
            }
            Ok(response.text().await?)
        }
        .await;
        let transient = match &result {
            Ok(_) => false,
            Err(Error::Status { status, .. }) => matches!(status, 408 | 429 | 500 | 502 | 503 | 504),
            Err(_) => true,
        };
        if !transient || attempt >= RETRIES {
            return result;
        }
        let base = 200u64 << attempt;
        tokio::time::sleep(Duration::from_millis(base * (80 + flock::random() % 41) / 100)).await;
        attempt += 1;
    }
}

// Downloads the catalog and replaces the cache through a temp file, so readers never see a partial write.
async fn fetch_and_write(http: &reqwest::Client, source: &Source) -> Result<String, Error> {
    let text = fetch(http, source).await?;
    let io = |e: std::io::Error| Error::Config(format!("cannot write {}: {e}", source.file.display()));
    if let Some(parent) = source.file.parent() {
        fs::create_dir_all(parent).map_err(io)?;
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let temp = PathBuf::from(format!("{}.{}.{now}.tmp", source.file.display(), std::process::id()));
    if let Err(error) = fs::write(&temp, &text).and_then(|()| fs::rename(&temp, &source.file)) {
        let _ = fs::remove_file(&temp);
        return Err(io(error));
    }
    Ok(text)
}

async fn locked(source: &Source) -> Result<flock::Lease, Error> {
    flock::acquire(&source.locks, &source.lock_key(), flock::Options::default()).await.map_err(|e| Error::Config(e.to_string()))
}

// The catalog: disk cache of any age, else a fetch (unless disabled), else empty.
pub async fn load(http: &reqwest::Client, source: &Source) -> Value {
    if let Some(found) = read(source) {
        return found;
    }
    if source.disabled {
        return json!({});
    }
    let fetched = async {
        let lease = locked(source).await?;
        let text = fetch_and_write(http, source).await;
        let _ = lease.release();
        serde_json::from_str::<Value>(&text?).map_err(|e| Error::Config(format!("invalid models catalog: {e}")))
    };
    fetched.await.unwrap_or_else(|_| json!({}))
}

// Re-downloads the cache when it is older than the TTL (or always with `force`). Ok(true) when a fetch happened.
pub async fn refresh(http: &reqwest::Client, source: &Source, force: bool) -> Result<bool, Error> {
    if source.disabled || (!force && fresh(&source.file)) {
        return Ok(false);
    }
    let lease = locked(source).await?;
    // Another process may have refreshed while this one waited for the lock.
    let result = if !force && fresh(&source.file) { Ok(false) } else { fetch_and_write(http, source).await.map(|_| true) };
    let _ = lease.release();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_names_and_reads() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(cache_file(tmp.path(), DEFAULT_URL), tmp.path().join("models.json"));
        let mirror = cache_file(tmp.path(), "https://mirror.test");
        assert_eq!(mirror, tmp.path().join(format!("models-{}.json", flock::hash("https://mirror.test"))));
        let mut source = Source { url: "http://127.0.0.1:9".into(), file: tmp.path().join("models.json"), locks: tmp.path().join("locks"), override_file: None, disabled: true };
        assert!(read(&source).is_none() && !fresh(&source.file));
        fs::write(&source.file, "{not json").unwrap();
        assert!(read(&source).is_none() && !source.file.exists());
        fs::write(&source.file, r#"{"p":{"id":"p"}}"#).unwrap();
        assert_eq!(read(&source).unwrap()["p"]["id"], "p");
        assert!(fresh(&source.file));
        let old = SystemTime::now() - Duration::from_secs(301);
        fs::OpenOptions::new().write(true).open(&source.file).unwrap().set_modified(old).unwrap();
        assert!(!fresh(&source.file));
        // The override file wins and survives being unreadable.
        let other = tmp.path().join("override.json");
        fs::write(&other, "broken").unwrap();
        source.override_file = Some(other.clone());
        assert!(read(&source).is_none() && other.exists() && source.file.exists());
        assert!(user_agent().starts_with("opencode/local/"));
        assert_eq!(source.lock_key(), format!("models-dev:{}", source.file.display()));
    }
}
