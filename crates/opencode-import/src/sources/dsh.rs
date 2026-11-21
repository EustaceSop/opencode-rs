// DeepSeek Harness (`dsh`): `$DSH_HOME` (default `~/.dsh`). MCP servers are rows of the YAML patches
// `cordis.patch.yml` and `profiles/*/cordis.patch.yml` (`@deepseek-ai/dsh-mcp-manager` `config.servers.*`, or
// `@deepseek-ai/dsh-mcp-client` rows); skills are `skills/<dir>/SKILL.md`; sessions are
// `sessions/<projectKey>/<id>/session[.vN].jsonl[.zstd]`, the highest version winning. zstd logs cannot be read here.
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::mcpjson::mcp_item;
use super::{Converted, Projects, SessionKind, SessionSource, first_line, jsonl, session, session_item, title_of};
use crate::convert::{self, Model, Transcript};
use crate::ids;
use crate::{Ctx, SourceReport, mcp, skills};

const ID: &str = "deepseek-harness";
const MANAGER: &str = "@deepseek-ai/dsh-mcp-manager";
const CLIENT: &str = "@deepseek-ai/dsh-mcp-client";

pub fn home(ctx: &Ctx) -> PathBuf {
    ctx.var("DSH_HOME").map_or_else(|| ctx.home.join(".dsh"), PathBuf::from)
}

fn yaml_to_json(value: serde_yaml_ng::Value) -> Value {
    use serde_yaml_ng::Value as Y;
    match value {
        Y::Null => Value::Null,
        Y::Bool(b) => Value::Bool(b),
        Y::Number(n) => n.as_i64().map(Value::from).or_else(|| n.as_u64().map(Value::from)).or_else(|| n.as_f64().and_then(serde_json::Number::from_f64).map(Value::Number)).unwrap_or(Value::Null),
        Y::String(s) => Value::String(s),
        Y::Sequence(items) => Value::Array(items.into_iter().map(yaml_to_json).collect()),
        Y::Mapping(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| {
                    let key = match k {
                        Y::String(s) => s,
                        other => serde_yaml_ng::to_string(&other).unwrap_or_default().trim().to_string(),
                    };
                    (key, yaml_to_json(v))
                })
                .collect(),
        ),
        // `!!js` expressions and other custom tags are not evaluated.
        Y::Tagged(_) => Value::Null,
    }
}

fn patch_files(root: &Path) -> Vec<PathBuf> {
    let mut files = vec![root.join("cordis.patch.yml")];
    let mut profiles: Vec<PathBuf> = std::fs::read_dir(root.join("profiles")).into_iter().flatten().flatten().map(|e| e.path().join("cordis.patch.yml")).collect();
    profiles.sort();
    files.extend(profiles);
    files.into_iter().filter(|f| f.is_file()).collect()
}

struct Found {
    servers: Vec<(String, Value, bool)>,
    roots: Vec<String>,
}

fn walk(value: &Value, found: &mut Found) {
    match value {
        Value::Array(items) => items.iter().for_each(|v| walk(v, found)),
        Value::Object(map) => {
            let disabled = map.get("disabled") == Some(&Value::Bool(true));
            let config = map.get("config").cloned().unwrap_or(Value::Null);
            match map.get("name").and_then(Value::as_str) {
                Some(MANAGER) => {
                    for (name, server) in config["servers"].as_object().into_iter().flatten() {
                        found.servers.push((name.clone(), server.clone(), disabled));
                    }
                }
                Some(CLIENT) => {
                    if let Some(name) = config["serverName"].as_str() {
                        found.servers.push((name.to_string(), config.clone(), disabled));
                    }
                }
                Some(name) if name.ends_with("session-persistence-jsonl") => {
                    if let Some(root) = config["root"].as_str() {
                        found.roots.push(root.to_string());
                    }
                }
                _ => {}
            }
            map.values().for_each(|v| walk(v, found));
        }
        _ => {}
    }
}

// `session.jsonl` is version 0, `session.vN.jsonl` version N; `.zstd` marks a compressed log.
fn version(name: &str) -> Option<(u32, bool)> {
    let (base, zstd) = match name.strip_suffix(".zstd") {
        Some(base) => (base, true),
        None => (name, false),
    };
    let rest = base.strip_prefix("session")?.strip_suffix(".jsonl")?;
    if rest.is_empty() {
        return Some((0, zstd));
    }
    rest.strip_prefix(".v")?.parse().ok().map(|n| (n, zstd))
}

fn latest(dir: &Path) -> Option<(PathBuf, bool)> {
    let entries = std::fs::read_dir(dir).ok()?;
    entries.flatten().filter_map(|e| version(&e.file_name().to_string_lossy()).map(|(n, z)| (n, z, e.path()))).max_by_key(|(n, _, _)| *n).map(|(_, z, p)| (p, z))
}

fn sessions(root: &Path, report: &mut SourceReport, compressed: &mut usize) {
    let Ok(projects) = std::fs::read_dir(root) else { return };
    let mut dirs: Vec<PathBuf> = projects.flatten().map(|e| e.path()).filter(|p| p.is_dir()).flat_map(|p| std::fs::read_dir(p).into_iter().flatten().flatten().map(|e| e.path())).filter(|p| p.is_dir()).collect();
    dirs.sort();
    for dir in dirs {
        let Some((file, zstd)) = latest(&dir) else { continue };
        if zstd {
            *compressed += 1;
            continue;
        }
        let Some(header) = first_line(&file).filter(|h| h["type"] == "session") else { continue };
        let Some(key) = header["id"].as_str().map(str::to_string) else { continue };
        let cwd = header["cwd"].as_str().unwrap_or_default().to_string();
        let source = SessionSource { key: key.clone(), updated: ids::file_mtime_ms(&file), kind: SessionKind::Dsh { file } };
        report.items.push(session_item(key, cwd, source));
    }
}

fn expand(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Some(rest) => home.join(rest),
        None if path == "~" => home.to_path_buf(),
        None => PathBuf::from(path),
    }
}

pub fn scan(ctx: &Ctx) -> SourceReport {
    let mut report = SourceReport::new(ID);
    let root = home(ctx);
    if !report.locate(&root) {
        return report;
    }
    let mut found = Found { servers: Vec::new(), roots: Vec::new() };
    for file in patch_files(&root) {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        match serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&text) {
            Ok(value) => walk(&yaml_to_json(value), &mut found),
            Err(error) => report.warnings.push(format!("{}: {error}", file.display())),
        }
    }
    let mut seen: Vec<String> = Vec::new();
    for (name, server, disabled) in found.servers {
        if seen.contains(&name) {
            continue;
        }
        match mcp::from_dsh(&server) {
            Ok(mut converted) => {
                if disabled {
                    converted.entry["enabled"] = false.into();
                }
                seen.push(name.clone());
                report.items.push(mcp_item(ctx, &name, converted, "DeepSeek Harness profile"));
            }
            Err(error) => report.warnings.push(format!("MCP server {name}: {error}")),
        }
    }
    report.items.extend(skills::items(&root.join("skills"), &ctx.visible));
    let agents_home = ctx.var("DSH_AGENTS_HOME").map_or_else(|| ctx.home.join(".agents"), PathBuf::from);
    if agents_home == ctx.home.join(".agents") {
        let shared = skills::count(&agents_home.join("skills"));
        if shared > 0 {
            report.shared.push(format!("{shared} skills in {} (discovered by the Rust build)", agents_home.join("skills").display()));
        }
    } else {
        report.items.extend(skills::items(&agents_home.join("skills"), &ctx.visible));
    }
    if root.join("AGENTS.md").is_file() {
        report.warnings.push(format!("{} is not imported (instruction files are not an import category)", root.join("AGENTS.md").display()));
    }
    let mut roots = vec![root.join("sessions")];
    roots.extend(found.roots.iter().map(|r| expand(r, ctx.home)));
    roots.dedup();
    let mut compressed = 0;
    for dir in roots {
        sessions(&dir, &mut report, &mut compressed);
    }
    if compressed > 0 {
        report.warnings.push(format!("{compressed} zstd-compressed session logs cannot be read by this build and were left out"));
    }
    report
}

fn texts(content: &Value) -> Vec<String> {
    content.as_array().into_iter().flatten().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str().map(str::to_string)).collect()
}

pub fn convert(file: &Path, item: &SessionSource, id: &str, projects: &mut Projects) -> Result<Converted, String> {
    let (records, _) = jsonl(file).map_err(|e| e.to_string())?;
    let header = records.first().filter(|h| h["type"] == "session").cloned().ok_or("no session header")?;
    let created = header["createdAt"].as_u64().unwrap_or(item.updated);
    let placed = projects.place(header["cwd"].as_str().unwrap_or("/"));
    let mut t = Transcript::new(ID, &item.key, id, &placed.directory, &placed.root);
    let mut title: Option<String> = None;
    let mut first_text: Option<String> = None;
    let mut ts = created;
    for event in records.iter().skip(1) {
        ts = event["time"].as_u64().unwrap_or(ts);
        let data = &event["data"];
        match event["type"].as_str().unwrap_or_default() {
            "user/message" if data.pointer("/source/kind").and_then(Value::as_str) == Some("user") => {
                let texts = texts(&data["content"]);
                if let Some(first) = texts.first() {
                    first_text.get_or_insert_with(|| first.clone());
                }
                t.user(ts, texts.iter().map(|text| convert::text(text)).collect());
            }
            "assistant/message" => {
                let message = &data["message"];
                let index = t.assistant(ts, true);
                if let (Some(provider), Some(model)) = (message.pointer("/source/provider").and_then(Value::as_str), message.pointer("/source/model").and_then(Value::as_str)) {
                    t.set_model(index, &Model::new(provider, model));
                }
                for block in message["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("text") => {
                            t.push(index, convert::text(block["text"].as_str().unwrap_or_default()));
                        }
                        Some("reasoning") => {
                            t.push(index, convert::reasoning(block["text"].as_str().unwrap_or_default(), ts, Some(ts)));
                        }
                        Some("tool-call") => {
                            t.tool_call(ts, block["id"].as_str().unwrap_or_default(), block["name"].as_str().unwrap_or_default(), convert::input_map(&block["arguments"]), None);
                        }
                        _ => {}
                    }
                }
                if let Some(usage) = data["usage"].as_object() {
                    let n = |key: &str| usage.get(key).and_then(Value::as_f64).unwrap_or(0.0);
                    t.set_usage(index, convert::tokens(n("inputTokens"), n("outputTokens"), 0.0, 0.0, 0.0), 0.0);
                }
                t.close();
            }
            "tool/result" => {
                let message = &data["message"];
                let failed = message["isError"] == true || data.get("error").is_some_and(|e| !e.is_null());
                t.tool_result(message["toolCallId"].as_str().unwrap_or_default(), ts, &texts(&message["content"]).join("\n"), failed, None);
            }
            "session/title" => title = data["title"].as_str().map(str::to_string).or(title),
            _ => {}
        }
    }
    if t.is_empty() {
        return Err(super::EMPTY.into());
    }
    let title = title.or_else(|| first_text.as_deref().map(title_of)).unwrap_or_else(|| "DeepSeek Harness session".into());
    let mut row = session(id, &placed, title, created, ts.max(created), Map::new());
    let parent = header["parentSession"].as_str().or(header.pointer("/parentSession/id").and_then(Value::as_str));
    row.parent_id = parent.map(|p| ids::session(ID, p));
    let messages = t.finish(&Model::new("deepseek", "unknown"));
    Ok(Converted { session: row, messages, todos: None, project: None, placed: Some(placed) })
}
