//! Shell tool description rendering (`opencode/src/tool/shell/prompt.ts:273-291`).

use crate::bash_profiles::{self as profiles, Profile};

const TEMPLATE: &str =
    include_str!("../../../../opencode-ts/packages/opencode/src/tool/shell/shell.txt");

pub(crate) struct RenderInput<'a> {
    pub shell_name: &'a str,
    pub platform: &'a str,
    pub tmp: &'a str,
    pub max_lines: usize,
    pub max_bytes: usize,
    pub default_timeout_ms: u64,
}

fn profile(name: &str, platform: &str) -> &'static Profile {
    let windows = platform == "win32";
    match name {
        "cmd" => &profiles::CMD,
        "pwsh" if windows => &profiles::PWSH_WIN,
        "pwsh" => &profiles::PWSH_POSIX,
        "powershell" if windows => &profiles::POWERSHELL_WIN,
        "powershell" => &profiles::POWERSHELL_POSIX,
        _ => &profiles::BASH,
    }
}

/// Single-pass `${key}` substitution (`renderPrompt`); unknown keys are left untouched.
fn render_template(template: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let key_len = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(after.len());
        let value = if key_len > 0 && after[key_len..].starts_with('}') {
            lookup(&after[..key_len])
        } else {
            None
        };
        match value {
            Some(value) => {
                out.push_str(&value);
                rest = &after[key_len + 1..];
            }
            None => {
                out.push_str("${");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

// Rust build only: bash on win32 is Git Bash (the default shell there), whose path rules differ from Unix.
pub(crate) const GIT_BASH_NOTES: &str = "# Git Bash (Windows) shell notes
- Commands run in Git Bash (MSYS2 bash) on Windows, not in PowerShell or cmd.exe: use POSIX syntax (`&&`, `$VAR`, `export`).
- Write Windows paths with forward slashes (`C:/path/to/file`) or in MSYS form (`/c/path/to/file`); quote paths that contain spaces or backslashes.
- Windows programs on PATH (`git`, `node`, `python`, ...) run normally; PowerShell cmdlets do not, so call `powershell -NoProfile -Command \"...\"` when one is needed.

";

pub(crate) fn render(input: &RenderInput) -> String {
    let selected = profile(input.shell_name, input.platform);
    let bash = !matches!(input.shell_name, "cmd" | "pwsh" | "powershell");
    let notes = if input.platform == "win32" && bash { GIT_BASH_NOTES } else { "" };
    let command_section = format!("{notes}{}", selected.command_section)
        .replace("{{maxLines}}", &input.max_lines.to_string())
        .replace("{{maxBytes}}", &input.max_bytes.to_string())
        .replace(
            "{{defaultTimeoutMs}}",
            &input.default_timeout_ms.to_string(),
        );
    render_template(TEMPLATE, |key| {
        Some(match key {
            "intro" => selected.intro.to_string(),
            "os" => input.platform.to_string(),
            "shell" => input.shell_name.to_string(),
            "tmp" => input.tmp.to_string(),
            "workdirSection" => selected.workdir_section.to_string(),
            "commandSection" => command_section.clone(),
            "gitCommands" => selected.git_commands.to_string(),
            "toolName" => "bash".to_string(),
            "gitCommandRestriction" => selected.git_command_restriction.to_string(),
            "createPrInstruction" => selected.create_pr_instruction.to_string(),
            "createPrExample" => selected.create_pr_example.to_string(),
            _ => return None,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(shell_name: &'a str, platform: &'a str) -> RenderInput<'a> {
        RenderInput {
            shell_name,
            platform,
            tmp: "/tmp/opencode",
            max_lines: 2000,
            max_bytes: 51200,
            default_timeout_ms: 120000,
        }
    }

    #[test]
    fn bash_profile() {
        let text = render(&input("zsh", "linux"));
        assert!(text.starts_with(
            "Executes a given bash command in a persistent shell session with optional timeout"
        ));
        assert!(text.contains("Be aware: OS: linux, Shell: zsh"));
        assert!(text.contains("Use `/tmp/opencode` for temporary work outside the workspace."));
        assert!(text.contains("commands will time out after 120000ms."));
        assert!(text.contains("If the output exceeds 2000 lines or 51200 bytes"));
        assert!(!text.contains("{{"));
        assert!(!text.contains("Git Bash"));
    }

    #[test]
    fn git_bash_profile_on_windows() {
        let text = render(&input("bash", "win32"));
        assert!(text.starts_with("Executes a given bash command in a persistent shell session"));
        assert!(text.contains("Be aware: OS: win32, Shell: bash"));
        assert!(text.contains("# Git Bash (Windows) shell notes\n- Commands run in Git Bash"));
        assert!(text.find("# Git Bash (Windows) shell notes").unwrap() < text.find("Before executing the command").unwrap());
        assert!(!render(&input("powershell", "win32")).contains("Git Bash (Windows)"));
    }

    #[test]
    fn powershell_and_cmd_profiles() {
        let text = render(&input("powershell", "win32"));
        assert!(text.starts_with(
            "Executes a given Windows PowerShell (5.1) command with optional timeout"
        ));
        assert!(text.contains("# Windows PowerShell (5.1) shell notes"));
        assert!(
            text.contains(
                "Set-Location -LiteralPath \"project\\subdir\"; if ($?) { pytest tests }"
            )
        );
        let text = render(&input("pwsh", "linux"));
        assert!(text.contains("# PowerShell (7+) shell notes"));
        assert!(text.contains("Set-Location -LiteralPath \"project/subdir\" && pytest tests"));
        let text = render(&input("cmd", "win32"));
        assert!(text.starts_with("Executes a given cmd.exe command with optional timeout"));
        assert!(text.contains("cd /d \"project\\subdir\" && dir"));
    }

    #[test]
    fn template_is_single_pass() {
        assert_eq!(
            render_template("a ${x} ${y} ${", |k| (k == "x").then(|| "${y}".to_string())),
            "a ${y} ${y} ${"
        );
    }
}
