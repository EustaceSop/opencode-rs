//! Shell selection (`core/src/shell.ts`).

use std::path::Path;

use crate::util::windows_path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Bash,
    Pwsh,
    Powershell,
    Cmd,
}

/// `ShellID.toKind`: anything that is not pwsh/powershell/cmd counts as bash.
pub fn kind(name: &str) -> Kind {
    match name {
        "pwsh" => Kind::Pwsh,
        "powershell" => Kind::Powershell,
        "cmd" => Kind::Cmd,
        _ => Kind::Bash,
    }
}

fn which(cmd: &str) -> Option<String> {
    crate::ripgrep::which(cmd).map(|p| p.to_string_lossy().into_owned())
}

/// `Shell.name`: lowercased stem on win32, lowercased basename elsewhere.
pub fn name(file: &str) -> String {
    if cfg!(windows) {
        let converted = windows_path(file);
        let base = converted.rsplit(['/', '\\']).next().unwrap_or("");
        let stem = match base.rfind('.') {
            Some(idx) if idx > 0 => &base[..idx],
            _ => base,
        };
        stem.to_lowercase()
    } else {
        file.rsplit('/').next().unwrap_or("").to_lowercase()
    }
}

pub fn ps(file: &str) -> bool {
    matches!(name(file).as_str(), "powershell" | "pwsh")
}

pub fn posix(file: &str) -> bool {
    matches!(name(file).as_str(), "bash" | "dash" | "ksh" | "sh" | "zsh")
}

fn denied(file: &str) -> bool {
    matches!(name(file).as_str(), "fish" | "nu")
}

/// `Shell.gitbash`: `OPENCODE_GIT_BASH_PATH`, else `<git>/../../bin/bash.exe`.
pub fn gitbash() -> Option<String> {
    if !cfg!(windows) {
        return None;
    }
    if let Some(path) = std::env::var("OPENCODE_GIT_BASH_PATH")
        .ok()
        .filter(|p| !p.is_empty())
    {
        return Some(path);
    }
    let git = which("git")?;
    let file = crate::util::normalize(
        &Path::new(&git)
            .join("..")
            .join("..")
            .join("bin")
            .join("bash.exe"),
    );
    std::fs::metadata(&file)
        .ok()
        .filter(|m| m.len() > 0)
        .map(|_| file.to_string_lossy().into_owned())
}

fn has_dir(file: &str) -> bool {
    file.contains('/') || file.contains('\\')
}

fn full(file: &str) -> String {
    if !cfg!(windows) {
        return file.to_string();
    }
    let shell = windows_path(file);
    if has_dir(&shell) {
        if shell.starts_with('/') && name(&shell) == "bash" {
            return gitbash().unwrap_or(shell);
        }
        return shell;
    }
    if name(&shell) == "bash" {
        return gitbash().or_else(managed).or_else(|| which(&shell)).unwrap_or(shell);
    }
    which(&shell).unwrap_or(shell)
}

/// Rust build, "agent" tool profile: the shell tool's exposed name for this shell.
pub fn tool_name(file: &str) -> &'static str {
    match kind(&name(file)) {
        Kind::Pwsh | Kind::Powershell => "PowerShell",
        Kind::Cmd => "Cmd",
        Kind::Bash => "Bash",
    }
}

/// Rust build, "agent" tool profile: how the shell is named in the shell tool's description.
pub fn label(file: &str) -> String {
    let short = name(file);
    match short.as_str() {
        "pwsh" => "PowerShell 7".to_string(),
        "powershell" => "Windows PowerShell 5.1".to_string(),
        "cmd" => "cmd.exe".to_string(),
        _ if cfg!(windows) && Path::new(file).starts_with(crate::mingit::root()) => "bash (portable Git for Windows MinGit)".to_string(),
        "bash" if cfg!(windows) => "Git Bash".to_string(),
        _ => short,
    }
}

const PS51_NOTES: &str = "# Windows PowerShell 5.1 notes
- This is Windows PowerShell 5.1 (`powershell.exe`), not PowerShell 7. It has no `&&` or `||`: chain dependent commands with `cmd1; if ($?) { cmd2 }`, and run a fallback with `cmd1; if (-not $?) { cmd2 }`.
- Also missing in 5.1: the ternary `a ? b : c`, `??`, `?.`, `ForEach-Object -Parallel` and `ConvertFrom-Json -AsHashtable`. Use `if`/`else` instead.
- `>`, `Out-File` and `Set-Content` do not write UTF-8 by default in 5.1; pass `-Encoding utf8` when another program reads the file.
- Use double quotes for interpolated strings (`\"Hello $name\"`), single quotes for verbatim strings; escape with the backtick.
- Prefer full cmdlet names (`Get-ChildItem`, `Remove-Item`, `New-Item`) over aliases. Call a native program whose path has spaces with `& \"C:\\path\\to\\tool.exe\" args`.
- Output of native programs is decoded as UTF-8.";

const PWSH_NOTES: &str = "# PowerShell 7 notes
- This is PowerShell 7 (`pwsh`). It supports `&&` and `||`, the ternary `a ? b : c`, `??` and `?.`.
- Windows PowerShell 5.1 (`powershell.exe`) has none of these; scripts that must also run there chain with `cmd1; if ($?) { cmd2 }`.
- Files written by `>`, `Out-File` and `Set-Content` are UTF-8 without BOM.
- Use double quotes for interpolated strings (`\"Hello $name\"`), single quotes for verbatim strings; escape with the backtick.
- Prefer full cmdlet names (`Get-ChildItem`, `Remove-Item`, `New-Item`) over aliases. Call a native program whose path has spaces with `& \"path/to/tool\" args`.";

const CMD_NOTES: &str = "# cmd.exe notes
- Chain dependent commands with `&&`, fallbacks with `||`.
- Use double quotes for paths with spaces, `%VAR%` for environment variables, `if exist` for existence checks and `call` to run batch files.";

/// Rust build, "agent" tool profile: shell notes appended to the shell tool's description.
pub fn notes(file: &str) -> &'static str {
    match name(file).as_str() {
        "powershell" => PS51_NOTES,
        "pwsh" => PWSH_NOTES,
        "cmd" => CMD_NOTES,
        _ if cfg!(windows) => crate::bash_prompt::GIT_BASH_NOTES.trim_end(),
        _ => "",
    }
}

/// The managed MinGit bash, when one was downloaded before (Rust build only).
pub fn managed() -> Option<String> {
    crate::mingit::installed().map(|found| found.shell.to_string_lossy().into_owned())
}

/// Extra environment for a shell process: a MinGit-style `usr\bin\{bash,sh}.exe` needs `usr\bin` and `cmd` on PATH.
pub fn env(file: &str) -> Vec<(String, String)> {
    if !cfg!(windows) {
        return Vec::new();
    }
    crate::mingit::path_env(Path::new(file)).map(|path| ("PATH".to_string(), path)).into_iter().collect()
}

fn rooted(file: &str) -> bool {
    let converted = windows_path(file);
    Path::new(&converted).is_absolute() || converted.starts_with('/') || converted.starts_with('\\')
}

fn resolve(file: &str) -> Option<String> {
    let shell = full(file);
    if rooted(&shell) {
        return std::fs::metadata(&shell)
            .is_ok_and(|m| m.is_file())
            .then_some(shell);
    }
    which(&shell)
}

/// Win32 candidates, most preferred first. Unlike the original (pwsh, powershell, Git Bash), the Rust build orders them
/// Git Bash, PowerShell 7, the managed MinGit bash, Windows PowerShell 5.1, cmd; config `shell` still wins.
fn win_order(git_bash: Option<String>, pwsh: Option<String>, managed: Option<String>, powershell: Option<String>, comspec: String) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in [git_bash, pwsh, managed, powershell, Some(comspec)].into_iter().flatten() {
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

fn win() -> Vec<String> {
    let comspec = std::env::var("COMSPEC")
        .ok()
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "cmd.exe".to_string());
    let mut out: Vec<String> = Vec::new();
    for item in win_order(gitbash(), which("pwsh"), managed(), which("powershell"), comspec) {
        let item = full(&item);
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

fn unix() -> Vec<String> {
    let text = std::fs::read_to_string("/etc/shells").unwrap_or_default();
    if text.is_empty() {
        return ["/bin/bash", "/bin/zsh", "/bin/sh"].map(str::to_string).to_vec();
    }
    let mut out: Vec<String> = Vec::new();
    for line in text.split('\n').filter(|line| !line.trim().is_empty() && !line.starts_with('#')) {
        if !out.iter().any(|known| known == line) {
            out.push(line.to_string());
        }
    }
    out
}

/// One entry of `Shell.list`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Item {
    pub path: String,
    pub name: String,
    pub acceptable: bool,
}

/// `Shell.info`: the full path, the bare name when that alone resolves, and whether the agent may use it.
pub fn info(file: &str) -> Item {
    let item = full(file);
    let short = name(&item);
    Item { name: if resolve(&short).is_some() { short } else { item.clone() }, acceptable: !denied(&item), path: item }
}

/// `Shell.list`: the machine's shells that resolve (`/etc/shells`, else bash/zsh/sh; on win32 Git Bash, pwsh,
/// powershell, `COMSPEC`).
pub fn list() -> Vec<Item> {
    let shells = if cfg!(windows) { win() } else { unix() };
    shells.iter().filter(|shell| resolve(shell).is_some()).map(|shell| info(shell)).collect()
}

fn fallback() -> String {
    if cfg!(target_os = "macos") {
        return "/bin/zsh".to_string();
    }
    which("bash").unwrap_or_else(|| "/bin/sh".to_string())
}

/// `Shell.acceptable(cfg.shell)`: configured shell, else `$SHELL`, skipping fish/nu, else platform default.
pub fn acceptable(config_shell: Option<&str>) -> String {
    let env_shell = std::env::var("SHELL").ok();
    let candidate = config_shell
        .filter(|s| !s.is_empty())
        .or(env_shell.as_deref().filter(|s| !s.is_empty()));
    if let Some(file) = candidate
        && !denied(file)
        && let Some(shell) = resolve(file)
    {
        return shell;
    }
    if cfg!(windows) {
        win()
            .into_iter()
            .next()
            .unwrap_or_else(|| "cmd.exe".to_string())
    } else {
        fallback()
    }
}

/// `Shell.preferred(cfg.shell)`: configured shell, else `$SHELL`, else platform default (fish/nu allowed).
pub fn preferred(config_shell: Option<&str>) -> String {
    let env_shell = std::env::var("SHELL").ok();
    let candidate = config_shell
        .filter(|s| !s.is_empty())
        .or(env_shell.as_deref().filter(|s| !s.is_empty()));
    if let Some(shell) = candidate.and_then(resolve) {
        return shell;
    }
    if cfg!(windows) {
        win()
            .into_iter()
            .next()
            .unwrap_or_else(|| "cmd.exe".to_string())
    } else {
        fallback()
    }
}

/// `Shell.args(file, command, cwd)`: argv that runs `command` the way a login shell of that kind would.
pub fn args(file: &str, command: &str, cwd: &str) -> Vec<String> {
    let quoted = serde_json::Value::String(command.to_string()).to_string();
    let login = |rc: &str| {
        vec![
            "-l".to_string(),
            "-c".to_string(),
            format!("\n        {rc}\n        cd -- \"$1\"\n        eval {quoted}\n      "),
            "opencode".to_string(),
            cwd.to_string(),
        ]
    };
    match name(file).as_str() {
        "nu" | "fish" => vec!["-c".to_string(), command.to_string()],
        "zsh" => login(
            "[[ -f ~/.zshenv ]] && source ~/.zshenv >/dev/null 2>&1 || true\n        [[ -f \"${ZDOTDIR:-$HOME}/.zshrc\" ]] && source \"${ZDOTDIR:-$HOME}/.zshrc\" >/dev/null 2>&1 || true",
        ),
        "bash" => login(
            "shopt -s expand_aliases\n        [[ -f ~/.bashrc ]] && source ~/.bashrc >/dev/null 2>&1 || true",
        ),
        "cmd" => vec!["/c".to_string(), command.to_string()],
        _ if ps(file) => vec![
            "-NoProfile".to_string(),
            "-Command".to_string(),
            command.to_string(),
        ],
        _ => vec!["-c".to_string(), command.to_string()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_shell_arguments() {
        assert_eq!(args("fish", "ls", "/w"), ["-c", "ls"]);
        assert_eq!(args("pwsh", "ls", "/w"), ["-NoProfile", "-Command", "ls"]);
        let bash = args("bash", "echo \"hi\"", "/w");
        assert_eq!((bash[0].as_str(), bash[1].as_str(), bash[3].as_str(), bash[4].as_str()), ("-l", "-c", "opencode", "/w"));
        assert!(bash[2].contains("shopt -s expand_aliases") && bash[2].contains("eval \"echo \\\"hi\\\"\""));
        assert!(!preferred(None).is_empty());
    }

    #[test]
    fn names_and_kinds() {
        assert_eq!(kind("pwsh"), Kind::Pwsh);
        assert_eq!(kind("zsh"), Kind::Bash);
        assert_eq!(kind("cmd"), Kind::Cmd);
        if cfg!(windows) {
            assert_eq!(
                name("C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\PowerShell.EXE"),
                "powershell"
            );
            assert_eq!(name("C:/Program Files/Git/bin/bash.exe"), "bash");
            assert!(ps("pwsh.exe"));
        } else {
            assert_eq!(name("/usr/bin/Zsh"), "zsh");
        }
        assert!(posix("bash") && !posix("fish") && denied("nu"));
    }

    #[test]
    fn list_holds_resolvable_shells() {
        let shells = list();
        assert!(!shells.is_empty());
        for shell in &shells {
            assert!(resolve(&shell.path).is_some(), "{shell:?}");
            assert_eq!(shell.acceptable, !denied(&shell.path));
            assert!(shell.name == name(&shell.path) || shell.name == shell.path, "{shell:?}");
        }
        let fish = info("fish");
        assert!(!fish.acceptable && (fish.name == "fish" || fish.name == fish.path));
    }

    #[test]
    fn windows_order_prefers_git_bash() {
        let some = |s: &str| Some(s.to_string());
        let order = win_order(some("C:/Git/bin/bash.exe"), some("C:/pwsh.exe"), some("C:/m/usr/bin/sh.exe"), some("C:/powershell.exe"), "cmd.exe".into());
        assert_eq!(order, ["C:/Git/bin/bash.exe", "C:/pwsh.exe", "C:/m/usr/bin/sh.exe", "C:/powershell.exe", "cmd.exe"]);
        let managed = win_order(None, None, some("C:/m/usr/bin/sh.exe"), some("C:/powershell.exe"), "cmd.exe".into());
        assert_eq!(managed, ["C:/m/usr/bin/sh.exe", "C:/powershell.exe", "cmd.exe"]);
        assert_eq!(win_order(None, None, None, some("C:/powershell.exe"), "cmd.exe".into()), ["C:/powershell.exe", "cmd.exe"]);
        assert_eq!(win_order(None, None, None, None, "cmd.exe".into()), ["cmd.exe"]);
        if cfg!(windows) {
            let unset = std::env::var("SHELL").ok().is_none_or(|s| s.is_empty() || name(&s) == "bash");
            if gitbash().is_some() && unset {
                assert_eq!(name(&acceptable(None)), "bash");
                assert_eq!(name(&preferred(None)), "bash");
                assert_eq!(list().first().map(|s| name(&s.path)).as_deref(), Some("bash"));
            }
            // A configured shell still wins over the Git Bash default.
            if which("powershell").is_some() {
                assert_eq!(name(&acceptable(Some("powershell"))), "powershell");
            }
        }
    }

    #[test]
    fn acceptable_returns_existing_shell() {
        let shell = acceptable(None);
        assert!(!shell.is_empty());
        assert!(!denied(&shell));
        let fallback = acceptable(Some("fish"));
        assert!(!denied(&fallback));
    }
}
