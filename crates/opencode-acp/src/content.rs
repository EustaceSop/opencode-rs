// ACP content blocks to prompt parts and stored parts back to content chunks (`acp/content.ts`).
use base64::Engine as _;
use opencode_storage::{FilePart, Part, PartBody};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Annotations {
    #[serde(default)]
    pub audience: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Resource {
    Text {
        uri: String,
        text: String,
    },
    Blob {
        uri: String,
        blob: String,
        #[serde(rename = "mimeType", default)]
        mime_type: Option<String>,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(default)]
        annotations: Option<Annotations>,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
        #[serde(default)]
        uri: Option<String>,
    },
    Audio {
        #[allow(dead_code)]
        data: String,
    },
    ResourceLink {
        name: String,
        uri: String,
        #[serde(rename = "mimeType", default)]
        mime_type: Option<String>,
    },
    Resource {
        resource: Resource,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum PromptPart {
    Text { text: String, synthetic: bool, ignored: bool },
    File(FilePart),
}

fn text(text: impl Into<String>) -> PromptPart {
    PromptPart::Text { text: text.into(), synthetic: false, ignored: false }
}

fn file(url: impl Into<String>, filename: impl Into<String>, mime: impl Into<String>) -> PromptPart {
    PromptPart::File(FilePart { mime: mime.into(), filename: Some(filename.into()), url: url.into(), source: None })
}

// The parts of a URL the conversions look at: (scheme, host, pathname, query, hash).
fn split_url(uri: &str) -> Option<(&str, &str, &str, &str, &str)> {
    let colon = uri.find(':')?;
    let scheme = &uri[..colon];
    let mut chars = scheme.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_alphabetic()) || !chars.all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) {
        return None;
    }
    let rest = &uri[colon + 1..];
    let (rest, hash) = rest.find('#').map_or((rest, ""), |at| (&rest[..at], &rest[at..]));
    let (rest, query) = rest.find('?').map_or((rest, ""), |at| (&rest[..at], &rest[at + 1..]));
    match rest.strip_prefix("//") {
        Some(authority) => {
            let end = authority.find('/').unwrap_or(authority.len());
            Some((scheme, &authority[..end], &authority[end..], query, hash))
        }
        None => Some((scheme, "", rest, query, hash)),
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = (bytes[i] == b'%').then(|| text.get(i + 1..i + 3)).flatten().and_then(|h| u8::from_str_radix(h, 16).ok());
        match hex {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['/', '\\']);
    trimmed.rsplit(['/', '\\']).next().unwrap_or_default()
}

fn filename_from_uri(uri: Option<&str>) -> Option<String> {
    let uri = uri.filter(|u| !u.is_empty())?;
    if uri.starts_with("data:") {
        return None;
    }
    let name = match split_url(uri) {
        Some((_, _, pathname, _, _)) => basename(pathname),
        None => basename(uri),
    };
    (!name.is_empty()).then(|| name.to_string())
}

// `fileURLToPath` with forward slashes, falling back to the decoded pathname where it would throw.
fn file_url_path(host: &str, pathname: &str) -> String {
    let decoded = percent_decode(pathname);
    if cfg!(windows) {
        if let Some(drive) = decoded.strip_prefix('/').filter(|p| p.as_bytes().get(1) == Some(&b':')) {
            return drive.replace('\\', "/");
        }
        if !host.is_empty() {
            return format!("//{host}{decoded}");
        }
    }
    decoded
}

fn audience_flags(annotations: Option<&Annotations>) -> (bool, bool) {
    match annotations.and_then(|a| a.audience.as_deref()) {
        Some([only]) if only == "assistant" => (true, false),
        Some([only]) if only == "user" => (false, true),
        _ => (false, false),
    }
}

fn resource_link(uri: &str, mime: &str, name: &str) -> PromptPart {
    if uri.starts_with("file://") {
        return file(uri, name, mime);
    }
    if uri.starts_with("zed://")
        && let Some((_, _, _, query, _)) = split_url(uri)
        && let Some(path) = query.split('&').find_map(|pair| pair.split_once('=').filter(|(k, _)| *k == "path").map(|(_, v)| percent_decode(&v.replace('+', " "))))
        && !path.is_empty()
    {
        return file(opencode_session::file_url(Path::new(&path)), name, mime);
    }
    text(uri)
}

pub fn block_parts(block: &ContentBlock) -> Vec<PromptPart> {
    match block {
        ContentBlock::Text { text, annotations } => {
            let (synthetic, ignored) = audience_flags(annotations.as_ref());
            vec![PromptPart::Text { text: text.clone(), synthetic, ignored }]
        }
        ContentBlock::Image { data, mime_type, uri } => {
            let name = || filename_from_uri(uri.as_deref()).unwrap_or_else(|| "image".into());
            if !data.is_empty() {
                return vec![file(format!("data:{mime_type};base64,{data}"), name(), mime_type)];
            }
            match uri.as_deref() {
                Some(uri) if uri.starts_with("data:") || uri.starts_with("http://") || uri.starts_with("https://") => vec![file(uri, name(), mime_type)],
                _ => Vec::new(),
            }
        }
        ContentBlock::Audio { .. } => Vec::new(),
        ContentBlock::ResourceLink { name, uri, mime_type } => vec![resource_link(uri, mime_type.as_deref().unwrap_or("text/plain"), name)],
        ContentBlock::Resource { resource: Resource::Text { uri, text: body } } => match split_url(uri) {
            Some((scheme, host, pathname, _, hash)) if scheme.eq_ignore_ascii_case("file") => {
                let line = hash.strip_prefix("#L").map(|rest| rest.bytes().take_while(u8::is_ascii_digit).count()).filter(|n| *n > 0).map(|n| &hash[2..2 + n]);
                let location = file_url_path(host, pathname);
                let suffix = line.map(|l| format!(":{l}")).unwrap_or_default();
                vec![text(format!("[{location}{suffix}]\n{body}"))]
            }
            _ => vec![text(format!("[{uri}]\n{body}"))],
        },
        ContentBlock::Resource { resource: Resource::Blob { uri, blob, mime_type } } => match mime_type.as_deref().filter(|m| !m.is_empty()) {
            Some(mime) => {
                let url = if uri.starts_with("data:") { uri.clone() } else { format!("data:{mime};base64,{blob}") };
                vec![file(url, filename_from_uri(Some(uri)).unwrap_or_else(|| "file".into()), mime)]
            }
            None => Vec::new(),
        },
    }
}

pub fn prompt_parts(blocks: &[ContentBlock]) -> Vec<PromptPart> {
    blocks.iter().flat_map(block_parts).collect()
}

// `detectSlashCommand`: "/name args..." across the text parts.
pub fn slash_command(parts: &[PromptPart]) -> Option<(String, String)> {
    let joined: String = parts
        .iter()
        .filter_map(|p| match p {
            PromptPart::Text { text, .. } => Some(text.as_str()),
            PromptPart::File(_) => None,
        })
        .collect();
    let rest = joined.trim().strip_prefix('/')?;
    if rest.starts_with(char::is_whitespace) {
        return None;
    }
    let mut words = rest.split_whitespace();
    let name = words.next()?;
    Some((name.to_string(), words.collect::<Vec<_>>().join(" ")))
}

fn lenient_base64(data: &str) -> Vec<u8> {
    let cleaned: String = data.chars().filter_map(|c| match c {
        '-' => Some('+'),
        '_' => Some('/'),
        c if c.is_ascii_alphanumeric() || c == '+' || c == '/' => Some(c),
        _ => None,
    }).collect();
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new().with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent).with_decode_allow_trailing_bits(true),
    );
    let usable = cleaned.len() - usize::from(cleaned.len() % 4 == 1);
    engine.decode(&cleaned[..usable]).unwrap_or_default()
}

// `pathToFileURL(name).href`: relative names resolve against the process directory.
fn file_href(name: &str) -> String {
    let path = PathBuf::from(name);
    let absolute = if path.is_absolute() { path } else { PathBuf::from(crate::tool::resolve(&std::env::current_dir().unwrap_or_default().to_string_lossy(), name)) };
    opencode_session::file_url(&absolute)
}

fn file_chunks(part: &FilePart) -> Vec<Value> {
    if part.url.starts_with("file://") {
        return vec![json!({"content": {"type": "resource_link", "uri": part.url, "name": part.filename.as_deref().unwrap_or("file"), "mimeType": part.mime}})];
    }
    // `^data:([^;]+);base64,(.*)$`
    let Some((mime, data)) = part.url.strip_prefix("data:").and_then(|rest| rest.split_once(";base64,")) else { return Vec::new() };
    if mime.is_empty() || mime.contains(';') || data.contains(['\n', '\r']) {
        return Vec::new();
    }
    if mime.starts_with("image/") {
        return vec![json!({"content": {"type": "image", "mimeType": mime, "data": data, "uri": file_href(part.filename.as_deref().unwrap_or("image"))}})];
    }
    let uri = file_href(part.filename.as_deref().unwrap_or("file"));
    let resource = if mime.starts_with("text/") || mime == "application/json" {
        json!({"uri": uri, "mimeType": mime, "text": String::from_utf8_lossy(&lenient_base64(data))})
    } else {
        json!({"uri": uri, "mimeType": mime, "blob": data})
    };
    vec![json!({"content": {"type": "resource", "resource": resource}})]
}

// `partToContentChunks` for text, file and reasoning parts.
pub fn part_chunks(part: &Part) -> Vec<Value> {
    match &part.body {
        PartBody::Text(body) if !body.text.is_empty() => {
            let mut content = Map::new();
            content.insert("type".into(), "text".into());
            content.insert("text".into(), body.text.clone().into());
            let audience = if body.synthetic == Some(true) {
                Some("assistant")
            } else if body.ignored == Some(true) {
                Some("user")
            } else {
                None
            };
            if let Some(audience) = audience {
                content.insert("annotations".into(), json!({"audience": [audience]}));
            }
            vec![json!({"content": content})]
        }
        PartBody::Reasoning(body) if !body.text.is_empty() => vec![json!({"content": {"type": "text", "text": body.text}})],
        PartBody::File(body) => file_chunks(body),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocks(value: Value) -> Vec<ContentBlock> {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn blocks_become_text_and_file_parts() {
        let parts = prompt_parts(&blocks(json!([
            {"type": "text", "text": "hi", "annotations": {"audience": ["assistant"]}},
            {"type": "image", "data": "AAA", "mimeType": "image/png", "uri": "https://x.test/a/pic.png?s=1"},
            {"type": "resource_link", "name": "main.rs", "uri": "file:///tmp/main.rs"},
            {"type": "resource_link", "name": "n", "uri": "https://example.com"},
            {"type": "resource", "resource": {"uri": "file:///tmp/a.txt#L12", "text": "body"}},
            {"type": "resource", "resource": {"uri": "mem://x", "text": "body"}},
            {"type": "resource", "resource": {"uri": "mem://blob.bin", "blob": "AQID", "mimeType": "application/octet-stream"}},
            {"type": "audio", "data": "AAA", "mimeType": "audio/wav"}
        ])));
        assert_eq!(parts[0], PromptPart::Text { text: "hi".into(), synthetic: true, ignored: false });
        assert_eq!(parts[1], file("data:image/png;base64,AAA", "pic.png", "image/png"));
        assert_eq!(parts[2], file("file:///tmp/main.rs", "main.rs", "text/plain"));
        assert_eq!(parts[3], text("https://example.com"));
        assert_eq!(parts[4], text("[/tmp/a.txt:12]\nbody"));
        assert_eq!(parts[5], text("[mem://x]\nbody"));
        // `new URL("mem://blob.bin").pathname` is empty: the name is the host.
        assert_eq!(parts[6], file("data:application/octet-stream;base64,AQID", "file", "application/octet-stream"));
        assert_eq!(parts.len(), 7);
    }

    #[test]
    fn slash_commands_need_a_name_right_after_the_slash() {
        let parts = |t: &str| vec![text(t)];
        assert_eq!(slash_command(&parts("  /review  this   file ")), Some(("review".into(), "this file".into())));
        assert_eq!(slash_command(&parts("/compact")), Some(("compact".into(), String::new())));
        assert_eq!(slash_command(&parts("/ review")), None);
        assert_eq!(slash_command(&parts("hello /x")), None);
    }

    #[test]
    fn base64_decoding_is_lenient() {
        assert_eq!(lenient_base64("aGk="), b"hi");
        assert_eq!(lenient_base64("aGk"), b"hi");
        assert_eq!(lenient_base64("aG\nk="), b"hi");
    }
}
