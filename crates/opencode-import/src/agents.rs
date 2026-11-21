// Agent definitions of other tools to opencode markdown agents (`agents/<name>.md`: frontmatter + prompt body).
use opencode_core::config::markdown;
use serde_json::{Map, Value};

use crate::target::safe_name;

#[derive(Debug, Clone, PartialEq)]
pub struct Agent {
    pub name: String,
    pub file: String,
    pub content: String,
    pub flags: Vec<String>,
}

// Claude Code tool names (as in `tools:` frontmatter) to opencode permission keys; write tools are gated by `edit`.
const CLAUDE_TOOLS: [(&str, &str); 13] = [
    ("Read", "read"),
    ("Write", "edit"),
    ("Edit", "edit"),
    ("MultiEdit", "edit"),
    ("NotebookEdit", "edit"),
    ("Bash", "bash"),
    ("Grep", "grep"),
    ("Glob", "glob"),
    ("WebFetch", "webfetch"),
    ("WebSearch", "websearch"),
    ("TodoWrite", "todowrite"),
    ("Task", "task"),
    ("Skill", "skill"),
];

// Grok Build tool ids (`ToolId::new`) and the opencode-harness names it also knows.
const GROK_TOOLS: [(&str, &str); 15] = [
    ("run_terminal_cmd", "bash"),
    ("read_file", "read"),
    ("search_replace", "edit"),
    ("grep", "grep"),
    ("todo_write", "todowrite"),
    ("task", "task"),
    ("web_fetch", "webfetch"),
    ("web_search", "websearch"),
    ("bash", "bash"),
    ("edit", "edit"),
    ("write", "edit"),
    ("read", "read"),
    ("glob", "glob"),
    ("skill", "skill"),
    ("todowrite", "todowrite"),
];

const COLORS: [(&str, &str); 9] = [
    ("red", "#ef4444"),
    ("orange", "#f97316"),
    ("yellow", "#eab308"),
    ("green", "#22c55e"),
    ("blue", "#3b82f6"),
    ("purple", "#a855f7"),
    ("pink", "#ec4899"),
    ("cyan", "#06b6d4"),
    ("gray", "#6b7280"),
];

const THEME_COLORS: [&str; 7] = ["primary", "secondary", "accent", "success", "warning", "error", "info"];

// Splits on commas and whitespace outside parentheses, so `Bash(git *)` stays one entry.
fn split(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    for c in text.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth == 0 && (c == ',' || c.is_whitespace()) {
            if !current.trim().is_empty() {
                out.push(current.trim().to_string());
            }
            current.clear();
        } else {
            current.push(c);
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

fn list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(text)) => split(text),
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        _ => Vec::new(),
    }
}

// `Tool(pattern)` and `Prefix:tool` forms reduce to the tool name.
fn bare(tool: &str) -> &str {
    let tool = tool.split('(').next().unwrap_or(tool).trim();
    tool.rsplit(':').next().unwrap_or(tool)
}

fn permissions(allowed: &[String], denied: &[String], table: &[(&str, &str)], flags: &mut Vec<String>) -> Option<Value> {
    let map = |tools: &[String], dropped: &mut Vec<String>| -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for tool in tools {
            match table.iter().find(|(from, _)| *from == bare(tool)) {
                Some((_, to)) if !out.iter().any(|t| t == to) => out.push(to.to_string()),
                Some(_) => {}
                None => dropped.push(tool.clone()),
            }
        }
        out
    };
    let mut dropped = Vec::new();
    let allow = map(allowed, &mut dropped);
    let deny = map(denied, &mut dropped);
    if !dropped.is_empty() {
        flags.push(format!("dropped tools: {}", dropped.join(", ")));
    }
    let patterned: Vec<&str> = allowed.iter().chain(denied).filter(|t| t.contains('(') && !dropped.contains(t)).map(String::as_str).collect();
    if !patterned.is_empty() {
        flags.push(format!("tool patterns not kept: {}", patterned.join(", ")));
    }
    if allow.is_empty() && deny.is_empty() {
        if !allowed.is_empty() {
            flags.push("no listed tool maps to opencode; all tools stay allowed".into());
        }
        return None;
    }
    let mut permission = Map::new();
    if !allow.is_empty() {
        permission.insert("*".into(), "deny".into());
        for tool in allow {
            permission.insert(tool, "allow".into());
        }
    }
    for tool in deny {
        permission.insert(tool, "deny".into());
    }
    Some(Value::Object(permission))
}

fn color(value: Option<&Value>, flags: &mut Vec<String>) -> Option<Value> {
    let name = value?.as_str()?.trim().to_lowercase();
    let is_hex = name.len() == 7 && name.starts_with('#') && name[1..].chars().all(|c| c.is_ascii_hexdigit());
    if is_hex || THEME_COLORS.contains(&name.as_str()) {
        return Some(name.into());
    }
    match COLORS.iter().find(|(n, _)| *n == name) {
        Some((_, hex)) => Some((*hex).into()),
        None => {
            flags.push(format!("dropped color {name}"));
            None
        }
    }
}

fn render(name: &str, frontmatter: Map<String, Value>, prompt: &str, flags: Vec<String>) -> Agent {
    let yaml = serde_yaml_ng::to_string(&Value::Object(frontmatter)).unwrap_or_default();
    let content = format!("---\n{yaml}---\n{}\n", prompt.trim());
    let file = format!("{}.md", safe_name(name));
    Agent { name: safe_name(name), file, content, flags }
}

fn description(value: Option<&Value>, name: &str) -> Value {
    value.and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map_or_else(|| Value::String(format!("Imported agent {name}")), |s| s.into())
}

fn model(value: Option<&Value>, provider: &str, aliases: &[&str], flags: &mut Vec<String>) -> Option<Value> {
    let model = value?.as_str()?.trim();
    if model.is_empty() || model == "inherit" {
        return None;
    }
    if model.contains('/') {
        return Some(model.into());
    }
    if aliases.contains(&model) {
        flags.push(format!("dropped model alias {model}"));
        return None;
    }
    Some(format!("{provider}/{model}").into())
}

fn leftover(data: &Map<String, Value>, known: &[&str], flags: &mut Vec<String>) {
    let extra: Vec<&str> = data.keys().map(String::as_str).filter(|k| !known.contains(k)).collect();
    if !extra.is_empty() {
        flags.push(format!("dropped settings: {}", extra.join(", ")));
    }
}

// Claude Code `~/.claude/agents/<name>.md`: `name`, `description`, `tools`, `model`, `color` and a prompt body.
pub fn from_claude(stem: &str, text: &str) -> Result<Agent, String> {
    let md = markdown::parse(text)?;
    let name = md.data.get("name").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).unwrap_or(stem).to_string();
    let mut flags = Vec::new();
    let mut front = Map::new();
    front.insert("description".into(), description(md.data.get("description"), &name));
    front.insert("mode".into(), "subagent".into());
    if let Some(model) = model(md.data.get("model"), "anthropic", &["sonnet", "opus", "haiku", "opusplan"], &mut flags) {
        front.insert("model".into(), model);
    }
    if let Some(color) = color(md.data.get("color"), &mut flags) {
        front.insert("color".into(), color);
    }
    if let Some(permission) = permissions(&list(md.data.get("tools")), &list(md.data.get("disallowedTools")), &CLAUDE_TOOLS, &mut flags) {
        front.insert("permission".into(), permission);
    }
    leftover(&md.data, &["name", "description", "tools", "disallowedTools", "model", "color"], &mut flags);
    Ok(render(&name, front, &md.content, flags))
}

// Grok Build `agents/<name>.md` (`AgentDefinition`, camelCase): `name`, `description`, `tools`, `disallowedTools`,
// `model` (`inherit` or an id), `color`; the body is the system prompt.
pub fn from_grok(stem: &str, text: &str) -> Result<Agent, String> {
    let md = markdown::parse(text)?;
    let name = md.data.get("name").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).unwrap_or(stem).to_string();
    let mut flags = Vec::new();
    let mut front = Map::new();
    front.insert("description".into(), description(md.data.get("description"), &name));
    front.insert("mode".into(), "subagent".into());
    if let Some(model) = model(md.data.get("model"), "xai", &[], &mut flags) {
        front.insert("model".into(), model);
    }
    if let Some(color) = color(md.data.get("color"), &mut flags) {
        front.insert("color".into(), color);
    }
    if let Some(permission) = permissions(&list(md.data.get("tools")), &list(md.data.get("disallowedTools")), &GROK_TOOLS, &mut flags) {
        front.insert("permission".into(), permission);
    }
    leftover(&md.data, &["name", "description", "tools", "disallowedTools", "model", "color"], &mut flags);
    Ok(render(&name, front, &md.content, flags))
}

// Codex `~/.codex/agents/<name>.toml`: `name`, `description`, `developer_instructions` (text or `{prompt}`), `model`,
// `model_reasoning_effort`, `[tools]` and `[permissions]` switches.
pub fn from_codex(stem: &str, table: &Value) -> Result<Agent, String> {
    let data = table.as_object().ok_or("not a table")?;
    let name = data.get("name").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).unwrap_or(stem).to_string();
    let prompt = match data.get("developer_instructions") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Object(inner)) => inner.get("prompt").and_then(Value::as_str).unwrap_or_default().to_string(),
        _ => String::new(),
    };
    let mut flags = Vec::new();
    let mut front = Map::new();
    front.insert("description".into(), description(data.get("description"), &name));
    front.insert("mode".into(), "subagent".into());
    if let Some(model) = model(data.get("model"), "openai", &[], &mut flags) {
        front.insert("model".into(), model);
    }
    if let Some(effort) = data.get("model_reasoning_effort").and_then(Value::as_str) {
        front.insert("variant".into(), effort.into());
    }
    let mut permission = Map::new();
    let switches = [("tools", "bash", "bash"), ("tools", "file_read", "read"), ("tools", "file_write", "edit"), ("permissions", "allow_exec", "bash"), ("permissions", "workspace_write", "edit"), ("permissions", "network_access", "webfetch")];
    let mut dropped = Vec::new();
    for (group, map) in ["tools", "permissions"].iter().filter_map(|g| data.get(*g).and_then(Value::as_object).map(|m| (*g, m))) {
        for (key, value) in map {
            let Some(on) = value.as_bool() else { continue };
            match switches.iter().find(|(g, k, _)| *g == group && k == key) {
                Some((_, _, to)) => {
                    let action = if on { "allow" } else { "deny" };
                    if permission.get(*to).and_then(Value::as_str) != Some("deny") {
                        permission.insert(to.to_string(), action.into());
                    }
                }
                None => dropped.push(format!("{group}.{key}")),
            }
        }
    }
    if !dropped.is_empty() {
        flags.push(format!("dropped tools: {}", dropped.join(", ")));
    }
    if !permission.is_empty() {
        front.insert("permission".into(), Value::Object(permission));
    }
    leftover(data, &["name", "description", "developer_instructions", "model", "model_reasoning_effort", "tools", "permissions"], &mut flags);
    if prompt.trim().is_empty() {
        flags.push("no developer_instructions; the agent has an empty prompt".into());
    }
    Ok(render(&name, front, &prompt, flags))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn front(agent: &Agent) -> Map<String, Value> {
        markdown::parse(&agent.content).unwrap().data
    }

    #[test]
    fn claude_agent() {
        let text = "---\nname: code-reviewer\ndescription: Reviews code: carefully\ntools: Read, Grep, Glob, Bash(git *), mcp__x__y\nmodel: sonnet\ncolor: red\n---\n\nYou review.\n";
        let agent = from_claude("file", text).unwrap();
        assert_eq!((agent.name.as_str(), agent.file.as_str()), ("code-reviewer", "code-reviewer.md"));
        let data = front(&agent);
        assert_eq!(Value::Object(data), json!({"description": "Reviews code: carefully", "mode": "subagent", "color": "#ef4444", "permission": {"*": "deny", "read": "allow", "grep": "allow", "glob": "allow", "bash": "allow"}}));
        assert_eq!(markdown::parse(&agent.content).unwrap().content.trim(), "You review.");
        assert_eq!(agent.flags, ["dropped model alias sonnet", "dropped tools: mcp__x__y", "tool patterns not kept: Bash(git *)"]);
        let plain = from_claude("helper", "---\ndescription: d\nmodel: claude-opus-4-1\n---\nGo").unwrap();
        assert_eq!(front(&plain)["model"], "anthropic/claude-opus-4-1");
        assert!(plain.flags.is_empty() && !front(&plain).contains_key("permission"));
    }

    #[test]
    fn codex_and_grok_agents() {
        let table = json!({"name": "rev", "description": "R", "developer_instructions": {"prompt": "Check."}, "model": "gpt-5", "model_reasoning_effort": "high", "sandbox_mode": "read-only", "tools": {"bash": true, "file_write": false, "playwright_mcp": true}, "permissions": {"network_access": false}});
        let agent = from_codex("x", &table).unwrap();
        let data = front(&agent);
        assert_eq!(Value::Object(data), json!({"description": "R", "mode": "subagent", "model": "openai/gpt-5", "variant": "high", "permission": {"bash": "allow", "edit": "deny", "webfetch": "deny"}}));
        assert_eq!(agent.flags, ["dropped tools: tools.playwright_mcp", "dropped settings: sandbox_mode"]);
        let grok = from_grok("g", "---\ndescription: G\ntools: [GrokBuild:read_file, run_terminal_cmd]\ndisallowedTools: web_fetch\nmodel: inherit\n---\nBody").unwrap();
        assert_eq!(front(&grok)["permission"], json!({"*": "deny", "read": "allow", "bash": "allow", "webfetch": "deny"}));
        assert_eq!(grok.name, "g");
    }
}
