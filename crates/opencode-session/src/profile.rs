// Rust build: the "agent" tool profile (config `tool_profile`, the default). The built-in tools are shown to the model
// as Read, Write, Edit, Glob, Grep, Bash/PowerShell/Cmd, WebFetch, TodoWrite and Task with snake_case parameters and
// translated back here; stored parts, permissions, plugin hooks and the HTTP API keep the internal names and arguments.
use opencode_llm as llm;
use opencode_tools::{ToolError, shell};
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

pub(crate) const SYSTEM: &str = include_str!("../prompts/agent/system.txt");
const TOOLS: &str = include_str!("../prompts/agent/tools.md");
const SCHEMA: &str = "https://json-schema.org/draft/2020-12/schema";
pub(crate) const BASH_MAX_TIMEOUT_MS: u64 = 600_000;

// (exposed, internal) parameter names.
type Fields = &'static [(&'static str, &'static str)];

// (internal id, exposed name, parameters). The shell tool's name follows the shell.
const ADAPTED: &[(&str, &str, Fields)] = &[
    ("read", "Read", &[("file_path", "filePath")]),
    ("write", "Write", &[("file_path", "filePath")]),
    ("edit", "Edit", &[("file_path", "filePath"), ("old_string", "oldString"), ("new_string", "newString"), ("replace_all", "replaceAll")]),
    ("glob", "Glob", &[]),
    ("grep", "Grep", &[("glob", "include")]),
    ("bash", "Bash", &[]),
    ("webfetch", "WebFetch", &[]),
    ("todowrite", "TodoWrite", &[]),
    ("task", "Task", &[]),
];

fn fields(internal: &str) -> Fields {
    ADAPTED.iter().find(|(id, ..)| *id == internal).map_or(&[], |(_, _, fields)| fields)
}

// Text between `<!--` and `-->` is for implementers only.
fn strip_notes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        rest = rest[start..].find("-->").map_or("", |end| &rest[start + end + 3..]);
    }
    out.push_str(rest);
    out
}

// `## <Name>` sections of tools.md.
static SECTIONS: LazyLock<Vec<(String, String)>> = LazyLock::new(|| {
    TOOLS
        .split("\n## ")
        .skip(1)
        .map(|chunk| {
            let (title, body) = chunk.split_once('\n').unwrap_or((chunk, ""));
            (title.trim().to_string(), strip_notes(body).trim().to_string())
        })
        .collect()
});

pub(crate) fn section(name: &str) -> &'static str {
    SECTIONS.iter().find(|(title, _)| title == name).map_or("", |(_, body)| body.as_str())
}

// Which internal tools are adapted, and their exposed names, for one instance.
#[derive(Debug, Clone, Default)]
pub(crate) struct Names {
    map: Vec<(&'static str, &'static str)>,
    pub(crate) shell: String,
}

impl Names {
    // `skip`: ids a plugin or custom tool replaced; those keep their own name and schema.
    pub(crate) fn new(agent: bool, shell: &str, skip: &HashSet<String>) -> Self {
        let map = if agent {
            ADAPTED
                .iter()
                .filter(|(id, ..)| *id == "task" || !skip.contains(*id))
                .map(|(id, name, _)| (*id, if *id == "bash" { shell::tool_name(shell) } else { *name }))
                .collect()
        } else {
            Vec::new()
        };
        Self { map, shell: shell.to_string() }
    }

    pub(crate) fn active(&self) -> bool {
        !self.map.is_empty()
    }

    pub(crate) fn adapted(&self, internal: &str) -> bool {
        self.map.iter().any(|(id, _)| *id == internal)
    }

    pub(crate) fn exposed<'a>(&self, internal: &'a str) -> &'a str {
        self.map.iter().find(|(id, _)| *id == internal).map_or(internal, |(_, name)| *name)
    }

    // The internal id of an exposed name, exact first, then ignoring case.
    pub(crate) fn internal(&self, exposed: &str) -> Option<&'static str> {
        let exact = self.map.iter().find(|(_, name)| *name == exposed);
        exact.or_else(|| self.map.iter().find(|(_, name)| name.eq_ignore_ascii_case(exposed))).map(|(id, _)| *id)
    }
}

fn object(properties: Value, required: &[&str]) -> Value {
    json!({"$schema": SCHEMA, "type": "object", "properties": properties, "required": required})
}

pub(crate) fn schema(internal: &str) -> Value {
    match internal {
        "read" => object(
            json!({
                "file_path": {"type": "string", "description": "Absolute path of the file to read"},
                "offset": {"type": "integer", "minimum": 1, "description": "1-based line number to start reading from"},
                "limit": {"type": "integer", "minimum": 1, "description": "Number of lines to read (default 2000)"}
            }),
            &["file_path"],
        ),
        "write" => object(
            json!({
                "file_path": {"type": "string", "description": "Absolute path of the file to create or replace"},
                "content": {"type": "string", "description": "The complete new content of the file"}
            }),
            &["file_path", "content"],
        ),
        "edit" => object(
            json!({
                "file_path": {"type": "string", "description": "Absolute path of a file you have already Read"},
                "old_string": {"type": "string", "description": "Exact text to replace, copied from the file without line-number prefixes"},
                "new_string": {"type": "string", "description": "The replacement text (must differ from old_string)"},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence of old_string (default false)"}
            }),
            &["file_path", "old_string", "new_string"],
        ),
        "glob" => object(
            json!({
                "pattern": {"type": "string", "description": "Glob pattern such as **/*.rs"},
                "path": {"type": "string", "description": "Absolute directory to search in; defaults to the working directory"}
            }),
            &["pattern"],
        ),
        "grep" => object(
            json!({
                "pattern": {"type": "string", "description": "Regular expression (ripgrep syntax)"},
                "path": {"type": "string", "description": "Absolute file or directory to search; defaults to the working directory"},
                "glob": {"type": "string", "description": "Only search files matching this glob, e.g. *.ts or *.{ts,tsx}"},
                "output_mode": {
                    "type": "string",
                    "enum": ["files_with_matches", "content", "count"],
                    "description": "files_with_matches (default): matching file paths; content: matching lines with line numbers; count: matches per file"
                },
                "-i": {"type": "boolean", "description": "Case-insensitive search"},
                "context": {"type": "integer", "minimum": 0, "description": "Lines of context around each match (content mode)"},
                "head_limit": {"type": "integer", "minimum": 1, "description": "Maximum number of results"}
            }),
            &["pattern"],
        ),
        "bash" => object(
            json!({
                "command": {"type": "string", "description": "The command line to run"},
                "description": {"type": "string", "description": "5-10 words saying what the command does"},
                "timeout": {"type": "integer", "minimum": 1, "maximum": BASH_MAX_TIMEOUT_MS, "description": "Timeout in milliseconds (default 120000, max 600000)"},
                "workdir": {"type": "string", "description": "Absolute directory to run the command in"},
                "background": {"type": "boolean", "description": opencode_tools::bash::BACKGROUND_DESCRIPTION}
            }),
            &["command"],
        ),
        "webfetch" => object(
            json!({
                "url": {"type": "string", "description": "Full http(s) URL"},
                "format": {"type": "string", "enum": ["markdown", "text", "html"], "description": "Format of the returned content (default markdown)"},
                "timeout": {"type": "number", "description": "Timeout in seconds (max 120)"}
            }),
            &["url"],
        ),
        "todowrite" => object(
            json!({
                "todos": {
                    "type": "array",
                    "description": "The complete task list",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string", "description": "Imperative description of the task"},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"]},
                            "priority": {"type": "string", "enum": ["high", "medium", "low"]}
                        },
                        "required": ["content", "status", "priority"]
                    }
                }
            }),
            &["todos"],
        ),
        "task" => object(
            json!({
                "description": {"type": "string", "description": "3-5 words describing the task"},
                "prompt": {"type": "string", "description": "Complete brief for the subagent"},
                "subagent_type": {"type": "string", "description": "One of the available agents"}
            }),
            &["description", "prompt", "subagent_type"],
        ),
        _ => json!({"type": "object"}),
    }
}

// The model-facing description; `agents` lists the subagents for Task.
pub(crate) fn description(internal: &str, names: &Names, agents: &[String]) -> String {
    match internal {
        "bash" => {
            let text = section("Bash").replace("{shell}", &shell::label(&names.shell));
            match shell::notes(&names.shell) {
                "" => text,
                notes => format!("{text}\n\n{notes}"),
            }
        }
        "task" if !agents.is_empty() => format!("{}\n\nAvailable agents:\n{}", section("Task"), agents.join("\n")),
        other => section(ADAPTED.iter().find(|(id, ..)| *id == other).map_or("", |(_, name, _)| name)).to_string(),
    }
}

fn rename(args: Value, map: impl Fn(&str) -> Option<&'static str>) -> Map<String, Value> {
    match args {
        Value::Object(fields) => fields.into_iter().map(|(key, value)| (map(&key).map_or(key, str::to_string), value)).collect(),
        other => Map::from_iter([("value".to_string(), other)]),
    }
}

// Model arguments -> internal arguments. Unknown keys (and internal spellings) pass through.
pub(crate) fn to_internal(internal: &str, args: Value) -> Value {
    let fields = fields(internal);
    let mut out = rename(args, |key| fields.iter().find(|(exposed, _)| *exposed == key).map(|(_, inner)| *inner));
    match internal {
        "bash" => {
            if out.get("timeout").and_then(Value::as_f64).is_some_and(|t| t > BASH_MAX_TIMEOUT_MS as f64) {
                out.insert("timeout".into(), BASH_MAX_TIMEOUT_MS.into());
            }
        }
        "todowrite" => {
            for todo in out.get_mut("todos").and_then(Value::as_array_mut).into_iter().flatten() {
                if let Some(todo) = todo.as_object_mut()
                    && !todo.contains_key("priority")
                {
                    todo.insert("priority".into(), "medium".into());
                }
            }
        }
        _ => {}
    }
    Value::Object(out)
}

// Internal arguments -> the model's spelling, for replaying history.
pub(crate) fn to_exposed(internal: &str, args: Value) -> Value {
    let fields = fields(internal);
    Value::Object(rename(args, |key| fields.iter().find(|(_, inner)| *inner == key).map(|(exposed, _)| *exposed)))
}

// Earlier turns as the model made them: exposed names and snake_case arguments.
pub(crate) fn replay(messages: &mut [llm::Message], names: &Names) {
    if !names.active() {
        return;
    }
    for message in messages {
        let llm::Message::Assistant { tool_calls, .. } = message else { continue };
        for call in tool_calls {
            let Some(internal) = names.map.iter().map(|(id, _)| *id).find(|id| call.name == *id) else { continue };
            call.name = names.exposed(internal).to_string();
            if let Ok(args) = serde_json::from_str::<Value>(&call.arguments) {
                call.arguments = to_exposed(internal, args).to_string();
            }
        }
    }
}

// The path a file tool's internal arguments name, resolved like the tools do.
pub(crate) fn file_of(cwd: &Path, args: &Value) -> Option<PathBuf> {
    let file = args.get("filePath").and_then(Value::as_str).filter(|f| !f.is_empty())?;
    Some(opencode_tools::util::normalize_path(&opencode_tools::util::resolve(cwd, file)))
}

// Identity of a file for the read-before-write rule (case-insensitive on Windows).
pub(crate) fn key(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) { text.to_lowercase() } else { text }
}

pub(crate) fn unread(path: &Path, tool: &str) -> ToolError {
    let action = if tool == "write" { "overwrite" } else { "edit" };
    ToolError::Other(format!("{} has not been Read in this session — Read it first, then {action} it.", path.display()))
}

pub(crate) fn directory(path: &Path) -> ToolError {
    ToolError::Other(format!("{} is a directory, not a file — use Glob or the shell to list it.", path.display()))
}

fn occurrences(file: &Path, old: &str) -> usize {
    let Ok(content) = std::fs::read_to_string(file) else { return 0 };
    let content = content.replace("\r\n", "\n");
    let old = old.replace("\r\n", "\n");
    if old.is_empty() { 0 } else { content.matches(&old).count() }
}

// Internal errors in the profile's terms: exposed tool and parameter names, actionable wording for Edit.
pub(crate) fn error(internal: &str, names: &Names, args: &Value, cwd: &Path, error: ToolError) -> ToolError {
    let fields = fields(internal);
    match error {
        ToolError::InvalidArguments { detail, .. } => {
            let detail = fields.iter().fold(detail, |text, (exposed, inner)| text.replace(&format!("`{inner}`"), &format!("`{exposed}`")));
            ToolError::InvalidArguments { tool: names.exposed(internal).to_string(), detail }
        }
        ToolError::Other(message) if internal == "edit" => {
            let path = file_of(cwd, args).map_or_else(|| "the file".to_string(), |p| p.display().to_string());
            let old = args.get("oldString").and_then(Value::as_str).unwrap_or_default();
            let text = if message.starts_with("Could not find oldString") {
                format!("old_string not found in {path} — re-Read the file: it may have changed, or your copy has different whitespace, indentation or line-number prefixes.")
            } else if message.starts_with("Found multiple matches for oldString") {
                let count = file_of(cwd, args).map_or(0, |file| occurrences(&file, old));
                let found = if count > 1 { format!("found {count} matches") } else { "found several matches".to_string() };
                format!("{found} of old_string in {path} — add surrounding lines to make it unique, or set replace_all to true to change every occurrence.")
            } else if message.starts_with("No changes to apply") {
                "old_string and new_string are identical — nothing to change.".to_string()
            } else if message.starts_with("oldString cannot be empty") {
                format!("old_string is empty but {path} already exists — put the exact text to replace in old_string, or Read the file and use Write to replace it entirely.")
            } else if message.starts_with("Refusing replacement because the matched span") {
                format!("old_string matched a much larger span of {path} than given — re-Read the file and copy the exact text into old_string.")
            } else if message.starts_with("File ") && message.ends_with(" not found") {
                format!("{path} does not exist — check the path with Glob, or create the file with Write.")
            } else {
                message
            };
            ToolError::Other(text)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Names {
        Names::new(true, "C:/Program Files/Git/bin/bash.exe", &HashSet::new())
    }

    #[test]
    fn every_tool_has_a_section_schema_and_round_trips() {
        let names = names();
        for (id, exposed, fields) in ADAPTED {
            let text = description(id, &names, &["- explore: Fast".to_string()]);
            assert!(!text.is_empty() && !text.contains("<!--") && !text.contains("maps to"), "{id}: {text}");
            let schema = schema(id);
            let properties = schema["properties"].as_object().unwrap();
            for (key, _) in *fields {
                assert!(properties.contains_key(*key), "{id} lacks {key}");
            }
            for required in schema["required"].as_array().unwrap() {
                assert!(properties.contains_key(required.as_str().unwrap()), "{id}: {required}");
            }
            // Every exposed property survives a round trip through the internal spelling.
            let args = Value::Object(properties.keys().map(|k| (k.clone(), json!(format!("v-{k}")))).collect());
            let internal = to_internal(id, args.clone());
            if *id != "bash" {
                assert_eq!(to_exposed(id, internal.clone()), args, "{id}");
            }
            for (exposed_key, inner) in *fields {
                assert_eq!(internal[*inner], json!(format!("v-{exposed_key}")), "{id}");
                assert!(internal.get(*exposed_key).is_none(), "{id}");
            }
            assert_eq!(names.internal(names.exposed(id)), Some(*id), "{exposed}");
        }
        assert_eq!(schema("grep")["properties"]["output_mode"]["enum"], json!(["files_with_matches", "content", "count"]));
    }

    #[test]
    fn translations_and_defaults() {
        let edit = to_internal("edit", json!({"file_path": "/a", "old_string": "x", "new_string": "y", "replace_all": true}));
        assert_eq!(edit, json!({"filePath": "/a", "oldString": "x", "newString": "y", "replaceAll": true}));
        assert_eq!(to_internal("read", json!({"filePath": "/a"})), json!({"filePath": "/a"}));
        assert_eq!(to_internal("grep", json!({"pattern": "p", "glob": "*.rs", "-i": true})), json!({"pattern": "p", "include": "*.rs", "-i": true}));
        assert_eq!(to_internal("bash", json!({"command": "x", "timeout": 9_000_000}))["timeout"], 600_000);
        assert_eq!(to_internal("bash", json!({"command": "x", "timeout": 5000}))["timeout"], 5000);
        let todos = to_internal("todowrite", json!({"todos": [{"content": "a", "status": "pending"}, {"content": "b", "status": "completed", "priority": "high"}]}));
        assert_eq!(todos["todos"][0]["priority"], "medium");
        assert_eq!(todos["todos"][1]["priority"], "high");
    }

    #[test]
    fn names_follow_the_shell_and_plugins() {
        assert_eq!(names().exposed("bash"), "Bash");
        let ps = Names::new(true, "C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe", &HashSet::new());
        assert_eq!((ps.exposed("bash"), ps.internal("PowerShell"), ps.internal("powershell")), ("PowerShell", Some("bash"), Some("bash")));
        let text = description("bash", &ps, &[]);
        assert!(text.starts_with("Runs a command in a Windows PowerShell 5.1 shell"), "{text}");
        assert!(text.contains("# Windows PowerShell 5.1 notes") && text.contains("cmd1; if ($?) { cmd2 }"), "{text}");
        let cmd = Names::new(true, "C:/Windows/System32/cmd.exe", &HashSet::new());
        assert_eq!(cmd.exposed("bash"), "Cmd");
        // A plugin tool that replaces `read` keeps its own name; the original profile adapts nothing.
        let skip = HashSet::from(["read".to_string(), "task".to_string()]);
        let plugged = Names::new(true, "/bin/bash", &skip);
        assert!(!plugged.adapted("read") && plugged.adapted("task") && plugged.exposed("read") == "read");
        let original = Names::new(false, "/bin/bash", &HashSet::new());
        assert!(!original.active() && original.exposed("read") == "read" && original.internal("Read").is_none());
        assert_eq!(names().internal("read"), Some("read"));
        assert_eq!(names().internal("skill"), None);
    }

    #[test]
    fn history_replays_with_exposed_names() {
        let call = |name: &str, args: Value| llm::ToolCall { id: "c".into(), name: name.into(), arguments: args.to_string() };
        let mut messages = vec![llm::Message::Assistant {
            text: String::new(),
            reasoning: String::new(),
            tool_calls: vec![
                call("edit", json!({"filePath": "/a", "oldString": "x", "newString": "y"})),
                call("skill", json!({"name": "s"})),
                call("bash", json!({"command": "ls"})),
            ],
            reasoning_parts: Vec::new(),
        }];
        replay(&mut messages, &names());
        let llm::Message::Assistant { tool_calls, .. } = &messages[0] else { panic!() };
        assert_eq!((tool_calls[0].name.as_str(), tool_calls[1].name.as_str(), tool_calls[2].name.as_str()), ("Edit", "skill", "Bash"));
        let args: Value = serde_json::from_str(&tool_calls[0].arguments).unwrap();
        assert_eq!(args, json!({"file_path": "/a", "old_string": "x", "new_string": "y"}));
    }

    #[test]
    fn errors_use_the_profile_terms() {
        let names = names();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "a\nb\na\n").unwrap();
        let args = json!({"filePath": file.to_string_lossy(), "oldString": "a", "newString": "c"});
        let other = |text: &str| ToolError::Other(text.into());
        let multiple = error("edit", &names, &args, dir.path(), other("Found multiple matches for oldString. Provide more surrounding context to make the match unique.")).to_string();
        assert!(multiple.starts_with("found 2 matches of old_string in ") && multiple.ends_with("or set replace_all to true to change every occurrence."), "{multiple}");
        let missing = error("edit", &names, &args, dir.path(), other("Could not find oldString in the file. It must match exactly, including whitespace, indentation, and line endings.")).to_string();
        assert!(missing.starts_with("old_string not found in ") && missing.contains("re-Read the file"), "{missing}");
        let invalid = error("read", &names, &json!({}), dir.path(), ToolError::InvalidArguments { tool: "read".into(), detail: "missing field `filePath`".into() });
        assert_eq!(invalid.to_string(), "The Read tool was called with invalid arguments: missing field `file_path`.\nPlease rewrite the input so it satisfies the expected schema.");
        assert!(matches!(error("read", &names, &json!({}), dir.path(), ToolError::Rejected), ToolError::Rejected));
    }

    #[test]
    fn sections_are_parsed_without_notes() {
        assert!(section("Read").starts_with("Reads a file from the local filesystem"));
        assert!(section("Bash").starts_with("Runs a command in a {shell} shell"));
        assert!(section("Task").ends_with("Do not use it for a single file read or a known-location lookup."));
        assert_eq!(section("Nope"), "");
        assert_eq!(strip_notes("a\n<!-- x\ny -->\nb"), "a\n\nb");
    }
}
