//! Shared output truncation (`opencode/src/tool/truncate.ts`).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::util::tool_id;

pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 50 * 1024;
const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    #[default]
    Head,
    Tail,
}

#[derive(Debug, Clone)]
pub struct TruncateOptions {
    pub max_lines: usize,
    pub max_bytes: usize,
    pub direction: Direction,
    /// Agent may use the task tool (`evaluate("task","*",agent.permission).action != "deny"`).
    pub has_task_tool: bool,
}

impl Default for TruncateOptions {
    fn default() -> Self {
        Self {
            max_lines: MAX_LINES,
            max_bytes: MAX_BYTES,
            direction: Direction::Head,
            has_task_tool: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Truncated {
    pub content: String,
    pub truncated: bool,
    pub output_path: Option<PathBuf>,
}

pub fn truncation_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("tool-output")
}

/// `Truncate.write`: store the full text under `<data_dir>/tool-output/tool_<id>`.
pub fn write_output(data_dir: &Path, text: &str) -> std::io::Result<PathBuf> {
    let dir = truncation_dir(data_dir);
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(tool_id());
    std::fs::write(&file, text)?;
    Ok(file)
}

pub fn truncate_output(
    text: &str,
    data_dir: &Path,
    opts: &TruncateOptions,
) -> std::io::Result<Truncated> {
    let lines: Vec<&str> = text.split('\n').collect();
    let total_bytes = text.len();
    if lines.len() <= opts.max_lines && total_bytes <= opts.max_bytes {
        return Ok(Truncated {
            content: text.to_string(),
            truncated: false,
            output_path: None,
        });
    }

    let mut out: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    let mut hit_bytes = false;
    match opts.direction {
        Direction::Head => {
            for (i, line) in lines.iter().enumerate().take(opts.max_lines) {
                let size = line.len() + usize::from(i > 0);
                if bytes + size > opts.max_bytes {
                    hit_bytes = true;
                    break;
                }
                out.push(line);
                bytes += size;
            }
        }
        Direction::Tail => {
            for line in lines.iter().rev() {
                if out.len() >= opts.max_lines {
                    break;
                }
                let size = line.len() + usize::from(!out.is_empty());
                if bytes + size > opts.max_bytes {
                    hit_bytes = true;
                    break;
                }
                out.push(line);
                bytes += size;
            }
            out.reverse();
        }
    }

    let removed = if hit_bytes {
        total_bytes - bytes
    } else {
        lines.len() - out.len()
    };
    let unit = if hit_bytes { "bytes" } else { "lines" };
    let preview = out.join("\n");
    let file = write_output(data_dir, text)?;
    let shown = file.to_string_lossy();
    let hint = if opts.has_task_tool {
        format!(
            "The tool call succeeded but the output was truncated. Full output saved to: {shown}\nUse the Task tool to have explore agent process this file with Grep and Read (with offset/limit). Do NOT read the full file yourself - delegate to save context."
        )
    } else {
        format!(
            "The tool call succeeded but the output was truncated. Full output saved to: {shown}\nUse Grep to search the full content or Read with offset/limit to view specific sections."
        )
    };
    let content = match opts.direction {
        Direction::Head => format!("{preview}\n\n...{removed} {unit} truncated...\n\n{hint}"),
        Direction::Tail => format!("...{removed} {unit} truncated...\n\n{hint}\n\n{preview}"),
    };
    Ok(Truncated {
        content,
        truncated: true,
        output_path: Some(file),
    })
}

/// `Truncate.cleanup`: delete `tool_*` files older than 7 days.
pub fn cleanup_tool_output(data_dir: &Path) {
    let dir = truncation_dir(data_dir);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let Some(cutoff) = SystemTime::now().checked_sub(RETENTION) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with("tool_") {
            continue;
        }
        let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if mtime >= cutoff {
            continue;
        }
        let _ = std::fs::remove_file(entry.path());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn within_limits_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let r = truncate_output("a\nb", dir.path(), &TruncateOptions::default()).unwrap();
        assert_eq!(
            r,
            Truncated {
                content: "a\nb".into(),
                truncated: false,
                output_path: None
            }
        );
        assert!(!truncation_dir(dir.path()).exists());
    }

    #[test]
    fn head_by_lines() {
        let dir = tempfile::tempdir().unwrap();
        let text = (0..10)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let opts = TruncateOptions {
            max_lines: 3,
            ..Default::default()
        };
        let r = truncate_output(&text, dir.path(), &opts).unwrap();
        let file = r.output_path.clone().unwrap();
        assert!(file.starts_with(dir.path().join("tool-output")));
        assert!(
            file.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("tool_")
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
        assert_eq!(
            r.content,
            format!(
                "l0\nl1\nl2\n\n...7 lines truncated...\n\nThe tool call succeeded but the output was truncated. Full output saved to: {}\nUse Grep to search the full content or Read with offset/limit to view specific sections.",
                file.display()
            )
        );
    }

    #[test]
    fn head_by_bytes_and_task_hint() {
        let dir = tempfile::tempdir().unwrap();
        let opts = TruncateOptions {
            max_bytes: 7,
            has_task_tool: true,
            ..Default::default()
        };
        let r = truncate_output("aaa\nbbb\nccc", dir.path(), &opts).unwrap();
        assert!(
            r.content
                .starts_with("aaa\nbbb\n\n...4 bytes truncated...\n\n")
        );
        assert!(
            r.content
                .contains("Use the Task tool to have explore agent process this file")
        );
    }

    #[test]
    fn tail_direction() {
        let dir = tempfile::tempdir().unwrap();
        let opts = TruncateOptions {
            max_lines: 2,
            direction: Direction::Tail,
            ..Default::default()
        };
        let r = truncate_output("a\nb\nc\nd", dir.path(), &opts).unwrap();
        assert!(r.content.starts_with("...2 lines truncated...\n\n"));
        assert!(r.content.ends_with("\n\nc\nd"));
    }

    #[test]
    fn cleanup_keeps_recent_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_output(dir.path(), "x").unwrap();
        cleanup_tool_output(dir.path());
        assert!(file.exists());
    }
}
