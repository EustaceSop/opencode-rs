use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::id::now_ms;
use crate::types::{Message, Part, Project, Session, SessionTokens, TokenCache, Todo, Tokens};

pub const DATABASE_FILE: &str = "opencode-rs.db";
const FOREIGN_DATABASE_FILE: &str = "opencode.db";

const MIGRATIONS: &[(&str, &str)] = &[(
    "0001_init",
    "
    CREATE TABLE project (
      id TEXT PRIMARY KEY,
      worktree TEXT NOT NULL,
      time_created INTEGER NOT NULL,
      time_updated INTEGER NOT NULL,
      data TEXT NOT NULL
    );
    CREATE TABLE session (
      id TEXT PRIMARY KEY,
      project_id TEXT NOT NULL REFERENCES project(id) ON DELETE CASCADE,
      parent_id TEXT,
      time_created INTEGER NOT NULL,
      time_updated INTEGER NOT NULL,
      cost REAL NOT NULL DEFAULT 0,
      tokens_input INTEGER NOT NULL DEFAULT 0,
      tokens_output INTEGER NOT NULL DEFAULT 0,
      tokens_reasoning INTEGER NOT NULL DEFAULT 0,
      tokens_cache_read INTEGER NOT NULL DEFAULT 0,
      tokens_cache_write INTEGER NOT NULL DEFAULT 0,
      data TEXT NOT NULL
    );
    CREATE TABLE message (
      id TEXT PRIMARY KEY,
      session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE,
      time_created INTEGER NOT NULL,
      time_updated INTEGER NOT NULL,
      data TEXT NOT NULL
    );
    CREATE TABLE part (
      id TEXT PRIMARY KEY,
      message_id TEXT NOT NULL REFERENCES message(id) ON DELETE CASCADE,
      session_id TEXT NOT NULL,
      time_created INTEGER NOT NULL,
      time_updated INTEGER NOT NULL,
      data TEXT NOT NULL
    );
    CREATE INDEX session_project_idx ON session (project_id, time_updated, id);
    CREATE INDEX session_parent_idx ON session (parent_id);
    CREATE INDEX message_session_idx ON message (session_id, id);
    CREATE INDEX part_message_id_id_idx ON part (message_id, id);
    CREATE INDEX part_session_idx ON part (session_id);
    ",
), (
    "0002_todo",
    "
    CREATE TABLE todo (
      session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE,
      content TEXT NOT NULL,
      status TEXT NOT NULL,
      priority TEXT NOT NULL,
      position INTEGER NOT NULL,
      time_created INTEGER NOT NULL,
      time_updated INTEGER NOT NULL,
      PRIMARY KEY (session_id, position)
    );
    ",
), (
    "0003_event",
    "
    CREATE TABLE event_sequence (
      aggregate_id TEXT PRIMARY KEY,
      seq INTEGER NOT NULL,
      owner_id TEXT
    );
    CREATE TABLE event (
      id TEXT PRIMARY KEY,
      aggregate_id TEXT NOT NULL REFERENCES event_sequence(aggregate_id) ON DELETE CASCADE,
      seq INTEGER NOT NULL,
      type TEXT NOT NULL,
      data TEXT NOT NULL
    );
    CREATE UNIQUE INDEX event_aggregate_seq_idx ON event (aggregate_id, seq);
    CREATE INDEX event_aggregate_type_seq_idx ON event (aggregate_id, type, seq);
    ",
), (
    "0004_workspace",
    "
    CREATE TABLE workspace (
      id TEXT PRIMARY KEY,
      type TEXT NOT NULL,
      name TEXT NOT NULL DEFAULT '',
      branch TEXT,
      directory TEXT,
      extra TEXT,
      project_id TEXT NOT NULL REFERENCES project(id) ON DELETE CASCADE,
      time_used INTEGER NOT NULL
    );
    ",
), (
    "0005_project_directory",
    "
    CREATE TABLE project_directory (
      project_id TEXT NOT NULL REFERENCES project(id) ON DELETE CASCADE,
      directory TEXT NOT NULL,
      type TEXT,
      strategy TEXT,
      time_created INTEGER NOT NULL,
      PRIMARY KEY (project_id, directory)
    );
    ",
), (
    "0006_account",
    "
    CREATE TABLE account (
      id TEXT PRIMARY KEY,
      email TEXT NOT NULL,
      url TEXT NOT NULL,
      access_token TEXT NOT NULL,
      refresh_token TEXT NOT NULL,
      token_expiry INTEGER,
      time_created INTEGER NOT NULL,
      time_updated INTEGER NOT NULL
    );
    CREATE TABLE account_state (
      id INTEGER PRIMARY KEY,
      active_account_id TEXT REFERENCES account(id) ON DELETE SET NULL,
      active_org_id TEXT
    );
    ",
)];

const SESSION_COLUMNS: &str = "data, cost, tokens_input, tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{kind} not found: {id}")]
    NotFound { kind: &'static str, id: String },
    #[error("{kind} already exists: {id}")]
    AlreadyExists { kind: &'static str, id: String },
    #[error("refusing to open {0}: it belongs to the TypeScript build")]
    ForeignDatabase(PathBuf),
    // `EventV2.InvalidDurableEvent`: a replayed event that does not fit the log.
    #[error("{message}")]
    InvalidEvent { kind: String, message: String },
}

pub type Result<T> = std::result::Result<T, StoreError>;

// A committed row write, reported to `Store::watch` listeners. Session deletes are not reported.
#[derive(Debug, Clone)]
pub enum Change {
    SessionCreated(Session),
    SessionUpdated(Session),
    MessageUpdated(Message),
    MessageRemoved { session_id: String, message_id: String },
    PartUpdated(Part),
    PartRemoved { session_id: String, message_id: String, part_id: String },
}

type Listener = Arc<dyn Fn(&Change) + Send + Sync>;

#[derive(Clone, Default)]
struct Listeners(Arc<Mutex<Vec<Listener>>>);

impl std::fmt::Debug for Listeners {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Listeners")
    }
}

#[derive(Debug, Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
    listeners: Listeners,
}

type SessionRow = (String, f64, f64, f64, f64, f64, f64);

fn session_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
}

fn session_from_row(row: SessionRow) -> Result<Session> {
    let mut session: Session = serde_json::from_str(&row.0)?;
    session.cost = Some(row.1);
    session.tokens = Some(SessionTokens {
        input: row.2,
        output: row.3,
        reasoning: row.4,
        cache: TokenCache {
            read: row.5,
            write: row.6,
        },
    });
    Ok(session)
}

fn not_found(kind: &'static str, id: &str) -> StoreError {
    StoreError::NotFound {
        kind,
        id: id.to_string(),
    }
}

pub(crate) fn read_session(connection: &Connection, id: &str) -> Result<Option<Session>> {
    connection
        .query_row(
            &format!("SELECT {SESSION_COLUMNS} FROM session WHERE id = ?1"),
            [id],
            session_row,
        )
        .optional()?
        .map(session_from_row)
        .transpose()
}

fn query_sessions(connection: &Connection, clause: &str, value: &str) -> Result<Vec<Session>> {
    let mut statement = connection.prepare(&format!(
        "SELECT {SESSION_COLUMNS} FROM session WHERE {clause} ORDER BY time_updated DESC, id DESC"
    ))?;
    let rows = statement.query_map([value], session_row)?;
    rows.map(|row| session_from_row(row?)).collect()
}

// Inserts the row of a new session; false when the id is taken.
pub(crate) fn insert_session(connection: &Connection, session: &Session) -> Result<bool> {
    let inserted = connection.execute(
        "INSERT INTO session (id, project_id, parent_id, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, ?4, ?5, '') ON CONFLICT(id) DO NOTHING",
        params![
            session.id,
            session.project_id,
            session.parent_id,
            session.time.created as i64,
            session.time.updated as i64,
        ],
    )?;
    if inserted == 0 {
        return Ok(false);
    }
    write_session(connection, session)?;
    // The session projector marks the workspace it was created in as used.
    if let Some(workspace) = &session.workspace_id {
        connection.execute("UPDATE workspace SET time_used = ?2 WHERE id = ?1", params![workspace, now_ms() as i64])?;
    }
    Ok(true)
}

// The durable `SessionV1.Event` the original logs with every session, message and part write.
fn log(connection: &Connection, session_id: &str, kind: &str, data: serde_json::Value) -> Result<()> {
    crate::events::append(connection, &crate::id::ascending(crate::id::Prefix::Event), session_id, kind, &data).map(|_| ())
}

// Drops the event logs of the given sessions (`EventV2.remove`).
fn forget(connection: &Connection, session_ids: &[String]) -> Result<()> {
    for id in session_ids {
        connection.execute("DELETE FROM event_sequence WHERE aggregate_id = ?1", [id])?;
    }
    Ok(())
}

pub(crate) fn write_session(connection: &Connection, session: &Session) -> Result<usize> {
    let tokens = session.tokens.unwrap_or_default();
    Ok(connection.execute(
        "UPDATE session SET project_id = ?2, parent_id = ?3, time_updated = ?4, cost = ?5, tokens_input = ?6,
           tokens_output = ?7, tokens_reasoning = ?8, tokens_cache_read = ?9, tokens_cache_write = ?10, data = ?11
         WHERE id = ?1",
        params![
            session.id,
            session.project_id,
            session.parent_id,
            session.time.updated as i64,
            session.cost.unwrap_or(0.0),
            tokens.input,
            tokens.output,
            tokens.reasoning,
            tokens.cache.read,
            tokens.cache.write,
            serde_json::to_string(session)?,
        ],
    )?)
}

// Session totals are the signed sum of step-finish parts; `time_updated` is left untouched.
pub(crate) fn apply_usage(
    connection: &Connection,
    session_id: &str,
    cost: f64,
    tokens: &Tokens,
    sign: f64,
) -> Result<()> {
    connection.execute(
        "UPDATE session SET cost = cost + ?2, tokens_input = tokens_input + ?3, tokens_output = tokens_output + ?4,
           tokens_reasoning = tokens_reasoning + ?5, tokens_cache_read = tokens_cache_read + ?6,
           tokens_cache_write = tokens_cache_write + ?7
         WHERE id = ?1",
        params![
            session_id,
            cost * sign,
            tokens.input * sign,
            tokens.output * sign,
            tokens.reasoning * sign,
            tokens.cache.read * sign,
            tokens.cache.write * sign,
        ],
    )?;
    Ok(())
}

pub(crate) fn revoke_usage(connection: &Connection, data: &str) -> Result<()> {
    let part: Part = serde_json::from_str(data)?;
    if let Some((cost, tokens)) = part.usage() {
        apply_usage(connection, &part.session_id, cost, tokens, -1.0)?;
    }
    Ok(())
}

pub(crate) fn write_message(connection: &Connection, message: &Message) -> Result<()> {
    connection.execute(
        "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(id) DO UPDATE SET data = excluded.data, time_updated = excluded.time_updated",
        params![
            message.id(),
            message.session_id(),
            message.time_created() as i64,
            now_ms() as i64,
            serde_json::to_string(message)?,
        ],
    )?;
    Ok(())
}

// Deletes a message with its parts, taking their usage off the session; the message's session when it existed.
pub(crate) fn remove_message(connection: &Connection, id: &str) -> Result<Option<String>> {
    let session_id: Option<String> = connection
        .query_row("SELECT session_id FROM message WHERE id = ?1", [id], |row| row.get(0))
        .optional()?;
    let parts: Vec<String> = connection
        .prepare("SELECT data FROM part WHERE message_id = ?1")?
        .query_map([id], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for data in &parts {
        revoke_usage(connection, data)?;
    }
    connection.execute("DELETE FROM message WHERE id = ?1", [id])?;
    Ok(session_id)
}

// Session totals follow the step-finish parts: the previous version's usage is taken off, the new one added.
pub(crate) fn write_part(connection: &Connection, part: &Part, time_created: u64) -> Result<()> {
    let previous: Option<String> = connection
        .query_row("SELECT data FROM part WHERE id = ?1", [&part.id], |row| row.get(0))
        .optional()?;
    if let Some(previous) = previous {
        revoke_usage(connection, &previous)?;
    }
    connection.execute(
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(id) DO UPDATE SET data = excluded.data, time_updated = excluded.time_updated",
        params![
            part.id,
            part.message_id,
            part.session_id,
            time_created as i64,
            now_ms() as i64,
            serde_json::to_string(part)?
        ],
    )?;
    if let Some((cost, tokens)) = part.usage() {
        apply_usage(connection, &part.session_id, cost, tokens, 1.0)?;
    }
    Ok(())
}

// The removed part's stored data, when there was one.
pub(crate) fn remove_part(connection: &Connection, id: &str) -> Result<Option<String>> {
    let previous: Option<String> = connection
        .query_row("SELECT data FROM part WHERE id = ?1", [id], |row| row.get(0))
        .optional()?;
    let Some(previous) = previous else {
        return Ok(None);
    };
    revoke_usage(connection, &previous)?;
    connection.execute("DELETE FROM part WHERE id = ?1", [id])?;
    Ok(Some(previous))
}

fn migrate(connection: &mut Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS migration (id TEXT PRIMARY KEY, time_completed INTEGER NOT NULL)",
    )?;
    for (id, sql) in MIGRATIONS {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let applied: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM migration WHERE id = ?1)",
            [id],
            |row| row.get(0),
        )?;
        if !applied {
            transaction.execute_batch(sql)?;
            transaction.execute(
                "INSERT INTO migration (id, time_completed) VALUES (?1, ?2)",
                params![id, now_ms() as i64],
            )?;
        }
        transaction.commit()?;
    }
    Ok(())
}

impl Store {
    pub fn default_path(data_dir: &Path) -> PathBuf {
        data_dir.join(DATABASE_FILE)
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Store> {
        let path = path.as_ref();
        let foreign = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case(FOREIGN_DATABASE_FILE));
        if foreign {
            return Err(StoreError::ForeignDatabase(path.to_path_buf()));
        }
        Store::setup(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Store> {
        Store::setup(Connection::open_in_memory()?)
    }

    fn setup(mut connection: Connection) -> Result<Store> {
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.pragma_update(None, "busy_timeout", 5000)?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&mut connection)?;
        Ok(Store {
            connection: Arc::new(Mutex::new(connection)),
            listeners: Listeners::default(),
        })
    }

    // Calls `listener` after every committed write, on the writing thread, with no store lock held.
    pub fn watch(&self, listener: impl Fn(&Change) + Send + Sync + 'static) {
        let mut listeners = self.listeners.0.lock().unwrap_or_else(|p| p.into_inner());
        listeners.push(Arc::new(listener));
    }

    pub(crate) fn emit(&self, change: impl FnOnce() -> Change) {
        let listeners: Vec<Listener> = self.listeners.0.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if listeners.is_empty() {
            return;
        }
        let change = change();
        for listener in listeners {
            listener(&change);
        }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn transaction<T>(&self, run: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut connection = self.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value = run(&transaction)?;
        transaction.commit()?;
        Ok(value)
    }

    pub fn applied_migrations(&self) -> Result<Vec<String>> {
        let connection = self.lock();
        let mut statement = connection.prepare("SELECT id FROM migration ORDER BY id")?;
        let rows = statement.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn upsert_project(&self, project: &Project) -> Result<()> {
        self.lock().execute(
            "INSERT INTO project (id, worktree, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET worktree = excluded.worktree, time_updated = excluded.time_updated,
               data = excluded.data",
            params![
                project.id,
                project.worktree,
                project.time.created as i64,
                project.time.updated as i64,
                serde_json::to_string(project)?,
            ],
        )?;
        Ok(())
    }

    pub fn get_project(&self, id: &str) -> Result<Option<Project>> {
        let data: Option<String> = self
            .lock()
            .query_row("SELECT data FROM project WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(data.as_deref().map(serde_json::from_str).transpose()?)
    }

    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let connection = self.lock();
        let mut statement =
            connection.prepare("SELECT data FROM project ORDER BY time_updated DESC, id")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    // Cascades to the project's sessions, messages and parts, and drops their event logs.
    pub fn delete_project(&self, id: &str) -> Result<bool> {
        self.transaction(|transaction| {
            let sessions: Vec<String> = transaction
                .prepare("SELECT id FROM session WHERE project_id = ?1")?
                .query_map([id], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            forget(transaction, &sessions)?;
            Ok(transaction.execute("DELETE FROM project WHERE id = ?1", [id])? > 0)
        })
    }

    // Also logs `session.created`, as every session write below logs its durable event.
    pub fn create_session(&self, session: &Session) -> Result<()> {
        self.transaction(|transaction| {
            if !insert_session(transaction, session)? {
                return Err(StoreError::AlreadyExists {
                    kind: "session",
                    id: session.id.clone(),
                });
            }
            log(transaction, &session.id, "session.created.1", serde_json::json!({"sessionID": session.id, "info": session}))
        })?;
        self.emit(|| Change::SessionCreated(session.clone()));
        Ok(())
    }

    // Writes the whole object, including the cost/token totals it carries.
    pub fn update_session(&self, session: &Session) -> Result<()> {
        self.transaction(|transaction| {
            if write_session(transaction, session)? == 0 {
                return Err(not_found("session", &session.id));
            }
            log(transaction, &session.id, "session.updated.1", serde_json::json!({"sessionID": session.id, "info": session}))
        })?;
        self.emit(|| Change::SessionUpdated(session.clone()));
        Ok(())
    }

    // Atomic read-modify-write against the current row; prefer this for setters.
    pub fn patch_session(&self, id: &str, patch: impl FnOnce(&mut Session)) -> Result<Session> {
        self.patch_with(id, patch, true)
    }

    // `patch_session` as a durable v2 event's projection writes the row: no `session.updated` is logged, since the
    // event that caused it is the one in the log.
    pub fn project_session(&self, id: &str, patch: impl FnOnce(&mut Session)) -> Result<Session> {
        self.patch_with(id, patch, false)
    }

    fn patch_with(&self, id: &str, patch: impl FnOnce(&mut Session), logged: bool) -> Result<Session> {
        let session = self.transaction(|transaction| {
            let mut session =
                read_session(transaction, id)?.ok_or_else(|| not_found("session", id))?;
            patch(&mut session);
            session.id = id.to_string();
            write_session(transaction, &session)?;
            if logged {
                log(transaction, id, "session.updated.1", serde_json::json!({"sessionID": id, "info": session}))?;
            }
            Ok(session)
        })?;
        self.emit(|| Change::SessionUpdated(session.clone()));
        Ok(session)
    }

    pub fn get_session(&self, id: &str) -> Result<Option<Session>> {
        read_session(&self.lock(), id)
    }

    // Most recently updated first.
    pub fn list_sessions(&self, project_id: &str) -> Result<Vec<Session>> {
        query_sessions(&self.lock(), "project_id = ?1", project_id)
    }

    pub fn list_children(&self, parent_id: &str) -> Result<Vec<Session>> {
        query_sessions(&self.lock(), "parent_id = ?1", parent_id)
    }

    // Removes the session, its descendants, all their messages and parts, and their event logs.
    pub fn delete_session(&self, id: &str) -> Result<bool> {
        self.transaction(|transaction| {
            let tree: Vec<String> = transaction
                .prepare(
                    "WITH RECURSIVE tree(id) AS (
                       SELECT id FROM session WHERE id = ?1
                       UNION
                       SELECT session.id FROM session JOIN tree ON session.parent_id = tree.id
                     )
                     SELECT id FROM tree",
                )?
                .query_map([id], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            forget(transaction, &tree)?;
            let mut deleted = 0;
            for id in &tree {
                deleted += transaction.execute("DELETE FROM session WHERE id = ?1", [id])?;
            }
            Ok(deleted > 0)
        })
    }

    pub fn upsert_message(&self, message: &Message) -> Result<()> {
        self.transaction(|transaction| {
            write_message(transaction, message)?;
            log(transaction, message.session_id(), "message.updated.1", serde_json::json!({"sessionID": message.session_id(), "info": message}))
        })?;
        self.emit(|| Change::MessageUpdated(message.clone()));
        Ok(())
    }

    pub fn get_message(&self, id: &str) -> Result<Option<(Message, Vec<Part>)>> {
        let connection = self.lock();
        let data: Option<String> = connection
            .query_row("SELECT data FROM message WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        let Some(data) = data else {
            return Ok(None);
        };
        let mut statement =
            connection.prepare("SELECT data FROM part WHERE message_id = ?1 ORDER BY id")?;
        let rows = statement.query_map([id], |row| row.get::<_, String>(0))?;
        let parts = rows
            .map(|row| Ok(serde_json::from_str(&row?)?))
            .collect::<Result<Vec<Part>>>()?;
        Ok(Some((serde_json::from_str(&data)?, parts)))
    }

    // Messages in id order, each with its parts in id order.
    pub fn list_messages(&self, session_id: &str) -> Result<Vec<(Message, Vec<Part>)>> {
        let connection = self.lock();
        let mut parts: HashMap<String, Vec<Part>> = HashMap::new();
        let mut statement = connection.prepare(
            "SELECT part.message_id, part.data FROM part JOIN message ON message.id = part.message_id
             WHERE message.session_id = ?1 ORDER BY part.id",
        )?;
        let rows = statement.query_map([session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (message_id, data) = row?;
            parts
                .entry(message_id)
                .or_default()
                .push(serde_json::from_str(&data)?);
        }
        let mut statement =
            connection.prepare("SELECT id, data FROM message WHERE session_id = ?1 ORDER BY id")?;
        let rows = statement.query_map([session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (id, data) = row?;
            Ok((
                serde_json::from_str(&data)?,
                parts.remove(&id).unwrap_or_default(),
            ))
        })
        .collect()
    }

    pub fn delete_message(&self, id: &str) -> Result<bool> {
        let session_id = self.transaction(|transaction| {
            let session_id = remove_message(transaction, id)?;
            if let Some(session_id) = &session_id {
                log(transaction, session_id, "message.removed.1", serde_json::json!({"sessionID": session_id, "messageID": id}))?;
            }
            Ok(session_id)
        })?;
        let Some(session_id) = session_id else {
            return Ok(false);
        };
        self.emit(|| Change::MessageRemoved { session_id, message_id: id.to_string() });
        Ok(true)
    }

    pub fn upsert_part(&self, part: &Part) -> Result<()> {
        self.transaction(|transaction| {
            let now = now_ms();
            write_part(transaction, part, now)?;
            log(transaction, &part.session_id, "message.part.updated.1", serde_json::json!({"sessionID": part.session_id, "part": part, "time": now}))
        })?;
        self.emit(|| Change::PartUpdated(part.clone()));
        // A step's usage also moved the session totals.
        if part.usage().is_some()
            && let Some(session) = self.get_session(&part.session_id)?
        {
            self.emit(|| Change::SessionUpdated(session));
        }
        Ok(())
    }

    pub fn get_part(&self, id: &str) -> Result<Option<Part>> {
        let data: Option<String> = self
            .lock()
            .query_row("SELECT data FROM part WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(data.as_deref().map(serde_json::from_str).transpose()?)
    }

    pub fn delete_part(&self, id: &str) -> Result<bool> {
        let previous = self.transaction(|transaction| {
            let Some(previous) = remove_part(transaction, id)? else {
                return Ok(None);
            };
            if let Ok(part) = serde_json::from_str::<Part>(&previous) {
                let data = serde_json::json!({"sessionID": part.session_id, "messageID": part.message_id, "partID": part.id});
                log(transaction, &part.session_id, "message.part.removed.1", data)?;
            }
            Ok(Some(previous))
        })?;
        let Some(previous) = previous else {
            return Ok(false);
        };
        if let Ok(part) = serde_json::from_str::<Part>(&previous) {
            self.emit(|| Change::PartRemoved { session_id: part.session_id, message_id: part.message_id, part_id: part.id });
        }
        Ok(true)
    }

    // Replaces the whole list; rows keep their index as `position`.
    pub fn set_todos(&self, session_id: &str, todos: &[Todo]) -> Result<()> {
        self.transaction(|transaction| {
            transaction.execute("DELETE FROM todo WHERE session_id = ?1", [session_id])?;
            let now = now_ms() as i64;
            for (position, todo) in todos.iter().enumerate() {
                transaction.execute(
                    "INSERT INTO todo (session_id, content, status, priority, position, time_created, time_updated)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                    params![session_id, todo.content, todo.status, todo.priority, position as i64, now],
                )?;
            }
            Ok(())
        })
    }

    pub fn get_todos(&self, session_id: &str) -> Result<Vec<Todo>> {
        let connection = self.lock();
        let mut statement =
            connection.prepare("SELECT content, status, priority FROM todo WHERE session_id = ?1 ORDER BY position")?;
        let rows = statement.query_map([session_id], |row| {
            Ok(Todo { content: row.get(0)?, status: row.get(1)?, priority: row.get(2)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}
