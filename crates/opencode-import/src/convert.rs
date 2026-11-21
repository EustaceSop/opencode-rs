// Builds Rust messages and parts from a linear transcript, with ids derived from the source session.
use std::collections::HashMap;

use opencode_storage::{
    AssistantError, AssistantMessage, AssistantTime, CompactionPart, FileAttachment, FileKind, FilePart, JsonMap,
    Message, MessagePath, Part, PartBody, PartTime, ReasoningPart, StepFinishPart, StepStartPart, TextPart, TokenCache,
    Tokens, ToolPart, ToolState, ToolStateCompleted, ToolStateError, ToolTimeCompleted, ToolTimeError, UserMessage,
    UserModel, UserTime,
};
use serde_json::Value;

use crate::ids::{self, Clock};

pub const ABORTED: &str = "Tool execution aborted";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Model {
    pub provider: String,
    pub id: String,
    pub variant: Option<String>,
}

impl Model {
    pub fn new(provider: &str, id: &str) -> Self {
        Model { provider: provider.to_string(), id: id.to_string(), variant: None }
    }

    fn known(&self) -> bool {
        !self.id.is_empty()
    }
}

struct Entry {
    message: Message,
    parts: Vec<Part>,
    ms: u64,
    open: bool,
}

pub struct Transcript {
    source: &'static str,
    key: String,
    pub session_id: String,
    path: MessagePath,
    pub agent: String,
    clock: Clock,
    entries: Vec<Entry>,
    last_user: Option<String>,
    current: Option<usize>,
    tools: HashMap<String, (usize, usize)>,
    pub model: Model,
}

pub fn text(text: &str) -> PartBody {
    PartBody::Text(TextPart { text: text.to_string(), ..Default::default() })
}

pub fn synthetic(text: &str) -> PartBody {
    PartBody::Text(TextPart { text: text.to_string(), synthetic: Some(true), ..Default::default() })
}

pub fn reasoning(text: &str, start: u64, end: Option<u64>) -> PartBody {
    PartBody::Reasoning(ReasoningPart { text: text.to_string(), metadata: None, time: PartTime { start, end } })
}

pub fn tokens(input: f64, output: f64, reasoning: f64, read: f64, write: f64) -> Tokens {
    Tokens { total: None, input, output, reasoning, cache: TokenCache { read, write } }
}

// Tool input as an object: objects pass through, JSON text is parsed, anything else is kept under `input`.
pub fn input_map(value: &Value) -> JsonMap {
    match value {
        Value::Object(map) => map.clone(),
        Value::String(raw) => match serde_json::from_str::<Value>(raw) {
            Ok(Value::Object(map)) => map,
            _ => JsonMap::from_iter([("input".to_string(), value.clone())]),
        },
        Value::Null => JsonMap::new(),
        other => JsonMap::from_iter([("input".to_string(), other.clone())]),
    }
}

impl Transcript {
    pub fn new(source: &'static str, key: &str, session_id: &str, directory: &str, root: &str) -> Self {
        Transcript {
            source,
            key: key.to_string(),
            session_id: session_id.to_string(),
            path: MessagePath { cwd: directory.to_string(), root: root.to_string() },
            agent: "build".to_string(),
            clock: Clock::default(),
            entries: Vec::new(),
            last_user: None,
            current: None,
            tools: HashMap::new(),
            model: Model::default(),
        }
    }

    fn message_id(&mut self, ms: u64) -> String {
        let index = self.entries.len().to_string();
        ids::ascending("msg", self.clock.next(ms), &[self.source, &self.key, "message", &index])
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn user(&mut self, ms: u64, bodies: Vec<PartBody>) {
        self.close();
        if bodies.is_empty() {
            return;
        }
        let id = self.message_id(ms);
        let message = Message::User(UserMessage {
            id: id.clone(),
            session_id: self.session_id.clone(),
            time: UserTime { created: ms },
            format: None,
            summary: None,
            agent: self.agent.clone(),
            model: UserModel { provider_id: String::new(), model_id: String::new(), variant: None },
            system: None,
            tools: None,
        });
        self.entries.push(Entry { message, parts: Vec::new(), ms, open: false });
        self.last_user = Some(id);
        let index = self.entries.len() - 1;
        for body in bodies {
            self.push(index, body);
        }
    }

    // The open assistant message, or a new one; `fresh` always starts a new one.
    pub fn assistant(&mut self, ms: u64, fresh: bool) -> usize {
        if !fresh && let Some(index) = self.current {
            return index;
        }
        self.close();
        let id = self.message_id(ms);
        let message = Message::Assistant(AssistantMessage {
            id,
            session_id: self.session_id.clone(),
            time: AssistantTime { created: ms, completed: Some(ms) },
            error: None,
            parent_id: self.last_user.clone().unwrap_or_default(),
            model_id: self.model.id.clone(),
            provider_id: self.model.provider.clone(),
            mode: self.agent.clone(),
            agent: self.agent.clone(),
            path: self.path.clone(),
            summary: None,
            cost: 0.0,
            tokens: Tokens::default(),
            structured: None,
            variant: self.model.variant.clone(),
            finish: None,
        });
        self.entries.push(Entry { message, parts: Vec::new(), ms, open: true });
        let index = self.entries.len() - 1;
        self.current = Some(index);
        index
    }

    pub fn current(&self) -> Option<usize> {
        self.current
    }

    pub fn close(&mut self) {
        if let Some(index) = self.current.take() {
            self.entries[index].open = false;
        }
    }

    fn assistant_mut(&mut self, index: usize) -> Option<&mut AssistantMessage> {
        match &mut self.entries.get_mut(index)?.message {
            Message::Assistant(message) => Some(message),
            Message::User(_) => None,
        }
    }

    // Moves the completion time of the message forward.
    pub fn touch(&mut self, index: usize, ms: u64) {
        if let Some(message) = self.assistant_mut(index) {
            message.time.completed = Some(message.time.completed.unwrap_or(ms).max(ms));
        }
    }

    pub fn push(&mut self, index: usize, body: PartBody) -> usize {
        let entry = &mut self.entries[index];
        let position = entry.parts.len();
        let seed_index = format!("{index}/{position}");
        // Slot 0 is kept for the step-start part `finish` puts first.
        let id = ids::ascending("prt", ids::part_value(entry.ms, position + 1), &[self.source, &self.key, "part", &seed_index]);
        entry.parts.push(Part { id, session_id: self.session_id.clone(), message_id: entry.message.id().to_string(), body });
        position
    }

    // Files a completed tool returned, as attachments of the tool part at `position`.
    pub fn attach(&mut self, index: usize, position: usize, files: Vec<FilePart>) {
        if files.is_empty() {
            return;
        }
        let (source, key, ms) = (self.source, self.key.clone(), self.entries[index].ms);
        let part = &mut self.entries[index].parts[position];
        let PartBody::Tool(ToolPart { state: ToolState::Completed(done), .. }) = &mut part.body else { return };
        let attachments = files
            .into_iter()
            .enumerate()
            .map(|(n, file)| FileAttachment {
                id: ids::ascending("prt", ids::part_value(ms, position + 1), &[source, &key, "attachment", &format!("{index}/{position}/{n}")]),
                session_id: part.session_id.clone(),
                message_id: part.message_id.clone(),
                kind: FileKind::File,
                file,
            })
            .collect();
        done.attachments = Some(attachments);
    }

    pub fn set_usage(&mut self, index: usize, tokens: Tokens, cost: f64) {
        if let Some(message) = self.assistant_mut(index) {
            message.tokens = tokens;
            message.cost = cost;
        }
    }

    pub fn set_finish(&mut self, index: usize, finish: &str) {
        if let Some(message) = self.assistant_mut(index) {
            message.finish = Some(finish.to_string());
        }
    }

    pub fn set_error(&mut self, index: usize, error: &str) {
        if let Some(message) = self.assistant_mut(index) {
            message.error = Some(AssistantError::UnknownError { message: error.to_string(), r#ref: None });
        }
    }

    pub fn set_model(&mut self, index: usize, model: &Model) {
        if let Some(message) = self.assistant_mut(index) {
            message.model_id = model.id.clone();
            message.provider_id = model.provider.clone();
            message.variant = model.variant.clone();
        }
    }

    // A tool call on the current assistant message; it stays unfinished until its result arrives.
    pub fn tool_call(&mut self, ms: u64, call_id: &str, name: &str, input: JsonMap, metadata: Option<JsonMap>) {
        let index = self.assistant(ms, false);
        let state = ToolState::Error(ToolStateError { input, error: ABORTED.to_string(), metadata: None, time: ToolTimeError { start: ms, end: ms } });
        let position = self.push(index, PartBody::Tool(ToolPart { call_id: call_id.to_string(), tool: name.to_string(), state, metadata }));
        self.tools.insert(call_id.to_string(), (index, position));
    }

    // Completes the call with `call_id`; false when no such call was seen.
    pub fn tool_result(&mut self, call_id: &str, ms: u64, output: &str, failed: bool, metadata: Option<JsonMap>) -> bool {
        let Some(&(index, position)) = self.tools.get(call_id) else { return false };
        let PartBody::Tool(tool) = &mut self.entries[index].parts[position].body else { return false };
        let input = tool.state.input().clone();
        let start = tool.state.start().unwrap_or(ms);
        let end = ms.max(start);
        tool.state = if failed {
            ToolState::Error(ToolStateError { input, error: output.to_string(), metadata, time: ToolTimeError { start, end } })
        } else {
            ToolState::Completed(ToolStateCompleted {
                input,
                output: output.to_string(),
                title: String::new(),
                metadata: metadata.unwrap_or_default(),
                time: ToolTimeCompleted { start, end, compacted: None },
                attachments: None,
            })
        };
        self.touch(index, end);
        true
    }

    // A compaction: the request (user message with a compaction part) and its summary (an assistant summary message).
    pub fn compaction(&mut self, ms: u64, auto: bool, summary: &str) {
        self.user(ms, vec![PartBody::Compaction(CompactionPart { auto, overflow: None, tail_start_id: None })]);
        if summary.trim().is_empty() {
            return;
        }
        let index = self.assistant(ms, true);
        if let Some(message) = self.assistant_mut(index) {
            message.summary = Some(true);
            message.finish = Some("stop".to_string());
        }
        self.push(index, text(summary));
        self.close();
    }

    // Users get the model of the nearest assistant; assistants get step parts carrying their usage.
    pub fn finish(mut self, fallback: &Model) -> Vec<(Message, Vec<Part>)> {
        self.close();
        let models: Vec<Option<Model>> = self
            .entries
            .iter()
            .map(|entry| match &entry.message {
                Message::Assistant(a) if !a.model_id.is_empty() => {
                    Some(Model { provider: a.provider_id.clone(), id: a.model_id.clone(), variant: a.variant.clone() })
                }
                _ => None,
            })
            .collect();
        let fallback = if fallback.known() { fallback.clone() } else { Model::new("unknown", "unknown") };
        let pick = |index: usize| -> Model {
            models[index..].iter().flatten().next().or_else(|| models[..index].iter().rev().flatten().next()).cloned().unwrap_or_else(|| fallback.clone())
        };
        let picks: Vec<Model> = (0..self.entries.len()).map(pick).collect();
        let mut out = Vec::with_capacity(self.entries.len());
        let source = self.source;
        let key = self.key.clone();
        for (index, mut entry) in self.entries.into_iter().enumerate() {
            match &mut entry.message {
                Message::User(user) => {
                    user.model = UserModel { provider_id: picks[index].provider.clone(), model_id: picks[index].id.clone(), variant: picks[index].variant.clone() };
                }
                Message::Assistant(assistant) => {
                    if assistant.model_id.is_empty() {
                        assistant.model_id = picks[index].id.clone();
                        assistant.provider_id = picks[index].provider.clone();
                    }
                    let usage = assistant.tokens != Tokens::default() || assistant.cost != 0.0;
                    let ms = entry.ms;
                    let seed = |name: &str| ids::ascending("prt", ids::part_value(ms, 0), &[source, &key, name, &index.to_string()]);
                    let start = Part { id: seed("step-start"), session_id: assistant.session_id.clone(), message_id: assistant.id.clone(), body: PartBody::StepStart(StepStartPart::default()) };
                    let mut parts = vec![start];
                    parts.append(&mut entry.parts);
                    if usage && assistant.summary != Some(true) {
                        let last = ids::part_value(ms, parts.len());
                        let reason = assistant.finish.clone().unwrap_or_else(|| "stop".to_string());
                        let body = PartBody::StepFinish(StepFinishPart { reason, snapshot: None, cost: assistant.cost, tokens: assistant.tokens });
                        let id = ids::ascending("prt", last, &[source, &key, "step-finish", &index.to_string()]);
                        parts.push(Part { id, session_id: assistant.session_id.clone(), message_id: assistant.id.clone(), body });
                    }
                    entry.parts = parts;
                }
            }
            out.push((entry.message, entry.parts));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_orders_ids_and_resolves_tools() {
        let mut t = Transcript::new("test", "k", "ses_x", "/w", "/w");
        t.model = Model::new("anthropic", "m1");
        t.user(1000, vec![text("hi")]);
        let a = t.assistant(1001, false);
        t.push(a, reasoning("think", 1001, Some(1002)));
        t.tool_call(1002, "c1", "Read", input_map(&Value::String("{\"file_path\":\"a\"}".into())), None);
        t.tool_call(1002, "c2", "Bash", input_map(&serde_json::json!({"command": "ls"})), None);
        t.set_usage(a, tokens(10.0, 5.0, 0.0, 1.0, 2.0), 0.0);
        assert!(t.tool_result("c1", 1500, "content", false, None));
        assert!(!t.tool_result("missing", 1500, "x", false, None));
        t.user(2000, vec![text("next")]);
        let out = t.finish(&Model::default());
        assert_eq!(out.len(), 3);
        assert!(out.windows(2).all(|w| w[0].0.id() < w[1].0.id()));
        let parts = &out[1].1;
        assert_eq!(parts.iter().map(Part::kind).collect::<Vec<_>>(), ["step-start", "reasoning", "tool", "tool", "step-finish"]);
        assert!(parts.windows(2).all(|w| w[0].id < w[1].id));
        let PartBody::Tool(read) = &parts[2].body else { panic!() };
        assert_eq!((read.tool.as_str(), read.state.status(), read.state.input()["file_path"].as_str()), ("Read", "completed", Some("a")));
        let PartBody::Tool(bash) = &parts[3].body else { panic!() };
        assert_eq!(bash.state.status(), "error");
        let user = out[0].0.as_user().unwrap();
        assert_eq!((user.model.provider_id.as_str(), user.model.model_id.as_str()), ("anthropic", "m1"));
        assert_eq!(out[1].0.as_assistant().unwrap().parent_id, user.id);
        assert_eq!(out[1].0.as_assistant().unwrap().time.completed, Some(1500));
    }
}
