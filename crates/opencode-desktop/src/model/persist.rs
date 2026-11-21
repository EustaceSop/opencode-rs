// UI state kept across launches in `<state>/desktop-rs.json`.
use super::models::ModelPrefs;
use super::settings::Settings;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const FILE: &str = "desktop-rs.json";
const DRAFT_LIMIT: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowGeometry {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub maximized: bool,
}

impl WindowGeometry {
    // A geometry written by a broken run must not produce an unusable window.
    pub fn is_sane(&self) -> bool {
        [self.x, self.y, self.width, self.height].iter().all(|v| v.is_finite())
            && (320.0..=16_384.0).contains(&self.width)
            && (240.0..=16_384.0).contains(&self.height)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    // Unsent prompt text per session id; the empty key is the new-session draft.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub drafts: BTreeMap<String, String>,
}

impl ProjectState {
    pub fn draft(&self, session: Option<&str>) -> &str {
        self.drafts.get(session.unwrap_or("")).map_or("", String::as_str)
    }

    pub fn set_draft(&mut self, session: Option<&str>, text: &str) {
        let key = session.unwrap_or("");
        if text.trim().is_empty() {
            self.drafts.remove(key);
        } else {
            self.drafts.insert(key.to_string(), text.to_string());
        }
        // Oldest session ids sort first, so trimming from the front drops the stalest drafts.
        while self.drafts.len() > DRAFT_LIMIT {
            let stale = self.drafts.keys().find(|k| !k.is_empty() && k.as_str() != key).cloned();
            match stale {
                Some(stale) => drop(self.drafts.remove(&stale)),
                None => break,
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Persisted {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowGeometry>,
    #[serde(default)]
    pub projects: BTreeMap<String, ProjectState>,
    #[serde(default)]
    pub settings: Settings,
    // Sent prompts, oldest first (the original's global `prompt-history`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<String>,
    // Which models the picker lists (the original app's `model` store `user` list).
    #[serde(default, skip_serializing_if = "ModelPrefs::is_empty")]
    pub models: ModelPrefs,
}

pub fn file(state_dir: &Path) -> PathBuf {
    state_dir.join(FILE)
}

fn key(directory: &Path) -> String {
    directory.to_string_lossy().into_owned()
}

impl Persisted {
    // A missing or corrupt file yields defaults: this state is a convenience, never a requirement.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&temporary, path)
    }

    pub fn project(&self, directory: &Path) -> ProjectState {
        self.projects.get(&key(directory)).cloned().unwrap_or_default()
    }

    // Returns whether anything changed, so callers only write when needed.
    pub fn update_project(&mut self, directory: &Path, edit: impl FnOnce(&mut ProjectState)) -> bool {
        let entry = self.projects.entry(key(directory)).or_default();
        let before = entry.clone();
        edit(entry);
        *entry != before
    }

    pub fn set_window(&mut self, geometry: WindowGeometry) -> bool {
        if !geometry.is_sane() || self.window == Some(geometry) {
            return false;
        }
        self.window = Some(geometry);
        true
    }
}

enum Job {
    Save(Box<Persisted>),
    Flush(std::sync::mpsc::Sender<()>),
}

// Writes happen on one thread, in the order they were requested.
pub struct Saver {
    jobs: std::sync::mpsc::Sender<Job>,
}

impl Saver {
    pub fn spawn(path: PathBuf) -> Self {
        let (jobs, queue) = std::sync::mpsc::channel();
        let worker = move || {
            for job in queue {
                match job {
                    Job::Save(state) => {
                        let _ = state.save(&path);
                    }
                    Job::Flush(done) => {
                        let _ = done.send(());
                    }
                }
            }
        };
        let _ = std::thread::Builder::new().name("opencode-persist".into()).spawn(worker);
        Self { jobs }
    }

    // Accepts and drops every request; used when nothing may be written.
    pub fn disabled() -> Self {
        Self { jobs: std::sync::mpsc::channel().0 }
    }

    pub fn save(&self, state: &Persisted) {
        let _ = self.jobs.send(Job::Save(Box::new(state.clone())));
    }

    // Returns once everything requested so far is on disk.
    pub fn flush(&self) {
        let (done, wait) = std::sync::mpsc::channel();
        if self.jobs.send(Job::Flush(done)).is_ok() {
            let _ = wait.recv_timeout(std::time::Duration::from_secs(2));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saver_writes_in_order_and_flushes() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path());
        let saver = Saver::spawn(path.clone());
        let mut state = Persisted::default();
        for n in 0..20 {
            state.update_project(Path::new("/p"), |p| p.last_session = Some(format!("ses_{n}")));
            saver.save(&state);
        }
        saver.flush();
        assert_eq!(Persisted::load(&path).project(Path::new("/p")).last_session.as_deref(), Some("ses_19"));

        let silent = Saver::disabled();
        silent.save(&state);
        silent.flush();
    }

    #[test]
    fn round_trips_through_the_state_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir.path().join("nested"));
        assert_eq!(Persisted::load(&path), Persisted::default());

        let mut state = Persisted::default();
        let project = Path::new("/work/app");
        assert!(state.set_window(WindowGeometry { x: 10.0, y: 20.0, width: 1200.0, height: 800.0, maximized: false }));
        assert!(state.update_project(project, |p| p.last_session = Some("ses_1".into())));
        assert!(!state.update_project(project, |p| p.last_session = Some("ses_1".into())));
        state.save(&path).unwrap();

        let loaded = Persisted::load(&path);
        assert_eq!(loaded, state);
        assert_eq!(loaded.project(project).last_session.as_deref(), Some("ses_1"));
        assert_eq!(loaded.project(Path::new("/other")), ProjectState::default());
        assert_eq!(path.file_name().unwrap(), FILE);
    }

    #[test]
    fn settings_history_and_drafts_persist() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path());
        let project = Path::new("/work/app");
        let mut state = Persisted::default();
        state.settings.theme = "nord".into();
        state.history = vec!["first".into(), "second".into()];
        state.update_project(project, |p| {
            p.set_draft(None, "unsent");
            p.set_draft(Some("ses_1"), "half a thought");
            p.set_draft(Some("ses_2"), "   ");
        });
        state.save(&path).unwrap();

        let loaded = Persisted::load(&path);
        assert_eq!(loaded, state);
        let saved = loaded.project(project);
        assert_eq!((saved.draft(None), saved.draft(Some("ses_1")), saved.draft(Some("ses_2"))), ("unsent", "half a thought", ""));

        // A file written by the first version has neither settings nor drafts.
        std::fs::write(&path, r#"{"projects": {"/work/app": {"last_session": "ses_9"}}}"#).unwrap();
        let old = Persisted::load(&path);
        assert_eq!(old.settings, Settings::default());
        assert_eq!(old.project(project).last_session.as_deref(), Some("ses_9"));

        let mut many = ProjectState::default();
        for n in 0..DRAFT_LIMIT + 10 {
            many.set_draft(Some(&format!("ses_{n:03}")), "text");
        }
        many.set_draft(None, "draft");
        assert_eq!(many.drafts.len(), DRAFT_LIMIT);
        assert_eq!(many.draft(None), "draft");
        assert_eq!(many.draft(Some("ses_000")), "");
    }

    #[test]
    fn corrupt_files_and_bad_geometry_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path());
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(Persisted::load(&path), Persisted::default());

        let mut state = Persisted::default();
        assert!(!state.set_window(WindowGeometry { x: 0.0, y: 0.0, width: 0.0, height: 600.0, maximized: false }));
        assert!(!state.set_window(WindowGeometry { x: f32::NAN, y: 0.0, width: 800.0, height: 600.0, maximized: false }));
        assert_eq!(state.window, None);
    }
}
