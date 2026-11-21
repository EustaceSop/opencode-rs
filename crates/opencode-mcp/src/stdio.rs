//! Local servers: newline-delimited JSON-RPC over a child process' stdio (`StdioClientTransport`).

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::Mutex;

use crate::client::{Inbound, InboundTx, Transport};
use crate::error::McpError;

const CLOSE_GRACE: Duration = Duration::from_millis(2000);

pub(crate) struct StdioTransport {
    stdin: Mutex<Option<ChildStdin>>,
    child: Mutex<Option<Child>>,
    pid: Option<u32>,
}

/// cross-spawn semantics: look the command up on the child's `PATH` (with `PATHEXT` on Windows).
pub(crate) fn resolve_command(
    command: &str,
    cwd: &Path,
    env: &HashMap<String, String>,
) -> OsString {
    let path = env
        .iter()
        .find(|(key, _)| {
            if cfg!(windows) {
                key.eq_ignore_ascii_case("PATH")
            } else {
                key.as_str() == "PATH"
            }
        })
        .map(|(_, value)| value.clone());
    which::which_in(command, path, cwd)
        .map(|p| p.into_os_string())
        .unwrap_or_else(|_| command.into())
}

impl StdioTransport {
    pub(crate) fn spawn(
        command: &[String],
        cwd: &Path,
        env: &HashMap<String, String>,
        tx: InboundTx,
    ) -> Result<Self, McpError> {
        let Some((program, args)) = command.split_first() else {
            return Err(McpError::other("MCP command is empty"));
        };
        let mut cmd = Command::new(resolve_command(program, cwd, env));
        cmd.args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW (`windowsHide`)
        let mut child = cmd
            .spawn()
            .map_err(|e| McpError::Other(format!("spawn {program} failed: {e}")))?;
        let pid = child.id();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");

        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim_end_matches('\r');
                if line.is_empty() {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                if tx.send(Inbound::Message(value)).is_err() {
                    return;
                }
            }
            let _ = tx.send(Inbound::Closed);
        });
        tokio::spawn(async move {
            let mut sink = [0u8; 4096];
            while matches!(stderr.read(&mut sink).await, Ok(n) if n > 0) {}
        });

        Ok(Self {
            stdin: Mutex::new(stdin),
            child: Mutex::new(Some(child)),
            pid,
        })
    }
}

#[async_trait::async_trait]
impl Transport for StdioTransport {
    async fn send(&self, message: &Value) -> Result<(), McpError> {
        let mut guard = self.stdin.lock().await;
        let stdin = guard.as_mut().ok_or(McpError::NotConnected)?;
        let mut line = serde_json::to_vec(message).map_err(|e| McpError::Other(e.to_string()))?;
        line.push(b'\n');
        let written = async {
            stdin.write_all(&line).await?;
            stdin.flush().await
        };
        // A dead pipe surfaces as "Connection closed" once stdout reaches EOF, as in the SDK.
        let _ = written.await;
        Ok(())
    }

    // stdin EOF, then terminate the whole tree if the server does not exit on its own.
    async fn close(&self) {
        self.stdin.lock().await.take();
        let Some(mut child) = self.child.lock().await.take() else {
            return;
        };
        let descendants = match self.pid {
            Some(pid) => crate::process::descendants(pid).await,
            None => Vec::new(),
        };
        let exited = tokio::time::timeout(CLOSE_GRACE, child.wait())
            .await
            .is_ok();
        crate::process::terminate(&descendants).await;
        if !exited {
            if let Some(pid) = self.pid {
                crate::process::kill_tree(pid).await;
            }
            let _ = child.kill().await;
        }
    }

    fn pid(&self) -> Option<u32> {
        self.pid
    }
}
