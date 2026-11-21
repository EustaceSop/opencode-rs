//! Scripted MCP server logic shared by the in-process HTTP test server and `examples/mcp_test_server.rs`.
#![allow(dead_code)]

use serde_json::{Value, json};

#[derive(Default)]
pub struct ServerState {
    pub grown: bool,
}

pub enum Step {
    Sleep(u64),
    Notify(Value),
    /// Ask the client for `roots/list` and append the answer to the reply text.
    AskRoots,
    Exit,
}

pub struct Outcome {
    pub steps: Vec<Step>,
    pub reply: Result<Value, (i64, String)>,
}

fn ok(value: Value) -> Outcome {
    Outcome {
        steps: Vec::new(),
        reply: Ok(value),
    }
}

fn text(value: impl Into<String>) -> Value {
    json!({"content": [{"type": "text", "text": value.into()}]})
}

fn tool(name: &str, description: &str, properties: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {"type": "object", "properties": properties},
        "outputSchema": {"$ref": "#/does/not/exist"},
    })
}

pub fn handle(state: &mut ServerState, method: &str, params: &Value) -> Outcome {
    let arg = |key: &str| {
        params
            .get("arguments")
            .and_then(|a| a.get(key))
            .cloned()
            .unwrap_or(Value::Null)
    };
    match method {
        "initialize" => ok(json!({
            "protocolVersion": params.get("protocolVersion").cloned().unwrap_or(Value::Null),
            "capabilities": {"tools": {"listChanged": true}, "resources": {}, "prompts": {}},
            "serverInfo": {"name": "mcp-test-server", "version": "0.0.0"},
            "instructions": "  Use the test tools.  ",
        })),
        "ping" => ok(json!({})),
        "tools/list" => match params.get("cursor").and_then(Value::as_str) {
            None => ok(json!({
                "tools": [
                    tool("echo", "Echo text", json!({"text": {"type": "string"}})),
                    tool("fail", "Always fails", json!({})),
                    tool("image", "Mixed content", json!({})),
                ],
                "nextCursor": "page-2",
            })),
            Some("page-2") => {
                let mut tools = vec![
                    tool("slow", "Sleeps", json!({"ms": {"type": "number"}})),
                    tool("slow_progress", "Sleeps with progress", json!({})),
                    tool("structured", "Structured only", json!({})),
                    tool("grow", "Adds a tool", json!({})),
                    tool(
                        "env",
                        "Reads the environment",
                        json!({"name": {"type": "string"}}),
                    ),
                    tool("roots", "Asks for roots", json!({})),
                    tool("die", "Exits", json!({})),
                    json!({"name": "weird.name", "inputSchema": {"type": "object", "required": ["x"]}}),
                ];
                if state.grown {
                    tools.push(tool("added", "Added later", json!({})));
                }
                ok(json!({"tools": tools}))
            }
            Some(other) => Outcome {
                steps: Vec::new(),
                reply: Err((-32602, format!("unknown cursor {other}"))),
            },
        },
        "tools/call" => match params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "echo" => ok(text(format!(
                "echo: {}",
                arg("text").as_str().unwrap_or_default()
            ))),
            "fail" => ok(json!({
                "isError": true,
                "content": [{"type": "text", "text": "boom"}, {"type": "text", "text": "  "}, {"type": "text", "text": "again"}],
            })),
            "image" => ok(json!({"content": [
                {"type": "text", "text": "caption"},
                {"type": "image", "mimeType": "image/png", "data": "QUJD"},
                {"type": "resource", "resource": {"uri": "mem://doc", "mimeType": "application/pdf", "blob": "QUI="}},
                {"type": "resource", "resource": {"uri": "mem://note", "text": "note"}},
            ]})),
            "slow" => Outcome {
                steps: vec![Step::Sleep(arg("ms").as_u64().unwrap_or(0))],
                reply: Ok(text("done")),
            },
            "slow_progress" => {
                let token = params
                    .get("_meta")
                    .and_then(|m| m.get("progressToken"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let progress = |n: u64| {
                    Step::Notify(json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/progress",
                        "params": {"progressToken": token, "progress": n},
                    }))
                };
                Outcome {
                    steps: vec![
                        Step::Sleep(250),
                        progress(1),
                        Step::Sleep(250),
                        progress(2),
                        Step::Sleep(250),
                    ],
                    reply: Ok(text("progressed")),
                }
            }
            "structured" => ok(json!({"content": [], "structuredContent": {"a": 1}})),
            "grow" => {
                state.grown = true;
                Outcome {
                    steps: vec![Step::Notify(
                        json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}),
                    )],
                    reply: Ok(text("grown")),
                }
            }
            "env" => {
                let value =
                    std::env::var(arg("name").as_str().unwrap_or_default()).unwrap_or_default();
                let cwd = std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                ok(text(format!("{value}|{cwd}")))
            }
            "roots" => Outcome {
                steps: vec![Step::AskRoots],
                reply: Ok(text("roots: ")),
            },
            "die" => Outcome {
                steps: vec![Step::Exit],
                reply: Ok(text("unreachable")),
            },
            other => ok(
                json!({"isError": true, "content": [{"type": "text", "text": format!("unknown tool {other}")}]}),
            ),
        },
        "resources/list" => ok(json!({"resources": [
            {"name": "readme", "uri": "file:///readme.md", "mimeType": "text/markdown"},
        ]})),
        "resources/templates/list" => ok(json!({"resourceTemplates": [
            {"name": "file", "uriTemplate": "file:///{path}"},
        ]})),
        "resources/read" => ok(json!({"contents": [
            {"uri": params.get("uri").cloned().unwrap_or(Value::Null), "text": "hello resource"},
        ]})),
        "prompts/list" => ok(json!({"prompts": [
            {"name": "greet user", "description": "Greets", "arguments": [{"name": "who", "required": true}]},
        ]})),
        "prompts/get" => {
            let who = params
                .get("arguments")
                .and_then(|a| a.get("who"))
                .and_then(Value::as_str)
                .unwrap_or("nobody");
            ok(
                json!({"messages": [{"role": "user", "content": {"type": "text", "text": format!("Hello {who}")}}]}),
            )
        }
        other => Outcome {
            steps: Vec::new(),
            reply: Err((-32601, format!("Method not found: {other}"))),
        },
    }
}

pub fn response(id: &Value, reply: Result<Value, (i64, String)>) -> Value {
    match reply {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
        }
    }
}
