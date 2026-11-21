// The tool call a `session/request_permission` shows, and the edit it proposes (`acp/permission.ts`).
use crate::patch;
use crate::tool::{self, Input, string};
use serde_json::{Value, json};
use std::path::Path;

pub fn options() -> Value {
    json!([
        {"optionId": "once", "kind": "allow_once", "name": "Allow once"},
        {"optionId": "always", "kind": "allow_always", "name": "Always allow"},
        {"optionId": "reject", "kind": "reject_once", "name": "Reject"},
    ])
}

struct FileMeta<'a> {
    file_path: &'a str,
    relative_path: Option<&'a str>,
    move_path: Option<&'a str>,
    patch: Option<&'a str>,
}

fn file_metadata(input: &Input) -> Vec<FileMeta<'_>> {
    let Some(files) = input.get("files").and_then(Value::as_array) else { return Vec::new() };
    files
        .iter()
        .filter_map(Value::as_object)
        .filter_map(|file| {
            let file_path = string(file.get("filePath")).filter(|p| !p.is_empty())?;
            Some(FileMeta { file_path, relative_path: string(file.get("relativePath")), move_path: string(file.get("movePath")), patch: string(file.get("patch")) })
        })
        .collect()
}

fn edit_title(input: &Input) -> Option<String> {
    let files = file_metadata(input);
    match files.len() {
        1 => Some(files[0].relative_path.unwrap_or(files[0].file_path).to_string()),
        0 => string(input.get("filePath")).or_else(|| string(input.get("filepath"))).or_else(|| string(input.get("path"))).map(str::to_string),
        n => Some(format!("{n} files")),
    }
}

fn title(permission: &str, input: &Input) -> Option<String> {
    let first = |keys: &[&str]| keys.iter().find_map(|k| string(input.get(*k))).map(str::to_string);
    match permission.to_lowercase().as_str() {
        "external_directory" => first(&["description", "command", "parentDir"]),
        "webfetch" => first(&["url"]),
        "websearch" => first(&["query"]),
        "grep" | "glob" => first(&["pattern"]),
        "read" | "edit" | "write" => edit_title(input),
        _ => None,
    }
}

fn locations(permission: &str, input: &Input) -> Vec<Value> {
    let files = file_metadata(input);
    if files.is_empty() {
        return tool::locations(permission, input, None);
    }
    let mut paths: Vec<&str> = Vec::new();
    for path in files.iter().flat_map(|f| [Some(f.file_path), f.move_path]).flatten().filter(|p| !p.is_empty()) {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths.into_iter().map(|path| json!({"path": path})).collect()
}

fn current_text(path: &str) -> Option<String> {
    if !Path::new(path).exists() {
        return Some(String::new());
    }
    std::fs::read_to_string(path).ok()
}

// The file as it would be after the patch; None when the patch does not apply.
pub fn proposed(path: &str, diff: &str) -> Option<(String, String)> {
    let old = current_text(path)?;
    let new = patch::apply(&old, diff)?;
    Some((old, new))
}

fn diff_for_patch(path: &str, diff: &str, shown: &str) -> Option<Value> {
    let (old, new) = proposed(path, diff)?;
    Some(json!({"type": "diff", "path": shown, "oldText": old, "newText": new}))
}

fn content(permission: &str, input: &Input) -> Vec<Value> {
    if permission.to_lowercase() != "edit" {
        return Vec::new();
    }
    let files = file_metadata(input);
    if !files.is_empty() {
        return files.iter().filter_map(|f| diff_for_patch(f.file_path, f.patch.filter(|p| !p.is_empty())?, f.move_path.unwrap_or(f.file_path))).collect();
    }
    let path = string(input.get("filepath")).or_else(|| string(input.get("filePath"))).filter(|p| !p.is_empty());
    let diff = string(input.get("diff")).filter(|d| !d.is_empty());
    match (path, diff) {
        (Some(path), Some(diff)) => diff_for_patch(path, diff, path).into_iter().collect(),
        _ => Vec::new(),
    }
}

pub fn tool_call(call_id: &str, permission: &str, input: &Input) -> Value {
    let mut call = tool::pending(call_id, permission, input, title(permission, input).as_deref(), None);
    call.insert("locations".into(), locations(permission, input).into());
    let content = content(permission, input);
    if !content.is_empty() {
        call.insert("content".into(), content.into());
    }
    Value::Object(call)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_permissions_show_the_proposed_diff() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one\ntwo\n").unwrap();
        let path = file.to_string_lossy().into_owned();
        let diff = format!("Index: {path}\n===================================================================\n--- {path}\n+++ {path}\n@@ -1,2 +1,2 @@\n one\n-two\n+TWO\n");
        let input = json!({"filepath": path, "diff": diff}).as_object().cloned().unwrap();
        let call = tool_call("call_1", "edit", &input);
        assert_eq!(call["title"], path);
        assert_eq!(call["kind"], "edit");
        assert_eq!(call["status"], "pending");
        assert_eq!(call["locations"], json!([{"path": path}]));
        assert_eq!(call["content"], json!([{"type": "diff", "path": path, "oldText": "one\ntwo\n", "newText": "one\nTWO\n"}]));
        let keys: Vec<&str> = call.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["toolCallId", "title", "kind", "status", "locations", "rawInput", "content"]);

        let bash = json!({"command": "rm -rf x"}).as_object().cloned().unwrap();
        let call = tool_call("per_1", "bash", &bash);
        assert_eq!((call["title"].as_str(), call["kind"].as_str(), call.get("content")), (Some("rm -rf x"), Some("execute"), None));
    }
}
