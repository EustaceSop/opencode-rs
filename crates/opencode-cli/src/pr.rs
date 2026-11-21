// `opencode pr <number>` (`opencode/src/cli/cmd/pr.ts`).
use crate::ui::{self, fail};
use anyhow::Result;
use opencode_core::config;
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Stdio};

fn git(worktree: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).current_dir(worktree).stdin(Stdio::null()).output().ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub fn run(number: u64) -> Result<i32> {
    let Some(worktree) = config::find_worktree(&std::env::current_dir()?) else {
        return Err(fail("Could not find git repository. Please run this command from a git repository."));
    };
    let branch = format!("pr/{number}");
    ui::println(&format!("Fetching and checking out PR #{number}..."));
    let checkout = Command::new("gh").args(["pr", "checkout", &number.to_string(), "--branch", &branch, "--force"]).stdin(Stdio::null()).output();
    if !checkout.is_ok_and(|output| output.status.success()) {
        return Err(fail(format!("Failed to checkout PR #{number}. Make sure you have gh CLI installed and authenticated.")));
    }

    let view = Command::new("gh").args(["pr", "view", &number.to_string(), "--json", "headRepository,headRepositoryOwner,isCrossRepository,headRefName"]).stdin(Stdio::null()).output();
    let info = view.ok().filter(|output| output.status.success()).and_then(|output| serde_json::from_slice::<Value>(&output.stdout).ok());
    if let Some(info) = info
        && info["isCrossRepository"] == true
        && let (Some(owner), Some(name)) = (info["headRepositoryOwner"]["login"].as_str(), info["headRepository"]["name"].as_str())
    {
        let remotes = git(&worktree, &["remote"]).unwrap_or_default();
        if !remotes.trim().split('\n').any(|remote| remote == owner) {
            git(&worktree, &["remote", "add", owner, &format!("https://github.com/{owner}/{name}.git")]);
            ui::println(&format!("Added fork remote: {owner}"));
        }
        let head = info["headRefName"].as_str().unwrap_or_default();
        git(&worktree, &["branch", &format!("--set-upstream-to={owner}/{head}"), &branch]);
    }

    ui::println(&format!("Successfully checked out PR #{number} as branch '{branch}'"));
    ui::println("");
    ui::println("Starting opencode...");
    ui::println("");
    // The original runs `opencode` from PATH; this build runs itself so a PATH entry of the TypeScript build is not picked up.
    let status = Command::new(std::env::current_exe()?).status()?;
    match status.code() {
        Some(0) => Ok(0),
        code => anyhow::bail!("opencode exited with code {}", code.map_or_else(|| "null".to_string(), |code| code.to_string())),
    }
}
