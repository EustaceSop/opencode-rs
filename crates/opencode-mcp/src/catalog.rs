//! Listing, naming and result conversion (`mcp/catalog.ts`, `session/tools.ts:426-470`).

use std::collections::HashSet;
use std::time::Duration;

use opencode_tools::{Attachment, ToolOutput};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::client::{Client, RequestOptions};
use crate::error::McpError;

pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_LIST_PAGES: usize = 1_000;
const MAX_RESOURCE_BLOB_BYTES: u64 = 10 * 1024 * 1024;
const ATTACHMENT_MIMES: [&str; 5] = [
    "application/pdf",
    "image/gif",
    "image/jpeg",
    "image/png",
    "image/webp",
];

/// `s.replace(/[^a-zA-Z0-9_-]/g, "_")`: one `_` per UTF-16 code unit.
pub fn sanitize(value: &str) -> String {
    value
        .encode_utf16()
        .map(|unit| match char::from_u32(unit as u32) {
            Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '-' => c,
            _ => '_',
        })
        .collect()
}

pub fn tool_name(server: &str, tool: &str) -> String {
    format!("{}_{}", sanitize(server), sanitize(tool))
}

pub fn prompt_key(server: &str, prompt: &str) -> String {
    format!("{}:{}", sanitize(server), sanitize(prompt))
}

/// `server:uri` with `%` and `:` of the server name escaped so the key stays unambiguous.
pub fn resource_key(server: &str, uri: &str) -> String {
    format!("{}:{uri}", server.replace('%', "%25").replace(':', "%3A"))
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    /// The definition exactly as the server sent it.
    pub raw: Value,
}

impl ToolDef {
    fn parse(raw: Value) -> Result<Self, McpError> {
        let name = raw
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| McpError::other("Invalid tools/list result: tool without a name"))?
            .to_string();
        let input_schema = raw
            .get("inputSchema")
            .filter(|schema| schema.is_object())
            .cloned()
            .ok_or_else(|| {
                McpError::Other(format!(
                    "Invalid tools/list result: tool {name} has no inputSchema"
                ))
            })?;
        Ok(Self {
            description: raw
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            name,
            input_schema,
            raw,
        })
    }

    /// `{...inputSchema, type: "object", properties: inputSchema.properties ?? {}, additionalProperties: false}`.
    pub fn parameters(&self) -> Value {
        let mut schema = self.input_schema.as_object().cloned().unwrap_or_default();
        let properties = schema
            .get("properties")
            .filter(|p| !p.is_null())
            .cloned()
            .unwrap_or_else(|| json!({}));
        schema.insert("type".into(), "object".into());
        schema.insert("properties".into(), properties);
        schema.insert("additionalProperties".into(), false.into());
        Value::Object(schema)
    }
}

async fn paginate(
    client: &Client,
    method: &str,
    field: &str,
    timeout: Option<Duration>,
) -> Result<Vec<Value>, McpError> {
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_LIST_PAGES {
        let params = cursor.as_ref().map(|cursor| json!({"cursor": cursor}));
        let options = RequestOptions {
            timeout,
            ..RequestOptions::default()
        };
        let page = client.request(method, params, options).await?;
        match page.get(field) {
            Some(Value::Array(list)) => items.extend(list.iter().cloned()),
            _ => {
                return Err(McpError::Other(format!(
                    "Invalid {method} result: missing {field}"
                )));
            }
        }
        let Some(next) = page.get("nextCursor").and_then(Value::as_str) else {
            return Ok(items);
        };
        if !seen.insert(next.to_string()) {
            return Err(McpError::Other(format!(
                "MCP list returned duplicate cursor: {next}"
            )));
        }
        cursor = Some(next.to_string());
    }
    Err(McpError::Other(format!(
        "MCP list exceeded {MAX_LIST_PAGES} pages"
    )))
}

/// `tools/list`; output schemas are never validated, so the original's tolerant retry is implicit.
pub async fn list_tools(
    client: &Client,
    timeout_ms: Option<u64>,
) -> Result<Vec<ToolDef>, McpError> {
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
    paginate(client, "tools/list", "tools", Some(timeout))
        .await?
        .into_iter()
        .map(ToolDef::parse)
        .collect()
}

fn named(items: Vec<Value>) -> Vec<Value> {
    items
        .into_iter()
        .filter(|item| item.get("name").is_some_and(Value::is_string))
        .collect()
}

pub async fn list_prompts(
    client: &Client,
    timeout_ms: Option<u64>,
) -> Result<Vec<Value>, McpError> {
    if !client.has_capability("prompts") {
        return Ok(Vec::new());
    }
    let timeout = timeout_ms.map(Duration::from_millis);
    paginate(client, "prompts/list", "prompts", timeout)
        .await
        .map(named)
}

pub async fn list_resources(
    client: &Client,
    timeout_ms: Option<u64>,
) -> Result<Vec<Value>, McpError> {
    if !client.has_capability("resources") {
        return Ok(Vec::new());
    }
    let timeout = timeout_ms.map(Duration::from_millis);
    paginate(client, "resources/list", "resources", timeout)
        .await
        .map(named)
}

pub async fn list_resource_templates(
    client: &Client,
    timeout_ms: Option<u64>,
) -> Result<Vec<Value>, McpError> {
    if !client.has_capability("resources") {
        return Ok(Vec::new());
    }
    let timeout = timeout_ms.map(Duration::from_millis);
    paginate(
        client,
        "resources/templates/list",
        "resourceTemplates",
        timeout,
    )
    .await
    .map(named)
}

/// `tools/call` as `convertTool().execute`: error results become `Err`, `structuredContent` backfills
/// an empty `content`.
pub async fn call_tool(
    client: &Client,
    name: &str,
    args: Value,
    timeout_ms: Option<u64>,
    abort: Option<CancellationToken>,
) -> Result<Value, McpError> {
    let arguments = match args {
        Value::Object(map) => Value::Object(map),
        _ => Value::Object(Map::new()),
    };
    let options = RequestOptions {
        timeout: timeout_ms.map(Duration::from_millis),
        reset_timeout_on_progress: true,
        abort,
    };
    let mut result = client
        .request(
            "tools/call",
            Some(json!({"name": name, "arguments": arguments})),
            options,
        )
        .await?;
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if result.get("isError").is_some_and(crate::client::js_truthy) {
        let text = content
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        return Err(McpError::Other(if text.is_empty() {
            "MCP tool returned an error".to_string()
        } else {
            text
        }));
    }
    if !result.is_object() {
        result = json!({});
    }
    let structured = result
        .get("structuredContent")
        .filter(|v| !v.is_null())
        .cloned();
    if content.is_empty() {
        result["content"] = match structured {
            Some(structured) => json!([{"type": "text", "text": structured.to_string()}]),
            None => json!([]),
        };
    }
    Ok(result)
}

fn base64_size(value: &str) -> u64 {
    let trimmed: String = value.chars().filter(|c| !c.is_whitespace()).collect();
    let padding = if trimmed.ends_with("==") {
        2
    } else if trimmed.ends_with('=') {
        1
    } else {
        0
    };
    ((trimmed.len() as u64 * 3) / 4).saturating_sub(padding)
}

fn format_bytes(value: u64) -> String {
    if value < 1024 {
        return format!("{value} B");
    }
    if value < 1024 * 1024 {
        return format!("{} KB", value.div_ceil(1024));
    }
    format!("{} MB", value.div_ceil(1024 * 1024))
}

/// Text blocks joined with blank lines; images and supported resource blobs become attachments.
pub fn tool_output(result: &Value) -> ToolOutput {
    let mut text: Vec<String> = Vec::new();
    let mut attachments = Vec::new();
    let content = result.get("content").and_then(Value::as_array);
    for item in content.into_iter().flatten() {
        let field = |key: &str| item.get(key).and_then(Value::as_str);
        match field("type") {
            Some("text") => text.push(field("text").unwrap_or_default().to_string()),
            Some("image") => {
                let mime = field("mimeType").unwrap_or_default();
                attachments.push(Attachment {
                    mime: mime.to_string(),
                    url: format!("data:{mime};base64,{}", field("data").unwrap_or_default()),
                });
            }
            Some("resource") => {
                let Some(resource) = item.get("resource") else {
                    continue;
                };
                let field = |key: &str| resource.get(key).and_then(Value::as_str);
                if let Some(body) = field("text").filter(|t| !t.is_empty()) {
                    text.push(body.to_string());
                }
                let Some(blob) = field("blob").filter(|b| !b.is_empty()) else {
                    continue;
                };
                let uri = field("uri").unwrap_or_default();
                let mime = field("mimeType").unwrap_or("application/octet-stream");
                let size = base64_size(blob);
                if !ATTACHMENT_MIMES.contains(&mime) {
                    text.push(format!(
                        "[Binary MCP resource omitted: {uri} ({mime}, {}) is not a supported attachment type]",
                        format_bytes(size)
                    ));
                    continue;
                }
                if size > MAX_RESOURCE_BLOB_BYTES {
                    text.push(format!(
                        "[Binary MCP resource omitted: {uri} ({mime}, {}) exceeds {}]",
                        format_bytes(size),
                        format_bytes(MAX_RESOURCE_BLOB_BYTES)
                    ));
                    continue;
                }
                attachments.push(Attachment {
                    mime: mime.to_string(),
                    url: format!("data:{mime};base64,{blob}"),
                });
            }
            _ => {}
        }
    }
    let metadata = match result.get("metadata") {
        Some(Value::Object(map)) => Value::Object(map.clone()),
        _ => json!({}),
    };
    ToolOutput {
        title: String::new(),
        output: text.join("\n\n"),
        metadata,
        attachments,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_keys() {
        assert_eq!(sanitize("my.server/x y"), "my_server_x_y");
        assert_eq!(sanitize("a-b_C9"), "a-b_C9");
        assert_eq!(sanitize("é😀"), "___");
        assert_eq!(tool_name("git hub", "list.issues"), "git_hub_list_issues");
        assert_eq!(prompt_key("a b", "p/q"), "a_b:p_q");
        assert_eq!(resource_key("a:b%c", "file:///x"), "a%3Ab%25c:file:///x");
    }

    #[test]
    fn schema_passthrough() {
        let def = ToolDef::parse(json!({
            "name": "t",
            "inputSchema": {"type": "object", "required": ["a"], "additionalProperties": true, "$defs": {"x": {}}},
            "outputSchema": {"$ref": "#/nowhere"},
        }))
        .unwrap();
        assert_eq!(
            def.parameters().to_string(),
            r#"{"type":"object","required":["a"],"additionalProperties":false,"$defs":{"x":{}},"properties":{}}"#
        );
        assert!(ToolDef::parse(json!({"name": "t"})).is_err());
        assert!(ToolDef::parse(json!({"inputSchema": {}})).is_err());
    }

    #[test]
    fn result_conversion() {
        let big = "A".repeat(4 * 1024 * 1024 * 4);
        let out = tool_output(&json!({"content": [
            {"type": "text", "text": "one"},
            {"type": "image", "mimeType": "image/png", "data": "QUJD"},
            {"type": "resource", "resource": {"uri": "r://t", "text": "two"}},
            {"type": "resource", "resource": {"uri": "r://pdf", "mimeType": "application/pdf", "blob": "QUI="}},
            {"type": "resource", "resource": {"uri": "r://bin", "blob": "QUJDRA=="}},
            {"type": "resource", "resource": {"uri": "r://big", "mimeType": "image/png", "blob": big}},
            {"type": "audio", "data": "x", "mimeType": "audio/wav"},
        ]}));
        assert_eq!(
            out.output,
            "one\n\ntwo\n\n[Binary MCP resource omitted: r://bin (application/octet-stream, 4 B) is not a supported attachment type]\n\n[Binary MCP resource omitted: r://big (image/png, 12 MB) exceeds 10 MB]"
        );
        assert_eq!(
            out.attachments,
            vec![
                Attachment {
                    mime: "image/png".into(),
                    url: "data:image/png;base64,QUJD".into()
                },
                Attachment {
                    mime: "application/pdf".into(),
                    url: "data:application/pdf;base64,QUI=".into()
                },
            ]
        );
        assert_eq!(out.title, "");
        assert_eq!(out.metadata, json!({}));
    }

    #[test]
    fn byte_formatting() {
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1025), "2 KB");
        assert_eq!(format_bytes(10 * 1024 * 1024), "10 MB");
        assert_eq!(base64_size("QUI=\n"), 2);
    }
}
