use opencode_core::agent::{Agents, Context};
use opencode_core::config::{LoadOptions, find_worktree, load_with};
use opencode_core::paths::Paths;
use opencode_core::{command, instruction, skill};
use std::path::{Path, PathBuf};

// Loads the upstream repository's own `.opencode` directory (read-only) through the whole pipeline.
#[test]
fn upstream_repo_config_loads() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../opencode-ts");
    let Ok(repo) = repo.canonicalize() else { return };
    let repo = PathBuf::from(repo.to_string_lossy().trim_start_matches(r"\\?\").to_string());
    if !repo.join(".opencode").is_dir() {
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let paths = Paths {
        home: scratch.path().join("home"),
        data: scratch.path().join("data"),
        cache: scratch.path().join("cache"),
        config: scratch.path().join("config"),
        state: scratch.path().join("state"),
        tmp: scratch.path().join("tmp"),
    };
    let managed = scratch.path().join("managed").to_string_lossy().into_owned();
    let env = |key: &str| (key == "OPENCODE_TEST_MANAGED_CONFIG_DIR").then(|| managed.clone());
    let cwd = repo.join("packages").join("opencode");
    let worktree = find_worktree(&cwd).unwrap_or_else(|| repo.clone());
    let opts = LoadOptions { cwd: cwd.clone(), worktree: Some(worktree.clone()) };

    let loaded = load_with(&paths, &opts, &env).unwrap();
    let info = loaded.info().unwrap();
    assert!(loaded.directories.contains(&repo.join(".opencode")));
    assert!(info.command.contains_key("commit"), "commands: {:?}", info.command.keys().collect::<Vec<_>>());
    assert!(info.agent.contains_key("triage"), "agents: {:?}", info.agent.keys().collect::<Vec<_>>());
    assert!(!info.command["commit"].template.is_empty());

    let discovered = skill::discover(&skill::DiscoverInput {
        home: &paths.home,
        cwd: &cwd,
        worktree: Some(&worktree),
        config_dirs: &loaded.directories,
        config: &info.skills,
        disable_external: false,
        disable_claude_code: false,
    });
    let skills = skill::Registry::load(&discovered);
    assert!(skills.errors.is_empty(), "{:?}", skills.errors);
    assert!(skills.skills.len() > 1);

    let ctx = Context { paths: &paths, worktree: &worktree, skill_dirs: &skills.dirs, reference_dirs: &[] };
    let agents = Agents::build(&info, &ctx);
    assert!(agents.get("triage").is_some());
    assert_eq!(agents.default_info().unwrap().name, "build");

    let all: Vec<skill::Skill> = skills.all().into_iter().cloned().collect();
    let commands = command::registry(&info, &worktree, Vec::new(), &all);
    assert!(commands.contains_key("init") && commands.contains_key("commit"));

    let ictx = instruction::Context {
        global_config: paths.config.clone(),
        home: paths.home.clone(),
        directory: cwd,
        worktree: Some(worktree.clone()),
        disable_project_config: false,
        disable_claude_code_prompt: false,
    };
    let system = instruction::system_paths(&ictx, &info.instructions);
    assert!(system.files.contains(&worktree.join("AGENTS.md")), "{:?}", system.files);
}
