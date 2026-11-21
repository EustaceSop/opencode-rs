// Pseudo-terminals: sessions keyed by id and scoped to an instance directory, connect tickets, and the WebSocket protocol.
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use opencode_storage::id::{self, Prefix};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

// UTF-16 code units, like every cursor and limit of the protocol.
pub const BUFFER_LIMIT: usize = 1024 * 1024 * 2;
pub const REPLAY_CHUNK: usize = 64 * 1024;
const EXITED_LIMIT: usize = 25;
const CURSOR_QUERY: &str = "\u{1b}[6n";
pub const TICKET_TTL: Duration = Duration::from_secs(60);
const TICKET_CAPACITY: usize = 10_000;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Info {
    pub id: String,
    pub title: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub status: &'static str,
    pub pid: u32,
    #[serde(rename = "exitCode", skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<u32>,
}

#[derive(Debug, Default)]
pub struct CreateInput {
    pub command: Option<String>,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub enum Out {
    Data(String),
    Meta(u64),
    Close(u16, &'static str),
}

#[derive(Debug, PartialEq)]
pub enum AttachError {
    NotFound,
    Exited,
}

struct Output {
    buffer: VecDeque<u16>,
    // Absolute offset of the first retained unit, and of the end of all output.
    start: u64,
    cursor: u64,
    subscribers: Vec<(u64, mpsc::UnboundedSender<Out>)>,
}

struct Session {
    directory: String,
    info: Mutex<Info>,
    output: Mutex<Output>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    last_read: AtomicU64,
}

impl Session {
    fn running(&self) -> bool {
        lock(&self.info).status == "running"
    }

    fn push(&self, chunk: String) {
        let units: Vec<u16> = chunk.encode_utf16().collect();
        let mut output = lock(&self.output);
        output.cursor += units.len() as u64;
        output.subscribers.retain(|(_, tx)| tx.send(Out::Data(chunk.clone())).is_ok());
        output.buffer.extend(units);
        if output.buffer.len() > BUFFER_LIMIT {
            let excess = output.buffer.len() - BUFFER_LIMIT;
            output.buffer.drain(..excess);
            output.start += excess as u64;
        }
    }

    fn end(&self, frame: Out) {
        for (_, tx) in lock(&self.output).subscribers.drain(..) {
            let _ = tx.send(frame.clone());
        }
    }

    fn write(&self, data: &str) {
        if !self.running() {
            return;
        }
        if let Some(writer) = lock(&self.writer).as_mut() {
            let _ = writer.write_all(data.as_bytes()).and_then(|()| writer.flush());
        }
    }

    fn teardown(&self) {
        if self.running() {
            let _ = lock(&self.killer).kill();
        }
        self.end(Out::Close(1000, ""));
        self.release();
    }

    // Closing a pseudoconsole can wait on its host process, so it never happens on the caller's thread.
    fn release(&self) {
        let writer = lock(&self.writer).take();
        let master = lock(&self.master).take();
        if writer.is_some() || master.is_some() {
            std::thread::spawn(move || {
                drop(writer);
                drop(master);
            });
        }
    }
}

struct Ticket {
    pty_id: String,
    directory: String,
    issued: Instant,
}

type Publish = dyn Fn(&str, &str, Value) + Send + Sync;

pub struct PtyManager {
    sessions: Mutex<Vec<(String, Arc<Session>)>>,
    exit_order: Mutex<Vec<String>>,
    tickets: Mutex<HashMap<String, Ticket>>,
    closing: AtomicBool,
    next_subscriber: AtomicU64,
    // (directory, event type, data)
    publish: Mutex<Option<Arc<Publish>>>,
}

impl Default for PtyManager {
    fn default() -> Self {
        Self {
            sessions: Mutex::default(),
            exit_order: Mutex::default(),
            tickets: Mutex::default(),
            closing: AtomicBool::new(false),
            next_subscriber: AtomicU64::new(0),
            publish: Mutex::new(None),
        }
    }
}

pub struct Attachment {
    session: Arc<Session>,
    token: u64,
    pub rx: mpsc::UnboundedReceiver<Out>,
}

impl Attachment {
    pub fn write(&self, data: &str) {
        self.session.write(data);
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        lock(&self.session.output).subscribers.retain(|(token, _)| *token != self.token);
    }
}

fn now_ms() -> u64 {
    id::now_ms()
}

// Splits at UTF-16 offsets; a pair cut in half becomes U+FFFD, which is what a text frame would carry anyway.
pub fn chunks(units: &[u16]) -> Vec<String> {
    units.chunks(REPLAY_CHUNK).map(String::from_utf16_lossy).collect()
}

pub fn meta_frame(cursor: u64) -> Vec<u8> {
    let mut out = vec![0u8];
    out.extend_from_slice(json!({"cursor": cursor}).to_string().as_bytes());
    out
}

// The valid UTF-8 prefix of `pending` as text; an incomplete trailing sequence stays for the next read.
fn decode(pending: &mut Vec<u8>) -> String {
    let mut out = String::new();
    loop {
        match std::str::from_utf8(pending) {
            Ok(text) => {
                out.push_str(text);
                pending.clear();
                return out;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                out.push_str(&String::from_utf8_lossy(&pending[..valid]));
                match error.error_len() {
                    Some(bad) => {
                        out.push('\u{fffd}');
                        pending.drain(..valid + bad);
                    }
                    None => {
                        pending.drain(..valid);
                        return out;
                    }
                }
            }
        }
    }
}

impl PtyManager {
    pub fn set_publisher(&self, publish: Arc<Publish>) {
        *lock(&self.publish) = Some(publish);
    }

    fn emit(&self, directory: &str, kind: &str, data: Value) {
        let publish = lock(&self.publish).clone();
        if let Some(publish) = publish {
            publish(directory, kind, data);
        }
    }

    fn find(&self, directory: &str, id: &str) -> Option<Arc<Session>> {
        lock(&self.sessions).iter().find(|(key, session)| key == id && session.directory == directory).map(|(_, s)| s.clone())
    }

    pub fn list(&self, directory: &str) -> Vec<Info> {
        lock(&self.sessions).iter().filter(|(_, s)| s.directory == directory).map(|(_, s)| lock(&s.info).clone()).collect()
    }

    pub fn get(&self, directory: &str, id: &str) -> Option<Info> {
        self.find(directory, id).map(|session| lock(&session.info).clone())
    }

    pub fn create(self: &Arc<Self>, directory: &str, shell: Option<&str>, input: CreateInput) -> Result<Info, String> {
        let pty_id = id::ascending(Prefix::Pty);
        let command = input.command.filter(|c| !c.is_empty()).unwrap_or_else(|| opencode_tools::shell::preferred(shell));
        let mut args = input.args;
        if matches!(opencode_tools::shell::name(&command).as_str(), "bash" | "dash" | "fish" | "ksh" | "sh" | "zsh") {
            args.push("-l".to_string());
        }
        let cwd = input.cwd.filter(|c| !c.is_empty()).unwrap_or_else(|| directory.to_string());
        let pair = native_pty_system()
            .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| e.to_string())?;
        let mut builder = CommandBuilder::new(&command);
        builder.args(&args);
        builder.cwd(&cwd);
        for (key, value) in &input.env {
            builder.env(key, value);
        }
        builder.env("TERM", "xterm-256color");
        builder.env("OPENCODE_TERMINAL", "1");
        if cfg!(windows) {
            for key in ["LC_ALL", "LC_CTYPE", "LANG"] {
                builder.env(key, "C.UTF-8");
            }
        }
        let mut child = pair.slave.spawn_command(builder).map_err(|e| e.to_string())?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
        let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
        let info = Info {
            title: input.title.filter(|t| !t.is_empty()).unwrap_or_else(|| format!("Terminal {}", &pty_id[pty_id.len() - 4..])),
            id: pty_id.clone(),
            command,
            args,
            cwd,
            status: "running",
            pid: child.process_id().unwrap_or(0),
            exit_code: None,
        };
        let session = Arc::new(Session {
            directory: directory.to_string(),
            info: Mutex::new(info.clone()),
            output: Mutex::new(Output { buffer: VecDeque::new(), start: 0, cursor: 0, subscribers: Vec::new() }),
            master: Mutex::new(Some(pair.master)),
            writer: Mutex::new(Some(writer)),
            killer: Mutex::new(child.clone_killer()),
            last_read: AtomicU64::new(now_ms()),
        });
        lock(&self.sessions).push((pty_id.clone(), session.clone()));

        let sink = session.clone();
        std::thread::spawn(move || {
            let mut pending = Vec::new();
            let mut chunk = [0u8; 8192];
            // This ConPTY starts by asking where the cursor is and holds the shell back until told. The original's
            // PTY layer never asks, so the question is answered here and kept out of the output.
            let mut greeted = !cfg!(windows);
            while let Ok(n) = reader.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                pending.extend_from_slice(&chunk[..n]);
                let mut text = decode(&mut pending);
                if !greeted && let Some(at) = text.find(CURSOR_QUERY) {
                    greeted = true;
                    text.replace_range(at..at + CURSOR_QUERY.len(), "");
                    sink.write("\u{1b}[1;1R");
                }
                sink.last_read.store(now_ms(), Ordering::SeqCst);
                if !text.is_empty() {
                    sink.push(text);
                }
            }
        });

        let manager = Arc::downgrade(self);
        let exited = session.clone();
        std::thread::spawn(move || {
            let status = child.wait();
            // Output written just before the exit may still be in flight.
            let deadline = Instant::now() + Duration::from_millis(1000);
            while Instant::now() < deadline && now_ms().saturating_sub(exited.last_read.load(Ordering::SeqCst)) < 100 {
                std::thread::sleep(Duration::from_millis(20));
            }
            let code = status.map(|s| s.exit_code()).unwrap_or(1);
            {
                let mut info = lock(&exited.info);
                if info.status == "exited" {
                    return;
                }
                info.status = "exited";
                info.exit_code = Some(code);
            }
            exited.end(Out::Close(1000, ""));
            exited.release();
            if let Some(manager) = manager.upgrade() {
                manager.exited(&exited.directory, &pty_id, code);
            }
        });

        self.emit(directory, "pty.created", json!({"info": info}));
        Ok(info)
    }

    fn exited(&self, directory: &str, id: &str, code: u32) {
        if !lock(&self.sessions).iter().any(|(key, _)| key == id) {
            return;
        }
        self.emit(directory, "pty.exited", json!({"id": id, "exitCode": code}));
        let stale: Vec<String> = {
            let mut order = lock(&self.exit_order);
            order.push(id.to_string());
            let excess = order.len().saturating_sub(EXITED_LIMIT);
            order.drain(..excess).collect()
        };
        for old in stale {
            self.remove_session(&old);
        }
    }

    fn remove_session(&self, id: &str) -> bool {
        let session = {
            let mut sessions = lock(&self.sessions);
            let Some(index) = sessions.iter().position(|(key, _)| key == id) else { return false };
            sessions.remove(index).1
        };
        lock(&self.exit_order).retain(|key| key != id);
        session.teardown();
        self.emit(&session.directory, "pty.deleted", json!({"id": id}));
        true
    }

    pub fn update(&self, directory: &str, id: &str, title: Option<&str>, size: Option<(u16, u16)>) -> Option<Info> {
        let session = self.find(directory, id)?;
        if let Some(title) = title.filter(|t| !t.is_empty()) {
            lock(&session.info).title = title.to_string();
        }
        if let (Some((rows, cols)), true) = (size, session.running())
            && let Some(master) = lock(&session.master).as_ref()
        {
            let _ = master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        }
        let info = lock(&session.info).clone();
        self.emit(directory, "pty.updated", json!({"info": info}));
        Some(info)
    }

    pub fn remove(&self, directory: &str, id: &str) -> bool {
        self.find(directory, id).is_some() && self.remove_session(id)
    }

    // Everything of one instance directory goes away with the instance.
    pub fn dispose_directory(&self, directory: &str) {
        let ids: Vec<String> = lock(&self.sessions).iter().filter(|(_, s)| s.directory == directory).map(|(id, _)| id.clone()).collect();
        for id in ids {
            self.remove_session(&id);
        }
    }

    pub fn dispose_all(&self) {
        let ids: Vec<String> = lock(&self.sessions).iter().map(|(id, _)| id.clone()).collect();
        for id in ids {
            self.remove_session(&id);
        }
    }

    // Open sockets get 1001, then every process is killed.
    pub fn shutdown(&self) {
        self.closing.store(true, Ordering::SeqCst);
        let sessions: Vec<Arc<Session>> = lock(&self.sessions).drain(..).map(|(_, s)| s).collect();
        lock(&self.exit_order).clear();
        for session in sessions {
            session.end(Out::Close(1001, "server closing"));
            session.teardown();
        }
    }

    pub fn closing(&self) -> bool {
        self.closing.load(Ordering::SeqCst)
    }

    // `cursor`: -1 tails, None replays everything retained, n replays from absolute offset n.
    pub fn attach(&self, directory: &str, id: &str, cursor: Option<i64>) -> Result<Attachment, AttachError> {
        let session = self.find(directory, id).ok_or(AttachError::NotFound)?;
        if !session.running() {
            return Err(AttachError::Exited);
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let token = self.next_subscriber.fetch_add(1, Ordering::SeqCst);
        {
            let mut output = lock(&session.output);
            let end = output.cursor;
            let from = match cursor {
                Some(-1) => end,
                Some(n) => n.max(0) as u64,
                None => 0,
            };
            if from < end {
                let offset = from.saturating_sub(output.start) as usize;
                let units: Vec<u16> = output.buffer.iter().skip(offset).copied().collect();
                for chunk in chunks(&units) {
                    let _ = tx.send(Out::Data(chunk));
                }
            }
            let _ = tx.send(Out::Meta(end));
            output.subscribers.push((token, tx));
        }
        Ok(Attachment { session, token, rx })
    }

    pub fn issue_ticket(&self, directory: &str, id: &str) -> Value {
        let ticket = uuid::Uuid::new_v4().to_string();
        let mut tickets = lock(&self.tickets);
        let now = Instant::now();
        tickets.retain(|_, t| now.duration_since(t.issued) < TICKET_TTL);
        if tickets.len() >= TICKET_CAPACITY
            && let Some(oldest) = tickets.iter().min_by_key(|(_, t)| t.issued).map(|(key, _)| key.clone())
        {
            tickets.remove(&oldest);
        }
        tickets.insert(ticket.clone(), Ticket { pty_id: id.to_string(), directory: directory.to_string(), issued: now });
        json!({"ticket": ticket, "expires_in": TICKET_TTL.as_secs()})
    }

    // Single use; a ticket presented for another PTY or directory stays valid for its own scope.
    pub fn consume_ticket(&self, directory: &str, id: &str, ticket: &str) -> bool {
        let mut tickets = lock(&self.tickets);
        let valid = tickets.get(ticket).is_some_and(|t| t.pty_id == id && t.directory == directory && t.issued.elapsed() < TICKET_TTL);
        if valid {
            tickets.remove(ticket);
        }
        valid
    }
}

fn close(code: u16, reason: &'static str) -> Message {
    Message::Close(Some(CloseFrame { code, reason: reason.into() }))
}

// One writer drains the queue, so replay, cursor frame, live output and the close frame keep their order.
pub async fn serve_socket(mut socket: WebSocket, attached: Result<Attachment, AttachError>, exited_reason: &'static str, closing: bool) {
    if closing {
        let _ = socket.send(close(1001, "server closing")).await;
        return;
    }
    let mut attachment = match attached {
        Ok(attachment) => attachment,
        Err(error) => {
            let reason = if error == AttachError::Exited { exited_reason } else { "session not found" };
            let _ = socket.send(close(4404, reason)).await;
            return;
        }
    };
    let (mut sink, mut stream) = socket.split();
    loop {
        tokio::select! {
            out = attachment.rx.recv() => {
                let message = match out {
                    Some(Out::Data(text)) => Message::Text(text.into()),
                    Some(Out::Meta(cursor)) => Message::Binary(meta_frame(cursor).into()),
                    Some(Out::Close(code, reason)) => {
                        let _ = sink.send(close(code, reason)).await;
                        break;
                    }
                    None => break,
                };
                if sink.send(message).await.is_err() {
                    break;
                }
            }
            incoming = stream.next() => match incoming {
                Some(Ok(Message::Text(text))) => attachment.write(text.as_str()),
                // Strict UTF-8; anything else is dropped.
                Some(Ok(Message::Binary(bytes))) => {
                    if let Ok(text) = std::str::from_utf8(&bytes) {
                        attachment.write(text);
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            }
        }
    }
}

// `Shell.list`: the shells of this machine, and whether the agent may use each.
pub fn shells() -> Vec<opencode_tools::shell::Item> {
    opencode_tools::shell::list()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_split_across_reads_is_reassembled() {
        let bytes = "aé😀".as_bytes();
        let mut pending = bytes[..2].to_vec();
        assert_eq!(decode(&mut pending), "a");
        pending.extend_from_slice(&bytes[2..5]);
        assert_eq!(decode(&mut pending), "é");
        pending.extend_from_slice(&bytes[5..]);
        assert_eq!(decode(&mut pending), "😀");
        let mut bad = vec![b'a', 0xff, b'b'];
        assert_eq!(decode(&mut bad), "a\u{fffd}b");
    }

    #[test]
    fn frames_and_chunks_count_utf16_units() {
        assert_eq!(meta_frame(42), [&[0u8][..], br#"{"cursor":42}"#].concat());
        let units: Vec<u16> = "😀".repeat(REPLAY_CHUNK / 2 + 1).encode_utf16().collect();
        let parts = chunks(&units);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].encode_utf16().count(), REPLAY_CHUNK);
    }

    #[test]
    fn tickets_are_single_use_and_scoped() {
        let manager = PtyManager::default();
        let issued = manager.issue_ticket("/a", "pty_1");
        assert_eq!(issued["expires_in"], 60);
        let ticket = issued["ticket"].as_str().unwrap();
        assert!(!manager.consume_ticket("/b", "pty_1", ticket));
        assert!(!manager.consume_ticket("/a", "pty_2", ticket));
        assert!(manager.consume_ticket("/a", "pty_1", ticket));
        assert!(!manager.consume_ticket("/a", "pty_1", ticket));
    }
}
