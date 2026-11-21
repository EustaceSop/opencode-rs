// Grok Build (xai-org/grok-build): `$GROK_HOME` (default `~/.grok`) holding `config.toml` (`[mcp_servers.*]`,
// `disabled_mcp_servers`), `agents/*.md`, `skills/**/SKILL.md`, and sessions
// `sessions/<url-encoded cwd>/<id>/{summary.json, chat_history.jsonl}`.
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::claude::agent_items;
use super::mcpjson::{mcp_item, read_json};
use super::{Converted, Projects, SessionKind, SessionSource, jsonl, session, session_item, title_of};
use crate::convert::{self, Model, Transcript};
use crate::ids;
use crate::{Ctx, SourceReport, agents, mcp, skills};

const ID: &str = "grok-build";

pub fn home(ctx: &Ctx) -> PathBuf {
    ctx.var("GROK_HOME").map_or_else(|| ctx.home.join(".grok"), PathBuf::from)
}

fn mcp_items(ctx: &Ctx, config: &Path, report: &mut SourceReport) {
    let Ok(text) = std::fs::read_to_string(config) else { return };
    let value = match text.parse::<toml::Table>() {
        Ok(table) => mcp::toml_to_json(toml::Value::Table(table)),
        Err(error) => {
            report.warnings.push(format!("{}: {error}", config.display()));
            return;
        }
    };
    let disabled: Vec<&str> = value["disabled_mcp_servers"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    for (name, server) in value.get("mcp_servers").and_then(Value::as_object).into_iter().flatten() {
        match mcp::from_grok(server) {
            Ok(mut converted) => {
                if disabled.contains(&name.as_str()) {
                    converted.entry["enabled"] = false.into();
                }
                report.items.push(mcp_item(ctx, name, converted, &config.to_string_lossy()));
            }
            Err(error) => report.warnings.push(format!("MCP server {name}: {error}")),
        }
    }
}

fn sessions(root: &Path, report: &mut SourceReport) {
    let Ok(buckets) = std::fs::read_dir(root) else { return };
    let mut dirs: Vec<PathBuf> = buckets.flatten().map(|e| e.path()).filter(|p| p.is_dir()).flat_map(|bucket| std::fs::read_dir(bucket).into_iter().flatten().flatten().map(|e| e.path())).filter(|p| p.join("summary.json").is_file()).collect();
    dirs.sort();
    for dir in dirs {
        let Some(summary) = read_json(&dir.join("summary.json"), report) else { continue };
        if summary["hidden"] == true {
            continue;
        }
        let key = summary.pointer("/info/id").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        let updated = ids::file_mtime_ms(&dir.join("summary.json")).max(ids::file_mtime_ms(&dir.join("chat_history.jsonl")));
        let title = summary["generated_title"].as_str().or(summary["session_summary"].as_str()).filter(|t| !t.is_empty()).map(str::to_string);
        let cwd = summary.pointer("/info/cwd").and_then(Value::as_str).unwrap_or_default().to_string();
        report.items.push(session_item(title.unwrap_or_else(|| key.clone()), cwd, SessionSource { key, updated, kind: SessionKind::Grok { dir } }));
    }
}

pub fn scan(ctx: &Ctx) -> SourceReport {
    let mut report = SourceReport::new(ID);
    let root = home(ctx);
    if !report.locate(&root) {
        return report;
    }
    mcp_items(ctx, &root.join("config.toml"), &mut report);
    let legacy = ctx.home.join(".grok").join("agents");
    let mut agent_dirs = vec![root.join("agents")];
    if !agent_dirs.contains(&legacy) {
        agent_dirs.push(legacy);
    }
    for dir in agent_dirs {
        agent_items(ctx, &dir, &mut report, agents::from_grok);
    }
    report.items.extend(skills::items(&root.join("skills"), &ctx.visible));
    for name in ["AGENTS.md", "Agents.md", "AGENT.md"] {
        if root.join(name).is_file() {
            report.warnings.push(format!("{} is not imported (instruction files are not an import category)", root.join(name).display()));
        }
    }
    sessions(&root.join("sessions"), &mut report);
    report
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts.iter().filter(|p| p["type"] == "text" || p.get("type").is_none()).filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

fn call_name(call: &Value) -> &str {
    call["name"].as_str().or(call.pointer("/function/name").and_then(Value::as_str)).unwrap_or_default()
}

fn call_arguments(call: &Value) -> &Value {
    if call.get("arguments").is_some() { &call["arguments"] } else { call.pointer("/function/arguments").unwrap_or(&Value::Null) }
}

pub fn convert(dir: &Path, item: &SessionSource, id: &str, projects: &mut Projects) -> Result<Converted, String> {
    let summary: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("summary.json")).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let (records, _) = jsonl(&dir.join("chat_history.jsonl")).map_err(|e| e.to_string())?;
    let placed = projects.place(summary.pointer("/info/cwd").and_then(Value::as_str).unwrap_or("/"));
    let created = summary["created_at"].as_str().and_then(ids::iso_ms).unwrap_or(item.updated);
    let updated = summary["updated_at"].as_str().and_then(ids::iso_ms).unwrap_or(item.updated).max(created);
    let mut t = Transcript::new(ID, &item.key, id, &placed.directory, &placed.root);
    if let Some(model) = summary["current_model_id"].as_str() {
        t.model = Model::new("xai", model);
    }
    let mut first_text: Option<String> = None;
    let mut after_reasoning = false;
    // Chat history lines carry no timestamps: they are spread one millisecond apart from the session start.
    for (index, record) in records.iter().enumerate() {
        let ts = created + index as u64;
        let kind = record["type"].as_str().or(record["role"].as_str()).unwrap_or_default();
        match kind {
            "user" if record.get("synthetic_reason").is_none() => {
                let text = text_of(&record["content"]);
                if !text.trim().is_empty() {
                    first_text.get_or_insert_with(|| text.clone());
                    t.user(ts, vec![convert::text(&text)]);
                }
                after_reasoning = false;
            }
            "reasoning" => {
                let text = record["summary"].as_array().into_iter().flatten().filter_map(|s| s["text"].as_str()).collect::<Vec<_>>().join("\n\n");
                let index = t.assistant(ts, !after_reasoning);
                if !text.trim().is_empty() {
                    t.push(index, convert::reasoning(&text, ts, Some(ts)));
                }
                after_reasoning = true;
            }
            "assistant" => {
                let index = t.assistant(ts, !after_reasoning);
                after_reasoning = false;
                if let Some(model) = record["model_id"].as_str() {
                    t.set_model(index, &Model::new("xai", model));
                }
                let text = text_of(&record["content"]);
                if !text.trim().is_empty() {
                    t.push(index, convert::text(&text));
                }
                for call in record["tool_calls"].as_array().into_iter().flatten() {
                    t.tool_call(ts, call["id"].as_str().unwrap_or_default(), call_name(call), convert::input_map(call_arguments(call)), None);
                }
                t.close();
            }
            "tool_result" | "tool" => {
                t.tool_result(record["tool_call_id"].as_str().unwrap_or_default(), ts, &text_of(&record["content"]), false, None);
            }
            "backend_tool_call" => {
                let name = record.pointer("/kind/tool_type").and_then(Value::as_str).unwrap_or("backend_tool");
                let call = format!("backend-{index}");
                let input = record["kind"].as_object().cloned().unwrap_or_default();
                t.tool_call(ts, &call, name, input, None);
                t.tool_result(&call, ts, "", false, None);
            }
            _ => {}
        }
    }
    if t.is_empty() {
        return Err(super::EMPTY.into());
    }
    let title = summary["generated_title"].as_str().or(summary["session_summary"].as_str()).filter(|t| !t.is_empty()).map(str::to_string);
    let title = title.or_else(|| first_text.as_deref().map(title_of)).unwrap_or_else(|| "Grok Build session".into());
    let mut row = session(id, &placed, title, created, updated, Map::new());
    row.parent_id = summary["parent_session_id"].as_str().map(|p| ids::session(ID, p));
    let fallback = t.model.clone();
    let messages = t.finish(&fallback);
    Ok(Converted { session: row, messages, todos: None, project: None, placed: Some(placed) })
}
