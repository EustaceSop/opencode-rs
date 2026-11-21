// Writing config back (`Config.update`, `Config.updateGlobal` in `opencode/src/config/config.ts`).
use super::{ConfigError, merge_deep};
use crate::paths::Paths;
use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstInputValue, CstObject, CstRootNode};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

fn options() -> ParseOptions {
    ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_loose_object_property_names: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
        ..Default::default()
    }
}

// JSONC as written, without `{env:}` / `{file:}` substitution.
pub fn parse_jsonc(text: &str, source: &str) -> Result<Value, ConfigError> {
    if text.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    jsonc_parser::parse_to_serde_value(text, &options())
        .map_err(|e| ConfigError::Json { path: source.to_string(), message: e.to_string() })
}

fn io(path: &Path, error: std::io::Error) -> ConfigError {
    ConfigError::Invalid { path: path.to_string_lossy().into_owned(), message: error.to_string() }
}

fn read(path: &Path) -> Result<Option<String>, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io(path, error)),
    }
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

// `writable`: the patch without derived keys.
fn writable(patch: &Value) -> Value {
    let mut patch = patch.clone();
    if let Some(map) = patch.as_object_mut() {
        map.shift_remove("plugin_origins");
    }
    patch
}

// Deep-merges `patch` into `<directory>/config.json`, creating it when missing.
pub fn update_project(directory: &Path, patch: &Value) -> Result<PathBuf, ConfigError> {
    let file = directory.join("config.json");
    let source = file.to_string_lossy().into_owned();
    let mut merged = match read(&file)?.filter(|text| !text.is_empty()) {
        Some(text) => Some(parse_jsonc(&text, &source)?).filter(Value::is_object),
        None => None,
    }
    .unwrap_or_else(|| Value::Object(Map::new()));
    merge_deep(&mut merged, writable(patch));
    std::fs::write(&file, pretty(&merged)).map_err(|e| io(&file, e))?;
    Ok(file)
}

// The global file that gets written: the first existing of these, else `opencode.jsonc`.
pub fn global_file(paths: &Paths) -> PathBuf {
    let candidates = ["opencode.jsonc", "opencode.json", "config.json"].map(|name| paths.config.join(name));
    candidates.iter().find(|file| file.exists()).unwrap_or(&candidates[0]).clone()
}

fn input(value: &Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(flag) => CstInputValue::Bool(*flag),
        Value::Number(number) => CstInputValue::Number(number.to_string()),
        Value::String(text) => CstInputValue::String(text.clone()),
        Value::Array(items) => CstInputValue::Array(items.iter().map(input).collect()),
        Value::Object(map) => CstInputValue::Object(map.iter().map(|(key, value)| (key.clone(), input(value))).collect()),
    }
}

// `patchJsonc`: objects are walked, everything else replaces the value at its path. `removed` paths are deleted.
fn patch_object(object: &CstObject, patch: &Map<String, Value>, removed: &[&str]) {
    for (key, value) in patch {
        if removed.contains(&key.as_str()) {
            if let Some(prop) = object.get(key) {
                prop.remove();
            }
            continue;
        }
        match value {
            Value::Object(inner) => patch_object(&object.object_value_or_set(key), inner, &[]),
            other => match object.get(key) {
                Some(prop) => prop.set_value(input(other)),
                None => {
                    object.append(key, input(other));
                }
            },
        }
    }
}

// jsonc-parser's `modify(text, path, value)`: replaces the value at `path`, creating parents, keeping comments.
pub fn set_jsonc(text: &str, path: &[&str], value: &Value, source: &str) -> Result<String, ConfigError> {
    let text = if text.trim().is_empty() { "{}" } else { text };
    let root = CstRootNode::parse(text, &options()).map_err(|e| ConfigError::Json { path: source.to_string(), message: e.to_string() })?;
    let Some((last, parents)) = path.split_last() else { return Ok(text.to_string()) };
    let mut object = root.object_value_or_set();
    for key in parents {
        object = object.object_value_or_set(key);
    }
    match object.get(last) {
        Some(prop) => prop.set_value(input(value)),
        None => {
            object.append(last, input(value));
        }
    }
    Ok(root.to_string())
}

// Merges `patch` into the global config file, keeping comments of a `.jsonc` file. Returns (file, changed).
pub fn update_global(paths: &Paths, patch: &Value) -> Result<(PathBuf, bool), ConfigError> {
    let file = global_file(paths);
    let source = file.to_string_lossy().into_owned();
    let before = read(&file)?.unwrap_or_else(|| "{}".to_string());
    let patch = writable(patch);
    // An emptied shell setting removes the key instead of leaving `"shell": ""`.
    let blank_shell = patch.get("shell").and_then(Value::as_str) == Some("");
    let after = if file.extension().is_some_and(|ext| ext == "jsonc") {
        let invalid = |message: String| ConfigError::Json { path: source.clone(), message };
        let root = CstRootNode::parse(&before, &options()).map_err(|e| invalid(e.to_string()))?;
        let removed: &[&str] = if blank_shell { &["shell"] } else { &[] };
        if let Some(map) = patch.as_object() {
            patch_object(&root.object_value_or_set(), map, removed);
        }
        let updated = root.to_string();
        parse_jsonc(&updated, &source)?;
        updated
    } else {
        let mut merged = Some(parse_jsonc(&before, &source)?).filter(Value::is_object).unwrap_or_else(|| Value::Object(Map::new()));
        merge_deep(&mut merged, patch);
        if blank_shell && let Some(map) = merged.as_object_mut() {
            map.shift_remove("shell");
        }
        pretty(&merged)
    };
    let changed = after != before;
    if changed {
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io(&file, e))?;
        }
        std::fs::write(&file, &after).map_err(|e| io(&file, e))?;
    }
    Ok((file, changed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn paths(root: &Path) -> Paths {
        let dir = |name: &str| root.join(name);
        Paths { home: dir("home"), data: dir("data"), cache: dir("cache"), config: dir("config"), state: dir("state"), tmp: dir("tmp") }
    }

    #[test]
    fn set_jsonc_replaces_one_path() {
        let text = "{
  // servers
  \"mcp\": { \"a\": { \"type\": \"local\", \"command\": [\"x\"] } }
}";
        let out = set_jsonc(text, &["mcp", "b"], &json!({"type": "remote", "url": "https://e/mcp"}), "x").unwrap();
        assert!(out.contains("// servers"), "{out}");
        let expected = json!({"mcp": {"a": {"type": "local", "command": ["x"]}, "b": {"type": "remote", "url": "https://e/mcp"}}});
        assert_eq!(parse_jsonc(&out, "x").unwrap(), expected);
        let replaced = set_jsonc(&out, &["mcp", "a"], &json!({"type": "remote", "url": "u"}), "x").unwrap();
        assert_eq!(parse_jsonc(&replaced, "x").unwrap()["mcp"]["a"], json!({"type": "remote", "url": "u"}));
        assert_eq!(parse_jsonc(&set_jsonc("", &["mcp", "n"], &json!(1), "x").unwrap(), "x").unwrap(), json!({"mcp": {"n": 1}}));
    }

    #[test]
    fn project_update_merges_into_config_json() {
        let root = tempfile::tempdir().unwrap();
        let file = update_project(root.path(), &json!({"model": "a/b", "plugin_origins": [1]})).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{\n  \"model\": \"a/b\"\n}");
        std::fs::write(&file, "{\n  // kept as data only\n  \"model\": \"a/b\",\n  \"agent\": {\"x\": {\"steps\": 1}},\n}").unwrap();
        update_project(root.path(), &json!({"agent": {"x": {"model": "c/d"}}, "instructions": ["i"]})).unwrap();
        let written: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(written, json!({"model": "a/b", "agent": {"x": {"steps": 1, "model": "c/d"}}, "instructions": ["i"]}));
    }

    #[test]
    fn global_update_picks_the_existing_file_and_keeps_jsonc_comments() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        // Nothing exists yet: opencode.jsonc is created.
        let (file, changed) = update_global(&paths, &json!({"model": "a/b"})).unwrap();
        assert_eq!((file.file_name().unwrap().to_str().unwrap(), changed), ("opencode.jsonc", true));
        assert_eq!(parse_jsonc(&std::fs::read_to_string(&file).unwrap(), "x").unwrap(), json!({"model": "a/b"}));
        assert!(!update_global(&paths, &json!({"model": "a/b"})).unwrap().1);

        std::fs::write(&file, "{\n  // my model\n  \"model\": \"a/b\",\n  \"shell\": \"zsh\",\n  \"agent\": { \"x\": { \"steps\": 1 } }\n}\n").unwrap();
        update_global(&paths, &json!({"model": "c/d", "shell": "", "agent": {"x": {"steps": 2}}, "username": "me"})).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("// my model"), "{text}");
        assert_eq!(parse_jsonc(&text, "x").unwrap(), json!({"model": "c/d", "agent": {"x": {"steps": 2}}, "username": "me"}));

        // A plain JSON file is rewritten as formatted JSON.
        std::fs::remove_file(&file).unwrap();
        let json_file = paths.config.join("config.json");
        std::fs::write(&json_file, "{\"model\":\"a/b\"}").unwrap();
        let (file, changed) = update_global(&paths, &json!({"username": "me"})).unwrap();
        assert_eq!((file.clone(), changed), (json_file, true));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{\n  \"model\": \"a/b\",\n  \"username\": \"me\"\n}");
    }
}
