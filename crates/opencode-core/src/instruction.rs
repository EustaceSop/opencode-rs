use crate::config::{LoadOptions, up};
use crate::flag::Flags;
use crate::glob;
use crate::paths::Paths;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone)]
pub struct Context {
    // `OPENCODE_CONFIG_DIR` when set, else the XDG config dir.
    pub global_config: PathBuf,
    pub home: PathBuf,
    pub directory: PathBuf,
    pub worktree: Option<PathBuf>,
    pub disable_project_config: bool,
    pub disable_claude_code_prompt: bool,
}

impl Context {
    pub fn new(paths: &Paths, opts: &LoadOptions, flags: &Flags) -> Self {
        Self {
            global_config: flags.config_dir.as_ref().map_or_else(|| paths.config.clone(), PathBuf::from),
            home: paths.home.clone(),
            directory: opts.cwd.clone(),
            worktree: opts.worktree.clone(),
            disable_project_config: flags.disable_project_config,
            disable_claude_code_prompt: flags.disable_claude_code_prompt,
        }
    }

    fn global_files(&self) -> Vec<PathBuf> {
        let mut files = vec![self.global_config.join("AGENTS.md")];
        if !self.disable_claude_code_prompt {
            files.push(self.home.join(".claude").join("CLAUDE.md"));
        }
        files
    }

    // AGENTS.md, CLAUDE.md (unless disabled), then the deprecated CONTEXT.md.
    pub fn instruction_files(&self) -> Vec<&'static str> {
        let mut names = vec!["AGENTS.md"];
        if !self.disable_claude_code_prompt {
            names.push("CLAUDE.md");
        }
        names.push("CONTEXT.md");
        names
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SystemPaths {
    pub files: Vec<PathBuf>,
    // http(s) entries of `instructions`: remote, not fetched here.
    pub remote: Vec<String>,
}

fn is_remote(instruction: &str) -> bool {
    instruction.starts_with("https://") || instruction.starts_with("http://")
}

fn glob_up(pattern: &str, start: &Path, stop: Option<&Path>) -> Vec<PathBuf> {
    up(start, stop)
        .iter()
        .flat_map(|dir| glob::scan(pattern, dir, glob::Options { dot: true, follow: false }))
        .collect()
}

// One global file, the first project file name with hits (all ancestors), then config `instructions`.
pub fn system_paths(ctx: &Context, instructions: &[String]) -> SystemPaths {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut add = |file: PathBuf| {
        if !files.contains(&file) {
            files.push(file);
        }
    };
    if let Some(file) = ctx.global_files().into_iter().find(|f| f.exists()) {
        add(file);
    }
    if !ctx.disable_project_config {
        for name in ctx.instruction_files() {
            let matches: Vec<PathBuf> = up(&ctx.directory, ctx.worktree.as_deref())
                .into_iter()
                .map(|dir| dir.join(name))
                .filter(|f| f.exists())
                .collect();
            if !matches.is_empty() {
                matches.into_iter().for_each(&mut add);
                break;
            }
        }
    }
    for raw in instructions.iter().filter(|raw| !is_remote(raw)) {
        let instruction = match raw.strip_prefix("~/") {
            Some(rest) => ctx.home.join(rest),
            None => PathBuf::from(raw),
        };
        let matches = if instruction.is_absolute() {
            match (instruction.parent(), instruction.file_name()) {
                (Some(dir), Some(name)) => glob::scan(&name.to_string_lossy(), dir, glob::Options::default()),
                _ => Vec::new(),
            }
        } else if ctx.disable_project_config {
            glob_up(raw, &ctx.global_config, Some(&ctx.global_config))
        } else {
            glob_up(raw, &ctx.directory, ctx.worktree.as_deref())
        };
        matches.into_iter().for_each(&mut add);
    }
    SystemPaths { files, remote: instructions.iter().filter(|raw| is_remote(raw)).cloned().collect() }
}

pub fn format(source: &str, content: &str) -> String {
    format!("Instructions from: {source}\n{content}")
}

// Reads every file; unreadable or empty files are omitted.
pub fn load(files: &[PathBuf]) -> Vec<String> {
    files
        .iter()
        .filter_map(|file| {
            let content = fs::read_to_string(file).unwrap_or_default();
            (!content.is_empty()).then(|| format(&file.to_string_lossy(), &content))
        })
        .collect()
}

// First existing instruction file name in `dir`.
pub fn find(ctx: &Context, dir: &Path) -> Option<PathBuf> {
    ctx.instruction_files().into_iter().map(|name| dir.join(name)).find(|f| f.exists())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nearby {
    pub filepath: PathBuf,
    pub content: String,
}

// Per-message record of instruction files already attached by the read tool.
#[derive(Debug, Default)]
pub struct Claims {
    claims: Mutex<HashMap<String, HashSet<PathBuf>>>,
}

impl Claims {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&self, message_id: &str) {
        self.claims.lock().unwrap_or_else(|e| e.into_inner()).remove(message_id);
    }

    fn claim(&self, message_id: &str, file: &Path) -> bool {
        let mut claims = self.claims.lock().unwrap_or_else(|e| e.into_inner());
        claims.entry(message_id.to_string()).or_default().insert(file.to_path_buf())
    }

    // Instruction files between the read file's directory and the instance directory (exclusive)
    // that are not system paths, not already loaded in history, and not yet claimed for this message.
    pub fn resolve(
        &self,
        ctx: &Context,
        system: &[PathBuf],
        loaded: &HashSet<PathBuf>,
        filepath: &Path,
        message_id: &str,
    ) -> Vec<Nearby> {
        let root = ctx.directory.to_string_lossy().into_owned();
        let mut results = Vec::new();
        let mut current = filepath.parent();
        while let Some(dir) = current {
            if !dir.to_string_lossy().starts_with(&root) || dir == ctx.directory {
                break;
            }
            current = dir.parent();
            let Some(found) = find(ctx, dir) else { continue };
            if found == filepath || system.contains(&found) || loaded.contains(&found) {
                continue;
            }
            if !self.claim(message_id, &found) {
                continue;
            }
            let content = fs::read_to_string(&found).unwrap_or_default();
            if !content.is_empty() {
                results.push(Nearby { content: format(&found.to_string_lossy(), &content), filepath: found });
            }
        }
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(file: PathBuf, text: &str) {
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, text).unwrap();
    }

    struct Fixture {
        _root: tempfile::TempDir,
        ctx: Context,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("repo");
        let ctx = Context {
            global_config: root.path().join("config"),
            home: root.path().join("home"),
            directory: work.join("pkg").join("sub"),
            worktree: Some(work),
            disable_project_config: false,
            disable_claude_code_prompt: false,
        };
        fs::create_dir_all(&ctx.directory).unwrap();
        fs::create_dir_all(&ctx.global_config).unwrap();
        Fixture { _root: root, ctx }
    }

    fn work(f: &Fixture) -> PathBuf {
        f.ctx.worktree.clone().unwrap()
    }

    #[test]
    fn global_file_first_existing_only() {
        let mut f = fixture();
        write(f.ctx.home.join(".claude/CLAUDE.md"), "claude");
        assert_eq!(system_paths(&f.ctx, &[]).files, [f.ctx.home.join(".claude").join("CLAUDE.md")]);
        write(f.ctx.global_config.join("AGENTS.md"), "agents");
        assert_eq!(system_paths(&f.ctx, &[]).files, [f.ctx.global_config.join("AGENTS.md")]);
        fs::remove_file(f.ctx.global_config.join("AGENTS.md")).unwrap();
        f.ctx.disable_claude_code_prompt = true;
        assert!(system_paths(&f.ctx, &[]).files.is_empty());
    }

    #[test]
    fn project_first_name_with_hits_wins_across_ancestors() {
        let mut f = fixture();
        let root = work(&f);
        write(root.join("CLAUDE.md"), "c-root");
        write(f.ctx.directory.join("CLAUDE.md"), "c-near");
        write(root.join("CONTEXT.md"), "ctx");
        write(root.parent().unwrap().join("AGENTS.md"), "above worktree");
        assert_eq!(system_paths(&f.ctx, &[]).files, [f.ctx.directory.join("CLAUDE.md"), root.join("CLAUDE.md")]);

        write(root.join("pkg").join("AGENTS.md"), "agents");
        assert_eq!(system_paths(&f.ctx, &[]).files, [root.join("pkg").join("AGENTS.md")]);

        fs::remove_file(root.join("pkg").join("AGENTS.md")).unwrap();
        f.ctx.disable_claude_code_prompt = true;
        assert_eq!(system_paths(&f.ctx, &[]).files, [root.join("CONTEXT.md")]);
        f.ctx.disable_project_config = true;
        assert!(system_paths(&f.ctx, &[]).files.is_empty());
    }

    #[test]
    fn config_instructions() {
        let mut f = fixture();
        let root = work(&f);
        write(root.join("docs/a.md"), "a");
        write(root.join("docs/b.md"), "b");
        write(root.join("docs/.hidden.md"), "h");
        write(root.join("pkg/docs/a.md"), "pkg-a");
        write(root.join("STYLE.md"), "style");
        write(f.ctx.home.join("notes/rules.md"), "rules");
        write(f.ctx.home.join("notes/.dot.md"), "dot");
        write(f.ctx.global_config.join("docs/g.md"), "g");
        let abs = root.join("docs").join("*.md").to_string_lossy().into_owned();
        let instructions: Vec<String> = [
            "docs/*.md",
            "https://example.com/rules.md",
            "STYLE.md",
            "~/notes/*.md",
            "http://insecure.example/x",
            "missing.md",
            abs.as_str(),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let paths = system_paths(&f.ctx, &instructions);
        assert_eq!(
            paths.files,
            [
                root.join("pkg").join("docs").join("a.md"),
                root.join("docs").join(".hidden.md"),
                root.join("docs").join("a.md"),
                root.join("docs").join("b.md"),
                root.join("STYLE.md"),
                f.ctx.home.join("notes").join("rules.md"),
            ]
        );
        assert_eq!(paths.remote, ["https://example.com/rules.md", "http://insecure.example/x"]);

        f.ctx.disable_project_config = true;
        let paths = system_paths(&f.ctx, &["docs/*.md".to_string()]);
        assert_eq!(paths.files, [f.ctx.global_config.join("docs").join("g.md")]);
    }

    #[test]
    fn load_formats_and_skips_empty() {
        let f = fixture();
        let a = f.ctx.directory.join("a.md");
        let empty = f.ctx.directory.join("empty.md");
        write(a.clone(), "hello");
        write(empty.clone(), "");
        let out = load(&[a.clone(), empty, f.ctx.directory.join("gone.md")]);
        assert_eq!(out, [format!("Instructions from: {}\nhello", a.display())]);
    }

    #[test]
    fn nearby_resolution_and_claims() {
        let f = fixture();
        let dir = &f.ctx.directory;
        write(dir.join("AGENTS.md"), "root level");
        write(dir.join("a/AGENTS.md"), "a rules");
        write(dir.join("a/b/CLAUDE.md"), "b rules");
        write(dir.join("a/b/c/file.rs"), "code");
        let target = dir.join("a/b/c/file.rs");
        let claims = Claims::new();
        let none = HashSet::new();
        let found = claims.resolve(&f.ctx, &[], &none, &target, "msg1");
        assert_eq!(
            found.iter().map(|n| n.filepath.clone()).collect::<Vec<_>>(),
            [dir.join("a/b").join("CLAUDE.md"), dir.join("a").join("AGENTS.md")]
        );
        assert!(found[0].content.ends_with("\nb rules"));
        assert!(claims.resolve(&f.ctx, &[], &none, &target, "msg1").is_empty());
        assert_eq!(claims.resolve(&f.ctx, &[], &none, &target, "msg2").len(), 2);
        claims.clear("msg1");
        let system = [dir.join("a").join("AGENTS.md")];
        let loaded: HashSet<PathBuf> = [dir.join("a/b").join("CLAUDE.md")].into_iter().collect();
        assert!(claims.resolve(&f.ctx, &system, &loaded, &target, "msg1").is_empty());
        let own = dir.join("a").join("AGENTS.md");
        assert!(claims.resolve(&f.ctx, &[], &none, &own, "msg3").is_empty());
    }
}
