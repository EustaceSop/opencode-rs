//! Lazy acquisition of the built-in language servers, ported from the `spawn` bodies in
//! `opencode/src/lsp/server.ts`. Layout under `<cache>/bin` and `<cache>/packages` matches the TypeScript build.

mod archive;
pub(crate) mod npm;
mod release;
mod toolchain;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, OnceLock};

use indexmap::IndexMap;
use opencode_core::flag;
use serde::{Deserialize, Serialize};
use tokio::sync::OnceCell;

use super::server::{self, Context, Env, Launch, SERVERS};
use crate::util::{self, RunOptions};

pub const DISABLE_FLAG: &str = "OPENCODE_DISABLE_LSP_DOWNLOAD";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstallState {
    Installing,
    Installed,
    /// The install ran and did not produce a usable server.
    Failed,
    /// Nothing was attempted: downloads disabled, toolchain or project dependency missing, unsupported platform.
    Unavailable,
}

/// Acquisition state of one built-in server that was missing when a matching file was touched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallStatus {
    pub id: String,
    pub state: InstallState,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Stop {
    Unavailable(String),
    Failed(String),
}

pub(crate) type Step<T = ()> = Result<T, Stop>;

pub(crate) fn unavailable(reason: impl Into<String>) -> Stop {
    Stop::Unavailable(reason.into())
}

pub(crate) fn failed(reason: impl Into<String>) -> Stop {
    Stop::Failed(reason.into())
}

/// Node's `process.platform` / `process.arch` spellings, which the asset names are built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Platform {
    pub os: &'static str,
    pub arch: &'static str,
}

impl Platform {
    pub(crate) fn current() -> Self {
        let os = match std::env::consts::OS {
            "windows" => "win32",
            "macos" => "darwin",
            other => other,
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            "x86" => "ia32",
            other => other,
        };
        Self { os, arch }
    }

    pub(crate) fn exe(&self, name: &str) -> String {
        if self.os == "win32" {
            format!("{name}.exe")
        } else {
            name.to_string()
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Endpoints {
    pub github_api: String,
    pub github: String,
    pub hashicorp: String,
    pub eclipse: String,
    pub jetbrains: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            github_api: "https://api.github.com".into(),
            github: "https://github.com".into(),
            hashicorp: "https://api.releases.hashicorp.com".into(),
            eclipse: "https://www.eclipse.org".into(),
            jetbrains: "https://download-cdn.jetbrains.com".into(),
        }
    }
}

pub(crate) struct Installer {
    disabled: bool,
    platform: Platform,
    endpoints: Endpoints,
    // Replaces the process PATH for lookups and toolchain commands; only tests set it.
    path: Option<OsString>,
    http: OnceLock<reqwest::Client>,
    inflight: Mutex<HashMap<&'static str, Arc<OnceCell<Step>>>>,
    records: Mutex<IndexMap<String, InstallStatus>>,
    seq: AtomicU64,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

// csharp and razor share one roslyn install, as `roslynLanguageServerInstall` does.
fn unit(id: &str) -> Option<&'static str> {
    if matches!(id, "csharp" | "razor") {
        return Some("roslyn");
    }
    SERVERS
        .iter()
        .find(|def| def.id == id && def.install.is_some())
        .map(|def| def.id)
}

fn tail(text: &str) -> String {
    let line = text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    line.chars().take(300).collect()
}

impl Installer {
    pub(crate) fn from_env() -> Self {
        let disabled = flag::effect_bool(&flag::process_env, DISABLE_FLAG)
            .ok()
            .flatten()
            .unwrap_or(false);
        Self::new(disabled)
    }

    fn new(disabled: bool) -> Self {
        Self {
            disabled,
            platform: Platform::current(),
            endpoints: Endpoints::default(),
            path: None,
            http: OnceLock::new(),
            inflight: Mutex::default(),
            records: Mutex::default(),
            seq: AtomicU64::new(0),
        }
    }

    pub(crate) fn env<'a>(&'a self, ctx: &'a Context) -> Env<'a> {
        Env {
            ctx,
            path: self.path.as_deref(),
        }
    }

    pub(crate) fn status(&self) -> Vec<InstallStatus> {
        lock(&self.records).values().cloned().collect()
    }

    fn set(&self, id: &str, state: InstallState, reason: Option<String>) {
        lock(&self.records).insert(
            id.to_string(),
            InstallStatus {
                id: id.to_string(),
                state,
                reason,
            },
        );
    }

    fn gate(&self) -> Step {
        if self.disabled {
            return Err(unavailable(format!(
                "downloads are disabled by {DISABLE_FLAG}"
            )));
        }
        Ok(())
    }

    fn need(&self, env: Env<'_>, tool: &str) -> Step<String> {
        env.which(tool)
            .ok_or_else(|| unavailable(format!("{tool} was not found on PATH")))
    }

    // Run a toolchain command; a non-zero exit fails the install.
    async fn tool(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
        vars: &[(&str, String)],
    ) -> Step {
        let mut env: HashMap<String, String> = vars
            .iter()
            .map(|(key, value)| (key.to_string(), value.clone()))
            .collect();
        if let Some(path) = &self.path {
            env.insert("PATH".into(), path.to_string_lossy().into_owned());
        }
        let opts = RunOptions {
            cwd,
            env: Some(&env),
            stdin: None,
        };
        let out = util::run(program, args, opts).await;
        if out.code == 0 {
            return Ok(());
        }
        let name = Path::new(program)
            .file_stem()
            .unwrap_or(OsStr::new(program))
            .to_string_lossy()
            .into_owned();
        let detail = match tail(&out.stderr) {
            line if line.is_empty() => tail(&out.text()),
            line => line,
        };
        Err(failed(format!(
            "`{name} {}` exited with code {}: {detail}",
            args.join(" "),
            out.code
        )))
    }

    /// Install the server behind `id` if it is still missing, then resolve its command line.
    /// Concurrent callers for the same server share one install.
    pub(crate) async fn ensure(&self, id: &str, root: &Path, ctx: &Context) -> Step<Launch> {
        let Some(unit) = unit(id) else {
            return Err(unavailable("not installed"));
        };
        let env = self.env(ctx);
        let cell = lock(&self.inflight).entry(unit).or_default().clone();
        let mut ran = false;
        let outcome = cell
            .get_or_init(|| async {
                ran = true;
                // Another root may have finished the same install since the caller looked.
                if server::resolve_with(id, root, env).await.is_some() {
                    return Ok(());
                }
                self.set(id, InstallState::Installing, None);
                self.install(id, root, ctx).await
            })
            .await
            .clone();
        if ran {
            lock(&self.inflight).remove(unit);
        }
        let result = match outcome {
            Err(stop) => Err(stop),
            Ok(()) => match server::resolve_with(id, root, env).await {
                Some(launch) => Ok(launch),
                None if id == "razor" => Err(unavailable(
                    "the C# VS Code extension (.razorExtension) was not found",
                )),
                None => Err(failed("installed, but the server executable was not found")),
            },
        };
        match &result {
            Ok(_) => self.set(id, InstallState::Installed, None),
            Err(Stop::Failed(reason)) => self.set(id, InstallState::Failed, Some(reason.clone())),
            Err(Stop::Unavailable(reason)) => {
                self.set(id, InstallState::Unavailable, Some(reason.clone()))
            }
        }
        result
    }

    async fn install(&self, id: &str, root: &Path, ctx: &Context) -> Step {
        let cache = ctx
            .cache_dir
            .as_deref()
            .ok_or_else(|| unavailable("no cache directory is configured"))?;
        let bin = cache.join("bin");
        let env = self.env(ctx);
        let tsserver = || {
            util::resolve_module("typescript/lib/tsserver.js", &ctx.directory)
                .map(drop)
                .ok_or_else(|| unavailable("typescript is not installed in the project"))
        };
        // `Npm.which(pkg)` behind the download flag.
        let gated = async |pkg: &str, hint: Option<&str>| {
            self.gate()?;
            self.npm(env, cache, pkg, hint).await
        };
        match id {
            // typescript and biome call `Npm.which` without consulting the flag.
            "typescript" => {
                tsserver()?;
                self.npm(env, cache, "typescript-language-server", None)
                    .await
            }
            "biome" => {
                util::resolve_module("biome", root)
                    .ok_or_else(|| unavailable("biome is not installed in the project"))?;
                self.npm(env, cache, "biome", None).await
            }
            "vue" => gated("@vue/language-server", None).await,
            "pyright" => gated("pyright", Some("pyright-langserver")).await,
            "svelte" => gated("svelte-language-server", None).await,
            "astro" => {
                tsserver()?;
                gated("@astrojs/language-server", None).await
            }
            "yaml-ls" => gated("yaml-language-server", None).await,
            "php intelephense" => gated("intelephense", None).await,
            "bash" => gated("bash-language-server", None).await,
            "dockerfile" => gated("dockerfile-language-server-nodejs", None).await,
            "eslint" => self.eslint(env, &bin).await,
            "elixir-ls" => self.elixir_ls(env, &bin).await,
            "zls" => self.zls(env, &bin).await,
            "clangd" => self.clangd(&bin).await,
            "lua-ls" => self.lua_ls(&bin).await,
            "texlab" => self.texlab(&bin).await,
            "tinymist" => self.tinymist(&bin).await,
            "terraform" => self.terraform(&bin).await,
            "kotlin-ls" => self.kotlin_ls(&bin).await,
            "jdtls" => self.jdtls(env, &bin).await,
            "gopls" => self.gopls(env, &bin).await,
            "ruby-lsp" => self.rubocop(env, &bin).await,
            "fsharp" => self.fsautocomplete(env, &bin).await,
            "csharp" | "razor" => self.roslyn(env).await,
            _ => Err(unavailable("not installed")),
        }
    }
}
