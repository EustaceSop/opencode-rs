// `opencode session ...` (`opencode/src/cli/cmd/session.ts`).
use crate::ui::{self, NORMAL, SUCCESS_BOLD, fail};
use anyhow::Result;
use chrono::{DateTime, Datelike, Local, TimeZone};
use opencode_core::paths::Paths;
use opencode_session::Engine;
use opencode_storage::Session;
use serde_json::{Value, json};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn local(ms: u64) -> DateTime<Local> {
    Local.timestamp_millis_opt(ms as i64).single().unwrap_or_else(Local::now)
}

// `Locale.time` / `Locale.datetime` in the en-US shapes (`3:07 PM`, `3:07 PM · 11/9/2026`); other locales are not ported.
pub fn time(ms: u64) -> String {
    local(ms).format("%-I:%M %p").to_string()
}

pub fn datetime(ms: u64) -> String {
    format!("{} · {}", time(ms), local(ms).format("%-m/%-d/%Y"))
}

pub fn today_time_or_datetime(ms: u64) -> String {
    let (date, now) = (local(ms), Local::now());
    if date.year() == now.year() && date.ordinal() == now.ordinal() { time(ms) } else { datetime(ms) }
}

// Lengths are UTF-16 units, as in JavaScript.
fn units(text: &str) -> usize {
    text.encode_utf16().count()
}

pub fn truncate(text: &str, len: usize) -> String {
    let all: Vec<u16> = text.encode_utf16().collect();
    if all.len() <= len {
        return text.to_string();
    }
    format!("{}…", String::from_utf16_lossy(&all[..len.saturating_sub(1)]))
}

fn pad(text: &str, width: usize) -> String {
    format!("{text}{}", " ".repeat(width.saturating_sub(units(text))))
}

pub fn table(sessions: &[Session]) -> String {
    let id_width = sessions.iter().map(|s| units(&s.id)).max().unwrap_or(0).max(20);
    let title_width = sessions.iter().map(|s| units(&s.title)).max().unwrap_or(0).max(25);
    let header = format!("Session ID{}  Title{}  Updated", " ".repeat(id_width - 10), " ".repeat(title_width - 5));
    let mut lines = vec![header.clone(), "─".repeat(units(&header))];
    for session in sessions {
        lines.push(format!("{}  {}  {}", pad(&session.id, id_width), pad(&truncate(&session.title, title_width), title_width), today_time_or_datetime(session.time.updated)));
    }
    lines.join("\n")
}

fn sized(path: PathBuf) -> Option<PathBuf> {
    std::fs::metadata(&path).is_ok_and(|meta| meta.len() > 0).then_some(path)
}

// `less -R -S`; on Windows a `less.exe` from PATH or next to Git, else `more`.
fn pager() -> Vec<String> {
    let less = |program: &Path| vec![program.to_string_lossy().into_owned(), "-R".into(), "-S".into()];
    if !cfg!(windows) {
        return less(Path::new("less"));
    }
    if let Some(found) = which::which("less").ok().and_then(sized) {
        return less(&found);
    }
    let beside = |anchor: PathBuf| sized(anchor.join("..").join("..").join("usr").join("bin").join("less.exe"));
    let bash = std::env::var_os("OPENCODE_GIT_BASH_PATH").map(PathBuf::from);
    if let Some(found) = bash.and_then(beside).or_else(|| which::which("git").ok().and_then(beside)) {
        return less(&found);
    }
    vec!["cmd".into(), "/c".into(), "more".into()]
}

pub fn list(paths: &Paths, max_count: Option<usize>, format: &str) -> Result<i32> {
    let engine = Engine::new(paths.clone())?;
    let project = opencode_storage::identity::resolve(&std::env::current_dir()?).id;
    let mut sessions: Vec<Session> = engine.store.list_sessions(&project)?.into_iter().filter(|s| !s.is_child()).collect();
    sessions.truncate(max_count.unwrap_or(usize::MAX));
    if sessions.is_empty() {
        return Ok(0);
    }
    let output = if format == "json" {
        let rows: Vec<Value> = sessions
            .iter()
            .map(|s| json!({"id": s.id, "title": s.title, "updated": s.time.updated, "created": s.time.created, "projectId": s.project_id, "directory": s.directory}))
            .collect();
        serde_json::to_string_pretty(&rows)?
    } else {
        table(&sessions)
    };
    if std::io::stdout().is_terminal() && max_count.is_none() && format == "table" {
        let command = pager();
        if let Ok(mut child) = Command::new(&command[0]).args(&command[1..]).stdin(Stdio::piped()).spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(output.as_bytes());
            }
            let _ = child.wait();
            return Ok(0);
        }
    }
    println!("{output}");
    Ok(0)
}

pub fn delete(paths: &Paths, session_id: &str) -> Result<i32> {
    if !Engine::new(paths.clone())?.store.delete_session(session_id)? {
        return Err(fail(format!("Session not found: {session_id}")));
    }
    ui::println(&format!("{SUCCESS_BOLD}Session {session_id} deleted{NORMAL}"));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_counts_utf16_units() {
        assert_eq!(truncate("short", 25), "short");
        assert_eq!(truncate("abcdefghij", 5), "abcd…");
        assert_eq!(truncate("exactly-25-characters-ok!", 25), "exactly-25-characters-ok!");
        assert_eq!(pad("ab", 4), "ab  ");
        assert_eq!(pad("abcdef", 4), "abcdef");
    }

    #[test]
    fn times_use_the_en_us_shapes() {
        let stamp = Local.with_ymd_and_hms(2026, 3, 9, 15, 7, 0).unwrap().timestamp_millis() as u64;
        assert_eq!(time(stamp), "3:07 PM");
        assert_eq!(datetime(stamp), "3:07 PM · 3/9/2026");
        assert_eq!(today_time_or_datetime(stamp), "3:07 PM · 3/9/2026");
        let now = Local::now().timestamp_millis() as u64;
        assert_eq!(today_time_or_datetime(now), time(now));
    }
}
