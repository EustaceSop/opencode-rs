// Runs engine turns and turns what the engine reports into the original's event types.
use crate::bus::Location;
use crate::error::{ApiError, ApiResult};
use crate::state::{AppState, Inner, Pending, PendingQuestion, lock};
use opencode_core::permission::{self, Action, Rule};
use opencode_session::{
    CommandInput, Error as EngineError, Host, PermissionAnswer, PermissionAsk, PromptInput, QuestionRequest, Reply, ShellInput, Status,
    Update,
};
use opencode_storage::id::{self, Prefix};
use opencode_storage::{Change, Message, Part, Session};
use opencode_tools::PermissionRequest;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub fn status_json(status: &Status) -> Value {
    match status {
        Status::Idle => json!({"type": "idle"}),
        Status::Busy => json!({"type": "busy"}),
        Status::Retry { attempt, message, next } => json!({"type": "retry", "attempt": attempt, "message": message, "next": next}),
    }
}

fn take<T>(list: &mut Vec<T>, test: impl Fn(&T) -> bool) -> Vec<T> {
    let (taken, rest) = std::mem::take(list).into_iter().partition(test);
    *list = rest;
    taken
}

impl Inner {
    pub fn is_busy(&self, session_id: &str) -> bool {
        self.engine.status(session_id) != Status::Idle
    }

    pub fn statuses(&self) -> Value {
        let mut out = serde_json::Map::new();
        for session_id in self.engine.busy() {
            let status = self.engine.status(&session_id);
            if status != Status::Idle {
                out.insert(session_id, status_json(&status));
            }
        }
        Value::Object(out)
    }

    fn session_location(&self, session_id: &str) -> Option<Location> {
        if let Some(location) = lock(&self.locations).get(session_id) {
            return Some(location.clone());
        }
        let session = self.engine.store.get_session(session_id).ok().flatten()?;
        Some(self.remember(&session))
    }

    fn remember(&self, session: &Session) -> Location {
        let location = self.location(session);
        lock(&self.locations).insert(session.id.clone(), location.clone());
        location
    }

    // Every row the store commits becomes the event the original's projector publishes for it.
    pub fn on_change(&self, change: &Change) {
        let (session_id, kind, properties) = match change {
            Change::SessionCreated(session) | Change::SessionUpdated(session) => {
                let kind = if matches!(change, Change::SessionCreated(_)) { "session.created" } else { "session.updated" };
                self.bus.publish(&self.remember(session), kind, json!({"sessionID": session.id, "info": session}));
                return;
            }
            Change::MessageUpdated(message) => {
                (message.session_id(), "message.updated", json!({"sessionID": message.session_id(), "info": message}))
            }
            Change::MessageRemoved { session_id, message_id } => {
                (session_id.as_str(), "message.removed", json!({"sessionID": session_id, "messageID": message_id}))
            }
            Change::PartUpdated(part) => {
                (part.session_id.as_str(), "message.part.updated", json!({"sessionID": part.session_id, "part": part, "time": id::now_ms()}))
            }
            Change::PartRemoved { session_id, message_id, part_id } => (
                session_id.as_str(),
                "message.part.removed",
                json!({"sessionID": session_id, "messageID": message_id, "partID": part_id}),
            ),
        };
        if let Some(location) = self.session_location(session_id) {
            self.bus.publish(&location, kind, properties);
            if let Change::PartUpdated(part) = change {
                self.v2_part(&location.directory, part);
            }
        }
    }

    pub fn abort(&self, session_id: &str) {
        self.engine.abort(session_id);
        // Unblock tools that are waiting on the user.
        let permissions = take(&mut lock(&self.pending), |p| p.session_id == session_id);
        for item in permissions {
            self.finish(item, PermissionAnswer::Reject);
        }
        let questions = take(&mut lock(&self.questions), |q| q.session_id == session_id);
        for item in questions {
            self.finish_question(item, None);
        }
    }

    fn finish(&self, item: Pending, answer: PermissionAnswer) {
        let name = match answer {
            PermissionAnswer::Once => "once",
            PermissionAnswer::Always => "always",
            PermissionAnswer::Reject | PermissionAnswer::Corrected(_) => "reject",
        };
        let replied = json!({"sessionID": item.session_id, "requestID": item.info["id"], "reply": name});
        self.v2_publish(Some(&item.location.directory), "permission.v2.replied", replied.clone());
        self.bus.publish(&item.location, "permission.replied", replied);
        let _ = item.reply.send(answer);
    }

    pub fn pending_permissions(&self) -> Vec<Value> {
        lock(&self.pending).iter().map(|p| p.info.clone()).collect()
    }

    // `message` turns a rejection into feedback for the model; the session's other requests are plainly rejected.
    pub fn reply_permission(&self, request_id: &str, reply: Reply, message: Option<&str>) -> ApiResult<()> {
        let (existing, others) = {
            let mut pending = lock(&self.pending);
            let Some(index) = pending.iter().position(|p| p.info["id"] == request_id) else {
                return Err(ApiError::tagged_not_found(
                    "PermissionNotFoundError",
                    "requestID",
                    request_id,
                    format!("Permission request not found: {request_id}"),
                ));
            };
            let existing = pending.remove(index);
            let session = existing.session_id.as_str();
            let others = match reply {
                Reply::Reject => take(&mut pending, |p| p.session_id == session),
                Reply::Once => Vec::new(),
                Reply::Always => {
                    let mut approved = lock(&self.approved);
                    approved.extend(existing.always.iter().map(|pattern| Rule::new(existing.permission.clone(), pattern.clone(), Action::Allow)));
                    let approved: &[Rule] = &approved;
                    take(&mut pending, |p| {
                        p.session_id == session
                            && p.patterns.iter().all(|pattern| permission::evaluate(&p.permission, pattern, &[approved]).action == Action::Allow)
                    })
                }
            };
            (existing, others)
        };
        let answer = match (reply, message.filter(|message| !message.is_empty())) {
            (Reply::Reject, Some(message)) => PermissionAnswer::Corrected(message.to_string()),
            (reply, _) => reply.into(),
        };
        self.finish(existing, answer);
        for item in others {
            self.finish(item, reply.into());
        }
        Ok(())
    }

    fn finish_question(&self, item: PendingQuestion, answers: Option<Vec<Vec<String>>>) {
        let request_id = &item.info["id"];
        let directory = Some(item.location.directory.as_str());
        match &answers {
            Some(answers) => {
                self.v2_publish(directory, "question.v2.replied", json!({"sessionID": item.session_id, "requestID": request_id, "answers": answers}))
            }
            None => self.v2_publish(directory, "question.v2.rejected", json!({"sessionID": item.session_id, "requestID": request_id})),
        }
        match &answers {
            Some(answers) => self.bus.publish(
                &item.location,
                "question.replied",
                json!({"sessionID": item.session_id, "requestID": request_id, "answers": answers}),
            ),
            None => self.bus.publish(&item.location, "question.rejected", json!({"sessionID": item.session_id, "requestID": request_id})),
        }
        let _ = item.reply.send(answers);
    }

    pub fn pending_questions(&self) -> Vec<Value> {
        lock(&self.questions).iter().map(|q| q.info.clone()).collect()
    }

    pub fn reply_question(&self, request_id: &str, answers: Option<Vec<Vec<String>>>) -> ApiResult<()> {
        let item = {
            let mut questions = lock(&self.questions);
            let Some(index) = questions.iter().position(|q| q.info["id"] == request_id) else {
                return Err(ApiError::tagged_not_found(
                    "QuestionNotFoundError",
                    "requestID",
                    request_id,
                    format!("Question request not found: {request_id}"),
                ));
            };
            questions.remove(index)
        };
        self.finish_question(item, answers);
        Ok(())
    }
}

// The engine's Host for one turn. Rows reach the bus through the store watcher; this carries everything else.
pub struct Tracker {
    state: AppState,
    session_id: String,
    // Subagent sessions live in the same directory and project as their parent.
    location: Location,
    errored: AtomicBool,
}

impl Tracker {
    fn new(state: AppState, session: &Session) -> Arc<Self> {
        Arc::new(Tracker { location: state.location(session), state, session_id: session.id.clone(), errored: AtomicBool::new(false) })
    }

    fn publish(&self, kind: &str, properties: Value) {
        self.state.bus.publish(&self.location, kind, properties);
    }
}

#[async_trait::async_trait]
impl Host for Tracker {
    fn update(&self, update: Update) {
        match update {
            Update::Child { update, .. } => self.update(*update),
            Update::Delta { session_id, message_id, part_id, field, delta } => {
                if let Ok(Some(part)) = self.state.engine.store.get_part(&part_id)
                    && matches!(part.kind(), "text" | "reasoning")
                {
                    self.state.v2_delta(&self.location.directory, &session_id, &message_id, &part_id, part.kind(), &delta);
                }
                self.publish(
                    "message.part.delta",
                    json!({"sessionID": session_id, "messageID": message_id, "partID": part_id, "field": field, "delta": delta}),
                )
            }
            Update::Status { session_id, status } => {
                self.publish("session.status", json!({"sessionID": session_id, "status": status_json(&status)}));
                if status == Status::Idle {
                    self.publish("session.idle", json!({"sessionID": session_id}));
                }
            }
            Update::Todos { session_id, todos } => self.publish("todo.updated", json!({"sessionID": session_id, "todos": todos})),
            Update::Compacted { session_id } => self.publish("session.compacted", json!({"sessionID": session_id})),
            Update::SessionError { session_id, error } => {
                if session_id == self.session_id {
                    self.errored.store(true, Ordering::SeqCst);
                }
                self.publish("session.error", json!({"sessionID": session_id, "error": error}));
            }
            _ => {}
        }
    }

    async fn permission(&self, request: &PermissionRequest) -> Reply {
        let ask = PermissionAsk { session_id: self.session_id.clone(), request: request.clone(), tool: None };
        match self.ask(&ask).await {
            PermissionAnswer::Once => Reply::Once,
            PermissionAnswer::Always => Reply::Always,
            PermissionAnswer::Reject | PermissionAnswer::Corrected(_) => Reply::Reject,
        }
    }

    async fn ask(&self, ask: &PermissionAsk) -> PermissionAnswer {
        let request = &ask.request;
        let metadata = if request.metadata.is_object() { request.metadata.clone() } else { json!({}) };
        let mut info = json!({
            "id": id::ascending(Prefix::Permission),
            "sessionID": ask.session_id,
            "permission": request.permission,
            "patterns": request.patterns,
            "metadata": metadata,
            "always": request.always,
        });
        if let Some((message_id, call_id)) = &ask.tool {
            info["tool"] = json!({"messageID": message_id, "callID": call_id});
        }
        let (tx, rx) = oneshot::channel();
        lock(&self.state.pending).push(Pending {
            info: info.clone(),
            session_id: ask.session_id.clone(),
            permission: request.permission.clone(),
            patterns: request.patterns.clone(),
            always: request.always.clone(),
            location: self.location.clone(),
            reply: tx,
        });
        self.state.v2_publish(Some(&self.location.directory), "permission.v2.asked", crate::v2::permission_v2(&info));
        self.publish("permission.asked", info);
        rx.await.unwrap_or(PermissionAnswer::Reject)
    }

    async fn question(&self, request: &QuestionRequest) -> Option<Vec<Vec<String>>> {
        let mut info = json!({"id": request.id, "sessionID": request.session_id, "questions": request.questions});
        if let Some((message_id, call_id)) = &request.tool {
            info["tool"] = json!({"messageID": message_id, "callID": call_id});
        }
        let (tx, rx) = oneshot::channel();
        lock(&self.state.questions).push(PendingQuestion {
            info: info.clone(),
            session_id: request.session_id.clone(),
            location: self.location.clone(),
            reply: tx,
        });
        self.state.v2_publish(Some(&self.location.directory), "question.v2.asked", info.clone());
        self.publish("question.asked", info);
        rx.await.ok().flatten()
    }
}

pub enum Job {
    Prompt(Box<PromptInput>),
    Command(Box<CommandInput>),
    Summarize { auto: bool },
    Shell(Box<ShellInput>),
}

// Runs one job to completion. Returns the message it produced: the last new assistant message, else the last new message.
pub async fn run_turn(state: AppState, session: Session, job: Job) -> ApiResult<Option<(Message, Vec<Part>)>> {
    let tracker = Tracker::new(state.clone(), &session);
    let store = &state.engine.store;
    let before: HashSet<String> = store.list_messages(&session.id)?.iter().map(|(message, _)| message.id().to_string()).collect();
    // Connects the directory's MCP servers (as the loop would) so their events reach the bus from the start.
    let _ = state.mcp(&session.directory).await;
    let host: Arc<dyn Host> = tracker.clone();
    let token = CancellationToken::new();
    let command = match &job {
        Job::Command(input) => Some((input.command.clone(), input.arguments.clone())),
        _ => None,
    };
    let result = match job {
        Job::Prompt(input) => state.engine.prompt(*input, host, token).await.map(|_| ()),
        Job::Command(input) => state.engine.command(*input, host, token).await.map(|_| ()),
        Job::Summarize { auto } => state.engine.summarize(&session.id, auto, host, token).await.map(|_| ()),
        Job::Shell(input) => state.engine.shell(*input, host, token).await.map(|_| ()),
    };

    let fresh: Vec<(Message, Vec<Part>)> = store.list_messages(&session.id)?.into_iter().filter(|(message, _)| !before.contains(message.id())).collect();
    let produced = fresh.iter().rposition(|(message, _)| matches!(message, Message::Assistant(_))).or(fresh.len().checked_sub(1));
    let produced = produced.and_then(|index| fresh.into_iter().nth(index));
    match result {
        Ok(()) => {
            if let (Some((name, arguments)), Some((message, _))) = (command, &produced) {
                tracker.publish(
                    "command.executed",
                    json!({"name": name, "sessionID": session.id, "arguments": arguments, "messageID": message.id()}),
                );
            }
            Ok(produced)
        }
        Err(error) => {
            if !matches!(error, EngineError::Aborted | EngineError::Busy(_)) && !tracker.errored.load(Ordering::SeqCst) {
                let failure = json!({"name": "UnknownError", "data": {"message": error.to_string()}});
                tracker.publish("session.error", json!({"sessionID": session.id, "error": failure}));
            }
            match (error, produced) {
                (EngineError::Busy(_), _) => Err(ApiError::busy(&session.id)),
                // The failure is recorded on the assistant message, which is what the original returns.
                (_, Some(row @ (Message::Assistant(_), _))) => Ok(Some(row)),
                _ => Err(ApiError::bad_request()),
            }
        }
    }
}
