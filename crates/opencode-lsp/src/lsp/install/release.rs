//! Servers fetched as release or source archives into `<cache>/bin`.

use std::path::Path;

use serde_json::Value;

use super::archive::{Format, Place};
use super::{Env, Installer, Platform, Step, failed, unavailable};
use crate::lsp::server;
use crate::util::{self, RunOptions};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Asset {
    pub name: String,
    pub url: String,
}

fn assets(release: &Value) -> Vec<Asset> {
    let items = release.get("assets").and_then(Value::as_array);
    items
        .into_iter()
        .flatten()
        .filter_map(|item| {
            Some(Asset {
                name: item.get("name")?.as_str()?.to_string(),
                url: item.get("browser_download_url")?.as_str()?.to_string(),
            })
        })
        .collect()
}

fn find(release: &Value, name: &str) -> Step<Asset> {
    assets(release)
        .into_iter()
        .find(|asset| asset.name == name)
        .ok_or_else(|| failed(format!("the latest release has no asset named {name}")))
}

fn unsupported(platform: Platform) -> super::Stop {
    unavailable(format!("no build for {}-{}", platform.os, platform.arch))
}

fn present(path: &Path) -> Step {
    if path.exists() {
        return Ok(());
    }
    Err(failed(format!(
        "{} is missing after extraction",
        path.display()
    )))
}

pub(super) fn zls_asset(platform: Platform) -> Option<String> {
    let arch = match platform.arch {
        "arm64" => "aarch64",
        "x64" => "x86_64",
        "ia32" => "x86",
        other => other,
    };
    let os = match platform.os {
        "darwin" => "macos",
        "win32" => "windows",
        other => other,
    };
    let ext = if platform.os == "win32" {
        "zip"
    } else {
        "tar.xz"
    };
    let name = format!("zls-{arch}-{os}.{ext}");
    const SUPPORTED: [&str; 8] = [
        "zls-x86_64-linux.tar.xz",
        "zls-x86_64-macos.tar.xz",
        "zls-x86_64-windows.zip",
        "zls-aarch64-linux.tar.xz",
        "zls-aarch64-macos.tar.xz",
        "zls-aarch64-windows.zip",
        "zls-x86-linux.tar.xz",
        "zls-x86-windows.zip",
    ];
    SUPPORTED.contains(&name.as_str()).then_some(name)
}

// Asset whose name carries the platform token and the tag; `.zip` first, then `.tar.xz`, then anything.
pub(super) fn clangd_asset(platform: Platform, tag: &str, assets: &[Asset]) -> Option<Asset> {
    let token = match platform.os {
        "darwin" => "mac",
        "linux" => "linux",
        "win32" => "windows",
        _ => return None,
    };
    let valid = |asset: &&Asset| asset.name.contains(token) && asset.name.contains(tag);
    let by_ext = |ext: &str| {
        assets
            .iter()
            .find(|asset| valid(asset) && asset.name.ends_with(ext))
    };
    by_ext(".zip")
        .or_else(|| by_ext(".tar.xz"))
        .or_else(|| assets.iter().find(valid))
        .cloned()
}

pub(super) fn lua_asset(platform: Platform, tag: &str) -> Option<String> {
    let ext = if platform.os == "win32" {
        "zip"
    } else {
        "tar.gz"
    };
    let suffix = format!("{}-{}.{ext}", platform.os, platform.arch);
    const SUPPORTED: [&str; 6] = [
        "darwin-arm64.tar.gz",
        "darwin-x64.tar.gz",
        "linux-x64.tar.gz",
        "linux-arm64.tar.gz",
        "win32-x64.zip",
        "win32-ia32.zip",
    ];
    SUPPORTED
        .contains(&suffix.as_str())
        .then(|| format!("lua-language-server-{tag}-{suffix}"))
}

pub(super) fn texlab_asset(platform: Platform) -> String {
    let arch = if platform.arch == "arm64" {
        "aarch64"
    } else {
        "x86_64"
    };
    let (os, ext) = match platform.os {
        "darwin" => ("macos", "tar.gz"),
        "win32" => ("windows", "zip"),
        _ => ("linux", "tar.gz"),
    };
    format!("texlab-{arch}-{os}.{ext}")
}

pub(super) fn tinymist_asset(platform: Platform) -> String {
    let arch = if platform.arch == "arm64" {
        "aarch64"
    } else {
        "x86_64"
    };
    let (os, ext) = match platform.os {
        "darwin" => ("apple-darwin", "tar.gz"),
        "win32" => ("pc-windows-msvc", "zip"),
        _ => ("unknown-linux-gnu", "tar.gz"),
    };
    format!("tinymist-{arch}-{os}.{ext}")
}

pub(super) fn terraform_build(platform: Platform) -> (&'static str, &'static str) {
    let arch = if platform.arch == "arm64" {
        "arm64"
    } else {
        "amd64"
    };
    let os = if platform.os == "win32" {
        "windows"
    } else {
        platform.os
    };
    (arch, os)
}

pub(super) fn kotlin_asset(platform: Platform, version: &str) -> Option<String> {
    let arch = if platform.arch == "arm64" {
        "aarch64"
    } else {
        platform.arch
    };
    let os = match platform.os {
        "darwin" => "mac",
        "win32" => "win",
        other => other,
    };
    const SUPPORTED: [&str; 6] = [
        "mac-x64",
        "mac-aarch64",
        "linux-x64",
        "linux-aarch64",
        "win-x64",
        "win-aarch64",
    ];
    SUPPORTED
        .contains(&format!("{os}-{arch}").as_str())
        .then(|| format!("kotlin-lsp-{version}-{os}-{arch}.zip"))
}

fn format_of(name: &str, tar_flag: &'static str) -> Format {
    if name.ends_with(".zip") {
        Format::Zip
    } else {
        Format::Tar {
            flag: tar_flag,
            strip: false,
        }
    }
}

impl Installer {
    fn chmod(&self, path: &Path) {
        #[cfg(unix)]
        if self.platform.os != "win32" {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
        }
        #[cfg(not(unix))]
        let _ = path;
    }

    async fn latest(&self, repo: &str) -> Step<Value> {
        let url = format!("{}/repos/{repo}/releases/latest", self.endpoints.github_api);
        self.json(&url).await
    }

    pub(super) async fn zls(&self, env: Env<'_>, bin: &Path) -> Step {
        self.need(env, "zig")?;
        self.gate()?;
        let release = self.latest("zigtools/zls").await?;
        let name = zls_asset(self.platform).ok_or_else(|| unsupported(self.platform))?;
        let asset = find(&release, &name)?;
        self.unpack(
            bin,
            &asset.url,
            &name,
            format_of(&name, "-xf"),
            Place::Into(bin),
        )
        .await?;
        let exe = bin.join(self.platform.exe("zls"));
        present(&exe)?;
        self.chmod(&exe);
        Ok(())
    }

    pub(super) async fn clangd(&self, bin: &Path) -> Step {
        self.gate()?;
        let release = self.latest("clangd/clangd").await?;
        let tag = release
            .get("tag_name")
            .and_then(Value::as_str)
            .filter(|tag| !tag.is_empty())
            .ok_or_else(|| failed("the latest clangd release has no tag"))?;
        let asset = clangd_asset(self.platform, tag, &assets(&release))
            .ok_or_else(|| unsupported(self.platform))?;
        if !asset.name.ends_with(".zip") && !asset.name.ends_with(".tar.xz") {
            return Err(failed(format!("unsupported archive {}", asset.name)));
        }
        let format = format_of(&asset.name, "-xf");
        self.unpack(bin, &asset.url, &asset.name, format, Place::Into(bin))
            .await?;
        let exe = bin
            .join(format!("clangd_{tag}"))
            .join("bin")
            .join(self.platform.exe("clangd"));
        present(&exe)?;
        self.chmod(&exe);
        let link = bin.join("clangd");
        let _ = std::fs::remove_file(&link);
        #[cfg(unix)]
        let _ = std::os::unix::fs::symlink(&exe, &link);
        #[cfg(windows)]
        let _ = std::os::windows::fs::symlink_file(&exe, &link);
        Ok(())
    }

    pub(super) async fn lua_ls(&self, bin: &Path) -> Step {
        self.gate()?;
        let release = self.latest("LuaLS/lua-language-server").await?;
        let tag = release
            .get("tag_name")
            .and_then(Value::as_str)
            .ok_or_else(|| failed("the latest lua-language-server release has no tag"))?;
        let name = lua_asset(self.platform, tag).ok_or_else(|| unsupported(self.platform))?;
        let asset = find(&release, &name)?;
        let exe = server::lua_ls_bin(bin, self.platform);
        // The whole tree (meta/, locale/, …) is kept in its own directory.
        let dir = exe
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| failed("invalid install directory"))?;
        self.unpack(
            bin,
            &asset.url,
            &name,
            format_of(&name, "-xzf"),
            Place::As(dir),
        )
        .await?;
        present(&exe)?;
        self.chmod(&exe);
        Ok(())
    }

    pub(super) async fn texlab(&self, bin: &Path) -> Step {
        self.gate()?;
        let release = self.latest("latex-lsp/texlab").await?;
        let version = release
            .get("tag_name")
            .and_then(Value::as_str)
            .map(|tag| tag.replacen('v', "", 1))
            .unwrap_or_default();
        if version.is_empty() {
            return Err(failed("the latest texlab release has no tag"));
        }
        let name = texlab_asset(self.platform);
        let asset = find(&release, &name)?;
        self.unpack(
            bin,
            &asset.url,
            &name,
            format_of(&name, "-xzf"),
            Place::Into(bin),
        )
        .await?;
        let exe = bin.join(self.platform.exe("texlab"));
        present(&exe)?;
        self.chmod(&exe);
        Ok(())
    }

    pub(super) async fn tinymist(&self, bin: &Path) -> Step {
        self.gate()?;
        let release = self.latest("Myriad-Dreamin/tinymist").await?;
        let name = tinymist_asset(self.platform);
        let asset = find(&release, &name)?;
        let format = if name.ends_with(".zip") {
            Format::Zip
        } else {
            Format::Tar {
                flag: "-xzf",
                strip: true,
            }
        };
        self.unpack(bin, &asset.url, &name, format, Place::Into(bin))
            .await?;
        let exe = bin.join(self.platform.exe("tinymist"));
        present(&exe)?;
        self.chmod(&exe);
        Ok(())
    }

    pub(super) async fn terraform(&self, bin: &Path) -> Step {
        self.gate()?;
        let url = format!(
            "{}/v1/releases/terraform-ls/latest",
            self.endpoints.hashicorp
        );
        let release = self.json(&url).await?;
        let (arch, os) = terraform_build(self.platform);
        let field =
            |build: &Value, key: &str| build.get(key).and_then(Value::as_str).map(str::to_string);
        let builds = release.get("builds").and_then(Value::as_array);
        let download = builds
            .into_iter()
            .flatten()
            .find(|build| {
                field(build, "arch").as_deref() == Some(arch)
                    && field(build, "os").as_deref() == Some(os)
            })
            .and_then(|build| field(build, "url"))
            .filter(|url| !url.is_empty())
            .ok_or_else(|| unsupported(self.platform))?;
        self.unpack(
            bin,
            &download,
            "terraform-ls.zip",
            Format::Zip,
            Place::Into(bin),
        )
        .await?;
        let exe = bin.join(self.platform.exe("terraform-ls"));
        present(&exe)?;
        self.chmod(&exe);
        Ok(())
    }

    pub(super) async fn kotlin_ls(&self, bin: &Path) -> Step {
        self.gate()?;
        let release = self.latest("Kotlin/kotlin-lsp").await?;
        let name = release
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let version = name.strip_prefix('v').unwrap_or(name);
        if version.is_empty() {
            return Err(failed("the latest kotlin-lsp release has no name"));
        }
        let asset =
            kotlin_asset(self.platform, version).ok_or_else(|| unsupported(self.platform))?;
        let url = format!("{}/kotlin-lsp/{version}/{asset}", self.endpoints.jetbrains);
        let dist = bin.join("kotlin-ls");
        self.unpack(bin, &url, "kotlin-ls.zip", Format::Zip, Place::Into(&dist))
            .await?;
        let script = dist.join(if self.platform.os == "win32" {
            "kotlin-lsp.cmd"
        } else {
            "kotlin-lsp.sh"
        });
        present(&script)?;
        self.chmod(&script);
        Ok(())
    }

    pub(super) async fn jdtls(&self, env: Env<'_>, bin: &Path) -> Step {
        let java = self.need(env, "java")?;
        let version = util::run(&java, &["-version"], RunOptions::default()).await;
        if server::java_major(&version.stderr).is_none_or(|major| major < 21) {
            return Err(unavailable("Java 21 or newer is required"));
        }
        self.gate()?;
        let url = format!(
            "{}/downloads/download.php?file=/jdtls/snapshots/jdt-language-server-latest.tar.gz",
            self.endpoints.eclipse
        );
        let dist = bin.join("jdtls");
        let format = Format::Tar {
            flag: "-xzf",
            strip: false,
        };
        self.unpack(bin, &url, "release.tar.gz", format, Place::Into(&dist))
            .await?;
        present(&dist.join("plugins"))
    }

    // Source archive of vscode-eslint, built with npm.
    pub(super) async fn eslint(&self, env: Env<'_>, bin: &Path) -> Step {
        util::resolve_module("eslint", &env.ctx.directory)
            .ok_or_else(|| unavailable("eslint is not installed in the project"))?;
        self.gate()?;
        let npm = self.need(env, if cfg!(windows) { "npm.cmd" } else { "npm" })?;
        let url = format!(
            "{}/microsoft/vscode-eslint/archive/refs/heads/main.zip",
            self.endpoints.github
        );
        // The archive holds `vscode-eslint-main/`, which the source renames to `vscode-eslint`.
        let unpacked = bin.join("vscode-eslint-main");
        self.unpack(
            bin,
            &url,
            "vscode-eslint.zip",
            Format::Zip,
            Place::Into(bin),
        )
        .await?;
        let dir = bin.join("vscode-eslint");
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|e| failed(format!("{}: {e}", dir.display())))?;
        }
        std::fs::rename(&unpacked, &dir).map_err(|e| failed(format!("{}: {e}", dir.display())))?;
        self.tool(&npm, &["install"], Some(&dir), &[]).await?;
        self.tool(&npm, &["run", "compile"], Some(&dir), &[])
            .await?;
        present(&dir.join("server").join("out").join("eslintServer.js"))
    }

    // Source archive of elixir-ls, built with mix.
    pub(super) async fn elixir_ls(&self, env: Env<'_>, bin: &Path) -> Step {
        self.need(env, "elixir")?;
        self.gate()?;
        let mix = self.need(env, "mix")?;
        let url = format!(
            "{}/elixir-lsp/elixir-ls/archive/refs/heads/master.zip",
            self.endpoints.github
        );
        self.unpack(bin, &url, "elixir-ls.zip", Format::Zip, Place::Into(bin))
            .await?;
        let dir = bin.join("elixir-ls-master");
        // `{ MIX_ENV: "prod", ...process.env }`: an existing value wins.
        let vars: Vec<(&str, String)> = match std::env::var_os("MIX_ENV") {
            Some(_) => Vec::new(),
            None => vec![("MIX_ENV", "prod".to_string())],
        };
        self.tool(&mix, &["deps.get"], Some(&dir), &vars).await?;
        self.tool(&mix, &["compile"], Some(&dir), &vars).await?;
        self.tool(
            &mix,
            &["elixir_ls.release2", "-o", "release"],
            Some(&dir),
            &vars,
        )
        .await?;
        let script = if cfg!(windows) {
            "language_server.bat"
        } else {
            "language_server.sh"
        };
        present(&dir.join("release").join(script))
    }
}
