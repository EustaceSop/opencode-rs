//! Stdio MCP server used by the crate's tests. `--hang` never answers, `--exit` quits immediately.

use std::io::{BufRead, Write};
use std::time::Duration;

use serde_json::{Value, json};

#[path = "../src/test_server.rs"]
mod test_server;

use test_server::{ServerState, Step, handle, response};

fn send(message: &Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{message}");
    let _ = out.flush();
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    if mode == "--exit" {
        return;
    }
    eprintln!("mcp-test-server starting");
    println!("this line is not JSON-RPC and must be ignored");
    let mut state = ServerState::default();
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    while let Some(Ok(line)) = lines.next() {
        if mode == "--hang" {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            continue;
        };
        let Some(id) = message.get("id") else {
            continue;
        };
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let mut outcome = handle(&mut state, method, &params);
        for step in outcome.steps {
            match step {
                Step::Sleep(ms) => std::thread::sleep(Duration::from_millis(ms)),
                Step::Notify(notification) => send(&notification),
                Step::Exit => std::process::exit(0),
                Step::AskRoots => {
                    send(&json!({"jsonrpc": "2.0", "id": "srv-1", "method": "roots/list"}));
                    let mut answer = Value::Null;
                    for line in lines.by_ref() {
                        let Ok(line) = line else { break };
                        let Ok(reply) = serde_json::from_str::<Value>(&line) else {
                            continue;
                        };
                        if reply.get("id").and_then(Value::as_str) == Some("srv-1") {
                            answer = reply.get("result").cloned().unwrap_or(Value::Null);
                            break;
                        }
                    }
                    if let Ok(result) = &mut outcome.reply {
                        result["content"][0]["text"] = format!("roots: {answer}").into();
                    }
                }
            }
        }
        send(&response(id, outcome.reply));
    }
}
