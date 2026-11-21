// Tool parts as ACP tool calls (`acp/tool.ts`).
use opencode_storage::{FileAttachment, ToolStateCompleted, ToolStateError};
use serde_json::{Map, Value, json};
use std::path::{Component, Path, PathBuf};

pub type Input = Map<String, Value>;

pub fn kind(tool: &str) -> &'static str {
    match tool.to_lowercase().as_str() {
        "bash" | "shell" => "execute",
        "webfetch" => "fetch",
        "edit" | "apply_patch" | "patch" | "write" => "edit",
        "grep" | "glob" | "context" | "context7_resolve_library_id" | "context7_get_library_docs" => "search",
        "read" => "read",
        "task" => "think",
        _ => "other",
    }
}

pub fn string(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str)
}

// `a ?? b ?? ...` over keys: the first value that is neither missing nor null.
pub fn nullish<'a>(input: &'a Input, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| input.get(*key).filter(|value| !value.is_null()))
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

fn is_shell(tool: &str) -> bool {
    matches!(tool.to_lowercase().as_str(), "bash" | "shell")
}

fn is_absolute(path: &str) -> bool {
    let path = Path::new(path);
    path.is_absolute() || path.has_root()
}

// `path.resolve(base, value)`.
pub fn resolve(base: &str, value: &str) -> String {
    let base = if is_absolute(base) { PathBuf::from(base) } else { std::env::current_dir().unwrap_or_default().join(base) };
    let mut out = PathBuf::new();
    for component in base.join(value).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out.to_string_lossy().into_owned()
}

fn resolve_path(value: Option<&str>, cwd: Option<&str>) -> Option<String> {
    let value = value.filter(|v| !v.is_empty())?;
    if is_absolute(value) {
        return Some(value.to_string());
    }
    let base = cwd.map(str::to_string).unwrap_or_else(|| std::env::current_dir().unwrap_or_default().to_string_lossy().into_owned());
    Some(resolve(&base, value))
}

fn shell_workdir(input: &Input, cwd: Option<&str>) -> Option<String> {
    let explicit = string(input.get("workdir")).or_else(|| string(input.get("cwd")));
    resolve_path(explicit, cwd).or_else(|| cwd.map(str::to_string))
}

fn shell_command(input: &Input) -> Option<&str> {
    string(input.get("command")).or_else(|| string(input.get("cmd")))
}

// Shell tools show their command; the rest keep the model-provided title.
pub fn title(tool: &str, input: &Input, fallback: Option<&str>) -> String {
    if is_shell(tool) {
        return shell_command(input).or(fallback).unwrap_or(tool).to_string();
    }
    fallback.filter(|f| !f.is_empty()).unwrap_or(tool).to_string()
}

fn raw_input(tool: &str, input: &Input, cwd: Option<&str>) -> Value {
    if !is_shell(tool) || truthy(input.get("cwd")) || truthy(input.get("workdir")) {
        return Value::Object(input.clone());
    }
    let mut out = input.clone();
    if let Some(workdir) = shell_workdir(input, cwd) {
        out.insert("cwd".into(), workdir.into());
    }
    Value::Object(out)
}

pub fn location_from(values: &[Option<&Value>]) -> Vec<Value> {
    let mut paths: Vec<&str> = Vec::new();
    for value in values.iter().flatten() {
        let found: Vec<&str> = match value {
            Value::Array(items) => items.iter().filter_map(Value::as_str).filter(|s| !s.is_empty()).collect(),
            Value::String(s) if !s.is_empty() => vec![s.as_str()],
            _ => Vec::new(),
        };
        for path in found {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    paths.into_iter().map(|path| json!({"path": path})).collect()
}

pub fn locations(tool: &str, input: &Input, cwd: Option<&str>) -> Vec<Value> {
    match tool.to_lowercase().as_str() {
        "bash" | "shell" => shell_workdir(input, cwd).map(|path| vec![json!({"path": path})]).unwrap_or_default(),
        "read" | "edit" | "write" => location_from(&[nullish(input, &["filePath", "filepath"])]),
        "external_directory" => location_from(&[nullish(input, &["filePath", "filepath"]), input.get("parentDir"), input.get("directories")]),
        "grep" | "glob" | "context" | "context7_resolve_library_id" | "context7_get_library_docs" => location_from(&[input.get("path")]),
        _ => Vec::new(),
    }
}

fn text_content(text: &str) -> Value {
    json!({"type": "content", "content": {"type": "text", "text": text}})
}

pub fn pending(call_id: &str, tool: &str, input: &Input, state_title: Option<&str>, cwd: Option<&str>) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("toolCallId".into(), call_id.into());
    out.insert("title".into(), title(tool, input, state_title).into());
    out.insert("kind".into(), kind(tool).into());
    out.insert("status".into(), "pending".into());
    out.insert("locations".into(), locations(tool, input, cwd).into());
    out.insert("rawInput".into(), raw_input(tool, input, cwd));
    out
}

pub fn running(call_id: &str, tool: &str, input: &Input, state_title: Option<&str>, output: Option<&str>, cwd: Option<&str>) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("toolCallId".into(), call_id.into());
    out.insert("status".into(), "in_progress".into());
    out.insert("kind".into(), kind(tool).into());
    out.insert("title".into(), title(tool, input, state_title).into());
    out.insert("locations".into(), locations(tool, input, cwd).into());
    out.insert("rawInput".into(), raw_input(tool, input, cwd));
    if let Some(output) = output.filter(|o| !o.is_empty()) {
        out.insert("content".into(), json!([text_content(output)]));
    }
    out
}

pub fn completed(call_id: &str, tool: &str, state: &ToolStateCompleted) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("toolCallId".into(), call_id.into());
    out.insert("status".into(), "completed".into());
    if !state.title.is_empty() {
        out.insert("title".into(), state.title.clone().into());
    }
    out.insert("content".into(), completed_content(tool, state).into());
    let mut raw = Map::new();
    raw.insert("output".into(), state.output.clone().into());
    raw.insert("metadata".into(), Value::Object(state.metadata.clone()));
    if let Some(attachments) = state.attachments.as_ref().filter(|a| !a.is_empty()) {
        raw.insert("attachments".into(), serde_json::to_value(attachments).unwrap_or_default());
    }
    out.insert("rawOutput".into(), Value::Object(raw));
    out
}

pub fn failed(call_id: &str, tool: &str, state: &ToolStateError, cwd: Option<&str>) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("toolCallId".into(), call_id.into());
    out.insert("status".into(), "failed".into());
    out.insert("kind".into(), kind(tool).into());
    out.insert("title".into(), title(tool, &state.input, None).into());
    out.insert("locations".into(), locations(tool, &state.input, cwd).into());
    out.insert("rawInput".into(), raw_input(tool, &state.input, cwd));
    out.insert("content".into(), json!([text_content(&state.error)]));
    let mut raw = Map::new();
    raw.insert("error".into(), state.error.clone().into());
    if let Some(metadata) = &state.metadata {
        raw.insert("metadata".into(), Value::Object(metadata.clone()));
    }
    out.insert("rawOutput".into(), Value::Object(raw));
    out
}

fn completed_content(tool: &str, state: &ToolStateCompleted) -> Vec<Value> {
    let metadata = Value::Object(state.metadata.clone());
    let text = if tool.to_lowercase() == "read" { read_display_text(&metadata).unwrap_or_else(|| state.output.clone()) } else { state.output.clone() };
    let mut content = vec![text_content(&text)];
    if kind(tool) == "edit" {
        content.extend(diff_content(&state.input));
    }
    content.extend(images(state.attachments.as_deref().unwrap_or_default()));
    content
}

fn diff_content(input: &Input) -> Option<Value> {
    let old = string(input.get("oldString"))?;
    let new = string(input.get("newString")).or_else(|| string(input.get("content")))?;
    Some(json!({"type": "diff", "path": string(input.get("filePath")).unwrap_or(""), "oldText": old, "newText": new}))
}

fn read_display_text(metadata: &Value) -> Option<String> {
    let display = metadata.get("display").filter(|d| d.is_object())?;
    match display.get("type").and_then(Value::as_str) {
        Some("file") => display.get("text").and_then(Value::as_str).map(str::to_string),
        Some("directory") => display.get("entries").and_then(Value::as_array).map(|entries| entries.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n")),
        _ => None,
    }
}

// `^data:([^;,]+)(?:;[^,]*)*;base64,(.*)$`
fn data_url_image(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("data:")?;
    let (head, data) = rest.split_once(',')?;
    let params = head.strip_suffix(";base64")?;
    let mime = params.split(';').next().filter(|m| !m.is_empty())?;
    if data.contains('\n') || data.contains('\r') || !mime.starts_with("image/") {
        return None;
    }
    Some((mime, data))
}

fn images(attachments: &[FileAttachment]) -> Vec<Value> {
    attachments
        .iter()
        .filter_map(|attachment| data_url_image(&attachment.file.url))
        .map(|(mime, data)| json!({"type": "content", "content": {"type": "image", "mimeType": mime, "data": data}}))
        .collect()
}

pub fn shell_output(metadata: Option<&Map<String, Value>>) -> Option<&str> {
    metadata.and_then(|m| m.get("output")).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(value: Value) -> Input {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn kinds_and_locations_follow_the_tool_name() {
        assert_eq!((kind("Bash"), kind("apply_patch"), kind("glob"), kind("task"), kind("todowrite")), ("execute", "edit", "search", "think", "other"));
        let read = input(json!({"filePath": "/a/b.rs"}));
        assert_eq!(locations("read", &read, None), vec![json!({"path": "/a/b.rs"})]);
        let ext = input(json!({"filepath": "/x", "parentDir": "/x", "directories": ["/y", "", "/x"]}));
        assert_eq!(locations("external_directory", &ext, None), vec![json!({"path": "/x"}), json!({"path": "/y"})]);
        assert!(locations("webfetch", &read, None).is_empty());
    }

    #[test]
    fn shell_calls_show_the_command_and_working_directory() {
        let cwd = if cfg!(windows) { r"C:\work" } else { "/work" };
        let bash = input(json!({"command": "ls -la", "description": "List"}));
        let call = pending("c1", "bash", &bash, None, Some(cwd));
        assert_eq!(call["title"], "ls -la");
        assert_eq!(call["kind"], "execute");
        assert_eq!(call["locations"], json!([{"path": cwd}]));
        assert_eq!(call["rawInput"], json!({"command": "ls -la", "description": "List", "cwd": cwd}));
        let relative = input(json!({"command": "pwd", "workdir": "sub"}));
        let expected = Path::new(cwd).join("sub").to_string_lossy().into_owned();
        assert_eq!(locations("bash", &relative, Some(cwd)), vec![json!({"path": expected})]);
        assert_eq!(raw_input("bash", &relative, Some(cwd)), json!({"command": "pwd", "workdir": "sub"}));
        assert_eq!(title("edit", &bash, Some("")), "edit");
    }

    #[test]
    fn data_urls_only_yield_base64_images() {
        assert_eq!(data_url_image("data:image/png;base64,AAA"), Some(("image/png", "AAA")));
        assert_eq!(data_url_image("data:image/png;x=1;base64,AAA"), Some(("image/png", "AAA")));
        assert_eq!(data_url_image("data:text/plain;base64,AAA"), None);
        assert_eq!(data_url_image("data:image/png,AAA"), None);
    }
}
