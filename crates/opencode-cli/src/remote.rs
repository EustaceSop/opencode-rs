// The terminal UI against a server: `opencode attach <url>` (`cli/cmd/attach.ts`) and the default command's network
// flags, which start this build's server beside the UI (`cli/cmd/tui.ts`, worker RPC `server`).
use crate::serve::Network;
use crate::ui::fail;
use anyhow::Result;
use opencode_client::{Client, Credentials};
use opencode_core::paths::Paths;
use opencode_server::ServerOptions;
use opencode_session::Engine;
use opencode_tui::TuiOptions;
use std::path::PathBuf;
use std::sync::Arc;

pub struct AttachArgs {
    pub url: String,
    pub dir: Option<String>,
    pub resume: bool,
    pub session: Option<String>,
    pub fork: bool,
    pub password: Option<String>,
    pub username: Option<String>,
    pub mini: bool,
    pub replay: bool,
    pub no_replay: bool,
    pub replay_limit: Option<f64>,
}

// `Schema.decodeUnknownSync(SessionID)`: ids start with "ses".
pub fn check_session_id(session_id: &str) -> Result<()> {
    if session_id.starts_with("ses") {
        return Ok(());
    }
    Err(fail(format!("Invalid session ID: Expected a string starting with \"ses\", got \"{session_id}\"")))
}

// `validateSession`: a `--session` must name a session the server has, before the UI starts.
pub async fn validate_session(client: &Client, session_id: Option<&str>) -> Result<()> {
    let Some(session_id) = session_id else { return Ok(()) };
    check_session_id(session_id)?;
    client.session(session_id).await.map_err(|error| fail(error.to_string()))?;
    Ok(())
}

// The same check against the engine this process runs.
pub fn validate_local(paths: &Paths, session_id: &str) -> Result<()> {
    check_session_id(session_id)?;
    if Engine::new(paths.clone())?.store.get_session(session_id)?.is_none() {
        return Err(fail(format!("Session not found: {session_id}")));
    }
    Ok(())
}

// A wildcard listen address is reached through loopback.
fn connect_url(server: &opencode_server::Server) -> String {
    match server.hostname.as_str() {
        "0.0.0.0" | "::" | "[::]" => format!("http://127.0.0.1:{}", server.addr.port()),
        _ => server.url(),
    }
}

// Starts the server the network flags ask for. Its password and username come from the environment, as the
// original server's do; the UI then talks to it like any client, with `ServerAuth.headers()`. The original prints
// nothing here: the server's listen log is off and the UI takes the screen.
pub async fn start_server(engine: Arc<Engine>, network: &Network, directory: &str) -> Result<(opencode_server::Server, Client)> {
    let mut options = ServerOptions::from_env();
    options.hostname = network.hostname.clone();
    options.port = network.port;
    options.cors = network.cors.clone();
    options.directory = Some(directory.to_string());
    let server = opencode_server::serve(options, engine).await?;
    if network.mdns {
        tracing::warn!("mDNS service discovery is not implemented in the Rust build; the server is reachable by address only");
    }
    tracing::info!(url = %server.url(), "server started for the terminal UI");
    let client = Client::new(&connect_url(&server)).with_directory(Some(directory.to_string())).with_credentials(Credentials::from_env(None, None));
    Ok((server, client))
}

pub async fn tui_with_server(paths: &Paths, network: Network, options: TuiOptions) -> Result<i32> {
    let directory = options.directory.to_string_lossy().into_owned();
    let (server, client) = start_server(Arc::new(Engine::new(paths.clone())?), &network, &directory).await?;
    let result = async {
        validate_session(&client, options.session.as_deref()).await?;
        opencode_tui::run_remote(options, client).await
    }
    .await;
    server.shutdown().await;
    result?;
    Ok(0)
}

pub async fn attach(paths: &Paths, args: AttachArgs) -> Result<i32> {
    if args.replay {
        return Err(fail("--replay is not supported; replay is enabled by default"));
    }
    // An existing local directory becomes the working directory; anything else is passed to the server as is.
    let directory = args.dir.map(|dir| match std::env::set_current_dir(&dir) {
        Ok(()) => std::env::current_dir().map_or(dir, |cwd| cwd.to_string_lossy().into_owned()),
        Err(_) => dir,
    });
    if args.mini {
        let options = crate::mini::Options {
            directory,
            attach: Some(args.url),
            password: args.password,
            username: args.username,
            resume: args.resume,
            session: args.session,
            fork: args.fork,
            replay: !args.no_replay,
            replay_limit: args.replay_limit,
            ..Default::default()
        };
        return crate::mini::run(paths, options).await;
    }
    let unsupported = [("--no-replay", args.no_replay), ("--replay-limit", args.replay_limit.is_some())];
    if let Some((flag, _)) = unsupported.into_iter().find(|(_, set)| *set) {
        return Err(fail(format!("{flag} requires --mini")));
    }
    if args.fork && !args.resume && args.session.is_none() {
        return Err(fail("--fork requires --continue or --session"));
    }
    let client = Client::new(&args.url).with_directory(directory.clone()).with_credentials(Credentials::from_env(args.password, args.username));
    validate_session(&client, args.session.as_deref()).await?;
    let options = TuiOptions { directory: directory.map(PathBuf::from).unwrap_or_default(), session: args.session, continue_last: args.resume, fork: args.fork, ..Default::default() };
    opencode_tui::run_remote(options, client).await?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencode_storage::Store;

    fn offline(root: &std::path::Path) -> Arc<Engine> {
        let paths = Paths { home: root.join("home"), data: root.join("data"), cache: root.join("cache"), config: root.join("config"), state: root.join("state"), tmp: root.join("tmp") };
        let source = opencode_llm::catalog::Source { disabled: true, ..opencode_llm::catalog::Source::from_env(&paths) };
        let engine = Engine::with_store(paths, Store::open_in_memory().unwrap());
        engine.set_catalog_source(source);
        Arc::new(engine)
    }

    #[test]
    fn session_ids_are_checked_before_any_request() {
        assert!(check_session_id("ses_abc").is_ok());
        let error = check_session_id("abc").unwrap_err();
        assert_eq!(error.to_string(), "Invalid session ID: Expected a string starting with \"ses\", got \"abc\"");
    }

    #[tokio::test]
    async fn the_network_flags_serve_the_api_beside_the_ui() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let network = Network { hostname: "127.0.0.1".into(), port: 0, mdns: false, mdns_domain: "opencode.local".into(), cors: vec!["https://a.test".into()] };
        let (server, client) = start_server(offline(dir.path()), &network, &project.to_string_lossy()).await.unwrap();
        assert!(client.health().await.unwrap().healthy);
        assert_eq!(client.path().await.unwrap().directory, project.to_string_lossy());
        let session = client.create_session(&Default::default()).await.unwrap();
        assert!(validate_session(&client, Some(&session.id)).await.is_ok());
        let missing = validate_session(&client, Some("ses_missing")).await.unwrap_err();
        assert_eq!(missing.to_string(), "Session not found: ses_missing");
        server.shutdown().await;
    }
}
