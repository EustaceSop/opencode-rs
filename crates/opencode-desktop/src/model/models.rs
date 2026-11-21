// What the model picker lists and how: visibility, favourites, variants and search.
// Visibility follows the original app's `model` store (`providers/models/models.tsx`): a `user` list of
// `{providerID, modelID, visibility}` overrides over a default; that store is the original's own, so this client keeps
// the same list in `desktop-rs.json`. Favourites and variants live in `<state>/model.json`, the file the TUI reads and
// writes (`{recent, favorite, variant}`), so both clients agree on them.
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

// `hooks/use-providers.ts` `popularProviders`: listed first, in this order.
pub const POPULAR: [&str; 8] = ["opencode", "opencode-go", "anthropic", "github-copilot", "openai", "google", "openrouter", "vercel"];

// luxon's average month, as `models.tsx` keeps it.
const SIX_MONTHS_MS: f64 = 6.0 * 30.436875 * 24.0 * 60.0 * 60.0 * 1000.0;

pub fn popular_rank(provider_id: &str) -> usize {
    POPULAR.iter().position(|id| *id == provider_id).unwrap_or(POPULAR.len())
}

// "provider/model": provider ids never contain a slash, model ids may.
pub fn split(spec: &str) -> Option<(&str, &str)> {
    spec.split_once('/')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    Show,
    Hide,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserModel {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(rename = "modelID")]
    pub model_id: String,
    pub visibility: Visibility,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelPrefs {
    #[serde(default)]
    pub user: Vec<UserModel>,
}

impl ModelPrefs {
    pub fn is_empty(&self) -> bool {
        self.user.is_empty()
    }

    pub fn visibility(&self, spec: &str) -> Option<bool> {
        let (provider, model) = split(spec)?;
        self.user.iter().find(|u| u.provider_id == provider && u.model_id == model).map(|u| u.visibility == Visibility::Show)
    }

    // `setVisibility`: an existing entry is updated in place, a new one appended. Returns whether anything changed.
    pub fn set_visible(&mut self, spec: &str, visible: bool) -> bool {
        let Some((provider, model)) = split(spec) else { return false };
        let visibility = if visible { Visibility::Show } else { Visibility::Hide };
        match self.user.iter_mut().find(|u| u.provider_id == provider && u.model_id == model) {
            Some(entry) if entry.visibility == visibility => false,
            Some(entry) => {
                entry.visibility = visibility;
                true
            }
            None => {
                self.user.push(UserModel { provider_id: provider.into(), model_id: model.into(), visibility });
                true
            }
        }
    }
}

// `<state>/model.json`, the part this client uses.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Shared {
    // "provider/model", newest favourite first.
    pub favorite: Vec<String>,
    // "provider/model" -> variant name, "default" for the explicit default.
    pub variant: Map<String, Value>,
}

fn read(path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str::<Value>(&text).ok()).and_then(|v| v.as_object().cloned()).unwrap_or_default()
}

fn key(entry: &Value) -> Option<String> {
    Some(format!("{}/{}", entry.get("providerID")?.as_str()?, entry.get("modelID")?.as_str()?))
}

pub fn file(state_dir: &Path) -> PathBuf {
    state_dir.join("model.json")
}

impl Shared {
    pub fn load(path: &Path) -> Self {
        let value = read(path);
        let favorite = value.get("favorite").and_then(Value::as_array).into_iter().flatten().filter_map(key).collect();
        Self { favorite, variant: value.get("variant").and_then(Value::as_object).cloned().unwrap_or_default() }
    }

    pub fn is_favorite(&self, spec: &str) -> bool {
        self.favorite.iter().any(|f| f == spec)
    }

    // The selected variant of a model, when it still names one of the model's variants (`current_variant`).
    pub fn variant(&self, spec: &str, variants: &[Arc<str>]) -> Option<String> {
        let selected = self.variant.get(spec)?.as_str()?;
        variants.iter().any(|v| v.as_ref() == selected).then(|| selected.to_string())
    }

    // Reads the file again, applies `edit` and writes it back, so keys another client wrote meanwhile (the TUI's
    // `recent`, its other favourites) survive. The TUI writes compact JSON in `{recent, favorite, variant}` order.
    pub fn update(path: &Path, edit: impl FnOnce(&mut Self)) -> std::io::Result<Self> {
        let mut file = read(path);
        let mut shared = Self::load(path);
        edit(&mut shared);
        let favorite: Vec<Value> = shared
            .favorite
            .iter()
            .filter_map(|spec| split(spec))
            .map(|(provider, model)| json!({"providerID": provider, "modelID": model}))
            .collect();
        if !file.contains_key("recent") {
            file.insert("recent".into(), json!([]));
        }
        file.insert("favorite".into(), Value::Array(favorite));
        file.insert("variant".into(), Value::Object(shared.variant.clone()));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
        std::fs::write(&temporary, Value::Object(file).to_string())?;
        std::fs::rename(&temporary, path)?;
        Ok(shared)
    }

    // `toggleFavorite`: newest first, as in the TUI.
    pub fn toggle_favorite(&mut self, spec: &str) {
        if self.is_favorite(spec) {
            self.favorite.retain(|f| f != spec);
        } else {
            self.favorite.insert(0, spec.to_string());
        }
    }

    // `setVariant`: no variant is stored as "default".
    pub fn set_variant(&mut self, spec: &str, value: Option<&str>) {
        self.variant.insert(spec.to_string(), Value::from(value.unwrap_or("default")));
    }
}

// `dialog-select-model-search.ts`.
fn normalize(value: &str) -> String {
    let lowered: String = value.to_lowercase().chars().map(|c| if c.is_alphanumeric() { c } else { ' ' }).collect();
    lowered.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn matches_search(query: &str, values: &[&str]) -> bool {
    let search = normalize(query);
    if search.is_empty() {
        return true;
    }
    let compact = search.replace(' ', "");
    values.iter().any(|value| {
        let value = normalize(value);
        value.contains(&search) || value.replace(' ', "").contains(&compact)
    })
}

// Milliseconds since the epoch of `YYYY`, `YYYY-MM` or `YYYY-MM-DD` (a time after `T` is ignored), as UTC.
// UNKNOWN: `Date.parse` accepts more formats; catalog release dates use these.
pub fn release_ms(text: &str) -> Option<f64> {
    let date = text.split('T').next()?.trim();
    let mut parts = date.split('-');
    let number = |part: Option<&str>, len: usize| part.filter(|p| p.len() == len && p.bytes().all(|b| b.is_ascii_digit())).and_then(|p| p.parse::<i64>().ok());
    let year = number(parts.next(), 4)?;
    let month = match parts.next() {
        Some(part) => number(Some(part), 2).filter(|m| (1..=12).contains(m))?,
        None => 1,
    };
    let day = match parts.next() {
        Some(part) => number(Some(part), 2).filter(|d| (1..=31).contains(d))?,
        None => 1,
    };
    if parts.next().is_some() {
        return None;
    }
    // Days from civil (Howard Hinnant).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(((era * 146_097 + doe - 719_468) * 86_400_000) as f64)
}

// One model of a provider as the default visibility needs it.
pub struct Candidate<'a> {
    pub family: &'a str,
    pub release: &'a str,
    pub free: bool,
}

// `visible` without a user override: the newest release of each family (per free/paid) from the last six months,
// free models, and models without a parseable release date are shown.
pub fn default_visible(models: &[Candidate], now_ms: f64) -> Vec<bool> {
    let mut shown: Vec<bool> = models.iter().map(|m| m.free || release_ms(m.release).is_none()).collect();
    let recent: Vec<usize> = (0..models.len()).filter(|&i| release_ms(models[i].release).is_some_and(|r| (now_ms - r).abs() < SIX_MONTHS_MS)).collect();
    let mut groups: Vec<((&str, bool), usize)> = Vec::new();
    for i in recent {
        let group = (models[i].family, models[i].free);
        match groups.iter_mut().find(|(g, _)| *g == group) {
            // `x.release_date > best.release_date`: a tie keeps the first listed.
            Some((_, best)) if models[i].release > models[*best].release => *best = i,
            Some(_) => {}
            None => groups.push((group, i)),
        }
    }
    for (_, best) in groups {
        shown[best] = true;
    }
    shown
}

// `composer/model-label.ts`: "Provider / Model", the model alone for OpenCode's own models.
pub fn chip_label(provider_id: &str, provider: &str, model: &str) -> String {
    if provider_id == "opencode" { model.to_string() } else { format!("{provider} / {model}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visibility_overrides_are_kept_in_the_original_shape() {
        let mut prefs = ModelPrefs::default();
        assert_eq!(prefs.visibility("a/m"), None);
        assert!(prefs.set_visible("a/m", false));
        assert!(!prefs.set_visible("a/m", false));
        assert!(prefs.set_visible("a/m", true));
        assert!(prefs.set_visible("b/x/y", false));
        assert_eq!((prefs.visibility("a/m"), prefs.visibility("b/x/y")), (Some(true), Some(false)));
        assert_eq!(
            serde_json::to_value(&prefs).unwrap(),
            json!({"user": [{"providerID": "a", "modelID": "m", "visibility": "show"}, {"providerID": "b", "modelID": "x/y", "visibility": "hide"}]})
        );
    }

    #[test]
    fn favourites_and_variants_share_the_tui_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path());
        // What the TUI wrote (`state.rs` `ModelState::save`).
        std::fs::write(&path, r#"{"recent":[{"providerID":"anthropic","modelID":"claude"}],"favorite":[{"providerID":"openai","modelID":"gpt"}],"variant":{"anthropic/claude":"high"}}"#).unwrap();
        let shared = Shared::update(&path, |s| s.toggle_favorite("anthropic/claude")).unwrap();
        assert_eq!(shared.favorite, ["anthropic/claude", "openai/gpt"]);
        Shared::update(&path, |s| {
            s.toggle_favorite("openai/gpt");
            s.set_variant("openai/gpt", Some("max"));
            s.set_variant("anthropic/claude", None);
        })
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"recent":[{"providerID":"anthropic","modelID":"claude"}],"favorite":[{"providerID":"anthropic","modelID":"claude"}],"variant":{"anthropic/claude":"default","openai/gpt":"max"}}"#
        );
        let loaded = Shared::load(&path);
        let variants: Vec<Arc<str>> = vec!["high".into(), "max".into()];
        assert_eq!(loaded.variant("openai/gpt", &variants).as_deref(), Some("max"));
        assert_eq!(loaded.variant("anthropic/claude", &variants), None, "the explicit default is no variant");
        assert_eq!(loaded.variant("openai/gpt", &variants[..1]), None, "a variant the model no longer has is ignored");

        // A missing file starts in the TUI's key order.
        let fresh = file(&dir.path().join("new"));
        Shared::update(&fresh, |s| s.toggle_favorite("a/b")).unwrap();
        assert_eq!(std::fs::read_to_string(&fresh).unwrap(), r#"{"recent":[],"favorite":[{"providerID":"a","modelID":"b"}],"variant":{}}"#);
    }

    #[test]
    fn search_matches_like_the_original() {
        assert!(matches_search("", &["anything"]));
        assert!(matches_search("gpt 5", &["GPT-5 Mini"]));
        assert!(matches_search("gpt5", &["GPT-5 Mini"]));
        assert!(matches_search("claude", &["Sonnet", "anthropic/claude-sonnet"]));
        assert!(!matches_search("gemini", &["GPT-5", "openai"]));
    }

    #[test]
    fn default_visibility_keeps_the_newest_of_each_family() {
        let now = release_ms("2026-11-01").unwrap();
        assert_eq!(release_ms("1970-01-02"), Some(86_400_000.0));
        assert_eq!(release_ms("2024-13-01"), None);
        assert_eq!(release_ms("soon"), None);
        let models = [
            Candidate { family: "gpt", release: "2026-09-01", free: false },
            Candidate { family: "gpt", release: "2026-10-01", free: false },
            Candidate { family: "gpt", release: "2024-01-01", free: false },
            Candidate { family: "old", release: "2023-01-01", free: true },
            Candidate { family: "x", release: "", free: false },
            Candidate { family: "claude", release: "2026-08", free: false },
        ];
        assert_eq!(default_visible(&models, now), [false, true, false, true, true, true]);
    }

    #[test]
    fn chip_labels_name_the_provider() {
        assert_eq!(chip_label("anthropic", "Anthropic", "Claude"), "Anthropic / Claude");
        assert_eq!(chip_label("opencode", "OpenCode Zen", "Big Pickle"), "Big Pickle");
    }
}
