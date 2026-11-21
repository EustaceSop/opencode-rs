//! `McpBrowser.open`: launch the system browser; failure only if the launcher exits non-zero within 500 ms.

use std::process::Stdio;
use std::time::Duration;

use reqwest::Url;
use tokio::process::Command;

pub async fn open(url: &str) -> Result<(), String> {
    let parsed = Url::parse(url)
        .ok()
        .filter(|u| u.scheme() == "http" || u.scheme() == "https")
        .ok_or_else(|| format!("Only http and https links can be opened in the browser: {url}"))?;
    let mut command = if cfg!(windows) {
        let mut command = Command::new("rundll32");
        command.arg("url.dll,FileProtocolHandler");
        command
    } else if cfg!(target_os = "macos") {
        Command::new("open")
    } else {
        Command::new("xdg-open")
    };
    let mut child = command
        .arg(parsed.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    match tokio::time::timeout(Duration::from_millis(500), child.wait()).await {
        Ok(Ok(status)) if !status.success() => Err(match status.code() {
            Some(code) => format!("Browser open failed with exit code {code}"),
            None => "Browser open failed".to_string(),
        }),
        Ok(Err(error)) => Err(error.to_string()),
        _ => Ok(()),
    }
}
