// What the Rust build already sees (names that must not be shadowed), and the writers for `<XDG config>/opencode-rs/`.
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use opencode_core::config::{self, markdown};
use opencode_core::glob;
use opencode_core::paths::Paths;
use serde_json::{Map, Value};

use crate::{Category, ImportReport, Payload, Plan};

pub const BUILTIN_AGENTS: [&str; 7] = ["build", "plan", "general", "explore", "compaction", "title", "summary"];
const HEADER: &str = "// Written by `opencode import`. Only the Rust build of opencode reads this directory.\n{}\n";

pub fn name_key(category: Category, name: &str) -> String {
    match category {
        Category::Mcp => name.to_string(),
        _ => name.to_lowercase(),
    }
}

fn read_json(file: &Path) -> Option<Value> {
    let text = fs::read_to_string(file).ok()?;
    config::parse_jsonc(&text, &file.to_string_lossy()).ok()
}

fn keys(value: Option<&Value>) -> impl Iterator<Item = String> + '_ {
    value.and_then(Value::as_object).into_iter().flat_map(|m| m.keys().cloned())
}

fn files(dir: &Path, pattern: &str) -> Vec<PathBuf> {
    if !dir.is_dir() {
        return Vec::new();
    }
    glob::scan(pattern, dir, glob::Options { dot: true, follow: true })
}

// The `name` a SKILL.md declares, else its directory name.
pub fn skill_name(file: &Path) -> String {
    let declared = fs::read_to_string(file).ok().and_then(|text| markdown::parse(&text).ok()).and_then(|md| md.data.get("name").and_then(Value::as_str).map(str::to_string));
    declared.unwrap_or_else(|| file.parent().and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
}

#[derive(Debug, Default)]
pub struct Visible {
    pub mcp_shared: HashSet<String>,
    pub mcp_rust: Map<String, Value>,
    pub agents: HashSet<String>,
    pub skills: HashSet<String>,
    pub rust: PathBuf,
}

impl Visible {
    pub fn load(paths: &Paths, home: &Path) -> Self {
        let rust = paths.rust_config();
        let dot = home.join(".opencode");
        let mut visible = Visible { rust: rust.clone(), ..Default::default() };
        let shared_files = ["config.json", "opencode.json", "opencode.jsonc"].iter().map(|n| paths.config.join(n)).chain(["opencode.json", "opencode.jsonc"].iter().map(|n| dot.join(n)));
        for file in shared_files {
            let Some(value) = read_json(&file) else { continue };
            visible.mcp_shared.extend(keys(value.get("mcp")));
            for group in ["agent", "mode"] {
                visible.agents.extend(keys(value.get(group)).map(|k| k.to_lowercase()));
            }
        }
        for name in ["opencode.json", "opencode.jsonc"] {
            if let Some(Value::Object(mcp)) = read_json(&rust.join(name)).and_then(|v| v.get("mcp").cloned()) {
                visible.mcp_rust.extend(mcp);
            }
        }
        visible.agents.extend(BUILTIN_AGENTS.iter().map(|s| s.to_string()));
        for dir in [&paths.config, &dot] {
            for file in files(dir, "{agent,agents}/**/*.md") {
                let relative = file.strip_prefix(dir).unwrap_or(&file).to_string_lossy().into_owned();
                visible.agents.insert(config::entry_name(&relative, &["agent/", "agents/"]).to_lowercase());
            }
        }
        let skill_roots = [(home.join(".claude"), "skills/**/SKILL.md"), (home.join(".agents"), "skills/**/SKILL.md"), (paths.config.clone(), "{skill,skills}/**/SKILL.md"), (dot, "{skill,skills}/**/SKILL.md")];
        for (dir, pattern) in skill_roots {
            visible.skills.extend(files(&dir, pattern).iter().map(|f| skill_name(f).to_lowercase()));
        }
        visible
    }

    pub fn agents_dir(&self) -> PathBuf {
        self.rust.join("agents")
    }

    pub fn skills_dir(&self) -> PathBuf {
        self.rust.join("skills")
    }

    pub fn mcp_conflict(&self, name: &str, entry: &Value) -> Option<String> {
        if self.mcp_shared.contains(name) {
            return Some("an MCP server of this name is already configured for opencode".into());
        }
        match self.mcp_rust.get(name) {
            Some(existing) if existing != entry => Some("an MCP server of this name is already in opencode-rs/opencode.jsonc".into()),
            _ => None,
        }
    }

    pub fn agent_conflict(&self, name: &str, file: &str, content: &str) -> Option<String> {
        let key = name.to_lowercase();
        if BUILTIN_AGENTS.contains(&key.as_str()) {
            return Some("built-in agent name".into());
        }
        if self.agents.contains(&key) {
            return Some("an agent of this name is already configured for opencode".into());
        }
        match fs::read_to_string(self.agents_dir().join(file)) {
            Ok(existing) if existing != content => Some("an agent of this name is already in opencode-rs/agents".into()),
            _ => None,
        }
    }

    pub fn skill_conflict(&self, name: &str, source: &Path, target: &str) -> Option<String> {
        if self.skills.contains(&name.to_lowercase()) {
            return Some("a skill of this name is already visible to the Rust build".into());
        }
        let existing = self.skills_dir().join(target).join("SKILL.md");
        match (fs::read(&existing), fs::read(source.join("SKILL.md"))) {
            (Ok(a), Ok(b)) if a != b => Some("a skill of this name is already in opencode-rs/skills".into()),
            _ => None,
        }
    }
}

// A file or directory name made from an arbitrary name.
pub fn safe_name(name: &str) -> String {
    let cleaned: String = name.trim().chars().map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' }).collect();
    let cleaned = cleaned.trim_matches(['-', '.']).to_string();
    if cleaned.is_empty() { "imported".into() } else { cleaned }
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let kind = fs::metadata(entry.path())?;
        if kind.is_dir() {
            copy_dir(&entry.path(), &to.join(&name))?;
        } else if kind.is_file() {
            fs::copy(entry.path(), to.join(&name))?;
        }
    }
    Ok(())
}

fn write_mcp(entries: &[(&'static str, String, Value)], paths: &Paths, report: &mut ImportReport) {
    let file = paths.rust_config().join("opencode.jsonc");
    let source = file.to_string_lossy().into_owned();
    let before = fs::read_to_string(&file).unwrap_or_else(|_| HEADER.to_string());
    let current = config::parse_jsonc(&before, &source).ok();
    let mut text = before.clone();
    let mut written = Vec::new();
    for (owner, name, entry) in entries {
        if current.as_ref().and_then(|c| c.get("mcp")).and_then(|m| m.get(name)) == Some(entry) {
            report.outcome(owner, Category::Mcp).unchanged += 1;
            continue;
        }
        match config::set_jsonc(&text, &["mcp", name], entry, &source) {
            Ok(next) => {
                text = next;
                written.push(*owner);
            }
            Err(error) => {
                report.outcome(owner, Category::Mcp).failed += 1;
                report.errors.push(format!("mcp {name}: {error}"));
            }
        }
    }
    if text == before {
        return;
    }
    let saved = config::parse_jsonc(&text, &source).map_err(|e| e.to_string()).and_then(|_| {
        fs::create_dir_all(paths.rust_config()).map_err(|e| e.to_string())?;
        fs::write(&file, &text).map_err(|e| e.to_string())
    });
    match saved {
        Ok(()) => {
            report.files.push(file.clone());
            for owner in written {
                report.outcome(owner, Category::Mcp).written += 1;
            }
        }
        Err(error) => {
            for owner in written {
                report.outcome(owner, Category::Mcp).failed += 1;
            }
            report.errors.push(format!("{}: {error}", file.display()));
        }
    }
}

pub fn apply_config(plan: &Plan, paths: &Paths, report: &mut ImportReport) {
    let rust = paths.rust_config();
    let mut servers = Vec::new();
    let mut written_skills: HashSet<String> = HashSet::new();
    for planned in &plan.items {
        let owner = planned.source;
        match &planned.item.payload {
            Payload::Mcp(entry) => servers.push((owner, planned.item.name.clone(), entry.clone())),
            Payload::Agent { file, content } => {
                let path = rust.join("agents").join(file);
                if fs::read_to_string(&path).is_ok_and(|existing| existing == *content) {
                    report.outcome(owner, Category::Agents).unchanged += 1;
                    continue;
                }
                let saved = fs::create_dir_all(rust.join("agents")).and_then(|()| fs::write(&path, content));
                match saved {
                    Ok(()) => {
                        report.outcome(owner, Category::Agents).written += 1;
                        report.files.push(path);
                    }
                    Err(error) => {
                        report.outcome(owner, Category::Agents).failed += 1;
                        report.errors.push(format!("{}: {error}", path.display()));
                    }
                }
            }
            Payload::Skill { dir, target } => {
                let to = rust.join("skills").join(target);
                if written_skills.contains(target) {
                    report.outcome(owner, Category::Skills).skipped += 1;
                    continue;
                }
                let same = matches!((fs::read(to.join("SKILL.md")), fs::read(dir.join("SKILL.md"))), (Ok(a), Ok(b)) if a == b);
                if same {
                    report.outcome(owner, Category::Skills).unchanged += 1;
                    continue;
                }
                match copy_dir(dir, &to) {
                    Ok(()) => {
                        report.outcome(owner, Category::Skills).written += 1;
                        report.files.push(to);
                        written_skills.insert(target.clone());
                    }
                    Err(error) => {
                        report.outcome(owner, Category::Skills).failed += 1;
                        report.errors.push(format!("{}: {error}", to.display()));
                    }
                }
            }
            Payload::Session(_) => {}
        }
    }
    if !servers.is_empty() {
        write_mcp(&servers, paths, report);
    }
}
