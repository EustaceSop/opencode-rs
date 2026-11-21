// Rust build: read-only view of the logins opencode 2.x keeps in the `credential` table of `<data>/opencode.db`
// (`packages/core/src/credential/sql.ts`). 2.x copies `auth.json` into that table once
// (`20260805200742_import_legacy_credentials`) and never reads the file again, so a login made in 2.x exists only
// there. The database is opened read-only; a missing, locked or foreign file reads as no credentials.
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

pub const DATABASE_FILE: &str = "opencode.db";

// `Credential.Service` selects the active row, else the newest (`currentActive`, and `list().at(-1)` for MCP).
const ORDER: &str = "active DESC, time_created DESC, id DESC";

fn uri(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let mut encoded = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '%' => encoded.push_str("%25"),
            '?' => encoded.push_str("%3f"),
            '#' => encoded.push_str("%23"),
            ' ' => encoded.push_str("%20"),
            other => encoded.push(other),
        }
    }
    let prefix = if encoded.starts_with('/') { "file://" } else { "file:///" };
    format!("{prefix}{encoded}?mode=ro")
}

fn open(path: &Path) -> Option<Connection> {
    if !path.is_file() {
        return None;
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = Connection::open_with_flags(uri(path), flags).ok()?;
    connection.pragma_update(None, "query_only", true).ok()?;
    connection.busy_timeout(Duration::from_millis(250)).ok()?;
    Some(connection)
}

// The selected credential value of every integration, by integration id.
pub fn active(path: &Path) -> Vec<(String, Value)> {
    let Some(connection) = open(path) else { return Vec::new() };
    let sql = format!("SELECT integration_id, value FROM credential WHERE integration_id IS NOT NULL ORDER BY integration_id, {ORDER}");
    let Ok(mut statement) = connection.prepare(&sql) else { return Vec::new() };
    let Ok(rows) = statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) else { return Vec::new() };
    let mut out = Vec::new();
    let mut last: Option<String> = None;
    for (id, value) in rows.flatten() {
        if last.as_deref() == Some(id.as_str()) {
            continue;
        }
        if let Ok(value) = serde_json::from_str(&value) {
            out.push((id.clone(), value));
        }
        last = Some(id);
    }
    out
}

// The selected credential value of one integration.
pub fn active_for(path: &Path, integration_id: &str) -> Option<Value> {
    let connection = open(path)?;
    let sql = format!("SELECT value FROM credential WHERE integration_id = ?1 ORDER BY {ORDER} LIMIT 1");
    let text: String = connection.query_row(&sql, [integration_id], |r| r.get(0)).optional().ok()??;
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // `credential` as created by migration `20260611192811_lush_chimera` (unchanged through v2.2.7).
    pub(crate) const SCHEMA: &str = "CREATE TABLE `credential` (
      `id` text PRIMARY KEY, `integration_id` text, `label` text NOT NULL, `value` text NOT NULL, `connector_id` text,
      `method_id` text, `active` integer, `time_created` integer NOT NULL, `time_updated` integer NOT NULL);";

    pub(crate) fn insert(db: &Connection, id: &str, integration: Option<&str>, value: &str, active: Option<bool>, created: i64) {
        db.execute(
            "INSERT INTO credential (id, integration_id, label, value, active, time_created, time_updated) VALUES (?1, ?2, 'x', ?3, ?4, ?5, ?5)",
            rusqlite::params![id, integration, value, active, created],
        )
        .unwrap();
    }

    #[test]
    fn selects_active_then_newest_and_tolerates_foreign_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(DATABASE_FILE);
        assert!(active(&file).is_empty());
        std::fs::write(&file, "{}").unwrap();
        assert!(active(&file).is_empty() && active_for(&file, "openai").is_none());
        assert_eq!(std::fs::read(&file).unwrap(), b"{}");
        std::fs::remove_file(&file).unwrap();
        {
            let db = Connection::open(&file).unwrap();
            db.execute_batch("CREATE TABLE kv (key text PRIMARY KEY)").unwrap();
        }
        assert!(active(&file).is_empty());
        {
            let db = Connection::open(&file).unwrap();
            db.execute_batch(SCHEMA).unwrap();
            // Legacy imports have no active flag: the newest wins until one row is pinned.
            insert(&db, "cred_1", Some("anthropic"), r#"{"type":"key","key":"old"}"#, None, 1);
            insert(&db, "cred_2", Some("anthropic"), r#"{"type":"key","key":"new"}"#, None, 2);
            insert(&db, "cred_3", Some("openai"), r#"{"type":"key","key":"pinned"}"#, Some(true), 1);
            insert(&db, "cred_4", Some("openai"), r#"{"type":"key","key":"later"}"#, Some(false), 5);
            insert(&db, "cred_5", None, r#"{"type":"key","key":"orphan"}"#, Some(true), 9);
            insert(&db, "cred_6", Some("broken"), "not json", None, 1);
        }
        let found = active(&file);
        let keys: Vec<(&str, &str)> = found.iter().map(|(id, v)| (id.as_str(), v["key"].as_str().unwrap())).collect();
        assert_eq!(keys, [("anthropic", "new"), ("openai", "pinned")]);
        assert_eq!(active_for(&file, "openai").unwrap()["key"], "pinned");
        assert!(active_for(&file, "nobody").is_none());
        assert!(!dir.path().join("opencode.db-wal").exists() && !dir.path().join("opencode.db-journal").exists());
    }
}
