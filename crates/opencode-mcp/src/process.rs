//! Child process tree helpers for shutdown (`index.ts:418-440,530-555`).

use std::process::Stdio;

use tokio::process::Command;

/// Recursive `pgrep -P`; empty on Windows, as in the original.
pub(crate) async fn descendants(pid: u32) -> Vec<u32> {
    if cfg!(windows) {
        return Vec::new();
    }
    let mut pids: Vec<u32> = Vec::new();
    let mut queue = vec![pid];
    let mut index = 0;
    while index < queue.len() {
        let current = queue[index];
        index += 1;
        let Ok(output) = Command::new("pgrep")
            .args(["-P", &current.to_string()])
            .stdin(Stdio::null())
            .output()
            .await
        else {
            return pids;
        };
        for token in String::from_utf8_lossy(&output.stdout).split('\n') {
            if let Ok(child) = token.trim().parse::<u32>()
                && !pids.contains(&child)
            {
                pids.push(child);
                queue.push(child);
            }
        }
    }
    pids
}

/// SIGTERM each pid (no-op on Windows).
pub(crate) async fn terminate(pids: &[u32]) {
    if cfg!(windows) || pids.is_empty() {
        return;
    }
    let _ = Command::new("kill")
        .arg("-TERM")
        .args(pids.iter().map(u32::to_string))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

/// Force-kill `pid` and everything below it.
pub(crate) async fn kill_tree(pid: u32) {
    if cfg!(windows) {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        return;
    }
    let mut pids = descendants(pid).await;
    pids.push(pid);
    terminate(&pids).await;
}
