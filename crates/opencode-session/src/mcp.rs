// MCP in the loop: resource tools, prompt commands and the system block (`session/tools.ts:27-39,136-386,493-590`).
use crate::tools::{Def, locale_cmp};
use indexmap::IndexMap;
use opencode_core::command::{Command, Source};
use opencode_core::permission::{self, Rule};
use opencode_mcp::{McpManager, ServerInstructions};
use opencode_storage::FilePart;
use opencode_tools::{Attachment, PermissionRequest, Tool, ToolContext, ToolError, ToolOutput};
use serde_json::{Value, json};

pub const LIST: &str = "list_mcp_resources";
pub const LIST_TEMPLATES: &str = "list_mcp_resource_templates";
pub const READ: &str = "read_mcp_resource";

const MAX_BLOB_BYTES: u64 = 10 * 1024 * 1024;
const ATTACHMENT_MIMES: [&str; 5] = ["application/pdf", "image/gif", "image/jpeg", "image/png", "image/webp"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    List,
    ListTemplates,
    Read,
}

pub fn kind(name: &str) -> Option<Kind> {
    match name {
        LIST => Some(Kind::List),
        LIST_TEMPLATES => Some(Kind::ListTemplates),
        READ => Some(Kind::Read),
        _ => None,
    }
}

fn server_schema(description: &str) -> Value {
    json!({
        "type": "object",
        "properties": {"server": {"type": "string", "description": description}},
        "additionalProperties": false
    })
}

pub fn resource_defs() -> Vec<Def> {
    vec![
        Def {
            name: LIST.into(),
            description: "Lists resources provided by connected MCP servers. Resources provide context such as files, database schemas, or application-specific information.".into(),
            parameters: server_schema("Optional MCP server name. When omitted, lists resources from every connected server."),
        },
        Def {
            name: LIST_TEMPLATES.into(),
            description: "Lists resource templates provided by connected MCP servers. Resource templates are parameterized resources that can be read after filling in their URI template.".into(),
            parameters: server_schema("Optional MCP server name. When omitted, lists resource templates from every connected server."),
        },
        Def {
            name: READ.into(),
            description: "Read a specific resource from an MCP server using the server name and resource URI. The URI is an MCP identifier and does not need to be a file URL.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "server": {"type": "string", "description": "MCP server name exactly as returned by list_mcp_resources."},
                    "uri": {"type": "string", "description": "Resource URI to read. Use the exact URI string returned by list_mcp_resources."}
                },
                "required": ["server", "uri"],
                "additionalProperties": false
            }),
        },
    ]
}

fn optional_string(args: &Value, key: &str) -> Result<Option<String>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.is_empty() => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(ToolError::Other(format!("{key} must be a string"))),
    }
}

fn required_string(args: &Value, key: &str) -> Result<String, ToolError> {
    optional_string(args, key)?.ok_or_else(|| ToolError::Other(format!("{key} is required")))
}

// `{...item without client, server: client}`
fn with_server(item: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for (key, value) in item.as_object().into_iter().flatten() {
        if key != "client" {
            out.insert(key.clone(), value.clone());
        }
    }
    out.insert("server".into(), item.get("client").cloned().unwrap_or(Value::Null));
    Value::Object(out)
}

fn base64_size(value: &str) -> u64 {
    let trimmed: String = value.chars().filter(|c| !opencode_tools::util::is_js_whitespace(*c)).collect();
    let padding = if trimmed.ends_with("==") {
        2
    } else if trimmed.ends_with('=') {
        1
    } else {
        0
    };
    ((trimmed.encode_utf16().count() as u64 * 3) / 4).saturating_sub(padding)
}

fn format_bytes(value: u64) -> String {
    if value < 1024 {
        format!("{value} B")
    } else if value < 1024 * 1024 {
        format!("{} KB", value.div_ceil(1024))
    } else {
        format!("{} MB", value.div_ceil(1024 * 1024))
    }
}

// Text for a blob that cannot be attached, or None when it can.
fn blob_refusal(label: &str, mime: &str, blob: &str) -> Option<String> {
    let size = base64_size(blob);
    if !ATTACHMENT_MIMES.contains(&mime) {
        return Some(format!("[Binary MCP resource omitted: {label} ({mime}, {}) is not a supported attachment type]", format_bytes(size)));
    }
    (size > MAX_BLOB_BYTES).then(|| {
        format!("[Binary MCP resource omitted: {label} ({mime}, {}) exceeds {}]", format_bytes(size), format_bytes(MAX_BLOB_BYTES))
    })
}

fn contents(content: &Value) -> Vec<&Value> {
    match content.get("contents") {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(other) => vec![other],
        None => Vec::new(),
    }
}

struct Formatted {
    count: usize,
    text: String,
    attachments: Vec<Attachment>,
}

fn format_content(server: &str, uri: &str, content: &Value) -> Formatted {
    let items: Vec<&Value> = contents(content).into_iter().filter(|item| item.is_object()).collect();
    let mut text = Vec::new();
    let mut attachments = Vec::new();
    for item in &items {
        let item_uri = item.get("uri").and_then(Value::as_str).unwrap_or(uri);
        let mime = item.get("mimeType").and_then(Value::as_str).unwrap_or("application/octet-stream");
        if let Some(body) = item.get("text").and_then(Value::as_str) {
            text.push(format!("Resource: {item_uri}\nMIME: {mime}\n{body}"));
        } else if let Some(blob) = item.get("blob").and_then(Value::as_str) {
            match blob_refusal(item_uri, mime, blob) {
                Some(refusal) => text.push(refusal),
                None => {
                    text.push(format!("[Binary MCP resource attached: {item_uri} ({mime})]"));
                    attachments.push(Attachment { mime: mime.to_string(), url: format!("data:{mime};base64,{blob}") });
                }
            }
        } else {
            text.push(format!("[MCP resource content without text or blob: {item_uri}]"));
        }
    }
    let text = if text.is_empty() { format!("MCP resource {uri} from {server} returned no contents.") } else { text.join("\n\n") };
    Formatted { count: items.len(), text, attachments }
}

pub struct ResourceTool {
    pub kind: Kind,
    pub manager: McpManager,
}

impl ResourceTool {
    async fn list(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let templates = self.kind == Kind::ListTemplates;
        let server = optional_string(args, "server")?;
        let servers = self.manager.resource_servers();
        if let Some(server) = &server
            && !servers.contains(server)
        {
            let hint = if servers.is_empty() { String::new() } else { format!(". Available resource servers: {}", servers.join(", ")) };
            return Err(ToolError::Other(format!("MCP server \"{server}\" does not support resources{hint}")));
        }
        let patterns: Vec<String> = match &server {
            Some(server) => vec![format!("mcp:{server}:*")],
            None => servers.iter().map(|server| format!("mcp:{server}:*")).collect(),
        };
        let metadata = server.as_ref().map_or_else(|| json!({}), |server| json!({"server": server}));
        ctx.permissions.ask(PermissionRequest { permission: "read".into(), patterns: patterns.clone(), always: patterns, metadata }).await?;

        let (listed, id_key, out_key, label) = if templates {
            (self.manager.resource_templates(server.as_deref()).await, "uriTemplate", "resourceTemplates", "MCP resource templates")
        } else {
            (self.manager.resources(server.as_deref()).await, "uri", "resources", "MCP resources")
        };
        let field = |item: &Value, key: &str| item.get(key).and_then(Value::as_str).unwrap_or("undefined").to_string();
        let sort_key = |item: &Value| format!("{}\u{0}{}\u{0}{}", field(item, "client"), field(item, "name"), field(item, id_key));
        let mut items: Vec<Value> =
            listed.into_values().filter(|item| server.as_deref().is_none_or(|server| item["client"] == server)).collect();
        items.sort_by(|a, b| locale_cmp(&sort_key(a), &sort_key(b)));
        let mut body = serde_json::Map::new();
        body.insert(out_key.into(), items.iter().map(with_server).collect());
        let mut metadata = json!({"count": items.len(), "servers": servers});
        if let Some(server) = &server {
            metadata["server"] = server.clone().into();
        }
        Ok(ToolOutput {
            title: server.map_or_else(|| label.to_string(), |server| format!("{label}: {server}")),
            output: serde_json::to_string_pretty(&Value::Object(body)).unwrap_or_default(),
            metadata,
            attachments: Vec::new(),
        })
    }

    async fn read(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let server = required_string(args, "server")?;
        let uri = required_string(args, "uri")?;
        let clients = self.manager.clients();
        let Some(client) = clients.get(&server) else {
            return Err(ToolError::Other(format!("MCP server \"{server}\" is not connected")));
        };
        if !client.has_capability("resources") {
            return Err(ToolError::Other(format!("MCP server \"{server}\" does not support resources")));
        }
        ctx.permissions
            .ask(PermissionRequest {
                permission: "read".into(),
                patterns: vec![format!("mcp:{server}:{uri}")],
                always: vec![format!("mcp:{server}:*")],
                metadata: json!({"server": server, "uri": uri}),
            })
            .await?;
        let content = self
            .manager
            .read_resource(&server, &uri)
            .await
            .ok_or_else(|| ToolError::Other(format!("Failed to read MCP resource: {server}/{uri}")))?;
        let formatted = format_content(&server, &uri, &content);
        Ok(ToolOutput {
            title: format!("MCP resource: {uri}"),
            output: formatted.text,
            metadata: json!({"server": server, "uri": uri, "contents": formatted.count, "attachments": formatted.attachments.len()}),
            attachments: formatted.attachments,
        })
    }
}

#[async_trait::async_trait]
impl Tool for ResourceTool {
    fn id(&self) -> &str {
        match self.kind {
            Kind::List => LIST,
            Kind::ListTemplates => LIST_TEMPLATES,
            Kind::Read => READ,
        }
    }
    fn description(&self) -> String {
        String::new()
    }
    fn parameters(&self) -> Value {
        Value::Null
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        match self.kind {
            Kind::Read => self.read(&args, ctx).await,
            _ => self.list(&args, ctx).await,
        }
    }
}

// `SystemPrompt.mcp`: instructions of servers that still have a usable tool (or none at all).
pub fn system(instructions: &[ServerInstructions], ruleset: &[Rule]) -> Option<String> {
    let kept: Vec<&ServerInstructions> = instructions
        .iter()
        .filter(|item| item.tools.is_empty() || permission::disabled(item.tools.iter().map(String::as_str), ruleset).len() < item.tools.len())
        .collect();
    if kept.is_empty() {
        return None;
    }
    let mut lines = vec!["<mcp_instructions>".to_string()];
    for item in kept {
        lines.push(format!("  <server name=\"{}\">", item.name));
        lines.extend(item.instructions.split('\n').map(|line| format!("    {line}")));
        lines.push("  </server>".to_string());
    }
    lines.push("</mcp_instructions>".to_string());
    Some(lines.join("\n"))
}

// An MCP prompt exposed as a slash command; its template is fetched when used.
#[derive(Debug, Clone)]
pub struct PromptCommand {
    pub command: Command,
    pub server: String,
    pub prompt: String,
    pub arguments: Vec<String>,
}

pub async fn prompt_commands(manager: &McpManager) -> Vec<PromptCommand> {
    manager
        .prompts()
        .await
        .into_iter()
        .filter_map(|(key, prompt)| {
            let arguments: Vec<String> = prompt
                .get("arguments")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|argument| argument.get("name").and_then(Value::as_str).unwrap_or_default().to_string())
                .collect();
            Some(PromptCommand {
                command: Command {
                    name: key,
                    description: prompt.get("description").and_then(Value::as_str).map(str::to_string),
                    agent: None,
                    model: None,
                    source: Some(Source::Mcp),
                    template: String::new(),
                    subtask: None,
                    hints: (1..=arguments.len()).map(|i| format!("${i}")).collect(),
                },
                server: prompt.get("client")?.as_str()?.to_string(),
                prompt: prompt.get("name")?.as_str()?.to_string(),
                arguments,
            })
        })
        .collect()
}

// `getPrompt(client, name, {arg_i: "$<i+1>"})`, text messages joined with newlines.
pub async fn template(manager: &McpManager, command: &PromptCommand) -> String {
    let args: IndexMap<String, String> = command.arguments.iter().enumerate().map(|(i, name)| (name.clone(), format!("${}", i + 1))).collect();
    let Some(result) = manager.get_prompt(&command.server, &command.prompt, Some(args)).await else { return String::new() };
    let messages = result.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    let texts: Vec<&str> = messages
        .iter()
        .map(|message| match &message["content"] {
            content if content["type"] == "text" => content["text"].as_str().unwrap_or_default(),
            _ => "",
        })
        .collect();
    texts.join("\n")
}

pub enum Piece {
    Text(String),
    File(FilePart),
}

// A user-attached MCP resource, read at prompt time (`prompt.ts:703-783`).
pub async fn attached(manager: Option<&McpManager>, file: &FilePart, server: &str, uri: &str) -> Vec<Piece> {
    let name = file.filename.as_deref().unwrap_or("undefined");
    let mut pieces = vec![Piece::Text(format!("Reading MCP resource: {name} ({uri})"))];
    let content = match manager {
        Some(manager) => manager.read_resource(server, uri).await,
        None => None,
    };
    let Some(content) = content else {
        pieces.push(Piece::Text(format!("Failed to read MCP resource {name}: Resource not found: {server}/{uri}")));
        return pieces;
    };
    for item in contents(&content) {
        if let Some(text) = item.get("text").and_then(Value::as_str).filter(|text| !text.is_empty()) {
            pieces.push(Piece::Text(text.to_string()));
        } else if let Some(blob) = item.get("blob").and_then(Value::as_str).filter(|blob| !blob.is_empty()) {
            let mime = item.get("mimeType").and_then(Value::as_str).unwrap_or(&file.mime);
            let filename = item.get("uri").and_then(Value::as_str).map(str::to_string).or_else(|| file.filename.clone());
            let label = filename.as_deref().unwrap_or(uri);
            match blob_refusal(label, mime, blob) {
                Some(refusal) => pieces.push(Piece::Text(refusal)),
                None => {
                    pieces.push(Piece::Text(format!("[Binary MCP resource attached: {label} ({mime})]")));
                    pieces.push(Piece::File(FilePart { mime: mime.to_string(), filename, url: format!("data:{mime};base64,{blob}"), source: None }));
                }
            }
        }
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencode_core::permission::Action;

    #[test]
    fn system_block_skips_fully_disabled_servers() {
        let item = |name: &str, tools: &[&str]| ServerInstructions {
            name: name.into(),
            instructions: "line one\nline two".into(),
            tools: tools.iter().map(|t| t.to_string()).collect(),
        };
        let list = [item("alpha", &["alpha_a", "alpha_b"]), item("beta", &["beta_a"]), item("gamma", &[])];
        let rules = [Rule::new("beta_*", "*", Action::Deny), Rule::new("alpha_a", "*", Action::Deny)];
        let expected = "<mcp_instructions>\n  <server name=\"alpha\">\n    line one\n    line two\n  </server>\n  <server name=\"gamma\">\n    line one\n    line two\n  </server>\n</mcp_instructions>";
        assert_eq!(system(&list, &rules).as_deref(), Some(expected));
        assert_eq!(system(&list[1..2], &rules), None);
    }

    #[test]
    fn resource_content_formatting() {
        let content = json!({"contents": [
            {"uri": "a://t", "mimeType": "text/plain", "text": "hello"},
            {"uri": "a://z", "mimeType": "application/zip", "blob": "AAAA"},
            {"uri": "a://p", "mimeType": "image/png", "blob": "AAAA"},
            {"uri": "a://n"}
        ]});
        let out = format_content("srv", "a://t", &content);
        assert_eq!(out.count, 4);
        assert_eq!(
            out.text,
            "Resource: a://t\nMIME: text/plain\nhello\n\n[Binary MCP resource omitted: a://z (application/zip, 3 B) is not a supported attachment type]\n\n[Binary MCP resource attached: a://p (image/png)]\n\n[MCP resource content without text or blob: a://n]"
        );
        assert_eq!(out.attachments, [Attachment { mime: "image/png".into(), url: "data:image/png;base64,AAAA".into() }]);
        assert_eq!(format_content("srv", "u", &json!({"contents": []})).text, "MCP resource u from srv returned no contents.");
        assert_eq!((base64_size("AA=="), base64_size("AAA=")), (1, 2));
        assert_eq!((format_bytes(1023), format_bytes(1025), format_bytes(MAX_BLOB_BYTES)), ("1023 B".into(), "2 KB".into(), "10 MB".into()));
        assert_eq!(with_server(&json!({"name": "n", "client": "srv", "uri": "u"})).to_string(), r#"{"name":"n","uri":"u","server":"srv"}"#);
    }
}
