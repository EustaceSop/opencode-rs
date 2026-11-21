//! Servers installed by the language's own toolchain; attempted only when that toolchain is on PATH.

use std::path::Path;

use super::{Env, Installer, Step, failed};
use crate::lsp::server;

impl Installer {
    pub(super) async fn gopls(&self, env: Env<'_>, bin: &Path) -> Step {
        let go = self.need(env, "go")?;
        self.gate()?;
        std::fs::create_dir_all(bin).map_err(|e| failed(format!("{}: {e}", bin.display())))?;
        let gobin = ("GOBIN", bin.to_string_lossy().into_owned());
        self.tool(
            &go,
            &["install", "golang.org/x/tools/gopls@latest"],
            None,
            &[gobin],
        )
        .await
    }

    pub(super) async fn rubocop(&self, env: Env<'_>, bin: &Path) -> Step {
        self.need(env, "ruby")?;
        let gem = self.need(env, "gem")?;
        self.gate()?;
        let dir = bin.to_string_lossy();
        self.tool(&gem, &["install", "rubocop", "--bindir", &dir], None, &[])
            .await
    }

    pub(super) async fn fsautocomplete(&self, env: Env<'_>, bin: &Path) -> Step {
        let dotnet = self.need(env, "dotnet")?;
        self.gate()?;
        let dir = bin.to_string_lossy();
        self.tool(
            &dotnet,
            &["tool", "install", "fsautocomplete", "--tool-path", &dir],
            None,
            &[],
        )
        .await
    }

    // Installed as a global dotnet tool, exactly as the source does; shared by csharp and razor.
    pub(super) async fn roslyn(&self, env: Env<'_>) -> Step {
        if server::roslyn(env).is_some() {
            return Ok(());
        }
        let dotnet = self.need(env, "dotnet")?;
        self.gate()?;
        self.tool(
            &dotnet,
            &[
                "tool",
                "install",
                "--global",
                "roslyn-language-server",
                "--prerelease",
            ],
            None,
            &[],
        )
        .await
    }
}
