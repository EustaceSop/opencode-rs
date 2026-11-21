// Runs the built binary against a scripted provider; every path it can touch is redirected into a temp dir.
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn sse(chunks: &[Value]) -> String {
    let mut out: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    out.push_str("data: [DONE]\n\n");
    out
}

fn text_reply(text: &str) -> String {
    sse(&[
        json!({"choices": [{"index": 0, "delta": {"content": text}}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 20, "completion_tokens": 2, "total_tokens": 22}}),
    ])
}

fn tool_reply(name: &str, arguments: &Value) -> String {
    sse(&[
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": name, "arguments": arguments.to_string()}}]}}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}], "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}}),
    ])
}

type Seen = Arc<Mutex<Vec<String>>>;

// Serves the scripted replies in order and records request bodies; title requests never consume the script.
async fn provider(responses: Vec<String>, seen: Seen) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let script = Arc::new(Mutex::new(VecDeque::from(responses)));
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (script, seen) = (script.clone(), seen.clone());
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let body = loop {
                    let mut chunk = [0u8; 8192];
                    let n = socket.read(&mut chunk).await.unwrap();
                    buffer.extend_from_slice(&chunk[..n]);
                    let Some(split) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                    let head = String::from_utf8_lossy(&buffer[..split]).to_lowercase();
                    let length: usize = head.lines().find_map(|l| l.strip_prefix("content-length:")).unwrap().trim().parse().unwrap();
                    if buffer.len() >= split + 4 + length {
                        break String::from_utf8_lossy(&buffer[split + 4..]).into_owned();
                    }
                };
                let response = if body.contains("Generate a title for this conversation") {
                    text_reply("Mock title")
                } else {
                    seen.lock().unwrap().push(body);
                    script.lock().unwrap().pop_front().expect("unscripted provider request")
                };
                let reply = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{response}", response.len());
                socket.write_all(reply.as_bytes()).await.unwrap();
                socket.shutdown().await.ok();
            });
        }
    });
    port
}

fn stdio_server() -> String {
    static BUILT: std::sync::Once = std::sync::Once::new();
    let exe = std::env::current_exe().unwrap();
    let debug = exe.parent().unwrap().parent().unwrap();
    let path = debug.join("examples").join(format!("mcp_test_server{}", std::env::consts::EXE_SUFFIX));
    // A workspace test run builds it; a single-crate run has to ask for it.
    BUILT.call_once(|| {
        if !path.exists() {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let status = std::process::Command::new(cargo).args(["build", "-p", "opencode-mcp", "--example", "mcp_test_server"]).status();
            assert!(status.is_ok_and(|s| s.success()), "could not build the MCP test server");
        }
    });
    assert!(path.exists(), "missing test server at {}", path.display());
    path.to_string_lossy().into_owned()
}

struct Sandbox {
    _root: tempfile::TempDir,
    base: PathBuf,
    project: PathBuf,
    seen: Seen,
}

struct Output {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Sandbox {
    // `extra` is merged over a project config that points provider `mock` at the scripted server.
    async fn new(responses: Vec<String>, extra: Value) -> Self {
        Self::scripted(|_| responses, extra).await
    }

    async fn scripted(responses: impl FnOnce(&Path) -> Vec<String>, extra: Value) -> Self {
        let root = tempfile::tempdir().unwrap();
        let canonical = root.path().canonicalize().unwrap();
        let base = PathBuf::from(canonical.to_str().unwrap().trim_start_matches(r"\\?\"));
        let project = base.join("project");
        // The marker makes the project its own worktree, so nothing above the temp dir is searched.
        std::fs::create_dir_all(project.join(".git")).unwrap();
        let seen = Seen::default();
        let port = provider(responses(&project), seen.clone()).await;
        let mut config = json!({
            "model": "mock/m1",
            "provider": {"mock": {"options": {"baseURL": format!("http://127.0.0.1:{port}/v1"), "apiKey": "k"}, "models": {"m1": {"name": "Mock"}}}}
        });
        opencode_core::config::merge_deep(&mut config, extra);
        std::fs::write(project.join("opencode.json"), config.to_string()).unwrap();
        Sandbox { _root: root, base, project, seen }
    }

    fn data(&self) -> PathBuf {
        self.base.join("xdg-data").join("opencode")
    }

    fn config(&self) -> PathBuf {
        self.base.join("xdg-config").join("opencode")
    }

    async fn run(&self, args: &[&str]) -> Output {
        self.run_with(args, &[], None).await
    }

    fn command(&self, args: &[&str], envs: &[(&str, &str)]) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_opencode"));
        for (key, _) in std::env::vars() {
            if key.starts_with("OPENCODE_") || key.starts_with("XDG_") {
                command.env_remove(key);
            }
        }
        let home = self.base.join("home");
        std::fs::create_dir_all(&home).unwrap();
        command
            .args(args)
            .current_dir(&self.project)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("XDG_DATA_HOME", self.base.join("xdg-data"))
            .env("XDG_CONFIG_HOME", self.base.join("xdg-config"))
            .env("XDG_CACHE_HOME", self.base.join("xdg-cache"))
            .env("XDG_STATE_HOME", self.base.join("xdg-state"))
            .env("OPENCODE_TEST_MANAGED_CONFIG_DIR", self.base.join("managed"))
            .env("OPENCODE_DISABLE_MODELS_FETCH", "1")
            .env("OPENCODE_TEST_NO_BROWSER", "1")
            .envs(envs.iter().copied())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    // `stdin` is piped to the command; without it stdin is closed.
    async fn run_with(&self, args: &[&str], envs: &[(&str, &str)], stdin: Option<&str>) -> Output {
        let mut command = self.command(args, envs);
        command.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
        let mut child = command.spawn().unwrap();
        if let Some(text) = stdin {
            let mut pipe = child.stdin.take().unwrap();
            pipe.write_all(text.as_bytes()).await.unwrap();
        }
        let output = tokio::time::timeout(std::time::Duration::from_secs(60), child.wait_with_output()).await.expect("the command hung").unwrap();
        Output {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
            stderr: String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n"),
        }
    }

    async fn sessions(&self) -> Vec<Value> {
        let listed = self.run(&["session", "list", "--format", "json"]).await;
        serde_json::from_str::<Value>(&listed.stdout).unwrap_or_else(|e| panic!("{e}: {}{}", listed.stdout, listed.stderr)).as_array().unwrap().clone()
    }
}

#[tokio::test]
async fn run_prints_the_header_tools_and_text() {
    let script = |project: &Path| vec![tool_reply("read", &json!({"filePath": project.join("notes.txt").to_string_lossy()})), text_reply("All done.")];
    let sandbox = Sandbox::scripted(script, json!({})).await;
    std::fs::write(sandbox.project.join("notes.txt"), "remember").unwrap();

    let out = sandbox.run(&["run", "read", "the", "notes", "--title", ""]).await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    // Not a terminal: finished text goes to stdout as a bare line, everything else to stderr.
    assert_eq!(out.stdout, "All done.\n");
    assert!(out.stderr.contains("> build · m1"), "{}", out.stderr);
    assert!(out.stderr.contains("Read notes.txt"), "{}", out.stderr);
    assert!(out.stderr.find("> build · m1").unwrap() < out.stderr.find("Read notes.txt").unwrap());
    assert!(sandbox.seen.lock().unwrap()[0].contains("read the notes"));

    // `--title ""` names the session after the prompt; such a session denies the question tool.
    let sessions = sandbox.sessions().await;
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["title"], "read the notes");
    assert!(!sandbox.seen.lock().unwrap()[0].contains("\"name\":\"question\""));
}

#[tokio::test]
async fn run_format_json_emits_the_event_schema() {
    let script = |project: &Path| vec![tool_reply("read", &json!({"filePath": project.join("opencode.json").to_string_lossy()})), text_reply("Done.")];
    let sandbox = Sandbox::scripted(script, json!({})).await;
    let out = sandbox.run(&["run", "--format", "json", "look"]).await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(!out.stderr.contains("> build"), "{}", out.stderr);
    let events: Vec<Value> = out.stdout.lines().map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}"))).collect();
    let kinds: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["step_start", "tool_use", "step_finish", "step_start", "text", "step_finish"]);
    let session_id = events[0]["sessionID"].as_str().unwrap();
    assert!(session_id.starts_with("ses"));
    for event in &events {
        assert!(event["timestamp"].is_u64());
        assert_eq!(event["sessionID"], session_id);
        let keys: Vec<&str> = event.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["type", "timestamp", "sessionID", "part"]);
        assert_eq!(event["part"]["sessionID"], session_id);
    }
    assert_eq!((&events[0]["part"]["type"], &events[2]["part"]["type"]), (&json!("step-start"), &json!("step-finish")));
    assert_eq!((&events[1]["part"]["tool"], &events[1]["part"]["state"]["status"]), (&json!("read"), &json!("completed")));
    assert_eq!((&events[4]["part"]["type"], &events[4]["part"]["text"]), (&json!("text"), &json!("Done.")));
    assert!(events[4]["part"]["time"]["end"].is_u64());

    // A failed call is an `error` event and exit code 1.
    let failed = sandbox.run(&["run", "--format", "json", "-m", "nope/none", "hi"]).await;
    assert_eq!(failed.code, 1);
    let event: Value = serde_json::from_str(failed.stdout.lines().last().unwrap()).unwrap();
    assert_eq!((&event["type"], &event["error"]["name"]), (&json!("error"), &json!("UnknownError")));
    assert!(event["error"]["data"]["message"].as_str().unwrap().starts_with("Model not found: nope/none."));
}

#[tokio::test]
async fn run_takes_files_commands_and_rejects_bad_input() {
    let extra = json!({"permission": {"edit": "ask"}, "command": {"greet": {"template": "Say hello to $1"}}});
    let script = |project: &Path| {
        let write = tool_reply("write", &json!({"filePath": project.join("out.txt").to_string_lossy(), "content": "x"}));
        vec![text_reply("Summarized."), text_reply("Hello Bob."), write.clone(), write, text_reply("Written.")]
    };
    let sandbox = Sandbox::scripted(script, extra).await;
    std::fs::write(sandbox.project.join("notes.txt"), "remember the milk").unwrap();
    let seen = sandbox.seen.clone();

    let out = sandbox.run(&["run", "-f", "notes.txt", "summarize"]).await;
    assert_eq!((out.code, out.stdout.as_str()), (0, "Summarized.\n"), "{}", out.stderr);
    let request = seen.lock().unwrap()[0].clone();
    assert!(request.contains("Called the Read tool with the following input") && request.contains("remember the milk"), "{request}");

    let out = sandbox.run(&["run", "--command", "greet", "Bob"]).await;
    assert_eq!((out.code, out.stdout.as_str()), (0, "Hello Bob.\n"), "{}", out.stderr);
    assert!(seen.lock().unwrap()[1].contains("Say hello to Bob"));

    // Without --auto a permission request is rejected and the turn ends there.
    let out = sandbox.run(&["run", "write the file"]).await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stderr.contains("permission requested: edit (") && out.stderr.contains("; auto-rejecting"), "{}", out.stderr);
    assert!(out.stderr.contains("Write out.txt failed"), "{}", out.stderr);
    assert!(!sandbox.project.join("out.txt").exists());
    let out = sandbox.run(&["run", "--auto", "write the file"]).await;
    assert_eq!((out.code, out.stdout.as_str()), (0, "Written.\n"), "{}", out.stderr);
    assert!(sandbox.project.join("out.txt").exists());

    for (args, message) in [
        (vec!["run"], "You must provide a message or a command"),
        (vec!["run", "-f", "missing.txt", "x"], "File not found: missing.txt"),
        (vec!["run", "--fork", "x"], "--fork requires --continue or --session"),
        (vec!["run", "-s", "ses_missing", "x"], "Session not found"),
        (vec!["run", "-m", "nope/none", "x"], "Model not found: nope/none."),
    ] {
        let out = sandbox.run(&args).await;
        assert_eq!(out.code, 1, "{args:?}");
        assert!(out.stderr.contains("Error: ") && out.stderr.contains(message), "{args:?}: {}", out.stderr);
        assert_eq!(out.stdout, "", "{args:?}");
    }
    assert_eq!(seen.lock().unwrap().len(), 5);
}

#[tokio::test]
async fn export_and_import_round_trip() {
    let sandbox = Sandbox::new(vec![text_reply("Hello there.")], json!({})).await;
    assert_eq!(sandbox.run(&["run", "hi"]).await.code, 0);
    let session_id = sandbox.sessions().await[0]["id"].as_str().unwrap().to_string();

    let exported = sandbox.run(&["export", &session_id]).await;
    assert_eq!(exported.code, 0, "{}", exported.stderr);
    assert!(exported.stderr.contains(&format!("Exporting session: {session_id}")));
    let data: Value = serde_json::from_str(&exported.stdout).unwrap();
    assert_eq!(data["info"]["id"], session_id.as_str());
    let messages = data["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!((&messages[0]["info"]["role"], &messages[0]["parts"][0]["text"]), (&json!("user"), &json!("hi")));
    assert!(messages[1]["parts"].as_array().unwrap().iter().any(|p| p["text"] == "Hello there."));
    // Pretty-printed with two spaces, like JSON.stringify(data, null, 2).
    assert!(exported.stdout.starts_with("{\n  \"info\": {\n    \"id\""), "{}", exported.stdout);
    assert_eq!(sandbox.run(&["export"]).await.stdout, exported.stdout);

    let file = sandbox.base.join("export.json");
    std::fs::write(&file, &exported.stdout).unwrap();
    assert_eq!(sandbox.run(&["session", "delete", &session_id]).await.code, 0);
    assert_eq!(sandbox.run(&["export", &session_id]).await.code, 1);
    let imported = sandbox.run(&["import", file.to_str().unwrap()]).await;
    assert_eq!((imported.code, imported.stdout.as_str()), (0, format!("Imported session: {session_id}\n").as_str()), "{}", imported.stderr);
    // Importing twice changes nothing; the totals are the exported ones, not doubled.
    assert_eq!(sandbox.run(&["import", file.to_str().unwrap()]).await.code, 0);
    assert_eq!(sandbox.run(&["export", &session_id]).await.stdout, exported.stdout);

    let missing = sandbox.run(&["import", "nowhere.json"]).await;
    assert_eq!(missing.code, 1);
    assert!(missing.stderr.contains("File not found: nowhere.json"));
    let shared = sandbox.run(&["import", "https://opncd.ai/share/x"]).await;
    assert!(shared.code == 1 && shared.stderr.contains("Importing from share URLs is no longer supported"));
}

#[tokio::test]
async fn import_from_other_agents_hint_list_dry_run_and_apply() {
    let sandbox = Sandbox::new(vec![text_reply("One."), text_reply("Two.")], json!({})).await;
    let home = sandbox.base.join("home");
    std::fs::create_dir_all(home.join(".claude/agents")).unwrap();
    std::fs::write(home.join(".claude.json"), r#"{"mcpServers":{"demo":{"command":"srv","args":["--x"]}}}"#).unwrap();
    std::fs::write(home.join(".claude/agents/helper.md"), "---\ndescription: Helps\n---\nHelp out.").unwrap();
    let source = std::fs::read(home.join(".claude.json")).unwrap();

    // The first run mentions the data once; the next one does not.
    let first = sandbox.run(&["run", "hi"]).await;
    assert_eq!(first.code, 0, "{}", first.stderr);
    assert!(first.stderr.contains("Found data from Claude Code; run `opencode import` to choose what to bring over."), "{}", first.stderr);
    let second = sandbox.run(&["run", "again"]).await;
    assert!(!second.stderr.contains("Found data from"), "{}", second.stderr);
    let state: Value = serde_json::from_str(&std::fs::read_to_string(sandbox.base.join("xdg-state/opencode/import-rs.json")).unwrap()).unwrap();
    assert_eq!(state["asked"], true);

    let list = sandbox.run(&["import", "--list"]).await;
    assert_eq!(list.code, 0, "{}", list.stderr);
    assert!(list.stdout.contains("Claude Code (claude-code): found\n  MCP servers 1, agents 1, skills 0, sessions 0"), "{}", list.stdout);
    assert!(list.stdout.contains("Codex: not found"), "{}", list.stdout);
    let dry = sandbox.run(&["import", "--dry-run", "--category", "mcp"]).await;
    assert!(dry.code == 0 && dry.stdout.contains("Claude Code\n  MCP servers: 1 to import, 0 skipped"), "{}", dry.stdout);
    assert!(!dry.stdout.contains("agents:"), "{}", dry.stdout);
    let rust = sandbox.base.join("xdg-config/opencode-rs");
    assert!(!rust.exists());

    let unattended = sandbox.run(&["import"]).await;
    assert!(unattended.code == 1 && unattended.stderr.contains("Not a terminal"), "{}", unattended.stderr);
    let unknown = sandbox.run(&["import", "--source", "nope", "--yes"]).await;
    assert!(unknown.code == 1 && unknown.stderr.contains("Unknown source: nope"), "{}", unknown.stderr);

    let applied = sandbox.run(&["import", "--yes", "--source", "claude-code"]).await;
    assert_eq!(applied.code, 0, "{}", applied.stderr);
    assert!(applied.stderr.contains("Claude Code MCP servers: 1 imported"), "{}", applied.stderr);
    assert!(rust.join("agents/helper.md").is_file());
    let config = sandbox.run(&["debug", "config"]).await;
    let config: Value = serde_json::from_str(&config.stdout).unwrap();
    assert_eq!(config["mcp"]["demo"], json!({"type": "local", "command": ["srv", "--x"]}));
    assert_eq!(config["agent"]["helper"]["prompt"], "Help out.");
    assert!(!sandbox.config().join("opencode.json").exists() && !sandbox.config().join("opencode.jsonc").exists());
    assert_eq!(std::fs::read(home.join(".claude.json")).unwrap(), source);

    let repeat = sandbox.run(&["import", "--yes", "--source", "claude-code"]).await;
    assert!(repeat.code == 0 && repeat.stderr.contains("Claude Code MCP servers: 0 imported, 1 unchanged"), "{}", repeat.stderr);
}

#[tokio::test]
async fn mcp_commands_edit_the_global_config_and_report_status() {
    let sandbox = Sandbox::new(vec![], json!({})).await;
    let empty = sandbox.run(&["mcp", "list"]).await;
    assert!(empty.code == 0 && empty.stderr.contains("No MCP servers configured"), "{}", empty.stderr);

    let server = stdio_server();
    let added = sandbox.run(&["mcp", "add", "demo", "--env", "A=1", "--", &server, "--flag"]).await;
    assert_eq!(added.code, 0, "{}", added.stderr);
    let file = sandbox.config().join("opencode.json");
    assert!(added.stderr.contains(&format!("MCP server \"demo\" added to {}", file.display())), "{}", added.stderr);
    // A closed local port: a remote server that fails to connect without leaving the machine.
    let remote = sandbox.run(&["mcp", "add", "web", "--url", "http://127.0.0.1:1/mcp", "--header", "X-Key=v=1"]).await;
    assert_eq!(remote.code, 0, "{}", remote.stderr);
    let written: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let expected = json!({"mcp": {
        "demo": {"type": "local", "command": [server, "--flag"], "environment": {"A": "1"}},
        "web": {"type": "remote", "url": "http://127.0.0.1:1/mcp", "headers": {"X-Key": "v=1"}}
    }});
    assert_eq!(written, expected);
    for (args, message) in [
        (vec!["mcp", "add", "x"], "Provide either --url <url> or a command after --"),
        (vec!["mcp", "add", "x", "--url", "nope"], "Invalid URL: nope"),
        (vec!["mcp", "add", "--url", "https://e.test/mcp"], "A server name is required for non-interactive MCP configuration"),
        (vec!["mcp", "add", "x", "--url", "https://e.test/mcp", "--env", "A=1"], "--env is only valid for local MCP servers"),
        (vec!["mcp", "add", "x", "--header", "A=1", "--", "cmd"], "--header is only valid for remote MCP servers"),
        (vec!["mcp", "add", "x", "--env", "=1", "--", "cmd"], "Invalid environment variable: =1. Expected KEY=VALUE"),
    ] {
        let out = sandbox.run(&args).await;
        assert!(out.code == 1 && out.stderr.contains(message), "{args:?}: {}", out.stderr);
    }

    let listed = sandbox.run(&["mcp", "list"]).await;
    assert_eq!(listed.code, 0, "{}", listed.stderr);
    assert!(listed.stderr.contains("✓ demo") && listed.stderr.contains("connected"), "{}", listed.stderr);
    assert!(listed.stderr.contains("✗ web") && listed.stderr.contains("failed"), "{}", listed.stderr);
    assert!(listed.stderr.contains("2 server(s)"), "{}", listed.stderr);

    let statuses = sandbox.run(&["mcp", "auth", "list"]).await;
    assert!(statuses.stderr.contains("✗ web") && statuses.stderr.contains("not authenticated"), "{}", statuses.stderr);
    assert!(statuses.stderr.contains("1 OAuth-capable server(s)"), "{}", statuses.stderr);
    let local = sandbox.run(&["mcp", "auth", "demo"]).await;
    assert!(local.stderr.contains("MCP server demo is not an OAuth-capable remote server"), "{}", local.stderr);
    let unknown = sandbox.run(&["mcp", "auth", "nothing"]).await;
    assert!(unknown.stderr.contains("MCP server not found: nothing"), "{}", unknown.stderr);

    let none = sandbox.run(&["mcp", "logout"]).await;
    assert!(none.stderr.contains("No MCP OAuth credentials stored"), "{}", none.stderr);
    let auth_file = sandbox.data().join("mcp-auth.json");
    std::fs::write(&auth_file, json!({"web": {"tokens": {"accessToken": "t"}}, "other": {"tokens": {"accessToken": "u"}}}).to_string()).unwrap();
    let wrong = sandbox.run(&["mcp", "logout", "nothing"]).await;
    assert!(wrong.stderr.contains("No credentials found for: nothing"), "{}", wrong.stderr);
    let removed = sandbox.run(&["mcp", "logout", "web"]).await;
    assert!(removed.stderr.contains("Removed OAuth credentials for web"), "{}", removed.stderr);
    let left: Value = serde_json::from_str(&std::fs::read_to_string(&auth_file).unwrap()).unwrap();
    assert_eq!(left.as_object().unwrap().keys().collect::<Vec<_>>(), ["other"]);
}

#[tokio::test]
async fn models_lists_ids_and_verbose_objects() {
    let sandbox = Sandbox::new(vec![], json!({"provider": {"mock": {"models": {"a0": {"name": "First", "cost": {"input": 1, "output": 2}}}}}})).await;
    let plain = sandbox.run(&["models"]).await;
    assert_eq!((plain.code, plain.stdout.as_str()), (0, "mock/a0\nmock/m1\n"), "{}", plain.stderr);
    let verbose = sandbox.run(&["models", "mock", "--verbose"]).await;
    assert_eq!(verbose.code, 0, "{}", verbose.stderr);
    let (first, rest) = verbose.stdout.split_once('\n').unwrap();
    assert_eq!(first, "mock/a0");
    let end = rest.find("\n}\n").unwrap() + 2;
    let model: Value = serde_json::from_str(&rest[..end]).unwrap();
    assert_eq!((&model["id"], &model["providerID"], &model["name"]), (&json!("a0"), &json!("mock"), &json!("First")));
    assert_eq!(model["cost"], json!({"input": 1, "output": 2, "cache": {"read": 0, "write": 0}}));
    assert!(rest[end..].starts_with("\nmock/m1\n{"), "{}", &rest[end..]);
    let missing = sandbox.run(&["models", "nope"]).await;
    assert!(missing.code == 1 && missing.stderr.contains("Provider not found: nope"), "{}", missing.stderr);
    // Fetching is off in the sandbox, so `--refresh` only reports.
    let refreshed = sandbox.run(&["models", "--refresh"]).await;
    assert!(refreshed.code == 0 && refreshed.stderr.contains("Models cache refreshed"), "{}", refreshed.stderr);
    assert_eq!(refreshed.stdout, plain.stdout);
}

const VERSION: &str = env!("CARGO_PKG_VERSION");
const STUB: &str = "is not implemented in the Rust build yet";

#[tokio::test]
async fn global_flags_help_and_logging() {
    let sandbox = Sandbox::new(vec![], json!({})).await;
    for args in [vec!["--version"], vec!["-v"], vec!["debug", "paths", "-v"]] {
        let out = sandbox.run(&args).await;
        assert_eq!((out.code, out.stdout.as_str()), (0, format!("{VERSION}\n").as_str()), "{args:?}");
    }
    // Nothing is created for a version check.
    assert!(!sandbox.data().exists());

    let help = sandbox.run(&["--help"]).await;
    assert_eq!(help.code, 0, "{}", help.stderr);
    let order = [
        "acp", "mcp", "attach", "run", "debug", "providers", "agent", "upgrade", "uninstall", "serve", "web", "models", "stats", "export", "import", "github", "pr", "session", "plugin", "db",
    ];
    let mut at = 0;
    for name in order {
        let found = help.stdout[at..].find(&format!("\n  {name} ")).unwrap_or_else(|| panic!("{name} missing or out of order:\n{}", help.stdout));
        at += found + 1;
    }
    for flag in ["--print-logs", "--log-level", "--pure", "--version", "--port", "--hostname", "--mdns-domain", "--cors", "--continue", "--session", "--fork", "--prompt", "--agent", "--auto", "--mini"] {
        assert!(help.stdout.contains(flag), "{flag}:\n{}", help.stdout);
    }
    // Hidden commands and flags stay out of the listing but are accepted.
    assert!(!help.stdout.contains("\n  generate ") && !help.stdout.contains("console") && !help.stdout.contains("--yolo") && !help.stdout.contains("\n  help "), "{}", help.stdout);
    assert!(help.stdout.starts_with("Usage: opencode [OPTIONS] [PROJECT]\n       opencode <COMMAND>\n"), "{}", help.stdout);
    let run_help = sandbox.run(&["run", "--help"]).await;
    for flag in ["--command", "--attach", "--password", "--username", "--variant", "--thinking", "--interactive", "--title", "--file", "--format", "--dir", "--port"] {
        assert!(run_help.stdout.contains(flag), "{flag}:\n{}", run_help.stdout);
    }
    assert!(!run_help.stdout.contains("--share") && !run_help.stdout.contains("--demo"), "{}", run_help.stdout);

    // Usage errors exit 1, as yargs does.
    for args in [vec!["nonsense-command", "--bogus"], vec!["--bogus"], vec!["session"], vec!["providers"], vec!["debug"], vec!["--log-level", "LOUD", "debug", "paths"]] {
        let out = sandbox.run(&args).await;
        assert_eq!(out.code, 1, "{args:?}: {}", out.stderr);
    }

    let paths = sandbox.run(&["debug", "paths"]).await;
    assert_eq!(paths.code, 0, "{}", paths.stderr);
    let names: Vec<&str> = paths.stdout.lines().map(|line| line.split_whitespace().next().unwrap()).collect();
    assert_eq!(names, ["home", "data", "bin", "log", "cache", "config", "state", "tmp"]);
    assert!(paths.stdout.contains(&format!("data       {}", sandbox.data().display())), "{}", paths.stdout);

    // One key=value line per event in <data>/log/opencode.log; --print-logs mirrors it to stderr.
    let log = std::fs::read_to_string(sandbox.data().join("log").join("opencode.log")).unwrap();
    let first = log.lines().next().unwrap();
    assert!(first.starts_with("timestamp=") && first.contains(" level=INFO run=") && first.contains(&format!("version={VERSION}")), "{first}");
    assert!(paths.stderr.is_empty(), "{}", paths.stderr);
    let printed = sandbox.run(&["--print-logs", "debug", "paths"]).await;
    assert!(printed.stderr.contains(" level=INFO run="), "{}", printed.stderr);
    let quiet = sandbox.run(&["--print-logs", "--log-level", "ERROR", "debug", "paths"]).await;
    assert!(quiet.code == 0 && quiet.stderr.is_empty(), "{}", quiet.stderr);

    // The default command validates its flags before touching the terminal.
    for (args, message) in [
        (vec!["--fork"], "--fork requires --continue or --session"),
        (vec!["--replay"], "--replay is not supported; replay is enabled by default"),
        (vec!["--no-replay"], "--no-replay requires --mini"),
        (vec!["--replay-limit", "3"], "--replay-limit requires --mini"),
        (vec!["--mini", "--port", "1"], "--port cannot be used with --mini"),
        (vec!["--mini", "--replay-limit", "1.5"], "--replay-limit must be a positive integer"),
        (vec!["--mini"], "--mini requires a TTY stdout"),
        (vec!["-s", "nope"], "Invalid session ID: Expected a string starting with \"ses\", got \"nope\""),
        (vec!["-s", "ses_missing"], "Session not found: ses_missing"),
        // The server starts and the UI connects to it; only then does the missing terminal stop it.
        (vec!["--port", "0", "-c"], "the terminal UI needs a TTY stdout"),
        (vec!["run", "--mini", "hi"], "--mini must be used without the run subcommand"),
        (vec!["run", "--mini", "--command", "x"], "--mini cannot be used with --command"),
    ] {
        let out = sandbox.run(&args).await;
        assert!(out.code == 1 && out.stderr.contains(message), "{args:?}: {}", out.stderr);
    }

    let completion = sandbox.run(&["completion", "bash"]).await;
    assert!(completion.code == 0 && completion.stdout.contains("opencode"), "{}", completion.stderr);
}

#[tokio::test]
async fn console_commands_without_an_account() {
    let sandbox = Sandbox::new(vec![], json!({})).await;
    for (args, expected) in [
        (vec!["console", "orgs"], "No accounts found"),
        (vec!["console", "logout"], "Not logged in"),
        (vec!["console", "switch"], "Not logged in"),
        (vec!["console", "open"], "No active account"),
    ] {
        let out = sandbox.run(&args).await;
        assert_eq!(out.code, 0, "{args:?}: {}", out.stderr);
        assert!(out.stderr.contains(expected), "{args:?}: {}", out.stderr);
    }
    let unreachable = sandbox.run(&["console", "login", "http://127.0.0.1:9"]).await;
    assert_ne!(unreachable.code, 0);
    assert!(unreachable.stderr.contains("Could not reach POST http://127.0.0.1:9/auth/device/code"), "{}", unreachable.stderr);
}

#[tokio::test]
async fn commands_without_an_engine_behind_them_say_so() {
    let sandbox = Sandbox::new(vec![], json!({})).await;
    for args in [
        vec!["upgrade"],
        vec!["upgrade", "v1.2.3", "--method", "npm"],
        vec!["github", "install"],
        vec!["github", "run", "--event", "{}"],
        vec!["debug", "v2"],
        vec!["debug", "agent", "build", "--tool", "read"],
    ] {
        let out = sandbox.run(&args).await;
        assert_eq!(out.code, 1, "{args:?}");
        assert!(out.stderr.contains("Error: ") && out.stderr.contains(STUB), "{args:?}: {}", out.stderr);
        assert_eq!(out.stdout, "", "{args:?}");
    }
    // Their flags still parse as the original's do.
    let bad = sandbox.run(&["upgrade", "--method", "apt"]).await;
    assert!(bad.code == 1 && !bad.stderr.contains(STUB), "{}", bad.stderr);

    let spec = sandbox.run(&["generate"]).await;
    assert_eq!(spec.code, 0, "{}", spec.stderr);
    let document: Value = serde_json::from_str(&spec.stdout).unwrap();
    let operation = document["paths"].as_object().unwrap().values().find_map(|item| item.get("get").filter(|get| get["operationId"].is_string())).unwrap();
    let sample = operation["x-codeSamples"][0]["source"].as_str().unwrap();
    assert!(sample.contains(&format!("await client.{}({{", operation["operationId"].as_str().unwrap())), "{sample}");
}

#[tokio::test]
async fn providers_login_logout_and_list() {
    let sandbox = Sandbox::new(vec![], json!({})).await;
    let catalog = sandbox.base.join("models.json");
    std::fs::write(&catalog, json!({"acme": {"id": "acme", "name": "Acme AI", "env": ["ACME_API_KEY"], "models": {}}}).to_string()).unwrap();
    let env = [("OPENCODE_MODELS_PATH", catalog.to_str().unwrap())];
    let auth_file = sandbox.data().join("auth.json");
    let stored = || serde_json::from_str::<Value>(&std::fs::read_to_string(&auth_file).unwrap()).unwrap();

    let empty = sandbox.run_with(&["providers", "list"], &env, None).await;
    assert!(empty.code == 0 && empty.stderr.contains("0 credentials") && !empty.stderr.contains("Environment"), "{}", empty.stderr);
    assert_eq!(empty.stdout, "");

    // The key is prompted for; a piped line answers the prompt. `-p` takes the id or the name.
    let login = sandbox.run_with(&["providers", "login", "-p", "acme ai"], &env, Some("sk-test\n")).await;
    assert_eq!(login.code, 0, "{}", login.stderr);
    assert!(login.stderr.contains("Add credential") && login.stderr.contains("Done"), "{}", login.stderr);
    assert_eq!(stored(), json!({"acme": {"type": "api", "key": "sk-test"}}));
    // `auth` is the same command.
    let again = sandbox.run_with(&["auth", "login", "--provider", "acme"], &env, Some("sk-two\n")).await;
    assert_eq!(again.code, 0, "{}", again.stderr);
    assert_eq!(stored()["acme"]["key"], "sk-two");

    // OpenAI and Copilot have the original's built-in methods; `-m` picks one by label.
    let both = json!({"acme": {"id": "acme", "name": "Acme AI", "env": ["ACME_API_KEY"], "models": {}}, "openai": {"id": "openai", "name": "OpenAI", "env": ["OPENAI_API_KEY"], "models": {}}});
    let only = std::fs::read_to_string(&catalog).unwrap();
    std::fs::write(&catalog, both.to_string()).unwrap();
    let manual = sandbox.run_with(&["providers", "login", "-p", "openai", "-m", "manually enter api key"], &env, Some("sk-openai\n")).await;
    assert_eq!(manual.code, 0, "{}", manual.stderr);
    assert_eq!(stored()["openai"], json!({"type": "api", "key": "sk-openai"}));
    let method = sandbox.run_with(&["providers", "login", "-p", "openai", "-m", "sso"], &env, None).await;
    let expected = "Unknown method \"sso\" for openai. Available: ChatGPT Pro/Plus (browser), ChatGPT Pro/Plus (headless), Manually enter API Key";
    assert!(method.code == 1 && method.stderr.contains(expected), "{}", method.stderr);
    assert_eq!(sandbox.run_with(&["providers", "logout", "openai"], &env, None).await.code, 0);
    std::fs::write(&catalog, only).unwrap();

    let unknown = sandbox.run_with(&["providers", "login", "-p", "nope"], &env, Some("k\n")).await;
    assert!(unknown.code == 1 && unknown.stderr.contains("Unknown provider \"nope\""), "{}", unknown.stderr);
    // No key on stdin: the prompt is cancelled, nothing is stored and nothing is printed as an error.
    let cancelled = sandbox.run_with(&["providers", "login", "-p", "acme"], &env, None).await;
    assert!(cancelled.code == 1 && !cancelled.stderr.contains("Error"), "{}", cancelled.stderr);
    assert_eq!(stored()["acme"]["key"], "sk-two");

    let with_env = [env[0], ("ACME_API_KEY", "from-env")];
    let listed = sandbox.run_with(&["auth", "ls"], &with_env, None).await;
    assert_eq!(listed.code, 0, "{}", listed.stderr);
    for part in ["Credentials ", "auth.json", "Acme AI ", "api", "1 credentials", "Environment", "ACME_API_KEY", "1 environment variable\n"] {
        assert!(listed.stderr.contains(part), "{part}: {}", listed.stderr);
    }
    assert!(!listed.stderr.contains("sk-two") && !listed.stderr.contains("from-env"));

    let wrong = sandbox.run_with(&["providers", "logout", "nope"], &env, None).await;
    assert!(wrong.code == 1 && wrong.stderr.contains("Unknown configured provider \"nope\""), "{}", wrong.stderr);
    let out = sandbox.run_with(&["providers", "logout", "Acme AI"], &env, None).await;
    assert!(out.code == 0 && out.stderr.contains("Logout successful"), "{}", out.stderr);
    assert_eq!(stored(), json!({}));
    let none = sandbox.run_with(&["providers", "logout"], &env, None).await;
    assert!(none.code == 0 && none.stderr.contains("No credentials found"), "{}", none.stderr);

    // Rust build: a login only opencode 2.x has is listed from its database, and logout leaves it alone.
    {
        let db = rusqlite::Connection::open(sandbox.data().join("opencode.db")).unwrap();
        db.execute_batch("CREATE TABLE `credential` (`id` text PRIMARY KEY, `integration_id` text, `label` text NOT NULL, `value` text NOT NULL, `connector_id` text, `method_id` text, `active` integer, `time_created` integer NOT NULL, `time_updated` integer NOT NULL);").unwrap();
        db.execute("INSERT INTO credential (id, integration_id, label, value, time_created, time_updated) VALUES ('cred_1', 'acme', 'API key', '{\"type\":\"key\",\"key\":\"db-key\"}', 1, 1)", []).unwrap();
    }
    let listed = sandbox.run_with(&["auth", "ls"], &env, None).await;
    assert!(listed.stderr.contains("Acme AI ") && listed.stderr.contains("(opencode.db, read-only)") && listed.stderr.contains("1 credentials"), "{}", listed.stderr);
    let none = sandbox.run_with(&["providers", "logout"], &env, None).await;
    assert!(none.code == 0 && none.stderr.contains("No credentials found"), "{}", none.stderr);
}

#[tokio::test]
async fn stats_db_session_list_and_sanitized_export() {
    let sandbox = Sandbox::scripted(|project| vec![tool_reply("read", &json!({"filePath": project.join("opencode.json").to_string_lossy()})), text_reply("Secret answer.")], json!({})).await;
    assert_eq!(sandbox.run(&["session", "list"]).await.stdout, "");
    assert_eq!(sandbox.run(&["run", "--title", "A title", "the question"]).await.code, 0);
    let session_id = sandbox.sessions().await[0]["id"].as_str().unwrap().to_string();

    let table = sandbox.run(&["session", "list"]).await;
    let lines: Vec<&str> = table.stdout.lines().collect();
    assert_eq!(lines.len(), 3, "{}", table.stdout);
    assert_eq!(lines[0], format!("Session ID{}  Title{}  Updated", " ".repeat(session_id.len() - 10), " ".repeat(20)));
    assert_eq!(lines[1], "─".repeat(lines[0].chars().count()));
    assert!(lines[2].starts_with(&format!("{session_id}  A title{}  ", " ".repeat(18))), "{}", lines[2]);
    let limited = sandbox.run(&["session", "list", "-n", "1", "--format", "json"]).await;
    let rows: Value = serde_json::from_str(&limited.stdout).unwrap();
    assert_eq!(rows[0].as_object().unwrap().keys().collect::<Vec<_>>(), ["id", "title", "updated", "created", "projectId", "directory"]);
    assert_eq!(sandbox.run(&["session", "list", "--format", "xml"]).await.code, 1);

    let stats = sandbox.run(&["stats", "--models"]).await;
    assert_eq!(stats.code, 0, "{}", stats.stderr);
    let row = |label: &str, value: &str| format!("│{label}{}{value} │", " ".repeat(55 - label.chars().count() - value.chars().count()));
    for line in [
        "┌────────────────────────────────────────────────────────┐".to_string(),
        "│                       OVERVIEW                         │".to_string(),
        row("Sessions", "1"),
        row("Messages", "3"),
        row("Days", "1"),
        "│                    COST & TOKENS                       │".to_string(),
        row("Total Cost", "$0.00"),
        row("Input", "30"),
        row("Output", "7"),
        "│                      MODEL USAGE                       │".to_string(),
        format!("│ mock/m1{} │", " ".repeat(47)),
        row("  Messages", "2"),
        row("  Cost", "$0.0000"),
        "│                      TOOL USAGE                        │".to_string(),
        format!("│ read{} {}   1 (100.0%)   │", " ".repeat(14), "█".repeat(20)),
    ] {
        assert!(stats.stdout.lines().any(|have| have == line), "missing {line:?} in\n{}", stats.stdout);
    }
    for line in stats.stdout.lines().filter(|line| !line.is_empty()) {
        assert_eq!(line.chars().count(), 58, "{line}");
    }
    assert!(stats.stdout.ends_with("┘\n\n"), "{:?}", stats.stdout);
    let hidden = sandbox.run(&["stats", "--project", "nope", "--days", "0"]).await;
    assert!(hidden.stdout.contains(&row("Sessions", "0")) && !hidden.stdout.contains("MODEL USAGE") && !hidden.stdout.contains("TOOL USAGE"), "{}", hidden.stdout);
    assert!(sandbox.run(&["stats", "--project", ""]).await.stdout.contains(&row("Sessions", "1")));

    let path = sandbox.run(&["db", "path"]).await;
    assert_eq!(path.stdout.trim_end(), sandbox.data().join("opencode-rs.db").to_str().unwrap());
    let tsv = sandbox.run(&["db", "select id, parent_id, cost from session"]).await;
    assert_eq!(tsv.stdout, format!("id\tparent_id\tcost\n{session_id}\t\t0\n"), "{}", tsv.stderr);
    let rows = sandbox.run(&["db", "select count(*) as n from message", "--format", "json"]).await;
    assert_eq!(serde_json::from_str::<Value>(&rows.stdout).unwrap(), json!([{"n": 3}]));
    assert_eq!(sandbox.run(&["db", "select 1 where 0"]).await.stdout, "");
    let broken = sandbox.run(&["db", "select x from nowhere"]).await;
    assert!(broken.code == 1 && broken.stderr.contains("no such table"), "{}", broken.stderr);

    let plain: Value = serde_json::from_str(&sandbox.run(&["export", &session_id]).await.stdout).unwrap();
    let clean: Value = serde_json::from_str(&sandbox.run(&["export", &session_id, "--sanitize"]).await.stdout).unwrap();
    assert_eq!(clean["info"]["title"], format!("[redacted:session-title:{session_id}]"));
    assert_eq!(clean["info"]["directory"], format!("[redacted:session-directory:{session_id}]"));
    assert_eq!((&clean["info"]["id"], &clean["info"]["time"]), (&plain["info"]["id"], &plain["info"]["time"]));
    let text = serde_json::to_string(&clean).unwrap();
    assert!(!text.contains("Secret answer.") && !text.contains("the question") && !text.contains("opencode.json"), "{text}");
    let parts: Vec<&Value> = clean["messages"].as_array().unwrap().iter().flat_map(|message| message["parts"].as_array().unwrap()).collect();
    let tool = parts.iter().find(|part| part["type"] == "tool").unwrap();
    let id = tool["id"].as_str().unwrap();
    assert_eq!(tool["state"]["input"], json!({"redacted": format!("tool-input:{id}")}));
    assert_eq!(tool["state"]["output"], format!("[redacted:tool-output:{id}]"));
    assert_eq!((&tool["tool"], &tool["state"]["status"]), (&json!("read"), &json!("completed")));
    assert!(parts.iter().any(|part| part["type"] == "text" && part["text"] == format!("[redacted:text:{}]", part["id"].as_str().unwrap())));
    let assistant = clean["messages"].as_array().unwrap().iter().find(|message| message["info"]["role"] == "assistant").unwrap();
    assert_eq!(assistant["info"]["path"]["cwd"], format!("[redacted:cwd:{}]", assistant["info"]["id"].as_str().unwrap()));

    let deleted = sandbox.run(&["session", "delete", &session_id]).await;
    assert!(deleted.code == 0 && deleted.stderr.contains(&format!("Session {session_id} deleted")), "{}", deleted.stderr);
    let gone = sandbox.run(&["session", "delete", &session_id]).await;
    assert!(gone.code == 1 && gone.stderr.contains(&format!("Session not found: {session_id}")), "{}", gone.stderr);
}

#[tokio::test]
async fn agent_list_and_scripted_create() {
    let generated = json!({"identifier": "code-critic", "whenToUse": "Use this agent to review: code", "systemPrompt": "You review code.\nBe terse."});
    let sandbox = Sandbox::new(vec![text_reply(&format!("```json\n{generated}\n```")), text_reply(&generated.to_string()), text_reply("not json")], json!({})).await;

    let listed = sandbox.run(&["agent", "list"]).await;
    assert_eq!(listed.code, 0, "{}", listed.stderr);
    assert!(listed.stdout.starts_with("build (primary)\n  [\n  {\n    \"permission\": \"*\",\n    \"pattern\": \"*\",\n    \"action\": \"allow\"\n  },"), "{}", listed.stdout);
    let names: Vec<&str> = listed.stdout.lines().filter(|line| !line.starts_with([' ', ']'])).collect();
    assert_eq!(&names[..3], ["build (primary)", "compaction (primary)", "explore (subagent)"], "{names:?}");

    let target = sandbox.base.join("out");
    let args = ["agent", "create", "--path", target.to_str().unwrap(), "--description", "reviews code", "--mode", "subagent", "--tools", "read, grep"];
    let created = sandbox.run(&args).await;
    let file = target.join("agents").join("code-critic.md");
    assert_eq!((created.code, created.stdout.trim_end()), (0, file.to_str().unwrap()), "{}", created.stderr);
    let request = sandbox.seen.lock().unwrap()[0].clone();
    assert!(request.contains("Create an agent configuration based on this request: \\\"reviews code\\\".") && request.contains("must NOT be used: build, compaction, explore, general, plan"), "{request}");
    let expected = "---\ndescription: \"Use this agent to review: code\"\nmode: subagent\npermission:\n  bash: deny\n  edit: deny\n  glob: deny\n  webfetch: deny\n  task: deny\n  todowrite: deny\n  websearch: deny\n  lsp: deny\n  skill: deny\n---\nYou review code.\nBe terse.\n";
    assert_eq!(std::fs::read_to_string(&file).unwrap(), expected);

    // The file is never overwritten, and an answer that is not the JSON object fails the command.
    let exists = sandbox.run(&args).await;
    assert!(exists.code == 1 && exists.stderr.contains(&format!("Error: Agent file already exists: {}", file.display())), "{}", exists.stderr);
    let garbled = sandbox.run(&args).await;
    assert!(garbled.code == 1 && garbled.stderr.contains("LLM failed to generate agent"), "{}", garbled.stderr);
    assert_eq!(sandbox.run(&["agent", "create", "--mode", "everything"]).await.code, 1);
}

#[tokio::test]
async fn uninstall_removes_only_what_this_build_owns() {
    let sandbox = Sandbox::new(vec![text_reply("Hi.")], json!({})).await;
    assert_eq!(sandbox.run(&["run", "hi"]).await.code, 0);
    let (data, state, config) = (sandbox.data(), sandbox.base.join("xdg-state").join("opencode"), sandbox.config());
    let database = data.join("opencode-rs.db");
    assert!(database.exists());
    // What the TypeScript build and the user own, next to this build's files.
    let shared = [data.join("auth.json"), data.join("opencode.db"), data.join("opencode.db-wal"), config.join("opencode.json"), state.join("kv.json"), sandbox.base.join("xdg-cache").join("opencode").join("models.json")];
    for file in &shared {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, "{}").unwrap();
    }
    let seed = || {
        std::fs::create_dir_all(data.join("snapshot-rs").join("proj")).unwrap();
        std::fs::write(data.join("snapshot-rs").join("proj").join("HEAD"), "ref").unwrap();
        std::fs::write(data.join("opencode-rs.db-wal"), "wal").unwrap();
        std::fs::write(state.join("desktop-rs.json"), "{}").unwrap();
    };
    seed();
    let owned = [database.clone(), data.join("opencode-rs.db-wal"), data.join("snapshot-rs"), state.join("desktop-rs.json")];
    let present = |files: &[PathBuf]| files.iter().map(|file| file.exists()).collect::<Vec<_>>();

    let dry = sandbox.run(&["uninstall", "--dry-run"]).await;
    assert_eq!(dry.code, 0, "{}", dry.stderr);
    for part in ["Uninstall OpenCode", "Installation method: unknown", "The following will be removed:", "✓ Database: ", "opencode-rs.db", "✓ Snapshots: ", "✓ Desktop state: ", "Dry run - no changes made", "Done"] {
        assert!(dry.stderr.contains(part), "{part}: {}", dry.stderr);
    }
    assert!(!dry.stderr.contains("Thank you"), "{}", dry.stderr);
    assert_eq!(present(&owned), [true; 4]);
    // `--dry-run` wins over `--force`, and an unanswered confirmation cancels.
    assert!(sandbox.run(&["uninstall", "--dry-run", "--force"]).await.stderr.contains("Dry run - no changes made"));
    let unconfirmed = sandbox.run(&["uninstall"]).await;
    assert!(unconfirmed.code == 0 && unconfirmed.stderr.contains("Cancelled"), "{}", unconfirmed.stderr);
    assert_eq!(present(&owned), [true; 4]);

    let kept = sandbox.run(&["uninstall", "-f", "--keep-data", "--keep-config"]).await;
    assert_eq!(kept.code, 0, "{}", kept.stderr);
    for part in ["○ Database: ", "(keeping)", "Skipping Database (--keep-data)", "Skipping Snapshots (--keep-data)", "Skipping Desktop state (--keep-config)", "Thank you for using OpenCode!"] {
        assert!(kept.stderr.contains(part), "{part}: {}", kept.stderr);
    }
    assert_eq!(present(&owned), [true; 4]);

    let removed = sandbox.run(&["uninstall", "--force", "-d"]).await;
    assert!(removed.code == 0 && removed.stderr.contains("Removed Desktop state"), "{}", removed.stderr);
    assert_eq!(present(&owned), [true, true, true, false]);
    seed();
    let removed = sandbox.run(&["uninstall", "--force"]).await;
    assert_eq!(removed.code, 0, "{}", removed.stderr);
    for part in ["Removed Database", "Removed Snapshots", "Removed Desktop state", "Thank you for using OpenCode!"] {
        assert!(removed.stderr.contains(part), "{part}: {}", removed.stderr);
    }
    assert_eq!(present(&owned), [false; 4]);
    // Shared files and directories are untouched, the log directory included.
    assert_eq!(present(&shared), [true; 6]);
    assert!(data.join("log").join("opencode.log").exists() && sandbox.project.join("opencode.json").exists());
}

#[tokio::test]
async fn debug_subcommands_report_engine_state() {
    let extra = json!({"agent": {"locked": {"description": "No shell", "mode": "subagent", "permission": {"bash": "deny"}}}, "provider": {"mock": {"options": {"headers": {"X-Trace": "abc"}}}}, "mcp": {"web": {"type": "remote", "url": "https://user:pw@e.test/mcp", "enabled": false}}});
    let sandbox = Sandbox::new(vec![text_reply("Hi.")], extra).await;
    std::fs::write(sandbox.project.join("notes.txt"), "remember").unwrap();
    let skill = sandbox.project.join(".opencode").join("skills").join("greeter");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "---\nname: greeter\ndescription: Says hello\n---\nGreet the user.\n").unwrap();

    let config: Value = serde_json::from_str(&sandbox.run(&["debug", "config"]).await.stdout).unwrap();
    let options = &config["provider"]["mock"]["options"];
    assert_eq!((&options["apiKey"], &options["headers"]["X-Trace"], &config["mcp"]["web"]["url"]), (&json!("***"), &json!("***"), &json!("***")));
    assert!(options["baseURL"].as_str().unwrap().starts_with("http://127.0.0.1:"));
    assert_eq!(config["model"], "mock/m1");

    let info = sandbox.run_with(&["debug", "info"], &[("TERM_PROGRAM", "tester"), ("TERM_PROGRAM_VERSION", "9"), ("TERM", "xterm")], None).await;
    let lines: Vec<&str> = info.stdout.lines().collect();
    assert_eq!(lines[0], format!("opencode version: {VERSION}"));
    assert!(lines[1].starts_with("os: ") && lines[1].split(' ').count() == 4, "{}", lines[1]);
    assert_eq!(&lines[2..], ["terminal: tester 9 / xterm", "plugins:", "none"]);
    let pure = sandbox.run(&["--pure", "debug", "info"]).await;
    assert!(pure.stdout.ends_with("plugins:\nexternal plugins disabled (--pure)\n"), "{}", pure.stdout);

    let skills: Value = serde_json::from_str(&sandbox.run(&["debug", "skill"]).await.stdout).unwrap();
    let greeter = skills.as_array().unwrap().iter().find(|skill| skill["name"] == "greeter").unwrap();
    assert_eq!((&greeter["description"], greeter["content"].as_str().unwrap().trim()), (&json!("Says hello"), "Greet the user."));

    let agent: Value = serde_json::from_str(&sandbox.run(&["debug", "agent", "locked"]).await.stdout).unwrap();
    assert_eq!((&agent["name"], &agent["mode"], &agent["tools"]["read"], &agent["tools"]["bash"]), (&json!("locked"), &json!("subagent"), &json!(true), &json!(false)));
    let missing = sandbox.run(&["debug", "agent", "nobody"]).await;
    assert_eq!((missing.code, missing.stderr.as_str()), (1, "Agent nobody not found, run 'opencode agent list' to get an agent list\n"));

    assert_eq!(sandbox.run(&["debug", "scrap"]).await.stdout, "[]\n");
    assert_eq!(sandbox.run(&["run", "hi"]).await.code, 0);
    let projects: Value = serde_json::from_str(&sandbox.run(&["debug", "scrap"]).await.stdout).unwrap();
    assert_eq!(projects[0]["worktree"], sandbox.project.to_str().unwrap());

    let startup = sandbox.run(&["debug", "startup"]).await;
    assert!(startup.stdout.trim().parse::<f64>().unwrap() > 0.0, "{}", startup.stdout);

    let read: Value = serde_json::from_str(&sandbox.run(&["debug", "file", "read", "notes.txt"]).await.stdout).unwrap();
    assert_eq!(read, json!({"content": "cmVtZW1iZXI=", "encoding": "base64", "mime": "text/plain"}));
    let listed: Value = serde_json::from_str(&sandbox.run(&["debug", "file", "list", "."]).await.stdout).unwrap();
    let names: Vec<&str> = listed.as_array().unwrap().iter().map(|entry| entry["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"notes.txt") && names.contains(&"opencode.json"), "{names:?}");

    // ripgrep is located, never downloaded, by these commands; without it they fail with a message.
    let files = sandbox.run(&["debug", "rg", "files", "--glob", "*.txt"]).await;
    if files.code == 0 {
        assert_eq!(files.stdout, "notes.txt\n");
        let found: Value = serde_json::from_str(&sandbox.run(&["debug", "rg", "search", "remem", "--limit", "5"]).await.stdout).unwrap();
        assert_eq!((&found[0]["line"], found[0]["text"].as_str().unwrap().trim()), (&json!(1), "remember"));
        let search = sandbox.run(&["debug", "file", "search", "notes"]).await;
        assert!(search.stdout.lines().any(|line| line == "notes.txt"), "{}{}", search.stdout, search.stderr);
    } else {
        assert!(files.stderr.contains("ripgrep"), "{}", files.stderr);
    }

    for (args, message) in [
        (vec!["mcp", "debug", "nothing"], "MCP server not found: nothing"),
        (vec!["mcp", "debug", "web"], "Auth status: ✗ not authenticated"),
    ] {
        let out = sandbox.run(&args).await;
        assert!(out.code == 0 && out.stderr.contains("MCP OAuth Debug") && out.stderr.contains(message), "{args:?}: {}", out.stderr);
    }
}

#[tokio::test]
async fn serve_and_web_print_their_addresses() {
    use tokio::io::AsyncBufReadExt;
    let sandbox = Sandbox::new(vec![], json!({})).await;
    // The global config supplies what the flags leave out.
    std::fs::create_dir_all(sandbox.config()).unwrap();
    std::fs::write(sandbox.config().join("opencode.json"), json!({"server": {"hostname": "localhost"}}).to_string()).unwrap();

    let mut serve = sandbox.command(&["serve", "--port", "0"], &[]).stdin(Stdio::null()).spawn().unwrap();
    let mut lines = tokio::io::BufReader::new(serve.stdout.take().unwrap()).lines();
    let read = async {
        let warning = lines.next_line().await.unwrap().unwrap();
        (warning, lines.next_line().await.unwrap().unwrap())
    };
    let (warning, listening) = tokio::time::timeout(std::time::Duration::from_secs(30), read).await.expect("serve printed nothing");
    assert_eq!(warning, "Warning: OPENCODE_SERVER_PASSWORD is not set; server is unsecured.");
    let port: u16 = listening.strip_prefix("opencode server listening on http://localhost:").unwrap_or_else(|| panic!("{listening}")).parse().unwrap();
    assert!(port > 0);
    serve.kill().await.unwrap();

    let mut web = sandbox.command(&["web", "--hostname", "127.0.0.1"], &[("OPENCODE_SERVER_PASSWORD", "pw")]).stdin(Stdio::null()).spawn().unwrap();
    let mut lines = tokio::io::BufReader::new(web.stderr.take().unwrap()).lines();
    let read = async {
        let mut seen = String::new();
        while !seen.contains("serves the HTTP API only") {
            seen.push_str(&lines.next_line().await.unwrap().expect("web exited early"));
            seen.push('\n');
        }
        seen
    };
    let seen = tokio::time::timeout(std::time::Duration::from_secs(30), read).await.expect("web printed nothing");
    assert!(seen.contains("Web interface:") && seen.contains("http://127.0.0.1:") && !seen.contains("OPENCODE_SERVER_PASSWORD"), "{seen}");
    web.kill().await.unwrap();
}

// A password-protected Rust server in this test process, on an in-memory store, using the sandbox's provider.
async fn attached_server(sandbox: &Sandbox) -> opencode_server::Server {
    let base = sandbox.base.join("server");
    let paths = opencode_core::paths::Paths { home: base.join("home"), data: base.join("data"), cache: base.join("cache"), config: base.join("config"), state: base.join("state"), tmp: base.join("tmp") };
    let source = opencode_llm::catalog::Source { disabled: true, ..opencode_llm::catalog::Source::from_env(&paths) };
    let engine = opencode_session::Engine::with_store(paths, opencode_storage::Store::open_in_memory().unwrap());
    engine.set_catalog_source(source);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let options = opencode_server::ServerOptions { password: Some("pw".into()), directory: Some(sandbox.base.to_string_lossy().into_owned()), ..Default::default() };
    opencode_server::serve_on(listener, options, std::sync::Arc::new(engine)).await.unwrap()
}

fn attach_args<'a>(url: &'a str, project: &'a str, rest: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec!["run", "--attach", url, "-p", "pw", "--dir", project];
    args.extend_from_slice(rest);
    args
}

#[tokio::test]
async fn run_and_attach_talk_to_a_server() {
    let script = |project: &Path| {
        let write = tool_reply("write", &json!({"filePath": project.join("out.txt").to_string_lossy(), "content": "x"}));
        vec![text_reply("Hello there."), text_reply("Done."), write.clone(), write, text_reply("Written.")]
    };
    let sandbox = Sandbox::scripted(script, json!({"permission": {"edit": "ask"}})).await;
    let server = attached_server(&sandbox).await;
    let url = server.url();
    let project = sandbox.project.to_string_lossy().into_owned();
    let attach = |rest: &[&'static str]| attach_args(&url, &project, rest);

    let out = sandbox.run(&attach(&["say", "hi", "--title", ""])).await;
    assert_eq!((out.code, out.stdout.as_str()), (0, "Hello there.\n"), "{}", out.stderr);
    assert!(out.stderr.contains("> build · m1"), "{}", out.stderr);
    let client = opencode_client::Client::new(&url).with_directory(Some(project.clone())).with_credentials(Some(opencode_client::Credentials::new("opencode", "pw")));
    let sessions = client.sessions(&Default::default()).await.unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!((sessions[0].title.as_str(), sessions[0].directory.as_str()), ("say hi", project.as_str()));

    // `--continue` reuses that session; `--format json` streams the original's event lines.
    let out = sandbox.run(&attach(&["-c", "--format", "json", "look"])).await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    let events: Vec<Value> = out.stdout.lines().map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}"))).collect();
    let kinds: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["step_start", "text", "step_finish"]);
    assert!(events.iter().all(|e| e["sessionID"] == sessions[0].id.as_str() && e["timestamp"].is_u64()));
    assert_eq!(events[1]["part"]["text"], "Done.");
    assert_eq!(client.sessions(&Default::default()).await.unwrap().len(), 1);

    // Permissions of the run are rejected unless `--auto`.
    let out = sandbox.run(&attach(&["write the file"])).await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stderr.contains("permission requested: edit (") && out.stderr.contains("; auto-rejecting"), "{}", out.stderr);
    assert!(!sandbox.project.join("out.txt").exists());
    let out = sandbox.run(&attach(&["--auto", "write the file"])).await;
    assert_eq!((out.code, out.stdout.as_str()), (0, "Written.\n"), "{}", out.stderr);
    assert!(sandbox.project.join("out.txt").exists());
    assert_eq!(sandbox.seen.lock().unwrap().len(), 5);

    for (args, message) in [
        (vec!["run", "--attach", url.as_str(), "-p", "nope", "x"], "Session not found"),
        (attach(&["-s", "ses_missing", "x"]), "Session not found"),
        // Local files are found from $PWD, as the original resolves them.
        (attach(&["-f", "missing.txt", "x"]), "File not found: missing.txt"),
        (attach(&["-f", ".git", "x"]), "Cannot attach local directory without a shared filesystem: .git"),
        (vec!["run", "--attach", "http://127.0.0.1:1", "x"], "error sending request"),
        (vec!["attach", url.as_str(), "-p", "pw", "-s", "ses_missing"], "Session not found: ses_missing"),
        (vec!["attach", url.as_str(), "-s", "nope"], "Invalid session ID"),
        (vec!["attach", url.as_str(), "--fork"], "--fork requires --continue or --session"),
        (vec!["attach", url.as_str(), "--no-replay"], "--no-replay requires --mini"),
        (vec!["attach", url.as_str(), "--mini"], "--mini requires a TTY stdout"),
        (vec!["attach", "http://127.0.0.1:1"], "error sending request"),
        // Connected and authenticated; the missing terminal is what stops it.
        (vec!["attach", url.as_str(), "-p", "pw", "--dir", project.as_str(), "-s", sessions[0].id.as_str()], "the terminal UI needs a TTY stdout"),
    ] {
        let out = sandbox.run_with(&args, &[("PWD", project.as_str())], None).await;
        assert!(out.code == 1 && out.stderr.contains("Error: ") && out.stderr.contains(message), "{args:?}: {}", out.stderr);
        assert!(!out.stderr.contains(STUB), "{args:?}: {}", out.stderr);
    }
    server.shutdown().await;
}

#[tokio::test]
async fn acp_answers_initialize_over_stdio() {
    use tokio::io::AsyncBufReadExt;
    let sandbox = Sandbox::new(vec![], json!({})).await;
    let mut acp = sandbox.command(&["acp"], &[]).stdin(Stdio::piped()).spawn().unwrap();
    let mut stdin = acp.stdin.take().unwrap();
    let request = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1}});
    stdin.write_all(format!("{request}\n").as_bytes()).await.unwrap();
    let mut lines = tokio::io::BufReader::new(acp.stdout.take().unwrap()).lines();
    let line = tokio::time::timeout(std::time::Duration::from_secs(30), lines.next_line()).await.expect("acp answered nothing").unwrap().expect("acp closed stdout");
    let response: Value = serde_json::from_str(&line).unwrap();
    assert_eq!((&response["jsonrpc"], &response["id"]), (&json!("2.0"), &json!(1)), "{response}");
    assert_eq!(response["result"]["protocolVersion"], 1, "{response}");
    assert_eq!(response["result"]["agentInfo"]["version"], VERSION, "{response}");
    assert!(response["result"]["agentCapabilities"]["loadSession"].as_bool().unwrap_or(false), "{response}");
    // Closing stdin ends the agent.
    drop(stdin);
    let status = tokio::time::timeout(std::time::Duration::from_secs(30), acp.wait()).await.expect("acp did not exit").unwrap();
    assert!(status.success(), "{status:?}");
}

// Nothing is downloaded without a yes; `--decline` records the answer in `<state>/setup-rs.json`.
#[tokio::test]
async fn setup_bash_asks_first_and_records_a_decline() {
    let sandbox = Sandbox::new(vec![], json!({})).await;
    let state = sandbox.base.join("xdg-state").join("opencode").join("setup-rs.json");
    let mingit = sandbox.base.join("xdg-cache").join("opencode-rs");
    let asked = sandbox.run(&["setup", "bash"]).await;
    if cfg!(windows) {
        assert_eq!(asked.code, 1, "{}", asked.stderr);
        assert!(asked.stderr.contains("Download the official portable bash (Git for Windows MinGit, ~38 MB)"), "{}", asked.stderr);
        assert!(asked.stderr.contains("Not a terminal: pass --yes to download, or --decline"), "{}", asked.stderr);
    } else {
        assert_eq!(asked.code, 0, "{}", asked.stderr);
    }
    assert!(!state.exists() && !mingit.exists());
    let declined = sandbox.run(&["setup", "bash", "--decline"]).await;
    assert_eq!(declined.code, 0, "{}", declined.stderr);
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
    assert_eq!(saved, json!({"bash": "declined"}));
    assert!(!mingit.exists());
    let both = sandbox.run(&["setup", "bash", "--yes", "--decline"]).await;
    assert_ne!(both.code, 0);
}
