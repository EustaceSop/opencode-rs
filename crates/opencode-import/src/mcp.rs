// MCP server definitions of other tools to opencode `mcp` entries (`{type:"local",command,environment}` /
// `{type:"remote",url,headers}`). Env and header values are copied as-is; their key names are flagged when they look secret.
use serde_json::{Map, Value, json};

pub const SECRET_FLAG: &str = "secrets copied as-is";

#[derive(Debug, Clone, PartialEq)]
pub struct Converted {
    pub entry: Value,
    pub flags: Vec<String>,
}

fn sensitive(key: &str) -> bool {
    let key = key.to_lowercase();
    ["key", "token", "secret", "passw", "auth", "bearer", "credential", "cookie", "session", "pat"].iter().any(|s| key.contains(s))
}

fn string_map(value: Option<&Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for (k, v) in value.and_then(Value::as_object).into_iter().flatten() {
        let text = match v {
            Value::String(s) => s.clone(),
            Value::Null => continue,
            other => other.to_string(),
        };
        out.insert(k.clone(), Value::String(text));
    }
    out
}

fn strings(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items.iter().filter_map(|v| v.as_str().map(str::to_string).or_else(|| (!v.is_null()).then(|| v.to_string()))).collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

fn text<'a>(server: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| server.get(*k).and_then(Value::as_str)).map(str::trim).filter(|s| !s.is_empty())
}

#[derive(Default)]
struct Spec {
    command: Option<String>,
    args: Vec<String>,
    env: Map<String, Value>,
    cwd: Option<String>,
    url: Option<String>,
    headers: Map<String, Value>,
    enabled: Option<bool>,
    timeout: Option<u64>,
    dropped: Vec<String>,
}

impl Spec {
    fn build(self) -> Result<Converted, String> {
        let mut flags = Vec::new();
        let mut secret: Vec<String> = Vec::new();
        for (kind, map) in [("env", &self.env), ("header", &self.headers)] {
            for (key, value) in map {
                let literal = value.as_str().is_some_and(|v| !v.contains("{env:"));
                if literal && (sensitive(key) || value.as_str().is_some_and(|v| v.len() >= 24 && !v.contains([' ', '/', '\\', ':']))) {
                    secret.push(format!("{kind} {key}"));
                }
            }
        }
        if !secret.is_empty() {
            flags.push(format!("{SECRET_FLAG}: {}", secret.join(", ")));
        }
        let values = self.env.values().chain(self.headers.values()).filter_map(Value::as_str);
        if values.chain(self.args.iter().map(String::as_str)).any(|v| v.contains("${")) {
            flags.push("contains ${...} placeholders, which opencode does not expand (opencode uses {env:NAME})".into());
        }
        if !self.dropped.is_empty() {
            flags.push(format!("dropped settings: {}", self.dropped.join(", ")));
        }
        let mut entry = match (self.command, self.url) {
            (Some(command), _) => {
                let mut entry = json!({"type": "local", "command": std::iter::once(command).chain(self.args).collect::<Vec<_>>()});
                if let Some(cwd) = self.cwd {
                    entry["cwd"] = cwd.into();
                }
                if !self.env.is_empty() {
                    entry["environment"] = Value::Object(self.env);
                }
                entry
            }
            (None, Some(url)) => {
                let mut entry = json!({"type": "remote", "url": url});
                if !self.headers.is_empty() {
                    entry["headers"] = Value::Object(self.headers);
                }
                entry
            }
            (None, None) => return Err("neither a command nor a url".into()),
        };
        if let Some(enabled) = self.enabled {
            entry["enabled"] = enabled.into();
        }
        if let Some(timeout) = self.timeout.filter(|t| *t > 0) {
            entry["timeout"] = timeout.into();
        }
        Ok(Converted { entry, flags })
    }
}

fn dropped(server: &Map<String, Value>, known: &[&str]) -> Vec<String> {
    server.keys().filter(|k| !known.contains(&k.as_str())).cloned().collect()
}

fn object(server: &Value) -> Result<&Map<String, Value>, String> {
    server.as_object().ok_or_else(|| "not an object".to_string())
}

// Claude Code `~/.claude.json`, Cursor and Windsurf `mcp.json`, `~/.mcp.json`: `{type?, command, args, env}` or
// `{type: "http"|"sse", url, headers}`.
pub fn from_json(server: &Value) -> Result<Converted, String> {
    let server = object(server)?;
    let kind = server.get("type").and_then(Value::as_str).unwrap_or_default();
    let remote = matches!(kind, "http" | "sse" | "streamable-http") || (server.get("command").is_none() && server.get("url").is_some());
    let mut spec = Spec { dropped: dropped(server, &["type", "command", "args", "env", "cwd", "url", "headers"]), ..Default::default() };
    if remote {
        spec.url = text(server, &["url"]).map(str::to_string);
        spec.headers = string_map(server.get("headers"));
    } else {
        spec.command = text(server, &["command"]).map(str::to_string);
        spec.args = strings(server.get("args"));
        spec.env = string_map(server.get("env"));
        spec.cwd = text(server, &["cwd"]).map(str::to_string);
    }
    spec.build()
}

fn forwarded(names: Vec<String>, env: &mut Map<String, Value>) {
    for name in names {
        env.entry(name.clone()).or_insert_with(|| Value::String(format!("{{env:{name}}}")));
    }
}

fn bearer(server: &Map<String, Value>, headers: &mut Map<String, Value>) {
    if let Some(var) = text(server, &["bearer_token_env_var"]) {
        headers.entry("Authorization".to_string()).or_insert_with(|| Value::String(format!("Bearer {{env:{var}}}")));
    }
}

fn seconds(server: &Map<String, Value>, key: &str) -> Option<u64> {
    server.get(key).and_then(Value::as_f64).filter(|s| *s > 0.0).map(|s| (s * 1000.0) as u64)
}

// Codex `[mcp_servers.<name>]`: `command`, `args`, `env`, `env_vars` (names forwarded from the environment), `url`,
// `enabled`, `startup_timeout_sec`; `cwd`, `http_headers` and `bearer_token_env_var` are taken when present.
pub fn from_codex(server: &Value) -> Result<Converted, String> {
    let server = object(server)?;
    let known = ["command", "args", "env", "env_vars", "cwd", "url", "http_headers", "bearer_token_env_var", "enabled", "startup_timeout_sec"];
    let mut spec = Spec { dropped: dropped(server, &known), enabled: server.get("enabled").and_then(Value::as_bool), timeout: seconds(server, "startup_timeout_sec"), ..Default::default() };
    if let Some(command) = text(server, &["command"]) {
        spec.command = Some(command.to_string());
        spec.args = strings(server.get("args"));
        spec.env = string_map(server.get("env"));
        forwarded(strings(server.get("env_vars")), &mut spec.env);
        spec.cwd = text(server, &["cwd"]).map(str::to_string);
    } else {
        spec.url = text(server, &["url"]).map(str::to_string);
        spec.headers = string_map(server.get("http_headers"));
        bearer(server, &mut spec.headers);
    }
    spec.build()
}

// Grok Build `[mcp_servers.<name>]` (`McpServerConfig`): stdio `command/args/env/cwd`, or `url` (aliases `urlTemplate`,
// `url_template`) with `type`, `headers`, `bearer_token_env_var`; `enabled`, `startup_timeout_sec`.
pub fn from_grok(server: &Value) -> Result<Converted, String> {
    let server = object(server)?;
    let known = ["command", "args", "env", "cwd", "url", "urlTemplate", "url_template", "type", "headers", "bearer_token_env_var", "enabled", "startup_timeout_sec"];
    let mut spec = Spec { dropped: dropped(server, &known), enabled: server.get("enabled").and_then(Value::as_bool), timeout: seconds(server, "startup_timeout_sec"), ..Default::default() };
    if let Some(command) = text(server, &["command"]) {
        spec.command = Some(command.to_string());
        spec.args = strings(server.get("args"));
        spec.env = string_map(server.get("env"));
        spec.cwd = text(server, &["cwd"]).map(str::to_string);
    } else {
        spec.url = text(server, &["url", "urlTemplate", "url_template"]).map(str::to_string);
        spec.headers = string_map(server.get("headers"));
        bearer(server, &mut spec.headers);
    }
    spec.build()
}

// DeepSeek Harness `McpManagedServerEntry` / `dsh-mcp-client` config: `transport` `stdio` | `streamable-http`.
pub fn from_dsh(server: &Value) -> Result<Converted, String> {
    let server = object(server)?;
    let known = ["transport", "serverName", "command", "args", "env", "cwd", "url", "headers", "enabled", "startupTimeoutMs"];
    let timeout = server.get("startupTimeoutMs").and_then(Value::as_u64);
    let mut spec = Spec { dropped: dropped(server, &known), enabled: server.get("enabled").and_then(Value::as_bool), timeout, ..Default::default() };
    match server.get("transport").and_then(Value::as_str) {
        Some("stdio") => {
            spec.command = text(server, &["command"]).map(str::to_string);
            spec.args = strings(server.get("args"));
            spec.env = string_map(server.get("env"));
            spec.cwd = text(server, &["cwd"]).map(str::to_string);
        }
        Some("streamable-http") => {
            spec.url = text(server, &["url"]).map(str::to_string);
            spec.headers = string_map(server.get("headers"));
        }
        other => return Err(format!("unsupported transport {}", other.unwrap_or("(none)"))),
    }
    let mut converted = spec.build()?;
    if converted.entry.to_string().contains("${credential:") {
        converted.flags.push("uses DeepSeek Harness ${credential:...} templates, which opencode cannot resolve".into());
    }
    Ok(converted)
}

pub fn toml_to_json(value: toml::Value) -> Value {
    match value {
        toml::Value::String(s) => Value::String(s),
        toml::Value::Integer(i) => Value::Number(i.into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number),
        toml::Value::Boolean(b) => Value::Bool(b),
        toml::Value::Datetime(d) => Value::String(d.to_string()),
        toml::Value::Array(items) => Value::Array(items.into_iter().map(toml_to_json).collect()),
        toml::Value::Table(table) => Value::Object(table.into_iter().map(|(k, v)| (k, toml_to_json(v))).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_servers() {
        let local = from_json(&json!({"type": "stdio", "command": "npx", "args": ["-y", "pkg"], "env": {"API_KEY": "abc", "MODE": "x"}})).unwrap();
        assert_eq!(local.entry, json!({"type": "local", "command": ["npx", "-y", "pkg"], "environment": {"API_KEY": "abc", "MODE": "x"}}));
        assert_eq!(local.flags, ["secrets copied as-is: env API_KEY"]);
        let remote = from_json(&json!({"type": "http", "url": "https://h/mcp", "headers": {"Authorization": "Bearer t"}})).unwrap();
        assert_eq!(remote.entry, json!({"type": "remote", "url": "https://h/mcp", "headers": {"Authorization": "Bearer t"}}));
        assert!(remote.flags[0].contains("header Authorization"));
        assert_eq!(from_json(&json!({"url": "https://c/sse"})).unwrap().entry, json!({"type": "remote", "url": "https://c/sse"}));
        let bare = from_json(&json!({"command": "srv", "alwaysAllow": ["x"]})).unwrap();
        assert_eq!((bare.entry, bare.flags), (json!({"type": "local", "command": ["srv"]}), vec!["dropped settings: alwaysAllow".to_string()]));
        assert!(from_json(&json!({"type": "stdio"})).is_err());
        let placeholder = from_json(&json!({"command": "s", "env": {"HOME_DIR": "${HOME}"}})).unwrap();
        assert!(placeholder.flags.iter().any(|f| f.contains("placeholders")));
    }

    #[test]
    fn codex_grok_and_dsh_servers() {
        let codex = from_codex(&json!({"command": "uvx", "args": ["srv"], "env": {"A": "1"}, "env_vars": ["TOKEN"], "startup_timeout_sec": 20, "tools": {"t": {}}})).unwrap();
        assert_eq!(codex.entry, json!({"type": "local", "command": ["uvx", "srv"], "environment": {"A": "1", "TOKEN": "{env:TOKEN}"}, "timeout": 20000}));
        assert_eq!(codex.flags, ["dropped settings: tools"]);
        let remote = from_codex(&json!({"url": "https://h/mcp", "enabled": false, "bearer_token_env_var": "GH"})).unwrap();
        assert_eq!(remote.entry, json!({"type": "remote", "url": "https://h/mcp", "headers": {"Authorization": "Bearer {env:GH}"}, "enabled": false}));
        assert!(remote.flags.is_empty());
        let grok = from_grok(&json!({"urlTemplate": "https://g/sse", "type": "sse", "headers": {"X": "y"}})).unwrap();
        assert_eq!(grok.entry, json!({"type": "remote", "url": "https://g/sse", "headers": {"X": "y"}}));
        let dsh = from_dsh(&json!({"transport": "stdio", "command": "c", "args": ["a"], "cwd": "", "env": {}, "enabled": true, "reconnect": {}})).unwrap();
        assert_eq!(dsh.entry, json!({"type": "local", "command": ["c", "a"], "enabled": true}));
        let web = from_dsh(&json!({"transport": "streamable-http", "url": "https://d", "headers": {"Authorization": "Bearer ${credential:x}"}})).unwrap();
        assert!(web.flags.iter().any(|f| f.contains("credential")));
        assert!(from_dsh(&json!({"transport": "ws"})).is_err());
    }
}
