//! Workspace services for one instance directory: language servers, formatters and snapshots.

pub mod format;
pub mod lsp;
pub mod snapshot;
mod util;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use opencode_core::flag;
use serde_json::Value;

pub use format::{FormatOptions, FormatResult, Formatter, FormatterStatus};
pub use lsp::{
    Diagnostic, DiagnosticsMode, InstallState, InstallStatus, Location, Lsp, Status as LspStatus,
};
pub use snapshot::{FileDiff, FileDiffStatus, Patch, Snapshot};

#[derive(Debug, Clone, Default)]
pub struct WorkspaceOptions {
    /// `Global.Path.data`.
    pub data_dir: PathBuf,
    /// `Global.Path.cache`; previously installed servers under `<cache>/bin` and `<cache>/packages` are reused.
    pub cache_dir: Option<PathBuf>,
    pub project_id: String,
    pub directory: PathBuf,
    pub worktree: PathBuf,
    /// `project.vcs === "git"`.
    pub git: bool,
    /// Raw merged config; the `lsp`, `formatter` and `snapshot` keys are read from it.
    pub config: Value,
    pub experimental_lsp_ty: bool,
    pub experimental_oxfmt: bool,
}

impl WorkspaceOptions {
    /// Fill the experimental switches from `OPENCODE_EXPERIMENTAL_LSP_TY` / `OPENCODE_EXPERIMENTAL_OXFMT`.
    pub fn with_env_flags(mut self) -> Self {
        let env = &flag::process_env;
        self.experimental_lsp_ty = flag::effect_bool(env, "OPENCODE_EXPERIMENTAL_LSP_TY")
            .ok()
            .flatten()
            .unwrap_or(false);
        self.experimental_oxfmt =
            flag::effect_bool_or_experimental(env, "OPENCODE_EXPERIMENTAL_OXFMT").unwrap_or(false);
        self
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AfterEdit {
    /// A formatter matched and ran (the source's `Format.file` result; callers re-sync the BOM on `true`).
    pub formatted: bool,
    /// The formatter changed the bytes on disk.
    pub changed: bool,
    /// Text to append to the tool output; empty when there are no LSP errors.
    pub diagnostics_text: String,
}

/// Lazily built bundle of the three services; each one is inert when its config key is absent.
pub struct Workspace {
    opts: WorkspaceOptions,
    snapshot: OnceLock<Snapshot>,
    formatter: OnceLock<Formatter>,
    lsp: OnceLock<Lsp>,
    cleanup: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Workspace {
    pub fn new(opts: WorkspaceOptions) -> Self {
        Self {
            opts,
            snapshot: OnceLock::new(),
            formatter: OnceLock::new(),
            lsp: OnceLock::new(),
            cleanup: Mutex::new(None),
        }
    }

    pub fn options(&self) -> &WorkspaceOptions {
        &self.opts
    }

    pub fn snapshot(&self) -> &Snapshot {
        self.snapshot.get_or_init(|| {
            let snapshot = Snapshot::new(
                &self.opts.data_dir,
                &self.opts.project_id,
                &self.opts.worktree,
            )
            .with_directory(&self.opts.directory)
            .with_vcs(self.opts.git)
            .with_config(self.opts.config.get("snapshot").and_then(Value::as_bool));
            if snapshot.enabled() && tokio::runtime::Handle::try_current().is_ok() {
                *self.cleanup.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(snapshot.spawn_cleanup());
            }
            snapshot
        })
    }

    pub fn formatter(&self) -> &Formatter {
        self.formatter.get_or_init(|| {
            Formatter::new(
                self.opts.config.get("formatter"),
                FormatOptions {
                    directory: self.opts.directory.clone(),
                    worktree: self.opts.worktree.clone(),
                    cache_dir: self.opts.cache_dir.clone(),
                    experimental_oxfmt: self.opts.experimental_oxfmt,
                },
            )
        })
    }

    pub fn lsp(&self) -> &Lsp {
        self.lsp.get_or_init(|| {
            Lsp::new(
                self.opts.config.get("lsp"),
                lsp::Context {
                    directory: self.opts.directory.clone(),
                    worktree: self.opts.worktree.clone(),
                    cache_dir: self.opts.cache_dir.clone(),
                    experimental_ty: self.opts.experimental_lsp_ty,
                },
            )
        })
    }

    /// Run the matching formatters on a file that was just written.
    pub async fn format(&self, file: &Path) -> FormatResult {
        self.formatter().run(file).await
    }

    /// `touchFile(file, "document")` then the `edit`/`apply_patch` diagnostics suffix for that file.
    pub async fn diagnostics_text(&self, file: &Path) -> String {
        let lsp = self.lsp();
        if lsp.is_empty() {
            return String::new();
        }
        lsp.touch_file(file, Some(DiagnosticsMode::Document)).await;
        lsp.edit_report(file)
    }

    /// Format, then collect diagnostics the way `edit` reports them.
    pub async fn after_edit(&self, file: &Path) -> AfterEdit {
        let format = self.format(file).await;
        AfterEdit {
            formatted: format.ran,
            changed: format.changed,
            diagnostics_text: self.diagnostics_text(file).await,
        }
    }

    /// Like `after_edit`, with the `write` tool's report (adds up to five other files with errors).
    pub async fn after_write(&self, file: &Path) -> AfterEdit {
        let format = self.format(file).await;
        let lsp = self.lsp();
        let diagnostics_text = if lsp.is_empty() {
            String::new()
        } else {
            lsp.touch_file(file, Some(DiagnosticsMode::Document)).await;
            lsp.write_report(file)
        };
        AfterEdit {
            formatted: format.ran,
            changed: format.changed,
            diagnostics_text,
        }
    }

    /// `read` warms the servers for a file without waiting for diagnostics.
    pub async fn warm(&self, file: &Path) {
        if !self.lsp().is_empty() {
            self.lsp().touch_file(file, None).await;
        }
    }

    pub fn lsp_status(&self) -> Vec<LspStatus> {
        self.lsp.get().map(Lsp::status).unwrap_or_default()
    }

    /// Built-in servers that had to be acquired, with the state or failure reason of each.
    pub fn lsp_install_status(&self) -> Vec<InstallStatus> {
        self.lsp.get().map(Lsp::install_status).unwrap_or_default()
    }

    pub async fn formatter_status(&self) -> Vec<FormatterStatus> {
        self.formatter().status().await
    }

    /// Stop language servers and the snapshot cleanup loop.
    pub async fn dispose(&self) {
        if let Some(handle) = self
            .cleanup
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            handle.abort();
        }
        if let Some(lsp) = self.lsp.get() {
            lsp.shutdown().await;
        }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        if let Some(handle) = self
            .cleanup
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            handle.abort();
        }
    }
}
