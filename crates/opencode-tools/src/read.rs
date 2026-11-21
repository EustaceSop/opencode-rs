//! `read` tool (`opencode/src/tool/read.ts`).

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use base64::Engine;
use serde_json::{Value, json};

use crate::util::{
    PathKind, assert_external_directory, de_opt_uint, display, io_err, normalize_path, relative,
    resolve,
};
use crate::{Attachment, PermissionRequest, Tool, ToolContext, ToolError, ToolOutput, parse_args};

const DEFAULT_READ_LIMIT: u64 = 2000;
const MAX_LINE_LENGTH: usize = 2000;
const MAX_LINE_SUFFIX: &str = "... (line truncated to 2000 chars)";
const MAX_BYTES: usize = 50 * 1024;
const MAX_BYTES_LABEL: &str = "50 KB";
const SAMPLE_BYTES: usize = 4096;
const SUPPORTED_IMAGE_MIMES: [&str; 4] = ["image/jpeg", "image/png", "image/gif", "image/webp"];
const BINARY_EXTENSIONS: [&str; 28] = [
    "zip", "tar", "gz", "exe", "dll", "so", "class", "jar", "war", "7z", "doc", "docx", "xls",
    "xlsx", "ppt", "pptx", "odt", "ods", "odp", "bin", "dat", "obj", "o", "a", "lib", "wasm",
    "pyc", "pyo",
];

#[derive(Debug, serde::Deserialize)]
pub struct Params {
    #[serde(rename = "filePath")]
    pub file_path: String,
    #[serde(default, deserialize_with = "de_opt_uint")]
    pub offset: Option<u64>,
    #[serde(default, deserialize_with = "de_opt_uint")]
    pub limit: Option<u64>,
}

pub struct ReadTool;

/// `sniffAttachmentMime` (`util/media.ts:15-26`).
pub(crate) fn sniff_mime(bytes: &[u8], fallback: &str) -> String {
    let mime = if bytes.starts_with(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]) {
        "image/png"
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        "image/jpeg"
    } else if bytes.starts_with(&[0x47, 0x49, 0x46, 0x38]) {
        "image/gif"
    } else if bytes.starts_with(&[0x42, 0x4d]) {
        "image/bmp"
    } else if bytes.starts_with(&[0x25, 0x50, 0x44, 0x46, 0x2d]) {
        "application/pdf"
    } else if bytes.starts_with(b"RIFF") && bytes.len() >= 12 && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        fallback
    };
    mime.to_string()
}

// Only the media types the tool branches on; everything else is treated as text or binary.
fn extension_mime(path: &Path) -> &'static str {
    match extension(path).as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

fn extension(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

fn is_binary(path: &Path, sample: &[u8]) -> bool {
    if BINARY_EXTENSIONS.contains(&extension(path).as_str()) {
        return true;
    }
    if sample.is_empty() {
        return false;
    }
    let mut non_printable = 0usize;
    for b in sample {
        if *b == 0 {
            return true;
        }
        if *b < 9 || (*b > 13 && *b < 32) {
            non_printable += 1;
        }
    }
    non_printable as f64 / sample.len() as f64 > 0.3
}

struct Lines {
    raw: Vec<String>,
    count: u64,
    cut: bool,
    more: bool,
}

fn read_lines(path: &Path, offset: u64, limit: u64) -> std::io::Result<Lines> {
    let start = offset - 1;
    let mut reader = BufReader::new(std::fs::File::open(path)?);
    let mut out = Lines {
        raw: Vec::new(),
        count: 0,
        cut: false,
        more: false,
    };
    let mut bytes = 0usize;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        if buf.last() == Some(&b'\n') {
            buf.pop();
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
        }
        out.count += 1;
        if out.count <= start {
            continue;
        }
        if out.raw.len() as u64 >= limit {
            out.more = true;
            continue;
        }
        let text = String::from_utf8_lossy(&buf);
        let line = match text.char_indices().nth(MAX_LINE_LENGTH) {
            Some((idx, _)) => format!("{}{}", &text[..idx], MAX_LINE_SUFFIX),
            None => text.into_owned(),
        };
        let size = line.len() + usize::from(!out.raw.is_empty());
        if bytes + size <= MAX_BYTES {
            out.raw.push(line);
            bytes += size;
            continue;
        }
        out.cut = true;
        out.more = true;
        break;
    }
    Ok(out)
}

fn list_dir(path: &Path) -> std::io::Result<Vec<String>> {
    let mut items = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let file_type = entry.file_type()?;
        let is_dir = if file_type.is_symlink() {
            std::fs::metadata(entry.path())
                .map(|m| m.is_dir())
                .unwrap_or(false)
        } else {
            file_type.is_dir()
        };
        items.push(if is_dir { format!("{name}/") } else { name });
    }
    // Approximates `localeCompare`: case-insensitive, lowercase first on ties.
    items.sort_by(|a, b| {
        a.to_lowercase()
            .cmp(&b.to_lowercase())
            .then_with(|| b.cmp(a))
    });
    Ok(items)
}

fn miss(path: &Path) -> ToolError {
    let shown = display(path);
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let base = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let mut items: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let lower = name.to_lowercase();
            if lower.contains(&base) || base.contains(&lower) {
                items.push(display(&dir.join(name)));
            }
            if items.len() == 3 {
                break;
            }
        }
    }
    if items.is_empty() {
        ToolError::Other(format!("File not found: {shown}"))
    } else {
        ToolError::Other(format!(
            "File not found: {shown}\n\nDid you mean one of these?\n{}",
            items.join("\n")
        ))
    }
}

/// Rust build, "agent" tool profile: a text-file result as `<line number>\t<text>` rows followed by the footer
/// (continuation hint or end of file). `None` for anything that is not a text-file result.
pub fn numbered(output: &str) -> Option<String> {
    let rest = output.strip_prefix("<path>")?;
    let (_, rest) = rest.split_once("</path>\n<type>file</type>\n<content>\n")?;
    let body = rest.strip_suffix("\n</content>")?;
    // Every content row starts with its number, so the footer is the only "\n\n(" in the body.
    let (rows, footer) = match body.rfind("\n\n(") {
        Some(index) => (&body[..index], &body[index + 2..]),
        None => (body, ""),
    };
    let mut out: Vec<String> = rows
        .split('\n')
        .filter(|row| !row.is_empty())
        .map(|row| match row.split_once(": ") {
            Some((number, text)) if number.bytes().all(|b| b.is_ascii_digit()) => format!("{number}\t{text}"),
            _ => row.to_string(),
        })
        .collect();
    if !footer.is_empty() {
        if !out.is_empty() {
            out.push(String::new());
        }
        out.push(footer.to_string());
    }
    Some(out.join("\n"))
}

fn read_sample(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut sample = Vec::with_capacity(SAMPLE_BYTES);
    std::fs::File::open(path)?
        .take(SAMPLE_BYTES as u64)
        .read_to_end(&mut sample)?;
    Ok(sample)
}

#[async_trait::async_trait]
impl Tool for ReadTool {
    fn id(&self) -> &str {
        "read"
    }

    fn description(&self) -> String {
        include_str!("../../../../opencode-ts/packages/opencode/src/tool/read.txt").to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "$schema": crate::JSON_SCHEMA_URI,
            "type": "object",
            "properties": {
                "filePath": { "type": "string", "description": "The absolute path to the file or directory to read" },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 9007199254740991u64,
                    "description": "The line number to start reading from (1-indexed)"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 9007199254740991u64,
                    "description": "The maximum number of lines to read (defaults to 2000)"
                }
            },
            "required": ["filePath"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let params: Params = parse_args("read", args)?;
        let mut filepath: PathBuf = resolve(&ctx.cwd, &params.file_path);
        if cfg!(windows) {
            filepath = normalize_path(&filepath);
        }
        let shown = display(&filepath);
        let title = relative(&ctx.worktree, &filepath);

        let stat = match tokio::fs::metadata(&filepath).await {
            Ok(meta) => Some(meta),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(io_err(e)),
        };
        let is_dir = stat.as_ref().is_some_and(|m| m.is_dir());

        assert_external_directory(
            ctx,
            &filepath,
            if is_dir {
                PathKind::Directory
            } else {
                PathKind::File
            },
        )
        .await?;
        ctx.permissions
            .ask(PermissionRequest {
                permission: "read".into(),
                patterns: vec![title.clone()],
                always: vec!["*".into()],
                metadata: json!({}),
            })
            .await?;

        if stat.is_none() {
            return Err(miss(&filepath));
        }

        let limit = params.limit.unwrap_or(DEFAULT_READ_LIMIT);
        let offset = params.offset.filter(|o| *o != 0).unwrap_or(1);

        if is_dir {
            let dir = filepath.clone();
            let items = tokio::task::spawn_blocking(move || list_dir(&dir))
                .await
                .map_err(|e| ToolError::Other(e.to_string()))?
                .map_err(io_err)?;
            let start = (offset - 1) as usize;
            let sliced: Vec<String> = items
                .iter()
                .skip(start)
                .take(limit as usize)
                .cloned()
                .collect();
            let truncated = start + sliced.len() < items.len();
            let note = if truncated {
                format!(
                    "\n(Showing {} of {} entries. Use 'offset' parameter to read beyond entry {})",
                    sliced.len(),
                    items.len(),
                    offset as usize + sliced.len()
                )
            } else {
                format!("\n({} entries)", items.len())
            };
            let output = [
                format!("<path>{shown}</path>"),
                "<type>directory</type>".to_string(),
                "<entries>".to_string(),
                sliced.join("\n"),
                note,
                "</entries>".to_string(),
            ]
            .join("\n");
            return Ok(ToolOutput {
                title,
                output,
                metadata: json!({
                    "preview": sliced.iter().take(20).cloned().collect::<Vec<_>>().join("\n"),
                    "truncated": truncated,
                    "loaded": [],
                    "display": {
                        "type": "directory",
                        "path": shown,
                        "entries": sliced,
                        "offset": offset,
                        "totalEntries": items.len(),
                        "truncated": truncated,
                    },
                }),
                attachments: vec![],
            });
        }

        let sample_path = filepath.clone();
        let sample = tokio::task::spawn_blocking(move || read_sample(&sample_path))
            .await
            .map_err(|e| ToolError::Other(e.to_string()))?
            .map_err(io_err)?;
        let mime = sniff_mime(&sample, extension_mime(&filepath));
        let is_pdf = mime == "application/pdf";
        if SUPPORTED_IMAGE_MIMES.contains(&mime.as_str()) || is_pdf {
            let bytes = tokio::fs::read(&filepath).await.map_err(io_err)?;
            let msg = if is_pdf {
                "PDF read successfully"
            } else {
                "Image read successfully"
            };
            let url = format!(
                "data:{mime};base64,{}",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            );
            return Ok(ToolOutput {
                title,
                output: msg.to_string(),
                metadata: json!({ "preview": msg, "truncated": false, "loaded": [] }),
                attachments: vec![Attachment { mime, url }],
            });
        }

        if is_binary(&filepath, &sample) {
            return Err(ToolError::Other(format!(
                "Cannot read binary file: {shown}"
            )));
        }

        let lines_path = filepath.clone();
        let file = tokio::task::spawn_blocking(move || read_lines(&lines_path, offset, limit))
            .await
            .map_err(|e| ToolError::Other(e.to_string()))?
            .map_err(io_err)?;
        if file.count < offset && !(file.count == 0 && offset == 1) {
            return Err(ToolError::Other(format!(
                "Offset {offset} is out of range for this file ({} lines)",
                file.count
            )));
        }

        let mut output = format!("<path>{shown}</path>\n<type>file</type>\n<content>\n");
        let numbered: Vec<String> = file
            .raw
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{}: {line}", i as u64 + offset))
            .collect();
        output.push_str(&numbered.join("\n"));

        let last = (offset + file.raw.len() as u64).saturating_sub(1);
        let next = last + 1;
        let truncated = file.more || file.cut;
        if file.cut {
            output.push_str(&format!(
                "\n\n(Output capped at {MAX_BYTES_LABEL}. Showing lines {offset}-{last}. Use offset={next} to continue.)"
            ));
        } else if file.more {
            output.push_str(&format!(
                "\n\n(Showing lines {offset}-{last} of {}. Use offset={next} to continue.)",
                file.count
            ));
        } else {
            output.push_str(&format!("\n\n(End of file - total {} lines)", file.count));
        }
        output.push_str("\n</content>");

        if let Some(hooks) = &ctx.hooks {
            hooks.touch(&filepath, false).await;
        }

        Ok(ToolOutput {
            title,
            output,
            metadata: json!({
                "preview": file.raw.iter().take(20).cloned().collect::<Vec<_>>().join("\n"),
                "truncated": truncated,
                "loaded": [],
                "display": {
                    "type": "file",
                    "path": shown,
                    "text": file.raw.join("\n"),
                    "lineStart": offset,
                    "lineEnd": last,
                    "totalLines": file.count,
                    "truncated": truncated,
                },
            }),
            attachments: vec![],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Fixture;

    async fn run(t: &Fixture, args: Value) -> Result<ToolOutput, ToolError> {
        ReadTool.execute(args, &t.ctx()).await
    }

    #[tokio::test]
    async fn reads_file_with_line_numbers() {
        let t = Fixture::new();
        let path = t.write("a.txt", "one\r\ntwo\nthree");
        let out = run(&t, json!({"filePath": "a.txt"})).await.unwrap();
        assert_eq!(
            out.output,
            format!(
                "<path>{}</path>\n<type>file</type>\n<content>\n1: one\n2: two\n3: three\n\n(End of file - total 3 lines)\n</content>",
                path.display()
            )
        );
        assert_eq!(out.title, "a.txt");
        assert_eq!(out.metadata["truncated"], false);
        assert_eq!(out.metadata["display"]["totalLines"], 3);
        let reqs = t.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].permission, "read");
        assert_eq!(reqs[0].patterns, vec!["a.txt"]);
        assert_eq!(reqs[0].always, vec!["*"]);
    }

    #[tokio::test]
    async fn numbered_view_uses_tabs_and_keeps_the_footer() {
        let t = Fixture::new();
        t.write("a.txt", "one\n\n3: not a prefix\n4\n5\n");
        let out = run(&t, json!({"filePath": "a.txt", "limit": 3})).await.unwrap();
        assert_eq!(
            numbered(&out.output).unwrap(),
            "1\tone\n2\t\n3\t3: not a prefix\n\n(Showing lines 1-3 of 5. Use offset=4 to continue.)"
        );
        let out = run(&t, json!({"filePath": "a.txt", "offset": 4})).await.unwrap();
        assert_eq!(numbered(&out.output).unwrap(), "4\t4\n5\t5\n\n(End of file - total 5 lines)");
        t.write("empty.txt", "");
        let out = run(&t, json!({"filePath": "empty.txt"})).await.unwrap();
        assert_eq!(numbered(&out.output).unwrap(), "(End of file - total 0 lines)");
        t.write("d/x", "");
        let out = run(&t, json!({"filePath": "d"})).await.unwrap();
        assert_eq!(numbered(&out.output), None);
        assert_eq!(numbered("Image read successfully"), None);
    }

    #[tokio::test]
    async fn offset_limit_and_out_of_range() {
        let t = Fixture::new();
        t.write("a.txt", "1\n2\n3\n4\n5\n");
        let out = run(&t, json!({"filePath": "a.txt", "offset": 2, "limit": 2}))
            .await
            .unwrap();
        assert!(out.output.contains(
            "<content>\n2: 2\n3: 3\n\n(Showing lines 2-3 of 5. Use offset=4 to continue.)"
        ));
        assert_eq!(out.metadata["truncated"], true);
        let err = run(&t, json!({"filePath": "a.txt", "offset": 9}))
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Offset 9 is out of range for this file (5 lines)"
        );
    }

    #[tokio::test]
    async fn empty_file_and_long_lines() {
        let t = Fixture::new();
        t.write("empty.txt", "");
        let out = run(&t, json!({"filePath": "empty.txt"})).await.unwrap();
        assert!(
            out.output
                .ends_with("<content>\n\n\n(End of file - total 0 lines)\n</content>")
        );
        t.write("long.txt", &"x".repeat(2500));
        let out = run(&t, json!({"filePath": "long.txt"})).await.unwrap();
        assert!(out.output.contains(&format!(
            "1: {}... (line truncated to 2000 chars)\n",
            "x".repeat(2000)
        )));
    }

    #[tokio::test]
    async fn byte_cap() {
        let t = Fixture::new();
        t.write("big.txt", &format!("{}\n", "y".repeat(1000)).repeat(100));
        let out = run(&t, json!({"filePath": "big.txt"})).await.unwrap();
        assert!(
            out.output.contains(
                "(Output capped at 50 KB. Showing lines 1-51. Use offset=52 to continue.)"
            )
        );
    }

    #[tokio::test]
    async fn missing_file_suggests() {
        let t = Fixture::new();
        let real = t.write("config.json", "{}");
        let err = run(&t, json!({"filePath": "config"})).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "File not found: {}\n\nDid you mean one of these?\n{}",
                t.root.join("config").display(),
                real.display()
            )
        );
        assert_eq!(t.requests().len(), 1);
    }

    #[tokio::test]
    async fn lists_directory() {
        let t = Fixture::new();
        t.write("d/b.txt", "");
        t.write("d/a.txt", "");
        t.write("d/sub/x", "");
        let out = run(&t, json!({"filePath": "d"})).await.unwrap();
        assert_eq!(
            out.output,
            format!(
                "<path>{}</path>\n<type>directory</type>\n<entries>\na.txt\nb.txt\nsub/\n\n(3 entries)\n</entries>",
                t.root.join("d").display()
            )
        );
        let out = run(&t, json!({"filePath": "d", "limit": 1})).await.unwrap();
        assert!(
            out.output.contains(
                "(Showing 1 of 3 entries. Use 'offset' parameter to read beyond entry 2)"
            )
        );
    }

    #[tokio::test]
    async fn binary_and_image() {
        let t = Fixture::new();
        t.write("x.bin", "text");
        let err = run(&t, json!({"filePath": "x.bin"})).await.unwrap_err();
        assert!(err.to_string().starts_with("Cannot read binary file: "));
        std::fs::write(t.root.join("nul.txt"), [b'a', 0, b'b']).unwrap();
        assert!(run(&t, json!({"filePath": "nul.txt"})).await.is_err());
        std::fs::write(
            t.root.join("pic"),
            [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 1, 2],
        )
        .unwrap();
        let out = run(&t, json!({"filePath": "pic"})).await.unwrap();
        assert_eq!(out.output, "Image read successfully");
        assert_eq!(out.attachments[0].mime, "image/png");
        assert!(
            out.attachments[0]
                .url
                .starts_with("data:image/png;base64,iVBORw0KGgo")
        );
    }

    #[tokio::test]
    async fn external_directory_asked_and_rejection_propagates() {
        let t = Fixture::new();
        let other = Fixture::new();
        let path = other.write("o.txt", "hi");
        let out = run(&t, json!({"filePath": path.to_string_lossy()}))
            .await
            .unwrap();
        assert!(out.output.contains("1: hi"));
        let reqs = t.requests();
        assert_eq!(reqs[0].permission, "external_directory");
        let glob = other.root.join("*").to_string_lossy().into_owned();
        let glob = if cfg!(windows) {
            glob
        } else {
            glob.replace('\\', "/")
        };
        assert_eq!(reqs[0].patterns, vec![glob.clone()]);
        assert_eq!(reqs[0].always, vec![glob]);
        assert_eq!(reqs[1].permission, "read");

        t.deny("read");
        t.write("a.txt", "x");
        assert!(matches!(
            run(&t, json!({"filePath": "a.txt"})).await,
            Err(ToolError::Rejected)
        ));
    }
}
