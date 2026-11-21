use std::env;
use std::path::{Path, PathBuf};

const APP: &str = "opencode";

#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
    pub config: PathBuf,
    pub state: PathBuf,
    pub tmp: PathBuf,
}

fn xdg(var: &str, home: &Path, fallback: &str) -> PathBuf {
    match env::var_os(var).filter(|v| !v.is_empty()) {
        Some(v) => PathBuf::from(v),
        None => fallback.split('/').fold(home.to_path_buf(), |p, s| p.join(s)),
    }
}

impl Paths {
    // XDG layout on every OS, matching the original (no %APPDATA%, no ~/Library).
    pub fn from_env() -> Self {
        let home = env::home_dir().unwrap_or_else(|| PathBuf::from("."));
        Self {
            data: xdg("XDG_DATA_HOME", &home, ".local/share").join(APP),
            cache: xdg("XDG_CACHE_HOME", &home, ".cache").join(APP),
            config: xdg("XDG_CONFIG_HOME", &home, ".config").join(APP),
            state: xdg("XDG_STATE_HOME", &home, ".local/state").join(APP),
            tmp: env::temp_dir().join(APP),
            home,
        }
    }

    pub fn bin(&self) -> PathBuf {
        self.cache.join("bin")
    }
    pub fn log(&self) -> PathBuf {
        self.data.join("log")
    }
    pub fn auth_file(&self) -> PathBuf {
        self.data.join("auth.json")
    }
    // `<XDG config>/opencode-rs`: global config read by the Rust build only (the importer writes here).
    pub fn rust_config(&self) -> PathBuf {
        self.config.with_file_name(RUST_APP)
    }
}

const RUST_APP: &str = "opencode-rs";
