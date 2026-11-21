// Per-tool scanners and session converters, and the shared code that writes converted sessions.
mod claude;
mod codex;
mod dsh;
mod grok;
mod mcpjson;
mod opencode;

use std::collections::HashMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use opencode_core::config;
use opencode_core::flag::Lookup;
use opencode_core::paths::Paths;
use opencode_storage::{Message, Part, Project, ProjectTime, Session, SessionTime, Store, Todo, Vcs, identity};
use serde_json::{Map, Value};

use crate::{Category, Ctx, ImportReport, Payload, Plan, SourceReport};

// A conversion error meaning the source session holds no messages; such sessions are skipped quietly.
pub const EMPTY: &str = "no messages";

#[derive(Debug, Clone)]
pub struct SessionSource {
    pub key: String,
    // Source modification time (ms); a newer one replaces an earlier import.
    pub updated: u64,
    pub kind: SessionKind,
}

#[derive(Debug, Clone)]
pub enum SessionKind {
    Opencode { db: PathBuf, v2: bool },
    Claude { file: PathBuf, parent: Option<String>, title: Option<String> },
    Codex { file: PathBuf, title: Option<String> },
    Grok { dir: PathBuf },
    Dsh { file: PathBuf },
}

pub struct Converted {
    pub session: Session,
    pub messages: Vec<(Message, Vec<Part>)>,
    pub todos: Option<Vec<Todo>>,
    // The source's own project row (opencode), created in the Rust store when missing.
    pub project: Option<Project>,
    // The project resolved from the session's directory (other sources).
    pub placed: Option<Placed>,
}

pub fn scan_all(ctx: &Ctx) -> Vec<SourceReport> {
    vec![
        opencode::scan(ctx),
        claude::scan(ctx),
        codex::scan(ctx),
        mcpjson::scan(ctx, "cursor", &ctx.home.join(".cursor").join("mcp.json")),
        mcpjson::scan(ctx, "windsurf", &ctx.home.join(".windsurf").join("mcp.json")),
        mcpjson::scan(ctx, "mcp-json", &ctx.home.join(".mcp.json")),
        grok::scan(ctx),
        dsh::scan(ctx),
    ]
}

pub fn detect(paths: &Paths, home: &Path, env: Lookup) -> Vec<&'static str> {
    let var = |key: &str| env(key).filter(|v| !v.trim().is_empty()).map(PathBuf::from);
    let claude = home.join(".claude");
    let codex = home.join(".codex");
    let checks: [(&'static str, bool); 8] = [
        ("opencode", !opencode::databases(paths, home).is_empty()),
        ("claude-code", home.join(".claude.json").is_file() || claude.join("agents").is_dir() || claude.join("projects").is_dir()),
        ("codex", codex.join("config.toml").is_file() || codex.join("sessions").is_dir() || codex.join("agents").is_dir()),
        ("cursor", home.join(".cursor").join("mcp.json").is_file()),
        ("windsurf", home.join(".windsurf").join("mcp.json").is_file()),
        ("mcp-json", home.join(".mcp.json").is_file()),
        ("grok-build", var("GROK_HOME").unwrap_or_else(|| home.join(".grok")).is_dir()),
        ("deepseek-harness", var("DSH_HOME").unwrap_or_else(|| home.join(".dsh")).is_dir()),
    ];
    checks.into_iter().filter(|(_, found)| *found).map(|(id, _)| id).collect()
}

// Each line of a JSONL file that parses; the count of those that do not.
pub fn jsonl(file: &Path) -> std::io::Result<(Vec<Value>, usize)> {
    let reader = std::io::BufReader::new(std::fs::File::open(file)?);
    let mut out = Vec::new();
    let mut bad = 0;
    for line in reader.lines() {
        let Ok(line) = line else {
            bad += 1;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(&line) {
            Ok(value) => out.push(value),
            Err(_) => bad += 1,
        }
    }
    Ok((out, bad))
}

pub fn first_line(file: &Path) -> Option<Value> {
    let reader = std::io::BufReader::new(std::fs::File::open(file).ok()?);
    let line = reader.lines().next()?.ok()?;
    serde_json::from_str(&line).ok()
}

pub fn title_of(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    let mut title: String = line.chars().take(80).collect();
    if line.chars().count() > 80 {
        title.push('…');
    }
    title
}

pub fn import_metadata(source: &str, key: &str, updated: u64, mut base: Map<String, Value>) -> Map<String, Value> {
    base.insert("importedFrom".into(), source.into());
    base.insert("sourceId".into(), key.into());
    base.insert("sourceUpdated".into(), updated.into());
    base
}

// A new session row of an imported conversation.
pub fn session(id: &str, project: &Placed, title: String, created: u64, updated: u64, metadata: Map<String, Value>) -> Session {
    Session {
        id: id.to_string(),
        slug: opencode_storage::slug(),
        project_id: project.id.clone(),
        workspace_id: None,
        directory: project.directory.clone(),
        path: project.path.clone(),
        parent_id: None,
        summary: None,
        cost: Some(0.0),
        tokens: None,
        share: None,
        title: if title.trim().is_empty() { "Imported session".into() } else { title },
        agent: None,
        model: None,
        version: env!("CARGO_PKG_VERSION").to_string(),
        metadata: Some(metadata),
        time: SessionTime { created, updated: updated.max(created), compacting: None, archived: None },
        permission: None,
        revert: None,
    }
}

// Where a directory lands in the Rust store.
#[derive(Debug, Clone)]
pub struct Placed {
    pub id: String,
    pub directory: String,
    pub root: String,
    pub path: Option<String>,
    worktree: Option<PathBuf>,
    remembered: PathBuf,
}

#[derive(Default)]
pub struct Projects {
    cache: HashMap<String, Placed>,
}

fn plain(path: &str) -> String {
    path.strip_prefix(r"\\?\").unwrap_or(path).to_string()
}

impl Projects {
    // `identity::resolve` (read-only git queries; the id is not cached into the repository).
    pub fn place(&mut self, directory: &str) -> Placed {
        let directory = plain(directory);
        if let Some(found) = self.cache.get(&directory) {
            return found.clone();
        }
        let resolved = identity::resolve(Path::new(&directory));
        let worktree = config::find_worktree(Path::new(&directory));
        let root = worktree.as_ref().map_or_else(|| "/".to_string(), |w| w.to_string_lossy().into_owned());
        let path = worktree.as_ref().map(|w| {
            let relative = Path::new(&directory).strip_prefix(w).map(Path::to_path_buf).unwrap_or_default();
            relative.to_string_lossy().replace('\\', "/")
        });
        let placed = Placed { id: resolved.id, directory: directory.clone(), root, path, worktree, remembered: resolved.directory };
        self.cache.insert(directory, placed.clone());
        placed
    }

    pub fn ensure(&self, store: &Store, placed: &Placed) -> opencode_storage::store::Result<()> {
        if store.get_project(&placed.id)?.is_none() {
            let now = opencode_storage::id::now_ms();
            store.upsert_project(&Project {
                id: placed.id.clone(),
                worktree: placed.root.clone(),
                vcs: placed.worktree.as_ref().map(|_| Vcs::Git),
                name: None,
                icon: None,
                commands: None,
                time: ProjectTime { created: now, updated: now, initialized: None },
                sandboxes: Vec::new(),
            })?;
        }
        if placed.id != opencode_storage::GLOBAL_PROJECT_ID {
            let _ = store.add_project_directory(&placed.id, &plain(&placed.remembered.to_string_lossy()), None, false);
        }
        Ok(())
    }
}

fn rust_id(source: &str, item: &SessionSource) -> String {
    match item.kind {
        SessionKind::Opencode { .. } => item.key.clone(),
        _ => crate::ids::session(source, &item.key),
    }
}

enum Status {
    Unchanged,
    Taken,
    Import,
}

fn status(store: &Store, id: &str, source: &str, item: &SessionSource) -> opencode_storage::store::Result<Status> {
    let Some(existing) = store.get_session(id)? else { return Ok(Status::Import) };
    let metadata = existing.metadata.unwrap_or_default();
    let ours = metadata.get("importedFrom").and_then(Value::as_str) == Some(source) && metadata.get("sourceId").and_then(Value::as_str) == Some(item.key.as_str());
    if !ours {
        return Ok(Status::Taken);
    }
    let stored = metadata.get("sourceUpdated").and_then(Value::as_u64).unwrap_or(0);
    Ok(if stored >= item.updated { Status::Unchanged } else { Status::Import })
}

fn convert(source: &'static str, item: &SessionSource, id: &str, projects: &mut Projects, db: &mut opencode::Databases) -> Result<Converted, String> {
    match &item.kind {
        SessionKind::Opencode { db: path, v2 } => opencode::convert(db.open(path)?, item, *v2),
        SessionKind::Claude { file, parent, title } => claude::convert(file, item, id, parent.as_deref(), title.clone(), projects),
        SessionKind::Codex { file, title } => codex::convert(file, item, id, title.clone(), projects),
        SessionKind::Grok { dir } => grok::convert(dir, item, id, projects),
        SessionKind::Dsh { file } => dsh::convert(file, item, id, projects),
    }
    .map(|mut converted| {
        let metadata = converted.session.metadata.take().unwrap_or_default();
        converted.session.metadata = Some(import_metadata(source, &item.key, item.updated, metadata));
        converted
    })
}

pub fn apply_sessions(plan: &Plan, store: &Store, report: &mut ImportReport) {
    let mut projects = Projects::default();
    let mut databases = opencode::Databases::default();
    for planned in &plan.items {
        let Payload::Session(item) = &planned.item.payload else { continue };
        let source = planned.source;
        let id = rust_id(source, item);
        let result = status(store, &id, source, item).map_err(|e| e.to_string()).and_then(|state| match state {
            Status::Unchanged => Ok(Some(false)),
            Status::Taken => Ok(None),
            Status::Import => {
                let converted = convert(source, item, &id, &mut projects, &mut databases)?;
                if let Some(project) = &converted.project
                    && store.get_project(&project.id).map_err(|e| e.to_string())?.is_none()
                {
                    store.upsert_project(project).map_err(|e| e.to_string())?;
                }
                if let Some(placed) = &converted.placed {
                    projects.ensure(store, placed).map_err(|e| e.to_string())?;
                }
                let writes = store.import_session(&converted.session, &converted.messages, converted.todos.as_deref()).map_err(|e| e.to_string())?;
                Ok(Some(writes.changed()))
            }
        });
        let outcome = report.outcome(source, Category::Sessions);
        match result {
            Ok(Some(true)) => outcome.written += 1,
            Ok(Some(false)) => outcome.unchanged += 1,
            Ok(None) => {
                outcome.skipped += 1;
                report.warnings.push(format!("{source} session {}: a different session already uses id {id}", item.key));
            }
            Err(error) if error == EMPTY => outcome.skipped += 1,
            Err(error) => {
                outcome.failed += 1;
                report.errors.push(format!("{source} session {}: {error}", item.key));
            }
        }
    }
}

pub fn session_item(name: String, detail: String, source: SessionSource) -> crate::Item {
    crate::Item::new(Category::Sessions, name, detail, Payload::Session(source))
}
