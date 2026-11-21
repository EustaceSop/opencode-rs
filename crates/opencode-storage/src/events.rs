// The durable event log (`event`/`event_sequence`, `core/src/event.ts`), workspaces (`control-plane/workspace.sql.ts`)
// and project directories (`project/directories.ts`).
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::id::now_ms;
use crate::store::{Result, Store, StoreError, insert_session, read_session, remove_message, remove_part, write_message, write_part, write_session};
use crate::types::{Message, Part, Session};

// One row of the log. `kind` is the versioned type (`session.created.1`); `seq` counts from 0 per aggregate.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredEvent {
    pub id: String,
    pub aggregate_id: String,
    pub seq: u64,
    pub kind: String,
    pub data: Value,
}

// `SessionV1.Event` definitions with a durable aggregate.
pub const SESSION_V1_DURABLE: [&str; 7] = [
    "session.created.1",
    "session.updated.1",
    "session.deleted.1",
    "message.updated.1",
    "message.removed.1",
    "message.part.updated.1",
    "message.part.removed.1",
];

// `SessionEvent.DurableDefinitions` as versioned types: the `SessionDurable` manifest of `/api/session/{id}/history`.
pub const SESSION_NEXT_DURABLE: [&str; 28] = [
    "session.next.agent.switched.1",
    "session.next.model.switched.1",
    "session.next.moved.1",
    "session.next.prompted.1",
    "session.next.prompt.admitted.1",
    "session.next.context.updated.1",
    "session.next.synthetic.1",
    "session.next.shell.started.1",
    "session.next.shell.ended.1",
    "session.next.step.started.1",
    "session.next.step.ended.2",
    "session.next.step.failed.2",
    "session.next.text.started.1",
    "session.next.text.ended.1",
    "session.next.tool.input.started.1",
    "session.next.tool.input.ended.1",
    "session.next.tool.called.1",
    "session.next.tool.progress.1",
    "session.next.tool.success.1",
    "session.next.tool.failed.1",
    "session.next.reasoning.started.1",
    "session.next.reasoning.ended.1",
    "session.next.retried.1",
    "session.next.compaction.started.1",
    "session.next.compaction.ended.1",
    "session.next.revert.staged.1",
    "session.next.revert.cleared.1",
    "session.next.revert.committed.1",
];

// `EventV2.versionedType`
pub fn versioned_type(kind: &str, version: u32) -> String {
    format!("{kind}.{version}")
}

// The durable version of a `session.next.*` type, or None for the non-durable ones (deltas).
pub fn session_next_version(kind: &str) -> Option<u32> {
    SESSION_NEXT_DURABLE.iter().find_map(|versioned| {
        let (name, version) = versioned.rsplit_once('.')?;
        (name == kind).then(|| version.parse().ok()).flatten()
    })
}

fn invalid(kind: &str, message: String) -> StoreError {
    StoreError::InvalidEvent { kind: kind.to_string(), message }
}

fn stored_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(String, String, i64, String, String)> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
}

fn stored_from(row: (String, String, i64, String, String)) -> Result<StoredEvent> {
    Ok(StoredEvent { id: row.0, aggregate_id: row.1, seq: row.2.max(0) as u64, kind: row.3, data: serde_json::from_str(&row.4)? })
}

fn latest(connection: &Connection, aggregate_id: &str) -> Result<Option<(i64, Option<String>)>> {
    Ok(connection
        .query_row("SELECT seq, owner_id FROM event_sequence WHERE aggregate_id = ?1", [aggregate_id], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?)
}

fn existing(connection: &Connection, id: &str) -> Result<Option<(String, i64)>> {
    Ok(connection.query_row("SELECT aggregate_id, seq FROM event WHERE id = ?1", [id], |row| Ok((row.get(0)?, row.get(1)?))).optional()?)
}

// `owner`: the replaying owner, and whether the aggregate becomes theirs.
fn insert(connection: &Connection, id: &str, aggregate_id: &str, seq: i64, kind: &str, data: &Value, owner: Option<(&str, bool)>) -> Result<()> {
    let (owner, claim) = owner.map_or((None, false), |(owner, claim)| (Some(owner), claim));
    connection.execute(
        "INSERT INTO event_sequence (aggregate_id, seq, owner_id) VALUES (?1, ?2, ?3)
         ON CONFLICT(aggregate_id) DO UPDATE SET seq = excluded.seq,
           owner_id = CASE WHEN ?4 THEN excluded.owner_id ELSE event_sequence.owner_id END",
        params![aggregate_id, seq, owner, claim],
    )?;
    connection.execute(
        "INSERT INTO event (id, aggregate_id, seq, type, data) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, aggregate_id, seq, kind, serde_json::to_string(data)?],
    )?;
    Ok(())
}

// Publishes a durable event: the aggregate's next sequence number, starting at 0.
pub(crate) fn append(connection: &Connection, id: &str, aggregate_id: &str, kind: &str, data: &Value) -> Result<u64> {
    if let Some((aggregate, seq)) = existing(connection, id)? {
        return Err(invalid(kind, format!("Event {id} already exists at aggregate {aggregate} sequence {seq}")));
    }
    let seq = latest(connection, aggregate_id)?.map_or(0, |(seq, _)| seq + 1);
    insert(connection, id, aggregate_id, seq, kind, data, None)?;
    Ok(seq as u64)
}

fn parse<T: serde::de::DeserializeOwned>(kind: &str, value: &Value) -> Result<T> {
    serde_json::from_value(value.clone()).map_err(|e| invalid(kind, format!("Invalid {kind} payload: {e}")))
}

// What a replayed event does to the rows (the original's projectors); v2-only events project into tables this port
// does not keep, apart from the session fields they set.
fn project(connection: &Connection, event: &StoredEvent) -> Result<()> {
    let data = &event.data;
    let kind = event.kind.as_str();
    let session_id = data["sessionID"].as_str().unwrap_or_default();
    match kind {
        "session.created.1" => {
            let session: Session = parse(kind, &data["info"])?;
            if !insert_session(connection, &session)? {
                return Err(invalid(kind, format!("Session {} was already projected", session.id)));
            }
        }
        "session.updated.1" => {
            let mut session: Session = parse(kind, &data["info"])?;
            session.id = session_id.to_string();
            write_session(connection, &session)?;
        }
        "session.deleted.1" => {
            connection.execute("DELETE FROM session WHERE id = ?1", [session_id])?;
        }
        "message.updated.1" => write_message(connection, &parse::<Message>(kind, &data["info"])?)?,
        "message.removed.1" => {
            let message_id = data["messageID"].as_str().unwrap_or_default();
            let owner: Option<String> = connection.query_row("SELECT session_id FROM message WHERE id = ?1", [message_id], |row| row.get(0)).optional()?;
            if owner.as_deref() == Some(session_id) {
                remove_message(connection, message_id)?;
            }
        }
        "message.part.updated.1" => {
            let part: Part = parse(kind, &data["part"])?;
            write_part(connection, &part, data["time"].as_f64().map_or_else(now_ms, |t| t as u64))?;
        }
        "message.part.removed.1" => {
            let part_id = data["partID"].as_str().unwrap_or_default();
            let owner: Option<String> = connection.query_row("SELECT session_id FROM part WHERE id = ?1", [part_id], |row| row.get(0)).optional()?;
            if owner.as_deref() == Some(session_id) {
                remove_part(connection, part_id)?;
            }
        }
        "session.next.agent.switched.1" | "session.next.model.switched.1" | "session.next.moved.1" => {
            let Some(mut session) = read_session(connection, session_id)? else { return Ok(()) };
            match kind {
                "session.next.agent.switched.1" => session.agent = data["agent"].as_str().map(str::to_string),
                "session.next.model.switched.1" => session.model = Some(parse(kind, &data["model"])?),
                _ => {
                    session.directory = data["location"]["directory"].as_str().unwrap_or_default().to_string();
                    session.path = data["subdirectory"].as_str().map(str::to_string);
                    session.workspace_id = data["location"]["workspaceID"].as_str().map(str::to_string);
                }
            }
            if let Some(timestamp) = data["timestamp"].as_f64() {
                session.time.updated = timestamp as u64;
            }
            write_session(connection, &session)?;
        }
        _ => {}
    }
    Ok(())
}

// `commitDurableEvent` with a replay input. Ok(false) when the event is already there or the aggregate is owned by
// someone else.
fn replay_one(connection: &Connection, event: &StoredEvent, owner: Option<&str>, strict_owner: bool) -> Result<bool> {
    let kind = event.kind.as_str();
    if !SESSION_V1_DURABLE.contains(&kind) && !SESSION_NEXT_DURABLE.contains(&kind) {
        return Err(invalid(kind, format!("Unknown durable event type {kind}")));
    }
    let Some(aggregate) = event.data.get("sessionID").and_then(Value::as_str) else {
        return Err(invalid(kind, "Expected string aggregate field sessionID".into()));
    };
    if aggregate != event.aggregate_id {
        return Err(invalid(kind, format!("Aggregate mismatch: expected {}, got {aggregate}", event.aggregate_id)));
    }
    let row = latest(connection, aggregate)?;
    let latest = row.as_ref().map_or(-1, |(seq, _)| *seq);
    let row_owner = row.as_ref().and_then(|(_, owner)| owner.as_deref());
    if strict_owner && let Some(found) = row_owner.filter(|found| Some(*found) != owner) {
        return Err(invalid(kind, format!("Replay owner mismatch for aggregate {aggregate}: expected {found}, got {}", owner.unwrap_or("none"))));
    }
    let seq = event.seq as i64;
    if seq <= latest {
        let stored = connection
            .query_row("SELECT id, aggregate_id, seq, type, data FROM event WHERE aggregate_id = ?1 AND seq = ?2", params![aggregate, seq], stored_row)
            .optional()?
            .map(stored_from)
            .transpose()?;
        if stored.is_some_and(|stored| stored.id == event.id && stored.kind == event.kind && stored.data == event.data) {
            if let Some(owner) = owner.filter(|_| row_owner.is_none()) {
                connection.execute("UPDATE event_sequence SET owner_id = ?2 WHERE aggregate_id = ?1", params![aggregate, owner])?;
            }
            return Ok(false);
        }
        return Err(invalid(kind, format!("Replay diverged at aggregate {aggregate} sequence {seq}")));
    }
    if row_owner.is_some_and(|found| Some(found) != owner) {
        return Ok(false);
    }
    if seq != latest + 1 {
        return Err(invalid(kind, format!("Sequence mismatch for aggregate {aggregate}: expected {}, got {seq}", latest + 1)));
    }
    if let Some((found, at)) = existing(connection, &event.id)? {
        return Err(invalid(kind, format!("Event {} already exists at aggregate {found} sequence {at}", event.id)));
    }
    project(connection, event)?;
    insert(connection, &event.id, aggregate, seq, kind, &event.data, owner.map(|owner| (owner, row_owner.is_none())))?;
    Ok(true)
}

// A workspace row (`WorkspaceTable`), in the original's `Workspace.Info` JSON shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub branch: Option<String>,
    pub directory: Option<String>,
    pub extra: Option<Value>,
    #[serde(rename = "projectID")]
    pub project_id: String,
    #[serde(rename = "timeUsed")]
    pub time_used: u64,
}

type WorkspaceRow = (String, String, String, Option<String>, Option<String>, Option<String>, String, i64);

fn workspace_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkspaceRow> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?))
}

fn workspace_from(row: WorkspaceRow) -> Result<WorkspaceInfo> {
    let extra = row.5.as_deref().map(serde_json::from_str::<Value>).transpose()?.filter(|v| !v.is_null());
    Ok(WorkspaceInfo { id: row.0, kind: row.1, name: row.2, branch: row.3, directory: row.4, extra, project_id: row.6, time_used: row.7.max(0) as u64 })
}

const WORKSPACE_COLUMNS: &str = "id, type, name, branch, directory, extra, project_id, time_used";

// A `project_directory` row as `ProjectDirectories.list` returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectDirectory {
    pub directory: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
}

impl Store {
    // Logs a durable event (a `session.next.*` one, say) under `aggregate_id`; returns its sequence number.
    pub fn append_event(&self, id: &str, aggregate_id: &str, kind: &str, data: &Value) -> Result<u64> {
        self.transaction(|transaction| append(transaction, id, aggregate_id, kind, data))
    }

    // `EventV2.latestSequence`: None for an aggregate without events.
    pub fn latest_sequence(&self, aggregate_id: &str) -> Result<Option<u64>> {
        Ok(latest(&self.lock(), aggregate_id)?.map(|(seq, _)| seq.max(0) as u64))
    }

    // The aggregate's events after `after` (all when None) in sequence order, optionally only the given types.
    pub fn list_events(&self, aggregate_id: &str, after: Option<u64>, kinds: Option<&[&str]>, limit: Option<usize>) -> Result<Vec<StoredEvent>> {
        let connection = self.lock();
        let after = after.map_or(-1, |after| after as i64);
        let mut statement = connection.prepare("SELECT id, aggregate_id, seq, type, data FROM event WHERE aggregate_id = ?1 AND seq > ?2 ORDER BY seq")?;
        let rows = statement.query_map(params![aggregate_id, after], stored_row)?;
        let mut out = Vec::new();
        for row in rows {
            let event = stored_from(row?)?;
            if kinds.is_some_and(|kinds| !kinds.contains(&event.kind.as_str())) {
                continue;
            }
            out.push(event);
            if limit.is_some_and(|limit| out.len() >= limit) {
                break;
            }
        }
        Ok(out)
    }

    // `/sync/history`: every event, minus those at or below the given sequence of each listed aggregate.
    pub fn event_history(&self, known: &[(String, u64)]) -> Result<Vec<StoredEvent>> {
        let connection = self.lock();
        let mut statement = connection.prepare("SELECT id, aggregate_id, seq, type, data FROM event ORDER BY seq, rowid")?;
        let rows = statement.query_map([], stored_row)?;
        let mut out = Vec::new();
        for row in rows {
            let event = stored_from(row?)?;
            if known.iter().any(|(id, seq)| *id == event.aggregate_id && event.seq <= *seq) {
                continue;
            }
            out.push(event);
        }
        Ok(out)
    }

    // `EventV2.replayAll`: one aggregate, consecutive sequence numbers, each event committed with its projection in
    // its own transaction. Returns the aggregate id, None for an empty list. Replays are not reported to `watch`.
    pub fn replay_events(&self, events: &[StoredEvent], owner: Option<&str>, strict_owner: bool) -> Result<Option<String>> {
        let Some(first) = events.first() else { return Ok(None) };
        if events.iter().any(|event| event.aggregate_id != first.aggregate_id) {
            return Err(invalid(&first.kind, "Replay events must belong to the same aggregate".into()));
        }
        for (index, event) in events.iter().enumerate() {
            let seq = first.seq + index as u64;
            if event.seq != seq {
                return Err(invalid(&event.kind, format!("Replay sequence mismatch at index {index}: expected {seq}, got {}", event.seq)));
            }
        }
        for event in events {
            self.replay_event(event, owner, strict_owner)?;
        }
        Ok(Some(first.aggregate_id.clone()))
    }

    // `EventV2.replay` of one event; true when it was committed.
    pub fn replay_event(&self, event: &StoredEvent, owner: Option<&str>, strict_owner: bool) -> Result<bool> {
        self.transaction(|transaction| replay_one(transaction, event, owner, strict_owner))
    }

    // `EventV2.remove`
    pub fn remove_events(&self, aggregate_id: &str) -> Result<()> {
        self.lock().execute("DELETE FROM event_sequence WHERE aggregate_id = ?1", [aggregate_id])?;
        Ok(())
    }

    // `EventV2.claim`: replays from any other owner are ignored from now on.
    pub fn claim_events(&self, aggregate_id: &str, owner_id: &str) -> Result<()> {
        self.lock().execute("UPDATE event_sequence SET owner_id = ?2 WHERE aggregate_id = ?1", params![aggregate_id, owner_id])?;
        Ok(())
    }

    pub fn insert_workspace(&self, workspace: &WorkspaceInfo) -> Result<()> {
        let extra = workspace.extra.as_ref().map(serde_json::to_string).transpose()?;
        self.lock().execute(
            &format!("INSERT INTO workspace ({WORKSPACE_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"),
            params![workspace.id, workspace.kind, workspace.name, workspace.branch, workspace.directory, extra, workspace.project_id, workspace.time_used as i64],
        )?;
        Ok(())
    }

    pub fn get_workspace(&self, id: &str) -> Result<Option<WorkspaceInfo>> {
        let connection = self.lock();
        let row = connection.query_row(&format!("SELECT {WORKSPACE_COLUMNS} FROM workspace WHERE id = ?1"), [id], workspace_row).optional()?;
        row.map(workspace_from).transpose()
    }

    // A project's workspaces in id order.
    pub fn list_workspaces(&self, project_id: &str) -> Result<Vec<WorkspaceInfo>> {
        let connection = self.lock();
        let mut statement = connection.prepare(&format!("SELECT {WORKSPACE_COLUMNS} FROM workspace WHERE project_id = ?1 ORDER BY id"))?;
        let rows = statement.query_map([project_id], workspace_row)?;
        rows.map(|row| workspace_from(row?)).collect()
    }

    pub fn delete_workspace(&self, id: &str) -> Result<bool> {
        Ok(self.lock().execute("DELETE FROM workspace WHERE id = ?1", [id])? > 0)
    }

    // Sessions whose `workspaceID` is `workspace_id`, most recently updated first.
    pub fn list_workspace_sessions(&self, workspace_id: &str) -> Result<Vec<Session>> {
        let ids: Vec<String> = {
            let connection = self.lock();
            let mut statement = connection.prepare("SELECT id FROM session WHERE json_extract(data, '$.workspaceID') = ?1 ORDER BY time_updated DESC, id DESC")?;
            statement.query_map([workspace_id], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        let mut out = Vec::new();
        for id in ids {
            out.extend(self.get_session(&id)?);
        }
        Ok(out)
    }

    // `ProjectDirectories.create`: true when a row was added, or (with `replace`) its strategy changed.
    pub fn add_project_directory(&self, project_id: &str, directory: &str, strategy: Option<&str>, replace: bool) -> Result<bool> {
        let insert = "INSERT INTO project_directory (project_id, directory, strategy, time_created) VALUES (?1, ?2, ?3, ?4)";
        let sql = match (replace, strategy) {
            (false, _) => format!("{insert} ON CONFLICT(project_id, directory) DO NOTHING"),
            (true, Some(_)) => format!(
                "{insert} ON CONFLICT(project_id, directory) DO UPDATE SET strategy = excluded.strategy
                 WHERE project_directory.strategy IS NULL OR project_directory.strategy != excluded.strategy"
            ),
            (true, None) => format!(
                "{insert} ON CONFLICT(project_id, directory) DO UPDATE SET strategy = NULL WHERE project_directory.strategy IS NOT NULL"
            ),
        };
        Ok(self.lock().execute(&sql, params![project_id, directory, strategy, now_ms() as i64])? > 0)
    }

    pub fn remove_project_directory(&self, project_id: &str, directory: &str) -> Result<bool> {
        Ok(self.lock().execute("DELETE FROM project_directory WHERE project_id = ?1 AND directory = ?2", params![project_id, directory])? > 0)
    }

    pub fn get_project_directory(&self, project_id: &str, directory: &str) -> Result<Option<ProjectDirectory>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT directory, strategy FROM project_directory WHERE project_id = ?1 AND directory = ?2",
                params![project_id, directory],
                |row| Ok(ProjectDirectory { directory: row.get(0)?, strategy: row.get(1)? }),
            )
            .optional()?)
    }

    // Newest first, then by directory.
    pub fn list_project_directories(&self, project_id: &str) -> Result<Vec<ProjectDirectory>> {
        let connection = self.lock();
        let mut statement = connection.prepare("SELECT directory, strategy FROM project_directory WHERE project_id = ?1 ORDER BY time_created DESC, directory ASC")?;
        let rows = statement.query_map([project_id], |row| Ok(ProjectDirectory { directory: row.get(0)?, strategy: row.get(1)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

impl StoredEvent {
    // The `/sync/replay` input shape (`EventV2.SerializedEvent`).
    pub fn to_serialized(&self) -> Value {
        json!({"id": self.id, "aggregateID": self.aggregate_id, "seq": self.seq, "type": self.kind, "data": self.data})
    }

    // A `/sync/history` row (`EventTable` columns).
    pub fn to_row(&self) -> Value {
        json!({"id": self.id, "aggregate_id": self.aggregate_id, "seq": self.seq, "type": self.kind, "data": self.data})
    }
}
