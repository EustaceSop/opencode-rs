// `opencode plugin <module>` (`opencode/src/cli/cmd/plug.ts`, `opencode/src/plugin/install.ts`).
use crate::ui;
use anyhow::Result;
use opencode_core::{config, paths::Paths};
use opencode_plugin::{InstallOptions, InstallStatus, install, parse_plugin_specifier};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub kind: &'static str,
    pub options: Option<Value>,
}

fn export_target(package: &Value, kind: &str) -> Option<Option<Value>> {
    let value = package.get("exports")?.as_object()?.get(&format!("./{kind}"))?;
    let entry = match value {
        Value::String(text) => Some(text.trim()),
        Value::Object(map) => ["import", "default"].iter().find_map(|key| map.get(*key).and_then(Value::as_str).map(str::trim).filter(|hit| !hit.is_empty())),
        _ => None,
    };
    entry.filter(|entry| !entry.is_empty())?;
    Some(value.get("config").filter(|config| config.is_object()).cloned())
}

// `packageTargets`: `exports["./server"]` (or `main`), `exports["./tui"]` (or `oc-themes`).
pub fn package_targets(package: &Value) -> Vec<Target> {
    let mut targets = Vec::new();
    if let Some(options) = export_target(package, "server") {
        targets.push(Target { kind: "server", options });
    } else if package.get("main").and_then(Value::as_str).is_some_and(|main| !main.trim().is_empty()) {
        targets.push(Target { kind: "server", options: None });
    }
    if let Some(options) = export_target(package, "tui") {
        targets.push(Target { kind: "tui", options });
    } else if package.get("oc-themes").and_then(Value::as_array).is_some_and(|themes| !themes.is_empty()) {
        targets.push(Target { kind: "tui", options: None });
    }
    targets
}

fn spec_of(item: &Value) -> Option<&str> {
    item.as_str().or_else(|| item.as_array()?.first()?.as_str())
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Mode {
    Noop,
    Add,
    Replace,
}

// `patchPluginList` on the parsed list: the new `plugin` array, or nothing to write.
pub fn patch_list(list: Option<&Vec<Value>>, spec: &str, next: Value, force: bool) -> (Mode, Vec<Value>) {
    let package = parse_plugin_specifier(spec).0;
    let mut items = list.cloned().unwrap_or_default();
    let duplicates: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| spec_of(item).is_some_and(|own| own == spec || (!own.starts_with("file://") && parse_plugin_specifier(own).0 == package)))
        .map(|(i, _)| i)
        .collect();
    let Some(&keep) = duplicates.first() else {
        items.push(next);
        return (Mode::Add, items);
    };
    if !force || (duplicates.len() == 1 && spec_of(&items[keep]) == Some(spec)) {
        return (Mode::Noop, items);
    }
    match &mut items[keep] {
        Value::Array(pair) => pair[0] = spec.into(),
        item => *item = next,
    }
    for index in duplicates.into_iter().skip(1).rev() {
        items.remove(index);
    }
    (Mode::Replace, items)
}

fn patch_dir(paths: &Paths, global: bool, directory: &Path) -> PathBuf {
    if global {
        return paths.config.clone();
    }
    config::find_worktree(directory).unwrap_or_else(|| directory.to_path_buf()).join(".opencode")
}

// Updates `<dir>/opencode.json[c]` (server) or `<dir>/tui.json[c]` (tui). `Err` carries the lines to print.
fn patch_one(dir: &Path, target: &Target, spec: &str, force: bool) -> std::result::Result<(Mode, PathBuf), Vec<String>> {
    let name = if target.kind == "server" { "opencode" } else { "tui" };
    let files = [dir.join(format!("{name}.json")), dir.join(format!("{name}.jsonc"))];
    let file = files.iter().find(|file| file.exists()).unwrap_or(&files[0]).clone();
    let source = match std::fs::read_to_string(&file) {
        Ok(text) if !text.trim().is_empty() => text,
        Ok(_) => "{}".to_string(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "{}".to_string(),
        Err(error) => return Err(vec!["Failed updating plugin config".into(), error.to_string()]),
    };
    let label = file.to_string_lossy().into_owned();
    let data = config::parse_jsonc(&source, &label).map_err(|error| vec![format!("Failed updating {} config", target.kind), format!("Invalid JSON in {label} ({error})"), "Fix the config file and run the command again.".into()])?;
    let item = match &target.options {
        Some(options) => json!([spec, options]),
        None => spec.into(),
    };
    let (mode, list) = patch_list(data.get("plugin").and_then(Value::as_array), spec, item, force);
    if mode == Mode::Noop {
        return Ok((mode, file));
    }
    let failed = |error: String| vec!["Failed updating plugin config".to_string(), error];
    let updated = config::set_jsonc(&source, &["plugin"], &Value::Array(list), &label).map_err(|e| failed(e.to_string()))?;
    std::fs::create_dir_all(dir).map_err(|e| failed(e.to_string()))?;
    std::fs::write(&file, updated).map_err(|e| failed(e.to_string()))?;
    Ok((mode, file))
}

pub async fn run(paths: &Paths, module: &str, global: bool, force: bool) -> Result<i32> {
    let module = module.trim();
    if module.is_empty() {
        ui::error("module is required");
        return Ok(1);
    }
    ui::empty();
    ui::intro(&format!("Install plugin {module}"));
    let ok = task(paths, module, global, force).await?;
    ui::outro("Done");
    Ok(if ok { 0 } else { 1 })
}

async fn task(paths: &Paths, module: &str, global: bool, force: bool) -> Result<bool> {
    let options = InstallOptions { specs: vec![module.to_string()], cache_dir: paths.cache.clone(), bun_path: None };
    let failed = |detail: String| {
        ui::done("Install failed", false);
        ui::failure(&format!("Could not install \"{module}\""));
        ui::failure(&detail);
        Ok(false)
    };
    let outcome = match install(&options).await {
        Ok(outcomes) => outcomes.into_iter().next(),
        Err(error) => return failed(error.to_string()),
    };
    let Some(outcome) = outcome else {
        return failed("only npm package specs can be installed".into());
    };
    if let InstallStatus::Failed(message) = &outcome.status {
        return failed(message.clone());
    }
    ui::done("Plugin package ready", true);

    let manifest = outcome.target.module_dir().join("package.json");
    let package: Value = match std::fs::read_to_string(&manifest).map_err(|e| e.to_string()).and_then(|text| serde_json::from_str(&text).map_err(|e| e.to_string())) {
        Ok(package) => package,
        Err(error) => {
            ui::done("Manifest read failed", false);
            ui::failure(&format!("Installed \"{module}\" but failed to read {}", manifest.display()));
            ui::failure(&error);
            return Ok(false);
        }
    };
    let targets = package_targets(&package);
    if targets.is_empty() {
        ui::done("No plugin targets found", false);
        ui::failure(&format!("\"{module}\" does not expose plugin entrypoints in package.json"));
        ui::info("Expected one of: exports[\"./tui\"], exports[\"./server\"], package.json main for server, or package.json[\"oc-themes\"] for tui themes.");
        return Ok(false);
    }
    let kinds: Vec<&str> = targets.iter().map(|target| target.kind).collect();
    ui::done(&format!("Detected {} target{}", kinds.join(" + "), if targets.len() == 1 { "" } else { "s" }), true);

    let dir = patch_dir(paths, global, &std::env::current_dir()?);
    let mut items = Vec::new();
    for target in &targets {
        match patch_one(&dir, target, module, force) {
            Ok(item) => items.push(item),
            Err(lines) => {
                ui::done(&lines[0], false);
                for (i, line) in lines.iter().enumerate().skip(1) {
                    if i == 2 { ui::info(line) } else { ui::failure(line) }
                }
                return Ok(false);
            }
        }
    }
    ui::done("Plugin config updated", true);
    for (mode, file) in &items {
        ui::info(&match mode {
            Mode::Noop => format!("Already configured in {}", file.display()),
            Mode::Replace => format!("Replaced in {}", file.display()),
            Mode::Add => format!("Added to {}", file.display()),
        });
    }
    ui::success(&format!("Installed {module}"));
    ui::info(&format!("Scope: {} ({})", if global { "global" } else { "local" }, dir.display()));
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_targets() {
        assert_eq!(package_targets(&json!({"main": "index.js"})), [Target { kind: "server", options: None }]);
        assert_eq!(package_targets(&json!({"main": "  "})), []);
        let both = json!({"exports": {"./server": {"import": "./s.js", "config": {"a": 1}}, "./tui": "./t.js"}, "main": "x.js"});
        assert_eq!(package_targets(&both), [Target { kind: "server", options: Some(json!({"a": 1})) }, Target { kind: "tui", options: None }]);
        assert_eq!(package_targets(&json!({"oc-themes": ["themes/a.json"]})), [Target { kind: "tui", options: None }]);
        assert_eq!(package_targets(&json!({"exports": {"./server": {"require": "./s.cjs"}}})), []);
    }

    #[test]
    fn plugin_list_patching() {
        let list = vec![json!("other"), json!("demo@1.0.0"), json!(["demo@0.9.0", {"k": 1}])];
        assert_eq!(patch_list(None, "demo", json!("demo"), false), (Mode::Add, vec![json!("demo")]));
        let (mode, added) = patch_list(Some(&list), "fresh@2", json!("fresh@2"), false);
        assert_eq!((mode, added.len(), &added[3]), (Mode::Add, 4, &json!("fresh@2")));
        // An entry for the same package is left alone unless forced; forcing keeps the first and drops the rest.
        assert_eq!(patch_list(Some(&list), "demo@2.0.0", json!("demo@2.0.0"), false), (Mode::Noop, list.clone()));
        assert_eq!(patch_list(Some(&list), "demo@2.0.0", json!("demo@2.0.0"), true), (Mode::Replace, vec![json!("other"), json!("demo@2.0.0")]));
        let tuple = vec![json!(["demo@0.9.0", {"k": 1}])];
        assert_eq!(patch_list(Some(&tuple), "demo@2.0.0", json!("demo@2.0.0"), true), (Mode::Replace, vec![json!(["demo@2.0.0", {"k": 1}])]));
        let same = vec![json!("demo@2.0.0")];
        assert_eq!(patch_list(Some(&same), "demo@2.0.0", json!("demo@2.0.0"), true).0, Mode::Noop);
    }

    #[test]
    fn config_files_are_patched_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("opencode.jsonc");
        std::fs::write(&file, "{\n  // keep me\n  \"model\": \"a/b\"\n}\n").unwrap();
        let target = Target { kind: "server", options: Some(json!({"x": true})) };
        assert_eq!(patch_one(dir.path(), &target, "demo@1", false).unwrap(), (Mode::Add, file.clone()));
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("// keep me"), "{text}");
        let data = config::parse_jsonc(&text, "test").unwrap();
        assert_eq!(data, json!({"model": "a/b", "plugin": [["demo@1", {"x": true}]]}));
        assert_eq!(patch_one(dir.path(), &target, "demo@1", false).unwrap().0, Mode::Noop);

        let tui = Target { kind: "tui", options: None };
        let (mode, created) = patch_one(dir.path(), &tui, "demo@1", false).unwrap();
        assert_eq!((mode, created.clone()), (Mode::Add, dir.path().join("tui.json")));
        assert_eq!(config::parse_jsonc(&std::fs::read_to_string(created).unwrap(), "test").unwrap(), json!({"plugin": ["demo@1"]}));

        std::fs::write(&file, "{ nope").unwrap();
        let lines = patch_one(dir.path(), &target, "demo@1", false).unwrap_err();
        assert_eq!((lines[0].as_str(), lines[2].as_str()), ("Failed updating server config", "Fix the config file and run the command again."));
        assert!(lines[1].starts_with(&format!("Invalid JSON in {}", file.display())), "{}", lines[1]);
    }
}
