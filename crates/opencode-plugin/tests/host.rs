use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opencode_plugin::{
    ChatInput, ChatParams, ChatParamsOutput, Event, FetchRequest, HookName, PluginError, PluginHost, PluginOptions,
    PluginSpec, ShellEnv, ShellEnvInput, ShellEnvOutput, ToolExecuteBefore, ToolExecuteBeforeInput,
    ToolExecuteBeforeOutput, discover, find_bun, npm_target,
};
use opencode_tools::{PermissionAsker, PermissionRequest, ToolContext, ToolError};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn bun() -> Option<PathBuf> {
    match find_bun(None) {
        Ok(path) => Some(path),
        Err(error) => {
            eprintln!("SKIPPED (bun-dependent test): {error}");
            None
        }
    }
}

// Zod v4 from Bun's local install cache (read-only); never fetched from the network.
fn zod_url() -> Option<String> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    let cache = std::env::var_os("BUN_INSTALL_CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(home).join(".bun").join("install").join("cache"));
    let mut hits: Vec<PathBuf> = std::fs::read_dir(cache)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            name.starts_with("zod@4.") && path.join("index.js").is_file()
        })
        .collect();
    hits.sort();
    Some(url::Url::from_file_path(hits.pop()?.join("index.js")).ok()?.to_string())
}

struct Fixture {
    tmp: tempfile::TempDir,
    logs: Arc<Mutex<Vec<String>>>,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self { tmp: tempfile::tempdir().unwrap(), logs: Arc::default() };
        std::fs::create_dir_all(fixture.config_dir()).unwrap();
        fixture.write(
            "sdk-stub.mjs",
            "export function createOpencodeClient(config) { return { stub: true, config } }\n",
        );
        fixture
    }

    fn root(&self) -> PathBuf {
        self.tmp.path().to_path_buf()
    }

    fn project(&self) -> PathBuf {
        self.root().join("proj")
    }

    fn config_dir(&self) -> PathBuf {
        self.project().join(".opencode")
    }

    fn cache(&self) -> PathBuf {
        self.root().join("cache")
    }

    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let file = rel.split('/').fold(self.root(), |p, s| p.join(s));
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, text).unwrap();
        file
    }

    fn options(&self) -> PluginOptions {
        let found = discover(&json!({"plugin": []}), &[self.config_dir()]);
        let logs = self.logs.clone();
        PluginOptions {
            specs: found.specs,
            tool_files: found.tool_files,
            directory: self.project(),
            worktree: self.project(),
            project_id: "proj_1".into(),
            server_url: "http://127.0.0.1:1".into(),
            cache_dir: self.cache(),
            sdk_path: Some(self.root().join("sdk-stub.mjs").to_string_lossy().into_owned()),
            zod_path: zod_url(),
            resolve_dirs: vec![self.config_dir()],
            log: Some(Arc::new(move |line: &str| logs.lock().unwrap().push(line.to_string()))),
            ..PluginOptions::default()
        }
    }

    fn logs(&self) -> String {
        self.logs.lock().unwrap().join("\n")
    }
}

struct Asker {
    allow: bool,
    seen: Mutex<Vec<PermissionRequest>>,
}

#[async_trait::async_trait]
impl PermissionAsker for Asker {
    async fn ask(&self, req: PermissionRequest) -> Result<(), ToolError> {
        self.seen.lock().unwrap().push(req);
        if self.allow { Ok(()) } else { Err(ToolError::Rejected) }
    }
}

fn context(fixture: &Fixture, asker: Arc<Asker>) -> ToolContext {
    ToolContext {
        session_id: "ses_1".into(),
        message_id: "msg_1".into(),
        call_id: "call_1".into(),
        agent: "build".into(),
        cwd: fixture.project(),
        worktree: fixture.project(),
        data_dir: fixture.root().join("data"),
        abort: CancellationToken::new(),
        permissions: asker,
        hooks: None,
    }
}

fn asker(allow: bool) -> Arc<Asker> {
    Arc::new(Asker { allow, seen: Mutex::default() })
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let out = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
    }
    #[cfg(unix)]
    {
        std::process::Command::new("kill").args(["-0", &pid.to_string()]).status().is_ok_and(|s| s.success())
    }
}

async fn wait_dead(pid: u32) -> bool {
    for _ in 0..50 {
        if !pid_alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

const BEFORE_PLUGIN: &str = r#"
export default {
  id: "before",
  server: async (input) => ({
    "tool.execute.before": async (i, o) => {
      o.args.command = "echo patched"
      o.args.seen = `${i.tool}:${i.sessionID}:${i.callID}`
    },
    "shell.env": async (i, o) => {
      o.env.FROM_PLUGIN = i.cwd
    },
    "chat.params": async (_i, o) => {
      o.options.order = [...(o.options.order ?? []), "before"]
    },
  }),
}
"#;

const PARAMS_PLUGIN: &str = r#"
import fs from "node:fs"
import path from "node:path"

export const ParamsPlugin = async ({ client, directory, worktree, project, serverUrl, $ }, options) => ({
  "chat.params": async (input, output) => {
    output.temperature = 0.1
    output.maxOutputTokens = 1234
    output.options.order = [...(output.options.order ?? []), "params"]
    output.options.ctx = {
      agent: input.agent,
      url: serverUrl.href,
      base: client.config.baseUrl,
      clientDirectory: client.config.directory,
      authorization: client.config.headers?.Authorization ?? null,
      shell: typeof $,
      project: project.id,
      directory,
      worktree,
      options: options ?? null,
    }
  },
  event: async ({ event }) => {
    fs.writeFileSync(path.join(directory, "event.json"), JSON.stringify(event))
  },
  config: async (cfg) => {
    cfg.injected = { by: "params" }
  },
  tool: {
    legacy: {
      description: "legacy schema tool",
      args: { text: { type: "string", description: "text" } },
      async execute(args) {
        console.log("stdout-noise from legacy tool")
        return `legacy:${args.text}`
      },
    },
  },
})
"#;

const TOOL_FILE: &str = r#"
export const ask = {
  description: "asks for permission",
  args: {},
  async execute(_args, ctx) {
    await ctx.ask({ permission: "greet", patterns: ["a/*"], always: ["*"], metadata: { k: 1 } })
    return "asked"
  },
}
export const slow = {
  description: "waits for abort",
  args: {},
  async execute(_args, ctx) {
    await new Promise((resolve, reject) => {
      const timer = setTimeout(resolve, 60000)
      ctx.abort.addEventListener("abort", () => {
        clearTimeout(timer)
        reject(new Error("aborted by signal"))
      })
    })
    return "done"
  },
}
export const notATool = { description: "missing fields" }
"#;

fn zod_tool(zod: &str) -> String {
    format!(
        r#"
import {{ z }} from "{zod}"
export default {{
  description: "Greets someone",
  args: {{ name: z.string().describe("who to greet"), times: z.number().int().optional() }},
  async execute(args, ctx) {{
    return {{
      title: "greeting",
      output: `hello ${{args.name}} x${{args.times ?? 1}}`,
      metadata: {{ agent: ctx.agent, session: ctx.sessionID, message: ctx.messageID, call: ctx.callID, dir: ctx.directory }},
      attachments: [{{ type: "file", mime: "text/plain", url: "data:text/plain;base64,aGk=" }}],
    }}
  }},
}}
"#
    )
}

#[tokio::test]
async fn inert_without_plugins() {
    let host = PluginHost::start(PluginOptions {
        bun_path: Some(PathBuf::from("definitely-not-bun-binary")),
        ..PluginOptions::default()
    })
    .await
    .unwrap();
    assert!(!host.is_active());
    assert_eq!(host.pid(), None);
    assert!(host.tools().is_empty());
    let out = host.trigger(HookName::ToolExecuteBefore, json!({}), json!({"args": {"a": 1}})).await.unwrap();
    assert_eq!(out, json!({"args": {"a": 1}}));
    assert_eq!(host.config(json!({"x": 1})).await.unwrap(), json!({"x": 1}));
    host.event(&Event::default()).unwrap();
    host.shutdown().await;
}

#[tokio::test]
async fn missing_bun_is_a_clear_error() {
    let fixture = Fixture::new();
    fixture.write("proj/.opencode/plugin/a.ts", BEFORE_PLUGIN);
    let result = PluginHost::start(PluginOptions {
        bun_path: Some(PathBuf::from("definitely-not-bun-binary")),
        ..fixture.options()
    })
    .await;
    let error = result.err().expect("must fail");
    assert!(matches!(error, PluginError::BunNotFound { .. }));
    assert!(error.to_string().starts_with("plugins need Bun: install Bun from https://bun.sh"), "{error}");
    assert!(!fixture.cache().join("plugin-host").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn hooks_tools_and_shutdown() {
    if bun().is_none() {
        return;
    }
    let fixture = Fixture::new();
    fixture.write("proj/.opencode/plugin/a_before.ts", BEFORE_PLUGIN);
    fixture.write("proj/.opencode/plugins/b_params.js", PARAMS_PLUGIN);
    fixture.write("proj/.opencode/tool/perm.ts", TOOL_FILE);
    let zod = zod_url();
    match &zod {
        Some(url) => {
            fixture.write("proj/.opencode/tools/greet.ts", &zod_tool(url));
        }
        None => eprintln!("SKIPPED (zod assertions): no zod@4 in the local Bun install cache"),
    }

    let mut options = fixture.options();
    options.server_headers.insert("Authorization".into(), opencode_plugin::basic_auth_header(None, "pw"));
    let host = PluginHost::start(options).await.unwrap();
    let info = host.info().clone();
    eprintln!("bun {}", info.bun);
    assert!(info.errors.is_empty(), "{:?}", info.errors);
    assert_eq!(info.plugins.len(), 2);
    assert_eq!(info.plugins[0].id.as_deref(), Some("before"));
    assert_eq!(info.plugins[1].id, None);
    assert_eq!(info.plugins[1].tools, ["legacy"]);
    assert!(info.sdk.as_deref().is_some_and(|sdk| sdk.ends_with("sdk-stub.mjs")));
    for hook in ["tool.execute.before", "shell.env", "chat.params", "event", "config"] {
        assert!(host.has_hook(hook), "{hook}");
    }
    assert!(!host.has_hook("chat.headers"));
    assert!(fixture.cache().join("plugin-host").join("host.mjs").is_file());

    // tool.execute.before mutates args in place; untouched keys survive.
    let before = host
        .trigger_typed::<ToolExecuteBefore>(
            &ToolExecuteBeforeInput { tool: "bash".into(), session_id: "ses_1".into(), call_id: "call_9".into() },
            ToolExecuteBeforeOutput { args: json!({"command": "rm -rf /", "timeout": 5}) },
        )
        .await
        .unwrap();
    assert_eq!(before.args, json!({"command": "echo patched", "timeout": 5, "seen": "bash:ses_1:call_9"}));

    // chat.params: both plugins run in load order on the same output object.
    let params = host
        .trigger_typed::<ChatParams>(
            &ChatInput { session_id: "ses_1".into(), agent: "build".into(), ..ChatInput::default() },
            ChatParamsOutput { temperature: Some(0.7), top_p: Some(0.9), ..ChatParamsOutput::default() },
        )
        .await
        .unwrap();
    assert_eq!(params.temperature, Some(0.1));
    assert_eq!(params.top_p, Some(0.9));
    assert_eq!(params.max_output_tokens, Some(1234));
    assert_eq!(params.options["order"], json!(["before", "params"]));
    let ctx = &params.options["ctx"];
    assert_eq!(ctx["agent"], "build");
    assert_eq!(ctx["url"], "http://127.0.0.1:1/");
    assert_eq!(ctx["base"], "http://127.0.0.1:1");
    assert_eq!(ctx["authorization"], "Basic b3BlbmNvZGU6cHc=");
    assert_eq!(ctx["shell"], "function");
    assert_eq!(ctx["project"], "proj_1");
    assert_eq!(ctx["options"], Value::Null);
    assert_eq!(PathBuf::from(ctx["directory"].as_str().unwrap()), fixture.project());
    assert_eq!(ctx["clientDirectory"], ctx["directory"]);

    let env = host
        .trigger_typed::<ShellEnv>(
            &ShellEnvInput { cwd: "/work".into(), ..ShellEnvInput::default() },
            ShellEnvOutput::default(),
        )
        .await
        .unwrap();
    assert_eq!(env.env["FROM_PLUGIN"], "/work");

    // A hook nobody declares returns the output untouched.
    let untouched = host.trigger(HookName::ChatHeaders, json!({}), json!({"headers": {"a": "b"}})).await.unwrap();
    assert_eq!(untouched, json!({"headers": {"a": "b"}}));

    let config = host.config(json!({"model": "x"})).await.unwrap();
    assert_eq!(config, json!({"model": "x", "injected": {"by": "params"}}));

    host.event(&Event { id: "evt_1".into(), kind: "session.idle".into(), properties: json!({"sessionID": "ses_1"}) })
        .unwrap();
    let event_file = fixture.project().join("event.json");
    for _ in 0..50 {
        if event_file.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let event: Value = serde_json::from_str(&std::fs::read_to_string(&event_file).unwrap()).unwrap();
    assert_eq!(event, json!({"id": "evt_1", "type": "session.idle", "properties": {"sessionID": "ses_1"}}));

    // Tool ids: `<file>` for the default export, `<file>_<export>` otherwise; plugin `tool` ids verbatim.
    let tools = host.tools();
    let ids: Vec<&str> = tools.iter().map(|tool| tool.id()).collect();
    let expected: Vec<&str> = match zod {
        Some(_) => vec!["perm_ask", "perm_slow", "greet", "legacy"],
        None => vec!["perm_ask", "perm_slow", "legacy"],
    };
    assert_eq!(ids, expected);
    let find = |id: &str| tools.iter().find(|tool| tool.id() == id).unwrap().clone();

    let legacy = find("legacy");
    assert_eq!(
        legacy.parameters(),
        json!({"type": "object", "properties": {"text": {"type": "string", "description": "text"}}, "required": ["text"]})
    );
    let out = legacy.execute(json!({"text": "hi"}), &context(&fixture, asker(true))).await.unwrap();
    assert_eq!(out.output, "legacy:hi");
    assert_eq!(out.title, "");
    assert_eq!(out.metadata, json!({}));

    if zod.is_some() {
        let greet = find("greet");
        assert_eq!(greet.description(), "Greets someone");
        let schema = greet.parameters();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["name"], json!({"type": "string", "description": "who to greet"}));
        assert_eq!(schema["properties"]["times"]["type"], "integer");
        assert_eq!(schema["required"], json!(["name"]));
        assert_eq!(info.tools.iter().find(|t| t.id == "greet").unwrap().converter, "zod");

        let out = greet.execute(json!({"name": "ada", "times": 2}), &context(&fixture, asker(true))).await.unwrap();
        assert_eq!(out.title, "greeting");
        assert_eq!(out.output, "hello ada x2");
        assert_eq!(out.metadata["agent"], "build");
        assert_eq!(out.metadata["session"], "ses_1");
        assert_eq!(out.metadata["message"], "msg_1");
        assert_eq!(out.metadata["call"], "call_1");
        assert_eq!(PathBuf::from(out.metadata["dir"].as_str().unwrap()), fixture.project());
        assert_eq!(out.attachments.len(), 1);
        assert_eq!(out.attachments[0].mime, "text/plain");

        let invalid = greet.execute(json!({"name": 5}), &context(&fixture, asker(true))).await.unwrap_err();
        assert!(matches!(&invalid, ToolError::InvalidArguments { tool, .. } if tool == "greet"), "{invalid:?}");
    }

    // Permission asks are forwarded to ToolContext.permissions.
    let allow = asker(true);
    let out = find("perm_ask").execute(json!({}), &context(&fixture, allow.clone())).await.unwrap();
    assert_eq!(out.output, "asked");
    assert_eq!(
        allow.seen.lock().unwrap().as_slice(),
        [PermissionRequest {
            permission: "greet".into(),
            patterns: vec!["a/*".into()],
            always: vec!["*".into()],
            metadata: json!({"k": 1}),
        }]
    );
    let deny = asker(false);
    let rejected = find("perm_ask").execute(json!({}), &context(&fixture, deny.clone())).await.unwrap_err();
    assert!(matches!(rejected, ToolError::Rejected), "{rejected:?}");
    assert_eq!(deny.seen.lock().unwrap().len(), 1);

    // Abort reaches the plugin's AbortSignal and the call returns promptly.
    let ctx = context(&fixture, asker(true));
    let abort = ctx.abort.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        abort.cancel();
    });
    let started = std::time::Instant::now();
    let aborted = find("perm_slow").execute(json!({}), &ctx).await.unwrap_err();
    assert!(matches!(aborted, ToolError::Aborted), "{aborted:?}");
    assert!(started.elapsed() < Duration::from_secs(10));

    // Plugin stdout noise does not break the protocol and lands in the log.
    assert!(fixture.logs().contains("stdout-noise from legacy tool"), "{}", fixture.logs());

    let pid = host.pid().unwrap();
    assert!(pid_alive(pid));
    host.shutdown().await;
    assert!(wait_dead(pid).await, "bun {pid} still running after shutdown");
    let after = host.trigger(HookName::ShellEnv, json!({}), json!({"env": {}})).await.unwrap_err();
    assert!(matches!(after, PluginError::Closed), "{after:?}");
    let after = legacy.execute(json!({"text": "x"}), &context(&fixture, asker(true))).await.unwrap_err();
    assert!(after.to_string().contains("shut down"), "{after}");
    host.shutdown().await;
}

const SLOW_PLUGIN: &str = r#"
export default {
  id: "slow",
  server: async () => ({
    "experimental.text.complete": async (_i, o) => {
      await new Promise((resolve) => setTimeout(resolve, 30000))
      o.text = "late"
    },
    "experimental.session.compacting": async (_i, o) => {
      o.context.push("extra")
    },
    "command.execute.before": async () => {
      throw new Error("hook exploded")
    },
  }),
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn request_timeout_and_hook_errors() {
    if bun().is_none() {
        return;
    }
    let fixture = Fixture::new();
    fixture.write("proj/.opencode/plugin/slow.ts", SLOW_PLUGIN);
    let host = PluginHost::start(PluginOptions { request_timeout: Duration::from_millis(400), ..fixture.options() })
        .await
        .unwrap();

    let started = std::time::Instant::now();
    let error = host
        .trigger(HookName::ExperimentalTextComplete, json!({"sessionID": "s"}), json!({"text": "t"}))
        .await
        .unwrap_err();
    assert!(matches!(&error, PluginError::Timeout { method, .. } if method == "hook.trigger"), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(5));

    let out = host
        .trigger(HookName::ExperimentalSessionCompacting, json!({"sessionID": "s"}), json!({"context": ["a"]}))
        .await
        .unwrap();
    assert_eq!(out, json!({"context": ["a", "extra"]}));

    let error = host.trigger(HookName::CommandExecuteBefore, json!({}), json!({"parts": []})).await.unwrap_err();
    assert!(matches!(&error, PluginError::Remote { message, .. } if message == "hook exploded"), "{error:?}");
    host.shutdown().await;
}

const CRASH_PLUGIN: &str = r#"
let child
export default {
  id: "crash",
  server: async () => {
    child = Bun.spawn([process.execPath, "-e", "setInterval(() => {}, 1000)"], { stdout: "ignore", stderr: "ignore" })
    return {
      "experimental.text.complete": async (_i, o) => {
        o.text = String(child.pid)
      },
      "chat.headers": async () => {
        console.error("boom-marker before exit")
        process.exit(7)
      },
    }
  },
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn crash_is_detected() {
    if bun().is_none() {
        return;
    }
    let fixture = Fixture::new();
    fixture.write("proj/.opencode/plugin/crash.ts", CRASH_PLUGIN);
    let host = PluginHost::start(fixture.options()).await.unwrap();
    let pid = host.pid().unwrap();
    let child = host
        .trigger(HookName::ExperimentalTextComplete, json!({}), json!({"text": ""}))
        .await
        .unwrap()["text"]
        .as_str()
        .unwrap()
        .parse::<u32>()
        .unwrap();

    let error = host.trigger(HookName::ChatHeaders, json!({}), json!({"headers": {}})).await.unwrap_err();
    let PluginError::Crashed(message) = &error else {
        panic!("expected crash, got {error:?}");
    };
    assert!(message.contains('7'), "{message}");
    assert!(message.contains("boom-marker before exit"), "{message}");
    assert!(fixture.logs().contains("boom-marker before exit"));
    assert!(error.to_string().starts_with("plugin host crashed: "));

    let again = host.trigger(HookName::ChatHeaders, json!({}), json!({"headers": {}})).await.unwrap_err();
    assert!(matches!(again, PluginError::Crashed(_)), "{again:?}");
    let undeclared = host.trigger(HookName::ShellEnv, json!({}), json!({"env": {}})).await.unwrap_err();
    assert!(matches!(undeclared, PluginError::Crashed(_)), "{undeclared:?}");
    assert!(wait_dead(pid).await);
    host.shutdown().await;
    opencode_plugin_test_kill(child);
}

fn opencode_plugin_test_kill(pid: u32) {
    #[cfg(windows)]
    let _ = std::process::Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).output();
    #[cfg(unix)]
    let _ = std::process::Command::new("kill").args(["-9", &pid.to_string()]).output();
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_and_drop_kill_the_process_tree() {
    if bun().is_none() {
        return;
    }
    for explicit in [true, false] {
        let fixture = Fixture::new();
        fixture.write("proj/.opencode/plugin/crash.ts", CRASH_PLUGIN);
        let host = PluginHost::start(fixture.options()).await.unwrap();
        let pid = host.pid().unwrap();
        let child = host
            .trigger(HookName::ExperimentalTextComplete, json!({}), json!({"text": ""}))
            .await
            .unwrap()["text"]
            .as_str()
            .unwrap()
            .parse::<u32>()
            .unwrap();
        assert!(pid_alive(pid) && pid_alive(child));
        if explicit {
            host.shutdown().await;
        }
        drop(host);
        assert!(wait_dead(pid).await, "bun {pid} survived (explicit shutdown: {explicit})");
        assert!(wait_dead(child).await, "grandchild {child} survived (explicit shutdown: {explicit})");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn loader_rules_and_errors() {
    if bun().is_none() {
        return;
    }
    let fixture = Fixture::new();
    fixture.write("proj/.opencode/plugin/a_noid.ts", "export default { server: async () => ({}) }\n");
    fixture.write("proj/.opencode/plugin/b_notfn.ts", "export const value = 42\n");
    fixture.write("proj/.opencode/plugin/c_throws.ts", "throw new Error('import failed')\n");
    fixture.write(
        "proj/.opencode/plugin/d_two.ts",
        r#"
const one = async () => ({ "chat.headers": async (_i, o) => { o.headers.one = "1" } })
export const A = one
export const B = one
export const C = { server: async () => ({ "chat.headers": async (_i, o) => { o.headers.two = "2" } }) }
"#,
    );

    let cache = fixture.cache();
    let pkg = |name: &str, manifest: Value, body: &str| {
        let dir = npm_target(name, &cache).unwrap().module_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("package.json"), manifest.to_string()).unwrap();
        std::fs::write(dir.join("server.js"), body).unwrap();
    };
    pkg(
        "opencode-fake",
        json!({"name": "opencode-fake", "exports": {"./server": {"import": "./server.js"}}, "engines": {"opencode": ">=1.0.0"}}),
        r#"export default { server: async (_input, options) => ({ "chat.headers": async (_i, o) => { o.headers.fake = String(options?.flag) } }) }"#,
    );
    pkg(
        "@scope/old@0.1.0",
        json!({"name": "@scope/old", "main": "server.js", "engines": {"opencode": "<1.0.0"}}),
        "export default { server: async () => ({}) }",
    );
    pkg("opencode-escape", json!({"name": "opencode-escape", "main": "../../../outside.js"}), "");

    let mut options = fixture.options();
    options.opencode_version = Some("1.19.5".into());
    let mut specs = vec![
        PluginSpec { spec: "opencode-fake".into(), options: json!({"flag": true}).as_object().cloned() },
        PluginSpec::new("@scope/old@0.1.0"),
        PluginSpec::new("opencode-escape"),
        PluginSpec::new("opencode-not-installed@2.0.0"),
        PluginSpec::new("opencode-copilot-auth@0.0.1"),
    ];
    specs.append(&mut options.specs);
    options.specs = specs;

    let host = PluginHost::start(options).await.unwrap();
    let info = host.info();
    let loaded: Vec<(&str, Option<&str>)> = info.plugins.iter().map(|p| (p.spec.as_str(), p.id.as_deref())).collect();
    assert_eq!(loaded.len(), 3, "{loaded:?}");
    assert_eq!(loaded[0], ("opencode-fake", Some("opencode-fake")));
    assert!(loaded[1].0.ends_with("d_two.ts") && loaded[2].0.ends_with("d_two.ts"));

    let error = |needle: &str| {
        info.errors
            .iter()
            .find(|e| e.spec.contains(needle))
            .unwrap_or_else(|| panic!("no error for {needle}: {:?}", info.errors))
    };
    assert_eq!(error("a_noid").stage, "apply");
    assert!(error("a_noid").message.contains("must export id"));
    assert_eq!(error("b_notfn").message, "Plugin export is not a function");
    assert_eq!(error("c_throws").stage, "load");
    assert!(error("c_throws").session_message().starts_with("Failed to load plugin file://"));
    assert_eq!(error("@scope/old").stage, "compatibility");
    assert_eq!(
        error("@scope/old").session_message(),
        "Plugin @scope/old@0.1.0 skipped: Plugin requires opencode <1.0.0 but running 1.19.5"
    );
    assert_eq!(error("opencode-escape").stage, "entry");
    assert!(error("opencode-escape").message.contains("outside plugin directory"));
    assert_eq!(error("opencode-not-installed").stage, "install");
    assert!(
        error("opencode-not-installed")
            .session_message()
            .starts_with("Failed to install plugin opencode-not-installed@2.0.0: ")
    );
    assert_eq!(info.errors.len(), 6, "{:?}", info.errors);
    assert!(!info.errors.iter().any(|e| e.spec.contains("copilot")));

    let out = host.trigger(HookName::ChatHeaders, json!({}), json!({"headers": {}})).await.unwrap();
    assert_eq!(out, json!({"headers": {"fake": "true", "one": "1", "two": "2"}}));
    host.shutdown().await;
}

const AUTH_PLUGIN: &str = r#"
export default {
  id: "auth",
  server: async () => ({
    auth: {
      provider: "acme",
      loader: async (getAuth, provider) => {
        const auth = await getAuth()
        return {
          apiKey: `key-${auth.key}`,
          providerName: provider.name,
          fetch: async (url, init) => {
            const body = init.body ? await new Response(init.body).text() : ""
            const headers = new Headers(init.headers)
            const stream = new ReadableStream({
              async start(controller) {
                for (const part of ["hello ", "streamed ", "world"]) {
                  controller.enqueue(new TextEncoder().encode(part))
                  await new Promise((resolve) => setTimeout(resolve, 20))
                }
                controller.close()
              },
            })
            return new Response(stream, {
              status: 201,
              statusText: "Created",
              headers: { "x-echo": `${init.method} ${url} ${headers.get("x-in")} ${body}` },
            })
          },
        }
      },
      methods: [
        {
          type: "oauth",
          label: "Login",
          prompts: [{ type: "text", key: "org", message: "Org?", validate: (v) => (v ? undefined : "required") }],
          authorize: async (inputs) => ({
            url: `https://example.invalid/${inputs.org}`,
            instructions: "paste code",
            method: "code",
            callback: async (code) => ({ type: "success", key: `token-${code}` }),
          }),
        },
        { type: "api", label: "API key" },
      ],
    },
    provider: { id: "acme", models: async (provider, ctx) => ({ m1: { id: "m1", from: provider.id, auth: ctx.auth?.type ?? null } }) },
  }),
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn auth_passthrough_and_fetch_proxy() {
    if bun().is_none() {
        return;
    }
    let fixture = Fixture::new();
    fixture.write("proj/.opencode/plugin/auth.ts", AUTH_PLUGIN);
    let mut options = fixture.options();
    options.auth = Some(Arc::new(|provider: &str| Some(json!({"type": "api", "key": format!("stored-{provider}")}))));
    let host = PluginHost::start(options).await.unwrap();
    assert_eq!(host.info().plugins[0].auth.as_deref(), Some("acme"));

    assert_eq!(
        host.auth_list().await.unwrap(),
        json!([{
            "plugin": 0,
            "provider": "acme",
            "loader": true,
            "methods": [
                {"type": "oauth", "label": "Login", "authorize": true, "prompts": [
                    {"type": "text", "key": "org", "message": "Org?", "validate": true, "condition": false}
                ]},
                {"type": "api", "label": "API key", "authorize": false},
            ],
        }])
    );

    let invalid = host.auth_authorize("acme", 0, Some(json!({"org": ""}))).await.unwrap_err();
    assert!(matches!(&invalid, PluginError::Remote { message, .. } if message == "required"), "{invalid:?}");
    let started = host.auth_authorize("acme", 0, Some(json!({"org": "o1"}))).await.unwrap();
    assert_eq!(started["url"], "https://example.invalid/o1");
    assert_eq!(started["method"], "code");
    assert!(started.get("callback").is_none());
    let handle = started["handle"].as_str().unwrap();
    assert_eq!(host.auth_callback(handle, Some("abc")).await.unwrap(), json!({"type": "success", "key": "token-abc"}));
    assert!(host.auth_callback(handle, Some("abc")).await.is_err());
    assert_eq!(host.auth_authorize("acme", 1, None).await.unwrap(), Value::Null);

    let loaded = host.auth_loader("acme", json!({"type": "api", "key": "snapshot"}), json!({"name": "Acme"})).await.unwrap();
    assert_eq!(Value::Object(loaded.options.clone()), json!({"apiKey": "key-stored-acme", "providerName": "Acme"}));
    assert_eq!(loaded.functions.keys().collect::<Vec<_>>(), ["fetch"]);

    let mut response = host
        .fetch(
            &loaded.functions["fetch"],
            FetchRequest {
                url: "https://api.example.invalid/v1/chat".into(),
                method: "POST".into(),
                headers: vec![("x-in".into(), "yes".into())],
                body: Some(b"{\"q\":1}".to_vec()),
            },
        )
        .await
        .unwrap();
    assert_eq!(response.status, 201);
    assert_eq!(response.status_text, "Created");
    let echo = response.headers.iter().find(|(k, _)| k == "x-echo").map(|(_, v)| v.as_str());
    assert_eq!(echo, Some("POST https://api.example.invalid/v1/chat yes {\"q\":1}"));
    let mut chunks = Vec::new();
    while let Some(chunk) = response.body.recv().await {
        chunks.push(String::from_utf8(chunk.unwrap()).unwrap());
    }
    assert_eq!(chunks.concat(), "hello streamed world");
    assert!(chunks.len() > 1, "body should arrive in several chunks: {chunks:?}");

    let unknown = host.fetch("fn-missing", FetchRequest { url: "http://x".into(), method: "GET".into(), ..FetchRequest::default() }).await;
    assert!(unknown.is_err());

    assert_eq!(host.provider_list().await.unwrap(), json!([{"id": "acme", "models": true}]));
    let models = host.provider_models("acme", json!({"id": "acme"}), Some(json!({"type": "api"}))).await.unwrap();
    assert_eq!(models, json!({"m1": {"id": "m1", "from": "acme", "auth": "api"}}));
    host.shutdown().await;
}
