// Codex: `~/.codex/config.toml` (`[mcp_servers.*]`), `~/.codex/agents/*.toml`, `~/.codex/skills/<name>/SKILL.md`, and
// rollouts `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` + `~/.codex/archived_sessions/*.jsonl` (titles from
// `session_index.jsonl`). Rollout lines are `{timestamp, type, payload, ordinal}`.
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use opencode_core::glob;
use serde_json::{Map, Value, json};

use super::mcpjson::mcp_item;
use super::{Converted, Projects, SessionKind, SessionSource, first_line, jsonl, session, session_item, title_of};
use crate::convert::{self, Model, Transcript};
use crate::ids;
use crate::{Category, Ctx, Item, Payload, SourceReport, agents, mcp, skills};

const ID: &str = "codex";

// Context Codex injects as user messages; dropped from imported transcripts.
const INJECTED: [&str; 14] = [
    "<environment_context>",
    "<user_instructions>",
    "# AGENTS.md instructions",
    "<recommended_plugins>",
    "<guardian_tool_descriptions>",
    "<turn_aborted>",
    "<heartbeat>",
    "<node_repl_review_evidence>",
    "<external_codex_apps_open_page>",
    "<realtime_delegation>",
    "<task-notification>",
    "<app-context>",
    "<skills_instructions>",
    "<permissions instructions>",
];

fn read_toml(file: &Path, report: &mut SourceReport) -> Option<Value> {
    let text = std::fs::read_to_string(file).ok()?;
    match text.parse::<toml::Table>() {
        Ok(table) => Some(mcp::toml_to_json(toml::Value::Table(table))),
        Err(error) => {
            report.warnings.push(format!("{}: {error}", file.display()));
            None
        }
    }
}

fn titles(root: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Ok((rows, _)) = super::jsonl(&root.join("session_index.jsonl")) {
        for row in rows {
            if let (Some(id), Some(name)) = (row["id"].as_str(), row["thread_name"].as_str()) {
                out.insert(id.to_string(), name.to_string());
            }
        }
    }
    out
}

fn rollouts(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for (dir, pattern) in [(root.join("sessions"), "**/rollout-*.jsonl"), (root.join("archived_sessions"), "*.jsonl")] {
        if dir.is_dir() {
            files.extend(glob::scan(pattern, &dir, glob::Options { dot: false, follow: true }));
        }
    }
    files.sort();
    files
}

pub fn scan(ctx: &Ctx) -> SourceReport {
    let mut report = SourceReport::new(ID);
    let root = ctx.home.join(".codex");
    let config = root.join("config.toml");
    if report.locate(&config)
        && let Some(value) = read_toml(&config, &mut report)
    {
        for (name, server) in value.get("mcp_servers").and_then(Value::as_object).into_iter().flatten() {
            match mcp::from_codex(server) {
                Ok(converted) => report.items.push(mcp_item(ctx, name, converted, "~/.codex/config.toml")),
                Err(error) => report.warnings.push(format!("MCP server {name}: {error}")),
            }
        }
    }
    let agents_dir = root.join("agents");
    if report.locate(&agents_dir) {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&agents_dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "toml")).collect();
        files.sort();
        for file in files {
            let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let Some(table) = read_toml(&file, &mut report) else { continue };
            match agents::from_codex(&stem, &table) {
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
    if report.locate(&root.join("skills")) {
        report.items.extend(skills::items(&root.join("skills"), &ctx.visible));
    }
    if root.join("AGENTS.md").is_file() {
        report.found = true;
        report.warnings.push(format!("{} is not imported (instruction files are not an import category)", root.join("AGENTS.md").display()));
    }
    let names = titles(&root);
    for file in rollouts(&root) {
        report.found = true;
        let Some(meta) = first_line(&file).filter(|r| r["type"] == "session_meta") else {
            report.warnings.push(format!("{}: no session_meta line, skipped", file.display()));
            continue;
        };
        let Some(key) = meta["payload"]["id"].as_str().map(str::to_string) else { continue };
        let title = names.get(&key).cloned();
        let cwd = meta["payload"]["cwd"].as_str().unwrap_or_default().to_string();
        let source = SessionSource { key: key.clone(), updated: ids::file_mtime_ms(&file), kind: SessionKind::Codex { file, title: title.clone() } };
        report.items.push(session_item(title.unwrap_or(key), cwd, source));
    }
    report
}

fn injected(text: &str) -> bool {
    let text = text.trim_start();
    INJECTED.iter().any(|tag| text.starts_with(tag))
}

fn output_text(output: &Value) -> String {
    match output {
        Value::String(text) => text.clone(),
        Value::Array(items) => items.iter().filter_map(|i| i["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

fn reasoning_text(payload: &Value) -> String {
    let collect = |key: &str| payload[key].as_array().into_iter().flatten().filter_map(|i| i["text"].as_str()).collect::<Vec<_>>().join("\n\n");
    let summary = collect("summary");
    if summary.trim().is_empty() { collect("content") } else { summary }
}

fn usage(info: &Value) -> Option<opencode_storage::Tokens> {
    let usage = info.get("last_token_usage")?;
    let n = |key: &str| usage[key].as_f64().unwrap_or(0.0);
    let cached = n("cached_input_tokens");
    let reasoning = n("reasoning_output_tokens");
    Some(convert::tokens((n("input_tokens") - cached).max(0.0), (n("output_tokens") - reasoning).max(0.0), reasoning, cached, n("cache_write_input_tokens")))
}

pub fn convert(file: &Path, item: &SessionSource, id: &str, title: Option<String>, projects: &mut Projects) -> Result<Converted, String> {
    let (records, _) = jsonl(file).map_err(|e| e.to_string())?;
    let meta = records.first().filter(|r| r["type"] == "session_meta").map(|r| r["payload"].clone()).ok_or("no session_meta line")?;
    let provider = meta["model_provider"].as_str().filter(|p| !p.is_empty()).unwrap_or("openai").to_string();
    let history_start = meta["subagent_history_start_ordinal"].as_u64();
    let placed = projects.place(meta["cwd"].as_str().unwrap_or("/"));
    let created = meta["timestamp"].as_str().and_then(ids::iso_ms);
    let mut t = Transcript::new(ID, &item.key, id, &placed.directory, &placed.root);
    let mut first_text: Option<String> = None;
    let mut ts = created.unwrap_or(0);
    for record in &records {
        if let Some(at) = record["timestamp"].as_str().and_then(ids::iso_ms) {
            ts = at;
        }
        if let (Some(start), Some(ordinal)) = (history_start, record["ordinal"].as_u64())
            && ordinal < start
        {
            continue;
        }
        let payload = &record["payload"];
        match (record["type"].as_str().unwrap_or_default(), payload["type"].as_str().unwrap_or_default()) {
            ("turn_context", _) => {
                if let Some(model) = payload["model"].as_str() {
                    t.model = Model::new(&provider, model);
                }
            }
            ("response_item", "message") => {
                let texts: Vec<&str> = payload["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect();
                match payload["role"].as_str() {
                    Some("user") => {
                        let real: Vec<&str> = texts.into_iter().filter(|t| !t.trim().is_empty() && !injected(t)).collect();
                        if !real.is_empty() {
                            first_text.get_or_insert_with(|| real[0].to_string());
                            t.user(ts, real.into_iter().map(convert::text).collect());
                        }
                    }
                    Some("assistant") => {
                        let index = t.assistant(ts, false);
                        for text in texts {
                            t.push(index, convert::text(text));
                        }
                        t.touch(index, ts);
                    }
                    _ => {}
                }
            }
            ("response_item", "reasoning") => {
                let text = reasoning_text(payload);
                if !text.trim().is_empty() {
                    let index = t.assistant(ts, false);
                    t.push(index, convert::reasoning(&text, ts, Some(ts)));
                }
            }
            ("response_item", "function_call") => {
                let metadata = payload["namespace"].as_str().map(|ns| Map::from_iter([("namespace".to_string(), json!(ns))]));
                t.tool_call(ts, payload["call_id"].as_str().unwrap_or_default(), payload["name"].as_str().unwrap_or_default(), convert::input_map(&payload["arguments"]), metadata);
            }
            ("response_item", "custom_tool_call") => {
                let input = Map::from_iter([("input".to_string(), payload["input"].clone())]);
                t.tool_call(ts, payload["call_id"].as_str().unwrap_or_default(), payload["name"].as_str().unwrap_or_default(), input, None);
            }
            ("response_item", "function_call_output" | "custom_tool_call_output") => {
                t.tool_result(payload["call_id"].as_str().unwrap_or_default(), ts, &output_text(&payload["output"]), false, None);
            }
            ("response_item", "web_search_call") => {
                let call = payload["id"].as_str().unwrap_or_default().to_string();
                t.tool_call(ts, &call, "web_search", convert::input_map(&payload["action"]), None);
                t.tool_result(&call, ts, "", payload["status"] == "failed", None);
            }
            ("compacted", _) => {
                t.compaction(ts, true, payload["message"].as_str().unwrap_or_default());
            }
            ("event_msg", "token_count") => {
                if let (Some(index), Some(tokens)) = (t.current(), usage(&payload["info"])) {
                    t.set_usage(index, tokens, 0.0);
                    t.touch(index, ts);
                }
                t.close();
            }
            ("event_msg", "task_complete" | "turn_aborted") => t.close(),
            _ => {}
        }
    }
    if t.is_empty() {
        return Err(super::EMPTY.into());
    }
    let created = created.unwrap_or(item.updated);
    let title = title.or_else(|| first_text.as_deref().map(title_of)).unwrap_or_else(|| "Codex session".into());
    let mut row = session(id, &placed, title, created, ts.max(created), Map::new());
    row.parent_id = meta["parent_thread_id"].as_str().map(|p| ids::session(ID, p));
    let fallback = t.model.clone();
    let messages = t.finish(&if fallback.id.is_empty() { Model::new(&provider, "unknown") } else { fallback });
    Ok(Converted { session: row, messages, todos: None, project: None, placed: Some(placed) })
}
