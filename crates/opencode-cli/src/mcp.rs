// `opencode mcp ...` (`opencode/src/cli/cmd/mcp.ts`), without the interactive pickers.
use crate::ui::{self, DIM};
use anyhow::{Result, bail};
use opencode_core::config::{self, Info, McpConfig, McpOAuthSetting, McpServer};
use opencode_core::paths::Paths;
use opencode_mcp::{AuthStatus, AuthStore, Status};
use opencode_session::Engine;
use serde_json::{Value, json};
use std::io::{IsTerminal, Write};

fn load(paths: &Paths) -> Result<Info> {
    let cwd = std::env::current_dir()?;
    let opts = config::LoadOptions { worktree: config::find_worktree(&cwd), cwd };
    Ok(config::load_with(paths, &opts, &opencode_core::flag::process_env)?.info()?)
}

fn configured(info: &Info) -> Vec<(&String, &McpServer)> {
    info.mcp
        .iter()
        .filter_map(|(name, entry)| match entry {
            McpConfig::Server(server) => Some((name, server)),
            McpConfig::Toggle { .. } => None,
        })
        .collect()
}

// Remote servers unless `oauth: false`.
fn oauth_servers(info: &Info) -> Vec<(&String, &String)> {
    configured(info)
        .into_iter()
        .filter_map(|(name, server)| match server {
            McpServer::Remote { url, oauth, .. } if !matches!(oauth, Some(McpOAuthSetting::Toggle(false))) => Some((name, url)),
            _ => None,
        })
        .collect()
}

fn type_hint(server: &McpServer) -> String {
    match server {
        McpServer::Remote { url, .. } => url.clone(),
        McpServer::Local { command, .. } => command.join(" "),
    }
}

pub async fn list(paths: &Paths) -> Result<()> {
    ui::empty();
    ui::intro("MCP Servers");
    let info = load(paths)?;
    let servers = configured(&info);
    if servers.is_empty() {
        ui::log("▲", "No MCP servers configured");
        ui::outro("Add servers with: opencode mcp add");
        return Ok(());
    }
    let engine = Engine::new(paths.clone())?;
    let manager = engine.mcp(&std::env::current_dir()?).await?;
    let statuses = manager.status();
    for (name, server) in &servers {
        let has_oauth = matches!(server, McpServer::Remote { oauth: Some(McpOAuthSetting::Config(_)), .. });
        let (icon, text, hint) = match statuses.get(*name) {
            None => ("○", "not initialized", String::new()),
            Some(Status::Connected) if has_oauth && manager.has_stored_tokens(name).await => ("✓", "connected", " (OAuth)".to_string()),
            Some(Status::Connected) => ("✓", "connected", String::new()),
            Some(Status::Disabled) => ("○", "disabled", String::new()),
            Some(Status::NeedsAuth) => ("⚠", "needs authentication", String::new()),
            Some(Status::NeedsClientRegistration { error }) => ("✗", "needs client registration", format!("\n    {error}")),
            Some(Status::Failed { error }) => ("✗", "failed", format!("\n    {error}")),
        };
        ui::log("●", &format!("{icon} {name} {DIM}{text}{hint}\n    {DIM}{}", type_hint(server)));
    }
    engine.shutdown().await;
    ui::outro(&format!("{} server(s)", servers.len()));
    Ok(())
}

fn pairs(values: &[String], kind: &str) -> Result<serde_json::Map<String, Value>> {
    let mut out = serde_json::Map::new();
    for entry in values {
        match entry.split_once('=') {
            Some((key, value)) if !key.is_empty() => {
                out.insert(key.to_string(), value.into());
            }
            _ => bail!("Invalid {kind}: {entry}. Expected KEY=VALUE"),
        }
    }
    Ok(out)
}

pub fn add(paths: &Paths, name: Option<String>, url: Option<String>, env: Vec<String>, header: Vec<String>, command: Vec<String>) -> Result<()> {
    let Some(name) = name else {
        bail!("A server name is required for non-interactive MCP configuration");
    };
    if url.is_some() == !command.is_empty() {
        bail!("Provide either --url <url> or a command after --");
    }
    if let Some(url) = &url {
        if reqwest::Url::parse(url).is_err() {
            bail!("Invalid URL: {url}");
        }
        if !env.is_empty() {
            bail!("--env is only valid for local MCP servers");
        }
    }
    if !command.is_empty() && !header.is_empty() {
        bail!("--header is only valid for remote MCP servers");
    }
    let environment = pairs(&env, "environment variable")?;
    let headers = pairs(&header, "HTTP header")?;
    let mut entry = match &url {
        Some(url) => json!({"type": "remote", "url": url}),
        None => json!({"type": "local", "command": command}),
    };
    if !headers.is_empty() {
        entry["headers"] = headers.into();
    }
    if !environment.is_empty() {
        entry["environment"] = environment.into();
    }
    // The global config: an existing opencode.json or opencode.jsonc, else a new opencode.json.
    let candidates = ["opencode.json", "opencode.jsonc"].map(|file| paths.config.join(file));
    let file = candidates.iter().find(|file| file.exists()).unwrap_or(&candidates[0]);
    let text = std::fs::read_to_string(file).unwrap_or_else(|_| "{}".to_string());
    let updated = config::set_jsonc(&text, &["mcp", &name], &entry, &file.to_string_lossy())?;
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(file, updated)?;
    ui::log("◆", &format!("MCP server \"{name}\" added to {}", file.display()));
    Ok(())
}

fn auth_label(status: AuthStatus) -> (&'static str, &'static str) {
    match status {
        AuthStatus::Authenticated => ("✓", "authenticated"),
        AuthStatus::Expired => ("⚠", "expired"),
        AuthStatus::NotAuthenticated => ("✗", "not authenticated"),
    }
}

pub async fn auth_list(paths: &Paths) -> Result<()> {
    ui::empty();
    ui::intro("MCP OAuth Status");
    let info = load(paths)?;
    let servers = oauth_servers(&info);
    if servers.is_empty() {
        ui::log("▲", "No OAuth-capable MCP servers configured");
        ui::outro("Done");
        return Ok(());
    }
    let engine = Engine::new(paths.clone())?;
    let manager = engine.mcp(&std::env::current_dir()?).await?;
    for (name, url) in &servers {
        let (icon, text) = auth_label(manager.auth_status(name).await);
        ui::log("●", &format!("{icon} {name} {DIM}{text}\n    {DIM}{url}"));
    }
    engine.shutdown().await;
    ui::outro(&format!("{} OAuth-capable server(s)", servers.len()));
    Ok(())
}

fn confirm(message: &str) -> bool {
    if !std::io::stdin().is_terminal() {
        return false;
    }
    eprint!("◆  {message} (y/N) ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

pub async fn auth(paths: &Paths, name: Option<String>) -> Result<()> {
    ui::empty();
    ui::intro("MCP OAuth Authentication");
    let info = load(paths)?;
    let servers = oauth_servers(&info);
    if servers.is_empty() {
        ui::log("▲", "No OAuth-capable MCP servers configured");
        ui::log("●", "Remote MCP servers support OAuth by default. Add a remote server in opencode.json:");
        ui::log("●", "\n  \"mcp\": {\n    \"my-server\": {\n      \"type\": \"remote\",\n      \"url\": \"https://example.com/mcp\"\n    }\n  }");
        ui::outro("Done");
        return Ok(());
    }
    // The original offers a picker here; without one the name must be given unless there is only one candidate.
    let name = match (name, servers.as_slice()) {
        (Some(name), _) => name,
        (None, [(only, _)]) => (*only).clone(),
        (None, _) => {
            let names: Vec<&str> = servers.iter().map(|(name, _)| name.as_str()).collect();
            ui::log("■", &format!("Select an MCP server to authenticate: opencode mcp auth <name> ({})", names.join(", ")));
            ui::outro("Done");
            return Ok(());
        }
    };
    let Some((_, server)) = configured(&info).into_iter().find(|(key, _)| **key == name) else {
        ui::log("■", &format!("MCP server not found: {name}"));
        ui::outro("Done");
        return Ok(());
    };
    let McpServer::Remote { url, oauth, .. } = server else {
        ui::log("■", &format!("MCP server {name} is not an OAuth-capable remote server"));
        ui::outro("Done");
        return Ok(());
    };
    if matches!(oauth, Some(McpOAuthSetting::Toggle(false))) {
        ui::log("■", &format!("MCP server {name} is not an OAuth-capable remote server"));
        ui::outro("Done");
        return Ok(());
    }
    let engine = Engine::new(paths.clone())?;
    let manager = engine.mcp(&std::env::current_dir()?).await?;
    match manager.auth_status(&name).await {
        AuthStatus::Authenticated => {
            if !confirm(&format!("{name} already has valid credentials. Re-authenticate?")) {
                engine.shutdown().await;
                ui::outro("Cancelled");
                return Ok(());
            }
        }
        AuthStatus::Expired => ui::log("▲", &format!("{name} has expired credentials. Re-authenticating...")),
        AuthStatus::NotAuthenticated => {}
    }
    ui::log("◒", "Starting OAuth flow...");
    let announce = |url: &str| {
        ui::log("◇", "Authorize in your browser:");
        ui::log("●", url);
        ui::log("◒", "Waiting for authorization...");
    };
    match manager.authenticate(&name, Some(&announce)).await {
        Ok(Status::Connected) => ui::log("◇", "Authentication successful!"),
        Ok(Status::NeedsClientRegistration { error }) => {
            ui::log("▲", "Authentication failed");
            ui::log("■", &error);
            ui::log("●", "Add clientId to your MCP server config:");
            ui::log(
                "●",
                &format!(
                    "\n  \"mcp\": {{\n    \"{name}\": {{\n      \"type\": \"remote\",\n      \"url\": \"{url}\",\n      \"oauth\": {{\n        \"clientId\": \"your-client-id\",\n        \"clientSecret\": \"your-client-secret\"\n      }}\n    }}\n  }}"
                ),
            );
        }
        Ok(Status::Failed { error }) => {
            ui::log("▲", "Authentication failed");
            ui::log("■", &error);
        }
        Ok(other) => {
            let status = serde_json::to_value(&other).ok().and_then(|v| v["status"].as_str().map(str::to_string)).unwrap_or_default();
            ui::log("▲", &format!("Unexpected status: {status}"));
        }
        Err(error) => {
            ui::log("▲", "Authentication failed");
            ui::log("■", &error.to_string());
        }
    }
    engine.shutdown().await;
    ui::outro("Done");
    Ok(())
}

pub async fn logout(paths: &Paths, name: Option<String>) -> Result<()> {
    ui::empty();
    ui::intro("MCP OAuth Logout");
    let store = AuthStore::new(&paths.data);
    let credentials = store.all().await;
    if credentials.is_empty() {
        ui::log("▲", "No MCP OAuth credentials stored");
        ui::outro("Done");
        return Ok(());
    }
    // As with `auth`: no picker, so the name is required unless only one server has credentials.
    let name = match (name, credentials.len()) {
        (Some(name), _) => name,
        (None, 1) => credentials.keys().next().cloned().unwrap_or_default(),
        (None, _) => {
            let names: Vec<&str> = credentials.keys().map(String::as_str).collect();
            ui::log("■", &format!("Select an MCP server to logout: opencode mcp logout <name> ({})", names.join(", ")));
            ui::outro("Done");
            return Ok(());
        }
    };
    if !credentials.contains_key(&name) {
        ui::log("■", &format!("No credentials found for: {name}"));
        ui::outro("Done");
        return Ok(());
    }
    store.remove(&name).await;
    opencode_mcp::callback::cancel_pending(&name);
    ui::log("◆", &format!("Removed OAuth credentials for {name}"));
    ui::outro("Done");
    Ok(())
}

fn iso(seconds: f64) -> String {
    chrono::DateTime::from_timestamp_millis((seconds * 1000.0) as i64).map_or_else(String::new, |date| date.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

// `opencode mcp debug <name>`: stored OAuth state plus one unauthenticated `initialize` probe.
// The original goes on to run OAuth discovery through its MCP SDK transport on a 401; this port reports the stored client only.
pub async fn debug(paths: &Paths, name: &str, version: &str) -> Result<()> {
    let info = load(paths)?;
    ui::empty();
    ui::intro("MCP OAuth Debug");
    let Some((_, server)) = configured(&info).into_iter().find(|(key, _)| key.as_str() == name) else {
        ui::failure(&format!("MCP server not found: {name}"));
        ui::outro("Done");
        return Ok(());
    };
    let McpServer::Remote { url, headers, oauth, .. } = server else {
        ui::failure(&format!("MCP server {name} is not a remote server"));
        ui::outro("Done");
        return Ok(());
    };
    if matches!(oauth, Some(McpOAuthSetting::Toggle(false))) {
        ui::warn(&format!("MCP server {name} has OAuth explicitly disabled"));
        ui::outro("Done");
        return Ok(());
    }
    ui::info(&format!("Server: {name}"));
    ui::info(&format!("URL: {url}"));

    let engine = Engine::new(paths.clone())?;
    let manager = engine.mcp(&std::env::current_dir()?).await?;
    let (icon, text) = auth_label(manager.auth_status(name).await);
    engine.shutdown().await;
    ui::info(&format!("Auth status: {icon} {text}"));
    let entry = AuthStore::new(&paths.data).get(name).await;
    if let Some(tokens) = entry.as_ref().and_then(|entry| entry.tokens.as_ref()) {
        let token: Vec<char> = tokens.access_token.chars().collect();
        let masked = if token.len() > 8 { format!("{}***{}", token[..4].iter().collect::<String>(), token[token.len() - 4..].iter().collect::<String>()) } else { "***".to_string() };
        ui::info(&format!("  Access token: {masked}"));
        if let Some(expires) = tokens.expires_at.as_ref().and_then(serde_json::Number::as_f64).filter(|expires| *expires != 0.0) {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
            ui::info(&format!("  Expires: {} {}", iso(expires), if expires < now { "(EXPIRED)" } else { "" }));
        }
        if tokens.refresh_token.as_ref().is_some_and(|token| !token.is_empty()) {
            ui::info("  Refresh token: present");
        }
    }
    let client = entry.as_ref().and_then(|entry| entry.client_info.as_ref());
    if let Some(client) = client {
        ui::info(&format!("  Client ID: {}", client.client_id));
        if let Some(expires) = client.client_secret_expires_at.as_ref().and_then(serde_json::Number::as_f64).filter(|expires| *expires != 0.0) {
            ui::info(&format!("  Client secret expires: {}", iso(expires)));
        }
    }

    let body = json!({
        "jsonrpc": "2.0",
        "method": "initialize",
        "params": {"protocolVersion": opencode_mcp::client::LATEST_PROTOCOL_VERSION, "capabilities": {}, "clientInfo": {"name": "opencode-debug", "version": version}},
        "id": 1,
    });
    let mut request = reqwest::Client::new().post(url);
    for (key, value) in headers {
        request = request.header(key, value);
    }
    let sent = request.header("Content-Type", "application/json").header("Accept", "application/json, text/event-stream").body(body.to_string()).send().await;
    let response = match sent {
        Ok(response) => response,
        Err(error) => {
            ui::done("Connection failed", false);
            ui::failure(&format!("Error: {error}"));
            ui::outro("Debug complete");
            return Ok(());
        }
    };
    let status = response.status();
    ui::done(&format!("HTTP response: {} {}", status.as_u16(), status.canonical_reason().unwrap_or_default()), true);
    if let Some(challenge) = response.headers().get("www-authenticate").and_then(|value| value.to_str().ok()) {
        ui::info(&format!("WWW-Authenticate: {challenge}"));
    }
    if status.as_u16() == 401 {
        ui::info("Initial unauthenticated check returned 401, so this server requires OAuth");
        match client {
            Some(client) => ui::info(&format!("Client ID available: {}", client.client_id)),
            None => ui::info("No client ID - dynamic registration will be attempted"),
        }
    } else if status.is_success() {
        ui::success("Server responded successfully (no auth required or already authenticated)");
        let text = response.text().await.unwrap_or_default();
        if let Some(server) = serde_json::from_str::<Value>(&text).ok().and_then(|json| json["result"].get("serverInfo").cloned()) {
            ui::info(&format!("Server info: {server}"));
        }
    } else {
        ui::warn(&format!("Unexpected status: {}", status.as_u16()));
        let text = response.text().await.unwrap_or_default();
        if !text.is_empty() {
            ui::info(&format!("Response body: {}", text.chars().take(500).collect::<String>()));
        }
    }
    ui::outro("Debug complete");
    Ok(())
}
