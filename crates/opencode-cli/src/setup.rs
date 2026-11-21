// `opencode setup bash` (Rust build only): the one-time offer of a portable bash (MinGit) on Windows machines without
// one. The answer lives in `<state>/setup-rs.json`.
use crate::prompt;
use crate::ui::{self, fail};
use anyhow::Result;
use opencode_core::paths::Paths;
use opencode_tools::mingit::{self, Consent, Verified};
use std::io::{IsTerminal, Write};

pub struct Args {
    pub yes: bool,
    pub decline: bool,
}

// The one-line notice before `run` or the TUI starts, while nobody answered and no bash exists.
pub fn hint(paths: &Paths) {
    if mingit::should_offer(&paths.state, &paths.cache) {
        ui::println(mingit::HINT);
    }
}

// Redrawn in place, so only on a terminal; a redirected run gets the final line alone.
fn progress(done: u64, total: u64) {
    if total > 0 && std::io::stderr().is_terminal() {
        eprint!("\r  {:.1} / {:.1} MB", done as f64 / 1e6, total as f64 / 1e6);
        let _ = std::io::stderr().flush();
    }
}

pub async fn bash(paths: &Paths, args: Args) -> Result<i32> {
    if args.decline {
        mingit::set_consent(&paths.state, Some(Consent::Declined))?;
        ui::println("The portable bash will not be offered again; `opencode setup bash` still downloads it.");
        return Ok(0);
    }
    let installer = mingit::Installer::new(mingit::root_in(&paths.cache));
    if let Some(found) = installer.installed() {
        mingit::set_consent(&paths.state, Some(Consent::Accepted))?;
        ui::println(&format!("Portable bash {} is already installed: {}", found.version, found.shell.display()));
        return Ok(0);
    }
    if !cfg!(windows) {
        ui::println("The portable bash is for Windows; this system has its own shell.");
        return Ok(0);
    }
    ui::println(mingit::OFFER);
    ui::println(&format!("Source: {} (latest release, MinGit zip, sha256-checked)", mingit::RELEASES_PAGE));
    ui::println(&format!("Unpacked to: {}", installer.root.display()));
    if let Some(git_bash) = opencode_tools::shell::gitbash() {
        ui::println(&format!("Note: Git Bash is already installed ({git_bash}) and is used first."));
    }
    if !args.yes {
        if !prompt::interactive() {
            return Err(fail("Not a terminal: pass --yes to download, or --decline to stop the offer"));
        }
        if !prompt::confirm("Download it now?", false)? {
            ui::println("Not downloaded. Run `opencode setup bash` again any time, or `--decline` to stop the offer.");
            return Ok(0);
        }
    }
    mingit::set_consent(&paths.state, Some(Consent::Accepted))?;
    let outcome = installer.install(&progress).await;
    if std::io::stderr().is_terminal() {
        eprintln!();
    }
    let outcome = outcome.map_err(|e| fail(e.to_string()))?;
    let check = match outcome.verified {
        Some(Verified::Sha256) => "sha256 matched the release digest",
        Some(Verified::Size) => "the release lists no digest; only the size was checked",
        None => "already unpacked",
    };
    ui::println(&format!("Installed {} ({}; {check})", outcome.release.asset, outcome.release.version));
    ui::println(&format!("bash: {}", outcome.installed.shell.display()));
    Ok(0)
}
