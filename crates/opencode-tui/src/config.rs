use crate::keybind;
use jsonc_parser::ParseOptions;
use opencode_core::config::{self, LoadOptions, merge_deep};
use opencode_core::flag::{self, Lookup};
use opencode_core::paths::Paths;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

pub const LEADER_TIMEOUT_DEFAULT: u64 = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxWidth {
    Fixed(u16),
    Auto,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TuiConfig {
    pub theme: Option<String>,
    pub keybinds: Map<String, Value>,
    pub leader_timeout: u64,
    pub prompt_max_height: Option<u16>,
    pub prompt_max_width: MaxWidth,
    pub scroll_speed: f64,
    pub diff_style: Option<String>,
    pub mouse: bool,
    pub warnings: Vec<String>,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self::from_value(&Value::Object(Map::new()))
    }
}

fn substitute_env(text: &str, env: Lookup) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{env:") {
        let after = &rest[start + 5..];
        match after.find('}') {
            Some(end) if end > 0 => {
                out.push_str(&rest[..start]);
                out.push_str(&env(&after[..end]).unwrap_or_default());
                rest = &after[end + 1..];
            }
            _ => {
                out.push_str(&rest[..start + 5]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

// `{file:path}` becomes the file's trimmed content, JSON-escaped. Paths are relative to the config file, `~/` is
// the home directory; a missing file yields an empty string (the TUI loader's `missing: "empty"`), and tokens on
// `//` comment lines are left alone.
fn substitute_file(text: &str, directory: &Path, home: &Path) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(found) = text[cursor..].find("{file:") {
        let start = cursor + found;
        let Some(length) = text[start..].find('}').filter(|length| *length > 6) else {
            out.push_str(&text[cursor..start + 6]);
            cursor = start + 6;
            continue;
        };
        let end = start + length + 1;
        out.push_str(&text[cursor..start]);
        cursor = end;
        let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
        if text[line_start..start].trim_start().starts_with("//") {
            out.push_str(&text[start..end]);
            continue;
        }
        let name = &text[start + 6..end - 1];
        let path = match name.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None if Path::new(name).is_absolute() => PathBuf::from(name),
            None => directory.join(name),
        };
        let content = std::fs::read_to_string(path).unwrap_or_default();
        let quoted = Value::String(content.trim().to_string()).to_string();
        out.push_str(&quoted[1..quoted.len() - 1]);
    }
    out.push_str(&text[cursor..]);
    out
}

// An invalid file contributes nothing, as in the original loader.
fn read(file: &Path, home: &Path, env: Lookup, warnings: &mut Vec<String>) -> Option<Value> {
    let text = std::fs::read_to_string(file).ok()?;
    let options = ParseOptions { allow_comments: true, allow_trailing_commas: true, ..Default::default() };
    let text = substitute_file(&substitute_env(&text, env), file.parent().unwrap_or(Path::new(".")), home);
    let parsed: Result<Value, _> = jsonc_parser::parse_to_serde_value(&text, &options);
    let mut value = match parsed {
        Ok(value @ Value::Object(_)) => value,
        Ok(_) => {
            warnings.push(format!("{}: expected an object", file.display()));
            return None;
        }
        Err(error) => {
            warnings.push(format!("{}: {error}", file.display()));
            return None;
        }
    };
    // A nested `{"tui": {...}}` is flattened over its siblings.
    if let Some(Value::Object(nested)) = value.as_object_mut().and_then(|map| map.shift_remove("tui")) {
        merge_deep(&mut value, Value::Object(nested));
    }
    Some(value)
}

// Rust build: opencode 2.x keeps its TUI settings in `<config>/cli.json` (`packages/cli/src/config/config.ts`). It is
// built once from tui.json and kv.json (`config/migrate.ts`) and is the only file 2.x reads or writes afterwards. Its
// keybinds (named by command, `TuiKeybind.CommandMap`) and the settings this TUI has are mapped back to tui.json
// names. The theme is left out: this build keeps its own choice. The file is never written.
fn cli_layer(file: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(file).ok()?;
    let options = ParseOptions { allow_comments: true, allow_trailing_commas: true, ..Default::default() };
    let cli: Value = jsonc_parser::parse_to_serde_value(&text, &options).ok()?;
    cli.as_object()?;
    let mut out = Map::new();
    if let Some(keybinds) = cli.get("keybinds").and_then(Value::as_object) {
        let definitions = keybind::definitions();
        let mapped = keybinds.iter().filter_map(|(command, value)| {
            let found = definitions.iter().find(|d| d.command == *command || d.name == *command)?;
            Some((found.name.clone(), value.clone()))
        });
        out.insert("keybinds".into(), Value::Object(mapped.collect()));
    }
    for (pointer, key) in [("/leader/timeout", "leader_timeout"), ("/scroll/speed", "scroll_speed"), ("/mouse", "mouse")] {
        if let Some(value) = cli.pointer(pointer) {
            out.insert(key.into(), value.clone());
        }
    }
    // `diff_style: "stacked"` became `diffs.view: "unified"`; the other views lay out like "auto".
    if let Some(view) = cli.pointer("/diffs/view").and_then(Value::as_str) {
        out.insert("diff_style".into(), (if view == "unified" { "stacked" } else { "auto" }).into());
    }
    Some(Value::Object(out))
}

fn modified(file: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(file).and_then(|m| m.modified()).ok()
}

fn files(paths: &Paths, cwd: &Path, env: Lookup) -> Vec<PathBuf> {
    let mut out = vec![paths.config.join("tui.json"), paths.config.join("tui.jsonc")];
    if let Some(explicit) = env("OPENCODE_TUI_CONFIG").filter(|v| !v.is_empty()) {
        out.push(PathBuf::from(explicit));
    }
    let worktree = config::find_worktree(cwd);
    if !flag::truthy(env, "OPENCODE_DISABLE_PROJECT_CONFIG") {
        let mut chain: Vec<&Path> = Vec::new();
        for dir in cwd.ancestors() {
            chain.push(dir);
            if worktree.as_deref() == Some(dir) {
                break;
            }
        }
        for dir in chain.into_iter().rev() {
            out.push(dir.join("tui.jsonc"));
            out.push(dir.join("tui.json"));
        }
    }
    let opts = LoadOptions { cwd: cwd.to_path_buf(), worktree };
    for dir in config::config_dirs_with(paths, &opts, env).into_iter().skip(1) {
        out.push(dir.join("tui.json"));
        out.push(dir.join("tui.jsonc"));
    }
    out
}

impl TuiConfig {
    pub fn load(paths: &Paths, cwd: &Path) -> Self {
        Self::load_with(paths, cwd, &flag::process_env)
    }

    pub fn load_with(paths: &Paths, cwd: &Path, env: Lookup) -> Self {
        let mut merged = Value::Object(Map::new());
        let mut warnings = Vec::new();
        let files = files(paths, cwd, env);
        // cli.json takes the place of the global tui.json(c), the first two files. Whichever was written last holds
        // the user's latest choices (2.x copied tui.json once and edits only cli.json since), so it is merged above
        // the other; a tie goes to cli.json. Project files and OPENCODE_TUI_CONFIG still override both.
        let cli_file = paths.config.join("cli.json");
        let mut cli = cli_layer(&cli_file);
        let cli_wins = modified(&cli_file) >= files[..2].iter().filter_map(|f| modified(f)).max();
        for (index, file) in files.iter().enumerate() {
            if index == usize::from(cli_wins) * 2
                && let Some(value) = cli.take()
            {
                merge_deep(&mut merged, value);
            }
            if let Some(value) = read(file, &paths.home, env, &mut warnings) {
                merge_deep(&mut merged, value);
            }
        }
        if let Some(value) = cli {
            merge_deep(&mut merged, value);
        }
        let mut out = Self::from_value(&merged);
        warnings.append(&mut out.warnings);
        out.warnings = warnings;
        out
    }

    pub fn from_value(value: &Value) -> Self {
        let positive = |v: Option<&Value>| v.and_then(Value::as_u64).filter(|n| *n > 0);
        let mut warnings = Vec::new();
        let mut keybinds = value.get("keybinds").and_then(Value::as_object).cloned().unwrap_or_default();
        let known: std::collections::HashSet<&str> = keybind::definitions().iter().map(|d| d.name.as_str()).collect();
        keybinds.retain(|key, _| {
            let keep = known.contains(key.as_str());
            if !keep {
                warnings.push(format!("unrecognized keybind {key}"));
            }
            keep
        });
        let max_width = match value.pointer("/prompt/max_width") {
            Some(Value::String(s)) if s == "auto" => MaxWidth::Auto,
            other => MaxWidth::Fixed(positive(other).map_or(75, |n| n.min(u64::from(u16::MAX)) as u16)),
        };
        Self {
            theme: value.get("theme").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string),
            keybinds,
            leader_timeout: positive(value.get("leader_timeout")).unwrap_or(LEADER_TIMEOUT_DEFAULT),
            prompt_max_height: positive(value.pointer("/prompt/max_height")).map(|n| n.min(u64::from(u16::MAX)) as u16),
            prompt_max_width: max_width,
            scroll_speed: value.get("scroll_speed").and_then(Value::as_f64).filter(|n| *n >= 0.001).unwrap_or(3.0),
            diff_style: value.get("diff_style").and_then(Value::as_str).map(str::to_string),
            mouse: value.get("mouse").and_then(Value::as_bool).unwrap_or(true),
            warnings,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(root: &Path) -> Paths {
        Paths {
            home: root.join("home"),
            data: root.join("data"),
            cache: root.join("cache"),
            config: root.join("config"),
            state: root.join("state"),
            tmp: root.join("tmp"),
        }
    }

    #[test]
    fn defaults_match_the_original() {
        let config = TuiConfig::default();
        assert_eq!(config.leader_timeout, 2000);
        assert_eq!(config.prompt_max_width, MaxWidth::Fixed(75));
        assert_eq!(config.scroll_speed, 3.0);
        assert!(config.mouse && config.theme.is_none());
    }

    #[test]
    fn merges_global_then_project_and_drops_unknown_keybinds() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let project = dir.path().join("project");
        std::fs::create_dir_all(&paths.config).unwrap();
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::write(
            paths.config.join("tui.json"),
            r#"{ "theme": "nord", "leader_timeout": 900, "keybinds": { "session_new": "ctrl+n", "nope": "x" } }"#,
        )
        .unwrap();
        std::fs::write(
            project.join("tui.jsonc"),
            "{ // comment\n \"tui\": { \"theme\": \"{env:MY_THEME}\" }, \"mouse\": false, \"prompt\": { \"max_width\": \"auto\" }, }",
        )
        .unwrap();
        let env = |key: &str| (key == "MY_THEME").then(|| "dracula".to_string());
        let config = TuiConfig::load_with(&paths, &project, &env);
        assert_eq!(config.theme.as_deref(), Some("dracula"));
        assert_eq!(config.leader_timeout, 900);
        assert!(!config.mouse);
        assert_eq!(config.prompt_max_width, MaxWidth::Auto);
        assert_eq!(config.keybinds.len(), 1);
        assert_eq!(config.warnings, vec!["unrecognized keybind nope".to_string()]);
    }

    #[test]
    fn substitutes_file_contents() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let project = dir.path().join("project");
        std::fs::create_dir_all(&paths.config).unwrap();
        std::fs::create_dir_all(&paths.home).unwrap();
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::write(paths.config.join("theme.txt"), "  nord\n").unwrap();
        std::fs::write(paths.home.join("leader.txt"), "ctrl+o").unwrap();
        std::fs::write(project.join("quote.txt"), "a\"b\\c\nd").unwrap();
        std::fs::write(
            paths.config.join("tui.jsonc"),
            "{\n  // \"theme\": \"{file:absent.txt}\"\n  \"theme\": \"{file:theme.txt}\",\n  \"keybinds\": { \"leader\": \"{file:~/leader.txt}\" },\n  \"diff_style\": \"x{file:missing.txt}y\"\n}",
        )
        .unwrap();
        let config = TuiConfig::load_with(&paths, &project, &|_| None);
        assert_eq!(config.theme.as_deref(), Some("nord"));
        assert_eq!(config.keybinds["leader"], "ctrl+o");
        assert_eq!(config.diff_style.as_deref(), Some("xy"));
        assert_eq!(substitute_file("\"{file:quote.txt}\" {file:} {file:open", &project, &paths.home), "\"a\\\"b\\\\c\\nd\" {file:} {file:open");
        let absolute = project.join("quote.txt");
        assert_eq!(substitute_file(&format!("{{file:{}}}", absolute.display()), dir.path(), &paths.home), "a\\\"b\\\\c\\nd");
    }

    fn age(file: &Path, seconds: u64) {
        let at = std::time::SystemTime::now() - std::time::Duration::from_secs(seconds);
        std::fs::File::options().append(true).open(file).unwrap().set_modified(at).unwrap();
    }

    #[test]
    fn reads_cli_json_of_opencode_2() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let project = dir.path().join("project");
        std::fs::create_dir_all(&paths.config).unwrap();
        std::fs::create_dir_all(project.join(".git")).unwrap();
        let cli = paths.config.join("cli.json");
        std::fs::write(
            &cli,
            r#"{ "$schema": "https://opencode.ai/v2/cli.json", "theme": { "name": "nord" },
                 "keybinds": { "session.new": "ctrl+n", "leader": "ctrl+o", "app.scrap": "f9", "session.tab.next": "f8" },
                 "leader": { "timeout": 700 }, "scroll": { "speed": 5 }, "mouse": false, "diffs": { "view": "unified" } }"#,
        )
        .unwrap();
        let before = std::fs::read(&cli).unwrap();
        let config = TuiConfig::load_with(&paths, &project, &|_| None);
        assert_eq!(config.keybinds, serde_json::json!({"session_new": "ctrl+n", "leader": "ctrl+o"}).as_object().unwrap().clone());
        assert_eq!((config.leader_timeout, config.scroll_speed, config.mouse), (700, 5.0, false));
        assert_eq!(config.diff_style.as_deref(), Some("stacked"));
        assert_eq!(config.theme, None);
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
        assert_eq!(std::fs::read(&cli).unwrap(), before);

        // The newer of cli.json and the global tui.json wins; project files override both.
        let tui = paths.config.join("tui.json");
        std::fs::write(&tui, r#"{ "leader_timeout": 900, "keybinds": { "session_new": "ctrl+t" } }"#).unwrap();
        age(&tui, 3600);
        let config = TuiConfig::load_with(&paths, &project, &|_| None);
        assert_eq!((config.leader_timeout, config.keybinds["session_new"].as_str()), (700, Some("ctrl+n")));
        age(&cli, 7200);
        let config = TuiConfig::load_with(&paths, &project, &|_| None);
        assert_eq!((config.leader_timeout, config.keybinds["session_new"].as_str()), (900, Some("ctrl+t")));
        assert!(!config.mouse);
        std::fs::write(project.join("tui.json"), r#"{ "leader_timeout": 1200 }"#).unwrap();
        assert_eq!(TuiConfig::load_with(&paths, &project, &|_| None).leader_timeout, 1200);

        std::fs::write(&cli, "{ broken").unwrap();
        let config = TuiConfig::load_with(&paths, &project, &|_| None);
        assert!(config.mouse && config.warnings.is_empty());
    }

    #[test]
    fn invalid_files_contribute_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::create_dir_all(&paths.config).unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(paths.config.join("tui.json"), "{ not json").unwrap();
        let config = TuiConfig::load_with(&paths, dir.path(), &|_| None);
        assert_eq!(config.theme, None);
        assert_eq!(config.warnings.len(), 1);
    }
}
