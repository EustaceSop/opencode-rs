//! `Npm.which` (`core/src/npm.ts`): packages live in `<cache>/packages/<pkg>` and are run through
//! `node_modules/.bin`. The source reifies with its bundled arborist; here `bun` or `npm` does it.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{Env, Installer, Step, failed, unavailable};

// Launcher variants npm and bun put next to (or instead of) the bare name on Windows.
const SHIMS: &[&str] = &[".cmd", ".ps1", ".exe", ".bunx"];

pub(crate) fn sanitize(pkg: &str) -> String {
    if !cfg!(windows) {
        return pkg.to_string();
    }
    pkg.chars()
        .map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || (c as u32) < 32 {
                '_'
            } else {
                c
            }
        })
        .collect()
}

pub(crate) fn directory(cache: &Path, pkg: &str) -> PathBuf {
    cache.join("packages").join(sanitize(pkg))
}

fn module_dir(dir: &Path, pkg: &str) -> PathBuf {
    pkg.split('/')
        .fold(dir.join("node_modules"), |path, part| path.join(part))
}

fn bin_names(bin_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(bin_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .map(|name| {
            if !cfg!(windows) {
                return name;
            }
            let stripped = SHIMS.iter().find_map(|ext| name.strip_suffix(ext));
            stripped.map_or(name.clone(), str::to_string)
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

// The `pick` step of `Npm.which`.
fn pick_name(dir: &Path, pkg: &str, hint: Option<&str>) -> Option<String> {
    let names = bin_names(&dir.join("node_modules").join(".bin"));
    if names.is_empty() {
        return None;
    }
    if let Some(hint) = hint {
        return names
            .iter()
            .any(|name| name == hint)
            .then(|| hint.to_string());
    }
    if names.len() == 1 {
        return names.into_iter().next();
    }
    let manifest = std::fs::read_to_string(module_dir(dir, pkg).join("package.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    let unscoped = match pkg.strip_prefix('@') {
        Some(rest) => rest.split('/').nth(1).unwrap_or(pkg),
        None => pkg,
    };
    match manifest.as_ref().and_then(|json| json.get("bin")) {
        Some(Value::String(text)) if !text.is_empty() => Some(unscoped.to_string()),
        Some(Value::Object(bins)) => {
            if bins.len() == 1 || !bins.contains_key(unscoped) {
                return bins.keys().next().cloned();
            }
            Some(unscoped.to_string())
        }
        _ => names.into_iter().next(),
    }
}

/// The cached executable of `pkg`, without installing anything.
pub(crate) fn pick(cache: &Path, pkg: &str, hint: Option<&str>) -> Option<PathBuf> {
    let dir = directory(cache, pkg);
    let name = pick_name(&dir, pkg, hint)?;
    let bin_dir = dir.join("node_modules").join(".bin");
    let exts: &[&str] = if cfg!(windows) {
        &[".cmd", ".exe", ""]
    } else {
        &[""]
    };
    exts.iter()
        .map(|ext| bin_dir.join(format!("{name}{ext}")))
        .find(|path| path.is_file())
}

impl Installer {
    pub(super) async fn npm(
        &self,
        env: Env<'_>,
        cache: &Path,
        pkg: &str,
        hint: Option<&str>,
    ) -> Step {
        if pick(cache, pkg, hint).is_some() {
            return Ok(());
        }
        let dir = directory(cache, pkg);
        let _ = std::fs::remove_file(dir.join("package-lock.json"));
        // `Npm.add` skips the reify when the package directory is already there.
        if !module_dir(&dir, pkg).exists() {
            let with_bun = |bun: String| -> (String, Vec<&str>) {
                let flags = ["--exact", "--ignore-scripts", "--no-progress"];
                (bun, ["add", pkg].into_iter().chain(flags).collect())
            };
            let with_npm = |npm: String| -> (String, Vec<&str>) {
                let flags = ["--save-prod", "--save-exact", "--ignore-scripts", "--no-progress"];
                (npm, ["install", pkg].into_iter().chain(flags).collect())
            };
            // On Windows bun writes .exe/.bunx shims the TypeScript build does not resolve; npm's .cmd shims work for both.
            let choice = if cfg!(windows) {
                env.which("npm").map(with_npm).or_else(|| env.which("bun").map(with_bun))
            } else {
                env.which("bun").map(with_bun).or_else(|| env.which("npm").map(with_npm))
            };
            let Some((program, args)) = choice else {
                return Err(unavailable("neither bun nor npm was found on PATH"));
            };
            std::fs::create_dir_all(&dir).map_err(|e| failed(format!("{}: {e}", dir.display())))?;
            // Keeps either tool from adopting a parent directory as the project.
            let manifest = dir.join("package.json");
            if !manifest.exists() {
                std::fs::write(&manifest, "{}\n")
                    .map_err(|e| failed(format!("{}: {e}", manifest.display())))?;
            }
            self.tool(&program, &args, Some(&dir), &[]).await?;
        }
        pick(cache, pkg, hint).map(drop).ok_or_else(|| {
            failed(format!(
                "{pkg} was installed but exposes no matching executable"
            ))
        })
    }
}
