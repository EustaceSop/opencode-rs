use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const MAX_HISTORY_ENTRIES: usize = 50;

// Write-then-rename so a concurrent reader never sees a partial file.
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

// `<state>/kv.json`
#[derive(Debug, Clone, Default)]
pub struct Kv {
    pub path: PathBuf,
    pub map: Map<String, Value>,
    // Keys unset since load (`kv.set(key, undefined)`).
    removed: Vec<String>,
}

impl Kv {
    pub fn load(state: &Path) -> Self {
        let path = state.join("kv.json");
        let map = read_json(&path).and_then(|v| v.as_object().cloned()).unwrap_or_default();
        Self { path, map, removed: Vec::new() }
    }

    pub fn remove(&mut self, key: &str) {
        self.map.shift_remove(key);
        self.removed.push(key.to_string());
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.map.get(key).and_then(Value::as_str)
    }

    pub fn get_bool(&self, key: &str, default: bool) -> bool {
        self.map.get(key).and_then(Value::as_bool).unwrap_or(default)
    }

    pub fn set(&mut self, key: &str, value: Value) {
        self.removed.retain(|removed| removed != key);
        self.map.insert(key.to_string(), value);
    }

    pub fn save(&self) -> std::io::Result<()> {
        // Keys written by another process since load are kept.
        let mut merged = read_json(&self.path).and_then(|v| v.as_object().cloned()).unwrap_or_default();
        for (key, value) in &self.map {
            merged.insert(key.clone(), value.clone());
        }
        for key in &self.removed {
            merged.shift_remove(key);
        }
        // `writeJsonAtomic` stores compact JSON.
        write_atomic(&self.path, &Value::Object(merged).to_string())
    }
}

// `<state>/session.json`: `{pinned}`, session ids in the order they were pinned.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pins {
    pub pinned: Vec<String>,
}

impl Pins {
    pub fn load(state: &Path) -> Self {
        let pinned = read_json(&state.join("session.json"))
            .and_then(|value| value.get("pinned").and_then(Value::as_array).cloned())
            .map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        Self { pinned }
    }

    pub fn save(&self, state: &Path) -> std::io::Result<()> {
        write_atomic(&state.join("session.json"), &json!({ "pinned": self.pinned }).to_string())
    }

    pub fn toggle(&mut self, session_id: &str) {
        match self.pinned.iter().position(|id| id == session_id) {
            Some(index) => {
                self.pinned.remove(index);
            }
            None => self.pinned.push(session_id.to_string()),
        }
    }

    // `slots`: the first nine pinned sessions that still exist.
    pub fn slots<'a>(&'a self, existing: &[&str]) -> Vec<&'a str> {
        self.pinned.iter().map(String::as_str).filter(|id| existing.contains(id)).take(9).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelKey {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(rename = "modelID")]
    pub model_id: String,
}

impl ModelKey {
    pub fn new(provider_id: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self { provider_id: provider_id.into(), model_id: model_id.into() }
    }

    pub fn parse(spec: &str) -> Option<Self> {
        let (provider, model) = spec.split_once('/')?;
        Some(Self::new(provider, model))
    }

    pub fn spec(&self) -> String {
        format!("{}/{}", self.provider_id, self.model_id)
    }
}

// `<state>/model.json`: `{recent, favorite, variant}`
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelState {
    pub recent: Vec<ModelKey>,
    pub favorite: Vec<ModelKey>,
    pub variant: Map<String, Value>,
}

impl ModelState {
    pub fn load(state: &Path) -> Self {
        let Some(value) = read_json(&state.join("model.json")) else { return Self::default() };
        let list = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(|item| serde_json::from_value(item.clone()).ok()).collect())
                .unwrap_or_default()
        };
        Self { recent: list("recent"), favorite: list("favorite"), variant: value.get("variant").and_then(Value::as_object).cloned().unwrap_or_default() }
    }

    pub fn save(&self, state: &Path) -> std::io::Result<()> {
        let value = json!({ "recent": self.recent, "favorite": self.favorite, "variant": self.variant });
        write_atomic(&state.join("model.json"), &value.to_string())
    }

    // `recentModels`: newest first, unique, at most ten.
    pub fn push_recent(&mut self, model: &ModelKey) {
        let mut next = vec![model.clone()];
        next.extend(self.recent.iter().filter(|item| *item != model).cloned());
        next.truncate(10);
        self.recent = next;
    }

    pub fn toggle_favorite(&mut self, model: &ModelKey) {
        if self.favorite.contains(model) {
            self.favorite.retain(|item| item != model);
        } else {
            self.favorite.insert(0, model.clone());
        }
    }
}

// `<state>/prompt-history.jsonl`: one `PromptInfo` (`{input, mode?, parts}`) per line.
#[derive(Debug, Clone, Default)]
pub struct History {
    pub entries: Vec<Value>,
    // 0 is the live prompt; -1 the newest entry.
    pub index: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HistoryWrite {
    None,
    Append(Value),
    Rewrite,
}

pub fn history_path(state: &Path) -> PathBuf {
    state.join("prompt-history.jsonl")
}

impl History {
    pub fn parse(text: &str) -> Vec<Value> {
        let all: Vec<Value> = text
            .split('\n')
            .filter(|line| !line.is_empty())
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(Value::is_object)
            .collect();
        let skip = all.len().saturating_sub(MAX_HISTORY_ENTRIES);
        all.into_iter().skip(skip).collect()
    }

    pub fn load(state: &Path) -> Self {
        let entries = Self::parse(&std::fs::read_to_string(history_path(state)).unwrap_or_default());
        Self { entries, index: 0 }
    }

    pub fn serialize(&self) -> String {
        self.entries.iter().map(|entry| entry.to_string() + "\n").collect()
    }

    fn at(&self, index: i64) -> Option<&Value> {
        let len = self.entries.len() as i64;
        let position = if index < 0 { len + index } else { index };
        self.entries.get(usize::try_from(position).ok()?)
    }

    fn input(entry: &Value) -> &str {
        entry.get("input").and_then(Value::as_str).unwrap_or_default()
    }

    // Port of `move`: only navigates when the prompt is empty or still shows the entry under the index.
    pub fn step(&mut self, direction: i64, input: &str) -> Option<Value> {
        let current = self.at(self.index)?;
        if Self::input(current) != input && !input.is_empty() {
            return None;
        }
        let next = self.index + direction;
        if next.unsigned_abs() as usize <= self.entries.len() && next <= 0 {
            self.index = next;
        }
        if self.index == 0 {
            return Some(json!({ "input": "", "parts": [] }));
        }
        self.at(self.index).cloned()
    }

    pub fn append(&mut self, entry: Value) -> HistoryWrite {
        self.index = 0;
        if self.entries.last() == Some(&entry) {
            return HistoryWrite::None;
        }
        self.entries.push(entry.clone());
        if self.entries.len() > MAX_HISTORY_ENTRIES {
            let skip = self.entries.len() - MAX_HISTORY_ENTRIES;
            self.entries.drain(..skip);
            return HistoryWrite::Rewrite;
        }
        HistoryWrite::Append(entry)
    }

    pub fn persist(&self, state: &Path, write: &HistoryWrite) -> std::io::Result<()> {
        let path = history_path(state);
        match write {
            HistoryWrite::None => Ok(()),
            HistoryWrite::Rewrite => write_atomic(&path, &self.serialize()),
            HistoryWrite::Append(entry) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
                file.write_all((entry.to_string() + "\n").as_bytes())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_state_round_trips_the_original_format() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("model.json"),
            r#"{"recent":[{"providerID":"anthropic","modelID":"claude"}],"favorite":[],"variant":{"anthropic/claude":"high"}}"#,
        )
        .unwrap();
        let mut state = ModelState::load(dir.path());
        assert_eq!(state.recent, vec![ModelKey::new("anthropic", "claude")]);
        state.push_recent(&ModelKey::new("openai", "gpt"));
        state.push_recent(&ModelKey::new("anthropic", "claude"));
        state.toggle_favorite(&ModelKey::new("openai", "gpt"));
        state.save(dir.path()).unwrap();
        let text = std::fs::read_to_string(dir.path().join("model.json")).unwrap();
        assert_eq!(
            text,
            r#"{"recent":[{"providerID":"anthropic","modelID":"claude"},{"providerID":"openai","modelID":"gpt"}],"favorite":[{"providerID":"openai","modelID":"gpt"}],"variant":{"anthropic/claude":"high"}}"#
        );
        for i in 0..12 {
            state.push_recent(&ModelKey::new("p", format!("m{i}")));
        }
        assert_eq!(state.recent.len(), 10);
    }

    #[test]
    fn history_navigation_and_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let mut history = History::load(dir.path());
        assert!(history.step(-1, "").is_none());
        for text in ["one", "two"] {
            let write = history.append(json!({"input": text, "parts": []}));
            history.persist(dir.path(), &write).unwrap();
        }
        assert!(matches!(history.append(json!({"input": "two", "parts": []})), HistoryWrite::None));
        assert_eq!(std::fs::read_to_string(history_path(dir.path())).unwrap(), "{\"input\":\"one\",\"parts\":[]}\n{\"input\":\"two\",\"parts\":[]}\n");
        assert_eq!(history.step(-1, "").unwrap()["input"], "two");
        assert_eq!(history.step(-1, "two").unwrap()["input"], "one");
        assert_eq!(history.step(-1, "one").unwrap()["input"], "one");
        assert!(history.step(-1, "edited").is_none());
        assert_eq!(history.step(1, "one").unwrap()["input"], "two");
        assert_eq!(history.step(1, "two").unwrap()["input"], "");

        let many: String = (0..60).map(|i| format!("{{\"input\":\"{i}\",\"parts\":[]}}\n")).collect();
        let parsed = History::parse(&(many + "garbage\n"));
        assert_eq!(parsed.len(), MAX_HISTORY_ENTRIES);
        assert_eq!(parsed[0]["input"], "10");
    }

    #[test]
    fn kv_keeps_foreign_keys() {
        let dir = tempfile::tempdir().unwrap();
        let mut kv = Kv::load(dir.path());
        std::fs::write(dir.path().join("kv.json"), r#"{"other":1}"#).unwrap();
        kv.set("theme", json!("nord"));
        kv.save().unwrap();
        let loaded = Kv::load(dir.path());
        assert_eq!(loaded.get_str("theme"), Some("nord"));
        assert_eq!(loaded.map.get("other"), Some(&json!(1)));
        assert!(loaded.get_bool("missing", true));
        let mut kv = loaded;
        kv.remove("other");
        kv.save().unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("kv.json")).unwrap(), r#"{"theme":"nord"}"#);
    }

    #[test]
    fn pins_round_trip_and_fill_quick_slots() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("session.json"), r#"{"pinned":["ses_a",7,"ses_b"]}"#).unwrap();
        let mut pins = Pins::load(dir.path());
        assert_eq!(pins.pinned, vec!["ses_a".to_string(), "ses_b".to_string()]);
        pins.toggle("ses_c");
        pins.toggle("ses_a");
        pins.save(dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("session.json")).unwrap(), r#"{"pinned":["ses_b","ses_c"]}"#);
        assert_eq!(pins.slots(&["ses_c", "ses_x"]), vec!["ses_c"]);
        assert!(Pins::load(&dir.path().join("missing")).pinned.is_empty());
    }
}
