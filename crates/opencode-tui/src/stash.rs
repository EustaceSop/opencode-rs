use crate::state::write_atomic;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

pub const MAX_STASH_ENTRIES: usize = 50;
pub const MAX_FRECENCY_ENTRIES: usize = 1000;

fn lines(text: &str) -> Vec<Value> {
    text.split('\n').filter(|line| !line.is_empty()).filter_map(|line| serde_json::from_str(line).ok()).collect()
}

fn serialize(entries: &[Value]) -> String {
    entries.iter().map(|entry| entry.to_string() + "\n").collect()
}

fn append(path: &Path, entry: &Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all((entry.to_string() + "\n").as_bytes())
}

#[derive(Debug, Clone, PartialEq)]
pub enum Write {
    Append(Value),
    Rewrite,
}

// `<state>/prompt-stash.jsonl`: one `{input, parts, timestamp}` per line.
#[derive(Debug, Clone, Default)]
pub struct Stash {
    pub entries: Vec<Value>,
}

pub fn stash_path(state: &Path) -> PathBuf {
    state.join("prompt-stash.jsonl")
}

impl Stash {
    pub fn parse(text: &str) -> Vec<Value> {
        let all = lines(text);
        let skip = all.len().saturating_sub(MAX_STASH_ENTRIES);
        all.into_iter().skip(skip).collect()
    }

    // The original rewrites the file on load so malformed and surplus lines are dropped.
    pub fn load(state: &Path) -> Self {
        let path = stash_path(state);
        let entries = Self::parse(&std::fs::read_to_string(&path).unwrap_or_default());
        if !entries.is_empty() {
            let _ = write_atomic(&path, &serialize(&entries));
        }
        Self { entries }
    }

    pub fn push(&mut self, input: &str, parts: &[Value], now_ms: u64) -> Write {
        let entry = json!({ "input": input, "parts": parts, "timestamp": now_ms });
        self.entries.push(entry.clone());
        if self.entries.len() > MAX_STASH_ENTRIES {
            let skip = self.entries.len() - MAX_STASH_ENTRIES;
            self.entries.drain(..skip);
            return Write::Rewrite;
        }
        Write::Append(entry)
    }

    pub fn pop(&mut self) -> Option<Value> {
        self.entries.pop()
    }

    pub fn remove(&mut self, index: usize) -> Option<Value> {
        (index < self.entries.len()).then(|| self.entries.remove(index))
    }

    pub fn persist(&self, state: &Path, write: &Write) -> std::io::Result<()> {
        let path = stash_path(state);
        match write {
            Write::Append(entry) => append(&path, entry),
            Write::Rewrite => write_atomic(&path, &serialize(&self.entries)),
        }
    }
}

// `<state>/frecency.jsonl`: one `{path, frequency, lastOpen}` per line, later lines win.
#[derive(Debug, Clone, Default)]
pub struct Frecency {
    pub data: HashMap<String, (u64, u64)>,
}

pub fn frecency_path(state: &Path) -> PathBuf {
    state.join("frecency.jsonl")
}

fn frecency_entry(path: &str, frequency: u64, last_open: u64) -> Value {
    json!({ "path": path, "frequency": frequency, "lastOpen": last_open })
}

impl Frecency {
    pub fn parse(text: &str) -> Vec<(String, u64, u64)> {
        let mut latest: Vec<(String, u64, u64)> = Vec::new();
        for line in lines(text) {
            let (Some(path), Some(frequency), Some(last_open)) = (line.get("path").and_then(Value::as_str), line.get("frequency").and_then(Value::as_u64), line.get("lastOpen").and_then(Value::as_u64)) else {
                continue;
            };
            match latest.iter_mut().find(|(p, ..)| p == path) {
                Some(slot) => *slot = (path.to_string(), frequency, last_open),
                None => latest.push((path.to_string(), frequency, last_open)),
            }
        }
        latest.sort_by_key(|(_, _, last_open)| std::cmp::Reverse(*last_open));
        latest.truncate(MAX_FRECENCY_ENTRIES);
        latest
    }

    fn sorted(&self) -> Vec<Value> {
        let mut all: Vec<(&String, &(u64, u64))> = self.data.iter().collect();
        all.sort_by(|a, b| b.1.1.cmp(&a.1.1).then_with(|| a.0.cmp(b.0)));
        all.into_iter().map(|(path, (frequency, last_open))| frecency_entry(path, *frequency, *last_open)).collect()
    }

    pub fn load(state: &Path) -> Self {
        let path = frecency_path(state);
        let entries = Self::parse(&std::fs::read_to_string(&path).unwrap_or_default());
        let out = Self { data: entries.into_iter().map(|(path, frequency, last_open)| (path, (frequency, last_open))).collect() };
        if !out.data.is_empty() {
            let _ = write_atomic(&path, &serialize(&out.sorted()));
        }
        out
    }

    // `calculateFrecency`
    pub fn score(&self, absolute: &str, now_ms: u64) -> f64 {
        match self.data.get(absolute) {
            Some((frequency, last_open)) => *frequency as f64 / (1.0 + now_ms.saturating_sub(*last_open) as f64 / 86_400_000.0),
            None => 0.0,
        }
    }

    pub fn update(&mut self, absolute: &str, now_ms: u64) -> Write {
        let frequency = self.data.get(absolute).map_or(0, |(frequency, _)| *frequency) + 1;
        self.data.insert(absolute.to_string(), (frequency, now_ms));
        if self.data.len() <= MAX_FRECENCY_ENTRIES {
            return Write::Append(frecency_entry(absolute, frequency, now_ms));
        }
        let mut all: Vec<(String, (u64, u64))> = self.data.drain().collect();
        all.sort_by_key(|(_, (_, last_open))| std::cmp::Reverse(*last_open));
        all.truncate(MAX_FRECENCY_ENTRIES);
        self.data = all.into_iter().collect();
        Write::Rewrite
    }

    pub fn persist(&self, state: &Path, write: &Write) -> std::io::Result<()> {
        let path = frecency_path(state);
        match write {
            Write::Append(entry) => append(&path, entry),
            Write::Rewrite => write_atomic(&path, &serialize(&self.sorted())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stash_round_trips_the_original_format() {
        let dir = tempfile::tempdir().unwrap();
        let mut stash = Stash::load(dir.path());
        let write = stash.push("first\nline", &[], 1_700_000_000_000);
        stash.persist(dir.path(), &write).unwrap();
        let write = stash.push("second", &[json!({"type": "file"})], 1_700_000_000_001);
        stash.persist(dir.path(), &write).unwrap();
        assert_eq!(
            std::fs::read_to_string(stash_path(dir.path())).unwrap(),
            "{\"input\":\"first\\nline\",\"parts\":[],\"timestamp\":1700000000000}\n{\"input\":\"second\",\"parts\":[{\"type\":\"file\"}],\"timestamp\":1700000000001}\n"
        );
        assert_eq!(stash.pop().unwrap()["input"], "second");
        stash.persist(dir.path(), &Write::Rewrite).unwrap();
        std::fs::write(stash_path(dir.path()), std::fs::read_to_string(stash_path(dir.path())).unwrap() + "garbage\n").unwrap();
        let loaded = Stash::load(dir.path());
        assert_eq!(loaded.entries.len(), 1);
        assert!(!std::fs::read_to_string(stash_path(dir.path())).unwrap().contains("garbage"));
        let mut many = Stash::default();
        for i in 0..MAX_STASH_ENTRIES {
            assert!(matches!(many.push(&i.to_string(), &[], 0), Write::Append(_)));
        }
        assert_eq!(many.push("over", &[], 0), Write::Rewrite);
        assert_eq!(many.entries.len(), MAX_STASH_ENTRIES);
        assert_eq!(many.remove(0).unwrap()["input"], "1");
        assert!(many.remove(99).is_none());
    }

    #[test]
    fn frecency_appends_and_keeps_the_latest_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut frecency = Frecency::load(dir.path());
        for now in [1000, 2000] {
            let write = frecency.update("/p/a.rs", now);
            frecency.persist(dir.path(), &write).unwrap();
        }
        let write = frecency.update("/p/b.rs", 3000);
        frecency.persist(dir.path(), &write).unwrap();
        let text = std::fs::read_to_string(frecency_path(dir.path())).unwrap();
        assert_eq!(text.lines().next().unwrap(), r#"{"path":"/p/a.rs","frequency":1,"lastOpen":1000}"#);
        assert_eq!(text.lines().count(), 3);
        let loaded = Frecency::load(dir.path());
        assert_eq!(loaded.data["/p/a.rs"], (2, 2000));
        assert_eq!(
            std::fs::read_to_string(frecency_path(dir.path())).unwrap(),
            "{\"path\":\"/p/b.rs\",\"frequency\":1,\"lastOpen\":3000}\n{\"path\":\"/p/a.rs\",\"frequency\":2,\"lastOpen\":2000}\n"
        );
        assert_eq!(loaded.score("/p/a.rs", 2000), 2.0);
        assert_eq!(loaded.score("/p/a.rs", 2000 + 86_400_000), 1.0);
        assert_eq!(loaded.score("/p/none", 0), 0.0);
    }
}
