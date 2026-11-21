//! Plugin / custom-tool discovery and explicit npm install.
//! Sources: `opencode/src/config/plugin.ts`, `opencode/src/plugin/shared.ts`, `opencode/src/tool/registry.ts:183-197`,
//! `core/src/npm.ts` (v1.19.5).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use opencode_core::glob;
use serde_json::{Map, Value};
use url::Url;

use crate::PluginError;

pub const DEPRECATED_PLUGIN_PACKAGES: [&str; 2] = ["opencode-openai-codex-auth", "opencode-copilot-auth"];
const INDEX_FILES: [&str; 5] = ["index.ts", "index.tsx", "index.js", "index.mjs", "index.cjs"];

#[derive(Debug, Clone, PartialEq)]
pub struct PluginSpec {
    pub spec: String,
    pub options: Option<Map<String, Value>>,
}

impl PluginSpec {
    pub fn new(spec: impl Into<String>) -> Self {
        Self { spec: spec.into(), options: None }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Discovered {
    pub specs: Vec<PluginSpec>,
    pub tool_files: Vec<PathBuf>,
    /// Relative path specs whose declaring config file could not be determined from the merged config.
    pub unresolved: Vec<String>,
}

impl Discovered {
    pub fn is_empty(&self) -> bool {
        self.specs.is_empty() && self.tool_files.is_empty()
    }
}

pub fn is_deprecated_plugin(spec: &str) -> bool {
    DEPRECATED_PLUGIN_PACKAGES.iter().any(|pkg| spec.contains(pkg))
}

fn is_windows_absolute(raw: &str) -> bool {
    let b = raw.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

pub fn is_path_plugin_spec(spec: &str) -> bool {
    spec.starts_with("file://")
        || spec.starts_with(['.', '/'])
        || Path::new(spec).is_absolute()
        || is_windows_absolute(spec)
}

fn valid_name_part(part: &str) -> bool {
    !part.is_empty() && !part.starts_with('.') && part.chars().all(|c| c.is_ascii_alphanumeric() || "-_.~".contains(c))
}

// Registry-style `name[@range]` / `@scope/name[@range]`; anything else (git, URL, github shorthand) has no name.
fn split_registry_spec(spec: &str) -> Option<(&str, Option<&str>)> {
    let at = match spec.strip_prefix('@') {
        Some(rest) => rest.find('@').map(|i| i + 1),
        None => spec.find('@'),
    };
    let (name, rest) = match at {
        Some(i) => (&spec[..i], Some(&spec[i + 1..])),
        None => (spec, None),
    };
    let valid = match name.strip_prefix('@') {
        Some(scoped) => scoped.split_once('/').is_some_and(|(scope, pkg)| valid_name_part(scope) && valid_name_part(pkg)),
        None => valid_name_part(name),
    };
    valid.then_some((name, rest))
}

/// `parsePluginSpecifier` (`shared.ts:24-36`) for registry specs: `(pkg, version)`.
pub fn parse_plugin_specifier(spec: &str) -> (String, String) {
    match split_registry_spec(spec) {
        Some((name, None)) => (name.to_string(), "latest".to_string()),
        Some((name, Some(rest))) => (name.to_string(), rest.to_string()),
        None => (spec.to_string(), String::new()),
    }
}

fn file_url(path: &Path) -> Option<String> {
    Url::from_file_path(path).ok().map(String::from)
}

pub(crate) fn file_url_to_path(spec: &str) -> Option<PathBuf> {
    Url::parse(spec).ok()?.to_file_path().ok()
}

// `resolvePathPluginTarget` (`shared.ts:194-210`); falls back to the plain file URL like `resolvePluginSpec`.
fn resolve_path_target(file: &Path) -> Option<String> {
    if !file.is_dir() || file.join("package.json").exists() {
        return file_url(file);
    }
    match INDEX_FILES.iter().map(|name| file.join(name)).find(|p| p.exists()) {
        Some(index) => file_url(&index),
        None => file_url(file),
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

fn resolve_spec(spec: &str, directories: &[PathBuf], unresolved: &mut Vec<String>) -> String {
    if !is_path_plugin_spec(spec) {
        return spec.to_string();
    }
    if spec.starts_with("file://") {
        return file_url_to_path(spec).and_then(|p| resolve_path_target(&p)).unwrap_or_else(|| spec.to_string());
    }
    if Path::new(spec).is_absolute() || is_windows_absolute(spec) {
        return resolve_path_target(Path::new(spec)).unwrap_or_else(|| spec.to_string());
    }
    for dir in directories.iter().rev() {
        let parent = dir.parent().filter(|_| dir.file_name().is_some_and(|n| n == ".opencode"));
        for base in [Some(dir.as_path()), parent].into_iter().flatten() {
            let file = normalize(&base.join(spec));
            if file.exists() {
                return resolve_path_target(&file).unwrap_or_else(|| spec.to_string());
            }
        }
    }
    unresolved.push(spec.to_string());
    spec.to_string()
}

fn identity(spec: &str) -> String {
    if spec.starts_with("file://") { spec.to_string() } else { parse_plugin_specifier(spec).0 }
}

// `deduplicatePluginOrigins` (`config/plugin.ts:64-77`): last declaration wins, order preserved.
fn dedupe(specs: Vec<PluginSpec>) -> Vec<PluginSpec> {
    let mut seen = HashSet::new();
    let mut out: Vec<PluginSpec> = specs.into_iter().rev().filter(|item| seen.insert(identity(&item.spec))).collect();
    out.reverse();
    out
}

fn config_specs(config: &Value) -> Vec<PluginSpec> {
    let Some(list) = config.get("plugin").and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|item| match item {
            Value::String(spec) => Some(PluginSpec::new(spec.clone())),
            Value::Array(pair) => Some(PluginSpec {
                spec: pair.first()?.as_str()?.to_string(),
                options: pair.get(1).and_then(Value::as_object).cloned(),
            }),
            _ => None,
        })
        .collect()
}

/// Plugin specs from the merged config `plugin` key followed by auto-discovered `{plugin,plugins}/*.{ts,js}` of
/// every config directory (as `file://` URLs), de-duplicated; plus `{tool,tools}/*.{js,ts}` custom tool files.
/// Never installs anything.
pub fn discover(config: &Value, directories: &[PathBuf]) -> Discovered {
    let options = glob::Options { dot: true, follow: true };
    let mut unresolved = Vec::new();
    let mut specs: Vec<PluginSpec> = config_specs(config)
        .into_iter()
        .map(|item| PluginSpec { spec: resolve_spec(&item.spec, directories, &mut unresolved), options: item.options })
        .collect();
    let mut tool_files = Vec::new();
    for dir in directories {
        for file in glob::scan("{plugin,plugins}/*.{ts,js}", dir, options) {
            if let Some(url) = file_url(&file) {
                specs.push(PluginSpec::new(url));
            }
        }
        tool_files.extend(glob::scan("{tool,tools}/*.{js,ts}", dir, options));
    }
    Discovered { specs: dedupe(specs), tool_files, unresolved }
}

/// `sanitize` (`core/src/npm.ts:46-51`).
pub fn sanitize(pkg: &str) -> String {
    if !cfg!(windows) {
        return pkg.to_string();
    }
    pkg.chars().map(|c| if "<>:\"|?*".contains(c) || (c as u32) < 32 { '_' } else { c }).collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct NpmTarget {
    /// Argument handed to the package manager (`<name>@latest` for bare names, else the spec verbatim).
    pub package: String,
    /// Package name inside `node_modules`.
    pub name: String,
    /// `<cache>/packages/<sanitize(package)>`.
    pub dir: PathBuf,
}

impl NpmTarget {
    pub fn module_dir(&self) -> PathBuf {
        self.name.split('/').fold(self.dir.join("node_modules"), |p, s| p.join(s))
    }

    pub fn is_installed(&self) -> bool {
        self.module_dir().exists()
    }
}

/// Where `Npm.add` puts an npm plugin (`shared.ts:444-450`, `npm.ts:87,124-146`); `None` for path specs.
pub fn npm_target(spec: &str, cache_dir: &Path) -> Option<NpmTarget> {
    if is_path_plugin_spec(spec) {
        return None;
    }
    let parsed = split_registry_spec(spec);
    let package = match parsed {
        Some((name, None)) => format!("{name}@latest"),
        _ => spec.to_string(),
    };
    let dir = cache_dir.join("packages").join(sanitize(&package));
    let name = match parsed {
        Some((name, _)) => name.to_string(),
        None => sole_dependency(&dir).unwrap_or_else(|| spec.to_string()),
    };
    Some(NpmTarget { package, name, dir })
}

fn sole_dependency(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let json: Value = serde_json::from_str(&text).ok()?;
    let deps = json.get("dependencies")?.as_object()?;
    (deps.len() == 1).then(|| deps.keys().next().cloned()).flatten()
}

pub const BUN_INSTALL_HINT: &str = "install Bun from https://bun.sh and make sure `bun` is on PATH";

pub fn find_bun(explicit: Option<&Path>) -> Result<PathBuf, PluginError> {
    let missing = || PluginError::BunNotFound { hint: BUN_INSTALL_HINT.to_string() };
    match explicit {
        Some(path) if path.is_file() => Ok(path.to_path_buf()),
        Some(path) => which::which(path).map_err(|_| missing()),
        None => which::which("bun").map_err(|_| missing()),
    }
}

#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    pub specs: Vec<String>,
    /// opencode cache directory (`Paths.cache`); packages go to `<cache_dir>/packages/...`.
    pub cache_dir: PathBuf,
    pub bun_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InstallStatus {
    AlreadyInstalled,
    Installed,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct InstallOutcome {
    pub spec: String,
    pub target: NpmTarget,
    pub status: InstallStatus,
}

/// Runs `bun add` for every npm spec that is not installed yet. Path specs and deprecated packages are skipped.
/// `bun` is only looked up when something actually needs installing.
pub async fn install(options: &InstallOptions) -> Result<Vec<InstallOutcome>, PluginError> {
    let mut out = Vec::new();
    let mut bun = None;
    for spec in &options.specs {
        if is_deprecated_plugin(spec) {
            continue;
        }
        let Some(target) = npm_target(spec, &options.cache_dir) else {
            continue;
        };
        if target.is_installed() {
            out.push(InstallOutcome { spec: spec.clone(), target, status: InstallStatus::AlreadyInstalled });
            continue;
        }
        let bun = match &bun {
            Some(path) => path,
            None => bun.insert(find_bun(options.bun_path.as_deref())?),
        };
        let status = match bun_add(bun, &target).await {
            Ok(()) => InstallStatus::Installed,
            Err(message) => InstallStatus::Failed(message),
        };
        let target = npm_target(spec, &options.cache_dir).unwrap_or(target);
        out.push(InstallOutcome { spec: spec.clone(), target, status });
    }
    Ok(out)
}

async fn bun_add(bun: &Path, target: &NpmTarget) -> Result<(), String> {
    std::fs::create_dir_all(&target.dir).map_err(|e| e.to_string())?;
    let manifest = target.dir.join("package.json");
    if !manifest.exists() {
        std::fs::write(&manifest, "{}\n").map_err(|e| e.to_string())?;
    }
    let output = tokio::process::Command::new(bun)
        .args(["add", "--exact", "--ignore-scripts", &target.package])
        .current_dir(&target.dir)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!("bun add {} failed ({}): {}", target.package, output.status, stderr.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "export {}").unwrap();
    }

    #[test]
    fn specifier_parsing() {
        assert_eq!(parse_plugin_specifier("foo"), ("foo".into(), "latest".into()));
        assert_eq!(parse_plugin_specifier("foo@1.2.3"), ("foo".into(), "1.2.3".into()));
        assert_eq!(parse_plugin_specifier("@scope/pkg"), ("@scope/pkg".into(), "latest".into()));
        assert_eq!(parse_plugin_specifier("@scope/pkg@^2"), ("@scope/pkg".into(), "^2".into()));
        assert_eq!(parse_plugin_specifier("github:user/repo"), ("github:user/repo".into(), String::new()));
        assert_eq!(parse_plugin_specifier("user/repo"), ("user/repo".into(), String::new()));
        assert!(is_path_plugin_spec("./x.ts") && is_path_plugin_spec("file:///a") && is_path_plugin_spec("C:\\a\\b.ts"));
        assert!(!is_path_plugin_spec("opencode-foo"));
        assert!(is_deprecated_plugin("opencode-copilot-auth@1.0.0"));
    }

    #[test]
    fn npm_targets() {
        let cache = Path::new("cache");
        let bare = npm_target("opencode-foo", cache).unwrap();
        assert_eq!(bare.package, "opencode-foo@latest");
        assert_eq!(bare.dir, cache.join("packages").join("opencode-foo@latest"));
        assert_eq!(bare.module_dir(), bare.dir.join("node_modules").join("opencode-foo"));
        let scoped = npm_target("@a/b@1.0.0", cache).unwrap();
        assert_eq!(scoped.package, "@a/b@1.0.0");
        assert_eq!(scoped.name, "@a/b");
        assert_eq!(scoped.module_dir(), scoped.dir.join("node_modules").join("@a").join("b"));
        assert!(npm_target("./local.ts", cache).is_none());
        if cfg!(windows) {
            assert_eq!(sanitize("a@npm:b"), "a@npm_b");
        }
    }

    #[test]
    fn discovery_order_and_dedupe() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path().join("global");
        let project = tmp.path().join("proj").join(".opencode");
        touch(&global.join("plugin").join("g.ts"));
        touch(&project.join("plugins").join("p.js"));
        touch(&project.join("plugin").join("skip.txt"));
        touch(&project.join("tool").join("hello.ts"));
        touch(&global.join("tools").join("other.js"));
        touch(&tmp.path().join("proj").join("local.ts"));
        let p_url = file_url(&project.join("plugins").join("p.js")).unwrap();

        let config = json!({"plugin": [
            "opencode-foo@1.0.0",
            ["opencode-bar", {"k": 1}],
            "./local.ts",
            "./missing.ts",
            p_url,
            "opencode-foo@2.0.0",
        ]});
        let found = discover(&config, &[global.clone(), project.clone()]);
        let specs: Vec<&str> = found.specs.iter().map(|s| s.spec.as_str()).collect();
        let local = file_url(&tmp.path().join("proj").join("local.ts")).unwrap();
        let g_url = file_url(&global.join("plugin").join("g.ts")).unwrap();
        assert_eq!(specs, ["opencode-bar", &local, "./missing.ts", "opencode-foo@2.0.0", &g_url, &p_url]);
        assert_eq!(found.specs[0].options.as_ref().unwrap()["k"], 1);
        assert_eq!(found.unresolved, ["./missing.ts"]);
        assert_eq!(found.tool_files, [global.join("tools").join("other.js"), project.join("tool").join("hello.ts")]);
        assert!(discover(&json!({"plugin": []}), &[]).is_empty());
    }

    #[tokio::test]
    async fn install_skips_installed_and_paths_without_bun() {
        let tmp = tempfile::tempdir().unwrap();
        let target = npm_target("opencode-foo", tmp.path()).unwrap();
        std::fs::create_dir_all(target.module_dir()).unwrap();
        let out = install(&InstallOptions {
            specs: vec!["opencode-foo".into(), "./x.ts".into(), "opencode-copilot-auth".into()],
            cache_dir: tmp.path().to_path_buf(),
            bun_path: Some(PathBuf::from("definitely-not-bun-binary")),
        })
        .await
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].status, InstallStatus::AlreadyInstalled);

        let err = install(&InstallOptions {
            specs: vec!["opencode-needs-install".into()],
            cache_dir: tmp.path().to_path_buf(),
            bun_path: Some(PathBuf::from("definitely-not-bun-binary")),
        })
        .await
        .unwrap_err();
        assert!(err.to_string().starts_with("plugins need Bun: "), "{err}");
    }
}
