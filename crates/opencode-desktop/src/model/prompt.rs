// Prompt-input logic without a text widget: popup triggers, fuzzy ranking, history, attachments.
use std::path::Path;
use std::sync::Arc;

pub const HISTORY_LIMIT: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    // The whole text up to the cursor is `/name`.
    Slash { query: String },
    // `@query` ends at the cursor; `start` is the byte offset of the `@`.
    Mention { start: usize, query: String },
}

pub fn trigger(text: &str, cursor: usize) -> Option<Trigger> {
    let before = text.get(..cursor.min(text.len()))?;
    if let Some(rest) = before.strip_prefix('/')
        && !rest.contains(char::is_whitespace)
    {
        return Some(Trigger::Slash { query: rest.to_string() });
    }
    let start = before.rfind('@')?;
    let query = &before[start + 1..];
    let boundary = before[..start].chars().next_back().is_none_or(char::is_whitespace);
    (boundary && !query.contains(char::is_whitespace)).then(|| Trigger::Mention { start, query: query.to_string() })
}

// `/name arguments` as typed; None when the text is not a slash command.
pub fn parse_slash(text: &str) -> Option<(&str, &str)> {
    let rest = text.trim_start().strip_prefix('/')?;
    let (name, arguments) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    (!name.is_empty() && !name.contains('/')).then_some((name, arguments.trim()))
}

// Subsequence match, case-insensitive; higher is better. Rewards prefix, word-start and consecutive hits, shorter candidates.
pub fn fuzzy(query: &str, candidate: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let mut wanted = query.chars().flat_map(char::to_lowercase).peekable();
    let (mut score, mut streak, mut previous) = (0i32, 0i32, None::<char>);
    for (index, ch) in candidate.chars().enumerate() {
        let Some(&next) = wanted.peek() else { break };
        if ch.to_lowercase().next() == Some(next) {
            wanted.next();
            streak += 1;
            score += 10 + streak * 5;
            if index == 0 {
                score += 25;
            } else if previous.is_some_and(|p| matches!(p, '/' | '\\' | '_' | '-' | '.' | ' ')) {
                score += 15;
            }
        } else {
            streak = 0;
        }
        previous = Some(ch);
    }
    wanted.peek().is_none().then(|| score - candidate.chars().count() as i32 / 4)
}

// Indices of the best matches, best first; ties keep the input order.
pub fn rank<'a>(query: &str, candidates: impl IntoIterator<Item = &'a str>, limit: usize) -> Vec<usize> {
    let mut scored: Vec<(i32, usize)> = candidates.into_iter().enumerate().filter_map(|(i, c)| fuzzy(query, c).map(|s| (s, i))).collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().take(limit).map(|(_, i)| i).collect()
}

// File names weigh more than the directories leading to them.
pub fn rank_paths(query: &str, paths: &[String], limit: usize) -> Vec<usize> {
    let mut scored: Vec<(i32, usize)> = paths
        .iter()
        .enumerate()
        .filter_map(|(index, path)| {
            let name = path.rsplit('/').next().unwrap_or(path);
            let by_name = fuzzy(query, name).map(|s| s + 40);
            by_name.or_else(|| fuzzy(query, path)).map(|score| (score, index))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().take(limit).map(|(_, i)| i).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    New,
    Undo,
    Redo,
    Compact,
    Fork,
    Model,
    Agent,
}

impl Builtin {
    pub const ALL: [Builtin; 7] = [Builtin::New, Builtin::Undo, Builtin::Redo, Builtin::Compact, Builtin::Fork, Builtin::Model, Builtin::Agent];

    pub fn name(self) -> &'static str {
        match self {
            Builtin::New => "new",
            Builtin::Undo => "undo",
            Builtin::Redo => "redo",
            Builtin::Compact => "compact",
            Builtin::Fork => "fork",
            Builtin::Model => "model",
            Builtin::Agent => "agent",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Builtin::New => "command.session.new",
            Builtin::Undo => "command.session.undo.description",
            Builtin::Redo => "command.session.redo.description",
            Builtin::Compact => "command.session.compact.description",
            Builtin::Fork => "command.session.fork.description",
            Builtin::Model => "command.model.choose.description",
            Builtin::Agent => "command.agent.cycle.description",
        }
    }

    // Rust build: names other coding agents use for the same command (Claude Code, Codex, Gemini CLI).
    pub fn aliases(self) -> &'static [&'static str] {
        match self {
            Builtin::New => &["clear", "reset"],
            Builtin::Undo => &["rewind"],
            Builtin::Compact => &["compress", "summarize"],
            Builtin::Model => &["models"],
            Builtin::Agent => &["agents", "mode"],
            Builtin::Redo | Builtin::Fork => &[],
        }
    }

    pub fn find(name: &str) -> Option<Builtin> {
        Builtin::ALL.into_iter().find(|b| b.name() == name || b.aliases().contains(&name))
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct History {
    entries: Vec<String>,
    // Position while browsing; `stash` is what was being typed before.
    cursor: Option<usize>,
    stash: String,
}

impl History {
    pub fn new(entries: Vec<String>) -> Self {
        Self { entries, ..Default::default() }
    }

    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    pub fn push(&mut self, text: &str) {
        self.cursor = None;
        self.stash.clear();
        let text = text.trim();
        if text.is_empty() || self.entries.last().is_some_and(|last| last == text) {
            return;
        }
        self.entries.push(text.to_string());
        let extra = self.entries.len().saturating_sub(HISTORY_LIMIT);
        self.entries.drain(..extra);
    }

    pub fn browsing(&self) -> bool {
        self.cursor.is_some()
    }

    pub fn reset(&mut self) {
        self.cursor = None;
        self.stash.clear();
    }

    pub fn up(&mut self, current: &str) -> Option<String> {
        let next = match self.cursor {
            None => self.entries.len().checked_sub(1)?,
            Some(0) => return None,
            Some(index) => index - 1,
        };
        if self.cursor.is_none() {
            self.stash = current.to_string();
        }
        self.cursor = Some(next);
        Some(self.entries[next].clone())
    }

    pub fn down(&mut self) -> Option<String> {
        let index = self.cursor?;
        if index + 1 < self.entries.len() {
            self.cursor = Some(index + 1);
            return Some(self.entries[index + 1].clone());
        }
        self.cursor = None;
        Some(std::mem::take(&mut self.stash))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub name: Arc<str>,
    pub mime: Arc<str>,
    // `file:` for files on disk, `data:` for pasted images.
    pub url: Arc<str>,
}

// The original's accepted attachment types (`app/src/constants/file-picker.ts`); everything else is sent as plain text.
pub fn mime_for(path: &Path) -> &'static str {
    let extension = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "json" => "application/json",
        "toml" => "application/toml",
        "yaml" | "yml" => "application/yaml",
        "xml" => "application/xml",
        "md" => "text/markdown",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "csv" => "text/csv",
        _ => "text/plain",
    }
}

pub fn file_attachment(path: &Path) -> Attachment {
    let name = path.file_name().map_or_else(|| path.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned());
    Attachment { name: name.into(), mime: mime_for(path).into(), url: opencode_session::file_url(path).into() }
}

pub fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = u32::from(chunk[0]) << 16 | u32::from(*chunk.get(1).unwrap_or(&0)) << 8 | u32::from(*chunk.get(2).unwrap_or(&0));
        for (index, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            let sextet = TABLE[(n >> shift & 63) as usize] as char;
            out.push(if index <= chunk.len() { sextet } else { '=' });
        }
    }
    out
}

pub fn image_attachment(name: &str, mime: &str, bytes: &[u8]) -> Attachment {
    Attachment { name: name.into(), mime: mime.into(), url: format!("data:{mime};base64,{}", base64(bytes)).into() }
}

// Adds what is not attached yet; returns how many were new.
pub fn attach(list: &mut Vec<Attachment>, new: impl IntoIterator<Item = Attachment>) -> usize {
    let before = list.len();
    for attachment in new {
        if !list.iter().any(|a| a.url == attachment.url) {
            list.push(attachment);
        }
    }
    list.len() - before
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_follow_the_cursor() {
        assert_eq!(trigger("/", 1), Some(Trigger::Slash { query: "".into() }));
        assert_eq!(trigger("/com", 4), Some(Trigger::Slash { query: "com".into() }));
        assert_eq!(trigger("/compact now", 12), None);
        assert_eq!(trigger("/compact now", 3), Some(Trigger::Slash { query: "co".into() }));
        assert_eq!(trigger("see /x", 6), None);

        assert_eq!(trigger("look at @src/ma", 15), Some(Trigger::Mention { start: 8, query: "src/ma".into() }));
        assert_eq!(trigger("@", 1), Some(Trigger::Mention { start: 0, query: "".into() }));
        assert_eq!(trigger("mail me@example", 15), None);
        assert_eq!(trigger("@a b", 4), None);
        assert_eq!(trigger("héllo @ñ", "héllo @ñ".len()), Some(Trigger::Mention { start: 7, query: "ñ".into() }));
        assert_eq!(trigger("abc", 99), None);
        assert_eq!(trigger("plain", 5), None);
    }

    #[test]
    fn slash_commands_split_into_name_and_arguments() {
        assert_eq!(parse_slash("/compact"), Some(("compact", "")));
        assert_eq!(parse_slash("  /review  src/a.rs  now "), Some(("review", "src/a.rs  now")));
        assert_eq!(parse_slash("/"), None);
        assert_eq!(parse_slash("/usr/bin/env"), None);
        assert_eq!(parse_slash("not /a command"), None);
        assert_eq!(Builtin::find("undo"), Some(Builtin::Undo));
        assert_eq!(Builtin::find("nope"), None);
    }

    #[test]
    fn fuzzy_ranking_prefers_prefixes_and_file_names() {
        assert!(fuzzy("xyz", "src/main.rs").is_none());
        assert!(fuzzy("", "anything").is_some());
        assert!(fuzzy("MAIN", "src/main.rs").is_some());
        assert_eq!(rank("co", ["fork", "compact", "undo", "model-config"], 5), [1, 3]);
        assert_eq!(rank("", ["b", "a"], 1), [0]);

        let paths: Vec<String> = ["docs/main/index.md", "src/main.rs", "src/domain.rs", "README.md"].iter().map(|s| s.to_string()).collect();
        let ranked = rank_paths("main", &paths, 10);
        assert_eq!(ranked[0], 1, "a file-name hit beats a directory hit");
        assert_eq!(ranked.len(), 3);
        assert!(rank_paths("zzz", &paths, 10).is_empty());
    }

    #[test]
    fn history_walks_back_and_returns_to_the_draft() {
        let mut history = History::default();
        assert_eq!(history.up("draft"), None);
        history.push("first");
        history.push("second");
        history.push("second");
        history.push("   ");
        assert_eq!(history.entries(), ["first", "second"]);

        assert_eq!(history.up("draft").as_deref(), Some("second"));
        assert!(history.browsing());
        assert_eq!(history.up("ignored").as_deref(), Some("first"));
        assert_eq!(history.up("ignored"), None);
        assert_eq!(history.down().as_deref(), Some("second"));
        assert_eq!(history.down().as_deref(), Some("draft"));
        assert_eq!(history.down(), None);
        assert!(!history.browsing());

        for n in 0..HISTORY_LIMIT + 5 {
            history.push(&format!("entry {n}"));
        }
        assert_eq!(history.entries().len(), HISTORY_LIMIT);
        assert_eq!(history.entries().last().unwrap(), &format!("entry {}", HISTORY_LIMIT + 4));
    }

    #[test]
    fn attachments_carry_mime_and_url() {
        assert_eq!(mime_for(Path::new("a/B.PNG")), "image/png");
        assert_eq!(mime_for(Path::new("notes.pdf")), "application/pdf");
        assert_eq!(mime_for(Path::new("main.rs")), "text/plain");
        assert_eq!(mime_for(Path::new("Makefile")), "text/plain");

        let path = if cfg!(windows) { Path::new(r"C:\work\a b.png") } else { Path::new("/work/a b.png") };
        let file = file_attachment(path);
        assert_eq!((file.name.as_ref(), file.mime.as_ref()), ("a b.png", "image/png"));
        assert!(file.url.starts_with("file:///") && file.url.ends_with("/a%20b.png"), "{}", file.url);

        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        let image = image_attachment("pasted.png", "image/png", b"hi");
        assert_eq!(image.url.as_ref(), "data:image/png;base64,aGk=");

        let mut list = Vec::new();
        assert_eq!(attach(&mut list, [file.clone(), image.clone(), file.clone()]), 2);
        assert_eq!(attach(&mut list, [image]), 0);
    }
}
