// In-process event bus. Every subscriber gets its own unbounded queue, like the original's per-stream queues.
use opencode_storage::id::{self, Prefix};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct Event {
    pub id: String,
    pub kind: String,
    pub properties: Value,
    // None for events that only reach `/global/event`.
    pub directory: Option<String>,
    pub project: Option<String>,
    // Emitted straight on the global bus in the original; `/event` only forwards `server.instance.disposed` of these.
    pub global_only: bool,
}

impl Event {
    // `/event` frame.
    pub fn payload(&self) -> Value {
        json!({"id": self.id, "type": self.kind, "properties": self.properties})
    }

    // `/global/event` frame; absent keys are omitted (D10).
    pub fn global(&self) -> Value {
        let mut frame = serde_json::Map::new();
        if let Some(directory) = &self.directory {
            frame.insert("directory".into(), directory.clone().into());
        }
        if let Some(project) = &self.project {
            frame.insert("project".into(), project.clone().into());
        }
        frame.insert("payload".into(), self.payload());
        Value::Object(frame)
    }
}

pub fn event_id() -> String {
    id::ascending(Prefix::Event)
}

#[derive(Debug, Clone, Default)]
pub struct Location {
    pub directory: String,
    pub project: String,
}

type Tap = Arc<dyn Fn(&Event) + Send + Sync>;

#[derive(Clone, Default)]
pub struct Bus {
    subscribers: Arc<Mutex<Vec<mpsc::UnboundedSender<Arc<Event>>>>>,
    tap: Arc<Mutex<Option<Tap>>>,
}

impl Bus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<Arc<Event>> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.subscribers.lock().unwrap_or_else(|p| p.into_inner()).push(tx);
        rx
    }

    // Called synchronously for every event, before the subscribers see it.
    pub fn set_tap(&self, tap: impl Fn(&Event) + Send + Sync + 'static) {
        *self.tap.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(tap));
    }

    fn emit(&self, event: Event) {
        let tap = self.tap.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if let Some(tap) = tap {
            tap(&event);
        }
        let event = Arc::new(event);
        self.subscribers.lock().unwrap_or_else(|p| p.into_inner()).retain(|tx| tx.send(event.clone()).is_ok());
    }

    // An instance-scoped event: reaches `/event` of the same directory and `/global/event`.
    pub fn publish(&self, location: &Location, kind: &str, properties: Value) {
        self.emit(Event {
            id: event_id(),
            kind: kind.to_string(),
            properties,
            directory: Some(location.directory.clone()),
            project: Some(location.project.clone()),
            global_only: false,
        });
    }

    // A direct global-bus emit.
    pub fn publish_global(&self, directory: Option<&str>, project: Option<&str>, kind: &str, properties: Value) {
        self.emit(Event {
            id: event_id(),
            kind: kind.to_string(),
            properties,
            directory: directory.map(str::to_string),
            project: project.map(str::to_string),
            global_only: true,
        });
    }
}
