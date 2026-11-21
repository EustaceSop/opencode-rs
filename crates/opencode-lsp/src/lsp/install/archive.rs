//! Download and extraction. Archives are unpacked in a staging directory under `<cache>/bin` and the
//! results renamed into place, so a half-written install is never visible at the final path.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::Value;
use tokio::io::AsyncWriteExt;

use super::{Installer, Step, failed, tail};
use crate::util::{self, RunOptions};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Format {
    /// `Archive.extractZip`.
    Zip,
    /// `tar <flag> <archive>`, optionally with `--strip-components=1`.
    Tar { flag: &'static str, strip: bool },
}

pub(super) enum Place<'a> {
    /// Move every top-level entry of the archive into this directory, replacing same-named entries.
    Into(&'a Path),
    /// The archive's content becomes this directory.
    As(&'a Path),
}

struct Stage(PathBuf);

impl Drop for Stage {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn io(path: &Path) -> impl Fn(std::io::Error) -> super::Stop + '_ {
    move |err| failed(format!("{}: {err}", path.display()))
}

fn remove(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(_) => Ok(()),
    }
}

// Freshly extracted files can be held open briefly by scanners on Windows.
async fn rename(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut attempt = 0;
    loop {
        remove(to)?;
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(err) if attempt >= 4 => return Err(err),
            Err(_) => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

async fn extract(archive: &Path, name: &str, out: &Path, format: Format) -> Step {
    let result = match format {
        Format::Zip if cfg!(windows) => {
            // PowerShell providers reject verbatim (`\\?\`) paths.
            let quote = |path: &Path| {
                util::strip_verbatim(path.to_path_buf())
                    .to_string_lossy()
                    .replace('\'', "''")
            };
            let script = format!(
                "$ErrorActionPreference = 'Stop'; $global:ProgressPreference = 'SilentlyContinue'; Expand-Archive -Path '{}' -DestinationPath '{}' -Force",
                quote(archive),
                quote(out)
            );
            let args = ["-NoProfile", "-NonInteractive", "-Command", &script];
            util::run("powershell", &args, RunOptions::default()).await
        }
        Format::Zip => {
            let (zip, dest) = (archive.to_string_lossy(), out.to_string_lossy());
            let args = ["-o", "-q", &*zip, "-d", &*dest];
            util::run("unzip", &args, RunOptions::default()).await
        }
        Format::Tar { flag, strip } => {
            // Relative to the output directory: GNU tar reads `C:\…` as a remote host.
            let relative = format!("../{name}");
            let mut args = vec![flag, relative.as_str()];
            if strip {
                args.push("--strip-components=1");
            }
            let opts = RunOptions {
                cwd: Some(out),
                ..Default::default()
            };
            util::run("tar", &args, opts).await
        }
    };
    if result.code != 0 {
        // PowerShell leads with the message, tar ends with it.
        let detail = if cfg!(windows) && format == Format::Zip {
            let first = result.stderr.lines().find(|line| !line.trim().is_empty());
            first.unwrap_or("").trim().chars().take(300).collect()
        } else {
            tail(&result.stderr)
        };
        return Err(failed(format!("extracting {name} failed: {detail}")));
    }
    Ok(())
}

impl Installer {
    fn http(&self) -> &reqwest::Client {
        self.http.get_or_init(|| {
            let builder = reqwest::Client::builder().user_agent("opencode");
            // A local listener must not be routed through a system proxy.
            let builder = if cfg!(test) {
                builder.no_proxy()
            } else {
                builder
            };
            builder.build().unwrap_or_default()
        })
    }

    async fn get(&self, url: &str) -> Step<reqwest::Response> {
        let response = self
            .http()
            .get(url)
            .send()
            .await
            .map_err(|err| failed(format!("GET {url}: {}", err.without_url())))?;
        if !response.status().is_success() {
            return Err(failed(format!("GET {url}: HTTP {}", response.status())));
        }
        Ok(response)
    }

    pub(super) async fn json(&self, url: &str) -> Step<Value> {
        let body = self
            .get(url)
            .await?
            .bytes()
            .await
            .map_err(|err| failed(format!("GET {url}: {}", err.without_url())))?;
        serde_json::from_slice(&body).map_err(|err| failed(format!("GET {url}: {err}")))
    }

    async fn download(&self, url: &str, dest: &Path) -> Step<u64> {
        let mut response = self.get(url).await?;
        let mut file = tokio::fs::File::create(dest).await.map_err(io(dest))?;
        let mut size = 0u64;
        loop {
            let chunk = response
                .chunk()
                .await
                .map_err(|err| failed(format!("GET {url}: {}", err.without_url())))?;
            let Some(chunk) = chunk else { break };
            size += chunk.len() as u64;
            file.write_all(&chunk).await.map_err(io(dest))?;
        }
        file.flush().await.map_err(io(dest))?;
        Ok(size)
    }

    /// Fetch `url` as `name`, extract it and move the result into place.
    pub(super) async fn unpack(
        &self,
        bin: &Path,
        url: &str,
        name: &str,
        format: Format,
        place: Place<'_>,
    ) -> Step {
        std::fs::create_dir_all(bin).map_err(io(bin))?;
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let stage = Stage(bin.join(format!(".tmp-{}-{seq}", std::process::id())));
        let out = stage.0.join("out");
        std::fs::create_dir_all(&out).map_err(io(&out))?;
        let archive = stage.0.join(name);
        if self.download(url, &archive).await? == 0 {
            return Err(failed(format!("GET {url}: empty response")));
        }
        extract(&archive, name, &out, format).await?;
        match place {
            Place::As(dest) => rename(&out, dest).await.map_err(io(dest))?,
            Place::Into(dest) => {
                std::fs::create_dir_all(dest).map_err(io(dest))?;
                for entry in std::fs::read_dir(&out).map_err(io(&out))?.flatten() {
                    let target = dest.join(entry.file_name());
                    rename(&entry.path(), &target).await.map_err(io(&target))?;
                }
            }
        }
        Ok(())
    }
}
