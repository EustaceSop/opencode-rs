// Claude Code: `~/.claude.json` (`mcpServers`, `projects.<path>.mcpServers`), `~/.claude/agents/*.md`, and transcripts
// `~/.claude/projects/<encoded cwd>/<session>.jsonl` (+ `<session>/subagents/agent-*.jsonl`). Its skills and
// CLAUDE.md are read by the Rust build already.
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::mcpjson::{mcp_item, read_json};
use super::{Converted, Projects, SessionKind, SessionSource, jsonl, session, session_item, title_of};
use crate::convert::{self, Model, Transcript};
use crate::ids;
use crate::{Category, Ctx, Item, Payload, SourceReport, agents, mcp};

const ID: &str = "claude-code";

fn mcp_items(ctx: &Ctx, config: &Value, report: &mut SourceReport) {
    let mut seen: Vec<String> = Vec::new();
    for (name, server) in config.get("mcpServers").and_then(Value::as_object).into_iter().flatten() {
        match mcp::from_json(server) {
            Ok(converted) => {
                seen.push(name.clone());
                report.items.push(mcp_item(ctx, name, converted, "~/.claude.json"));
            }
            Err(error) => report.warnings.push(format!("MCP server {name}: {error}")),
        }
    }
    for (path, project) in config.get("projects").and_then(Value::as_object).into_iter().flatten() {
        for (name, server) in project.get("mcpServers").and_then(Value::as_object).into_iter().flatten() {
            if seen.contains(name) {
                report.warnings.push(format!("MCP server {name} of project {path} skipped: the name is already used by another Claude Code server"));
                continue;
            }
            match mcp::from_json(server) {
                Ok(mut converted) => {
                    converted.entry["enabled"] = false.into();
                    converted.flags.push(format!("project-scoped in Claude Code ({path}); imported disabled"));
                    seen.push(name.clone());
                    report.items.push(mcp_item(ctx, name, converted, path));
                }
                Err(error) => report.warnings.push(format!("MCP server {name} of project {path}: {error}")),
            }
        }
    }
}

pub fn agent_items(ctx: &Ctx, dir: &Path, report: &mut SourceReport, convert: fn(&str, &str) -> Result<agents::Agent, String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "md")).collect();
    files.sort();
    for file in files {
        let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let converted = std::fs::read_to_string(&file).map_err(|e| e.to_string()).and_then(|text| convert(&stem, &text));
        match converted {
            Ok(agent) => {
                let conflict = ctx.visible.agent_conflict(&agent.name, &agent.file, &agent.content);
                let mut item = Item::new(Category::Agents, &agent.name, file.to_string_lossy(), Payload::Agent { file: agent.file, content: agent.content });
                item.flags = agent.flags;
                item.conflict = conflict;
                report.items.push(item);
            }
            Err(error) => report.warnings.push(format!("agent {}: {error}", file.display())),
        }
    }
}

fn sessions(root: &Path, report: &mut SourceReport) {
    let Ok(projects) = std::fs::read_dir(root) else { return };
    let mut dirs: Vec<PathBuf> = projects.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    dirs.sort();
    for dir in dirs {
        let project = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "jsonl")).collect();
        files.sort();
        for file in files {
            let key = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let source = SessionSource { key: key.clone(), updated: ids::file_mtime_ms(&file), kind: SessionKind::Claude { file: file.clone(), parent: None, title: None } };
            report.items.push(session_item(key.clone(), project.clone(), source));
            let Ok(children) = std::fs::read_dir(dir.join(&key).join("subagents")) else { continue };
            let mut children: Vec<PathBuf> = children.flatten().map(|e| e.path()).filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "jsonl")).collect();
            children.sort();
            for child in children {
                let stem = child.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let meta = child.with_extension("meta.json");
                let title = read_json(&meta, report).and_then(|m| m.get("description").and_then(Value::as_str).map(str::to_string));
                let source = SessionSource {
                    key: format!("{key}/{stem}"),
                    updated: ids::file_mtime_ms(&child),
                    kind: SessionKind::Claude { file: child, parent: Some(key.clone()), title: title.clone() },
                };
                report.items.push(session_item(title.unwrap_or(stem), project.clone(), source));
            }
        }
    }
}

pub fn scan(ctx: &Ctx) -> SourceReport {
    let mut report = SourceReport::new(ID);
    let claude = ctx.home.join(".claude");
    let config = ctx.home.join(".claude.json");
    if report.locate(&config)
        && let Some(value) = read_json(&config, &mut report)
    {
        mcp_items(ctx, &value, &mut report);
    }
    if report.locate(&claude.join("agents")) {
        agent_items(ctx, &claude.join("agents"), &mut report, agents::from_claude);
    }
    if report.locate(&claude.join("projects")) {
        sessions(&claude.join("projects"), &mut report);
    }
    let skills = crate::skills::count(&claude.join("skills"));
    if skills > 0 {
        report.found = true;
        report.shared.push(format!("{skills} skills in {} (discovered by the Rust build like the original)", claude.join("skills").display()));
    }
    if claude.join("CLAUDE.md").is_file() {
        report.found = true;
        report.shared.push(format!("{} (read as global instructions)", claude.join("CLAUDE.md").display()));
    }
    report
}

fn finish(reason: &str) -> &str {
    match reason {
        "end_turn" | "stop_sequence" => "stop",
        "tool_use" => "tool-calls",
        "max_tokens" => "length",
        "refusal" => "content-filter",
        other => other,
    }
}

fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => items.iter().filter(|i| i["type"] == "text").filter_map(|i| i["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

pub fn convert(file: &Path, item: &SessionSource, id: &str, parent: Option<&str>, title: Option<String>, projects: &mut Projects) -> Result<Converted, String> {
    let (records, _) = jsonl(file).map_err(|e| e.to_string())?;
    let main = parent.is_none();
    let cwd = records.iter().find_map(|r| r["cwd"].as_str()).unwrap_or("/").to_string();
    let placed = projects.place(&cwd);
    let mut t = Transcript::new(ID, &item.key, id, &placed.directory, &placed.root);
    let mut group: Option<String> = None;
    let mut title = title;
    let mut first_text: Option<String> = None;
    let mut created: Option<u64> = None;
    let mut ts = 0;
    for record in &records {
        if let Some(at) = record["timestamp"].as_str().and_then(ids::iso_ms) {
            ts = at;
            created.get_or_insert(at);
        }
        let kind = record["type"].as_str().unwrap_or_default();
        if main && record["isSidechain"] == true && matches!(kind, "user" | "assistant") {
            continue;
        }
        match kind {
            "ai-title" => title = record["aiTitle"].as_str().map(str::to_string).or(title),
            "summary" if title.is_none() => title = record["summary"].as_str().map(str::to_string),
            "user" if record["isMeta"] != true => {
                let content = &record["message"]["content"];
                if record["isCompactSummary"] == true {
                    t.compaction(ts, true, &content_text(content));
                    group = None;
                    continue;
                }
                let mut bodies = Vec::new();
                match content {
                    Value::String(text) => bodies.push(convert::text(text)),
                    Value::Array(blocks) => {
                        for block in blocks {
                            match block["type"].as_str() {
                                Some("text") => bodies.push(convert::text(block["text"].as_str().unwrap_or_default())),
                                Some("tool_result") => {
                                    let output = content_text(&block["content"]);
                                    t.tool_result(block["tool_use_id"].as_str().unwrap_or_default(), ts, &output, block["is_error"] == true, None);
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                if !bodies.is_empty() {
                    if first_text.is_none()
                        && let Some(opencode_storage::PartBody::Text(text)) = bodies.first()
                    {
                        first_text = Some(text.text.clone());
                    }
                    t.user(ts, bodies);
                    group = None;
                }
            }
            "assistant" => {
                let message = &record["message"];
                let message_id = message["id"].as_str().map(str::to_string);
                let fresh = group.is_none() || group != message_id;
                let index = t.assistant(ts, fresh);
                group = message_id;
                if let Some(model) = message["model"].as_str() {
                    t.set_model(index, &Model::new("anthropic", model));
                }
                for block in message["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("text") => {
                            t.push(index, convert::text(block["text"].as_str().unwrap_or_default()));
                        }
                        Some("thinking") => {
                            t.push(index, convert::reasoning(block["thinking"].as_str().unwrap_or_default(), ts, Some(ts)));
                        }
                        Some("tool_use") => {
                            t.tool_call(ts, block["id"].as_str().unwrap_or_default(), block["name"].as_str().unwrap_or_default(), convert::input_map(&block["input"]), None);
                        }
                        _ => {}
                    }
                }
                let usage = &message["usage"];
                if usage.is_object() {
                    let n = |key: &str| usage[key].as_f64().unwrap_or(0.0);
                    t.set_usage(index, convert::tokens(n("input_tokens"), n("output_tokens"), 0.0, n("cache_read_input_tokens"), n("cache_creation_input_tokens")), 0.0);
                }
                if let Some(reason) = message["stop_reason"].as_str() {
                    t.set_finish(index, finish(reason));
                }
                if record["isApiErrorMessage"] == true {
                    let text = content_text(&message["content"]);
                    t.set_error(index, if text.is_empty() { "API error" } else { &text });
                }
                t.touch(index, ts);
            }
            _ => {}
        }
    }
    if t.is_empty() {
        return Err(super::EMPTY.into());
    }
    let created = created.unwrap_or(item.updated);
    let title = title.or_else(|| first_text.as_deref().map(title_of)).unwrap_or_else(|| "Claude Code session".into());
    let mut row = session(id, &placed, title, created, ts.max(created), serde_json::Map::new());
    row.parent_id = parent.map(|p| ids::session(ID, p));
    let messages = t.finish(&Model::new("anthropic", "unknown"));
    Ok(Converted { session: row, messages, todos: None, project: None, placed: Some(placed) })
}
