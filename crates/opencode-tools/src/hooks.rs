//! What the file tools call into after touching a file: formatter, bus events and language servers.

use std::path::Path;
use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::ToolContext;

const MAX_PER_FILE: usize = 20;

/// One entry of `lsp.diagnostics()`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileDiagnostics {
    /// Normalised absolute path, the key of the original's diagnostics record.
    pub file: String,
    /// The raw LSP diagnostics of the file.
    pub issues: Value,
    /// `LSP.Diagnostic.pretty` of every severity-1 issue.
    pub errors: Vec<String>,
}

/// Every method defaults to "nothing configured".
#[async_trait::async_trait]
pub trait WorkspaceHooks: Send + Sync {
    /// `Format.file`: true when a formatter ran on the file.
    async fn format(&self, _file: &Path) -> bool {
        false
    }

    /// An instance bus event (`file.edited`, `file.watcher.updated`).
    fn publish(&self, _kind: &str, _properties: Value) {}

    /// `lsp.touchFile(file, "document")` when `wait`, else the `read` warm-up (not awaited by the original).
    async fn touch(&self, _file: &Path, _wait: bool) {}

    /// `lsp.diagnostics()`.
    fn diagnostics(&self) -> Vec<FileDiagnostics> {
        Vec::new()
    }

    /// `FSUtil.normalizePath(file)` as used for the keys of [`WorkspaceHooks::diagnostics`].
    fn normalize(&self, file: &Path) -> String {
        crate::util::normalize_path(file).to_string_lossy().into_owned()
    }

    /// `lsp.hasClients(file)`.
    async fn lsp_available(&self, _file: &Path) -> bool {
        false
    }

    /// Extra environment for a shell command (the plugin `shell.env` hook); an error fails the call.
    async fn shell_env(&self, _cwd: &Path, _session_id: &str, _call_id: &str) -> Result<Vec<(String, String)>, String> {
        Ok(Vec::new())
    }

    /// Rust build: a command started with `background: true` ended; the session should hear about it.
    fn background_done(&self, _finished: crate::bash::background::Finished) {}

    /// One `lsp` tool operation; `line` and `character` are 0-based.
    async fn lsp_request(&self, _operation: &str, _file: &Path, _line: u32, _character: u32, _query: &str) -> Vec<Value> {
        Vec::new()
    }
}

pub type Hooks = Option<Arc<dyn WorkspaceHooks>>;

/// `LSP.Diagnostic.report(label, issues)` from already formatted error lines.
pub fn report(label: &str, errors: &[String]) -> String {
    if errors.is_empty() {
        return String::new();
    }
    let limited: Vec<&str> = errors.iter().take(MAX_PER_FILE).map(String::as_str).collect();
    let suffix = if errors.len() > MAX_PER_FILE {
        format!("\n... and {} more", errors.len() - MAX_PER_FILE)
    } else {
        String::new()
    };
    format!("<diagnostics file=\"{label}\">\n{}{suffix}\n</diagnostics>", limited.join("\n"))
}

pub(crate) fn diagnostics_json(all: &[FileDiagnostics]) -> Value {
    let mut map = Map::new();
    for item in all {
        map.insert(item.file.clone(), item.issues.clone());
    }
    Value::Object(map)
}

/// `format.file` + `Bom.syncFile`: the text on disk (without BOM) when a formatter ran.
pub(crate) async fn format_sync(ctx: &ToolContext, file: &Path, bom: bool) -> Option<String> {
    let hooks = ctx.hooks.as_ref()?;
    if !hooks.format(file).await {
        return None;
    }
    let (current_bom, text) = crate::util::bom_read(file).await.ok()?;
    if current_bom != bom {
        let _ = crate::util::write_with_dirs(file, &crate::util::bom_join(&text, bom)).await;
    }
    Some(text)
}

pub(crate) fn publish_edited(ctx: &ToolContext, file: &str) {
    if let Some(hooks) = &ctx.hooks {
        hooks.publish("file.edited", json!({ "file": file }));
    }
}

pub(crate) fn publish_watcher(ctx: &ToolContext, file: &str, event: &str) {
    if let Some(hooks) = &ctx.hooks {
        hooks.publish("file.watcher.updated", json!({ "file": file, "event": event }));
    }
}

/// `lsp.touchFile(file, "document")` for each file, then `lsp.diagnostics()`.
pub(crate) async fn collect(ctx: &ToolContext, files: &[&Path]) -> Vec<FileDiagnostics> {
    let Some(hooks) = &ctx.hooks else {
        return Vec::new();
    };
    for file in files {
        hooks.touch(file, true).await;
    }
    hooks.diagnostics()
}

/// The report block of one file out of a diagnostics snapshot.
pub(crate) fn block(ctx: &ToolContext, all: &[FileDiagnostics], file: &Path, label: &str) -> String {
    let Some(hooks) = &ctx.hooks else {
        return String::new();
    };
    let key = hooks.normalize(file);
    all.iter().find(|item| item.file == key).map_or_else(String::new, |item| report(label, &item.errors))
}
