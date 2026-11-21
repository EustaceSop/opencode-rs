use crate::flag::{self, Lookup};
use crate::glob;
use crate::paths::Paths;
use jsonc_parser::ParseOptions;
use regex::Regex;
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

mod compat;
mod info;
pub mod markdown;
pub mod plugin;

mod update;

pub use compat::{Diagnostic, DiagnosticKind};
pub use info::*;
pub use update::{global_file, parse_jsonc, set_jsonc, update_global, update_project};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("ConfigJsonError: {path}: {message}")]
    Json { path: String, message: String },
    #[error("ConfigInvalidError: {path}: {message}")]
    Invalid { path: String, message: String },
}

pub struct LoadOptions {
    pub cwd: PathBuf,
    pub worktree: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Value,
    pub directories: Vec<PathBuf>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Loaded {
    pub fn info(&self) -> Result<Info, ConfigError> {
        Info::from_value(&self.config)
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

fn substitute_file(text: &str, dir: &Path, home: &Path, source: &str) -> Result<String, ConfigError> {
    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    while let Some(found) = text[pos..].find("{file:") {
        let start = pos + found;
        let body = start + 6;
        let end = match text[body..].find('}') {
            Some(e) if e > 0 => body + e,
            _ => {
                out.push_str(&text[pos..body]);
                pos = body;
                continue;
            }
        };
        let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
        if text[line_start..start].trim().starts_with("//") {
            out.push_str(&text[pos..=end]);
            pos = end + 1;
            continue;
        }
        let raw = &text[body..end];
        let target = match raw.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None if Path::new(raw).is_absolute() => PathBuf::from(raw),
            None => dir.join(raw),
        };
        let content = fs::read_to_string(&target).map_err(|_| ConfigError::Invalid {
            path: source.to_string(),
            message: format!("bad file reference: \"{{file:{raw}}}\" {} does not exist", target.display()),
        })?;
        let escaped = serde_json::to_string(content.trim()).unwrap_or_default();
        out.push_str(&text[pos..start]);
        out.push_str(&escaped[1..escaped.len() - 1]);
        pos = end + 1;
    }
    out.push_str(&text[pos..]);
    Ok(out)
}

fn parse_text_with(text: &str, dir: &Path, home: &Path, source: &str, env: Lookup) -> Result<Value, ConfigError> {
    let text = substitute_file(&substitute_env(text, env), dir, home, source)?;
    let options = ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_loose_object_property_names: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
        ..Default::default()
    };
    let mut value: Value = if text.trim().is_empty() {
        Value::Object(Map::new())
    } else {
        jsonc_parser::parse_to_serde_value(&text, &options)
            .map_err(|e| ConfigError::Json { path: source.to_string(), message: e.to_string() })?
    };
    let Some(map) = value.as_object_mut() else {
        return Err(ConfigError::Invalid { path: source.to_string(), message: "expected an object".into() });
    };
    // These keys moved to tui.json in the original and are stripped on load.
    for key in ["theme", "keybinds", "tui"] {
        map.shift_remove(key);
    }
    Ok(value)
}

pub fn parse_text(text: &str, dir: &Path, home: &Path, source: &str) -> Result<Value, ConfigError> {
    parse_text_with(text, dir, home, source, &flag::process_env)
}

const ACTION_ONLY: [&str; 5] = ["todowrite", "question", "webfetch", "websearch", "doom_loop"];

fn is_action(value: &Value) -> bool {
    matches!(value.as_str(), Some("ask" | "allow" | "deny"))
}

fn validate_permission(value: &Value) -> Result<(), String> {
    if is_action(value) {
        return Ok(());
    }
    let Some(map) = value.as_object() else {
        return Err("permission: expected \"ask\" | \"allow\" | \"deny\" or an object".into());
    };
    for (key, rule) in map {
        let patterns = rule.as_object().filter(|_| !ACTION_ONLY.contains(&key.as_str()));
        if !is_action(rule) && !patterns.is_some_and(|p| p.values().all(is_action)) {
            return Err(format!("permission.{key}: invalid permission rule"));
        }
    }
    Ok(())
}

static COLOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^#[0-9a-fA-F]{6}$").unwrap());
const THEME_COLORS: [&str; 7] = ["primary", "secondary", "accent", "success", "warning", "error", "info"];

fn validate_agent(agent: &Map<String, Value>) -> Result<(), String> {
    let check = |key: &str, ok: &dyn Fn(&Value) -> bool, expected: &str| match agent.get(key) {
        Some(v) if !ok(v) => Err(format!("{key}: expected {expected}")),
        _ => Ok(()),
    };
    for key in ["model", "variant", "prompt", "description"] {
        check(key, &Value::is_string, "string")?;
    }
    for key in ["temperature", "top_p"] {
        check(key, &Value::is_number, "number")?;
    }
    for key in ["disable", "hidden"] {
        check(key, &Value::is_boolean, "boolean")?;
    }
    for key in ["steps", "maxSteps"] {
        check(key, &|v| compat::is_int_at_least(v, 1.0), "positive integer")?;
    }
    check("tools", &|v| v.as_object().is_some_and(|m| m.values().all(Value::is_boolean)), "record of booleans")?;
    check("mode", &|v| matches!(v.as_str(), Some("subagent" | "primary" | "all")), "subagent | primary | all")?;
    check("options", &Value::is_object, "object")?;
    check(
        "color",
        &|v| v.as_str().is_some_and(|s| COLOR.is_match(s) || THEME_COLORS.contains(&s)),
        "hex color or theme color",
    )?;
    agent.get("permission").map_or(Ok(()), validate_permission)
}

const AGENT_KEYS: [&str; 16] = [
    "name",
    "model",
    "variant",
    "prompt",
    "description",
    "temperature",
    "top_p",
    "mode",
    "hidden",
    "color",
    "steps",
    "maxSteps",
    "options",
    "permission",
    "disable",
    "tools",
];

fn tools_to_permission(tools: &Map<String, Value>) -> Map<String, Value> {
    let mut permission = Map::new();
    for (tool, enabled) in tools {
        let action = if enabled.as_bool().unwrap_or(false) { "allow" } else { "deny" };
        let key = if matches!(tool.as_str(), "write" | "edit" | "patch") { "edit" } else { tool };
        permission.insert(key.to_string(), action.into());
    }
    permission
}

fn permission_object(value: Value) -> Value {
    match value {
        Value::String(action) => {
            let mut map = Map::new();
            map.insert("*".into(), Value::String(action));
            Value::Object(map)
        }
        other => other,
    }
}

// core/src/v1/config/agent.ts `normalize`: unknown keys into options, tools into permission, maxSteps into steps.
fn normalize_agent(agent: &mut Map<String, Value>) {
    let mut options = agent.get("options").and_then(Value::as_object).cloned().unwrap_or_default();
    for (key, value) in agent.iter() {
        if !AGENT_KEYS.contains(&key.as_str()) {
            options.insert(key.clone(), value.clone());
        }
    }
    let mut permission = agent.get("tools").and_then(Value::as_object).map(tools_to_permission).unwrap_or_default();
    if let Some(Value::Object(explicit)) = agent.get("permission").cloned().map(permission_object) {
        for (key, value) in explicit {
            permission.insert(key, value);
        }
    }
    let steps = agent.get("steps").or_else(|| agent.get("maxSteps")).cloned();
    agent.insert("options".into(), Value::Object(options));
    agent.insert("permission".into(), Value::Object(permission));
    if let Some(steps) = steps {
        agent.insert("steps".into(), steps);
    }
}

fn validate_command(command: &Map<String, Value>) -> Result<(), String> {
    if !command.get("template").is_some_and(Value::is_string) {
        return Err("template: expected string".into());
    }
    for key in ["description", "agent", "model", "variant"] {
        if command.get(key).is_some_and(|v| !v.is_string()) {
            return Err(format!("{key}: expected string"));
        }
    }
    if command.get("subtask").is_some_and(|v| !v.is_boolean()) {
        return Err("subtask: expected boolean".into());
    }
    Ok(())
}

// Decode-time transforms of ConfigV1.Info that affect merging: permission and agent shapes.
fn normalize(mut value: Value, source: &str) -> Result<Value, ConfigError> {
    let invalid = |message: String| ConfigError::Invalid { path: source.to_string(), message };
    let Some(map) = value.as_object_mut() else { return Ok(value) };
    if let Some(permission) = map.get_mut("permission") {
        validate_permission(permission).map_err(invalid)?;
        *permission = permission_object(permission.take());
    }
    for group in ["agent", "mode"] {
        let Some(agents) = map.get_mut(group) else { continue };
        let Some(agents) = agents.as_object_mut() else {
            return Err(invalid(format!("{group}: expected an object")));
        };
        for (name, agent) in agents.iter_mut() {
            let Some(agent) = agent.as_object_mut() else {
                return Err(invalid(format!("{group}.{name}: expected an object")));
            };
            validate_agent(agent).map_err(|e| invalid(format!("{group}.{name}.{e}")))?;
            normalize_agent(agent);
        }
    }
    if let Some(commands) = map.get("command") {
        let Some(commands) = commands.as_object() else {
            return Err(invalid("command: expected an object".into()));
        };
        for (name, command) in commands {
            let checked = command.as_object().ok_or_else(|| "expected an object".to_string()).and_then(validate_command);
            checked.map_err(|e| invalid(format!("command.{name}.{e}")))?;
        }
    }
    Ok(value)
}

struct Loader<'a> {
    home: &'a Path,
    env: Lookup<'a>,
    diagnostics: Vec<Diagnostic>,
    // Every plugin spec declared so far, de-duplicated; `result.plugin` is rebuilt from it after each layer.
    plugins: Vec<Value>,
}

impl Loader<'_> {
    // `merge` + `mergePluginOrigins` (`config.ts:344-368`).
    fn merge(&mut self, result: &mut Value, next: Value) {
        let declared: Vec<Value> = next.get("plugin").and_then(Value::as_array).cloned().unwrap_or_default();
        merge_concat(result, next);
        if declared.is_empty() {
            return;
        }
        self.plugins = plugin::dedupe(std::mem::take(&mut self.plugins).into_iter().chain(declared).collect());
        result["plugin"] = Value::Array(self.plugins.clone());
    }

    fn text(&mut self, text: &str, dir: &Path, source: &str) -> Result<Value, ConfigError> {
        let parsed = parse_text_with(text, dir, self.home, source, self.env)?;
        normalize(compat::lower(parsed, source, &mut self.diagnostics)?, source)
    }

    fn file(&mut self, file: &Path) -> Result<Value, ConfigError> {
        let mut value = match fs::read_to_string(file) {
            Ok(text) => self.text(&text, file.parent().unwrap_or(Path::new(".")), &file.to_string_lossy())?,
            Err(_) => return Ok(Value::Object(Map::new())),
        };
        // Path-like plugin specs are pinned to the file that declared them.
        if let Some(list) = value.get_mut("plugin").and_then(Value::as_array_mut) {
            list.iter_mut().for_each(|spec| plugin::resolve(spec, file));
        }
        Ok(value)
    }

    fn global(&mut self, config: &Path) -> Result<Value, ConfigError> {
        let mut result = Value::Object(Map::new());
        for name in ["config.json", "opencode.json", "opencode.jsonc"] {
            merge_deep(&mut result, self.file(&config.join(name))?);
        }
        Ok(result)
    }

    // The Rust-only global dir's `opencode.json[c]`; like the global config, a broken file is reported and skipped.
    fn rust_global(&mut self, dir: &Path) -> Value {
        let mut result = object();
        for name in ["opencode.json", "opencode.jsonc"] {
            match self.file(&dir.join(name)) {
                Ok(value) => merge_deep(&mut result, value),
                Err(error) => self.diagnostics.push(Diagnostic {
                    kind: DiagnosticKind::Invalid,
                    source: dir.join(name).to_string_lossy().into_owned(),
                    path: Vec::new(),
                    message: format!("failed to load Rust-only global config, skipping: {error}"),
                }),
            }
        }
        result
    }
}

// Recurse only when both sides are objects; arrays and scalars are replaced.
pub fn merge_deep(target: &mut Value, source: Value) {
    match (target, source) {
        (Value::Object(t), Value::Object(s)) => {
            for (k, v) in s {
                match t.get_mut(&k) {
                    Some(slot) => merge_deep(slot, v),
                    None => {
                        t.insert(k, v);
                    }
                }
            }
        }
        (slot, v) => *slot = v,
    }
}

fn merge_concat(target: &mut Value, source: Value) {
    let union = match (target.get("instructions"), source.get("instructions")) {
        (Some(Value::Array(a)), Some(Value::Array(b))) => {
            let mut all: Vec<Value> = Vec::new();
            for item in a.iter().chain(b) {
                if !all.contains(item) {
                    all.push(item.clone());
                }
            }
            Some(all)
        }
        _ => None,
    };
    merge_deep(target, source);
    if let Some(all) = union {
        target["instructions"] = Value::Array(all);
    }
}

// Directories from `cwd` up to `stop` (inclusive); to the filesystem root when there is no worktree.
pub(crate) fn up(cwd: &Path, stop: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut current = Some(cwd);
    while let Some(dir) = current {
        dirs.push(dir.to_path_buf());
        if Some(dir) == stop {
            break;
        }
        current = dir.parent();
    }
    dirs
}

pub fn find_worktree(cwd: &Path) -> Option<PathBuf> {
    up(cwd, None).into_iter().find(|d| d.join(".git").exists())
}

fn managed_dir_with(env: Lookup) -> PathBuf {
    if let Some(dir) = env("OPENCODE_TEST_MANAGED_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/opencode")
    } else if cfg!(windows) {
        let base = env("ProgramData").filter(|v| !v.is_empty()).unwrap_or_else(|| "C:\\ProgramData".into());
        PathBuf::from(base).join("opencode")
    } else {
        PathBuf::from("/etc/opencode")
    }
}

pub fn managed_dir() -> PathBuf {
    managed_dir_with(&flag::process_env)
}

// Global config dir, the Rust-only global dir when it exists, `.opencode` dirs nearest first, `~/.opencode`, then
// OPENCODE_CONFIG_DIR.
pub fn config_dirs_with(paths: &Paths, opts: &LoadOptions, env: Lookup) -> Vec<PathBuf> {
    let mut dirs = vec![paths.config.clone()];
    let rust = paths.rust_config();
    if rust.is_dir() {
        dirs.push(rust);
    }
    if !flag::truthy(env, "OPENCODE_DISABLE_PROJECT_CONFIG") {
        dirs.extend(
            up(&opts.cwd, opts.worktree.as_deref()).into_iter().map(|d| d.join(".opencode")).filter(|d| d.exists()),
        );
    }
    let home = paths.home.join(".opencode");
    if home.exists() {
        dirs.push(home);
    }
    if let Some(dir) = env("OPENCODE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        dirs.push(PathBuf::from(dir));
    }
    let mut unique: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        if !unique.contains(&dir) {
            unique.push(dir);
        }
    }
    unique
}

pub fn config_dirs(paths: &Paths, opts: &LoadOptions) -> Vec<PathBuf> {
    config_dirs_with(paths, opts, &flag::process_env)
}

pub fn load_global(paths: &Paths) -> Result<Value, ConfigError> {
    Loader { home: &paths.home, env: &flag::process_env, diagnostics: Vec::new(), plugins: Vec::new() }.global(&paths.config)
}

// Node `path.extname` on the last segment, then strip it.
fn strip_extension(candidate: &str) -> &str {
    let base_start = candidate.rfind('/').map_or(0, |i| i + 1);
    match candidate[base_start..].rfind('.') {
        Some(dot) if dot > 0 => &candidate[..base_start + dot],
        _ => candidate,
    }
}

// Entry name: path relative to the scanned dir, known prefix stripped, extension removed.
pub fn entry_name(relative: &str, prefixes: &[&str]) -> String {
    let normalized = relative.replace('\\', "/");
    let candidate = prefixes
        .iter()
        .find_map(|prefix| normalized.strip_prefix(prefix))
        .unwrap_or_else(|| normalized.rsplit('/').next().unwrap_or(&normalized));
    strip_extension(candidate).to_string()
}

fn name_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

struct Entry {
    file: PathBuf,
    name: String,
    fields: Map<String, Value>,
}

// `{name, ...frontmatter, [body_key]: body.trim()}` for every parseable markdown file matching `pattern`.
fn markdown_entries(dir: &Path, pattern: &str, prefixes: &[&str], body_key: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    for file in glob::scan(pattern, dir, glob::Options { dot: true, follow: true }) {
        let Ok(text) = fs::read_to_string(&file) else { continue };
        let Ok(md) = markdown::parse(&text) else { continue };
        let relative = file.strip_prefix(dir).unwrap_or(&file).to_string_lossy().into_owned();
        let mut fields = Map::new();
        fields.insert("name".into(), Value::String(entry_name(&relative, prefixes)));
        for (key, value) in md.data {
            fields.insert(key, value);
        }
        fields.insert(body_key.into(), Value::String(md.content.trim().to_string()));
        let name = name_string(&fields["name"]);
        fields.insert("name".into(), Value::String(name.clone()));
        out.push(Entry { file, name, fields });
    }
    out
}

fn scan_commands(dir: &Path) -> Result<Map<String, Value>, ConfigError> {
    let mut result = Map::new();
    for entry in markdown_entries(dir, "{command,commands}/**/*.md", &["command/", "commands/"], "template") {
        validate_command(&entry.fields)
            .map_err(|message| ConfigError::Invalid { path: entry.file.to_string_lossy().into_owned(), message })?;
        let known = ["template", "description", "agent", "model", "variant", "subtask"];
        let fields: Map<String, Value> =
            entry.fields.into_iter().filter(|(key, _)| known.contains(&key.as_str())).collect();
        result.insert(entry.name, Value::Object(fields));
    }
    Ok(result)
}

// Rust build: agent files written for v2 may carry `permissions` rules; they become `permission` as in JSON config,
// and malformed ones are dropped like v2 ignores them.
fn lower_markdown_permissions(fields: &mut Map<String, Value>) {
    if let Some(rules) = fields.shift_remove("permissions")
        && let Some(permission) = compat::fold_permissions(fields.get("permission"), &rules)
    {
        fields.insert("permission".into(), permission);
    }
}

fn scan_agents(dir: &Path) -> Result<Map<String, Value>, ConfigError> {
    let mut result = Map::new();
    for mut entry in markdown_entries(dir, "{agent,agents}/**/*.md", &["agent/", "agents/"], "prompt") {
        lower_markdown_permissions(&mut entry.fields);
        validate_agent(&entry.fields)
            .map_err(|message| ConfigError::Invalid { path: entry.file.to_string_lossy().into_owned(), message })?;
        normalize_agent(&mut entry.fields);
        result.insert(entry.name, Value::Object(entry.fields));
    }
    Ok(result)
}

// Invalid mode files are dropped silently; valid ones are forced to `mode: "primary"`.
fn scan_modes(dir: &Path) -> Map<String, Value> {
    let mut result = Map::new();
    for mut entry in markdown_entries(dir, "{mode,modes}/*.md", &["mode/", "modes/"], "prompt") {
        lower_markdown_permissions(&mut entry.fields);
        if validate_agent(&entry.fields).is_err() {
            continue;
        }
        normalize_agent(&mut entry.fields);
        entry.fields.insert("mode".into(), "primary".into());
        result.insert(entry.name, Value::Object(entry.fields));
    }
    result
}

fn slot<'a>(result: &'a mut Value, key: &str, default: Value) -> &'a mut Value {
    let map = result.as_object_mut().expect("config root is an object");
    if !map.contains_key(key) || map[key].is_null() {
        map.insert(key.to_string(), default);
    }
    &mut map[key]
}

fn object() -> Value {
    Value::Object(Map::new())
}

fn post_process(result: &mut Value, env: Lookup, diagnostics: &mut Vec<Diagnostic>) {
    let modes = result.get("mode").and_then(Value::as_object).cloned().unwrap_or_default();
    for (name, mode) in modes {
        let Value::Object(mut mode) = mode else { continue };
        mode.insert("mode".into(), "primary".into());
        let mut patch = Map::new();
        patch.insert(name, Value::Object(mode));
        merge_deep(slot(result, "agent", object()), Value::Object(patch));
    }

    if let Some(raw) = env("OPENCODE_PERMISSION").filter(|v| !v.is_empty()) {
        match serde_json::from_str::<Value>(&raw) {
            Ok(Value::Object(extra)) => merge_deep(slot(result, "permission", object()), Value::Object(extra)),
            _ => diagnostics.push(Diagnostic {
                kind: DiagnosticKind::Invalid,
                source: "OPENCODE_PERMISSION".into(),
                path: Vec::new(),
                message: "OPENCODE_PERMISSION is not a JSON object, skipping".into(),
            }),
        }
    }

    if let Some(tools) = result.get("tools").and_then(Value::as_object) {
        let mut permission = Value::Object(tools_to_permission(tools));
        merge_deep(&mut permission, result.get("permission").cloned().unwrap_or_else(object));
        result["permission"] = permission;
    }

    if !result.get("username").and_then(Value::as_str).is_some_and(|u| !u.is_empty()) {
        let user = ["USERNAME", "USER", "LOGNAME"].iter().find_map(|key| env(key).filter(|v| !v.is_empty()));
        result["username"] = Value::String(user.unwrap_or_else(|| "user".into()));
    }

    let share_unset = match result.get("share") {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        Some(_) => false,
    };
    if result.get("autoshare") == Some(&Value::Bool(true)) && share_unset {
        result["share"] = "auto".into();
    }

    for (var, key) in [("OPENCODE_DISABLE_AUTOCOMPACT", "auto"), ("OPENCODE_DISABLE_PRUNE", "prune")] {
        if flag::truthy(env, var) {
            let compaction = slot(result, "compaction", object());
            if !compaction.is_object() {
                *compaction = object();
            }
            compaction[key] = Value::Bool(false);
        }
    }
}

// A config fetched from elsewhere: the active console org's `<url>/api/config` (`config.ts:492-528`).
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteConfig {
    // `<url>/api/config`, the source label of diagnostics and errors.
    pub source: String,
    pub config: Map<String, Value>,
}

// Precedence low to high: global, OPENCODE_CONFIG, project files (root to cwd), config dirs
// (json files, then markdown scans), OPENCODE_CONFIG_CONTENT, managed dir; then post-processing.
pub fn load_with(paths: &Paths, opts: &LoadOptions, env: Lookup) -> Result<Loaded, ConfigError> {
    load_with_remote(paths, opts, env, None)
}

// `load_with` plus the console org config, merged after OPENCODE_CONFIG_CONTENT and before the managed dir.
// A remote config that does not parse is skipped, as the original only logs the failure.
pub fn load_with_remote(paths: &Paths, opts: &LoadOptions, env: Lookup, remote: Option<&RemoteConfig>) -> Result<Loaded, ConfigError> {
    let mut loader = Loader { home: &paths.home, env, diagnostics: Vec::new(), plugins: Vec::new() };
    // A broken global config is reported and replaced by defaults, as in the original.
    let global = match loader.global(&paths.config) {
        Ok(value) => value,
        Err(error) => {
            loader.diagnostics.push(Diagnostic {
                kind: DiagnosticKind::Invalid,
                source: paths.config.to_string_lossy().into_owned(),
                path: Vec::new(),
                message: format!("failed to load global config, using defaults: {error}"),
            });
            object()
        }
    };
    let mut result = object();
    loader.merge(&mut result, global);
    let rust = loader.rust_global(&paths.rust_config());
    loader.merge(&mut result, rust);
    if let Some(file) = env("OPENCODE_CONFIG").filter(|v| !v.is_empty()) {
        let next = loader.file(Path::new(&file))?;
        loader.merge(&mut result, next);
    }
    if !flag::truthy(env, "OPENCODE_DISABLE_PROJECT_CONFIG") {
        for dir in up(&opts.cwd, opts.worktree.as_deref()).iter().rev() {
            for name in ["opencode.json", "opencode.jsonc"] {
                let next = loader.file(&dir.join(name))?;
                loader.merge(&mut result, next);
            }
        }
    }

    let explicit = env("OPENCODE_CONFIG_DIR").filter(|v| !v.is_empty()).map(PathBuf::from);
    let directories = config_dirs_with(paths, opts, env);
    for dir in &directories {
        if dir.to_string_lossy().ends_with(".opencode") || Some(dir) == explicit.as_ref() {
            for name in ["opencode.json", "opencode.jsonc"] {
                let next = loader.file(&dir.join(name))?;
                loader.merge(&mut result, next);
            }
        }
        merge_deep(slot(&mut result, "command", object()), Value::Object(scan_commands(dir)?));
        merge_deep(slot(&mut result, "agent", object()), Value::Object(scan_agents(dir)?));
        merge_deep(slot(&mut result, "agent", object()), Value::Object(scan_modes(dir)));
    }

    if let Some(content) = env("OPENCODE_CONFIG_CONTENT").filter(|v| !v.is_empty()) {
        let next = loader.text(&content, &opts.cwd, "OPENCODE_CONFIG_CONTENT")?;
        loader.merge(&mut result, next);
    }
    if let Some(remote) = remote {
        // `path.dirname(source)` is the base of relative `{file:}` references, as in the original.
        let dir = Path::new(&remote.source).parent().unwrap_or(Path::new(".")).to_path_buf();
        match loader.text(&Value::Object(remote.config.clone()).to_string(), &dir, &remote.source) {
            Ok(next) => loader.merge(&mut result, next),
            Err(error) => loader.diagnostics.push(Diagnostic {
                kind: DiagnosticKind::Invalid,
                source: remote.source.clone(),
                path: Vec::new(),
                message: format!("failed to fetch remote account config: {error}"),
            }),
        }
    }
    let managed = managed_dir_with(env);
    if managed.exists() {
        for name in ["opencode.json", "opencode.jsonc"] {
            let next = loader.file(&managed.join(name))?;
            loader.merge(&mut result, next);
        }
    }

    slot(&mut result, "agent", object());
    slot(&mut result, "mode", object());
    slot(&mut result, "plugin", Value::Array(Vec::new()));
    post_process(&mut result, env, &mut loader.diagnostics);
    Ok(Loaded { config: result, directories, diagnostics: loader.diagnostics })
}

pub fn load(paths: &Paths, opts: &LoadOptions) -> Result<Value, ConfigError> {
    load_with(paths, opts, &flag::process_env).map(|loaded| loaded.config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

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

    struct Fixture {
        _root: tempfile::TempDir,
        paths: Paths,
        work: PathBuf,
        env: HashMap<String, String>,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let paths = paths(root.path());
            let work = root.path().join("repo");
            fs::create_dir_all(&paths.config).unwrap();
            fs::create_dir_all(work.join(".git")).unwrap();
            let mut env = HashMap::new();
            let managed = root.path().join("managed").to_string_lossy().into_owned();
            env.insert("OPENCODE_TEST_MANAGED_CONFIG_DIR".to_string(), managed);
            env.insert("USERNAME".to_string(), "tester".to_string());
            Self { _root: root, paths, work, env }
        }

        fn write(&self, file: PathBuf, text: &str) {
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, text).unwrap();
        }

        fn set(&mut self, key: &str, value: &str) {
            self.env.insert(key.to_string(), value.to_string());
        }

        fn load_from(&self, cwd: &Path) -> Result<Loaded, ConfigError> {
            let opts = LoadOptions { worktree: find_worktree(cwd), cwd: cwd.to_path_buf() };
            load_with(&self.paths, &opts, &|key: &str| self.env.get(key).cloned())
        }

        fn load(&self) -> Loaded {
            self.load_from(&self.work).unwrap()
        }
    }

    #[test]
    fn jsonc_substitution_and_strip() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("key.txt"), "  se\"cret\n").unwrap();
        let env = |key: &str| (key == "MY_VAR").then(|| "val".to_string());
        let text = "{\n // {file:nope.txt}\n \"theme\": \"x\", \"a\": \"{file:key.txt}\", \"b\": [1,2,], \"c\": \"{env:MY_VAR}{env:NOPE}\",\n}";
        let v = parse_text_with(text, dir.path(), dir.path(), "t", &env).unwrap();
        assert_eq!(v, json!({"a": "se\"cret", "b": [1, 2], "c": "val"}));
        assert!(parse_text_with("{\"a\": \"{file:missing}\"}", dir.path(), dir.path(), "t", &env).is_err());
        assert!(parse_text_with("{a: 1}", dir.path(), dir.path(), "t", &env).is_err());
    }

    #[test]
    fn precedence_and_merge() {
        let f = Fixture::new();
        let cwd = f.work.join("pkg");
        f.write(
            f.paths.config.join("opencode.json"),
            r#"{"model":"g","instructions":["a"],"provider":{"x":{"k":1}}}"#,
        );
        f.write(f.paths.config.join("opencode.jsonc"), r#"{"instructions":["z"]}"#);
        f.write(
            f.work.join("opencode.json"),
            r#"{"model":"root","instructions":["b","z"],"provider":{"x":{"j":2}}}"#,
        );
        f.write(cwd.join("opencode.jsonc"), r#"{"model":"cwd"}"#);
        f.write(cwd.join(".opencode/opencode.json"), r#"{"model":"dot"}"#);
        let loaded = f.load_from(&cwd).unwrap();
        let v = &loaded.config;
        assert_eq!(v["model"], "dot");
        assert_eq!(v["instructions"], json!(["z", "b"]));
        assert_eq!(v["provider"]["x"], json!({"k": 1, "j": 2}));
        assert_eq!(loaded.directories, [f.paths.config.clone(), cwd.join(".opencode")]);
        assert_eq!(v["username"], "tester");
        assert_eq!(v["agent"], json!({}));
        assert_eq!(v["plugin"], json!([]));
    }

    #[test]
    fn plugins_accumulate_across_layers() {
        let mut f = Fixture::new();
        let cwd = f.work.join("pkg");
        f.write(f.paths.config.join("opencode.json"), r#"{"plugin":["global-old"]}"#);
        f.write(f.paths.config.join("opencode.jsonc"), r#"{"plugin":["global-a@1.0.0","shared@1.0.0"]}"#);
        f.write(f.work.join("opencode.json"), r#"{"plugin":["./local.ts",["shared@2.0.0",{"k":1}]]}"#);
        f.write(f.work.join("local.ts"), "");
        f.write(cwd.join("opencode.json"), r#"{"plugin":[]}"#);
        f.write(cwd.join(".opencode/opencode.json"), r#"{"plugin":["@scope/dot","global-a"]}"#);
        f.set("OPENCODE_CONFIG_CONTENT", r#"{"plugin":["./inline.ts"]}"#);
        let v = f.load_from(&cwd).unwrap().config;
        // The three global files replace each other's list; every later layer adds to it, last declaration wins.
        assert_eq!(
            v["plugin"],
            json!([plugin::file_url(&f.work.join("local.ts")), ["shared@2.0.0", {"k": 1}], "@scope/dot", "global-a", "./inline.ts"])
        );
    }

    #[test]
    fn env_sources_and_managed_override() {
        let mut f = Fixture::new();
        let custom = f.work.join("custom.json");
        let extra = f.work.join("extra-dir");
        f.write(custom.clone(), r#"{"model":"custom","small_model":"custom-small"}"#);
        f.write(f.work.join("opencode.json"), r#"{"model":"project","share":"manual"}"#);
        f.write(extra.join("opencode.json"), r#"{"model":"dir","default_agent":"plan"}"#);
        f.write(extra.join("command/hello.md"), "say hello");
        f.set("OPENCODE_CONFIG", &custom.to_string_lossy());
        f.set("OPENCODE_CONFIG_DIR", &extra.to_string_lossy());
        f.set("OPENCODE_CONFIG_CONTENT", r#"{"model":"inline"}"#);
        let v = f.load().config;
        assert_eq!(v["model"], "inline");
        assert_eq!(v["small_model"], "custom-small");
        assert_eq!(v["default_agent"], "plan");
        assert_eq!(v["command"]["hello"], json!({"template": "say hello"}));

        f.write(PathBuf::from(&f.env["OPENCODE_TEST_MANAGED_CONFIG_DIR"]).join("opencode.json"), r#"{"model":"managed"}"#);
        assert_eq!(f.load().config["model"], "managed");

        f.set("OPENCODE_DISABLE_PROJECT_CONFIG", "TRUE");
        f.set("OPENCODE_CONFIG_CONTENT", "");
        f.write(f.work.join(".opencode/opencode.json"), r#"{"share":"disabled"}"#);
        let loaded = f.load();
        assert!(loaded.config.get("share").is_none());
        assert_eq!(loaded.directories, [f.paths.config.clone(), extra]);
    }

    #[test]
    fn console_org_config_sits_between_inline_content_and_managed() {
        let mut f = Fixture::new();
        f.write(f.work.join("opencode.json"), r#"{"model":"project","small_model":"project-small"}"#);
        f.set("OPENCODE_CONFIG_CONTENT", r#"{"model":"inline","share":"manual"}"#);
        let remote = RemoteConfig {
            source: "https://console.test/api/config".into(),
            config: serde_json::from_str(r#"{"model":"remote","plugin":["remote-plugin"],"instructions":["r.md"]}"#).unwrap(),
        };
        let opts = LoadOptions { worktree: find_worktree(&f.work), cwd: f.work.clone() };
        let env = |key: &str| f.env.get(key).cloned();
        let loaded = load_with_remote(&f.paths, &opts, &env, Some(&remote)).unwrap();
        assert_eq!((&loaded.config["model"], &loaded.config["share"], &loaded.config["small_model"]), (&json!("remote"), &json!("manual"), &json!("project-small")));
        assert_eq!(loaded.config["plugin"], json!(["remote-plugin"]));
        assert_eq!(load_with(&f.paths, &opts, &env).unwrap().config["model"], "inline");

        f.write(PathBuf::from(&f.env["OPENCODE_TEST_MANAGED_CONFIG_DIR"]).join("opencode.json"), r#"{"model":"managed"}"#);
        let env = |key: &str| f.env.get(key).cloned();
        assert_eq!(load_with_remote(&f.paths, &opts, &env, Some(&remote)).unwrap().config["model"], "managed");

        // A remote config that cannot be loaded is left out.
        let broken = RemoteConfig { source: remote.source.clone(), config: serde_json::from_str(r#"{"permission":{"webfetch":{"*":"allow"}}}"#).unwrap() };
        let loaded = load_with_remote(&f.paths, &opts, &env, Some(&broken)).unwrap();
        assert!(loaded.diagnostics.iter().any(|d| d.source == remote.source));
    }

    #[test]
    fn rust_only_dir_merges_after_global_and_is_scanned() {
        let f = Fixture::new();
        let rust = f.paths.rust_config();
        assert_eq!(rust, f.paths.config.with_file_name("opencode-rs"));
        assert_eq!(f.load().directories, vec![f.paths.config.clone()]);
        f.write(f.paths.config.join("opencode.json"), r#"{"model":"g","small_model":"g-small","mcp":{"a":{"type":"remote","url":"https://a"}}}"#);
        f.write(rust.join("opencode.jsonc"), "// imported\n{\"model\":\"rs\",\"mcp\":{\"b\":{\"type\":\"local\",\"command\":[\"x\"]}}}");
        f.write(rust.join("agents/helper.md"), "---\ndescription: d\nmode: subagent\n---\nHelp.");
        f.write(f.work.join("opencode.json"), r#"{"small_model":"project-small"}"#);
        let loaded = f.load();
        assert_eq!(loaded.directories, [f.paths.config.clone(), rust.clone()]);
        assert_eq!((&loaded.config["model"], &loaded.config["small_model"]), (&json!("rs"), &json!("project-small")));
        assert_eq!(loaded.config["mcp"].as_object().unwrap().keys().collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(loaded.config["agent"]["helper"]["prompt"], "Help.");

        f.write(rust.join("opencode.jsonc"), "{ broken");
        let loaded = f.load();
        assert_eq!(loaded.config["model"], "g");
        assert!(loaded.diagnostics.iter().any(|d| d.source.contains("opencode-rs")));
    }

    #[test]
    fn farther_dot_opencode_overrides_nearer() {
        let f = Fixture::new();
        let cwd = f.work.join("pkg");
        f.write(cwd.join(".opencode/opencode.json"), r#"{"model":"near"}"#);
        f.write(f.work.join(".opencode/opencode.json"), r#"{"model":"far"}"#);
        f.write(f.paths.home.join(".opencode/opencode.json"), r#"{"model":"home","shell":"zsh"}"#);
        let loaded = f.load_from(&cwd).unwrap();
        assert_eq!(loaded.config["model"], "home");
        assert_eq!(
            loaded.directories,
            [f.paths.config.clone(), cwd.join(".opencode"), f.work.join(".opencode"), f.paths.home.join(".opencode")]
        );
    }

    #[test]
    fn markdown_commands_agents_modes() {
        let f = Fixture::new();
        let dot = f.work.join(".opencode");
        f.write(f.paths.config.join("command/global.md"), "---\ndescription: g\n---\nglobal body\n");
        f.write(dot.join("commands/nested/deploy.md"), "---\ndescription: Ship it\nagent: build\nsubtask: true\nextra: 1\n---\n\nDeploy $1 now\n");
        f.write(dot.join("command/named.md"), "---\nname: renamed\n---\nbody");
        f.write(dot.join("command/broken.md"), "---\na: [1\n---\nbody");
        f.write(
            dot.join("agents/team/rev.md"),
            "---\ndescription: Reviews: carefully\nmode: subagent\ntools:\n  write: false\n  bash: true\npermission:\n  bash: ask\nmaxSteps: 7\ncustom: x\n---\nYou review.\n",
        );
        f.write(dot.join("mode/fast.md"), "---\nmodel: a/b\nmode: subagent\n---\nGo fast");
        f.write(dot.join("modes/bad.md"), "---\ntemperature: hot\n---\nnope");
        f.write(dot.join("modes/deep/skipped.md"), "not scanned");
        f.write(f.work.join("opencode.json"), r#"{"command":{"global":{"template":"json wins nothing"}}}"#);
        let v = f.load().config;
        assert_eq!(v["command"]["global"], json!({"template": "global body", "description": "g"}));
        assert_eq!(
            v["command"]["nested/deploy"],
            json!({"description": "Ship it", "agent": "build", "subtask": true, "template": "Deploy $1 now"})
        );
        assert_eq!(v["command"]["renamed"], json!({"template": "body"}));
        assert!(v["command"].get("broken").is_none() && v["command"].get("named").is_none());
        assert_eq!(
            v["agent"]["team/rev"],
            json!({
                "name": "team/rev", "description": "Reviews: carefully", "mode": "subagent",
                "tools": {"write": false, "bash": true}, "permission": {"edit": "deny", "bash": "ask"},
                "maxSteps": 7, "custom": "x", "prompt": "You review.", "options": {"custom": "x"}, "steps": 7
            })
        );
        let rev = v["agent"]["team/rev"]["permission"].as_object().unwrap();
        assert_eq!(rev.keys().collect::<Vec<_>>(), ["edit", "bash"]);
        assert_eq!(v["agent"]["fast"]["mode"], "primary");
        assert_eq!(v["agent"]["fast"]["prompt"], "Go fast");
        assert!(v["agent"].get("bad").is_none() && v["agent"].get("skipped").is_none());
    }

    #[test]
    fn invalid_markdown_is_fatal_for_commands_and_agents() {
        let f = Fixture::new();
        f.write(f.work.join(".opencode/agent/bad.md"), "---\nmode: nope\n---\nx");
        assert!(matches!(f.load_from(&f.work), Err(ConfigError::Invalid { .. })));
        let f = Fixture::new();
        f.write(f.work.join(".opencode/command/bad.md"), "---\ndescription: 5\n---\nx");
        let err = f.load_from(&f.work).unwrap_err().to_string();
        assert!(err.contains("bad.md") && err.contains("description"));
    }

    #[test]
    fn entry_names() {
        assert_eq!(entry_name("agents\\team\\rev.md", &["agent/", "agents/"]), "team/rev");
        assert_eq!(entry_name("agent/a.b.md", &["agent/", "agents/"]), "a.b");
        assert_eq!(entry_name("other/x.md", &["agent/", "agents/"]), "x");
        assert_eq!(entry_name("command/.hidden", &["command/"]), ".hidden");
        assert_eq!(entry_name("command/noext", &["command/"]), "noext");
    }

    #[test]
    fn post_processing() {
        let mut f = Fixture::new();
        f.write(
            f.work.join("opencode.json"),
            r#"{
              "mode": {"build": {"model": "m/1"}, "custom": {"prompt": "p", "mode": "subagent"}},
              "agent": {"build": {"temperature": 0.1}, "x": {"permission": "deny", "tools": {"patch": true}}},
              "tools": {"write": false, "bash": true, "webfetch": false},
              "permission": {"bash": {"git *": "allow"}, "read": "ask"},
              "autoshare": true,
              "compaction": {"reserved": 5}
            }"#,
        );
        f.set("OPENCODE_PERMISSION", r#"{"read":"deny","glob":"allow"}"#);
        f.set("OPENCODE_DISABLE_PRUNE", "1");
        let loaded = f.load();
        let v = &loaded.config;
        assert_eq!(v["agent"]["build"]["model"], "m/1");
        assert_eq!(v["agent"]["build"]["temperature"], 0.1);
        assert_eq!(v["agent"]["build"]["mode"], "primary");
        assert_eq!(v["agent"]["custom"]["mode"], "primary");
        assert_eq!(v["agent"]["x"]["permission"], json!({"edit": "allow", "*": "deny"}));
        let permission = v["permission"].as_object().unwrap();
        assert_eq!(permission.keys().collect::<Vec<_>>(), ["edit", "bash", "webfetch", "read", "glob"]);
        assert_eq!(v["permission"]["bash"], json!({"git *": "allow"}));
        assert_eq!(v["permission"]["edit"], "deny");
        assert_eq!(v["permission"]["read"], "deny");
        assert_eq!(v["share"], "auto");
        assert_eq!(v["compaction"], json!({"reserved": 5, "prune": false}));
        assert!(loaded.diagnostics.is_empty());

        f.set("OPENCODE_PERMISSION", "{not json");
        f.set("OPENCODE_DISABLE_AUTOCOMPACT", "true");
        let loaded = f.load();
        assert_eq!(loaded.config["permission"]["read"], "ask");
        assert_eq!(loaded.config["compaction"]["auto"], false);
        assert_eq!(loaded.diagnostics.len(), 1);
    }

    #[test]
    fn bare_permission_action_merges_as_wildcard() {
        let f = Fixture::new();
        f.write(f.paths.config.join("opencode.json"), r#"{"permission":"allow","username":"me"}"#);
        f.write(f.work.join("opencode.json"), r#"{"permission":{"bash":"ask"}}"#);
        let v = f.load().config;
        assert_eq!(v["permission"], json!({"*": "allow", "bash": "ask"}));
        assert_eq!(v["username"], "me");
        assert_eq!(v["mode"], json!({}));
    }

    #[test]
    fn invalid_project_config_is_fatal_but_global_falls_back() {
        let f = Fixture::new();
        f.write(f.paths.config.join("opencode.json"), "{ nope");
        let loaded = f.load();
        assert_eq!(loaded.diagnostics.len(), 1);
        f.write(f.work.join("opencode.json"), r#"{"permission":{"webfetch":{"*":"allow"}}}"#);
        assert!(matches!(f.load_from(&f.work), Err(ConfigError::Invalid { .. })));
        f.write(f.work.join("opencode.json"), "{,}");
        assert!(matches!(f.load_from(&f.work), Err(ConfigError::Json { .. })));
    }

    #[test]
    fn config_written_for_v2_loads() {
        let f = Fixture::new();
        f.write(
            f.paths.config.join("opencode.json"),
            r#"{"permission":{"read":"ask"},"plugins":["some-plugin",{"package":"other","options":{"a":1}}],
                "update":{"channel":"latest"},"policies":[{"effect":"deny","action":"provider.use","resource":"x"}],
                "username":"me","timezone":"Europe/Berlin","worktree":{"directory":"~/wt"},
                "providers":{"acme":{"name":"Acme"}},"websearch":{"provider":"exa"},
                "mcp":{"web":{"type":"remote","url":"https://m/mcp","protocol":"streamable-http","pool":{"size":2},
                               "oauth":{"client_id":"id","auth_server_metadata_url":"https://m/.well-known"}},
                       "flat":{"type":"remote","url":"https://n/mcp","enabled":true,"protocol":"sse","oauth":{"auth_server_metadata_url":"https://n/x"}}}}"#,
        );
        f.write(f.work.join("opencode.json"), r#"{"permissions":[{"action":"shell","resource":"*","effect":"deny"}]}"#);
        f.write(
            f.work.join(".opencode/agents/rev.md"),
            "---
description: Reviews
mode: subagent
permissions:
  - action: edit
    resource: \"*\"
    effect: deny
  - action: subagent
    resource: \"*\"
    effect: allow
---
Review.",
        );
        f.write(f.work.join(".opencode/agents/odd.md"), "---
description: Odd
permissions: nope
---
Odd.");
        let loaded = f.load();
        let info = loaded.info().unwrap();
        assert_eq!(loaded.config["permission"], json!({"read": "ask", "bash": "deny"}));
        assert_eq!(info.username.as_deref(), Some("me"));
        assert!(info.mcp["web"].enabled() && info.mcp["flat"].enabled());
        assert_eq!(loaded.config["agent"]["rev"]["permission"], json!({"edit": "deny", "task": "allow"}));
        assert!(loaded.config["agent"]["rev"]["options"].get("permissions").is_none());
        assert_eq!(loaded.config["agent"]["odd"]["permission"], json!({}));
        // `plugins` entries load v2 plugin modules (`{id, effect|setup}`), which the v1 plugin host cannot run.
        assert_eq!(loaded.config["plugin"], json!([]));
    }

    #[test]
    fn v2_keys_lowered_and_typed() {
        let f = Fixture::new();
        f.write(
            f.work.join("opencode.json"),
            r#"{"model":"openai/gpt-5#high","agents":{"rev":{"system":"s","mode":"subagent"}},
                "commands":{"go":{"template":"run"}},"skills":["./sk","https://x/y"]}"#,
        );
        let loaded = f.load();
        let info = loaded.info().unwrap();
        assert_eq!(info.model.as_deref(), Some("openai/gpt-5"));
        assert_eq!(info.agent["rev"].prompt.as_deref(), Some("s"));
        assert_eq!(info.agent["rev"].mode, Some(AgentMode::Subagent));
        assert_eq!(info.command["go"].template, "run");
        assert_eq!(info.skills.paths, ["./sk"]);
        assert_eq!(info.skills.urls, ["https://x/y"]);
        assert_eq!(loaded.diagnostics.len(), 1);
    }
}
