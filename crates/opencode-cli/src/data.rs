// `opencode export` / `opencode import` (`opencode/src/cli/cmd/export.ts`, `import.ts`).
use crate::ui;
use anyhow::Result;
use opencode_core::agent;
use opencode_core::config;
use opencode_core::paths::Paths;
use opencode_session::Engine;
use opencode_storage::{Message, MessageWithParts, Session};
use crate::prompt::{self, Choice};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Serialize, Deserialize)]
struct ExportData {
    info: Session,
    messages: Vec<MessageWithParts>,
}

fn redact(kind: &str, id: &str, value: &mut Value) {
    if let Value::String(text) = value
        && !text.trim().is_empty()
    {
        *text = format!("[redacted:{kind}:{id}]");
    }
}

fn redact_key(object: &mut Value, key: &str, kind: &str, id: &str) {
    if let Some(value) = object.get_mut(key) {
        redact(kind, id, value);
    }
}

// A non-empty object becomes `{redacted: "<kind>:<id>"}`.
fn redact_data(object: &mut Value, key: &str, kind: &str, id: &str) {
    if let Some(value) = object.get_mut(key)
        && value.as_object().is_some_and(|map| !map.is_empty())
    {
        *value = json!({"redacted": format!("{kind}:{id}")});
    }
}

fn redact_diffs(owner: &mut Value, kind: &str) {
    for (i, item) in owner.get_mut("diffs").and_then(Value::as_array_mut).into_iter().flatten().enumerate() {
        redact_key(item, "file", &format!("{kind}-file"), &i.to_string());
        redact_key(item, "patch", &format!("{kind}-patch"), &i.to_string());
    }
}

fn redact_file(part: &mut Value, id: &str) {
    redact_key(part, "url", "file-url", id);
    redact_key(part, "filename", "file-name", id);
    let Some(source) = part.get_mut("source").filter(|source| source.is_object()) else { return };
    match source.get("type").and_then(Value::as_str) {
        Some("symbol") => {
            redact_key(source, "path", "file-path", id);
            redact_key(source, "name", "file-symbol", id);
        }
        Some("resource") => {
            redact_key(source, "clientName", "file-client", id);
            redact_key(source, "uri", "file-uri", id);
        }
        _ => redact_key(source, "path", "file-path", id),
    }
    if let Some(text) = source.get_mut("text") {
        redact_key(text, "value", "file-text", id);
    }
}

fn redact_part(part: &mut Value) {
    let id = part.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
    let id = id.as_str();
    match part.get("type").and_then(Value::as_str).unwrap_or_default() {
        "text" => {
            redact_key(part, "text", "text", id);
            redact_data(part, "metadata", "text-metadata", id);
        }
        "reasoning" => {
            redact_key(part, "text", "reasoning", id);
            redact_data(part, "metadata", "reasoning-metadata", id);
        }
        "file" => redact_file(part, id),
        "subtask" => {
            redact_key(part, "prompt", "subtask-prompt", id);
            redact_key(part, "description", "subtask-description", id);
            redact_key(part, "command", "subtask-command", id);
        }
        "tool" => {
            redact_data(part, "metadata", "tool-metadata", id);
            let Some(state) = part.get_mut("state") else { return };
            redact_data(state, "input", "tool-input", id);
            redact_data(state, "metadata", "tool-state-metadata", id);
            redact_key(state, "raw", "tool-raw", id);
            redact_key(state, "title", "tool-title", id);
            if state.get("status").and_then(Value::as_str) == Some("completed") {
                redact_key(state, "output", "tool-output", id);
                for attachment in state.get_mut("attachments").and_then(Value::as_array_mut).into_iter().flatten() {
                    let own = attachment.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                    redact_file(attachment, &own);
                }
            }
        }
        "patch" => {
            redact_key(part, "hash", "patch", id);
            for (i, file) in part.get_mut("files").and_then(Value::as_array_mut).into_iter().flatten().enumerate() {
                redact("patch-file", &format!("{id}-{i}"), file);
            }
        }
        "snapshot" | "step-start" | "step-finish" => redact_key(part, "snapshot", "snapshot", id),
        "agent" => {
            if let Some(source) = part.get_mut("source") {
                redact_key(source, "value", "agent-source", id);
            }
        }
        _ => {}
    }
}

// `--sanitize`: transcript text, paths and file data are replaced by `[redacted:<kind>:<id>]` markers.
pub fn sanitize(data: &mut Value) {
    let session = data["info"]["id"].as_str().unwrap_or_default().to_string();
    let info = &mut data["info"];
    redact_key(info, "title", "session-title", &session);
    redact_key(info, "directory", "session-directory", &session);
    if let Some(summary) = info.get_mut("summary") {
        redact_diffs(summary, "session-diff");
    }
    if let Some(revert) = info.get_mut("revert") {
        redact_key(revert, "snapshot", "revert-snapshot", &session);
        redact_key(revert, "diff", "revert-diff", &session);
    }
    for message in data["messages"].as_array_mut().into_iter().flatten() {
        let id = message["info"]["id"].as_str().unwrap_or_default().to_string();
        let info = &mut message["info"];
        if info["role"] == "user" {
            redact_key(info, "system", "system", &id);
            if let Some(summary) = info.get_mut("summary").filter(|summary| summary.is_object()) {
                redact_key(summary, "title", "summary-title", &id);
                redact_key(summary, "body", "summary-body", &id);
                redact_diffs(summary, "message-diff");
            }
        } else if let Some(path) = info.get_mut("path") {
            redact_key(path, "cwd", "cwd", &id);
            redact_key(path, "root", "root", &id);
        }
        for part in message["parts"].as_array_mut().into_iter().flatten() {
            redact_part(part);
        }
    }
}

pub fn export(paths: &Paths, session_id: Option<String>, sanitized: bool) -> Result<i32> {
    eprintln!("Exporting session: {}", session_id.as_deref().unwrap_or("latest"));
    let engine = Engine::new(paths.clone())?;
    let store = &engine.store;
    let session_id = match session_id {
        Some(id) => id,
        None => {
            ui::empty();
            ui::intro("Export session");
            let project = opencode_storage::identity::resolve(&std::env::current_dir()?).id;
            let mut sessions = store.list_sessions(&project)?;
            if sessions.is_empty() {
                ui::failure("No sessions found");
                ui::outro("Done");
                return Ok(0);
            }
            sessions.sort_by_key(|session| std::cmp::Reverse(session.time.updated));
            // Without a terminal there is no picker: the most recently updated session is exported.
            if !prompt::interactive() {
                return write(store, &sessions[0].id.clone(), sanitized);
            }
            let choices = sessions
                .iter()
                .map(|session| {
                    let tail: String = session.id.chars().skip(session.id.chars().count().saturating_sub(8)).collect();
                    Choice::hint(&session.title, format!("{} • {tail}", crate::session::datetime(session.time.updated)))
                })
                .collect();
            let picked = prompt::select("Select session to export", choices, 0, 10)?;
            ui::outro("Exporting session...");
            sessions.swap_remove(picked).id
        }
    };
    write(store, &session_id, sanitized)
}

fn write(store: &opencode_storage::Store, session_id: &str, sanitized: bool) -> Result<i32> {
    let Some(info) = store.get_session(session_id)? else {
        return Err(ui::fail(format!("Session not found: {session_id}")));
    };
    let messages = store.list_messages(session_id)?.into_iter().map(|(info, parts)| MessageWithParts { info, parts }).collect();
    let mut data = serde_json::to_value(ExportData { info, messages })?;
    if sanitized {
        sanitize(&mut data);
    }
    println!("{}", serde_json::to_string_pretty(&data)?);
    Ok(0)
}

pub fn import(paths: &Paths, file: &str) -> Result<i32> {
    if file.starts_with("http://") || file.starts_with("https://") {
        ui::error("Importing from share URLs is no longer supported: session sharing has been discontinued");
        return Ok(1);
    }
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) => {
            ui::error(&match error.kind() {
                std::io::ErrorKind::NotFound => format!("File not found: {file}"),
                std::io::ErrorKind::PermissionDenied => "Failed to read file: Permission denied".to_string(),
                _ => format!("Failed to read file: {error}"),
            });
            return Ok(1);
        }
    };
    let data: ExportData = match serde_json::from_str(&text) {
        Ok(data) => data,
        Err(error) => {
            ui::error(&format!("Invalid JSON in {file}: {error}"));
            return Ok(1);
        }
    };

    // The session moves to the current project and directory; rows that already exist are left alone.
    let directory = std::env::current_dir()?;
    let engine = Engine::new(paths.clone())?;
    let store = &engine.store;
    let project = engine.project(&directory)?;
    let root = config::find_worktree(&directory).unwrap_or_else(|| PathBuf::from("/"));
    let path = agent::relative(&root, &directory).replace('\\', "/");
    let place = |session: &mut Session| {
        session.project_id = project.id.clone();
        session.directory = directory.to_string_lossy().into_owned();
        session.path = Some(path.clone());
    };
    let session_id = data.info.id.clone();
    let row = if store.get_session(&session_id)?.is_some() {
        store.patch_session(&session_id, place)?
    } else {
        let mut info = data.info;
        place(&mut info);
        store.create_session(&info)?;
        info
    };
    for MessageWithParts { info, parts } in data.messages {
        if store.get_message(info.id())?.is_none() {
            let message = match info {
                Message::User(mut user) => {
                    user.session_id = session_id.clone();
                    Message::User(user)
                }
                Message::Assistant(mut assistant) => {
                    assistant.session_id = session_id.clone();
                    Message::Assistant(assistant)
                }
            };
            store.upsert_message(&message)?;
        }
        for mut part in parts {
            if store.get_part(&part.id)?.is_none() {
                part.session_id = session_id.clone();
                store.upsert_part(&part)?;
            }
        }
    }
    // Stored step parts add their usage to the session; the row keeps the totals it came with.
    store.update_session(&row)?;
    println!("Imported session: {session_id}");
    Ok(0)
}
