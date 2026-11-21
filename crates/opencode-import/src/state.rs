// `<state>/import-rs.json`: whether the first-run hint was shown, and the last import per source.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use opencode_core::flag::{self, Lookup};
use opencode_core::paths::Paths;
use serde::{Deserialize, Serialize};

use crate::{Category, ImportReport, sources};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceState {
    #[serde(rename = "lastImport")]
    pub last_import: u64,
    pub counts: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub asked: bool,
    #[serde(default)]
    pub sources: BTreeMap<String, SourceState>,
}

pub fn state_file(paths: &Paths) -> PathBuf {
    paths.state.join("import-rs.json")
}

pub fn load_state(paths: &Paths) -> State {
    std::fs::read_to_string(state_file(paths)).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

fn save(paths: &Paths, state: &State) -> std::io::Result<()> {
    let file = state_file(paths);
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(file, serde_json::to_string_pretty(state).unwrap_or_default())
}

// Display names of the sources that look importable, from cheap existence checks only.
pub fn detect(paths: &Paths, home: &Path) -> Vec<&'static str> {
    detect_with(paths, home, &flag::process_env)
}

pub fn detect_with(paths: &Paths, home: &Path, env: Lookup) -> Vec<&'static str> {
    sources::detect(paths, home, env).into_iter().map(crate::source_name).collect()
}

// True until the user has been told about importable data once (or has imported).
pub fn first_run_pending(paths: &Paths, home: &Path) -> bool {
    !load_state(paths).asked && !detect(paths, home).is_empty()
}

pub fn mark_asked(paths: &Paths) -> std::io::Result<()> {
    let mut state = load_state(paths);
    if state.asked {
        return Ok(());
    }
    state.asked = true;
    save(paths, &state)
}

pub(crate) fn record(paths: &Paths, report: &ImportReport) {
    let mut state = load_state(paths);
    state.asked = true;
    let now = opencode_storage::id::now_ms();
    for outcome in &report.outcomes {
        let entry = state.sources.entry(outcome.source.to_string()).or_default();
        entry.last_import = now;
        let count = entry.counts.entry(outcome.category.as_str().to_string()).or_default();
        *count = outcome.written + outcome.unchanged;
    }
    for category in Category::ALL {
        for entry in state.sources.values_mut() {
            entry.counts.entry(category.as_str().to_string()).or_default();
        }
    }
    let _ = save(paths, &state);
}
