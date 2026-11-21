// `opencode db [query]` / `opencode db path` (`opencode/src/cli/cmd/db.ts`).
use crate::ui::fail;
use anyhow::Result;
use opencode_core::paths::Paths;
use opencode_storage::Store;
use rusqlite::types::ValueRef;
use serde_json::{Map, Value};
use std::path::PathBuf;

pub fn path(paths: &Paths) -> PathBuf {
    Store::default_path(&paths.data)
}

fn json(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(n) => n.into(),
        // JavaScript has one number type: a whole REAL prints without a fraction.
        ValueRef::Real(n) if n.fract() == 0.0 && n.abs() < 9.0e15 => (n as i64).into(),
        ValueRef::Real(n) => serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number),
        ValueRef::Text(text) => String::from_utf8_lossy(text).into_owned().into(),
        ValueRef::Blob(bytes) => bytes.iter().map(|b| Value::from(*b)).collect::<Vec<_>>().into(),
    }
}

// How `Array.prototype.join` renders a cell: null is empty, everything else its string form.
fn cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Array(items) => items.iter().map(cell).collect::<Vec<_>>().join(","),
        other => other.to_string(),
    }
}

pub fn query(paths: &Paths, sql: &str, format: &str) -> Result<i32> {
    // Opening the store first creates and migrates the database, as the original's `Database.Service` does.
    drop(Store::open(path(paths))?);
    let connection = rusqlite::Connection::open(path(paths))?;
    let mut statement = connection.prepare(sql).map_err(|e| fail(e.to_string()))?;
    let names: Vec<String> = statement.column_names().map(str::to_string).collect();
    let mut rows = statement.query([]).map_err(|e| fail(e.to_string()))?;
    let mut out: Vec<Map<String, Value>> = Vec::new();
    while let Some(row) = rows.next().map_err(|e| fail(e.to_string()))? {
        let mut object = Map::new();
        for (index, name) in names.iter().enumerate() {
            object.insert(name.clone(), json(row.get_ref(index)?));
        }
        out.push(object);
    }
    if format == "json" {
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else if let Some(first) = out.first() {
        let keys: Vec<&String> = first.keys().collect();
        println!("{}", keys.iter().map(|key| key.as_str()).collect::<Vec<_>>().join("\t"));
        for row in &out {
            println!("{}", keys.iter().map(|key| row.get(*key).map(cell).unwrap_or_default()).collect::<Vec<_>>().join("\t"));
        }
    }
    Ok(0)
}

pub fn shell(paths: &Paths) -> Result<i32> {
    std::process::Command::new("sqlite3").arg(path(paths)).status().map_err(|e| fail(format!("Failed to start sqlite3: {e}")))?;
    Ok(0)
}
