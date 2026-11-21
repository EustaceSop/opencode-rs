use super::ConfigError;
use regex::Regex;
use serde_json::{Map, Value};
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticKind {
    Invalid,
    Unsupported,
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub kind: DiagnosticKind,
    pub source: String,
    pub path: Vec<String>,
    pub message: String,
}

struct Ctx<'a> {
    source: &'a str,
    out: &'a mut Vec<Diagnostic>,
}

impl Ctx<'_> {
    fn push(&mut self, kind: DiagnosticKind, path: &[&str]) {
        let message = match kind {
            DiagnosticKind::Invalid => "Native setting could not be lowered because it is malformed",
            DiagnosticKind::Unsupported => "Omitted native setting that cannot be represented in V1",
            DiagnosticKind::Conflict => "Retained legacy value over native value",
        };
        self.out.push(Diagnostic {
            kind,
            source: self.source.to_string(),
            path: path.iter().map(|s| s.to_string()).collect(),
            message: message.to_string(),
        });
    }
    fn invalid(&mut self, path: &[&str]) {
        self.push(DiagnosticKind::Invalid, path);
    }
    fn unsupported(&mut self, path: &[&str]) {
        self.push(DiagnosticKind::Unsupported, path);
    }
    fn prefer_legacy(&mut self, target: &mut Map<String, Value>, key: &str, value: Value, path: &[&str]) {
        match target.get(key) {
            Some(existing) if *existing != value => self.push(DiagnosticKind::Conflict, path),
            Some(_) => {}
            None => {
                target.insert(key.to_string(), value);
            }
        }
    }
}

static SELECTION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[^/#]+/[^#]+(?:#[^#]+)?$").unwrap());
static HEX_COLOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^#[0-9a-fA-F]{6}$").unwrap());

pub(super) fn is_int_at_least(value: &Value, min: f64) -> bool {
    value.as_f64().is_some_and(|n| n.is_finite() && n.fract() == 0.0 && n >= min)
}

fn opt(map: &Map<String, Value>, key: &str, check: impl Fn(&Value) -> bool) -> bool {
    map.get(key).is_none_or(check)
}

fn string_record(value: &Value) -> bool {
    value.as_object().is_some_and(|m| m.values().all(Value::is_string))
}

fn string_array(value: &Value) -> bool {
    value.as_array().is_some_and(|a| a.iter().all(Value::is_string))
}

// "provider/model#variant" or {providerID, model, variant?} -> ("provider/model", variant).
fn lower_selection(value: &Value) -> Option<(String, Option<String>)> {
    match value {
        Value::String(s) if SELECTION.is_match(s) => Some(match s.split_once('#') {
            Some((model, variant)) => (model.to_string(), Some(variant.to_string())),
            None => (s.clone(), None),
        }),
        Value::Object(map) => {
            let provider = map.get("providerID")?.as_str().filter(|s| !s.is_empty() && !s.contains(['/', '#']))?;
            let model = map.get("model")?.as_str().filter(|s| !s.is_empty() && !s.contains('#'))?;
            let variant = match map.get("variant") {
                None => None,
                Some(v) => Some(v.as_str().filter(|s| !s.is_empty() && !s.contains('#'))?.to_string()),
            };
            Some((format!("{provider}/{model}"), variant))
        }
        _ => None,
    }
}

fn lower_timeout(timeout: &Map<String, Value>) -> Option<Value> {
    if timeout.contains_key("startup") {
        return None;
    }
    let (catalog, execution) = (timeout.get("catalog")?, timeout.get("execution")?);
    (catalog.as_f64() == execution.as_f64()).then(|| catalog.clone())
}

fn valid_timeout(value: &Value) -> bool {
    value.as_object().is_some_and(|m| {
        ["startup", "catalog", "execution"].iter().all(|k| opt(m, k, |v| is_int_at_least(v, 1.0)))
    })
}

fn timeout_fields(value: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(map) = value.as_object() {
        for (k, v) in map {
            if matches!(k.as_str(), "startup" | "catalog" | "execution") {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    out
}

fn lower_agent(agent: &Map<String, Value>, path: &[&str], ctx: &mut Ctx) -> Option<Value> {
    let valid_request = |v: &Value| {
        v.as_object().is_some_and(|r| opt(r, "headers", string_record) && opt(r, "body", Value::is_object))
    };
    let valid = opt(agent, "model", |v| lower_selection(v).is_some())
        && opt(agent, "request", valid_request)
        && opt(agent, "system", Value::is_string)
        && opt(agent, "description", Value::is_string)
        && opt(agent, "mode", |v| matches!(v.as_str(), Some("subagent" | "primary" | "all")))
        && opt(agent, "hidden", Value::is_boolean)
        && opt(agent, "color", |v| v.as_str().is_some_and(|s| HEX_COLOR.is_match(s)))
        && opt(agent, "steps", |v| is_int_at_least(v, 1.0))
        && opt(agent, "disabled", Value::is_boolean)
        && opt(agent, "permissions", valid_rules);
    if !valid {
        ctx.invalid(path);
        return None;
    }
    let request = agent.get("request").and_then(Value::as_object);
    if request.is_some_and(|r| r.contains_key("headers")) {
        let mut at = path.to_vec();
        at.extend(["request", "headers"]);
        ctx.unsupported(&at);
    }
    let mut out = Map::new();
    for key in ["description", "mode", "hidden", "color", "steps"] {
        if let Some(v) = agent.get(key) {
            out.insert(key.to_string(), v.clone());
        }
    }
    if let Some(v) = agent.get("system") {
        out.insert("prompt".into(), v.clone());
    }
    if let Some(v) = agent.get("disabled") {
        out.insert("disable".into(), v.clone());
    }
    if let Some((model, variant)) = agent.get("model").and_then(lower_selection) {
        out.insert("model".into(), Value::String(model));
        if let Some(variant) = variant {
            out.insert("variant".into(), Value::String(variant));
        }
    }
    if let Some(body) = request.and_then(|r| r.get("body")) {
        out.insert("options".into(), body.clone());
    }
    if let Some(permission) = agent.get("permissions").and_then(|rules| fold_permissions(None, rules)) {
        out.insert("permission".into(), permission);
    }
    Some(Value::Object(out))
}

fn lower_command(command: &Value, path: &[&str], ctx: &mut Ctx) -> Option<Value> {
    let map = command.as_object();
    let valid = map.is_some_and(|m| {
        m.get("template").is_some_and(Value::is_string)
            && opt(m, "description", Value::is_string)
            && opt(m, "agent", Value::is_string)
            && opt(m, "model", |v| lower_selection(v).is_some())
            && opt(m, "subtask", Value::is_boolean)
    });
    if !valid {
        ctx.invalid(path);
        return None;
    }
    let mut out = Map::new();
    for (k, v) in map? {
        if matches!(k.as_str(), "template" | "description" | "agent" | "model" | "subtask") {
            out.insert(k.clone(), v.clone());
        }
    }
    if let Some((model, variant)) = out.get("model").and_then(lower_selection) {
        out.insert("model".into(), Value::String(model));
        if let Some(variant) = variant {
            out.insert("variant".into(), Value::String(variant));
        }
    }
    Some(Value::Object(out))
}

fn valid_server(server: &Map<String, Value>) -> bool {
    let common = opt(server, "disabled", Value::is_boolean)
        && opt(server, "codemode", Value::is_boolean)
        && opt(server, "timeout", valid_timeout);
    let oauth = |v: &Value| match v {
        Value::Bool(false) => true,
        Value::Object(o) => {
            ["client_id", "client_secret", "scope", "redirect_uri"].iter().all(|k| opt(o, k, Value::is_string))
                && opt(o, "callback_port", |p| is_int_at_least(p, 1.0) && p.as_f64().is_some_and(|n| n <= 65535.0))
        }
        _ => false,
    };
    match server.get("type").and_then(Value::as_str) {
        Some("local") => {
            common
                && server.get("command").is_some_and(string_array)
                && opt(server, "cwd", Value::is_string)
                && opt(server, "environment", string_record)
        }
        Some("remote") => {
            common
                && server.get("url").is_some_and(Value::is_string)
                && opt(server, "headers", string_record)
                && opt(server, "oauth", oauth)
        }
        _ => false,
    }
}

fn lower_server(server: &Map<String, Value>) -> Map<String, Value> {
    let remote = server.get("type").and_then(Value::as_str) == Some("remote");
    let known: &[&str] = if remote {
        &["type", "url", "headers", "oauth", "disabled", "codemode", "timeout"]
    } else {
        &["type", "command", "cwd", "environment", "disabled", "codemode", "timeout"]
    };
    let mut out = Map::new();
    for (k, v) in server {
        if known.contains(&k.as_str()) && !matches!(k.as_str(), "disabled" | "codemode" | "timeout") {
            out.insert(k.clone(), v.clone());
        }
    }
    out.insert("enabled".into(), Value::Bool(server.get("disabled") != Some(&Value::Bool(true))));
    if let Some(timeout) = server.get("timeout").map(timeout_fields).as_ref().and_then(lower_timeout) {
        out.insert("timeout".into(), timeout);
    }
    if let Some(Value::Object(oauth)) = server.get("oauth").filter(|_| remote) {
        let mut lowered = Map::new();
        for (from, to) in [
            ("client_id", "clientId"),
            ("client_secret", "clientSecret"),
            ("scope", "scope"),
            ("callback_port", "callbackPort"),
            ("redirect_uri", "redirectUri"),
        ] {
            if let Some(v) = oauth.get(from) {
                lowered.insert(to.to_string(), v.clone());
            }
        }
        out.insert("oauth".into(), Value::Object(lowered));
    }
    out
}

fn normalize_server(input: &Value, path: &[&str], ctx: &mut Ctx) -> Option<Value> {
    let Some(server) = input.as_object().filter(|m| valid_server(m)) else {
        ctx.invalid(path);
        return None;
    };
    let sub = |leaf: &'static str| {
        let mut at = path.to_vec();
        at.push(leaf);
        at
    };
    if server.contains_key("codemode") {
        ctx.unsupported(&sub("codemode"));
    }
    if let Some(timeout) = server.get("timeout").map(timeout_fields)
        && lower_timeout(&timeout).is_none()
        && !timeout.is_empty()
    {
        ctx.unsupported(&sub("timeout"));
    }
    let mut out = lower_server(server);
    if let Some(enabled) = server.get("enabled") {
        if server.get("disabled").is_some_and(|d| d == enabled) {
            ctx.push(DiagnosticKind::Conflict, &sub("disabled"));
        }
        out.insert("enabled".into(), enabled.clone());
    }
    Some(Value::Object(out))
}

// An object holding a scalar `type` or `enabled` is itself a server, not an envelope.
fn is_direct_server(map: &Map<String, Value>) -> bool {
    ["type", "enabled"].iter().any(|k| map.get(*k).is_some_and(|v| !v.is_object()))
}

fn normalize_mcp(result: &mut Map<String, Value>, ctx: &mut Ctx) {
    let Some(mcp) = result.get("mcp").and_then(Value::as_object).cloned() else { return };
    let nested = mcp.get("servers").and_then(Value::as_object).filter(|m| !is_direct_server(m));
    let timeout = mcp
        .get("timeout")
        .filter(|v| valid_timeout(v))
        .and_then(Value::as_object)
        .filter(|m| !is_direct_server(m))
        .filter(|m| m.is_empty() || ["startup", "catalog", "execution"].iter().any(|k| m.contains_key(*k)));

    let mut servers = Map::new();
    for (name, value) in &mcp {
        if (name == "servers" && nested.is_some()) || (name == "timeout" && timeout.is_some()) {
            continue;
        }
        let record = value.as_object();
        let native = record.is_some_and(|r| {
            r.contains_key("disabled")
                || r.contains_key("codemode")
                || matches!(r.get("timeout"), Some(Value::Object(_) | Value::Array(_) | Value::Null))
                || r.get("oauth").and_then(Value::as_object).is_some_and(|o| {
                    ["client_id", "client_secret", "callback_port", "redirect_uri"].iter().any(|k| o.contains_key(*k))
                })
        });
        let lowered = if native { normalize_server(value, &["mcp", name], ctx) } else { None };
        servers.insert(name.clone(), lowered.unwrap_or_else(|| value.clone()));
    }
    for (name, value) in nested.into_iter().flatten() {
        let path = ["mcp", "servers", name.as_str()];
        if let Some(existing) = servers.get(name) {
            if existing != value {
                ctx.push(DiagnosticKind::Conflict, &path);
            }
            continue;
        }
        let legacy = value
            .as_object()
            .is_some_and(|r| r.get("enabled").is_some_and(Value::is_boolean) && !r.contains_key("disabled"));
        if legacy {
            servers.insert(name.clone(), value.clone());
        } else if let Some(server) = normalize_server(value, &path, ctx) {
            servers.insert(name.clone(), server);
        }
    }
    result.insert("mcp".into(), Value::Object(servers));

    let Some(timeout) = timeout.map(|m| timeout_fields(&Value::Object(m.clone()))) else { return };
    let Some(value) = lower_timeout(&timeout) else {
        if !timeout.is_empty() {
            ctx.unsupported(&["mcp", "timeout"]);
        }
        return;
    };
    let mut experimental = match result.get("experimental") {
        None => Map::new(),
        Some(Value::Object(map)) => map.clone(),
        Some(_) => return,
    };
    ctx.prefer_legacy(&mut experimental, "mcp_timeout", value, &["mcp", "timeout"]);
    result.insert("experimental".into(), Value::Object(experimental));
}

const BUILTIN_LSP: [&str; 38] = [
    "deno",
    "typescript",
    "vue",
    "eslint",
    "oxlint",
    "biome",
    "gopls",
    "ruby-lsp",
    "ty",
    "pyright",
    "elixir-ls",
    "zls",
    "csharp",
    "razor",
    "fsharp",
    "sourcekit-lsp",
    "rust",
    "clangd",
    "svelte",
    "astro",
    "jdtls",
    "kotlin-ls",
    "yaml-ls",
    "lua-ls",
    "php intelephense",
    "prisma",
    "dart",
    "ocaml-lsp",
    "bash",
    "terraform",
    "texlab",
    "dockerfile",
    "gleam",
    "clojure-lsp",
    "nixd",
    "tinymist",
    "haskell-language-server",
    "julials",
];

// Custom LSP entries that decode but lack `extensions` are dropped.
fn normalize_lsp(result: &mut Map<String, Value>, ctx: &mut Ctx) {
    let Some(lsp) = result.get("lsp").and_then(Value::as_object).cloned() else { return };
    let mut kept = Map::new();
    for (name, value) in lsp {
        let entry = value.as_object();
        let disabled = entry.is_some_and(|e| e.get("disabled") == Some(&Value::Bool(true)));
        let decodes = entry.is_some_and(|e| {
            e.get("command").is_some_and(string_array)
                && opt(e, "extensions", string_array)
                && opt(e, "disabled", Value::is_boolean)
                && opt(e, "env", string_record)
                && opt(e, "initialization", Value::is_object)
        });
        let has_extensions = entry.is_some_and(|e| e.contains_key("extensions"));
        if BUILTIN_LSP.contains(&name.as_str()) || disabled || !decodes || has_extensions {
            kept.insert(name, value);
        } else {
            ctx.unsupported(&["lsp", name.as_str()]);
        }
    }
    result.insert("lsp".into(), Value::Object(kept));
}

// The v1 permission key of a v2 rule action: the reverse of `normalizeAction` (`core/src/v1/config/migrate.ts`).
// v1 `write`/`patch`/`apply_patch` all became `edit`, which v1 already uses for those tools.
fn v1_action(action: &str) -> &str {
    match action {
        "shell" => "bash",
        "subagent" => "task",
        other => other,
    }
}

// Keys whose v1 value must be a bare action (`ACTION_ONLY` in `config.rs`).
const ACTION_ONLY: [&str; 5] = ["todowrite", "question", "webfetch", "websearch", "doom_loop"];

fn valid_rules(value: &Value) -> bool {
    value.as_array().is_some_and(|rules| {
        rules.iter().all(|rule| {
            rule.get("action").is_some_and(Value::is_string)
                && rule.get("resource").is_some_and(Value::is_string)
                && matches!(rule.get("effect").and_then(Value::as_str), Some("allow" | "deny" | "ask"))
        })
    })
}

// Rust build: v2 `permissions` (`Permission.Ruleset`, `{action, resource, effect}`) folded into a v1 `permission`
// value. v2 evaluates `[...tools, ...permission, ...permissions]` with the last matching rule winning
// (`config/normalize.ts`, `Permission.evaluate`), so with both keys present the v2 rules take precedence: each one
// moves its key, and its pattern within the key, to the end of the v1 object, where v1's last-match also wins.
// Moving a key can reorder it past a legacy wildcard key; that edge is accepted to keep the v2 rules authoritative.
// None when `rules` is not a ruleset.
pub(super) fn fold_permissions(permission: Option<&Value>, rules: &Value) -> Option<Value> {
    if !valid_rules(rules) {
        return None;
    }
    let mut out = match permission {
        Some(Value::Object(map)) => map.clone(),
        Some(Value::String(action)) => Map::from_iter([("*".to_string(), Value::String(action.clone()))]),
        _ => Map::new(),
    };
    for rule in rules.as_array().into_iter().flatten() {
        let key = v1_action(rule["action"].as_str().unwrap_or_default()).to_string();
        let resource = rule["resource"].as_str().unwrap_or_default().to_string();
        if ACTION_ONLY.contains(&key.as_str()) && resource != "*" {
            continue;
        }
        let mut patterns = match out.shift_remove(&key) {
            Some(Value::Object(map)) => map,
            Some(Value::String(action)) => Map::from_iter([("*".to_string(), Value::String(action))]),
            _ => Map::new(),
        };
        patterns.shift_remove(&resource);
        patterns.insert(resource, rule["effect"].clone());
        out.insert(key, Value::Object(patterns));
    }
    for value in out.values_mut() {
        if let Some(action) = value.as_object().filter(|m| m.len() == 1).and_then(|m| m.get("*")).cloned() {
            *value = action;
        }
    }
    Some(Value::Object(out))
}

// Replaces a `permissions` key of `map` with the equivalent `permission`; malformed rules are reported and dropped.
fn lower_permissions(map: &mut Map<String, Value>, path: &[&str], ctx: &mut Ctx) {
    let Some(rules) = map.shift_remove("permissions") else { return };
    match fold_permissions(map.get("permission"), &rules) {
        Some(permission) => {
            map.insert("permission".into(), permission);
        }
        None => {
            let mut at = path.to_vec();
            at.push("permissions");
            ctx.invalid(&at);
        }
    }
}

// Port of ConfigV2Compat.lower: accepts V2-native keys and rewrites them to the V1 shape.
pub fn lower(input: Value, source: &str, diagnostics: &mut Vec<Diagnostic>) -> Result<Value, ConfigError> {
    let Value::Object(input) = input else { return Ok(input) };
    let ctx = &mut Ctx { source, out: diagnostics };
    let mut result = input.clone();
    lower_permissions(&mut result, &[], ctx);
    for group in ["agent", "mode"] {
        for (name, agent) in result.get_mut(group).and_then(Value::as_object_mut).into_iter().flatten() {
            if let Some(agent) = agent.as_object_mut() {
                lower_permissions(agent, &[group, name.as_str()], ctx);
            }
        }
    }
    // Rust build: `plugins` (v2 `plugin add`) name v2 plugin modules, whose `{id, effect|setup}` default export the
    // v1 plugin host cannot run, so they stay unloaded like the other v2-only keys.
    for key in ["plugins", "providers", "websearch", "warming"] {
        if input.contains_key(key) {
            ctx.unsupported(&[key]);
        }
    }

    if let Some(value) = input.get("snapshots") {
        match value {
            Value::Bool(_) => ctx.prefer_legacy(&mut result, "snapshot", value.clone(), &["snapshots"]),
            _ => ctx.invalid(&["snapshots"]),
        }
    }
    if let Some(value) = input.get("media") {
        let image_ok = |image: &Value| {
            image.as_object().is_some_and(|i| {
                opt(i, "auto_resize", Value::is_boolean)
                    && ["max_width", "max_height", "max_base64_bytes"].iter().all(|k| opt(i, k, |v| is_int_at_least(v, 1.0)))
            })
        };
        match value.as_object().filter(|m| opt(m, "image", image_ok)) {
            Some(_) => ctx.prefer_legacy(&mut result, "attachment", value.clone(), &["media"]),
            None => ctx.invalid(&["media"]),
        }
    }

    if let Some((model, variant)) = input.get("model").and_then(lower_selection) {
        result.insert("model".into(), Value::String(model));
        if variant.is_some() {
            ctx.unsupported(&["model", "variant"]);
        }
    }

    if let Some(Value::Array(skills)) = input.get("skills") {
        if skills.iter().all(Value::is_string) {
            let remote = |s: &str| {
                let lower = s.to_ascii_lowercase();
                lower.starts_with("http://") || lower.starts_with("https://")
            };
            let (urls, paths): (Vec<Value>, Vec<Value>) =
                skills.iter().cloned().partition(|v| v.as_str().is_some_and(remote));
            let mut lowered = Map::new();
            lowered.insert("paths".into(), Value::Array(paths));
            lowered.insert("urls".into(), Value::Array(urls));
            result.insert("skills".into(), Value::Object(lowered));
        } else {
            ctx.invalid(&["skills"]);
        }
    }

    if let Some(Value::Object(compaction)) = input.get("compaction") {
        let mut value = compaction.clone();
        if let Some(keep) = compaction.get("keep") {
            match keep.as_object() {
                None => ctx.invalid(&["compaction", "keep"]),
                Some(keep) => match keep.get("tokens") {
                    None => {}
                    Some(tokens) if is_int_at_least(tokens, 0.0) => ctx.prefer_legacy(
                        &mut value,
                        "preserve_recent_tokens",
                        tokens.clone(),
                        &["compaction", "keep", "tokens"],
                    ),
                    Some(_) => ctx.invalid(&["compaction", "keep", "tokens"]),
                },
            }
        }
        if let Some(buffer) = compaction.get("buffer") {
            if is_int_at_least(buffer, 0.0) {
                ctx.prefer_legacy(&mut value, "reserved", buffer.clone(), &["compaction", "buffer"]);
            } else {
                ctx.invalid(&["compaction", "buffer"]);
            }
        }
        result.insert("compaction".into(), Value::Object(value));
    }

    if let Some(Value::Object(experimental)) = input.get("experimental") {
        if experimental.contains_key("portable_shell_scanner") {
            ctx.unsupported(&["experimental", "portable_shell_scanner"]);
        }
        if let Some(depth) = experimental.get("subagent_depth") {
            if is_int_at_least(depth, 0.0) {
                ctx.prefer_legacy(&mut result, "subagent_depth", depth.clone(), &["experimental", "subagent_depth"]);
            } else {
                ctx.invalid(&["experimental", "subagent_depth"]);
            }
        }
    }

    if let Some(agents) = input.get("agents") {
        match agents.as_object() {
            None => ctx.invalid(&["agents"]),
            Some(agents) => {
                let legacy = result.get("agent").and_then(Value::as_object).cloned();
                let mut merged = legacy.clone().unwrap_or_default();
                for (name, value) in agents {
                    let path = ["agents", name.as_str()];
                    if let Some(existing) = merged.get(name) {
                        if existing != value {
                            ctx.push(DiagnosticKind::Conflict, &path);
                        }
                        continue;
                    }
                    let lowered = match value.as_object() {
                        Some(agent) => lower_agent(agent, &path, ctx),
                        None => {
                            ctx.invalid(&path);
                            None
                        }
                    };
                    if let Some(lowered) = lowered {
                        merged.insert(name.clone(), lowered);
                    }
                }
                let blocked = result.contains_key("agent") && legacy.is_none();
                if !blocked && (!merged.is_empty() || legacy.is_some()) {
                    result.insert("agent".into(), Value::Object(merged));
                }
            }
        }
    }

    if let Some(commands) = input.get("commands") {
        match commands.as_object() {
            None => ctx.invalid(&["commands"]),
            Some(commands) => {
                let legacy = result.get("command").and_then(Value::as_object).cloned();
                if !(result.contains_key("command") && legacy.is_none()) {
                    let mut merged = legacy.clone().unwrap_or_default();
                    for (name, value) in commands {
                        let path = ["commands", name.as_str()];
                        if let Some(lowered) = lower_command(value, &path, ctx) {
                            ctx.prefer_legacy(&mut merged, name, lowered, &path);
                        }
                    }
                    if !merged.is_empty() || legacy.is_some() {
                        result.insert("command".into(), Value::Object(merged));
                    }
                }
            }
        }
    }

    normalize_mcp(&mut result, ctx);
    normalize_lsp(&mut result, ctx);
    Ok(Value::Object(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(input: Value) -> (Value, Vec<Diagnostic>) {
        let mut diags = Vec::new();
        let value = lower(input, "test", &mut diags).unwrap();
        (value, diags)
    }

    #[test]
    fn v1_config_passes_through() {
        let input = json!({
            "model": "anthropic/claude",
            "agent": {"build": {"model": "a/b"}},
            "mcp": {"fs": {"type": "local", "command": ["x"], "enabled": true, "timeout": 5000}},
            "skills": {"paths": ["a"]},
            "compaction": {"auto": false},
            "lsp": {"rust": {"disabled": true}}
        });
        let (value, diags) = run(input.clone());
        assert_eq!(value, input);
        assert!(diags.is_empty());
    }

    #[test]
    fn v2_permissions_fold_into_v1_permission() {
        let (value, diags) = run(json!({"permissions": [
            {"action": "*", "resource": "*", "effect": "ask"},
            {"action": "shell", "resource": "git *", "effect": "allow"},
            {"action": "shell", "resource": "*", "effect": "deny"},
            {"action": "subagent", "resource": "*", "effect": "allow"},
            {"action": "webfetch", "resource": "*", "effect": "deny"},
            {"action": "webfetch", "resource": "https://x/*", "effect": "allow"}
        ]}));
        assert!(diags.is_empty(), "{diags:?}");
        assert!(value.get("permissions").is_none());
        assert_eq!(value["permission"], json!({"*": "ask", "bash": {"git *": "allow", "*": "deny"}, "task": "allow", "webfetch": "deny"}));

        // Both keys: the v2 rules come after the v1 ones, as v2 orders them, so they win on overlap.
        let (value, _) = run(json!({
            "permission": {"bash": {"*": "allow", "rm *": "deny"}, "edit": "ask", "read": "allow"},
            "permissions": [{"action": "shell", "resource": "*", "effect": "ask"}, {"action": "edit", "resource": "*", "effect": "allow"}]
        }));
        let permission = value["permission"].as_object().unwrap();
        assert_eq!(permission.keys().collect::<Vec<_>>(), ["read", "bash", "edit"]);
        assert_eq!(permission["bash"], json!({"rm *": "deny", "*": "ask"}));
        assert_eq!(permission["edit"], "allow");
        let (value, _) = run(json!({"permission": "deny", "permissions": [{"action": "read", "resource": "*", "effect": "allow"}]}));
        assert_eq!(value["permission"], json!({"*": "deny", "read": "allow"}));

        // Agents of either shape.
        let (value, diags) = run(json!({
            "agents": {"rev": {"system": "s", "permissions": [{"action": "edit", "resource": "*", "effect": "deny"}]}},
            "agent": {"old": {"permission": {"edit": "allow"}, "permissions": [{"action": "shell", "resource": "*", "effect": "deny"}]}},
            "mode": {"m": {"permissions": [{"action": "read", "resource": "*", "effect": "ask"}]}}
        }));
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(value["agent"]["rev"], json!({"prompt": "s", "permission": {"edit": "deny"}}));
        assert_eq!(value["agent"]["old"], json!({"permission": {"edit": "allow", "bash": "deny"}}));
        assert_eq!(value["mode"]["m"], json!({"permission": {"read": "ask"}}));

        // Malformed rules are reported and dropped instead of failing the config.
        let (value, diags) = run(json!({"permissions": {"*": "allow"}, "agent": {"x": {"permissions": [{"action": "a"}]}}}));
        assert!(value.get("permissions").is_none() && value.get("permission").is_none());
        assert!(value["agent"]["x"].get("permissions").is_none());
        let kinds: Vec<_> = diags.iter().map(|d| (d.kind, d.path.join("."))).collect();
        assert_eq!(kinds, [(DiagnosticKind::Invalid, "permissions".into()), (DiagnosticKind::Invalid, "agent.x.permissions".into())]);
    }

    #[test]
    fn settings_model_skills() {
        let (value, diags) = run(json!({
            "snapshots": false,
            "media": {"image": {"max_width": 100}},
            "model": "openai/gpt#high",
            "skills": ["./local", "HTTPS://x.dev/skills", "~/s"],
            "plugins": []
        }));
        assert_eq!(value["snapshot"], false);
        assert_eq!(value["attachment"], json!({"image": {"max_width": 100}}));
        assert_eq!(value["model"], "openai/gpt");
        assert_eq!(value["skills"], json!({"paths": ["./local", "~/s"], "urls": ["HTTPS://x.dev/skills"]}));
        let kinds: Vec<_> = diags.iter().map(|d| (d.kind, d.path.join("."))).collect();
        assert_eq!(
            kinds,
            [(DiagnosticKind::Unsupported, "plugins".to_string()), (DiagnosticKind::Unsupported, "model.variant".to_string())]
        );
        let (value, _) = run(json!({"model": {"providerID": "p", "model": "m/x", "variant": "v"}}));
        assert_eq!(value["model"], "p/m/x");
        let (value, _) = run(json!({"model": "bare"}));
        assert_eq!(value["model"], "bare");
    }

    #[test]
    fn legacy_value_wins_on_conflict() {
        let (value, diags) = run(json!({"snapshot": true, "snapshots": false, "subagent_depth": 2, "experimental": {"subagent_depth": 2}}));
        assert_eq!(value["snapshot"], true);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].kind, DiagnosticKind::Conflict);
    }

    #[test]
    fn compaction_and_experimental() {
        let (value, diags) = run(json!({
            "compaction": {"keep": {"tokens": 100}, "buffer": 5, "auto": true},
            "experimental": {"subagent_depth": 3, "portable_shell_scanner": true}
        }));
        assert_eq!(value["compaction"]["preserve_recent_tokens"], 100);
        assert_eq!(value["compaction"]["reserved"], 5);
        assert_eq!(value["subagent_depth"], 3);
        assert_eq!(diags.len(), 1);
        let (_, diags) = run(json!({"compaction": {"buffer": -1}}));
        assert_eq!(diags[0].kind, DiagnosticKind::Invalid);
    }

    #[test]
    fn agents_and_commands() {
        let (value, diags) = run(json!({
            "agent": {"keep": {"prompt": "legacy"}},
            "agents": {
                "keep": {"system": "native"},
                "rev": {
                    "system": "be careful", "disabled": false, "model": "a/b#fast", "mode": "subagent",
                    "request": {"headers": {"x": "y"}, "body": {"k": 1}}, "extra": true
                },
                "bad": {"steps": 0}
            },
            "commands": {"t": {"template": "do $1", "model": {"providerID": "p", "model": "m"}, "junk": 1}, "bad": {}}
        }));
        assert_eq!(value["agent"]["keep"], json!({"prompt": "legacy"}));
        assert_eq!(
            value["agent"]["rev"],
            json!({"mode": "subagent", "prompt": "be careful", "disable": false, "model": "a/b", "variant": "fast", "options": {"k": 1}})
        );
        assert!(value["agent"].get("bad").is_none());
        assert_eq!(value["command"], json!({"t": {"template": "do $1", "model": "p/m"}}));
        let kinds: Vec<_> = diags.iter().map(|d| (d.kind, d.path.join("."))).collect();
        assert!(kinds.contains(&(DiagnosticKind::Conflict, "agents.keep".into())));
        assert!(kinds.contains(&(DiagnosticKind::Unsupported, "agents.rev.request.headers".into())));
        assert!(kinds.contains(&(DiagnosticKind::Invalid, "agents.bad".into())));
        assert!(kinds.contains(&(DiagnosticKind::Invalid, "commands.bad".into())));
    }

    #[test]
    fn mcp_envelope_and_native_servers() {
        let (value, diags) = run(json!({
            "mcp": {
                "timeout": {"catalog": 9000, "execution": 9000},
                "flat": {"type": "local", "command": ["a"], "disabled": true, "codemode": true},
                "servers": {
                    "web": {
                        "type": "remote", "url": "https://x", "timeout": {"startup": 1},
                        "oauth": {"client_id": "id", "callback_port": 1234, "scope": "s"}
                    },
                    "old": {"type": "local", "command": ["b"], "enabled": false},
                    "broken": {"type": "local"}
                }
            }
        }));
        assert_eq!(value["mcp"]["flat"], json!({"type": "local", "command": ["a"], "enabled": false}));
        assert_eq!(
            value["mcp"]["web"],
            json!({"type": "remote", "url": "https://x", "oauth": {"clientId": "id", "scope": "s", "callbackPort": 1234}, "enabled": true})
        );
        assert_eq!(value["mcp"]["old"], json!({"type": "local", "command": ["b"], "enabled": false}));
        assert!(value["mcp"].get("broken").is_none());
        assert!(value["mcp"].get("servers").is_none());
        assert!(value["mcp"].get("timeout").is_none());
        assert_eq!(value["experimental"]["mcp_timeout"], 9000);
        let kinds: Vec<_> = diags.iter().map(|d| (d.kind, d.path.join("."))).collect();
        assert!(kinds.contains(&(DiagnosticKind::Unsupported, "mcp.flat.codemode".into())));
        assert!(kinds.contains(&(DiagnosticKind::Unsupported, "mcp.servers.web.timeout".into())));
        assert!(kinds.contains(&(DiagnosticKind::Invalid, "mcp.servers.broken".into())));
    }

    #[test]
    fn mcp_server_named_servers_is_not_an_envelope() {
        let input = json!({"mcp": {"servers": {"type": "local", "command": ["x"]}, "timeout": {"type": "local", "command": ["y"]}}});
        let (value, diags) = run(input.clone());
        assert_eq!(value, input);
        assert!(diags.is_empty());
    }

    #[test]
    fn lsp_custom_without_extensions_dropped() {
        let (value, diags) = run(json!({"lsp": {
            "rust": {"command": ["ra"]},
            "mine": {"command": ["x"]},
            "ok": {"command": ["x"], "extensions": [".x"]},
            "off": {"disabled": true},
            "odd": {"command": "not-array"}
        }}));
        assert_eq!(value["lsp"].as_object().unwrap().keys().collect::<Vec<_>>(), ["rust", "ok", "off", "odd"]);
        assert_eq!(diags.len(), 1);
    }
}
