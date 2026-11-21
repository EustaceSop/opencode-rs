// Stored history -> model messages (`session/message-v2.ts`).
use opencode_llm::provider::Model;
use opencode_llm::{self as llm, Reasoning, ToolCall};
use opencode_storage::{
    AssistantError, AssistantMessage, CompactionPart, Message, Part, PartBody, SubtaskPart, ToolState, UserMessage,
};
use serde_json::Value;

pub type Row = (Message, Vec<Part>);

pub const SYNTHETIC_ATTACHMENT_PROMPT: &str = "Attached media from tool result:";
const INTERRUPTED: &str = "[Tool execution was interrupted]";
const CLEARED: &str = "[Old tool result content cleared]";

pub fn is_media(mime: &str) -> bool {
    mime.starts_with("image/") || mime == "application/pdf"
}

fn compaction(parts: &[Part]) -> Option<&CompactionPart> {
    parts.iter().find_map(|p| match &p.body {
        PartBody::Compaction(c) => Some(c),
        _ => None,
    })
}

pub fn has_compaction(parts: &[Part]) -> bool {
    compaction(parts).is_some()
}

// History from the latest completed compaction on, reordered to
// [compaction user, summary, retained tail..., everything after the summary].
pub fn filter_compacted(all: Vec<Row>) -> Vec<Row> {
    let mut result: Vec<Row> = Vec::new();
    let mut completed: Vec<String> = Vec::new();
    let mut retain: Option<String> = None;
    for row in all.into_iter().rev() {
        let id = row.0.id().to_string();
        let summary_parent = match &row.0 {
            Message::Assistant(a) if a.summary == Some(true) && a.finish.is_some() && a.error.is_none() => {
                Some(a.parent_id.clone())
            }
            _ => None,
        };
        let boundary = matches!(&row.0, Message::User(_)) && completed.contains(&id);
        let tail = if boundary { compaction(&row.1).map(|c| c.tail_start_id.clone()) } else { None };
        result.push(row);
        if let Some(target) = &retain {
            if &id == target {
                break;
            }
            continue;
        }
        match tail {
            Some(None) => break,
            Some(Some(start)) => {
                if id == start {
                    break;
                }
                retain = Some(start);
                continue;
            }
            None => {}
        }
        if let Some(parent) = summary_parent {
            completed.push(parent);
        }
    }
    result.reverse();

    let Some(index) = result.iter().rposition(|(m, parts)| {
        matches!(m, Message::User(_)) && compaction(parts).is_some_and(|c| c.tail_start_id.is_some())
    }) else {
        return result;
    };
    let compaction_id = result[index].0.id().to_string();
    let tail_id = compaction(&result[index].1).and_then(|c| c.tail_start_id.clone()).unwrap_or_default();
    let summary = result.iter().enumerate().position(|(i, (m, _))| {
        i > index && matches!(m, Message::Assistant(a) if a.summary == Some(true) && a.parent_id == compaction_id)
    });
    let tail = result.iter().position(|(m, _)| m.id() == tail_id);
    match (tail, summary) {
        (Some(tail), Some(summary)) if tail < index => {
            let after = result.split_off(summary + 1);
            let head = result.split_off(index);
            let retained = result.split_off(tail);
            head.into_iter().chain(retained).chain(after).collect()
        }
        _ => result,
    }
}

#[derive(Debug, Clone)]
pub enum Task {
    Compaction(CompactionPart),
    Subtask(SubtaskPart),
}

#[derive(Debug, Default)]
pub struct Latest {
    pub user: Option<UserMessage>,
    pub assistant: Option<AssistantMessage>,
    pub finished: Option<AssistantMessage>,
    pub tasks: Vec<Task>,
}

fn after(created: u64, id: &str, other: Option<(u64, &str)>) -> bool {
    match other {
        None => true,
        Some((time, _)) if created != time => created > time,
        Some((_, other)) => id > other,
    }
}

pub fn latest(msgs: &[Row]) -> Latest {
    let mut out = Latest::default();
    for (message, _) in msgs {
        match message {
            Message::User(u) => {
                if after(u.time.created, &u.id, out.user.as_ref().map(|o| (o.time.created, o.id.as_str()))) {
                    out.user = Some(u.clone());
                }
            }
            Message::Assistant(a) => {
                if after(a.time.created, &a.id, out.assistant.as_ref().map(|o| (o.time.created, o.id.as_str()))) {
                    out.assistant = Some(a.clone());
                }
                if a.finish.is_some()
                    && after(a.time.created, &a.id, out.finished.as_ref().map(|o| (o.time.created, o.id.as_str())))
                {
                    out.finished = Some(a.clone());
                }
            }
        }
    }
    let finished = out.finished.as_ref().map(|f| (f.time.created, f.id.as_str()));
    for (message, parts) in msgs {
        if finished.is_some() && !after(message.time_created(), message.id(), finished) {
            continue;
        }
        for part in parts {
            match &part.body {
                PartBody::Compaction(c) => out.tasks.push(Task::Compaction(c.clone())),
                PartBody::Subtask(s) => out.tasks.push(Task::Subtask(s.clone())),
                _ => {}
            }
        }
    }
    out
}

// `data:<mime>;base64,<payload>` -> payload
fn data_payload(url: &str) -> Option<&str> {
    url.strip_prefix("data:")?.split_once(',').map(|(_, data)| data)
}

fn media_in_tool_result(model: &Model, mime: &str) -> bool {
    let id = model.api_id.to_lowercase();
    match model.npm.as_str() {
        "@ai-sdk/anthropic" | "@ai-sdk/openai" | "@ai-sdk/amazon-bedrock/mantle" | "@ai-sdk/google-vertex/anthropic" => true,
        "@ai-sdk/amazon-bedrock" => {
            mime.starts_with("image/") && ["anthropic.", "nova", "llama4", "llama-4"].iter().any(|s| id.contains(s))
        }
        "@ai-sdk/xai" => mime.starts_with("image/"),
        "@ai-sdk/google" => id.contains("gemini-3") && !id.contains("gemini-2"),
        _ => false,
    }
}

fn rejected_by_provider(model: &Model, mime: &str) -> bool {
    model.npm == "@ai-sdk/xai" && mime.starts_with("image/") && !["image/png", "image/jpeg", "image/webp"].contains(&mime)
}

fn user_parts(parts: &[Part]) -> Vec<llm::Part> {
    let mut out = Vec::new();
    for part in parts {
        match &part.body {
            PartBody::Text(t) if t.ignored != Some(true) && !t.text.is_empty() => out.push(llm::Part::Text(t.text.clone())),
            PartBody::File(f) if f.mime != "text/plain" && f.mime != "application/x-directory" => {
                if let Some(data) = data_payload(&f.url) {
                    out.push(llm::Part::Media { mime: f.mime.clone(), data: data.to_string(), filename: f.filename.clone() });
                }
            }
            PartBody::Compaction(_) => out.push(llm::Part::Text("What did we do so far?".into())),
            PartBody::Subtask(_) => out.push(llm::Part::Text("The following tool was executed by the user".into())),
            _ => {}
        }
    }
    out
}

// Text-only messages travel as one string, parts joined by newlines.
fn push_user(out: &mut Vec<llm::Message>, parts: Vec<llm::Part>) {
    if parts.is_empty() {
        return;
    }
    if parts.iter().any(|p| matches!(p, llm::Part::Media { .. })) {
        out.push(llm::Message::UserParts(parts));
        return;
    }
    let text: Vec<String> = parts
        .into_iter()
        .filter_map(|p| match p {
            llm::Part::Text(text) => Some(text),
            llm::Part::Media { .. } => None,
        })
        .collect();
    out.push(llm::Message::User(text.join("
")));
}

pub fn to_model_messages(rows: &[Row], model: &Model) -> Vec<llm::Message> {
    let mut out = Vec::new();
    for (message, parts) in rows {
        if parts.is_empty() {
            continue;
        }
        match message {
            Message::User(_) => push_user(&mut out, user_parts(parts)),
            Message::Assistant(info) => assistant(&mut out, info, parts, model),
        }
    }
    out
}

fn assistant(out: &mut Vec<llm::Message>, info: &AssistantMessage, parts: &[Part], model: &Model) {
    if let Some(error) = &info.error {
        let aborted = matches!(error, AssistantError::MessageAbortedError { .. });
        let content = parts.iter().any(|p| !matches!(p.body, PartBody::StepStart(_) | PartBody::Reasoning(_)));
        if !(aborted && content) {
            return;
        }
    }
    let different = model.provider_id != info.provider_id || model.id != info.model_id;
    let signed = parts.iter().any(|p| matches!(&p.body, PartBody::Reasoning(r) if r.metadata.as_ref().is_some_and(|m| m.contains_key("signature"))));
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut reasoning_parts = Vec::new();
    let mut calls = Vec::new();
    let mut results = Vec::new();
    let mut media = Vec::new();
    for part in parts {
        match &part.body {
            PartBody::Text(t) => text.push_str(if t.text.is_empty() && signed { " " } else { &t.text }),
            PartBody::Reasoning(r) if different => {
                if !r.text.trim().is_empty() {
                    text.push_str(&r.text);
                }
            }
            PartBody::Reasoning(r) => {
                reasoning.push_str(&r.text);
                let meta = |key: &str| r.metadata.as_ref()?.get(key)?.as_str().map(str::to_string);
                reasoning_parts.push(Reasoning {
                    text: r.text.clone(),
                    signature: meta("signature"),
                    item_id: meta("itemId"),
                    encrypted: meta("encrypted"),
                });
            }
            PartBody::Tool(t) => {
                calls.push(ToolCall {
                    id: t.call_id.clone(),
                    name: t.tool.clone(),
                    arguments: Value::Object(t.state.input().clone()).to_string(),
                });
                let mut attachments = Vec::new();
                let (content, is_error) = match &t.state {
                    ToolState::Completed(s) if s.time.compacted.is_some() => (CLEARED.to_string(), false),
                    ToolState::Completed(s) => {
                        for file in s.attachments.iter().flatten().map(|a| &a.file) {
                            let Some(data) = data_payload(&file.url) else { continue };
                            if rejected_by_provider(model, &file.mime) {
                                continue;
                            }
                            let item = llm::Part::Media { mime: file.mime.clone(), data: data.to_string(), filename: file.filename.clone() };
                            if is_media(&file.mime) && !media_in_tool_result(model, &file.mime) {
                                media.push(item);
                            } else {
                                attachments.push(item);
                            }
                        }
                        (s.output.clone(), false)
                    }
                    ToolState::Error(s) => {
                        let interrupted = s.metadata.as_ref().filter(|m| m.get("interrupted") == Some(&Value::Bool(true)));
                        match interrupted.and_then(|m| m.get("output")).and_then(Value::as_str) {
                            Some(output) => (output.to_string(), false),
                            None => (s.error.clone(), true),
                        }
                    }
                    _ => (INTERRUPTED.to_string(), true),
                };
                results.push(llm::Message::Tool { call_id: t.call_id.clone(), content, is_error, attachments });
            }
            _ => {}
        }
    }
    if text.is_empty() && calls.is_empty() && reasoning_parts.is_empty() {
        return;
    }
    out.push(llm::Message::Assistant { text, reasoning, tool_calls: calls, reasoning_parts });
    out.extend(results);
    if !media.is_empty() {
        let mut parts = vec![llm::Part::Text(SYNTHETIC_ATTACHMENT_PROMPT.to_string())];
        parts.extend(media);
        out.push(llm::Message::UserParts(parts));
    }
}
