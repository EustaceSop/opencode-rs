// A user-run shell command recorded as a `bash` tool call (`session/prompt.ts` shellImpl). The loop does not run.
use crate::run::{Guard, zero_assistant};
use crate::{Acquire, Engine, Error, Host, RunEnd, Status, Update, object, parse_model, plain_directory, report, unknown_error};
use opencode_storage::{
    Message, Part, PartBody, TextPart, ToolPart, ToolState, ToolStateRunning, ToolTimeRunning, UserMessage, UserModel, UserTime, id,
};
use serde_json::json;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const ABORTED: &str = "\n\n<metadata>\nUser aborted the command\n</metadata>";

#[derive(Debug, Clone, Default)]
pub struct ShellInput {
    pub session_id: String,
    pub agent: String,
    // "provider/model"
    pub model: Option<String>,
    pub command: String,
    pub message_id: Option<String>,
}

// Decodes a byte stream chunk by chunk, holding back an incomplete trailing character.
async fn pump(mut reader: impl tokio::io::AsyncRead + Unpin, chunks: mpsc::UnboundedSender<String>) {
    let mut pending: Vec<u8> = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        pending.extend_from_slice(&buffer[..read]);
        let valid = match std::str::from_utf8(&pending) {
            Ok(_) => pending.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => pending.len(),
        };
        let text = String::from_utf8_lossy(&pending[..valid]).into_owned();
        pending.drain(..valid);
        if !text.is_empty() && chunks.send(text).is_err() {
            return;
        }
    }
    if !pending.is_empty() {
        let _ = chunks.send(String::from_utf8_lossy(&pending).into_owned());
    }
}

async fn kill_tree(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        let mut killer = if cfg!(windows) {
            let mut killer = tokio::process::Command::new("taskkill");
            killer.args(["/pid", &pid.to_string(), "/T", "/F"]);
            killer
        } else {
            let mut killer = tokio::process::Command::new("kill");
            killer.args(["-TERM", "--", &format!("-{pid}")]);
            killer
        };
        let _ = killer.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().await;
    }
    if tokio::time::timeout(Duration::from_secs(3), child.wait()).await.is_err() {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

impl Engine {
    // Runs `command` in the session's directory and stores it as a user message plus an assistant `bash` tool part.
    pub async fn shell(&self, input: ShellInput, host: Arc<dyn Host>, abort: CancellationToken) -> Result<(Message, Vec<Part>), Error> {
        let session = self.session(&input.session_id)?;
        let session_id = session.id.as_str();
        let cancel = match self.acquire(session_id, &abort) {
            Acquire::Owner(cancel) => cancel,
            Acquire::Join(_) => return Err(Error::Busy(session_id.to_string())),
        };
        let mut guard = Guard { engine: self, session_id, end: RunEnd::Failed };
        self.set_status(session_id, Status::Busy, &host);
        let result = self.shell_run(&session, &input, &host, &cancel).await;
        if result.is_ok() {
            guard.end = RunEnd::Ok;
        }
        drop(guard);
        if let Err(error) = &result {
            report(&host, session_id, unknown_error(error.to_string()));
        }
        host.update(Update::Status { session_id: session_id.to_string(), status: Status::Idle });
        result
    }

    async fn shell_run(
        &self,
        session: &opencode_storage::Session,
        input: &ShellInput,
        host: &Arc<dyn Host>,
        abort: &CancellationToken,
    ) -> Result<(Message, Vec<Part>), Error> {
        let ctx = self.instance(&plain_directory(PathBuf::from(&session.directory)), Some(host)).await?;
        self.revert_cleanup(session)?;
        let agent = ctx.agent(&input.agent)?;
        let (provider_id, model_id) = match (&input.model, &agent.model) {
            (Some(model), _) => parse_model(model)?,
            (None, Some(model)) => (model.provider_id.clone(), model.model_id.clone()),
            (None, None) => self.current_model(&ctx, session)?,
        };
        let user = UserMessage {
            id: id::generate_or(id::Prefix::Message, input.message_id.clone()).map_err(|e| Error::Config(e.to_string()))?,
            session_id: session.id.clone(),
            time: UserTime { created: id::now_ms() },
            format: None,
            summary: None,
            agent: input.agent.clone(),
            model: UserModel { provider_id: provider_id.clone(), model_id: model_id.clone(), variant: None },
            system: None,
            tools: None,
        };
        self.store.upsert_message(&Message::User(user.clone()))?;
        let text = TextPart { text: "The following tool was executed by the user".into(), synthetic: Some(true), ..Default::default() };
        self.store.upsert_part(&Part::new(&session.id, &user.id, PartBody::Text(text)))?;

        let mut assistant = zero_assistant(&session.id, &user.id, &input.agent, &provider_id, &model_id, None, &ctx);
        self.store.upsert_message(&Message::Assistant(assistant.clone()))?;
        let command_input = object(json!({"command": input.command}));
        let running = |output: Option<&str>, start: u64| {
            ToolState::Running(ToolStateRunning {
                input: command_input.clone(),
                title: None,
                metadata: output.map(|output| object(json!({"output": output}))),
                time: ToolTimeRunning { start },
            })
        };
        let started = id::now_ms();
        let call_id = opencode_tools::util::tool_id();
        let tool = ToolPart { call_id: call_id.clone(), tool: "bash".into(), state: running(None, started), metadata: None };
        let mut part = Part::new(&session.id, &assistant.id, PartBody::Tool(tool));
        self.store.upsert_part(&part)?;
        host.update(Update::Tool(Box::new(part.clone())));

        let cwd = ctx.cwd.to_string_lossy().into_owned();
        let shell = opencode_tools::shell::preferred(ctx.info.shell.as_deref());
        let hooked = json!({"cwd": cwd, "sessionID": session.id, "callID": call_id});
        let extra = self.hook(&ctx, opencode_plugin::HookName::ShellEnv, hooked, json!({"env": {}})).await.map_err(Error::Config)?;
        let mut command = tokio::process::Command::new(&shell);
        command
            .args(opencode_tools::shell::args(&shell, &input.command, &cwd))
            .current_dir(&ctx.cwd)
            .envs(opencode_tools::shell::env(&shell))
            .envs(crate::plugins::env_pairs(&extra))
            .env("TERM", "dumb")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x0800_0000);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|e| Error::Config(format!("spawn {shell} failed: {e}")))?;

        let (chunks, mut merged) = mpsc::unbounded_channel();
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(pump(stdout, chunks.clone()));
        }
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(pump(stderr, chunks.clone()));
        }
        drop(chunks);
        let mut output = String::new();
        let mut aborted = false;
        loop {
            tokio::select! {
                chunk = merged.recv() => {
                    let Some(chunk) = chunk else { break };
                    output.push_str(&chunk);
                    if let PartBody::Tool(tool) = &mut part.body {
                        tool.state = running(Some(&output), started);
                    }
                    self.store.upsert_part(&part)?;
                    host.update(Update::Tool(Box::new(part.clone())));
                }
                _ = abort.cancelled(), if !aborted => {
                    aborted = true;
                    kill_tree(&mut child).await;
                }
            }
        }
        let _ = child.wait().await;

        if aborted {
            output.push_str(ABORTED);
        }
        let completed = id::now_ms();
        assistant.time.completed = Some(completed);
        self.store.upsert_message(&Message::Assistant(assistant.clone()))?;
        if let PartBody::Tool(tool) = &mut part.body {
            tool.state = tool.state.to_completed(output.clone(), String::new(), object(json!({"output": output})), None, completed);
        }
        self.store.upsert_part(&part)?;
        host.update(Update::Tool(Box::new(part.clone())));
        Ok((Message::Assistant(assistant), vec![part]))
    }
}
