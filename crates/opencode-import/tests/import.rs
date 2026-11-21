// Synthetic fixtures for every source: scan, plan, apply, idempotent re-import, Rust-only targets, untouched sources.
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use opencode_core::config::{self, LoadOptions};
use opencode_core::paths::Paths;
use opencode_import::{Category, ImportReport, Outcome, Selection, SourceReport, ids};
use opencode_storage::{Message, Part, PartBody, Store, ToolState};
use serde_json::{Value, json};

struct Fx {
    _root: tempfile::TempDir,
    root: PathBuf,
    paths: Paths,
    home: PathBuf,
    env: HashMap<String, String>,
}

impl Fx {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let root = PathBuf::from(canonical.to_string_lossy().trim_start_matches(r"\\?\"));
        let home = root.join("home");
        let paths = Paths {
            home: home.clone(),
            data: root.join("xdg-data").join("opencode"),
            cache: root.join("xdg-cache").join("opencode"),
            config: root.join("xdg-config").join("opencode"),
            state: root.join("xdg-state").join("opencode"),
            tmp: root.join("tmp"),
        };
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&paths.data).unwrap();
        Fx { _root: dir, root, paths, home, env: HashMap::new() }
    }

    fn write(&self, path: &Path, text: &str) -> PathBuf {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
        path.to_path_buf()
    }

    fn work(&self) -> String {
        let dir = self.root.join("work");
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }

    fn scan(&self) -> Vec<SourceReport> {
        let env = |key: &str| self.env.get(key).cloned();
        opencode_import::scan_with(&self.paths, &self.home, &env)
    }

    fn store(&self) -> Store {
        Store::open(Store::default_path(&self.paths.data)).unwrap()
    }

    fn import(&self, store: &Store, selection: &Selection) -> ImportReport {
        let reports = self.scan();
        let plan = opencode_import::plan(&reports, selection);
        opencode_import::apply(&plan, store, &self.paths)
    }

    fn rust_mcp(&self) -> Value {
        let text = std::fs::read_to_string(self.paths.rust_config().join("opencode.jsonc")).unwrap();
        config::parse_jsonc(&text, "x").unwrap()["mcp"].clone()
    }

    fn loaded(&self) -> config::Loaded {
        let opts = LoadOptions { cwd: self.root.join("work"), worktree: None };
        let managed = self.root.join("managed").to_string_lossy().into_owned();
        let env = move |key: &str| (key == "OPENCODE_TEST_MANAGED_CONFIG_DIR").then(|| managed.clone());
        config::load_with(&self.paths, &opts, &env).unwrap()
    }
}

fn jsonl(path: &Path, lines: &[Value]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(path, text).unwrap();
}

fn snapshot(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    if path.is_file() {
        out.insert(path.to_path_buf(), std::fs::read(path).unwrap());
        return out;
    }
    let Ok(entries) = std::fs::read_dir(path) else { return out };
    for entry in entries.flatten() {
        out.extend(snapshot(&entry.path()));
    }
    out
}

fn bump(file: &Path) {
    let later = SystemTime::now() + Duration::from_secs(60);
    std::fs::File::options().append(true).open(file).unwrap().set_modified(later).unwrap();
}

fn outcome(report: &ImportReport, source: &str, category: Category) -> Outcome {
    report.outcomes.iter().find(|o| o.source == source && o.category == category).cloned().unwrap_or_default()
}

fn source<'a>(reports: &'a [SourceReport], id: &str) -> &'a SourceReport {
    reports.iter().find(|r| r.id == id).unwrap()
}

fn kinds(parts: &[Part]) -> Vec<&'static str> {
    parts.iter().map(Part::kind).collect()
}

fn texts(parts: &[Part]) -> Vec<String> {
    parts.iter().filter_map(|p| match &p.body {
        PartBody::Text(t) => Some(t.text.clone()),
        _ => None,
    }).collect()
}

fn tool(parts: &[Part], name: &str) -> (ToolState, Value) {
    parts
        .iter()
        .find_map(|p| match &p.body {
            PartBody::Tool(t) if t.tool == name => Some((t.state.clone(), Value::Object(t.state.input().clone()))),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no {name} tool part"))
}

fn roles(messages: &[(Message, Vec<Part>)]) -> Vec<&'static str> {
    messages.iter().map(|(m, _)| m.role()).collect()
}

fn ordered(messages: &[(Message, Vec<Part>)]) {
    assert!(messages.windows(2).all(|w| w[0].0.id() < w[1].0.id()));
    for (_, parts) in messages {
        assert!(parts.windows(2).all(|w| w[0].id < w[1].id));
    }
}

fn claude_fixture(fx: &Fx) {
    let cwd = fx.work();
    fx.write(
        &fx.home.join(".claude.json"),
        &json!({
            "numStartups": 3,
            "mcpServers": {
                "fs": {"type": "stdio", "command": "npx", "args": ["-y", "fs-server"], "env": {"API_KEY": "synthetic-value"}},
                "web": {"type": "http", "url": "https://example.invalid/mcp"}
            },
            "projects": {"/proj": {"mcpServers": {"projsrv": {"type": "stdio", "command": "srv", "args": [], "env": {}}}, "allowedTools": []}}
        })
        .to_string(),
    );
    fx.write(&fx.home.join(".claude/agents/reviewer.md"), "---\nname: reviewer\ndescription: Reviews diffs\ntools: Read, Grep\ncolor: blue\n---\nYou review diffs.\n");
    fx.write(&fx.home.join(".claude/agents/notes.txt"), "not an agent");
    fx.write(&fx.home.join(".claude/skills/shared-skill/SKILL.md"), "---\nname: shared-skill\ndescription: d\n---\nbody");
    fx.write(&fx.home.join(".claude/CLAUDE.md"), "instructions");
    let at = |s: u32| format!("2025-01-01T00:00:{s:02}.000Z");
    let base = |kind: &str, s: u32| json!({"type": kind, "sessionId": "S1", "cwd": cwd, "timestamp": at(s), "isSidechain": false});
    let with = |mut v: Value, extra: Value| {
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        v
    };
    let usage = |i: u32, o: u32| json!({"input_tokens": i, "output_tokens": o, "cache_read_input_tokens": 2, "cache_creation_input_tokens": 3});
    let project = fx.home.join(".claude/projects/C--work");
    jsonl(
        &project.join("S1.jsonl"),
        &[
            json!({"type": "permission-mode", "permissionMode": "default", "sessionId": "S1"}),
            with(base("user", 0), json!({"message": {"role": "user", "content": "Fix the bug"}})),
            with(base("user", 0), json!({"isMeta": true, "message": {"role": "user", "content": "<meta>"}})),
            with(base("assistant", 1), json!({"message": {"id": "m1", "model": "claude-x", "role": "assistant", "content": [{"type": "thinking", "thinking": "hmm", "signature": "s"}], "stop_reason": null, "usage": usage(5, 1)}})),
            with(base("assistant", 2), json!({"message": {"id": "m1", "model": "claude-x", "role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "a.txt"}}], "stop_reason": "tool_use", "usage": usage(10, 4)}})),
            with(base("user", 3), json!({"message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": [{"type": "text", "text": "file body"}]}]}, "toolUseResult": {}})),
            with(base("assistant", 4), json!({"message": {"id": "m2", "model": "claude-x", "role": "assistant", "content": [{"type": "text", "text": "Done."}], "stop_reason": "end_turn", "usage": usage(20, 6)}})),
            with(base("assistant", 4), json!({"isSidechain": true, "message": {"id": "m9", "model": "claude-x", "role": "assistant", "content": [{"type": "text", "text": "side"}]}})),
            with(base("user", 5), json!({"isCompactSummary": true, "message": {"role": "user", "content": "Summary of earlier."}})),
            json!({"type": "ai-title", "aiTitle": "Bug fix", "sessionId": "S1"}),
        ],
    );
    let sub = project.join("S1/subagents");
    jsonl(
        &sub.join("agent-a1.jsonl"),
        &[
            with(base("user", 6), json!({"isSidechain": true, "message": {"role": "user", "content": "sub task"}})),
            with(base("assistant", 7), json!({"isSidechain": true, "message": {"id": "m3", "model": "claude-x", "role": "assistant", "content": [{"type": "text", "text": "sub done"}], "stop_reason": "end_turn"}})),
        ],
    );
    fx.write(&sub.join("agent-a1.meta.json"), r#"{"agentType":"Explore","description":"Explore code"}"#);
}

#[test]
fn claude_code_import_is_rust_only_idempotent_and_read_only() {
    let fx = Fx::new();
    claude_fixture(&fx);
    std::fs::create_dir_all(&fx.paths.config).unwrap();
    let before = (snapshot(&fx.home), snapshot(&fx.paths.config));

    let reports = fx.scan();
    let claude = source(&reports, "claude-code");
    assert!(claude.found);
    assert_eq!([Category::Mcp, Category::Agents, Category::Skills, Category::Sessions].map(|c| claude.count(c)), [3, 1, 0, 2]);
    assert_eq!(claude.shared.len(), 2, "{:?}", claude.shared);
    let fs = claude.items.iter().find(|i| i.name == "fs").unwrap();
    assert!(fs.secrets() && fs.flags[0].contains("env API_KEY") && !fs.flags[0].contains("synthetic-value"));

    let store = fx.store();
    let report = fx.import(&store, &Selection::all());
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(outcome(&report, "claude-code", Category::Mcp).written, 3);
    assert_eq!(outcome(&report, "claude-code", Category::Agents).written, 1);
    assert_eq!(outcome(&report, "claude-code", Category::Sessions).written, 2);

    let mcp = fx.rust_mcp();
    assert_eq!(mcp["fs"], json!({"type": "local", "command": ["npx", "-y", "fs-server"], "environment": {"API_KEY": "synthetic-value"}}));
    assert_eq!(mcp["web"], json!({"type": "remote", "url": "https://example.invalid/mcp"}));
    assert_eq!(mcp["projsrv"], json!({"type": "local", "command": ["srv"], "enabled": false}));
    let loaded = fx.loaded();
    assert!(loaded.directories.contains(&fx.paths.rust_config()));
    assert_eq!(loaded.config["agent"]["reviewer"]["mode"], "subagent");
    assert_eq!(loaded.config["agent"]["reviewer"]["permission"], json!({"*": "deny", "read": "allow", "grep": "allow"}));
    assert_eq!(loaded.config["agent"]["reviewer"]["prompt"], "You review diffs.");
    assert!(loaded.info().unwrap().mcp.contains_key("fs"));

    let id = ids::session("claude-code", "S1");
    let session = store.get_session(&id).unwrap().unwrap();
    assert_eq!(session.title, "Bug fix");
    let metadata = session.metadata.clone().unwrap();
    assert_eq!((metadata["importedFrom"].as_str(), metadata["sourceId"].as_str()), (Some("claude-code"), Some("S1")));
    assert_eq!(session.directory, fx.work());
    let messages = store.list_messages(&id).unwrap();
    ordered(&messages);
    assert_eq!(roles(&messages), ["user", "assistant", "assistant", "user", "assistant"]);
    assert_eq!(texts(&messages[0].1), ["Fix the bug"]);
    assert_eq!(kinds(&messages[1].1), ["step-start", "reasoning", "tool", "step-finish"]);
    let (state, input) = tool(&messages[1].1, "Read");
    let ToolState::Completed(done) = state else { panic!("{state:?}") };
    assert_eq!((done.output.as_str(), &input), ("file body", &json!({"file_path": "a.txt"})));
    let first = messages[1].0.as_assistant().unwrap();
    assert_eq!((first.provider_id.as_str(), first.model_id.as_str(), first.finish.as_deref()), ("anthropic", "claude-x", Some("tool-calls")));
    assert_eq!((first.tokens.input, first.tokens.output, first.tokens.cache.read, first.tokens.cache.write), (10.0, 4.0, 2.0, 3.0));
    assert_eq!(first.parent_id, messages[0].0.id());
    assert_eq!(texts(&messages[2].1), ["Done."]);
    assert_eq!(kinds(&messages[3].1), ["compaction"]);
    assert_eq!(messages[4].0.as_assistant().unwrap().summary, Some(true));
    assert_eq!(session.tokens.unwrap().input, 30.0);

    let child = store.get_session(&ids::session("claude-code", "S1/agent-a1")).unwrap().unwrap();
    assert_eq!((child.parent_id.as_deref(), child.title.as_str()), (Some(id.as_str()), "Explore code"));
    assert_eq!(roles(&store.list_messages(&child.id).unwrap()), ["user", "assistant"]);

    // Nothing was written outside the Rust-only dir and the Rust DB; the sources are byte-identical.
    assert_eq!((snapshot(&fx.home), snapshot(&fx.paths.config)), before);
    let written: Vec<&PathBuf> = report.files.iter().filter(|f| !f.starts_with(fx.paths.rust_config())).collect();
    assert!(written.is_empty(), "{written:?}");

    let again = fx.import(&store, &Selection::all());
    assert_eq!(outcome(&again, "claude-code", Category::Mcp).unchanged, 3);
    assert_eq!(outcome(&again, "claude-code", Category::Agents).unchanged, 1);
    assert_eq!(outcome(&again, "claude-code", Category::Sessions), Outcome { source: "claude-code", category: Category::Sessions, unchanged: 2, ..Default::default() });

    // A newer source updates the import in place.
    let file = fx.home.join(".claude/projects/C--work/S1.jsonl");
    let mut text = std::fs::read_to_string(&file).unwrap();
    text.push_str(&format!("{}\n", json!({"type": "user", "sessionId": "S1", "timestamp": "2025-01-01T00:01:00.000Z", "message": {"role": "user", "content": "One more"}})));
    text.push_str(&format!("{}\n", json!({"type": "assistant", "sessionId": "S1", "timestamp": "2025-01-01T00:01:01.000Z", "message": {"id": "m4", "model": "claude-x", "content": [{"type": "text", "text": "Sure."}]}})));
    std::fs::write(&file, text).unwrap();
    bump(&file);
    let ids_before: Vec<String> = messages.iter().map(|(m, _)| m.id().to_string()).collect();
    let updated = fx.import(&store, &Selection { sources: vec!["claude-code".into()], categories: vec![Category::Sessions], groups: None });
    assert_eq!((outcome(&updated, "claude-code", Category::Sessions).written, outcome(&updated, "claude-code", Category::Sessions).unchanged), (1, 1));
    let messages = store.list_messages(&id).unwrap();
    assert_eq!(messages.len(), 7);
    assert_eq!(messages.iter().take(5).map(|(m, _)| m.id().to_string()).collect::<Vec<_>>(), ids_before);
    let state = opencode_import::load_state(&fx.paths);
    assert!(state.asked && state.sources["claude-code"].counts["sessions"] == 2);
}

fn codex_fixture(fx: &Fx) {
    let cwd = fx.work();
    let root = fx.home.join(".codex");
    fx.write(
        &root.join("config.toml"),
        "model = \"gpt-5\"\n\n[mcp_servers.docs]\ncommand = \"uvx\"\nargs = [\"docs-mcp\"]\nenv_vars = [\"DOCS_TOKEN\"]\nstartup_timeout_sec = 15\n\n[mcp_servers.docs.tools.search]\napproval = \"auto\"\n\n[mcp_servers.remote]\nurl = \"https://example.invalid/mcp\"\nenabled = false\n",
    );
    fx.write(&root.join("agents/helper.toml"), "name = \"helper\"\ndescription = \"Helps\"\ndeveloper_instructions = \"Be helpful.\"\nmodel = \"gpt-5\"\n\n[tools]\nbash = true\nfile_write = false\n");
    fx.write(&root.join("skills/fmt/SKILL.md"), "---\nname: fmt\ndescription: Formats\n---\nRun the formatter.");
    fx.write(&root.join("skills/fmt/scripts/run.sh"), "echo fmt");
    fx.write(&root.join("AGENTS.md"), "global instructions");
    let at = |s: u32| format!("2025-01-02T03:04:{s:02}.000Z");
    let line = |s: u32, kind: &str, ordinal: u32, payload: Value| json!({"timestamp": at(s), "type": kind, "payload": payload, "ordinal": ordinal});
    let usage = json!({"input_tokens": 100, "cached_input_tokens": 40, "cache_write_input_tokens": 0, "output_tokens": 20, "reasoning_output_tokens": 5, "total_tokens": 120});
    jsonl(
        &root.join("sessions/2025/01/02/rollout-2025-01-02T03-04-05-T1.jsonl"),
        &[
            line(5, "session_meta", 0, json!({"id": "T1", "session_id": "T1", "timestamp": at(5), "cwd": cwd, "model_provider": "openai", "cli_version": "0"})),
            line(6, "turn_context", 1, json!({"cwd": cwd, "model": "gpt-5", "turn_id": "x"})),
            line(6, "response_item", 2, json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "<environment_context>x</environment_context>"}]})),
            line(7, "response_item", 3, json!({"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "rules"}]})),
            line(8, "response_item", 4, json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "List files"}]})),
            line(9, "response_item", 5, json!({"type": "reasoning", "summary": [{"type": "summary_text", "text": "plan"}], "content": null, "encrypted_content": "zz"})),
            line(10, "response_item", 6, json!({"type": "function_call", "name": "exec_command", "arguments": "{\"cmd\":\"ls\"}", "call_id": "call1"})),
            line(11, "response_item", 7, json!({"type": "function_call_output", "call_id": "call1", "output": [{"type": "input_text", "text": "a.txt"}]})),
            line(12, "event_msg", 8, json!({"type": "token_count", "info": {"last_token_usage": usage}})),
            line(13, "response_item", 9, json!({"type": "custom_tool_call", "name": "apply_patch", "input": "*** Begin Patch", "call_id": "call2", "status": "completed"})),
            line(14, "response_item", 10, json!({"type": "custom_tool_call_output", "call_id": "call2", "output": "Done!"})),
            line(15, "response_item", 11, json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Here they are."}], "phase": "final_answer"})),
            line(16, "event_msg", 12, json!({"type": "token_count", "info": {"last_token_usage": usage}})),
            line(17, "compacted", 13, json!({"message": "Compacted summary", "replacement_history": []})),
            line(18, "event_msg", 14, json!({"type": "task_complete", "turn_id": "x"})),
        ],
    );
    jsonl(
        &root.join("archived_sessions/rollout-2025-01-02T03-05-00-T2.jsonl"),
        &[
            line(30, "session_meta", 0, json!({"id": "T2", "session_id": "T1", "timestamp": at(30), "cwd": cwd, "model_provider": "openai", "parent_thread_id": "T1", "subagent_history_start_ordinal": 2, "source": {"subagent": {}}})),
            line(31, "response_item", 1, json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "inherited"}]})),
            line(32, "response_item", 2, json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "sub work"}]})),
            line(33, "response_item", 3, json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "ok"}]})),
        ],
    );
    fx.write(&root.join("session_index.jsonl"), &format!("{}\n", json!({"id": "T1", "thread_name": "Listing", "updated_at": at(18)})));
}

#[test]
fn codex_config_agents_skills_and_rollouts() {
    let fx = Fx::new();
    codex_fixture(&fx);
    let before = snapshot(&fx.home);
    let reports = fx.scan();
    let codex = source(&reports, "codex");
    assert_eq!([Category::Mcp, Category::Agents, Category::Skills, Category::Sessions].map(|c| codex.count(c)), [2, 1, 1, 2]);
    assert!(codex.warnings.iter().any(|w| w.contains("AGENTS.md")));

    let store = fx.store();
    let report = fx.import(&store, &Selection::all());
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let mcp = fx.rust_mcp();
    assert_eq!(mcp["docs"], json!({"type": "local", "command": ["uvx", "docs-mcp"], "environment": {"DOCS_TOKEN": "{env:DOCS_TOKEN}"}, "timeout": 15000}));
    assert_eq!(mcp["remote"], json!({"type": "remote", "url": "https://example.invalid/mcp", "enabled": false}));
    let skill = fx.paths.rust_config().join("skills/fmt");
    assert_eq!(std::fs::read_to_string(skill.join("scripts/run.sh")).unwrap(), "echo fmt");
    let loaded = fx.loaded();
    assert_eq!(loaded.config["agent"]["helper"]["permission"], json!({"bash": "allow", "edit": "deny"}));
    assert_eq!(loaded.config["agent"]["helper"]["model"], "openai/gpt-5");
    let discovered = opencode_core::skill::discover(&opencode_core::skill::DiscoverInput {
        home: &fx.home,
        cwd: &fx.root.join("work"),
        worktree: None,
        config_dirs: &loaded.directories,
        config: &Default::default(),
        disable_external: false,
        disable_claude_code: false,
    });
    assert!(discovered.files.contains(&skill.join("SKILL.md")));

    let id = ids::session("codex", "T1");
    let session = store.get_session(&id).unwrap().unwrap();
    assert_eq!(session.title, "Listing");
    let messages = store.list_messages(&id).unwrap();
    ordered(&messages);
    assert_eq!(roles(&messages), ["user", "assistant", "assistant", "user", "assistant"]);
    assert_eq!(texts(&messages[0].1), ["List files"]);
    assert_eq!(kinds(&messages[1].1), ["step-start", "reasoning", "tool", "step-finish"]);
    let (state, input) = tool(&messages[1].1, "exec_command");
    assert!(matches!(&state, ToolState::Completed(done) if done.output == "a.txt"));
    assert_eq!(input, json!({"cmd": "ls"}));
    let first = messages[1].0.as_assistant().unwrap();
    assert_eq!((first.provider_id.as_str(), first.model_id.as_str()), ("openai", "gpt-5"));
    assert_eq!((first.tokens.input, first.tokens.output, first.tokens.reasoning, first.tokens.cache.read), (60.0, 15.0, 5.0, 40.0));
    let (state, input) = tool(&messages[2].1, "apply_patch");
    assert!(matches!(&state, ToolState::Completed(done) if done.output == "Done!"));
    assert_eq!(input, json!({"input": "*** Begin Patch"}));
    assert_eq!(texts(&messages[2].1), ["Here they are."]);
    assert_eq!(texts(&messages[4].1), ["Compacted summary"]);

    let child = store.get_session(&ids::session("codex", "T2")).unwrap().unwrap();
    assert_eq!(child.parent_id.as_deref(), Some(id.as_str()));
    let child_messages = store.list_messages(&child.id).unwrap();
    assert_eq!(texts(&child_messages[0].1), ["sub work"]);
    assert_eq!(child_messages.len(), 2);
    assert_eq!(snapshot(&fx.home), before);

    let again = fx.import(&store, &Selection::all());
    assert_eq!(outcome(&again, "codex", Category::Sessions).unchanged, 2);
    assert_eq!(outcome(&again, "codex", Category::Skills).unchanged, 1);
    assert_eq!(outcome(&again, "codex", Category::Mcp).unchanged, 2);
}

const OPENCODE_SCHEMA: &str = "
CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT NOT NULL, vcs TEXT, name TEXT, icon_url TEXT, icon_color TEXT,
  time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, time_initialized INTEGER, sandboxes TEXT NOT NULL, commands TEXT);
CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, parent_id TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL,
  title TEXT NOT NULL, version TEXT NOT NULL, share_url TEXT, summary_additions INTEGER, summary_deletions INTEGER,
  summary_files INTEGER, summary_diffs TEXT, revert TEXT, permission TEXT, time_created INTEGER NOT NULL,
  time_updated INTEGER NOT NULL, time_compacting INTEGER, time_archived INTEGER, workspace_id TEXT, path TEXT, agent TEXT,
  model TEXT, cost REAL NOT NULL DEFAULT 0, tokens_input INTEGER NOT NULL DEFAULT 0, metadata TEXT);
CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
CREATE TABLE todo (session_id TEXT NOT NULL, content TEXT NOT NULL, status TEXT NOT NULL, priority TEXT NOT NULL, position INTEGER NOT NULL,
  time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, PRIMARY KEY (session_id, position));
CREATE TABLE session_v2 (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, parent_id TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL,
  path TEXT, title TEXT, version TEXT NOT NULL, agent TEXT, model TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL, seq INTEGER NOT NULL,
  time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
";

fn legacy_session(db: &rusqlite::Connection, id: &str, updated: i64) {
    db.execute(
        "INSERT INTO session (id, project_id, slug, directory, title, version, summary_additions, summary_deletions, summary_files, time_created, time_updated, model, permission)
         VALUES (?1, 'proj1', 'calm-otter', '/w', 'Legacy ' || ?1, '1.0.0', 3, 1, 2, 1000, ?2, '{\"id\":\"m\",\"providerID\":\"anthropic\"}', '[{\"permission\":\"edit\",\"pattern\":\"*\",\"action\":\"deny\"}]')",
        rusqlite::params![id, updated],
    )
    .unwrap();
    let tokens = json!({"input": 10, "output": 5, "reasoning": 0, "cache": {"read": 1, "write": 0}});
    let user = json!({"role": "user", "time": {"created": 1000}, "agent": "build", "model": {"providerID": "anthropic", "modelID": "m"}, "summary": {"diffs": []}});
    let assistant = json!({"role": "assistant", "time": {"created": 1001, "completed": 1005}, "parentID": format!("msg_{id}_1"), "modelID": "m", "providerID": "anthropic", "mode": "build", "agent": "build", "path": {"cwd": "/w", "root": "/w"}, "cost": 0.5, "tokens": tokens, "finish": "stop"});
    for (n, data) in [(1, user), (2, assistant)] {
        db.execute("INSERT INTO message VALUES (?1, ?2, 1000, 1000, ?3)", rusqlite::params![format!("msg_{id}_{n}"), id, data.to_string()]).unwrap();
    }
    let parts = [
        (1, 1, json!({"type": "text", "text": "hello"})),
        (2, 2, json!({"type": "tool", "callID": "c1", "tool": "read", "state": {"status": "completed", "input": {"filePath": "/w/a"}, "output": "x", "title": "a", "metadata": {}, "time": {"start": 1002, "end": 1003}}})),
        (3, 2, json!({"type": "step-finish", "reason": "stop", "cost": 0.5, "tokens": tokens})),
        (4, 2, json!({"type": "mystery"})),
    ];
    for (n, message, data) in parts {
        db.execute("INSERT INTO part VALUES (?1, ?2, ?3, 1000, 1000, ?4)", rusqlite::params![format!("prt_{id}_{n}"), format!("msg_{id}_{message}"), id, data.to_string()]).unwrap();
    }
    db.execute("INSERT INTO todo VALUES (?1, 'write tests', 'pending', 'high', 0, 1000, 1000)", [id]).unwrap();
}

fn v2_session(db: &rusqlite::Connection, id: &str, updated: i64) {
    db.execute("INSERT INTO session_v2 VALUES (?1, 'proj1', NULL, 'swift-moon', '/w', NULL, 'V2 ' || ?1, '2.0.0', 'build', '{\"id\":\"gpt\",\"providerID\":\"openai\"}', 2000, ?2)", rusqlite::params![id, updated]).unwrap();
    let rows = [
        ("user", json!({"text": "do it", "time": {"created": 2000}, "files": [{"name": "a.png", "mime": "image/png", "data": "AAAA", "source": {"type": "inline"}}]})),
        ("assistant", json!({"agent": "build", "model": {"id": "gpt", "providerID": "openai", "variant": "high"}, "content": [
            {"type": "reasoning", "text": "think", "time": {"created": 2001, "completed": 2002}},
            {"type": "text", "text": "working"},
            {"type": "tool", "id": "call_1", "name": "bash", "state": {"status": "completed", "input": {"command": "ls"}, "content": [{"type": "text", "text": "out"}], "metadata": {"exit": 0}}, "time": {"created": 2003, "ran": 2004, "completed": 2005}},
            {"type": "tool", "id": "call_2", "name": "read", "state": {"status": "error", "input": {}, "error": {"type": "unknown", "message": "nope"}}, "time": {"created": 2006, "completed": 2007}}
        ], "finish": "tool-calls", "cost": 0.25, "tokens": {"input": 7, "output": 3, "reasoning": 1, "cache": {"read": 0, "write": 0}}, "time": {"created": 2001, "completed": 2008}})),
        ("idle", json!({"outcome": "done", "time": {"created": 2009}})),
        ("model-switched", json!({"model": {"id": "x", "providerID": "y"}, "previous": {"id": "gpt", "providerID": "openai"}, "time": {"created": 2010}})),
        ("compaction", json!({"reason": "auto", "status": "completed", "summary": "Summary text", "recent": "", "time": {"created": 2011}})),
    ];
    // Ids deliberately out of seq order: the import follows `seq`.
    for (seq, (kind, data)) in rows.into_iter().enumerate() {
        let row_id = format!("msg_{id}_{}", 9 - seq);
        db.execute("INSERT INTO session_message VALUES (?1, ?2, ?3, ?4, 2000, 2000, ?5)", rusqlite::params![row_id, id, kind, seq as i64, data.to_string()]).unwrap();
    }
}

#[test]
fn opencode_database_v1_v2_dedupe_read_only() {
    let fx = Fx::new();
    std::fs::create_dir_all(fx.paths.config.join("agents")).unwrap();
    fx.write(&fx.paths.config.join("opencode.json"), r#"{"mcp":{"orig":{"type":"local","command":["o"]}}}"#);
    let file = fx.paths.data.join("opencode.db");
    {
        let db = rusqlite::Connection::open(&file).unwrap();
        db.execute_batch(OPENCODE_SCHEMA).unwrap();
        db.execute("INSERT INTO project VALUES ('proj1', '/w', 'git', 'Work', NULL, NULL, 10, 20, NULL, '[]', NULL)", []).unwrap();
        legacy_session(&db, "ses_legacy", 3000);
        legacy_session(&db, "ses_both", 3000);
        v2_session(&db, "ses_both", 4000);
        v2_session(&db, "ses_v2only", 2500);
    }
    let before = snapshot(&fx.paths.data);
    let reports = fx.scan();
    let opencode = source(&reports, "opencode");
    assert_eq!(opencode.count(Category::Sessions), 3);
    assert!(opencode.shared[0].contains("1 MCP servers"), "{:?}", opencode.shared);

    let store = fx.store();
    let report = fx.import(&store, &Selection { sources: vec!["opencode".into()], ..Default::default() });
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(outcome(&report, "opencode", Category::Sessions).written, 3);
    let after: BTreeMap<_, _> = snapshot(&fx.paths.data).into_iter().filter(|(p, _)| !p.to_string_lossy().contains("opencode-rs.db")).collect();
    assert_eq!(after, before);
    assert!(store.get_project("proj1").unwrap().is_some_and(|p| p.name.as_deref() == Some("Work")));

    let legacy = store.get_session("ses_legacy").unwrap().unwrap();
    assert_eq!((legacy.title.as_str(), legacy.cost, legacy.permission.as_ref().map(Vec::len)), ("Legacy ses_legacy", Some(0.5), Some(1)));
    assert_eq!(legacy.metadata.as_ref().unwrap()["importSkippedRows"], 1);
    let messages = store.list_messages("ses_legacy").unwrap();
    assert_eq!(messages.iter().map(|(m, _)| m.id()).collect::<Vec<_>>(), ["msg_ses_legacy_1", "msg_ses_legacy_2"]);
    assert_eq!(kinds(&messages[1].1), ["tool", "step-finish"]);
    assert_eq!(store.get_todos("ses_legacy").unwrap().len(), 1);

    let both = store.get_session("ses_both").unwrap().unwrap();
    assert_eq!(both.title, "V2 ses_both");
    let messages = store.list_messages("ses_both").unwrap();
    ordered(&messages);
    assert_eq!(roles(&messages), ["user", "assistant", "user", "assistant"]);
    assert_eq!(kinds(&messages[0].1), ["text", "file"]);
    let PartBody::File(image) = &messages[0].1[1].body else { panic!() };
    assert_eq!(image.url, "data:image/png;base64,AAAA");
    assert_eq!(kinds(&messages[1].1), ["step-start", "reasoning", "text", "tool", "tool", "step-finish"]);
    let assistant = messages[1].0.as_assistant().unwrap();
    assert_eq!((assistant.model_id.as_str(), assistant.variant.as_deref(), assistant.cost, assistant.finish.as_deref()), ("gpt", Some("high"), 0.25, Some("tool-calls")));
    assert!(matches!(tool(&messages[1].1, "bash").0, ToolState::Completed(done) if done.output == "out" && done.time.start == 2004));
    assert!(matches!(tool(&messages[1].1, "read").0, ToolState::Error(error) if error.error == "nope"));
    assert_eq!(kinds(&messages[2].1), ["compaction"]);
    assert_eq!(texts(&messages[3].1), ["Summary text"]);
    assert_eq!(messages[0].0.as_user().unwrap().model.model_id, "gpt");
    assert_eq!(both.cost, Some(0.25));

    let again = fx.import(&store, &Selection { sources: vec!["opencode".into()], ..Default::default() });
    assert_eq!(outcome(&again, "opencode", Category::Sessions).unchanged, 3);
    // MCP servers, agents and skills of the original are shared, not imported.
    assert!(!fx.paths.rust_config().exists());
}

// `session_part` (migration `20261029002625_session_part`) and `blob`/`blob_ref` (`20261025123711_blob`) of v2.2.7.
const OPENCODE_V2_PARTS_SCHEMA: &str = "
CREATE TABLE `session_part` (`message_id` text NOT NULL, `session_id` text NOT NULL, `ordinal` integer NOT NULL, `type` text NOT NULL,
  `call_id` text, `time_created` integer NOT NULL, `time_updated` integer NOT NULL, `data` text NOT NULL,
  CONSTRAINT `session_part_pk` PRIMARY KEY(`message_id`, `ordinal`));
CREATE TABLE `blob_ref` (`session_id` text NOT NULL, `message_id` text NOT NULL, `hash` text NOT NULL, `seq` integer NOT NULL,
  CONSTRAINT `blob_ref_pk` PRIMARY KEY(`session_id`, `message_id`, `hash`));
CREATE TABLE `blob` (`hash` text PRIMARY KEY, `mime` text NOT NULL, `bytes` integer NOT NULL, `width` integer, `height` integer,
  `data` blob NOT NULL, `time_created` integer NOT NULL);
";

#[test]
fn opencode_v2_parts_blobs_and_renamed_tools() {
    use base64::Engine;
    let b64 = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
    let fx = Fx::new();
    let file = fx.paths.data.join("opencode.db");
    let (image, shot) = ("a".repeat(64), "b".repeat(64));
    {
        let db = rusqlite::Connection::open(&file).unwrap();
        db.execute_batch(OPENCODE_SCHEMA).unwrap();
        db.execute_batch(OPENCODE_V2_PARTS_SCHEMA).unwrap();
        db.execute("INSERT INTO session_v2 VALUES ('ses_parts', 'proj1', NULL, 'swift-moon', '/w', NULL, 'Parts', '2.2.7', 'build', '{\"id\":\"gpt\",\"providerID\":\"openai\"}', 2000, 3000)", []).unwrap();
        for (hash, bytes) in [(&image, b"PNGDATA".as_slice()), (&shot, b"SHOT".as_slice())] {
            db.execute("INSERT INTO blob VALUES (?1, 'image/png', ?2, NULL, NULL, ?3, 1)", rusqlite::params![hash, bytes.len() as i64, bytes]).unwrap();
            db.execute("INSERT INTO blob_ref VALUES ('ses_parts', 'msg_1', ?1, 0)", [hash]).unwrap();
        }
        let user = json!({"text": "look", "time": {"created": 2000}, "files": [
            {"name": "a.png", "mime": "image/png", "uri": format!("media:{image}")},
            {"name": "gone.png", "mime": "image/png", "uri": format!("media:{}", "c".repeat(64))}
        ]});
        // Split assistant rows keep `content: []` (or what an older build appended inline after the split).
        let split = json!({"agent": "build", "model": {"id": "gpt", "providerID": "openai"}, "content": [{"type": "text", "text": "tail"}],
            "tokens": {"input": 1, "output": 1, "reasoning": 0, "cache": {"read": 0, "write": 0}}, "time": {"created": 2001, "completed": 2009}});
        let legacy = json!({"agent": "build", "model": {"id": "gpt", "providerID": "openai"}, "content": [{"type": "text", "text": "legacy inline"}], "time": {"created": 2010, "completed": 2011}});
        for (seq, (id, kind, data)) in [("msg_1", "user", user), ("msg_2", "assistant", split), ("msg_3", "assistant", legacy)].into_iter().enumerate() {
            db.execute("INSERT INTO session_message VALUES (?1, 'ses_parts', ?2, ?3, 2000, 2000, ?4)", rusqlite::params![id, kind, seq as i64, data.to_string()]).unwrap();
        }
        let done = |id: &str, name: &str, input: Value, content: Value| json!({"type": "tool", "id": id, "name": name, "state": {"status": "completed", "input": input, "content": content}, "time": {"created": 2003, "ran": 2004, "completed": 2005}});
        let parts = [
            json!({"type": "text", "text": "first"}),
            done("c1", "shell", json!({"command": "shot"}), json!([{"type": "text", "text": "out"}, {"type": "file", "mime": "image/png", "name": "s.png", "uri": format!("media:{shot}")}, {"type": "file", "mime": "image/png", "uri": format!("media:{}", "d".repeat(64))}])),
            done("c2", "subagent", json!({"agent": "explore", "prompt": "p", "sessionID": "ses_child"}), json!([{"type": "text", "text": "child done"}])),
            done("c3", "read", json!({"path": "src/a.rs"}), json!([{"type": "text", "text": "fn a() {}"}])),
            done("c4", "patch", json!({"patchText": "*** Begin Patch"}), json!([{"type": "text", "text": "ok"}])),
        ];
        // Inserted out of order: the import follows `ordinal`.
        for (ordinal, part) in parts.iter().enumerate().rev() {
            let call = part["id"].as_str();
            db.execute("INSERT INTO session_part VALUES ('msg_2', 'ses_parts', ?1, ?2, ?3, 2002, 2002, ?4)", rusqlite::params![ordinal as i64, part["type"].as_str().unwrap(), call, part.to_string()]).unwrap();
        }
    }
    let before = snapshot(&fx.paths.data);
    let store = fx.store();
    let report = fx.import(&store, &Selection { sources: vec!["opencode".into()], ..Default::default() });
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let messages = store.list_messages("ses_parts").unwrap();
    ordered(&messages);
    assert_eq!(roles(&messages), ["user", "assistant", "assistant"]);

    let files: Vec<(String, String)> = messages[0].1.iter().filter_map(|p| match &p.body {
        PartBody::File(f) => Some((f.mime.clone(), f.url.clone())),
        _ => None,
    }).collect();
    assert_eq!(files, [
        ("image/png".to_string(), format!("data:image/png;base64,{}", b64(b"PNGDATA"))),
        ("text/plain".to_string(), format!("data:text/plain;base64,{}", b64(b"[media unavailable: gone.png (image/png)]"))),
    ]);

    assert_eq!(kinds(&messages[1].1), ["step-start", "text", "tool", "tool", "tool", "tool", "text", "step-finish"]);
    assert_eq!(texts(&messages[1].1), ["first", "tail"]);
    let ToolState::Completed(bash) = tool(&messages[1].1, "bash").0 else { panic!("bash not completed") };
    assert_eq!(bash.output, "out\n[media unavailable: image/png]");
    let attachments = bash.attachments.unwrap();
    assert_eq!(attachments.len(), 1);
    assert_eq!((attachments[0].file.url.clone(), attachments[0].file.filename.as_deref()), (format!("data:image/png;base64,{}", b64(b"SHOT")), Some("s.png")));
    assert_eq!(attachments[0].message_id, messages[1].0.id());
    assert_eq!(tool(&messages[1].1, "task").1, json!({"prompt": "p", "subagent_type": "explore", "task_id": "ses_child"}));
    assert_eq!(tool(&messages[1].1, "read").1, json!({"filePath": "src/a.rs"}));
    assert!(matches!(tool(&messages[1].1, "apply_patch").0, ToolState::Completed(done) if done.output == "ok"));
    assert_eq!(texts(&messages[2].1), ["legacy inline"]);

    let after: BTreeMap<_, _> = snapshot(&fx.paths.data).into_iter().filter(|(p, _)| !p.to_string_lossy().contains("opencode-rs.db")).collect();
    assert_eq!(after, before);
}

#[test]
fn mcp_json_sources_and_name_collisions() {
    let fx = Fx::new();
    fx.write(&fx.paths.config.join("opencode.jsonc"), "// mine\n{\"mcp\":{\"taken\":{\"type\":\"remote\",\"url\":\"https://t\"}}}");
    fx.write(&fx.home.join(".cursor/mcp.json"), r#"{"mcpServers":{"taken":{"url":"https://x"},"c1":{"command":"c","args":["1"]}}}"#);
    fx.write(&fx.home.join(".windsurf/mcp.json"), r#"{"mcpServers":{"c1":{"command":"c","args":["1"]},"w1":{"type":"sse","url":"https://w/sse","headers":{"X-Api-Key":"k"}}}}"#);
    fx.write(&fx.home.join(".mcp.json"), r#"{"mcpServers":{"c1":{"command":"other"},"m1":{"command":"m"},"bad":{"type":"stdio"}}}"#);
    let before = snapshot(&fx.paths.config);
    let reports = fx.scan();
    assert!(source(&reports, "mcp-json").warnings.iter().any(|w| w.contains("bad")));
    let plan = opencode_import::plan(&reports, &Selection::all());
    let reasons: Vec<(&str, &str, &str)> = plan.skipped.iter().map(|s| (s.source, s.name.as_str(), s.reason.as_str())).collect();
    assert!(reasons.contains(&("cursor", "taken", "an MCP server of this name is already configured for opencode")));
    assert!(reasons.contains(&("windsurf", "c1", "duplicate of Cursor")));
    assert!(reasons.contains(&("mcp-json", "c1", "name also defined by Cursor")));
    assert!(plan.flagged().any(|p| p.item.name == "w1" && p.item.secrets()));

    let store = fx.store();
    let report = opencode_import::apply(&plan, &store, &fx.paths);
    assert!(report.errors.is_empty());
    let mcp = fx.rust_mcp();
    assert_eq!(mcp.as_object().unwrap().keys().collect::<Vec<_>>(), ["c1", "w1", "m1"]);
    assert_eq!(mcp["w1"], json!({"type": "remote", "url": "https://w/sse", "headers": {"X-Api-Key": "k"}}));
    assert_eq!(snapshot(&fx.paths.config), before);
    let text = std::fs::read_to_string(fx.paths.rust_config().join("opencode.jsonc")).unwrap();
    assert!(text.starts_with("// Written by `opencode import`"));

    // A server the user changed in the Rust-only config afterwards is not overwritten.
    std::fs::write(fx.paths.rust_config().join("opencode.jsonc"), text.replace("\"m\"", "\"m-edited\"")).unwrap();
    let reports = fx.scan();
    let m1 = source(&reports, "mcp-json").items.iter().find(|i| i.name == "m1").unwrap();
    assert!(m1.conflict.as_deref().is_some_and(|c| c.contains("opencode-rs")));
}

#[test]
fn grok_build_and_deepseek_harness_layouts() {
    let mut fx = Fx::new();
    let cwd = fx.work();
    let grok = fx.root.join("grokhome");
    let dsh = fx.root.join("dshhome");
    fx.env.insert("GROK_HOME".into(), grok.to_string_lossy().into_owned());
    fx.env.insert("DSH_HOME".into(), dsh.to_string_lossy().into_owned());
    fx.write(&grok.join("config.toml"), "disabled_mcp_servers = [\"off\"]\n\n[mcp_servers.gfs]\ncommand = \"gfs\"\nargs = [\"--x\"]\n\n[mcp_servers.off]\nurl = \"https://example.invalid/sse\"\ntype = \"sse\"\n");
    fx.write(&grok.join("agents/gagent.md"), "---\nname: gagent\ndescription: G agent\ntools: [read_file, run_terminal_cmd]\nmodel: grok-4\n---\nPrompt");
    fx.write(&grok.join("skills/gskill/SKILL.md"), "---\nname: gskill\ndescription: g\n---\nbody");
    let session = grok.join("sessions/%2Fwork/0190-a");
    fx.write(&session.join("summary.json"), &json!({"info": {"id": "G1", "cwd": cwd}, "generated_title": "Grok title", "created_at": "2025-02-01T00:00:00Z", "updated_at": "2025-02-01T00:10:00Z", "current_model_id": "grok-4", "chat_format_version": 1}).to_string());
    jsonl(
        &session.join("chat_history.jsonl"),
        &[
            json!({"type": "system", "content": "sys"}),
            json!({"type": "user", "content": [{"type": "text", "text": "hi grok"}], "prompt_index": 0}),
            json!({"type": "user", "content": [{"type": "text", "text": "reminder"}], "synthetic_reason": "reminder"}),
            json!({"type": "reasoning", "id": "rs1", "summary": [{"type": "summary_text", "text": "thinking"}]}),
            json!({"type": "assistant", "content": "Reading", "tool_calls": [{"id": "c1", "name": "read_file", "arguments": "{\"path\":\"a\"}"}], "model_id": "grok-4"}),
            json!({"type": "tool_result", "tool_call_id": "c1", "content": "A"}),
            json!({"type": "assistant", "content": "Done", "tool_calls": []}),
        ],
    );
    fx.write(&grok.join("sessions/%2Fwork/0190-b/summary.json"), r#"{"info":{"id":"G2","cwd":"/"},"hidden":true}"#);

    fx.write(
        &dsh.join("profiles/web/cordis.patch.yml"),
        "- id: mcp-manager\n  name: '@deepseek-ai/dsh-mcp-manager'\n  config:\n    servers:\n      dlocal: {transport: stdio, command: dcmd, args: [a], env: {}, cwd: '', enabled: true}\n      dweb: {transport: streamable-http, url: 'https://example.invalid/mcp', headers: {}, enabled: false}\n- insert:\n    - id: demo\n      name: '@deepseek-ai/dsh-mcp-client'\n      disabled: true\n      config: {serverName: ddemo, transport: stdio, command: demo}\n- id: x\n  name: other\n  config:\n    expr: !!js \"1 + 1\"\n",
    );
    fx.write(&dsh.join("skills/dskill/SKILL.md"), "---\nname: dskill\ndescription: d\n---\nbody");
    fx.write(&dsh.join("skills/.system/hidden/SKILL.md"), "---\nname: hidden\n---\nx");
    let dir = dsh.join("sessions/--work--/s1");
    jsonl(&dir.join("session.jsonl"), &[json!({"type": "session", "version": 0, "id": "OLD", "createdAt": 1, "isSeeded": false, "delegationDepth": 0})]);
    jsonl(
        &dir.join("session.v4.jsonl"),
        &[
            json!({"type": "session", "version": 4, "id": "D1", "createdAt": 1_700_000_000_000u64, "cwd": cwd, "isSeeded": false, "delegationDepth": 0}),
            json!({"type": "turn/start", "seq": 0, "time": 1_700_000_000_001u64, "data": {"turn": 1}}),
            json!({"type": "user/message", "seq": 1, "time": 1_700_000_000_002u64, "data": {"id": "u1", "role": "user", "content": [{"type": "text", "text": "hello dsh"}], "source": {"kind": "user"}}}),
            json!({"type": "user/message", "seq": 2, "time": 1_700_000_000_003u64, "data": {"id": "u2", "role": "user", "content": [{"type": "text", "text": "skill ctx"}], "source": {"kind": "skill"}}}),
            json!({"type": "assistant/message", "seq": 3, "time": 1_700_000_000_010u64, "data": {"turn": 1, "step": 1, "message": {"id": "a1", "role": "assistant", "content": [{"type": "reasoning", "text": "r"}, {"type": "text", "text": "t"}, {"type": "tool-call", "id": "c1", "name": "read", "arguments": "{\"file_path\":\"x\"}"}], "source": {"kind": "model", "provider": "deepseek", "model": "deepseek-v4"}}, "stream": [], "usage": {"inputTokens": 7, "outputTokens": 3}}}),
            json!({"type": "tool/call", "seq": 4, "time": 1_700_000_000_011u64, "data": {"turn": 1, "step": 1, "callId": "c1", "name": "read", "arguments": "{}"}}),
            json!({"type": "tool/result", "seq": 5, "time": 1_700_000_000_020u64, "data": {"turn": 1, "step": 1, "message": {"id": "r1", "role": "tool", "toolCallId": "c1", "source": {"kind": "tool", "callId": "c1"}, "content": [{"type": "text", "text": "err"}], "isError": true}, "error": {"name": "FsError", "code": "ENOENT"}}}),
            json!({"type": "session/title", "seq": 6, "time": 1_700_000_000_030u64, "data": {"title": "DSH title", "messageSeqs": [1], "source": {"kind": "user"}}}),
        ],
    );
    fx.write(&dsh.join("sessions/--work--/s2/session.v4.jsonl.zstd"), "\u{28}\u{b5}");
    let before = (snapshot(&grok), snapshot(&dsh));

    let reports = fx.scan();
    let g = source(&reports, "grok-build");
    assert_eq!([Category::Mcp, Category::Agents, Category::Skills, Category::Sessions].map(|c| g.count(c)), [2, 1, 1, 1]);
    let d = source(&reports, "deepseek-harness");
    assert_eq!([Category::Mcp, Category::Agents, Category::Skills, Category::Sessions].map(|c| d.count(c)), [3, 0, 1, 1]);
    assert!(d.warnings.iter().any(|w| w.contains("1 zstd-compressed")), "{:?}", d.warnings);

    let store = fx.store();
    let report = fx.import(&store, &Selection::all());
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let mcp = fx.rust_mcp();
    assert_eq!(mcp["gfs"], json!({"type": "local", "command": ["gfs", "--x"]}));
    assert_eq!(mcp["off"], json!({"type": "remote", "url": "https://example.invalid/sse", "enabled": false}));
    assert_eq!(mcp["dlocal"], json!({"type": "local", "command": ["dcmd", "a"], "enabled": true}));
    assert_eq!(mcp["dweb"]["enabled"], false);
    assert_eq!(mcp["ddemo"], json!({"type": "local", "command": ["demo"], "enabled": false}));
    let loaded = fx.loaded();
    assert_eq!(loaded.config["agent"]["gagent"]["model"], "xai/grok-4");
    assert_eq!(loaded.config["agent"]["gagent"]["permission"], json!({"*": "deny", "read": "allow", "bash": "allow"}));
    assert!(fx.paths.rust_config().join("skills/gskill/SKILL.md").is_file() && fx.paths.rust_config().join("skills/dskill/SKILL.md").is_file());

    let grok_session = store.get_session(&ids::session("grok-build", "G1")).unwrap().unwrap();
    assert_eq!(grok_session.title, "Grok title");
    let messages = store.list_messages(&grok_session.id).unwrap();
    ordered(&messages);
    assert_eq!(roles(&messages), ["user", "assistant", "assistant"]);
    assert_eq!(kinds(&messages[1].1), ["step-start", "reasoning", "text", "tool"]);
    assert!(matches!(tool(&messages[1].1, "read_file").0, ToolState::Completed(done) if done.output == "A"));
    assert_eq!(messages[1].0.as_assistant().unwrap().model_id, "grok-4");

    let dsh_session = store.get_session(&ids::session("deepseek-harness", "D1")).unwrap().unwrap();
    assert_eq!((dsh_session.title.as_str(), dsh_session.time.created), ("DSH title", 1_700_000_000_000));
    let messages = store.list_messages(&dsh_session.id).unwrap();
    assert_eq!(roles(&messages), ["user", "assistant"]);
    assert_eq!(texts(&messages[0].1), ["hello dsh"]);
    assert_eq!(kinds(&messages[1].1), ["step-start", "reasoning", "text", "tool", "step-finish"]);
    assert!(matches!(tool(&messages[1].1, "read").0, ToolState::Error(error) if error.error == "err"));
    assert_eq!((snapshot(&grok), snapshot(&dsh)), before);
}

#[test]
fn empty_home_finds_nothing_and_first_run_state() {
    let fx = Fx::new();
    let reports = fx.scan();
    assert_eq!(reports.len(), opencode_import::SOURCES.len());
    assert!(reports.iter().all(|r| !r.found && r.items.is_empty()), "{:?}", reports.iter().filter(|r| r.found).map(|r| r.id).collect::<Vec<_>>());
    assert!(!opencode_import::first_run_pending(&fx.paths, &fx.home));
    let report = opencode_import::apply(&opencode_import::plan(&reports, &Selection::all()), &fx.store(), &fx.paths);
    assert!(report.outcomes.is_empty() && report.files.is_empty());
    assert!(!fx.paths.rust_config().exists());

    let fx = Fx::new();
    fx.write(&fx.home.join(".codex/config.toml"), "");
    assert_eq!(opencode_import::detect(&fx.paths, &fx.home), ["Codex"]);
    assert!(opencode_import::first_run_pending(&fx.paths, &fx.home));
    opencode_import::mark_asked(&fx.paths).unwrap();
    assert!(!opencode_import::first_run_pending(&fx.paths, &fx.home));
    let state: Value = serde_json::from_str(&std::fs::read_to_string(fx.paths.state.join("import-rs.json")).unwrap()).unwrap();
    assert_eq!(state, json!({"asked": true, "sources": {}}));
}
