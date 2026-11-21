// The TypeScript opencode's database (`<data>/opencode.db`, opened read-only). Legacy `session`/`message`/`part` rows
// keep their ids; v2 `session_v2`/`session_message` rows are rebuilt in `seq` order, with assistant content folded in
// from `session_part` and media from `blob` the way `SessionHistory.withParts` and `SessionBlob.hydrate` read them.
// Its config dir is shared already.
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use opencode_core::paths::Paths;
use opencode_storage::{
    AgentPart, FilePart, JsonMap, Message, Part, PartBody, PermissionRule, Project, ProjectCommands, ProjectIcon,
    ProjectTime, Session, SessionModel, SessionRevert, SessionShare, SessionSummary, SessionTime, Todo, ToolPart,
    ToolState, ToolStateCompleted, ToolStateError, ToolTimeCompleted, ToolTimeError, Vcs,
};
use rusqlite::types::Value as Sql;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{Map, Value, json};

use super::{Converted, SessionKind, SessionSource, session_item};
use crate::convert::{self, Model, Transcript};
use crate::{Ctx, SourceReport};

const ID: &str = "opencode";

pub fn databases(paths: &Paths, home: &Path) -> Vec<PathBuf> {
    let own = opencode_storage::Store::default_path(&paths.data);
    let mut dirs = vec![paths.data.clone(), home.join(".local").join("share").join("opencode")];
    dirs.dedup();
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                let name = p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
                let channel = name.starts_with("opencode-") && name.ends_with(".db");
                p.is_file() && (name == "opencode.db" || channel) && name != opencode_storage::DATABASE_FILE && *p != own
            })
            .collect();
        found.sort();
        for file in found {
            if !out.contains(&file) {
                out.push(file);
            }
        }
    }
    out
}

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

// Read-only: `mode=ro` URI, read-only open flags and `query_only`.
pub fn open(path: &Path) -> Result<Connection, String> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = Connection::open_with_flags(uri(path), flags).map_err(|e| e.to_string())?;
    connection.pragma_update(None, "query_only", true).map_err(|e| e.to_string())?;
    connection.busy_timeout(std::time::Duration::from_secs(5)).map_err(|e| e.to_string())?;
    Ok(connection)
}

#[derive(Default)]
pub struct Databases {
    open: HashMap<PathBuf, Connection>,
}

impl Databases {
    pub fn open(&mut self, path: &Path) -> Result<&Connection, String> {
        if !self.open.contains_key(path) {
            self.open.insert(path.to_path_buf(), open(path)?);
        }
        Ok(&self.open[path])
    }
}

fn has_table(connection: &Connection, name: &str) -> bool {
    connection.query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1", [name], |_| Ok(())).optional().ok().flatten().is_some()
}

struct Listed {
    db: PathBuf,
    v2: bool,
    updated: u64,
    title: String,
    directory: String,
}

fn list(connection: &Connection, table: &str) -> rusqlite::Result<Vec<(String, u64, String, String)>> {
    let mut statement = connection.prepare(&format!("SELECT id, time_updated, coalesce(title, ''), coalesce(directory, '') FROM {table}"))?;
    let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?.max(0) as u64, row.get(2)?, row.get(3)?)))?;
    rows.collect()
}

fn shared(ctx: &Ctx, report: &mut SourceReport) {
    let config = &ctx.paths.config;
    let mut mcp = 0;
    for name in ["config.json", "opencode.json", "opencode.jsonc"] {
        let text = std::fs::read_to_string(config.join(name)).unwrap_or_default();
        if let Ok(value) = opencode_core::config::parse_jsonc(&text, name) {
            mcp += value.get("mcp").and_then(Value::as_object).map_or(0, Map::len);
        }
    }
    let agents = if config.is_dir() { opencode_core::glob::scan("{agent,agents}/**/*.md", config, opencode_core::glob::Options { dot: true, follow: true }).len() } else { 0 };
    let skills = crate::skills::count(&config.join("skill")) + crate::skills::count(&config.join("skills"));
    if config.is_dir() {
        report.shared.push(format!("{mcp} MCP servers, {agents} agents and {skills} skills in {} (the Rust build reads this config dir already)", config.display()));
    }
}

pub fn scan(ctx: &Ctx) -> SourceReport {
    let mut report = SourceReport::new(ID);
    let mut sessions: HashMap<String, Listed> = HashMap::new();
    for db in databases(ctx.paths, ctx.home) {
        report.locate(&db);
        let connection = match open(&db) {
            Ok(connection) => connection,
            Err(error) => {
                report.warnings.push(format!("{}: {error}", db.display()));
                continue;
            }
        };
        for (table, v2) in [("session", false), ("session_v2", true)] {
            if !has_table(&connection, table) {
                continue;
            }
            match list(&connection, table) {
                Ok(rows) => {
                    for (id, updated, title, directory) in rows {
                        let newer = sessions.get(&id).is_none_or(|known| updated > known.updated);
                        if newer {
                            sessions.insert(id, Listed { db: db.clone(), v2, updated, title, directory });
                        }
                    }
                }
                Err(error) => report.warnings.push(format!("{}: {table}: {error}", db.display())),
            }
        }
    }
    let mut listed: Vec<(String, Listed)> = sessions.into_iter().collect();
    listed.sort_by(|a, b| b.1.updated.cmp(&a.1.updated).then_with(|| a.0.cmp(&b.0)));
    for (id, entry) in listed {
        let source = SessionSource { key: id, updated: entry.updated, kind: SessionKind::Opencode { db: entry.db, v2: entry.v2 } };
        report.items.push(session_item(entry.title, entry.directory, source));
    }
    shared(ctx, &mut report);
    report
}

type Row = HashMap<String, Sql>;

fn row(connection: &Connection, sql: &str, id: &str) -> Result<Option<Row>, String> {
    let mut statement = connection.prepare(sql).map_err(|e| e.to_string())?;
    let names: Vec<String> = statement.column_names().map(str::to_string).collect();
    statement
        .query_row([id], |r| (0..names.len()).map(|i| Ok((names[i].clone(), r.get::<_, Sql>(i)?))).collect::<rusqlite::Result<Row>>())
        .optional()
        .map_err(|e| e.to_string())
}

fn text(row: &Row, key: &str) -> Option<String> {
    match row.get(key) {
        Some(Sql::Text(s)) => Some(s.clone()),
        _ => None,
    }
}

fn int(row: &Row, key: &str) -> Option<u64> {
    match row.get(key) {
        Some(Sql::Integer(i)) => Some((*i).max(0) as u64),
        Some(Sql::Real(f)) => Some(f.max(0.0) as u64),
        _ => None,
    }
}

fn float(row: &Row, key: &str) -> Option<f64> {
    match row.get(key) {
        Some(Sql::Integer(i)) => Some(*i as f64),
        Some(Sql::Real(f)) => Some(*f),
        _ => None,
    }
}

fn json_col<T: serde::de::DeserializeOwned>(row: &Row, key: &str) -> Option<T> {
    text(row, key).and_then(|t| serde_json::from_str(&t).ok())
}

fn session_from(row: &Row) -> Session {
    let summary = int(row, "summary_additions").map(|additions| SessionSummary {
        additions: additions as f64,
        deletions: int(row, "summary_deletions").unwrap_or(0) as f64,
        files: int(row, "summary_files").unwrap_or(0) as f64,
        diffs: json_col(row, "summary_diffs"),
    });
    let model = json_col::<Value>(row, "model").and_then(|m| {
        Some(SessionModel { id: m.get("id")?.as_str()?.to_string(), provider_id: m.get("providerID")?.as_str()?.to_string(), variant: m.get("variant").and_then(Value::as_str).map(str::to_string) })
    });
    let created = int(row, "time_created").unwrap_or(0);
    Session {
        id: text(row, "id").unwrap_or_default(),
        slug: text(row, "slug").unwrap_or_else(opencode_storage::slug),
        project_id: text(row, "project_id").unwrap_or_else(|| opencode_storage::GLOBAL_PROJECT_ID.into()),
        workspace_id: None,
        directory: text(row, "directory").unwrap_or_default(),
        path: text(row, "path"),
        parent_id: text(row, "parent_id"),
        summary,
        cost: Some(0.0),
        tokens: None,
        share: text(row, "share_url").map(|url| SessionShare { url }),
        title: text(row, "title").unwrap_or_default(),
        agent: text(row, "agent"),
        model,
        version: text(row, "version").unwrap_or_default(),
        metadata: json_col(row, "metadata"),
        time: SessionTime { created, updated: int(row, "time_updated").unwrap_or(created), compacting: None, archived: float(row, "time_archived") },
        permission: json_col::<Vec<PermissionRule>>(row, "permission"),
        revert: json_col::<SessionRevert>(row, "revert"),
    }
}

fn project_from(row: &Row) -> Project {
    let icon = ProjectIcon { url: text(row, "icon_url"), r#override: text(row, "icon_url_override"), color: text(row, "icon_color") };
    let has_icon = icon != ProjectIcon::default();
    let created = int(row, "time_created").unwrap_or(0);
    Project {
        id: text(row, "id").unwrap_or_default(),
        worktree: text(row, "worktree").unwrap_or_else(|| "/".into()),
        vcs: (text(row, "vcs").as_deref() == Some("git")).then_some(Vcs::Git),
        name: text(row, "name"),
        icon: has_icon.then_some(icon),
        commands: json_col::<ProjectCommands>(row, "commands"),
        time: ProjectTime { created, updated: int(row, "time_updated").unwrap_or(created), initialized: int(row, "time_initialized") },
        sandboxes: json_col(row, "sandboxes").unwrap_or_default(),
    }
}

fn with_ids(data: &str, ids: &[(&str, &str)]) -> Result<Value, String> {
    let mut value: Value = serde_json::from_str(data).map_err(|e| e.to_string())?;
    let object = value.as_object_mut().ok_or("not an object")?;
    for (key, id) in ids {
        object.insert((*key).to_string(), Value::String((*id).to_string()));
    }
    Ok(value)
}

fn rows3(connection: &Connection, sql: &str, id: &str) -> Result<Vec<(String, String, String)>, String> {
    let mut statement = connection.prepare(sql).map_err(|e| e.to_string())?;
    let rows = statement.query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).map_err(|e| e.to_string())?;
    rows.collect::<rusqlite::Result<_>>().map_err(|e| e.to_string())
}

type Messages = Vec<(Message, Vec<Part>)>;

fn legacy_messages(connection: &Connection, session_id: &str) -> Result<(Messages, usize), String> {
    let mut skipped = 0;
    let mut parts: HashMap<String, Vec<Part>> = HashMap::new();
    for (id, message_id, data) in rows3(connection, "SELECT id, message_id, data FROM part WHERE session_id = ?1 ORDER BY id", session_id)? {
        match with_ids(&data, &[("id", &id), ("sessionID", session_id), ("messageID", &message_id)]).and_then(|v| serde_json::from_value::<Part>(v).map_err(|e| e.to_string())) {
            Ok(part) => parts.entry(message_id).or_default().push(part),
            Err(_) => skipped += 1,
        }
    }
    let mut out = Vec::new();
    for (id, _, data) in rows3(connection, "SELECT id, session_id, data FROM message WHERE session_id = ?1 ORDER BY id", session_id)? {
        match with_ids(&data, &[("id", &id), ("sessionID", session_id)]).and_then(|v| serde_json::from_value::<Message>(v).map_err(|e| e.to_string())) {
            Ok(message) => out.push((message, parts.remove(&id).unwrap_or_default())),
            Err(_) => skipped += 1,
        }
    }
    Ok((out, skipped))
}

fn todos(connection: &Connection, session_id: &str) -> Option<Vec<Todo>> {
    if !has_table(connection, "todo") {
        return None;
    }
    let mut statement = connection.prepare("SELECT content, status, priority FROM todo WHERE session_id = ?1 ORDER BY position").ok()?;
    let rows = statement.query_map([session_id], |r| Ok(Todo { content: r.get(0)?, status: r.get(1)?, priority: r.get(2)? })).ok()?;
    let todos: Vec<Todo> = rows.flatten().collect();
    (!todos.is_empty()).then_some(todos)
}

fn number(value: &Value, pointer: &str) -> f64 {
    value.pointer(pointer).and_then(Value::as_f64).unwrap_or(0.0)
}

fn ms(value: &Value, pointer: &str) -> Option<u64> {
    value.pointer(pointer).and_then(Value::as_u64)
}

fn model_of(value: &Value) -> Option<Model> {
    let id = value.get("id").or_else(|| value.get("modelID"))?.as_str()?;
    let provider = value.get("providerID")?.as_str()?;
    Some(Model { provider: provider.into(), id: id.into(), variant: value.get("variant").and_then(Value::as_str).map(str::to_string) })
}

fn data_url(file: &Value) -> Option<String> {
    let mime = file.get("mime").and_then(Value::as_str).unwrap_or("application/octet-stream");
    if let Some(uri) = file.get("uri").and_then(Value::as_str) {
        return Some(uri.to_string());
    }
    let data = file.get("data")?.as_str()?;
    Some(if data.starts_with("data:") { data.to_string() } else { format!("data:{mime};base64,{}", data.trim()) })
}

// v2 renamed these tools and their arguments (`RENAMED_TOOLS` and `legacyToolNotice` in
// `database/v1-migration.bun.ts`); the import maps them back so the transcript reads like a v1 one.
fn v1_tool(name: &str) -> &str {
    match name {
        "shell" => "bash",
        "subagent" => "task",
        "patch" => "apply_patch",
        other => other,
    }
}

fn v1_input(tool: &str, mut input: JsonMap) -> JsonMap {
    let renames: &[(&str, &str)] = match tool {
        "read" | "edit" | "write" => &[("path", "filePath")],
        "task" => &[("agent", "subagent_type"), ("sessionID", "task_id")],
        _ => &[],
    };
    for (from, to) in renames {
        if !input.contains_key(*to)
            && let Some(value) = input.shift_remove(*from)
        {
            input.insert((*to).to_string(), value);
        }
    }
    input
}

// `SessionBlob`: a stored file may name a `blob` row as `media:<sha256>` instead of carrying its bytes. `None` when
// the value is not a ref (or the database has no `blob` table), `Some(None)` when its blob is gone.
fn blob(blobs: Option<&Connection>, uri: &str) -> Option<Option<String>> {
    let hash = uri.strip_prefix("media:")?;
    let bytes = blobs?.query_row("SELECT data FROM blob WHERE hash = ?1", [hash], |r| r.get::<_, Vec<u8>>(0)).optional().ok().flatten();
    Some(bytes.map(|b| STANDARD.encode(b)))
}

// The text a ref whose blob is gone reads as (`SessionBlob.unavailable`).
fn unavailable(file: &Value) -> String {
    let mime = file["mime"].as_str().unwrap_or("application/octet-stream");
    match file["name"].as_str() {
        Some(name) => format!("[media unavailable: {name} ({mime})]"),
        None => format!("[media unavailable: {mime}]"),
    }
}

// `inlineFile`: a user file ref becomes inline base64 data, or a text/plain placeholder when its blob is gone.
fn hydrate_file(blobs: Option<&Connection>, file: &Value) -> Value {
    let Some(found) = file["uri"].as_str().and_then(|uri| blob(blobs, uri)) else { return file.clone() };
    let mut out = file.clone();
    if let Some(map) = out.as_object_mut() {
        map.shift_remove("uri");
        match found {
            Some(data) => {
                map.insert("data".into(), data.into());
            }
            None => {
                map.insert("data".into(), STANDARD.encode(unavailable(file)).into());
                map.insert("mime".into(), "text/plain".into());
            }
        }
    }
    out
}

// `inlineToolItem`: a tool result file ref becomes a data URI, or the placeholder text when its blob is gone.
fn hydrate_tool_item(blobs: Option<&Connection>, item: &Value) -> Value {
    let Some(found) = item["uri"].as_str().filter(|_| item["type"] == "file").and_then(|uri| blob(blobs, uri)) else { return item.clone() };
    match found {
        Some(data) => {
            let mut out = item.clone();
            out["uri"] = format!("data:{};base64,{data}", item["mime"].as_str().unwrap_or("application/octet-stream")).into();
            out
        }
        None => json!({"type": "text", "text": unavailable(item)}),
    }
}

// The tool part and, when it completed, the files its result returned.
fn v2_tool(blobs: Option<&Connection>, item: &Value) -> (ToolPart, Vec<FilePart>) {
    let state = &item["state"];
    let tool = v1_tool(item["name"].as_str().unwrap_or_default());
    let input = v1_input(tool, convert::input_map(&state["input"]));
    let created = ms(item, "/time/created").unwrap_or(0);
    let start = ms(item, "/time/ran").unwrap_or(created);
    let end = ms(item, "/time/completed").unwrap_or(start).max(start);
    let metadata = state.get("metadata").and_then(Value::as_object).cloned();
    let content: Vec<Value> = state["content"].as_array().into_iter().flatten().map(|c| hydrate_tool_item(blobs, c)).collect();
    let output: Vec<&str> = content.iter().filter(|c| c["type"] == "text").filter_map(|c| c["text"].as_str()).collect();
    let mut files = Vec::new();
    let state = match state["status"].as_str() {
        Some("completed") => {
            files = content
                .iter()
                .filter(|c| c["type"] == "file")
                .filter_map(|c| Some(FilePart { mime: c["mime"].as_str()?.to_string(), filename: c["name"].as_str().map(str::to_string), url: c["uri"].as_str()?.to_string(), source: None }))
                .collect();
            ToolState::Completed(ToolStateCompleted {
                input,
                output: output.join("\n"),
                title: String::new(),
                metadata: metadata.unwrap_or_default(),
                time: ToolTimeCompleted { start, end, compacted: ms(item, "/time/pruned") },
                attachments: None,
            })
        }
        Some("error") => ToolState::Error(ToolStateError { input, error: state.pointer("/error/message").and_then(Value::as_str).unwrap_or("error").to_string(), metadata, time: ToolTimeError { start, end } }),
        _ => ToolState::Error(ToolStateError { input, error: convert::ABORTED.into(), metadata, time: ToolTimeError { start, end } }),
    };
    (ToolPart { call_id: item["id"].as_str().unwrap_or_default().to_string(), tool: tool.to_string(), state, metadata: None }, files)
}

// `session_part` rows by message id, in ordinal order (`SessionHistory.withParts`).
fn v2_parts(connection: &Connection, session_id: &str) -> Result<HashMap<String, Vec<Value>>, String> {
    let mut out: HashMap<String, Vec<Value>> = HashMap::new();
    if !has_table(connection, "session_part") {
        return Ok(out);
    }
    let mut statement = connection.prepare("SELECT message_id, data FROM session_part WHERE session_id = ?1 ORDER BY message_id, ordinal").map_err(|e| e.to_string())?;
    let rows = statement.query_map([session_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).map_err(|e| e.to_string())?;
    for row in rows {
        let (message_id, data) = row.map_err(|e| e.to_string())?;
        if let Ok(part) = serde_json::from_str::<Value>(&data) {
            out.entry(message_id).or_default().push(part);
        }
    }
    Ok(out)
}

fn v2_messages(connection: &Connection, session: &Session, root: &str) -> Result<(Messages, usize), String> {
    let mut parts = v2_parts(connection, &session.id)?;
    let blobs = has_table(connection, "blob").then_some(connection);
    let mut statement = connection.prepare("SELECT id, type, time_created, data FROM session_message WHERE session_id = ?1 ORDER BY seq").map_err(|e| e.to_string())?;
    let rows = statement.query_map([&session.id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?))).map_err(|e| e.to_string())?;
    let mut t = Transcript::new(ID, &session.id, &session.id, &session.directory, root);
    let fallback = session.model.as_ref().map(|m| Model { provider: m.provider_id.clone(), id: m.id.clone(), variant: m.variant.clone() }).unwrap_or_default();
    let mut skipped = 0;
    for row in rows {
        let (id, kind, created, data) = row.map_err(|e| e.to_string())?;
        let Ok(mut data) = serde_json::from_str::<Value>(&data) else {
            skipped += 1;
            continue;
        };
        // Split rows keep `content: []`; their parts come first, then any inline content an older build appended.
        if let Some(mut folded) = parts.remove(&id).filter(|_| kind == "assistant") {
            folded.extend(data["content"].as_array().cloned().unwrap_or_default());
            data["content"] = Value::Array(folded);
        }
        let at = ms(&data, "/time/created").unwrap_or(created.max(0) as u64);
        t.agent = session.agent.clone().unwrap_or_else(|| "build".into());
        match kind.as_str() {
            "user" | "synthetic" => {
                if let Some(agent) = data.pointer("/metadata/agent").and_then(Value::as_str) {
                    t.agent = agent.to_string();
                }
                let mut bodies = Vec::new();
                if let Some(text) = data["text"].as_str().filter(|t| !t.is_empty()) {
                    bodies.push(if kind == "synthetic" { convert::synthetic(text) } else { convert::text(text) });
                }
                for file in data["files"].as_array().into_iter().flatten() {
                    let file = &hydrate_file(blobs, file);
                    if let Some(url) = data_url(file) {
                        let mime = file["mime"].as_str().unwrap_or("application/octet-stream").to_string();
                        bodies.push(PartBody::File(FilePart { mime, filename: file["name"].as_str().map(str::to_string), url, source: None }));
                    }
                }
                for agent in data["agents"].as_array().into_iter().flatten() {
                    if let Some(name) = agent["name"].as_str() {
                        bodies.push(PartBody::Agent(AgentPart { name: name.to_string(), source: None }));
                    }
                }
                t.user(at, bodies);
            }
            "assistant" => {
                if let Some(agent) = data["agent"].as_str() {
                    t.agent = agent.to_string();
                }
                let index = t.assistant(at, true);
                if let Some(model) = model_of(&data["model"]) {
                    t.set_model(index, &model);
                }
                for item in data["content"].as_array().into_iter().flatten() {
                    match item["type"].as_str() {
                        Some("text") => {
                            t.push(index, convert::text(item["text"].as_str().unwrap_or_default()));
                        }
                        Some("reasoning") => {
                            let start = ms(item, "/time/created").unwrap_or(at);
                            t.push(index, convert::reasoning(item["text"].as_str().unwrap_or_default(), start, ms(item, "/time/completed")));
                        }
                        Some("tool") => {
                            let (tool, files) = v2_tool(blobs, item);
                            let position = t.push(index, PartBody::Tool(tool));
                            t.attach(index, position, files);
                        }
                        _ => {}
                    }
                }
                let tokens = convert::tokens(number(&data, "/tokens/input"), number(&data, "/tokens/output"), number(&data, "/tokens/reasoning"), number(&data, "/tokens/cache/read"), number(&data, "/tokens/cache/write"));
                t.set_usage(index, tokens, number(&data, "/cost"));
                if let Some(finish) = data["finish"].as_str() {
                    t.set_finish(index, finish);
                }
                if let Some(error) = data.pointer("/error/message").and_then(Value::as_str) {
                    t.set_error(index, error);
                }
                t.touch(index, ms(&data, "/time/completed").unwrap_or(at));
                t.close();
            }
            "compaction" => {
                let summary = if data["status"] == "completed" { data["summary"].as_str().unwrap_or_default() } else { "" };
                t.compaction(at, data["reason"] == "auto", summary);
            }
            _ => {}
        }
    }
    Ok((t.finish(&fallback), skipped))
}

pub fn convert(connection: &Connection, item: &SessionSource, v2: bool) -> Result<Converted, String> {
    let table = if v2 { "session_v2" } else { "session" };
    let row = row(connection, &format!("SELECT * FROM {table} WHERE id = ?1"), &item.key)?.ok_or("session not found in source")?;
    let mut session = session_from(&row);
    let found = if has_table(connection, "project") { row_project(connection, &session.project_id)? } else { None };
    // The session row needs its project row; a missing one is recreated bare.
    let project = found.unwrap_or_else(|| Project {
        id: session.project_id.clone(),
        worktree: "/".into(),
        vcs: None,
        name: None,
        icon: None,
        commands: None,
        time: ProjectTime { created: session.time.created, updated: session.time.updated, initialized: None },
        sandboxes: Vec::new(),
    });
    let root = project.worktree.clone();
    let project = Some(project);
    let (messages, skipped) = if v2 { v2_messages(connection, &session, &root)? } else { legacy_messages(connection, &item.key)? };
    if skipped > 0 {
        let mut metadata = session.metadata.take().unwrap_or_default();
        metadata.insert("importSkippedRows".into(), json!(skipped));
        session.metadata = Some(metadata);
    }
    let todos = todos(connection, &item.key);
    Ok(Converted { session, messages, todos, project, placed: None })
}

fn row_project(connection: &Connection, id: &str) -> Result<Option<Project>, String> {
    Ok(row(connection, "SELECT * FROM project WHERE id = ?1", id)?.map(|r| project_from(&r)))
}
