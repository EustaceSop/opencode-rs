// Typed view of the bus events a client acts on (`schema/src/event-manifest.ts`); anything else, or anything that
// does not decode, stays raw in `Event::Unknown`.
use crate::types::{PermissionRequest, QuestionRequest, SessionStatus};
use opencode_storage::{Message, Part, Session, Todo};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    ServerConnected,
    ServerHeartbeat,
    InstanceDisposed { directory: String },
    GlobalDisposed,
    SessionCreated(Box<Session>),
    SessionUpdated(Box<Session>),
    SessionDeleted(Box<Session>),
    SessionStatus { session_id: String, status: SessionStatus },
    SessionIdle { session_id: String },
    SessionCompacted { session_id: String },
    // The original's error object (`{name, data}`), kept raw: servers may send names this build does not know.
    SessionError { session_id: Option<String>, error: Option<Value> },
    SessionDiff { session_id: String, diff: Value },
    MessageUpdated(Box<Message>),
    MessageRemoved { session_id: String, message_id: String },
    PartUpdated(Box<Part>),
    PartRemoved { session_id: String, message_id: String, part_id: String },
    PartDelta { session_id: String, message_id: String, part_id: String, field: String, delta: String },
    PermissionAsked(PermissionRequest),
    PermissionReplied { session_id: String, request_id: String, reply: String },
    QuestionAsked(QuestionRequest),
    QuestionReplied { session_id: String, request_id: String, answers: Vec<Vec<String>> },
    QuestionRejected { session_id: String, request_id: String },
    TodoUpdated { session_id: String, todos: Vec<Todo> },
    CommandExecuted { name: String, session_id: String, arguments: String, message_id: String },
    TuiPromptAppend { text: String },
    TuiCommandExecute { command: String },
    TuiToastShow { title: Option<String>, message: String, variant: String, duration: Option<f64> },
    TuiSessionSelect { session_id: String },
    Unknown { kind: String, properties: Value },
}

// One frame of `/event` (`{id, type, properties}`) or `/global/event` (`{directory?, project?, workspace?, payload}`).
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub id: Option<String>,
    pub kind: String,
    pub properties: Value,
    pub directory: Option<String>,
    pub project: Option<String>,
    pub workspace: Option<String>,
    pub event: Event,
}

impl Frame {
    pub fn parse(data: &str, global: bool) -> Option<Frame> {
        let value: Value = serde_json::from_str(data).ok()?;
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
        let (directory, project, workspace) = if global { (text("directory"), text("project"), text("workspace")) } else { (None, None, None) };
        let payload = if global { value.get("payload")? } else { &value };
        let kind = payload.get("type")?.as_str()?.to_string();
        // A durable sync frame (`{type: "sync", syncEvent}`) carries its event under `syncEvent`.
        let properties = match kind.as_str() {
            "sync" => payload.get("syncEvent").cloned().unwrap_or(Value::Null),
            _ => payload.get("properties").cloned().unwrap_or(Value::Null),
        };
        let id = payload.get("id").and_then(Value::as_str).map(str::to_string);
        let event = Event::decode(&kind, &properties);
        Some(Frame { id, kind, properties, directory, project, workspace, event })
    }

    // The session an event concerns, when it names one.
    pub fn session_id(&self) -> Option<&str> {
        match &self.event {
            Event::SessionCreated(info) | Event::SessionUpdated(info) | Event::SessionDeleted(info) => Some(&info.id),
            Event::MessageUpdated(message) => Some(message.session_id()),
            Event::PartUpdated(part) => Some(&part.session_id),
            Event::PermissionAsked(request) => Some(&request.session_id),
            Event::QuestionAsked(request) => Some(&request.session_id),
            Event::SessionError { session_id, .. } => session_id.as_deref(),
            _ => self.properties.get("sessionID").and_then(Value::as_str),
        }
    }
}

fn field<T: DeserializeOwned>(properties: &Value, key: &str) -> Option<T> {
    serde_json::from_value(properties.get(key)?.clone()).ok()
}

fn text(properties: &Value, key: &str) -> Option<String> {
    properties.get(key).and_then(Value::as_str).map(str::to_string)
}

#[derive(Deserialize)]
struct Ids {
    #[serde(rename = "sessionID")]
    session_id: String,
    #[serde(rename = "messageID", default)]
    message_id: String,
    #[serde(rename = "partID", default)]
    part_id: String,
    #[serde(rename = "requestID", default)]
    request_id: String,
}

impl Event {
    pub fn decode(kind: &str, properties: &Value) -> Event {
        Self::typed(kind, properties).unwrap_or_else(|| Event::Unknown { kind: kind.to_string(), properties: properties.clone() })
    }

    fn typed(kind: &str, p: &Value) -> Option<Event> {
        let ids = || serde_json::from_value::<Ids>(p.clone()).ok();
        Some(match kind {
            "server.connected" => Event::ServerConnected,
            "server.heartbeat" => Event::ServerHeartbeat,
            "server.instance.disposed" => Event::InstanceDisposed { directory: text(p, "directory").unwrap_or_default() },
            "global.disposed" => Event::GlobalDisposed,
            "session.created" => Event::SessionCreated(Box::new(field(p, "info")?)),
            "session.updated" => Event::SessionUpdated(Box::new(field(p, "info")?)),
            "session.deleted" => Event::SessionDeleted(Box::new(field(p, "info")?)),
            "session.status" => Event::SessionStatus { session_id: text(p, "sessionID")?, status: field(p, "status")? },
            "session.idle" => Event::SessionIdle { session_id: text(p, "sessionID")? },
            "session.compacted" => Event::SessionCompacted { session_id: text(p, "sessionID")? },
            "session.error" => Event::SessionError { session_id: text(p, "sessionID"), error: p.get("error").cloned().filter(|e| !e.is_null()) },
            "session.diff" => Event::SessionDiff { session_id: text(p, "sessionID")?, diff: p.get("diff").cloned().unwrap_or(Value::Null) },
            "message.updated" => Event::MessageUpdated(Box::new(field(p, "info")?)),
            "message.removed" => {
                let ids = ids()?;
                Event::MessageRemoved { session_id: ids.session_id, message_id: ids.message_id }
            }
            "message.part.updated" => Event::PartUpdated(Box::new(field(p, "part")?)),
            "message.part.removed" => {
                let ids = ids()?;
                Event::PartRemoved { session_id: ids.session_id, message_id: ids.message_id, part_id: ids.part_id }
            }
            "message.part.delta" => {
                let ids = ids()?;
                Event::PartDelta { session_id: ids.session_id, message_id: ids.message_id, part_id: ids.part_id, field: text(p, "field")?, delta: text(p, "delta")? }
            }
            "permission.asked" => Event::PermissionAsked(serde_json::from_value(p.clone()).ok()?),
            "permission.replied" => {
                let ids = ids()?;
                Event::PermissionReplied { session_id: ids.session_id, request_id: ids.request_id, reply: text(p, "reply")? }
            }
            "question.asked" => Event::QuestionAsked(serde_json::from_value(p.clone()).ok()?),
            "question.replied" => {
                let ids = ids()?;
                Event::QuestionReplied { session_id: ids.session_id, request_id: ids.request_id, answers: field(p, "answers")? }
            }
            "question.rejected" => {
                let ids = ids()?;
                Event::QuestionRejected { session_id: ids.session_id, request_id: ids.request_id }
            }
            "todo.updated" => Event::TodoUpdated { session_id: text(p, "sessionID")?, todos: field(p, "todos")? },
            "command.executed" => Event::CommandExecuted {
                name: text(p, "name")?,
                session_id: text(p, "sessionID")?,
                arguments: text(p, "arguments").unwrap_or_default(),
                message_id: text(p, "messageID").unwrap_or_default(),
            },
            "tui.prompt.append" => Event::TuiPromptAppend { text: text(p, "text")? },
            "tui.command.execute" => Event::TuiCommandExecute { command: text(p, "command")? },
            "tui.toast.show" => Event::TuiToastShow {
                title: text(p, "title"),
                message: text(p, "message")?,
                variant: text(p, "variant").unwrap_or_else(|| "info".to_string()),
                duration: p.get("duration").and_then(Value::as_f64),
            },
            "tui.session.select" => Event::TuiSessionSelect { session_id: text(p, "sessionID")? },
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frames_decode_and_keep_unknown_events_raw() {
        let delta = json!({"id": "evt_1", "type": "message.part.delta", "properties": {"sessionID": "ses_1", "messageID": "msg_1", "partID": "prt_1", "field": "text", "delta": "Hel"}});
        let frame = Frame::parse(&delta.to_string(), false).unwrap();
        assert_eq!(frame.id.as_deref(), Some("evt_1"));
        assert_eq!(frame.session_id(), Some("ses_1"));
        assert_eq!(frame.event, Event::PartDelta { session_id: "ses_1".into(), message_id: "msg_1".into(), part_id: "prt_1".into(), field: "text".into(), delta: "Hel".into() });

        let global = json!({"directory": "/w", "project": "p1", "payload": {"id": "evt_2", "type": "lsp.updated", "properties": {}}});
        let frame = Frame::parse(&global.to_string(), true).unwrap();
        assert_eq!((frame.directory.as_deref(), frame.project.as_deref()), (Some("/w"), Some("p1")));
        assert_eq!(frame.event, Event::Unknown { kind: "lsp.updated".into(), properties: json!({}) });

        let status = json!({"type": "session.status", "properties": {"sessionID": "ses_1", "status": {"type": "retry", "attempt": 2, "message": "busy", "next": 9}}});
        let frame = Frame::parse(&status.to_string(), false).unwrap();
        assert_eq!(frame.event, Event::SessionStatus { session_id: "ses_1".into(), status: SessionStatus::Retry { attempt: 2, message: "busy".into(), next: 9 } });

        // A malformed known event is preserved rather than dropped.
        let broken = json!({"type": "session.updated", "properties": {"sessionID": "ses_1", "info": {"id": 5}}});
        assert!(matches!(Frame::parse(&broken.to_string(), false).unwrap().event, Event::Unknown { kind, .. } if kind == "session.updated"));
        let sync = json!({"directory": "/w", "payload": {"type": "sync", "id": "evt_3", "syncEvent": {"type": "session.created.1", "seq": 1}}});
        let frame = Frame::parse(&sync.to_string(), true).unwrap();
        assert_eq!((frame.kind.as_str(), frame.properties["seq"].as_i64()), ("sync", Some(1)));
        assert!(Frame::parse("not json", false).is_none());
    }
}
