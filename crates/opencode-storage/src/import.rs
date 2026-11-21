// Bulk writes of history brought over from other tools (`opencode-import`).
use rusqlite::{OptionalExtension, Transaction, params};

use crate::store::{Change, Result, Store, insert_session, read_session, write_message, write_part, write_session};
use crate::types::{Message, Part, Session, SessionTokens, Todo};

// What one `import_session` call changed; rows whose stored JSON already matched are not counted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportWrites {
    pub created: bool,
    pub session: bool,
    pub messages: usize,
    pub parts: usize,
}

impl ImportWrites {
    pub fn changed(&self) -> bool {
        self.created || self.session || self.messages > 0 || self.parts > 0
    }
}

fn stored(transaction: &Transaction<'_>, table: &str, id: &str) -> Result<Option<String>> {
    Ok(transaction.query_row(&format!("SELECT data FROM {table} WHERE id = ?1"), [id], |row| row.get(0)).optional()?)
}

fn same_todos(transaction: &Transaction<'_>, session_id: &str, todos: &[Todo]) -> Result<bool> {
    let mut statement = transaction.prepare("SELECT content, status, priority FROM todo WHERE session_id = ?1 ORDER BY position")?;
    let rows = statement.query_map([session_id], |row| Ok(Todo { content: row.get(0)?, status: row.get(1)?, priority: row.get(2)? }))?;
    let current: Vec<Todo> = rows.collect::<rusqlite::Result<_>>()?;
    Ok(current == todos)
}

impl Store {
    // Upserts a session with its messages, parts and todos in one transaction. The row's cost/token totals follow the
    // step-finish parts written (the session's own totals are ignored), as for live sessions. Messages and parts are
    // matched by id and never removed. Imported history is not added to the durable event log.
    pub fn import_session(&self, session: &Session, messages: &[(Message, Vec<Part>)], todos: Option<&[Todo]>) -> Result<ImportWrites> {
        let mut writes = ImportWrites::default();
        let row = self.transaction(|transaction| {
            let existing = read_session(transaction, &session.id)?;
            let mut row = session.clone();
            match &existing {
                Some(current) => {
                    row.cost = current.cost;
                    row.tokens = current.tokens;
                }
                None => {
                    row.cost = Some(0.0);
                    row.tokens = Some(SessionTokens::default());
                    insert_session(transaction, &row)?;
                    writes.created = true;
                }
            }
            for (message, parts) in messages {
                let data = serde_json::to_string(message)?;
                if stored(transaction, "message", message.id())?.as_deref() != Some(data.as_str()) {
                    write_message(transaction, message)?;
                    writes.messages += 1;
                }
                for part in parts {
                    let data = serde_json::to_string(part)?;
                    if stored(transaction, "part", &part.id)?.as_deref() != Some(data.as_str()) {
                        write_part(transaction, part, message.time_created())?;
                        writes.parts += 1;
                    }
                }
            }
            if let Some(todos) = todos
                && !same_todos(transaction, &session.id, todos)?
            {
                transaction.execute("DELETE FROM todo WHERE session_id = ?1", [&session.id])?;
                for (position, todo) in todos.iter().enumerate() {
                    transaction.execute(
                        "INSERT INTO todo (session_id, content, status, priority, position, time_created, time_updated)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                        params![session.id, todo.content, todo.status, todo.priority, position as i64, session.time.updated as i64],
                    )?;
                }
            }
            // The parts above moved the totals; the row is rewritten with them.
            let totals = read_session(transaction, &session.id)?.unwrap_or_else(|| row.clone());
            row.cost = totals.cost;
            row.tokens = totals.tokens;
            if existing.as_ref() != Some(&row) {
                write_session(transaction, &row)?;
                writes.session = !writes.created;
            }
            Ok(row)
        })?;
        if writes.created {
            self.emit(|| Change::SessionCreated(row));
        } else if writes.changed() {
            self.emit(|| Change::SessionUpdated(row));
        }
        Ok(writes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;

    fn session(id: &str) -> Session {
        let mut session = Session::new(NewSession { id: Some(id.into()), project_id: GLOBAL_PROJECT_ID.into(), directory: "/w".into(), version: "t".into(), ..Default::default() }).unwrap();
        session.cost = Some(99.0);
        session
    }

    fn messages(session_id: &str, cost: f64) -> Vec<(Message, Vec<Part>)> {
        let user = Message::User(UserMessage {
            id: "msg_1".into(),
            session_id: session_id.into(),
            time: UserTime { created: 1 },
            format: None,
            summary: None,
            agent: "build".into(),
            model: UserModel { provider_id: "p".into(), model_id: "m".into(), variant: None },
            system: None,
            tools: None,
        });
        let text = Part { id: "prt_1".into(), session_id: session_id.into(), message_id: "msg_1".into(), body: PartBody::Text(TextPart { text: "hi".into(), ..Default::default() }) };
        let tokens = Tokens { total: None, input: 10.0, output: 2.0, reasoning: 0.0, cache: TokenCache { read: 1.0, write: 0.0 } };
        let step = Part { id: "prt_2".into(), session_id: session_id.into(), message_id: "msg_1".into(), body: PartBody::StepFinish(StepFinishPart { reason: "stop".into(), snapshot: None, cost, tokens }) };
        vec![(user, vec![text, step])]
    }

    #[test]
    fn import_upserts_once_and_totals_follow_step_parts() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_project(&Project { id: GLOBAL_PROJECT_ID.into(), worktree: "/".into(), vcs: None, name: None, icon: None, commands: None, time: ProjectTime { created: 0, updated: 0, initialized: None }, sandboxes: vec![] }).unwrap();
        let row = session("ses_import");
        let todos = vec![Todo { content: "a".into(), status: "pending".into(), priority: "high".into() }];
        let first = store.import_session(&row, &messages(&row.id, 0.5), Some(&todos)).unwrap();
        assert_eq!(first, ImportWrites { created: true, session: false, messages: 1, parts: 2 });
        let stored = store.get_session(&row.id).unwrap().unwrap();
        assert_eq!((stored.cost, stored.tokens.unwrap().input), (Some(0.5), 10.0));
        assert_eq!(store.get_todos(&row.id).unwrap(), todos);
        assert!(store.list_events(&row.id, None, None, None).unwrap().is_empty());

        let again = store.import_session(&row, &messages(&row.id, 0.5), Some(&todos)).unwrap();
        assert!(!again.changed());
        assert_eq!(store.get_session(&row.id).unwrap().unwrap().cost, Some(0.5));

        let mut renamed = row.clone();
        renamed.title = "renamed".into();
        let changed = store.import_session(&renamed, &messages(&row.id, 0.75), None).unwrap();
        assert_eq!(changed, ImportWrites { created: false, session: true, messages: 0, parts: 1 });
        let stored = store.get_session(&row.id).unwrap().unwrap();
        assert_eq!((stored.title.as_str(), stored.cost), ("renamed", Some(0.75)));
        assert_eq!(store.list_messages(&row.id).unwrap()[0].1.len(), 2);
    }
}
