// Per-directory workspace services (snapshots, formatters, language servers) and what the loop does with them:
// tool hooks, session diff summaries (`session/summary.ts`) and file revert (`session/revert.ts`).
use crate::{Ctx, Engine, Error, Host, InstanceEvent, Update, plain_directory};
use opencode_lsp::lsp::diagnostic;
use opencode_lsp::{DiagnosticsMode, Location, Patch, Workspace, WorkspaceOptions};
use opencode_storage::{
    FileDiff, Message, Part, PartBody, PatchPart, Session, SessionRevert, SessionSummary, Store, UserSummary, id, identity,
};
use opencode_tools::{FileDiagnostics, WorkspaceHooks};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

type Listener = Arc<dyn Fn(&InstanceEvent) + Send + Sync>;

// Engine-wide fan-out of instance events; cheap to hand to background tasks.
#[derive(Default)]
pub(crate) struct Events {
    listeners: Mutex<Vec<Listener>>,
}

impl Events {
    pub(crate) fn watch(&self, listener: Listener) {
        self.listeners.lock().unwrap_or_else(|e| e.into_inner()).push(listener);
    }

    pub(crate) fn emit(&self, directory: &str, kind: &str, properties: Value) {
        let listeners: Vec<Listener> = self.listeners.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if listeners.is_empty() {
            return;
        }
        let event = InstanceEvent { directory: directory.to_string(), kind: kind.to_string(), properties };
        for listener in listeners {
            listener(&event);
        }
    }

    // To the engine listeners and, inside a turn, to the turn's host.
    pub(crate) fn publish(&self, host: Option<&Arc<dyn Host>>, directory: &str, kind: &str, properties: Value) {
        if let Some(host) = host {
            host.update(Update::Event { directory: directory.to_string(), kind: kind.to_string(), properties: properties.clone() });
        }
        self.emit(directory, kind, properties);
    }
}

// What the file tools see of the instance.
pub(crate) struct ToolHooks {
    pub(crate) workspace: Arc<Workspace>,
    pub(crate) events: Arc<Events>,
    pub(crate) host: Option<Arc<dyn Host>>,
    pub(crate) directory: String,
    pub(crate) plugins: Option<Arc<crate::plugins::Plugins>>,
    // Rust build: the engine, to tell a session that its background shell ended.
    pub(crate) engine: Option<std::sync::Weak<Engine>>,
}

#[async_trait::async_trait]
impl WorkspaceHooks for ToolHooks {
    fn background_done(&self, finished: opencode_tools::bash::background::Finished) {
        if let Some(engine) = self.engine.as_ref().and_then(std::sync::Weak::upgrade) {
            engine.background_finished(PathBuf::from(&self.directory), finished, self.host.clone());
        }
    }

    // The plugin `shell.env` hook of the bash tool.
    async fn shell_env(&self, cwd: &Path, session_id: &str, call_id: &str) -> Result<Vec<(String, String)>, String> {
        let name = opencode_plugin::HookName::ShellEnv;
        let Some(plugins) = self.plugins.as_ref().filter(|p| p.has(name)) else { return Ok(Vec::new()) };
        let input = json!({"cwd": cwd.to_string_lossy(), "sessionID": session_id, "callID": call_id});
        let output = plugins.host.trigger(name, input, json!({"env": {}})).await.map_err(|e| format!("plugin hook {name} failed: {e}"))?;
        Ok(crate::plugins::env_pairs(&output))
    }

    async fn format(&self, file: &Path) -> bool {
        self.workspace.format(file).await.ran
    }

    fn publish(&self, kind: &str, properties: Value) {
        self.events.publish(self.host.as_ref(), &self.directory, kind, properties);
    }

    async fn touch(&self, file: &Path, wait: bool) {
        if self.workspace.lsp().is_empty() {
            return;
        }
        if wait {
            self.workspace.lsp().touch_file(file, Some(DiagnosticsMode::Document)).await;
            return;
        }
        let (workspace, file) = (self.workspace.clone(), file.to_path_buf());
        tokio::spawn(async move { workspace.warm(&file).await });
    }

    fn diagnostics(&self) -> Vec<FileDiagnostics> {
        if self.workspace.lsp().is_empty() {
            return Vec::new();
        }
        self.workspace
            .lsp()
            .diagnostics()
            .into_iter()
            .map(|(file, issues)| FileDiagnostics {
                file,
                errors: issues.iter().filter(|issue| issue.severity == Some(1)).map(diagnostic::pretty).collect(),
                issues: serde_json::to_value(&issues).unwrap_or_default(),
            })
            .collect()
    }

    fn normalize(&self, file: &Path) -> String {
        self.workspace.lsp().diagnostics_key(file)
    }

    async fn lsp_available(&self, file: &Path) -> bool {
        self.workspace.lsp().has_clients(file)
    }

    async fn lsp_request(&self, operation: &str, file: &Path, line: u32, character: u32, query: &str) -> Vec<Value> {
        let lsp = self.workspace.lsp();
        let at = Location { file: file.to_path_buf(), line, character };
        match operation {
            "goToDefinition" => lsp.definition(&at).await,
            "findReferences" => lsp.references(&at).await,
            "hover" => lsp.hover(&at).await,
            "documentSymbol" => lsp.document_symbol(&crate::path_to_file_url(file)).await,
            "workspaceSymbol" => lsp.workspace_symbol(query).await,
            "goToImplementation" => lsp.implementation(&at).await,
            "prepareCallHierarchy" => lsp.prepare_call_hierarchy(&at).await,
            "incomingCalls" => lsp.incoming_calls(&at).await,
            "outgoingCalls" => lsp.outgoing_calls(&at).await,
            _ => Vec::new(),
        }
    }
}

// `SessionSummary.computeDiff`: first step-start snapshot to last step-finish snapshot.
async fn compute_diff(workspace: &Workspace, messages: &[&(Message, Vec<Part>)]) -> Vec<FileDiff> {
    let mut from: Option<&str> = None;
    let mut to: Option<&str> = None;
    for (_, parts) in messages {
        for part in parts {
            match &part.body {
                PartBody::StepStart(step) if from.is_none() => from = step.snapshot.as_deref().filter(|s| !s.is_empty()),
                PartBody::StepFinish(step) => to = step.snapshot.as_deref().filter(|s| !s.is_empty()).or(to),
                _ => {}
            }
        }
    }
    match (from, to) {
        (Some(from), Some(to)) => workspace.snapshot().diff_full(from, to).await,
        _ => Vec::new(),
    }
}

// What a background `SessionSummary.summarize` needs.
pub(crate) struct Summarize {
    pub(crate) store: Store,
    pub(crate) workspace: Arc<Workspace>,
    pub(crate) events: Arc<Events>,
    pub(crate) host: Arc<dyn Host>,
    pub(crate) directory: String,
    pub(crate) session_id: String,
    pub(crate) message_id: String,
    pub(crate) order: Arc<tokio::sync::Mutex<()>>,
}

impl Summarize {
    pub(crate) async fn run(self) {
        let _turn = self.order.lock().await;
        let zero = SessionSummary { additions: 0.0, deletions: 0.0, files: 0.0, diffs: None };
        let patched = self.store.patch_session(&self.session_id, |s| {
            s.time.updated = id::now_ms();
            s.summary = Some(zero);
        });
        if patched.is_err() {
            return;
        }
        self.events.publish(Some(&self.host), &self.directory, "session.diff", json!({"sessionID": self.session_id, "diff": []}));
        if self.workspace.options().config.get("snapshot") == Some(&Value::Bool(false)) {
            return;
        }
        let Ok(all) = self.store.list_messages(&self.session_id) else { return };
        let messages: Vec<&(Message, Vec<Part>)> = all
            .iter()
            .filter(|(m, _)| m.id() == self.message_id || m.as_assistant().is_some_and(|a| a.parent_id == self.message_id))
            .collect();
        let Some(mut target) = messages.iter().find(|(m, _)| m.id() == self.message_id).and_then(|(m, _)| m.as_user().cloned()) else { return };
        let diffs = compute_diff(&self.workspace, &messages).await;
        let previous = target.summary.take();
        target.summary = Some(UserSummary {
            title: previous.as_ref().and_then(|s| s.title.clone()),
            body: previous.and_then(|s| s.body),
            diffs,
        });
        let _ = self.store.upsert_message(&Message::User(target));
    }
}

impl Engine {
    // The directory's services, built on first use and kept until `dispose`.
    pub(crate) fn workspace_for(&self, directory: &Path, worktree: Option<&Path>, config: &Value) -> Arc<Workspace> {
        let mut workspaces = self.workspaces.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(workspace) = workspaces.get(directory) {
            return workspace.clone();
        }
        let options = WorkspaceOptions {
            data_dir: self.paths.data.clone(),
            cache_dir: Some(self.paths.cache.clone()),
            project_id: identity::resolve(directory).id,
            directory: directory.to_path_buf(),
            worktree: worktree.map_or_else(|| PathBuf::from("/"), Path::to_path_buf),
            git: worktree.is_some(),
            config: config.clone(),
            ..Default::default()
        }
        .with_env_flags();
        let workspace = Arc::new(Workspace::new(options));
        // `lsp.updated` for every client that connects.
        if !workspace.lsp().is_empty()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let mut updates = workspace.lsp().subscribe_updates();
            let (events, weak) = (self.events.clone(), Arc::downgrade(&workspace));
            let name = directory.to_string_lossy().into_owned();
            runtime.spawn(async move {
                loop {
                    match updates.recv().await {
                        Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    }
                    if weak.strong_count() == 0 {
                        return;
                    }
                    events.emit(&name, "lsp.updated", json!({}));
                }
            });
        }
        workspaces.insert(directory.to_path_buf(), workspace.clone());
        workspace
    }

    // Snapshots, formatters and language servers of an instance directory.
    pub fn workspace(&self, directory: &Path) -> Result<Arc<Workspace>, Error> {
        Ok(self.context(&plain_directory(directory.to_path_buf()))?.workspace.clone())
    }

    pub(crate) fn tool_hooks(&self, ctx: &Ctx, host: Option<&Arc<dyn Host>>) -> Arc<dyn WorkspaceHooks> {
        Arc::new(ToolHooks {
            workspace: ctx.workspace.clone(),
            events: self.events.clone(),
            host: host.cloned(),
            directory: ctx.cwd.to_string_lossy().into_owned(),
            plugins: ctx.plugins.clone(),
            engine: self.this.get().cloned(),
        })
    }

    // Forked like the original's `summary.summarize(...)`; `settle` waits for it.
    pub(crate) fn spawn_summarize(&self, ctx: &Ctx, session_id: &str, message_id: &str, host: &Arc<dyn Host>) {
        let job = Summarize {
            store: self.store.clone(),
            workspace: ctx.workspace.clone(),
            events: self.events.clone(),
            host: host.clone(),
            directory: ctx.cwd.to_string_lossy().into_owned(),
            session_id: session_id.to_string(),
            message_id: message_id.to_string(),
            order: self.summaries.clone(),
        };
        let handle = tokio::spawn(job.run());
        self.background.lock().unwrap_or_else(|e| e.into_inner()).push(handle);
    }

    // `SessionRevert.revert`: restores files from the patches after the target, then records the revert.
    pub async fn revert_files(&self, session_id: &str, message_id: &str, part_id: Option<&str>) -> Result<Session, Error> {
        self.assert_idle(session_id)?;
        let all = self.store.list_messages(session_id)?;
        let session = self.session(session_id)?;
        let mut last_user: Option<String> = None;
        let mut revert: Option<SessionRevert> = None;
        let mut patches: Vec<Patch> = Vec::new();
        for (message, parts) in &all {
            if matches!(message, Message::User(_)) {
                last_user = Some(message.id().to_string());
            }
            let mut content_before = false;
            for part in parts {
                if revert.is_some() {
                    if let PartBody::Patch(PatchPart { hash, files }) = &part.body {
                        patches.push(Patch { hash: hash.clone(), files: files.clone() });
                    }
                    continue;
                }
                if (message.id() == message_id && part_id.is_none()) || part_id == Some(part.id.as_str()) {
                    let kept = part_id.filter(|_| content_before).map(str::to_string);
                    let target = match (&kept, &last_user) {
                        (None, Some(user)) => user.clone(),
                        _ => message.id().to_string(),
                    };
                    revert = Some(SessionRevert { message_id: target, part_id: kept, snapshot: None, diff: None });
                }
                content_before |= matches!(part.body, PartBody::Text(_) | PartBody::Tool(_));
            }
        }
        let Some(mut revert) = revert else { return Ok(session) };

        let directory = plain_directory(PathBuf::from(&session.directory));
        let workspace = self.context(&directory)?.workspace.clone();
        let snapshot = workspace.snapshot();
        let previous = session.revert.as_ref().and_then(|r| r.snapshot.clone());
        revert.snapshot = match &previous {
            Some(existing) => Some(existing.clone()),
            None => snapshot.track().await,
        };
        if let Some(existing) = &previous {
            snapshot.restore(existing).await;
        }
        snapshot.revert(&patches).await;
        if let Some(hash) = &revert.snapshot {
            revert.diff = Some(snapshot.diff(hash).await);
        }
        let range: Vec<&(Message, Vec<Part>)> = match all.iter().position(|(m, _)| m.id() == revert.message_id) {
            Some(index) => all[index..].iter().collect(),
            None => Vec::new(),
        };
        let diffs = compute_diff(&workspace, &range).await;
        let file = self.paths.data.join("storage").join("session_diff").join(format!("{session_id}.json"));
        if let (Some(dir), Ok(text)) = (file.parent(), serde_json::to_string_pretty(&diffs)) {
            let _ = std::fs::create_dir_all(dir).and_then(|_| std::fs::write(&file, text));
        }
        self.events.emit(&session.directory, "session.diff", json!({"sessionID": session_id, "diff": diffs}));
        let summary = SessionSummary {
            additions: diffs.iter().map(|d| d.additions).sum(),
            deletions: diffs.iter().map(|d| d.deletions).sum(),
            files: diffs.len() as f64,
            diffs: None,
        };
        Ok(self.store.patch_session(session_id, |s| {
            s.summary = Some(summary);
            s.time.updated = id::now_ms();
            s.revert = Some(revert);
        })?)
    }

    // `SessionRevert.unrevert`: puts the files back as they were before the revert.
    pub async fn unrevert_files(&self, session_id: &str) -> Result<Session, Error> {
        self.assert_idle(session_id)?;
        let session = self.session(session_id)?;
        let Some(revert) = &session.revert else { return Ok(session) };
        if let Some(hash) = &revert.snapshot {
            let directory = plain_directory(PathBuf::from(&session.directory));
            self.context(&directory)?.workspace.snapshot().restore(hash).await;
        }
        Ok(self.store.patch_session(session_id, |s| {
            s.time.updated = id::now_ms();
            s.revert = None;
        })?)
    }

    // `SessionSummary.diff`: the stored diffs of one user message, git-quoted paths decoded.
    pub fn message_diff(&self, session_id: &str, message_id: Option<&str>) -> Result<Vec<FileDiff>, Error> {
        let Some(message_id) = message_id else { return Ok(Vec::new()) };
        let rows = self.store.list_messages(session_id)?;
        let Some(user) = rows.iter().find(|(m, _)| m.id() == message_id).and_then(|(m, _)| m.as_user()) else { return Ok(Vec::new()) };
        let diffs = user.summary.as_ref().map(|s| s.diffs.clone()).unwrap_or_default();
        Ok(diffs
            .into_iter()
            .map(|mut item| {
                item.file = item.file.map(|file| unquote_git_path(&file));
                item
            })
            .collect())
    }
}

// Runs a future to completion from synchronous code, inside or outside a runtime.
pub(crate) fn block_on<T: Send>(future: impl std::future::Future<Output = T> + Send) -> Result<T, Error> {
    let failed = |e: &dyn std::fmt::Display| Error::Config(format!("cannot run blocking task: {e}"));
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| failed(&e))?;
            Ok(runtime.block_on(future))
        });
        worker.join().unwrap_or_else(|_| Err(failed(&"worker panicked")))
    })
}

// `unquoteGitPath` (`session/summary.ts:10-64`).
pub(crate) fn unquote_git_path(input: &str) -> String {
    if !input.starts_with('"') || !input.ends_with('"') || input.len() < 2 {
        return input.to_string();
    }
    let body: Vec<u16> = input[1..input.len() - 1].encode_utf16().collect();
    let mut bytes: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < body.len() {
        let unit = body[i];
        if unit != u16::from(b'\\') {
            bytes.push(unit as u8);
            i += 1;
            continue;
        }
        let Some(&next) = body.get(i + 1) else {
            bytes.push(b'\\');
            i += 1;
            continue;
        };
        if (u16::from(b'0')..=u16::from(b'7')).contains(&next) {
            let digits: Vec<u8> = body[i + 1..].iter().take(3).take_while(|u| (u16::from(b'0')..=u16::from(b'7')).contains(u)).map(|u| *u as u8).collect();
            let value = digits.iter().fold(0u32, |acc, d| acc * 8 + u32::from(d - b'0'));
            bytes.push(value as u8);
            i += 1 + digits.len();
            continue;
        }
        let escaped = match char::from_u32(u32::from(next)) {
            Some('n') => b'\n',
            Some('r') => b'\r',
            Some('t') => b'\t',
            Some('b') => 0x08,
            Some('f') => 0x0c,
            Some('v') => 0x0b,
            _ => next as u8,
        };
        bytes.push(escaped);
        i += 2;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_paths_are_unquoted() {
        assert_eq!(unquote_git_path("plain.txt"), "plain.txt");
        assert_eq!(unquote_git_path("\"a\\tb.txt\""), "a\tb.txt");
        assert_eq!(unquote_git_path("\"caf\\303\\251.txt\""), "café.txt");
        assert_eq!(unquote_git_path("\"q\\\"uote\\\\.txt\""), "q\"uote\\.txt");
        assert_eq!(unquote_git_path("\"open"), "\"open");
    }

    fn fake_lsp() -> String {
        let exe = std::env::current_exe().unwrap();
        let debug = exe.parent().unwrap().parent().unwrap();
        let path = debug.join("examples").join(format!("fake_lsp{}", std::env::consts::EXE_SUFFIX));
        if !path.exists() {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let status = std::process::Command::new(cargo).args(["build", "-p", "opencode-lsp", "--example", "fake_lsp"]).status();
            assert!(status.is_ok_and(|s| s.success()), "could not build the fake language server");
        }
        path.to_string_lossy().into_owned()
    }

    // The `lsp` tool only exists behind OPENCODE_EXPERIMENTAL_LSP_TOOL, so it is driven here through the hooks the
    // loop hands to every tool.
    #[tokio::test(flavor = "multi_thread")]
    async fn lsp_tool_runs_against_the_workspace() {
        use opencode_tools::Tool as _;
        let tmp = tempfile::tempdir().unwrap();
        let dir = PathBuf::from(tmp.path().canonicalize().unwrap().to_string_lossy().trim_start_matches(r"\\?\"));
        let file = dir.join("a.fake");
        std::fs::write(&file, "ok\nERR here\n").unwrap();
        let workspace = Arc::new(Workspace::new(WorkspaceOptions {
            data_dir: dir.join("data"),
            directory: dir.clone(),
            worktree: dir.clone(),
            config: json!({"lsp": {"fake": {"command": [fake_lsp()], "extensions": [".fake"], "initialization": {"fake": {"value": 7}}}}}),
            ..Default::default()
        }));
        let hooks = ToolHooks { workspace: workspace.clone(), events: Arc::default(), host: None, directory: dir.to_string_lossy().into_owned(), plugins: None, engine: None };
        let ctx = opencode_tools::ToolContext {
            session_id: "ses_1".into(),
            message_id: "msg_1".into(),
            call_id: "call_1".into(),
            agent: "build".into(),
            cwd: dir.clone(),
            worktree: dir.clone(),
            data_dir: dir.join("data"),
            abort: tokio_util::sync::CancellationToken::new(),
            permissions: Arc::new(crate::AllowAll),
            hooks: Some(Arc::new(hooks)),
        };
        let tool = opencode_tools::lsp::LspTool;
        let hover = tool.execute(json!({"operation": "hover", "filePath": "a.fake", "line": 2, "character": 3}), &ctx).await.unwrap();
        assert_eq!(hover.title, "hover a.fake:2:3");
        assert_eq!(hover.metadata["result"], json!([{"contents": "hover 1:2 config=[7] change=none"}]));
        let symbols = tool.execute(json!({"operation": "documentSymbol", "filePath": file.to_string_lossy(), "line": 1, "character": 1}), &ctx).await.unwrap();
        assert_eq!((symbols.title.as_str(), &symbols.metadata["result"][0]["name"]), ("documentSymbol a.fake", &json!("main")));
        let none = tool.execute(json!({"operation": "goToImplementation", "filePath": "a.fake", "line": 1, "character": 1}), &ctx).await.unwrap();
        assert_eq!(none.output, "No results found for goToImplementation");
        std::fs::write(dir.join("b.txt"), "x").unwrap();
        let unsupported = tool.execute(json!({"operation": "hover", "filePath": "b.txt", "line": 1, "character": 1}), &ctx).await.unwrap_err();
        assert_eq!(unsupported.to_string(), "No LSP server available for this file type.");
        workspace.dispose().await;
    }

    #[test]
    fn block_on_works_inside_a_runtime() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let value = runtime.block_on(async { block_on(async { 41 + 1 }).unwrap() });
        assert_eq!(value, 42);
    }
}
