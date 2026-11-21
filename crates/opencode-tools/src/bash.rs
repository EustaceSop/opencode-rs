//! `bash` tool (`opencode/src/tool/shell.ts`).

pub mod background;

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use regex::Regex;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::truncate::{MAX_BYTES, MAX_LINES, write_output};
use crate::util::{
    contains_path, display, io_err, normalize_path, normalize_path_pattern, resolve, windows_path,
};
use crate::{
    PermissionRequest, Tool, ToolContext, ToolError, ToolOutput, arity, bash_prompt, parse_args,
    shell,
};

const MAX_METADATA_LENGTH: usize = 30_000;
/// v2 `Input.background` description (`packages/core/src/tool/plugin/shell.ts`).
pub const BACKGROUND_DESCRIPTION: &str = "Run the command in the background and return immediately (useful for dev servers and long-running builds). You do not need to use '&' at the end of the command when using this parameter. You will be notified when it completes. DO NOT poll for completion.";
const DEFAULT_TIMEOUT_MS: u64 = 2 * 60 * 1000;
const CWD: [&str; 6] = [
    "cd",
    "chdir",
    "popd",
    "pushd",
    "push-location",
    "set-location",
];
const FILES: [&str; 16] = [
    "rm",
    "cp",
    "mv",
    "mkdir",
    "touch",
    "chmod",
    "chown",
    "cat",
    "get-content",
    "set-content",
    "add-content",
    "copy-item",
    "move-item",
    "remove-item",
    "new-item",
    "rename-item",
];
const CMD_FILES: [&str; 12] = [
    "copy", "del", "dir", "erase", "md", "mkdir", "move", "rd", "ren", "rename", "rmdir", "type",
];
const FLAGS: [&str; 3] = ["-destination", "-literalpath", "-path"];
const SWITCHES: [&str; 7] = [
    "-confirm",
    "-debug",
    "-force",
    "-nonewline",
    "-recurse",
    "-verbose",
    "-whatif",
];

#[derive(Debug, Clone)]
pub struct BashConfig {
    /// Config key `shell`; falls back to `$SHELL`, then the platform default.
    pub shell: Option<String>,
    pub default_timeout_ms: u64,
    pub max_lines: usize,
    pub max_bytes: usize,
}

impl Default for BashConfig {
    fn default() -> Self {
        let default_timeout_ms = std::env::var("OPENCODE_EXPERIMENTAL_BASH_DEFAULT_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_TIMEOUT_MS);
        Self {
            shell: None,
            default_timeout_ms,
            max_lines: MAX_LINES,
            max_bytes: MAX_BYTES,
        }
    }
}

pub struct BashTool {
    shell: String,
    config: BashConfig,
}

impl BashTool {
    pub fn new(config: BashConfig) -> Self {
        Self {
            shell: shell::acceptable(config.shell.as_deref()),
            config,
        }
    }

    /// Resolved shell executable.
    pub fn shell(&self) -> &str {
        &self.shell
    }
}

#[derive(Debug, serde::Deserialize)]
struct Params {
    command: String,
    #[serde(default)]
    timeout: Option<serde_json::Number>,
    #[serde(default)]
    workdir: Option<String>,
    // Rust build: v2's `background`.
    #[serde(default)]
    background: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
struct Part {
    kind: String,
    text: String,
}

#[derive(Debug, Clone, PartialEq)]
struct ParsedCommand {
    parts: Vec<Part>,
    source: String,
}

#[derive(Debug, Default, PartialEq)]
struct Scan {
    dirs: Vec<String>,
    patterns: Vec<String>,
    always: Vec<String>,
}

fn add(list: &mut Vec<String>, item: String) {
    if !list.contains(&item) {
        list.push(item);
    }
}

fn node_text(node: tree_sitter::Node, source: &str) -> String {
    node.utf8_text(source.as_bytes()).unwrap_or("").to_string()
}

fn parts(node: tree_sitter::Node, source: &str) -> Vec<Part> {
    let mut out = Vec::new();
    for i in 0..node.child_count() {
        let Some(child) = node.child(i) else { continue };
        let kind = child.kind();
        if kind == "command_elements" {
            for j in 0..child.child_count() {
                let Some(item) = child.child(j) else { continue };
                if item.kind() == "command_argument_sep" || item.kind() == "redirection" {
                    continue;
                }
                out.push(Part {
                    kind: item.kind().to_string(),
                    text: node_text(item, source),
                });
            }
            continue;
        }
        if !matches!(
            kind,
            "command_name"
                | "command_name_expr"
                | "word"
                | "string"
                | "raw_string"
                | "concatenation"
        ) {
            continue;
        }
        out.push(Part {
            kind: kind.to_string(),
            text: node_text(child, source),
        });
    }
    out
}

fn collect_commands(node: tree_sitter::Node, source: &str, out: &mut Vec<ParsedCommand>) {
    if node.kind() == "command" {
        let text = match node.parent() {
            Some(parent) if parent.kind() == "redirected_statement" => node_text(parent, source),
            _ => node_text(node, source),
        };
        out.push(ParsedCommand {
            parts: parts(node, source),
            source: text.trim().to_string(),
        });
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            collect_commands(child, source, out);
        }
    }
}

/// Every `command` node of the tree-sitter parse (bash or PowerShell grammar), in document order.
fn parse_commands(command: &str, ps: bool) -> Result<Vec<ParsedCommand>, ToolError> {
    let language: tree_sitter::Language = if ps {
        tree_sitter_powershell::LANGUAGE.into()
    } else {
        tree_sitter_bash::LANGUAGE.into()
    };
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&language)
        .map_err(|e| ToolError::Other(format!("Failed to parse command: {e}")))?;
    let tree = parser
        .parse(command, None)
        .ok_or_else(|| ToolError::other("Failed to parse command"))?;
    let mut out = Vec::new();
    collect_commands(tree.root_node(), command, &mut out);
    Ok(out)
}

fn unquote(text: &str) -> &str {
    let mut chars = text.chars();
    match (chars.next(), chars.next_back()) {
        (Some(first), Some(last)) if (first == '"' || first == '\'') && first == last => {
            &text[1..text.len() - 1]
        }
        _ => text,
    }
}

fn home_dir() -> String {
    std::env::home_dir()
        .map(|p| display(&p))
        .unwrap_or_default()
}

fn home(text: &str) -> String {
    if text == "~" {
        return home_dir();
    }
    if let Some(rest) = text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")) {
        return display(&Path::new(&home_dir()).join(rest));
    }
    text.to_string()
}

fn env_value(key: &str) -> String {
    std::env::var(key).unwrap_or_default()
}

fn expand(text: &str, cwd: &Path, shell: &str) -> String {
    static BRACED: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(?i)\$\{env:([^}]+)\}").expect("regex"));
    static PLAIN: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\$env:([A-Za-z_][A-Za-z0-9_]*)").expect("regex")
    });
    static AUTO: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(?i)\$(HOME|PWD|PSHOME)").expect("regex"));

    let out = BRACED
        .replace_all(unquote(text), |c: &regex::Captures| env_value(&c[1]))
        .into_owned();
    let out = PLAIN
        .replace_all(&out, |c: &regex::Captures| env_value(&c[1]))
        .into_owned();
    let mut result = String::with_capacity(out.len());
    let mut last = 0;
    for caps in AUTO.captures_iter(&out) {
        let whole = caps.get(0).expect("match");
        let next = out[whole.end()..].chars().next();
        if !matches!(next, None | Some('/') | Some('\\')) {
            continue;
        }
        result.push_str(&out[last..whole.start()]);
        match caps[1].to_uppercase().as_str() {
            "HOME" => result.push_str(&home_dir()),
            "PWD" => result.push_str(&display(cwd)),
            _ => result.push_str(&Path::new(shell).parent().map(display).unwrap_or_default()),
        }
        last = whole.end();
    }
    result.push_str(&out[last..]);
    home(&result)
}

/// PowerShell provider prefix: `FileSystem::x` -> `x`, other providers and multi-letter drives are dropped.
fn provider(text: &str) -> Option<String> {
    let letters = text.chars().take_while(|c| c.is_ascii_alphabetic()).count();
    if letters == 0 {
        return Some(text.to_string());
    }
    let rest = &text[letters..];
    if let Some(path) = rest.strip_prefix("::") {
        return text[..letters]
            .eq_ignore_ascii_case("filesystem")
            .then(|| path.to_string());
    }
    if rest.starts_with(':') && letters > 1 {
        return None;
    }
    Some(text.to_string())
}

fn dynamic(text: &str, ps: bool) -> bool {
    if text.starts_with('(') || text.starts_with("@(") {
        return true;
    }
    if text.contains("$(") || text.contains("${") || text.contains('`') {
        return true;
    }
    if !ps {
        return text.contains('$');
    }
    text.match_indices('$').any(|(idx, _)| {
        !text[idx + 1..]
            .get(..4)
            .is_some_and(|next| next.eq_ignore_ascii_case("env:"))
    })
}

/// Path up to the first glob character; `None` when the glob is the first character.
fn glob_prefix(text: &str) -> Option<&str> {
    match text.find(['?', '*', '[']) {
        None => Some(text),
        Some(0) => None,
        Some(idx) => Some(&text[..idx]),
    }
}

fn path_args(list: &[Part], ps: bool, cmd: bool) -> Vec<String> {
    if !ps {
        let chmod = list.first().is_some_and(|p| p.text == "chmod");
        return list
            .iter()
            .skip(1)
            .filter(|item| {
                !item.text.starts_with('-')
                    && !(cmd && item.text.starts_with('/'))
                    && !(chmod && item.text.starts_with('+'))
            })
            .map(|item| item.text.clone())
            .collect();
    }
    let mut out = Vec::new();
    let mut want = false;
    for item in list.iter().skip(1) {
        if want {
            out.push(item.text.clone());
            want = false;
            continue;
        }
        if item.kind == "command_parameter" {
            let flag = item.text.to_lowercase();
            if SWITCHES.contains(&flag.as_str()) {
                continue;
            }
            want = FLAGS.contains(&flag.as_str());
            continue;
        }
        out.push(item.text.clone());
    }
    out
}

/// Permission patterns from the parse: bash `patterns`/`always`, plus raw path args of file commands.
fn scan_commands(commands: &[ParsedCommand], ps: bool, kind: shell::Kind) -> (Scan, Vec<String>) {
    let mut scan = Scan::default();
    let mut path_candidates = Vec::new();
    let is_cmd = kind == shell::Kind::Cmd;
    for command in commands {
        let tokens: Vec<String> = command.parts.iter().map(|p| p.text.clone()).collect();
        let head = tokens.first().map(|t| {
            if ps || is_cmd {
                t.to_lowercase()
            } else {
                t.clone()
            }
        });
        if let Some(head) = &head
            && (CWD.contains(&head.as_str())
                || FILES.contains(&head.as_str())
                || (is_cmd && CMD_FILES.contains(&head.as_str())))
        {
            path_candidates.extend(path_args(&command.parts, ps, is_cmd));
        }
        if !tokens.is_empty() && !head.as_ref().is_some_and(|h| CWD.contains(&h.as_str())) {
            add(&mut scan.patterns, command.source.clone());
            add(
                &mut scan.always,
                format!("{} *", arity::prefix(&tokens).join(" ")),
            );
        }
    }
    (scan, path_candidates)
}

fn preview(text: String) -> String {
    let count = text.chars().count();
    if count <= MAX_METADATA_LENGTH {
        return text;
    }
    let start = text
        .char_indices()
        .nth(count - MAX_METADATA_LENGTH)
        .map_or(0, |(i, _)| i);
    format!("...\n\n{}", &text[start..])
}

fn tail(text: &str, max_lines: usize, max_bytes: usize) -> (String, bool) {
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.len() <= max_lines && text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let mut out: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    for line in lines.iter().rev() {
        if out.len() >= max_lines {
            break;
        }
        let size = line.len() + usize::from(!out.is_empty());
        if bytes + size > max_bytes {
            if out.is_empty() {
                let mut start = line.len().saturating_sub(max_bytes);
                while start < line.len() && !line.is_char_boundary(start) {
                    start += 1;
                }
                out.push(&line[start..]);
            }
            break;
        }
        out.push(line);
        bytes += size;
    }
    out.reverse();
    (out.join("\n"), true)
}

/// Incremental decoder (`Stream.decodeText`): keeps a split multi-byte sequence for the next chunk.
/// Rust build: a line that is not UTF-8 is decoded with `legacy` instead of becoming U+FFFD. On Windows that is the
/// console code page (Big5, GBK, CP437, ...), in which native console programs and PowerShell's parser errors write.
struct Utf8Stream {
    pending: Vec<u8>,
    legacy: fn(&[u8]) -> Option<String>,
}

impl Default for Utf8Stream {
    fn default() -> Self {
        Utf8Stream { pending: Vec::new(), legacy: legacy_decode }
    }
}

/// An unfinished line with invalid UTF-8 waits for its end up to this size.
const HOLD_LIMIT: usize = 64 * 1024;

impl Utf8Stream {
    fn decode(&self, bytes: &[u8]) -> String {
        match std::str::from_utf8(bytes) {
            Ok(text) => text.to_string(),
            Err(_) => (self.legacy)(bytes).unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned()),
        }
    }

    fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut out = String::new();
        if let Some(end) = self.pending.iter().rposition(|&b| b == b'\n') {
            let lines: Vec<u8> = self.pending.drain(..=end).collect();
            for line in lines.split_inclusive(|&b| b == b'\n') {
                out.push_str(&self.decode(line));
            }
        }
        // The unfinished line streams while it is UTF-8; an invalid byte holds it until the line ends.
        match std::str::from_utf8(&self.pending) {
            Ok(text) => {
                out.push_str(text);
                self.pending.clear();
            }
            Err(err) if err.error_len().is_none() => {
                let valid = err.valid_up_to();
                out.push_str(std::str::from_utf8(&self.pending[..valid]).unwrap_or_default());
                self.pending.drain(..valid);
            }
            Err(_) if self.pending.len() > HOLD_LIMIT => {
                let rest = std::mem::take(&mut self.pending);
                out.push_str(&self.decode(&rest));
            }
            Err(_) => {}
        }
        out
    }

    fn finish(&mut self) -> String {
        let rest = std::mem::take(&mut self.pending);
        if rest.is_empty() { String::new() } else { self.decode(&rest) }
    }
}

#[cfg(windows)]
fn legacy_decode(bytes: &[u8]) -> Option<String> {
    use windows_sys::Win32::Globalization::{CP_OEMCP, MB_ERR_INVALID_CHARS, MultiByteToWideChar};
    let len = i32::try_from(bytes.len()).ok().filter(|&len| len > 0)?;
    // SAFETY: the input pointer and length describe `bytes`; the output buffer is sized by the first call.
    let size = unsafe { MultiByteToWideChar(CP_OEMCP, MB_ERR_INVALID_CHARS, bytes.as_ptr(), len, std::ptr::null_mut(), 0) };
    if size <= 0 {
        return None;
    }
    let mut wide = vec![0u16; size as usize];
    let written = unsafe { MultiByteToWideChar(CP_OEMCP, MB_ERR_INVALID_CHARS, bytes.as_ptr(), len, wide.as_mut_ptr(), size) };
    if written <= 0 {
        return None;
    }
    String::from_utf16(&wide[..written as usize]).ok()
}

#[cfg(not(windows))]
fn legacy_decode(_: &[u8]) -> Option<String> {
    None
}

struct Capture<'a> {
    data_dir: &'a Path,
    max_bytes: usize,
    list: VecDeque<String>,
    used: usize,
    full: String,
    last: String,
    file: Option<PathBuf>,
    sink: Option<std::fs::File>,
    cut: bool,
}

impl Capture<'_> {
    fn push(&mut self, chunk: String) -> std::io::Result<()> {
        let keep = self.max_bytes * 2;
        self.used += chunk.len();
        self.list.push_back(chunk.clone());
        while self.used > keep && self.list.len() > 1 {
            if let Some(item) = self.list.pop_front() {
                self.used -= item.len();
                self.cut = true;
            }
        }
        let mut last = std::mem::take(&mut self.last);
        last.push_str(&chunk);
        self.last = preview(last);

        if let Some(sink) = &mut self.sink {
            sink.write_all(chunk.as_bytes())?;
        } else {
            self.full.push_str(&chunk);
            if self.full.len() > self.max_bytes {
                let file = write_output(self.data_dir, &self.full)?;
                self.sink = Some(std::fs::OpenOptions::new().append(true).open(&file)?);
                self.file = Some(file);
                self.cut = true;
                self.full.clear();
            }
        }
        Ok(())
    }
}

fn is_cmd_exe(shell: &str) -> bool {
    let lower = shell.to_lowercase();
    let base = lower.rsplit('\\').next().unwrap_or("");
    base == "cmd" || base == "cmd.exe"
}

/// Rust build: PowerShell decodes native output and encodes its own with the console code page (GB2312, CP437, ...);
/// both become UTF-8, which is what the output reader expects. The command follows on its own line, so a parser error
/// quotes the command alone.
pub const PS_UTF8_PREFIX: &str = "try { [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false) } catch {}; $OutputEncoding = [System.Text.UTF8Encoding]::new($false)\n";

/// Appended when Windows PowerShell 5.1 rejected `&&` / `||`.
pub const PS51_CHAIN_HINT: &str = "Hint: Windows PowerShell 5.1 has no `&&` or `||`; write `cmd1; if ($?) { cmd2 }` instead (for `||`: `cmd1; if (-not $?) { cmd2 }`).";

/// The 5.1 parser error (`InvalidEndOfLine`, not localized) for a command that chains with `&&` or `||`.
pub fn ps51_chain_error(shell: &str, command: &str, output: &str) -> bool {
    shell::name(shell) == "powershell" && (command.contains("&&") || command.contains("||")) && output.contains("InvalidEndOfLine")
}

fn build_command(shell: &str, command: &str, cwd: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(shell);
    if cfg!(windows) && shell::ps(shell) {
        cmd.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("{PS_UTF8_PREFIX}{command}"),
        ]);
    } else if cfg!(windows) && is_cmd_exe(shell) {
        // Node `shell:` semantics for cmd.exe: verbatim `/d /s /c "<command>"`.
        #[cfg(windows)]
        cmd.raw_arg(format!("/d /s /c \"{command}\""));
    } else {
        cmd.args(["-c", command]);
    }
    cmd.current_dir(cwd)
        .envs(shell::env(shell))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    #[cfg(unix)]
    cmd.process_group(0);
    cmd
}

/// `Process.text([command], {shell, nothrow: true}).text`: stdout of `command` run through `shell`; failures yield what was captured.
pub async fn shell_stdout(shell: &str, command: &str, cwd: &Path) -> String {
    let mut cmd = build_command(shell, command, cwd);
    cmd.stderr(Stdio::null());
    match cmd.output().await {
        Ok(output) => String::from_utf8_lossy(&output.stdout).into_owned(),
        Err(_) => String::new(),
    }
}

/// Kill the whole process tree: `taskkill /T /F` on Windows, SIGTERM then SIGKILL (3 s) to the group elsewhere.
async fn kill_tree(child: &mut tokio::process::Child) {
    let Some(pid) = child.id() else { return };
    #[cfg(windows)]
    {
        let mut killer = tokio::process::Command::new("taskkill");
        killer
            .args(["/pid", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x0800_0000);
        let _ = killer.status().await;
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGTERM);
    }
    if tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .is_err()
    {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

fn spawn_reader(
    mut stream: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    tx: mpsc::UnboundedSender<String>,
) {
    tokio::spawn(async move {
        let mut decoder = Utf8Stream::default();
        let mut buf = vec![0u8; 8192];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let text = decoder.push(&buf[..n]);
                    if !text.is_empty() && tx.send(text).is_err() {
                        return;
                    }
                }
            }
        }
        let rest = decoder.finish();
        if !rest.is_empty() {
            let _ = tx.send(rest);
        }
    });
}

fn platform() -> &'static str {
    match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    }
}

impl BashTool {
    async fn start_background(&self, command: &str, cwd: &Path, timeout: u64, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let env = match &ctx.hooks {
            Some(hooks) => hooks.shell_env(cwd, &ctx.session_id, &ctx.call_id).await.map_err(ToolError::Other)?,
            None => Vec::new(),
        };
        let limits = (self.config.max_lines, self.config.max_bytes);
        let (shell_id, file) =
            background::start(&self.shell, command, cwd, env, timeout, &ctx.session_id, &ctx.data_dir, limits, ctx.hooks.clone())?;
        let output = background::started_output(&shell_id, &file);
        Ok(ToolOutput {
            title: command.to_string(),
            metadata: json!({ "output": output, "shellID": shell_id, "status": "running", "truncated": false }),
            output,
            attachments: vec![],
        })
    }

    async fn cygpath(&self, text: &str) -> Option<PathBuf> {
        let output = tokio::process::Command::new(&self.shell)
            .args(["-lc", "cygpath -w -- \"$1\"", "_", text])
            .envs(shell::env(&self.shell))
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .await
            .ok()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let file = stdout.lines().next()?.trim();
        (!file.is_empty()).then(|| normalize_path(Path::new(file)))
    }

    async fn resolve_path(&self, text: &str, root: &Path) -> PathBuf {
        if cfg!(windows) {
            if shell::posix(&self.shell)
                && text.starts_with('/')
                && windows_path(text) == text
                && let Some(file) = self.cygpath(text).await
            {
                return file;
            }
            return normalize_path(&resolve(root, &windows_path(text)));
        }
        resolve(root, text)
    }

    async fn arg_path(&self, arg: &str, cwd: &Path, ps: bool) -> Option<PathBuf> {
        let text = if ps {
            expand(arg, cwd, &self.shell)
        } else {
            home(unquote(arg))
        };
        if text.is_empty() {
            return None;
        }
        let file = glob_prefix(&text)?;
        if dynamic(file, ps) {
            return None;
        }
        let next = if ps {
            provider(file)?
        } else {
            file.to_string()
        };
        if next.is_empty() {
            return None;
        }
        Some(self.resolve_path(&next, cwd).await)
    }

    async fn ask(&self, ctx: &ToolContext, scan: &Scan, command: &str) -> Result<(), ToolError> {
        if !scan.dirs.is_empty() {
            let globs: Vec<String> = scan
                .dirs
                .iter()
                .map(|dir| {
                    let joined = display(&Path::new(dir).join("*"));
                    if cfg!(windows) {
                        normalize_path_pattern(&joined)
                    } else {
                        joined
                    }
                })
                .collect();
            ctx.permissions
                .ask(PermissionRequest {
                    permission: "external_directory".into(),
                    patterns: globs.clone(),
                    always: globs.clone(),
                    metadata: json!({ "command": command, "directories": scan.dirs, "patterns": globs }),
                })
                .await?;
        }
        if scan.patterns.is_empty() {
            return Ok(());
        }
        ctx.permissions
            .ask(PermissionRequest {
                permission: "bash".into(),
                patterns: scan.patterns.clone(),
                always: scan.always.clone(),
                metadata: json!({ "command": command }),
            })
            .await
    }

    async fn run(
        &self,
        command: &str,
        cwd: &Path,
        timeout: u64,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let mut capture = Capture {
            data_dir: &ctx.data_dir,
            max_bytes: self.config.max_bytes,
            list: VecDeque::new(),
            used: 0,
            full: String::new(),
            last: String::new(),
            file: None,
            sink: None,
            cut: false,
        };
        let mut expired = false;
        let mut aborted = false;
        let mut code: Option<i32> = None;

        let extra = match &ctx.hooks {
            Some(hooks) => hooks
                .shell_env(cwd, &ctx.session_id, &ctx.call_id)
                .await
                .map_err(ToolError::Other)?,
            None => Vec::new(),
        };
        let mut child = build_command(&self.shell, command, cwd)
            .envs(extra)
            .spawn()
            .map_err(io_err)?;
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        if let Some(stdout) = child.stdout.take() {
            spawn_reader(stdout, tx.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_reader(stderr, tx.clone());
        }
        drop(tx);

        let far = Duration::from_secs(60 * 60 * 24 * 365);
        let start = Instant::now();
        let deadline = start
            .checked_add(Duration::from_millis(timeout.saturating_add(100)))
            .unwrap_or(start + far);
        let mut ended = false;
        let mut reading = true;
        let mut grace = start + far;
        loop {
            tokio::select! {
                chunk = rx.recv(), if reading => match chunk {
                    Some(chunk) => capture.push(chunk).map_err(io_err)?,
                    None => reading = false,
                },
                status = child.wait(), if !ended => {
                    code = status.ok().and_then(|s| s.code());
                    ended = true;
                }
                _ = ctx.abort.cancelled(), if !ended => {
                    aborted = true;
                    kill_tree(&mut child).await;
                    ended = true;
                }
                _ = tokio::time::sleep_until(deadline), if !ended => {
                    expired = true;
                    kill_tree(&mut child).await;
                    ended = true;
                }
                _ = tokio::time::sleep_until(grace), if ended => break,
            }
            if ended {
                if !reading {
                    break;
                }
                // Readers normally hit EOF right away; a surviving grandchild may hold the pipe open.
                grace = grace.min(Instant::now() + Duration::from_secs(1));
            }
        }
        drop(capture.sink.take());

        let mut meta: Vec<String> = Vec::new();
        if expired {
            meta.push(format!(
                "shell tool terminated command after exceeding timeout {timeout} ms. If this command is expected to take longer and is not waiting for interactive input, retry with a larger timeout value in milliseconds."
            ));
        }
        if aborted {
            meta.push("User aborted the command".to_string());
        }

        let raw: String = capture.list.iter().map(String::as_str).collect();
        let (end_text, end_cut) = tail(&raw, self.config.max_lines, self.config.max_bytes);
        let mut cut = capture.cut;
        let mut file = capture.file.clone();
        if end_cut {
            cut = true;
            if file.is_none() {
                file = Some(write_output(&ctx.data_dir, &raw).map_err(io_err)?);
            }
        }

        let mut output = if end_text.is_empty() {
            "(no output)".to_string()
        } else {
            end_text
        };
        if cut && let Some(file) = &file {
            output = format!(
                "...output truncated...\n\nFull output saved to: {}\n\n{output}",
                display(file)
            );
        }
        if code != Some(0) && ps51_chain_error(&self.shell, command, &raw) {
            output.push_str(&format!("\n\n{PS51_CHAIN_HINT}"));
        }
        if !meta.is_empty() {
            output.push_str(&format!(
                "\n\n<shell_metadata>\n{}\n</shell_metadata>",
                meta.join("\n")
            ));
        }

        let shown = if capture.last.is_empty() {
            preview(output.clone())
        } else {
            capture.last
        };
        let mut metadata = json!({ "output": shown, "exit": code, "truncated": cut });
        if cut && let Some(file) = &file {
            metadata["outputPath"] = json!(display(file));
        }
        Ok(ToolOutput {
            title: command.to_string(),
            output,
            metadata,
            attachments: vec![],
        })
    }
}

#[async_trait::async_trait]
impl Tool for BashTool {
    fn id(&self) -> &str {
        "bash"
    }

    fn description(&self) -> String {
        let tmp = display(&std::env::temp_dir().join("opencode"));
        bash_prompt::render(&bash_prompt::RenderInput {
            shell_name: &shell::name(&self.shell),
            platform: platform(),
            tmp: &tmp,
            max_lines: self.config.max_lines,
            max_bytes: self.config.max_bytes,
            default_timeout_ms: self.config.default_timeout_ms,
        })
    }

    fn parameters(&self) -> Value {
        json!({
            "$schema": crate::JSON_SCHEMA_URI,
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The command to execute" },
                "timeout": {
                    "type": "integer",
                    "exclusiveMinimum": 0,
                    "maximum": 9007199254740991u64,
                    "description": "Optional timeout in milliseconds"
                },
                "workdir": {
                    "type": "string",
                    "description": "The working directory to run the command in. Defaults to the current directory. Use this instead of 'cd' commands."
                },
                "background": {
                    "type": "boolean",
                    "description": BACKGROUND_DESCRIPTION
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let params: Params = parse_args("bash", args)?;
        let background = params.background == Some(true);
        let timeout = match &params.timeout {
            // Background commands have no timeout unless one is given.
            None if background => 0,
            None => self.config.default_timeout_ms,
            Some(number) => {
                let value = number.as_f64().unwrap_or(f64::NAN);
                if value < 0.0 {
                    return Err(ToolError::Other(format!(
                        "Invalid timeout value: {number}. Timeout must be a positive number."
                    )));
                }
                if value.fract() != 0.0 || value <= 0.0 || !value.is_finite() {
                    return Err(ToolError::InvalidArguments {
                        tool: "bash".into(),
                        detail: format!("timeout must be a positive integer, got {number}"),
                    });
                }
                value as u64
            }
        };

        let cwd = match &params.workdir {
            Some(workdir) => self.resolve_path(workdir, &ctx.cwd).await,
            None => ctx.cwd.clone(),
        };
        let ps = shell::ps(&self.shell);
        let kind = shell::kind(&shell::name(&self.shell));

        let commands = parse_commands(&params.command, ps)?;
        let (mut scan, candidates) = scan_commands(&commands, ps, kind);
        for arg in candidates {
            let Some(resolved) = self.arg_path(&arg, &cwd, ps).await else {
                continue;
            };
            if contains_path(ctx, &resolved) {
                continue;
            }
            let is_dir = tokio::fs::metadata(&resolved)
                .await
                .is_ok_and(|m| m.is_dir());
            let dir = if is_dir {
                resolved
            } else {
                resolved.parent().map(Path::to_path_buf).unwrap_or(resolved)
            };
            add(&mut scan.dirs, display(&dir));
        }
        if !contains_path(ctx, &cwd) {
            add(&mut scan.dirs, display(&cwd));
        }
        self.ask(ctx, &scan, &params.command).await?;

        if background {
            return self.start_background(&params.command, &cwd, timeout, ctx).await;
        }
        self.run(&params.command, &cwd, timeout, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Fixture;

    fn scan(command: &str, ps: bool, kind: shell::Kind) -> (Scan, Vec<String>) {
        scan_commands(&parse_commands(command, ps).unwrap(), ps, kind)
    }

    #[test]
    fn bash_patterns_and_arity() {
        let (s, paths) = scan(
            "git status && npm run build -- --prod; echo hi > out.txt",
            false,
            shell::Kind::Bash,
        );
        assert_eq!(
            s.patterns,
            vec!["git status", "npm run build -- --prod", "echo hi > out.txt"]
        );
        assert_eq!(s.always, vec!["git status *", "npm run build *", "echo *"]);
        assert!(paths.is_empty());
    }

    #[test]
    fn bash_cd_is_not_asked_and_file_commands_yield_paths() {
        let (s, paths) = scan(
            "cd /tmp/x && rm -rf \"../out dir\" ~/y $HOME/z",
            false,
            shell::Kind::Bash,
        );
        assert_eq!(s.patterns, vec!["rm -rf \"../out dir\" ~/y $HOME/z"]);
        assert_eq!(s.always, vec!["rm *"]);
        assert_eq!(paths, vec!["/tmp/x", "\"../out dir\"", "~/y", "$HOME/z"]);
        let (s, _) = scan("cd sub", false, shell::Kind::Bash);
        assert_eq!(s, Scan::default());
        let (_, paths) = scan("chmod +x run.sh", false, shell::Kind::Bash);
        assert_eq!(paths, vec!["run.sh"]);
    }

    #[test]
    fn bash_nested_commands_and_pipes() {
        let (s, _) = scan("echo $(git rev-parse HEAD) | cat", false, shell::Kind::Bash);
        assert_eq!(
            s.patterns,
            vec!["echo $(git rev-parse HEAD)", "git rev-parse HEAD", "cat"]
        );
        assert_eq!(s.always, vec!["echo *", "git rev-parse *", "cat *"]);
    }

    #[test]
    fn powershell_patterns_and_path_flags() {
        let (s, paths) = scan(
            "Remove-Item -Recurse -Force -LiteralPath C:\\tmp\\x; Get-ChildItem . | Select-Object -First 1",
            true,
            shell::Kind::Powershell,
        );
        assert_eq!(
            s.patterns[0],
            "Remove-Item -Recurse -Force -LiteralPath C:\\tmp\\x"
        );
        assert_eq!(s.always[0], "Remove-Item *");
        assert!(s.patterns.iter().any(|p| p.starts_with("Get-ChildItem")));
        assert_eq!(paths, vec!["C:\\tmp\\x"]);
        let (s, paths) = scan("Set-Location C:\\other", true, shell::Kind::Powershell);
        assert!(s.patterns.is_empty());
        assert_eq!(paths, vec!["C:\\other"]);
    }

    #[test]
    fn path_helpers() {
        assert_eq!(unquote("\"a b\""), "a b");
        assert_eq!(unquote("'x"), "'x");
        assert_eq!(glob_prefix("src/*.rs"), Some("src/"));
        assert_eq!(glob_prefix("*.rs"), None);
        assert!(dynamic("$HOME/x", false) && !dynamic("$env:TEMP\\x", true) && dynamic("$x", true));
        assert!(dynamic("$(pwd)", true) && dynamic("`x`", false));
        assert_eq!(provider("FileSystem::C:\\x").as_deref(), Some("C:\\x"));
        assert_eq!(provider("Registry::HKLM"), None);
        assert_eq!(provider("HKLM:\\Software"), None);
        assert_eq!(provider("C:\\x").as_deref(), Some("C:\\x"));
        assert_eq!(
            expand("$PWD/sub", Path::new("/work"), "/bin/pwsh"),
            format!("{}/sub", Path::new("/work").display())
        );
        assert_eq!(expand("$PWDX", Path::new("/work"), "/bin/pwsh"), "$PWDX");
    }

    #[test]
    fn tail_and_preview() {
        assert_eq!(tail("a\nb\nc", 10, 100), ("a\nb\nc".to_string(), false));
        assert_eq!(tail("a\nb\nc", 2, 100), ("b\nc".to_string(), true));
        assert_eq!(tail("aaaa\nbbbb", 10, 6), ("bbbb".to_string(), true));
        assert_eq!(tail("héllo", 10, 4), ("llo".to_string(), true));
        let long = "x".repeat(MAX_METADATA_LENGTH + 5);
        let shown = preview(long);
        assert!(shown.starts_with("...\n\nx") && shown.len() == MAX_METADATA_LENGTH + 5);
    }

    #[test]
    fn utf8_stream_handles_split_sequences() {
        let mut s = Utf8Stream { pending: Vec::new(), legacy: |_| None };
        let bytes = "é".as_bytes();
        assert_eq!(s.push(&bytes[..1]), "");
        assert_eq!(s.push(&bytes[1..]), "é");
        assert_eq!(s.push(&[0xff, b'a']), "");
        assert_eq!(s.push(b"\n"), "\u{fffd}a\n");
        assert_eq!(s.push(&[b'b', 0xff]), "");
        assert_eq!(s.finish(), "b\u{fffd}");
    }

    #[test]
    fn utf8_stream_decodes_legacy_lines_with_the_fallback() {
        let latin1 = |bytes: &[u8]| Some(bytes.iter().map(|&b| b as char).collect::<String>());
        let mut s = Utf8Stream { pending: Vec::new(), legacy: latin1 };
        // UTF-8 streams at once, also mid-line; a legacy line is decoded whole when it ends.
        assert_eq!(s.push("中文 ".as_bytes()), "中文 ");
        assert_eq!(s.push(&[b'x', 0xe9, b'y']), "");
        assert_eq!(s.push(b"\nok\nz"), "x\u{e9}y\nok\nz");
        assert_eq!(s.push(&[0xe9]), "");
        assert_eq!(s.finish(), "\u{e9}");
        // A huge unterminated legacy line is not held forever.
        let mut s = Utf8Stream { pending: Vec::new(), legacy: latin1 };
        let long = vec![0xe9; HOLD_LIMIT + 1];
        assert_eq!(s.push(&long).chars().count(), HOLD_LIMIT + 1);
    }

    #[cfg(windows)]
    #[test]
    fn legacy_decode_uses_the_console_code_page() {
        assert_eq!(legacy_decode(b"plain ascii"), Some("plain ascii".to_string()));
        assert_eq!(legacy_decode(b""), None);
    }

    fn tool() -> BashTool {
        BashTool::new(BashConfig {
            default_timeout_ms: 60_000,
            ..BashConfig::default()
        })
    }

    fn sleep_command(tool: &BashTool) -> &'static str {
        match shell::kind(&shell::name(tool.shell())) {
            shell::Kind::Pwsh | shell::Kind::Powershell => "Start-Sleep -Seconds 30",
            shell::Kind::Cmd => "ping -n 30 127.0.0.1",
            shell::Kind::Bash => "sleep 30",
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runs_command_and_asks_permission() {
        let t = Fixture::new();
        let out = tool()
            .execute(json!({"command": "echo hello"}), &t.ctx())
            .await
            .unwrap();
        assert_eq!(out.output.trim(), "hello");
        assert_eq!(out.title, "echo hello");
        assert_eq!(out.metadata["exit"], 0);
        assert_eq!(out.metadata["truncated"], false);
        let reqs = t.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].permission, "bash");
        assert_eq!(reqs[0].patterns, vec!["echo hello"]);
        assert_eq!(reqs[0].always, vec!["echo *"]);
        assert_eq!(reqs[0].metadata, json!({"command": "echo hello"}));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nonzero_exit_is_not_an_error_and_workdir_applies() {
        let t = Fixture::new();
        t.write("sub/marker.txt", "x");
        let out = tool()
            .execute(json!({"command": "exit 3"}), &t.ctx())
            .await
            .unwrap();
        assert_eq!(out.metadata["exit"], 3);
        assert_eq!(out.output, "(no output)");
        let list = if shell::kind(&shell::name(tool().shell())) == shell::Kind::Cmd {
            "dir /b"
        } else {
            "ls"
        };
        let out = tool()
            .execute(json!({"command": list, "workdir": "sub"}), &t.ctx())
            .await
            .unwrap();
        assert!(out.output.contains("marker.txt"), "{}", out.output);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn external_workdir_asks_for_directory() {
        let t = Fixture::new();
        let other = Fixture::new();
        let out = tool()
            .execute(
                json!({"command": "echo hi", "workdir": other.root.to_string_lossy()}),
                &t.ctx(),
            )
            .await
            .unwrap();
        assert_eq!(out.output.trim(), "hi");
        let reqs = t.requests();
        assert_eq!(reqs[0].permission, "external_directory");
        assert_eq!(
            reqs[0].patterns,
            vec![other.root.join("*").to_string_lossy().into_owned()]
        );
        assert_eq!(reqs[0].always, reqs[0].patterns);
        assert_eq!(reqs[0].metadata["command"], "echo hi");
        assert_eq!(reqs[1].permission, "bash");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rejection_prevents_execution() {
        let t = Fixture::new();
        t.deny("bash");
        let res = tool()
            .execute(json!({"command": "echo x > made.txt"}), &t.ctx())
            .await;
        assert!(matches!(res, Err(ToolError::Rejected)));
        assert!(!t.root.join("made.txt").exists());
    }

    struct Notified(mpsc::UnboundedSender<background::Finished>);

    #[async_trait::async_trait]
    impl crate::WorkspaceHooks for Notified {
        fn background_done(&self, finished: background::Finished) {
            let _ = self.0.send(finished);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn background_returns_at_once_and_reports_completion() {
        let mut t = Fixture::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        t.hooks = Some(std::sync::Arc::new(Notified(tx)));
        let tool = tool();
        let out = tool.execute(json!({"command": "echo bg-done", "background": true}), &t.ctx()).await.unwrap();
        assert!(out.output.starts_with("Command moved to the background (shell ID: shell_"), "{}", out.output);
        assert!(out.output.contains("Output is streaming to: ") && out.output.ends_with(background::INSTRUCTION));
        assert_eq!(out.metadata["status"], "running");
        let shell_id = out.metadata["shellID"].as_str().unwrap().to_string();
        let done = tokio::time::timeout(Duration::from_secs(30), rx.recv()).await.unwrap().unwrap();
        assert_eq!((done.session_id.as_str(), done.shell_id.as_str(), done.state, done.exit), ("ses_test", shell_id.as_str(), "completed", Some(0)));
        let head = format!("<shell id=\"{shell_id}\" state=\"completed\" command=\"echo bg-done\">\nbg-done");
        assert!(done.notification().starts_with(&head), "{}", done.notification());
        assert!(background::running("ses_test").is_empty());

        // A long one runs until its owner goes away; then it dies without a notice.
        let long = sleep_command(&tool);
        tool.execute(json!({"command": long, "background": true}), &t.ctx()).await.unwrap();
        assert_eq!(background::running("ses_test"), [long]);
        background::kill_owned(t.data.path());
        assert!(background::running("ses_test").is_empty());
        assert!(tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_kills_and_reports() {
        let t = Fixture::new();
        let tool = tool();
        let started = std::time::Instant::now();
        let out = tool
            .execute(
                json!({"command": sleep_command(&tool), "timeout": 1500}),
                &t.ctx(),
            )
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(20));
        assert!(out.output.contains(
            "<shell_metadata>\nshell tool terminated command after exceeding timeout 1500 ms. If this command is expected to take longer and is not waiting for interactive input, retry with a larger timeout value in milliseconds.\n</shell_metadata>"
        ));
        assert!(out.metadata["exit"].is_null());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_kills_and_reports() {
        let t = Fixture::new();
        let tool = tool();
        let abort = t.abort.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            abort.cancel();
        });
        let started = std::time::Instant::now();
        let out = tool
            .execute(json!({"command": sleep_command(&tool)}), &t.ctx())
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(20));
        assert!(
            out.output
                .ends_with("<shell_metadata>\nUser aborted the command\n</shell_metadata>")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn large_output_is_truncated_to_file() {
        let t = Fixture::new();
        let tool = BashTool::new(BashConfig {
            max_lines: 5,
            max_bytes: 200,
            ..BashConfig::default()
        });
        let command = match shell::kind(&shell::name(tool.shell())) {
            shell::Kind::Pwsh | shell::Kind::Powershell => "1..50 | ForEach-Object { \"line $_\" }",
            shell::Kind::Cmd => "for /L %i in (1,1,50) do @echo line %i",
            shell::Kind::Bash => "for i in $(seq 1 50); do echo \"line $i\"; done",
        };
        let out = tool
            .execute(json!({"command": command}), &t.ctx())
            .await
            .unwrap();
        assert_eq!(out.metadata["truncated"], true);
        let path = out.metadata["outputPath"].as_str().unwrap();
        assert!(Path::new(path).starts_with(t.data.path().join("tool-output")));
        assert!(out.output.starts_with(&format!(
            "...output truncated...\n\nFull output saved to: {path}\n\n"
        )));
        assert!(out.output.contains("line 50") && !out.output.contains("line 1\n"));
        let saved = std::fs::read_to_string(path).unwrap();
        assert!(saved.contains("line 1") && saved.contains("line 50"));
    }

    #[tokio::test]
    async fn invalid_timeout() {
        let t = Fixture::new();
        let err = tool()
            .execute(json!({"command": "echo", "timeout": -5}), &t.ctx())
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Invalid timeout value: -5. Timeout must be a positive number."
        );
        let res = tool()
            .execute(json!({"command": "echo", "timeout": 0}), &t.ctx())
            .await;
        assert!(matches!(res, Err(ToolError::InvalidArguments { .. })));
    }

    // Manual end-to-end check of a MinGit fetched by `opencode setup bash`; ignored, and it downloads nothing:
    // OPENCODE_RS_MANAGED_SHELL=<cache>\opencode-rs\mingit\<version>\usr\bin\sh.exe cargo test -p opencode-tools managed_bash -- --ignored --nocapture
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore]
    async fn managed_bash_runs_commands() {
        let shell = std::env::var("OPENCODE_RS_MANAGED_SHELL").expect("OPENCODE_RS_MANAGED_SHELL");
        let t = Fixture::new();
        t.write("sub/marker.txt", "x");
        let tool = BashTool::new(BashConfig { shell: Some(shell.clone()), ..BashConfig::default() });
        assert_eq!(tool.shell(), shell);
        let command = "echo \"bash $BASH_VERSION\" && n=$((6*7)) && echo \"n=$n\" && ls && command -v ls grep sed && git --version && printf '%s\\n' 中文";
        let out = tool.execute(json!({"command": command, "workdir": "sub"}), &t.ctx()).await.unwrap();
        println!("{}", out.output);
        assert_eq!(out.metadata["exit"], 0, "{}", out.output);
        assert!(out.output.contains("bash 5.") && out.output.contains("n=42") && out.output.contains("marker.txt"), "{}", out.output);
        assert!(out.output.contains("git version") && out.output.contains("中文"), "{}", out.output);
    }

    #[test]
    fn chain_error_needs_windows_powershell_and_the_parser_id() {
        let error = "At line:1 char:8\n+ echo a && echo b\n    + FullyQualifiedErrorId : InvalidEndOfLine";
        let ps51 = "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe";
        assert!(ps51_chain_error(ps51, "echo a && echo b", error));
        assert!(ps51_chain_error(ps51, "a || b", error));
        assert!(!ps51_chain_error(ps51, "echo a; echo b", error));
        assert!(!ps51_chain_error(ps51, "echo a && echo b", "a\nb"));
        assert!(!ps51_chain_error("pwsh", "echo a && echo b", error));
    }

    // Windows PowerShell under a non-UTF-8 console code page: native and PowerShell output both arrive as UTF-8.
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn windows_powershell_is_utf8_and_hints_at_chaining() {
        let t = Fixture::new();
        let tool = BashTool::new(BashConfig {
            shell: Some("powershell".into()),
            ..BashConfig::default()
        });
        if shell::name(tool.shell()) != "powershell" {
            return;
        }
        let out = tool
            .execute(json!({"command": "cmd /c echo 中文输出; Write-Output 'é 中'"}), &t.ctx())
            .await
            .unwrap();
        assert!(out.output.contains("中文输出") && out.output.contains("é 中"), "{}", out.output);
        let out = tool
            .execute(json!({"command": "echo a && echo b"}), &t.ctx())
            .await
            .unwrap();
        assert!(out.output.ends_with(PS51_CHAIN_HINT), "{}", out.output);
        let out = tool
            .execute(json!({"command": "echo a; if ($?) { echo b }"}), &t.ctx())
            .await
            .unwrap();
        assert_eq!(out.output.trim().replace("\r\n", "\n"), "a\nb");
    }
}
