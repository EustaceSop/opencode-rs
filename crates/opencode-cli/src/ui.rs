// Terminal output helpers (`opencode/src/cli/ui.ts`): everything here goes to stderr.
use std::io::IsTerminal;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

pub const DIM: &str = "\x1b[90m";
pub const NORMAL: &str = "\x1b[0m";
pub const WARNING_BOLD: &str = "\x1b[93m\x1b[1m";
pub const DANGER_BOLD: &str = "\x1b[91m\x1b[1m";
pub const SUCCESS_BOLD: &str = "\x1b[92m\x1b[1m";
pub const INFO_BOLD: &str = "\x1b[94m\x1b[1m";

// A command failure with its own exit code (`CliError` in `cli/effect-cmd.ts`); an empty message prints nothing.
#[derive(Debug)]
pub struct CliError {
    pub message: String,
    pub code: i32,
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

pub fn fail(message: impl Into<String>) -> anyhow::Error {
    CliError { message: message.into(), code: 1 }.into()
}

// `UICancelledError`: the user left a prompt.
pub fn cancelled() -> anyhow::Error {
    fail("")
}

const WORDMARK: [&str; 4] = [
    "⠀                                ▄     ",
    "█▀▀█ █▀▀█ █▀▀█ █▀▀▄ █▀▀▀ █▀▀█ █▀▀█ █▀▀█",
    "█  █ █  █ █▀▀▀ █  █ █    █  █ █  █ █▀▀▀",
    "▀▀▀▀ █▀▀▀ ▀▀▀▀ ▀  ▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀",
];
const LOGO_LEFT: [&str; 4] = ["                   ", "█▀▀█ █▀▀█ █▀▀█ █▀▀▄", "█__█ █__█ █^^^ █__█", "▀▀▀▀ █▀▀▀ ▀▀▀▀ ▀~~▀"];
const LOGO_RIGHT: [&str; 4] = ["             ▄     ", "█▀▀▀ █▀▀█ █▀▀█ █▀▀█", "█___ █__█ █__█ █^^^", "▀▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀"];

fn draw(line: &str, fg: &str, shadow: &str, bg: &str) -> String {
    let mut out = String::new();
    for c in line.chars() {
        match c {
            '_' => out.push_str(&format!("{bg} {NORMAL}")),
            '^' => out.push_str(&format!("{fg}{bg}▀{NORMAL}")),
            '~' => out.push_str(&format!("{shadow}▀{NORMAL}")),
            ' ' => out.push(' '),
            other => out.push_str(&format!("{fg}{other}{NORMAL}")),
        }
    }
    out
}

// `UI.logo`: the plain wordmark when neither stdout nor stderr is a terminal.
pub fn logo(pad: &str) -> String {
    let mut out = String::new();
    if !std::io::stdout().is_terminal() && !std::io::stderr().is_terminal() {
        for row in WORDMARK {
            out.push_str(&format!("{pad}{row}\n"));
        }
        return out.trim_end().to_string();
    }
    for (left, right) in LOGO_LEFT.iter().zip(LOGO_RIGHT) {
        out.push_str(pad);
        out.push_str(&draw(left, DIM, "\x1b[38;5;235m", "\x1b[48;5;235m"));
        out.push(' ');
        out.push_str(&draw(right, NORMAL, "\x1b[38;5;238m", "\x1b[48;5;238m"));
        out.push('\n');
    }
    out.trim_end().to_string()
}

static BLANK: AtomicBool = AtomicBool::new(false);

pub fn println(message: &str) {
    BLANK.store(false, Ordering::SeqCst);
    eprintln!("{message}");
}

// At most one blank line in a row.
pub fn empty() {
    if !BLANK.swap(true, Ordering::SeqCst) {
        eprintln!("{NORMAL}");
    }
}

pub fn error(message: &str) {
    let message = message.strip_prefix("Error: ").unwrap_or(message);
    println(&format!("{DANGER_BOLD}Error: {NORMAL}{message}"));
}

// The frame `@clack/prompts` draws around a command's output, without its colors.
pub fn intro(title: &str) {
    println(&format!("┌  {title}"));
}

pub fn log(symbol: &str, message: &str) {
    println("│");
    let mut lines = message.split('\n');
    println(&format!("{symbol}  {}", lines.next().unwrap_or_default()));
    for line in lines {
        println(&format!("│  {line}"));
    }
}

pub fn outro(message: &str) {
    println("│");
    println(&format!("└  {message}"));
    println("");
}

pub fn info(message: &str) {
    log("●", message);
}

pub fn success(message: &str) {
    log("◆", message);
}

pub fn warn(message: &str) {
    log("▲", message);
}

pub fn failure(message: &str) {
    log("■", message);
}

pub fn step(message: &str) {
    log("◇", message);
}

pub fn message(message: &str) {
    log("│", message);
}

// Where a spinner would stop: `◇` on success, `■` on failure.
pub fn done(message: &str, ok: bool) {
    log(if ok { "◇" } else { "■" }, message);
}

// `~` for the home directory, as `shortenPath` does.
pub fn shorten(path: &Path, home: &Path) -> String {
    let text = path.to_string_lossy();
    let home = home.to_string_lossy();
    match text.strip_prefix(home.as_ref()) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => text.into_owned(),
    }
}
