//! Rust build: shell commands started with `background: true` (opencode 2.x `packages/core/src/tool/plugin/shell.ts`).
//! The command runs detached from the turn; its output streams to a file, and when it ends the session is told
//! through [`crate::WorkspaceHooks::background_done`] with v2's `<shell>` notification.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{build_command, kill_tree, spawn_reader, tail};
use crate::util::{display, io_err, tool_id};
use crate::{ToolError, WorkspaceHooks};

/// v2 `BACKGROUND_INSTRUCTION`, returned with the shell ID.
pub const INSTRUCTION: &str = "You will be notified automatically when the command finishes. The notification will include the command's output. Unless the user explicitly asks otherwise, DO NOT poll for completion, even if you need the final result to continue. Repeatedly sleeping and reading or searching the output file is polling, not useful work. You may read the current output if it lets you do useful work now, but do not repeatedly check it while waiting for the command to finish. Keep working on anything that does not depend on the result. If you have nothing else to do, end your response; you will be resumed automatically when the command finishes.";

/// How a background command ended.
#[derive(Debug, Clone, PartialEq)]
pub struct Finished {
    pub session_id: String,
    pub shell_id: String,
    pub command: String,
    /// `completed`, `cancelled` or `error`.
    pub state: &'static str,
    pub exit: Option<i32>,
    pub timeout: bool,
    pub truncated: bool,
    /// The output (its tail when long) and the exit notice.
    pub text: String,
}

impl Finished {
    /// v2 `ShellResult.notification(...).text`.
    pub fn notification(&self) -> String {
        format!("<shell id=\"{}\" state=\"{}\" command=\"{}\">\n{}\n</shell>", self.shell_id, self.state, self.command, self.text)
    }

    /// v2 `ShellResult.notification(...).metadata`.
    pub fn metadata(&self) -> serde_json::Value {
        let mut meta = serde_json::json!({"source": "shell", "shellID": self.shell_id, "jobID": self.shell_id, "state": self.state, "truncated": self.truncated});
        if let Some(exit) = self.exit {
            meta["exit"] = exit.into();
        }
        if self.timeout {
            meta["timeout"] = true.into();
        }
        meta
    }
}

struct Job {
    session_id: String,
    owner: PathBuf,
    command: String,
    pid: Option<u32>,
    cancel: CancellationToken,
    // Killed with its owner: nobody is left to tell.
    silent: Arc<std::sync::atomic::AtomicBool>,
}

static JOBS: LazyLock<Mutex<HashMap<String, Job>>> = LazyLock::new(Mutex::default);

fn jobs() -> std::sync::MutexGuard<'static, HashMap<String, Job>> {
    JOBS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Commands still running in the background for a session.
pub fn running(session_id: &str) -> Vec<String> {
    let mut list: Vec<(String, String)> =
        jobs().iter().filter(|(_, job)| job.session_id == session_id).map(|(id, job)| (id.clone(), job.command.clone())).collect();
    list.sort();
    list.into_iter().map(|(_, command)| command).collect()
}

fn kill_where(matches: impl Fn(&Job) -> bool) {
    let mut taken = Vec::new();
    jobs().retain(|_, job| {
        if matches(job) {
            taken.push((job.pid, job.cancel.clone(), job.silent.clone()));
            return false;
        }
        true
    });
    for (pid, cancel, silent) in taken {
        silent.store(true, std::sync::atomic::Ordering::SeqCst);
        cancel.cancel();
        if let Some(pid) = pid {
            kill_pid(pid);
        }
    }
}

/// Kills the background commands of one session (it was deleted).
pub fn kill_session(session_id: &str) {
    kill_where(|job| job.session_id == session_id);
}

/// Kills the background commands started under one data directory (its engine shuts down).
pub fn kill_owned(data_dir: &Path) {
    kill_where(|job| job.owner == data_dir);
}

/// Kills every background command of this process (the app exits).
pub fn kill_all() {
    kill_where(|_| true);
}

// Synchronous tree kill, usable while the runtime winds down.
fn kill_pid(pid: u32) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("taskkill")
            .args(["/pid", &pid.to_string(), "/T", "/F"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(0x0800_0000)
            .status();
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
}

// Windows: background commands join a job object that kills them when this process ends, however it ends.
#[cfg(windows)]
fn contain(pid: u32) {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};
    static JOB: LazyLock<usize> = LazyLock::new(|| unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return 0;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let size = std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
        SetInformationJobObject(job, JobObjectExtendedLimitInformation, &info as *const _ as *const _, size);
        job as usize
    });
    if *JOB == 0 {
        return;
    }
    unsafe {
        let process: HANDLE = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if !process.is_null() {
            AssignProcessToJobObject(*JOB as HANDLE, process);
            CloseHandle(process);
        }
    }
}

#[cfg(not(windows))]
fn contain(_pid: u32) {}

/// Starts `command` detached from the turn; returns its shell ID and output file at once.
#[allow(clippy::too_many_arguments)]
pub(crate) fn start(
    shell: &str,
    command: &str,
    cwd: &Path,
    env: Vec<(String, String)>,
    timeout: u64,
    session_id: &str,
    data_dir: &Path,
    limits: (usize, usize),
    hooks: Option<Arc<dyn WorkspaceHooks>>,
) -> Result<(String, PathBuf), ToolError> {
    let shell_id = format!("shell_{}", tool_id().trim_start_matches("tool_"));
    let dir = crate::truncate::truncation_dir(data_dir);
    std::fs::create_dir_all(&dir).map_err(io_err)?;
    let file = dir.join(&shell_id);
    let mut sink = std::fs::File::create(&file).map_err(io_err)?;
    let mut child = build_command(shell, command, cwd).envs(env).spawn().map_err(io_err)?;
    let pid = child.id();
    if let Some(pid) = pid {
        contain(pid);
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    if let Some(stdout) = child.stdout.take() {
        spawn_reader(stdout, tx.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_reader(stderr, tx.clone());
    }
    drop(tx);
    let cancel = CancellationToken::new();
    let silent = Arc::new(std::sync::atomic::AtomicBool::new(false));
    jobs().insert(
        shell_id.clone(),
        Job {
            session_id: session_id.to_string(),
            owner: data_dir.to_path_buf(),
            command: command.to_string(),
            pid,
            cancel: cancel.clone(),
            silent: silent.clone(),
        },
    );
    let (id, path, command, session_id) = (shell_id.clone(), file.clone(), command.to_string(), session_id.to_string());
    let (max_lines, max_bytes) = limits;
    tokio::spawn(async move {
        let mut kept = String::new();
        let mut code = None;
        let (mut ended, mut reading, mut cancelled, mut expired) = (false, true, false, false);
        let deadline = (timeout > 0).then(|| tokio::time::Instant::now() + Duration::from_millis(timeout));
        let mut grace: Option<tokio::time::Instant> = None;
        loop {
            tokio::select! {
                chunk = rx.recv(), if reading => match chunk {
                    Some(chunk) => {
                        let _ = sink.write_all(chunk.as_bytes());
                        kept.push_str(&chunk);
                        // Only the tail is reported; the file has everything.
                        if kept.len() > max_bytes * 4 {
                            let mut cut = kept.len() - max_bytes * 2;
                            while !kept.is_char_boundary(cut) {
                                cut += 1;
                            }
                            kept.drain(..cut);
                        }
                    }
                    None => reading = false,
                },
                status = child.wait(), if !ended => {
                    code = status.ok().and_then(|s| s.code());
                    ended = true;
                }
                _ = cancel.cancelled(), if !ended => {
                    cancelled = true;
                    kill_tree(&mut child).await;
                    ended = true;
                }
                _ = async { tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)).await }, if !ended && deadline.is_some() => {
                    expired = true;
                    kill_tree(&mut child).await;
                    ended = true;
                }
                _ = async { tokio::time::sleep_until(grace.unwrap_or_else(tokio::time::Instant::now)).await }, if ended && grace.is_some() => break,
            }
            if ended {
                if !reading {
                    break;
                }
                // A surviving grandchild may hold the pipe open.
                grace.get_or_insert_with(|| tokio::time::Instant::now() + Duration::from_secs(1));
            }
        }
        drop(sink);
        jobs().remove(&id);
        if silent.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let (shown, cut) = tail(&kept, max_lines, max_bytes);
        let truncated = cut || kept.len() < std::fs::metadata(&path).map_or(0, |m| m.len() as usize);
        let mut parts = Vec::new();
        if truncated {
            parts.push(format!("...output truncated...\n\nFull output saved to: {}\n\n{shown}", display(&path)));
        } else if !shown.is_empty() {
            parts.push(shown);
        }
        let state = if cancelled { "cancelled" } else { "completed" };
        // v2 `ShellResult.notice`.
        if expired {
            parts.push(format!(
                "Command timed out after {} seconds. If it needs longer and isn't waiting for input, rerun with a larger timeout.\nExit code: {}",
                timeout as f64 / 1000.0,
                code.map_or("unknown".to_string(), |c| c.to_string())
            ));
        } else if cancelled {
            parts = vec!["Cancelled".to_string()];
        } else if code != Some(0) {
            parts.push(format!("Exit code: {}", code.map_or("unknown".to_string(), |c| c.to_string())));
        }
        let finished = Finished { session_id, shell_id: id, command, state, exit: code, timeout: expired, truncated, text: parts.join("\n\n") };
        if let Some(hooks) = hooks {
            hooks.background_done(finished);
        }
    });
    Ok((shell_id, file))
}

/// v2 `backgroundResult` plus the instruction, as the tool's output.
pub(crate) fn started_output(shell_id: &str, file: &Path) -> String {
    format!("Command moved to the background (shell ID: {shell_id}).\nOutput is streaming to: {}\n\n{INSTRUCTION}", display(file))
}
