// Rust build: input that reaches a session outside a plain prompt, after opencode 2.x `session/inbox.ts`.
//
// - Steering (the default delivery): a prompt sent while the session runs is stored at once and the running loop
//   reads it at its next step boundary, so it redirects the current run instead of waiting for it to end.
// - Queueing: the prompt waits until the session is idle, one queued prompt at a time per session.
// - Notices (a background shell ended, a `/goal` continuation): a synthetic user message, then the session is woken.
use crate::{Engine, Error, Host, RunEnd};
use opencode_storage::{Message, Part, PartBody, TextPart, UserMessage, UserTime, id};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

// How a prompt sent while the session runs is delivered (v2 `SessionInbox.Delivery`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Delivery {
    // At the next step boundary of the current run.
    #[default]
    Steer,
    // Once the session is idle.
    Queue,
}

impl Engine {
    // Waits until no run owns the session.
    pub(crate) async fn idle(&self, session_id: &str) {
        loop {
            let done = self.running.lock().unwrap_or_else(|e| e.into_inner()).get(session_id).map(|run| run.done.subscribe());
            let Some(mut done) = done else { return };
            let _ = done.wait_for(|end| !matches!(end, RunEnd::Running)).await;
        }
    }

    // Queued prompts of one session go one at a time.
    pub(crate) fn queue_lock(&self, session_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.queues.lock().unwrap_or_else(|e| e.into_inner()).entry(session_id.to_string()).or_default().clone()
    }

    // A synthetic user message with the agent and model of the session's latest user message.
    pub(crate) fn post_synthetic(&self, session_id: &str, text: &str, metadata: Value) -> Result<UserMessage, Error> {
        let rows = self.store.list_messages(session_id)?;
        let last = rows
            .iter()
            .rev()
            .find_map(|(m, _)| m.as_user())
            .ok_or_else(|| Error::Config(format!("session {session_id} has no user message")))?;
        let user = UserMessage {
            id: id::message(),
            session_id: session_id.to_string(),
            time: UserTime { created: id::now_ms() },
            format: None,
            summary: None,
            agent: last.agent.clone(),
            model: last.model.clone(),
            system: None,
            tools: None,
        };
        self.store.upsert_message(&Message::User(user.clone()))?;
        let metadata = match metadata {
            Value::Object(map) => Some(map.into_iter().collect()),
            _ => None,
        };
        let body = PartBody::Text(TextPart { text: text.to_string(), synthetic: Some(true), ignored: None, time: None, metadata });
        self.store.upsert_part(&Part::new(session_id, &user.id, body))?;
        Ok(user)
    }

    // Runs the session in the background: a running loop picks new input up at its next step; an idle session starts
    // one. Needs `attach`; without it the input waits for the next prompt.
    pub(crate) fn wake(&self, directory: PathBuf, session_id: String, host: Arc<dyn Host>) {
        let Some(engine) = self.this.get().and_then(std::sync::Weak::upgrade) else { return };
        tokio::spawn(async move {
            let Ok(ctx) = engine.instance(&directory, Some(&host)).await else { return };
            let _ = engine.run(ctx, &session_id, host, CancellationToken::new()).await;
        });
    }

    // A background shell ended: v2 delivers its `<shell>` notification as a steered synthetic message.
    pub(crate) fn background_finished(&self, directory: PathBuf, finished: opencode_tools::bash::background::Finished, host: Option<Arc<dyn Host>>) {
        if self.store.get_session(&finished.session_id).ok().flatten().is_none() {
            return;
        }
        if self.post_synthetic(&finished.session_id, &finished.notification(), finished.metadata()).is_err() {
            return;
        }
        if let Some(host) = host {
            self.wake(directory, finished.session_id, host);
        }
    }
}
