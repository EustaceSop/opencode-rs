use crate::config::{SkillsConfig, markdown, up};
use crate::glob;
use crate::permission::{Action, Rule, evaluate};
use indexmap::IndexMap;
use serde::Serialize;
use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};

pub const BUILTIN_LOCATION: &str = "<built-in>";
pub const CUSTOMIZE_OPENCODE_NAME: &str = "customize-opencode";
pub const CUSTOMIZE_OPENCODE_DESCRIPTION: &str = "Use ONLY when the user is editing or creating opencode's own configuration: opencode.json, opencode.jsonc, files under .opencode/, or files under ~/.config/opencode/. Also use when creating or fixing opencode agents, subagents, skills, plugins, MCP servers, or permission rules. Do not use for the user's own application code, or for any project that is not configuring opencode itself.";
pub const CUSTOMIZE_OPENCODE_BODY: &str =
    include_str!("../../../../opencode-ts/packages/core/src/plugin/skill/customize-opencode.md");

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skill {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub location: String,
    pub content: String,
}

pub struct DiscoverInput<'a> {
    pub home: &'a Path,
    pub cwd: &'a Path,
    pub worktree: Option<&'a Path>,
    pub config_dirs: &'a [PathBuf],
    pub config: &'a SkillsConfig,
    pub disable_external: bool,
    pub disable_claude_code: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Discovered {
    pub files: Vec<PathBuf>,
    pub dirs: Vec<PathBuf>,
    // `skills.urls` entries; pulling them is left to the caller.
    pub remote: Vec<String>,
    pub missing_paths: Vec<PathBuf>,
}

impl Discovered {
    fn scan(&mut self, root: &Path, pattern: &str, dot: bool) {
        for file in glob::scan(pattern, root, glob::Options { dot, follow: true }) {
            if let Some(dir) = file.parent().map(Path::to_path_buf)
                && !self.dirs.contains(&dir)
            {
                self.dirs.push(dir);
            }
            if !self.files.contains(&file) {
                self.files.push(file);
            }
        }
    }

    // Scans a directory obtained by other means (e.g. a pulled `skills.urls` cache dir).
    pub fn scan_dir(&mut self, dir: &Path) {
        self.scan(dir, "**/SKILL.md", false);
    }
}

// Order: ~/.claude, ~/.agents, project .claude/.agents (cwd upward), config dirs, `skills.paths`.
pub fn discover(input: &DiscoverInput) -> Discovered {
    let mut state = Discovered::default();
    if !input.disable_external {
        let mut external = Vec::new();
        if !input.disable_claude_code {
            external.push(".claude");
        }
        external.push(".agents");
        for dir in &external {
            let root = input.home.join(dir);
            if root.is_dir() {
                state.scan(&root, "skills/**/SKILL.md", true);
            }
        }
        for dir in up(input.cwd, input.worktree) {
            for name in &external {
                let root = dir.join(name);
                if root.exists() {
                    state.scan(&root, "skills/**/SKILL.md", true);
                }
            }
        }
    }
    for dir in input.config_dirs {
        state.scan(dir, "{skill,skills}/**/SKILL.md", false);
    }
    for item in &input.config.paths {
        let expanded = match item.strip_prefix("~/") {
            Some(rest) => input.home.join(rest),
            None => PathBuf::from(item),
        };
        let dir = if expanded.is_absolute() { expanded } else { input.cwd.join(expanded) };
        if dir.is_dir() {
            state.scan_dir(&dir);
        } else {
            state.missing_paths.push(dir);
        }
    }
    state.remote = input.config.urls.clone();
    state
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Registry {
    pub skills: IndexMap<String, Skill>,
    pub dirs: Vec<PathBuf>,
    // Messages for files whose frontmatter could not be parsed (`session.error` in the original).
    pub errors: Vec<String>,
}

// Approximates `String.prototype.localeCompare` ordering: case-insensitive first, then exact.
fn locale_cmp(a: &str, b: &str) -> Ordering {
    a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| b.cmp(a))
}

impl Registry {
    // Built-in skill first so a disk skill with the same name overrides it; later files win.
    pub fn load(discovered: &Discovered) -> Self {
        let mut registry = Self { dirs: discovered.dirs.clone(), ..Self::default() };
        registry.skills.insert(
            CUSTOMIZE_OPENCODE_NAME.to_string(),
            Skill {
                name: CUSTOMIZE_OPENCODE_NAME.to_string(),
                description: Some(CUSTOMIZE_OPENCODE_DESCRIPTION.to_string()),
                location: BUILTIN_LOCATION.to_string(),
                content: CUSTOMIZE_OPENCODE_BODY.to_string(),
            },
        );
        for file in &discovered.files {
            let location = file.to_string_lossy().into_owned();
            let Ok(text) = fs::read_to_string(file) else {
                registry.errors.push(format!("Failed to parse skill {location}"));
                continue;
            };
            let md = match markdown::parse(&text) {
                Ok(md) => md,
                Err(error) => {
                    registry.errors.push(format!("{location}: Failed to parse YAML frontmatter: {error}"));
                    continue;
                }
            };
            let Some(name) = md.data.get("name").and_then(|v| v.as_str()) else { continue };
            let description = match md.data.get("description") {
                None => None,
                Some(value) => match value.as_str() {
                    Some(text) => Some(text.to_string()),
                    None => continue,
                },
            };
            registry.skills.insert(
                name.to_string(),
                Skill { name: name.to_string(), description, location, content: md.content },
            );
        }
        registry
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    pub fn all(&self) -> Vec<&Skill> {
        self.skills.values().collect()
    }

    pub fn not_found_message(&self, name: &str) -> String {
        let mut available: Vec<&str> = self.skills.keys().map(String::as_str).collect();
        available.sort();
        let list = if available.is_empty() { "none".to_string() } else { available.join(", ") };
        format!("Skill \"{name}\" not found. Available skills: {list}")
    }

    // Sorted by name; with an agent ruleset, skills denied by `skill` permission are dropped.
    pub fn available(&self, ruleset: Option<&[Rule]>) -> Vec<&Skill> {
        let mut list = self.all();
        list.sort_by(|a, b| locale_cmp(&a.name, &b.name));
        if let Some(ruleset) = ruleset {
            list.retain(|skill| evaluate("skill", &skill.name, &[ruleset]).action != Action::Deny);
        }
        list
    }
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub fn fmt(list: &[&Skill], verbose: bool) -> String {
    let mut described: Vec<(&Skill, &str)> =
        list.iter().filter_map(|skill| skill.description.as_deref().map(|d| (*skill, d))).collect();
    if described.is_empty() {
        return "No skills are currently available.".to_string();
    }
    described.sort_by(|a, b| locale_cmp(&a.0.name, &b.0.name));
    let mut lines: Vec<String> = Vec::new();
    if verbose {
        lines.push("<available_skills>".into());
        for (skill, description) in described {
            lines.push("  <skill>".into());
            lines.push(format!("    <name>{}</name>", skill.name));
            lines.push(format!("    <description>{description}</description>"));
            lines.push(format!("    <location>{}</location>", escape_html(&skill.location)));
            lines.push("  </skill>".into());
        }
        lines.push("</available_skills>".into());
    } else {
        lines.push("## Available Skills".into());
        for (skill, description) in described {
            lines.push(format!("- **{}**: {description}", skill.name));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(file: PathBuf, text: &str) {
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, text).unwrap();
    }

    fn skill(name: &str, description: &str) -> String {
        format!("---\nname: {name}\ndescription: {description}\n---\n# {name}\nbody\n")
    }

    struct Fixture {
        _root: tempfile::TempDir,
        home: PathBuf,
        work: PathBuf,
        cwd: PathBuf,
        config: PathBuf,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let work = root.path().join("repo");
        let cwd = work.join("pkg");
        let config = root.path().join("config");
        write(home.join(".claude/skills/a/SKILL.md"), &skill("home-claude", "hc"));
        write(home.join(".agents/skills/.hidden/b/SKILL.md"), &skill("home-agents", "ha"));
        write(cwd.join(".claude/skills/c/SKILL.md"), &skill("near", "n"));
        write(work.join(".agents/skills/d/SKILL.md"), &skill("far", "f"));
        write(work.join(".claude/skills/dup/SKILL.md"), &skill("near", "overridden later"));
        write(config.join("skill/e/SKILL.md"), &skill("cfg-one", "c1"));
        write(config.join("skills/deep/f/SKILL.md"), &skill("cfg-two", "c2"));
        write(config.join("skills/.dot/g/SKILL.md"), &skill("cfg-hidden", "no"));
        write(work.join("extra/h/SKILL.md"), &skill("extra", "e"));
        write(home.join("more/i/SKILL.md"), &skill("tilde", "t"));
        Fixture { _root: root, home, work, cwd, config }
    }

    fn run(f: &Fixture, disable_external: bool, disable_claude_code: bool) -> (Discovered, Registry) {
        let config = SkillsConfig {
            paths: vec!["../extra".into(), "~/more".into(), "missing".into()],
            urls: vec!["https://example.com/skills".into()],
        };
        let dirs = [f.config.clone()];
        let discovered = discover(&DiscoverInput {
            home: &f.home,
            cwd: &f.cwd,
            worktree: Some(&f.work),
            config_dirs: &dirs,
            config: &config,
            disable_external,
            disable_claude_code,
        });
        let registry = Registry::load(&discovered);
        (discovered, registry)
    }

    #[test]
    fn discovery_order_and_overrides() {
        let f = fixture();
        let (discovered, registry) = run(&f, false, false);
        let names: Vec<&str> = registry.skills.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            [CUSTOMIZE_OPENCODE_NAME, "home-claude", "home-agents", "near", "far", "cfg-one", "cfg-two", "extra", "tilde"]
        );
        assert_eq!(registry.get("near").unwrap().description.as_deref(), Some("overridden later"));
        assert_eq!(registry.get("near").unwrap().content, "# near\nbody\n");
        assert_eq!(discovered.remote, ["https://example.com/skills"]);
        assert_eq!(discovered.missing_paths, [f.cwd.join("missing")]);
        assert_eq!(discovered.files.len(), 9);
        assert!(discovered.dirs.contains(&f.home.join(".claude/skills/a")));
        assert!(registry.errors.is_empty());
    }

    #[test]
    fn external_flags() {
        let f = fixture();
        let (_, registry) = run(&f, false, true);
        assert!(registry.get("home-claude").is_none() && registry.get("near").is_none());
        assert!(registry.get("home-agents").is_some() && registry.get("far").is_some());
        let (_, registry) = run(&f, true, false);
        let names: Vec<&str> = registry.skills.keys().map(String::as_str).collect();
        assert_eq!(names, [CUSTOMIZE_OPENCODE_NAME, "cfg-one", "cfg-two", "extra", "tilde"]);
    }

    #[test]
    fn invalid_frontmatter_handling() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path().join("a/SKILL.md"), "---\nname: [broken\n---\nx");
        write(dir.path().join("b/SKILL.md"), "---\ndescription: no name\n---\nx");
        write(dir.path().join("c/SKILL.md"), "---\nname: c\ndescription: 5\n---\nx");
        write(dir.path().join("d/SKILL.md"), "---\nname: d\n---\nx");
        write(dir.path().join("e/SKILL.md"), &skill(CUSTOMIZE_OPENCODE_NAME, "mine"));
        let mut discovered = Discovered::default();
        discovered.scan_dir(dir.path());
        let registry = Registry::load(&discovered);
        assert_eq!(registry.errors.len(), 1);
        assert!(registry.errors[0].contains("Failed to parse YAML frontmatter"));
        assert_eq!(registry.skills.len(), 2);
        assert_eq!(registry.get("d").unwrap().description, None);
        assert_eq!(registry.get(CUSTOMIZE_OPENCODE_NAME).unwrap().description.as_deref(), Some("mine"));
        assert_eq!(registry.dirs.len(), 5);
        assert_eq!(registry.not_found_message("zz"), "Skill \"zz\" not found. Available skills: customize-opencode, d");
    }

    #[test]
    fn availability_and_formatting() {
        let mut registry = Registry::default();
        for (name, description) in [("zeta", Some("z")), ("Alpha", Some("a <b>")), ("beta", None)] {
            registry.skills.insert(
                name.into(),
                Skill { name: name.into(), description: description.map(Into::into), location: format!("/s/{name}/'q'&.md"), content: String::new() },
            );
        }
        let rules = [Rule::new("skill", "*", Action::Allow), Rule::new("skill", "z*", Action::Deny)];
        let names = |list: Vec<&Skill>| list.iter().map(|s| s.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(registry.available(None)), ["Alpha", "beta", "zeta"]);
        assert_eq!(names(registry.available(Some(&rules))), ["Alpha", "beta"]);
        assert_eq!(
            fmt(&registry.available(Some(&rules)), true),
            "<available_skills>\n  <skill>\n    <name>Alpha</name>\n    <description>a <b></description>\n    <location>/s/Alpha/&#39;q&#39;&amp;.md</location>\n  </skill>\n</available_skills>"
        );
        assert_eq!(fmt(&registry.all(), false), "## Available Skills\n- **Alpha**: a <b>\n- **zeta**: z");
        assert_eq!(fmt(&[], true), "No skills are currently available.");
    }
}
