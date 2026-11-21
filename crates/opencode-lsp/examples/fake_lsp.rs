//! Minimal language server for the crate's tests.
//!
//! Flags: `--pull` (pull diagnostics instead of push), `--incremental` (advertise incremental sync),
//! `--die` (exit before speaking). A line containing `ERR` is an error; every document gets one warning.

use std::io::{BufRead, BufReader, Write};

use serde_json::{Value, json};

fn send(message: &Value) {
    let body = message.to_string();
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "Content-Length: {}\r\n\r\n{body}", body.len());
    let _ = out.flush();
}

fn read(reader: &mut impl BufRead) -> Option<Value> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0u8; length?];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn range(line: usize) -> Value {
    json!({ "start": { "line": line, "character": 0 }, "end": { "line": line, "character": 3 } })
}

fn diagnose(text: &str, label: &str) -> Vec<Value> {
    let mut items: Vec<Value> = text
        .lines()
        .enumerate()
        .filter(|(_, line)| line.contains("ERR"))
        .map(|(index, _)| json!({ "range": range(index), "severity": 1, "source": "fake", "message": label }))
        .collect();
    items.push(
        json!({ "range": range(0), "severity": 2, "source": "fake", "message": "fake warning" }),
    );
    items
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    if has("--die") {
        std::process::exit(1);
    }
    let pull = has("--pull");
    let mut documents: std::collections::HashMap<String, String> = Default::default();
    let mut config = Value::Null;
    let mut last_change = "none";
    let mut reader = BufReader::new(std::io::stdin().lock());

    while let Some(message) = read(&mut reader) {
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            if id == Some(json!("cfg")) {
                config = message.get("result").cloned().unwrap_or(Value::Null);
            }
            continue;
        };
        let uri = params
            .pointer("/textDocument/uri")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let reply = |result: Value| send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
        let publish = |uri: &str, text: &str| {
            if !pull {
                let diagnostics = diagnose(text, "bad line");
                send(&json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/publishDiagnostics",
                    "params": { "uri": uri, "diagnostics": diagnostics },
                }));
            }
        };
        match method {
            "initialize" => {
                let mut capabilities =
                    json!({ "textDocumentSync": if has("--incremental") { 2 } else { 1 } });
                if pull {
                    capabilities["diagnosticProvider"] = json!({ "interFileDependencies": false });
                }
                reply(json!({ "capabilities": capabilities }));
            }
            "initialized" => send(&json!({
                "jsonrpc": "2.0",
                "id": "cfg",
                "method": "workspace/configuration",
                "params": { "items": [{ "section": "fake.value" }] },
            })),
            "textDocument/didOpen" => {
                let text = params
                    .pointer("/textDocument/text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                publish(&uri, &text);
                documents.insert(uri, text);
            }
            "textDocument/didChange" => {
                let change = params
                    .pointer("/contentChanges/0")
                    .cloned()
                    .unwrap_or(Value::Null);
                last_change = if change.get("range").is_some() {
                    "range"
                } else {
                    "full"
                };
                let text = change
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                publish(&uri, &text);
                documents.insert(uri, text);
            }
            "textDocument/diagnostic" => {
                let text = documents.get(&uri).cloned().unwrap_or_default();
                reply(json!({ "kind": "full", "items": diagnose(&text, "pull error") }));
            }
            "textDocument/hover" => {
                let line = params
                    .pointer("/position/line")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let character = params
                    .pointer("/position/character")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                reply(
                    json!({ "contents": format!("hover {line}:{character} config={config} change={last_change}") }),
                );
            }
            "textDocument/definition" => reply(json!({ "uri": uri, "range": range(0) })),
            "textDocument/references" => {
                reply(json!([{ "uri": uri, "range": range(0) }, { "uri": uri, "range": range(1) }]))
            }
            "textDocument/documentSymbol" => reply(
                json!([{ "name": "main", "kind": 12, "range": range(0), "selectionRange": range(0) }]),
            ),
            "workspace/symbol" => {
                let query = params
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let symbols: Vec<Value> = (0..15)
                    .map(|index| {
                        json!({
                            "name": format!("{query}{index}"),
                            "kind": if index % 5 == 0 { 1 } else { 12 },
                            "location": { "uri": "file:///fake", "range": range(index) },
                        })
                    })
                    .collect();
                reply(Value::Array(symbols));
            }
            _ if id.is_some() => send(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("Unhandled method {method}") },
            })),
            _ => {}
        }
    }
}
