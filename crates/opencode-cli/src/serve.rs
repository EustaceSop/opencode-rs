// `opencode serve` / `opencode web` (`opencode/src/cli/cmd/serve.ts`, `web.ts`, `cli/network.ts`).
use crate::ui::{self, INFO_BOLD, NORMAL, WARNING_BOLD};
use anyhow::Result;
use opencode_core::{config, paths::Paths};
use opencode_server::ServerOptions;
use opencode_session::Engine;
use serde_json::Value;
use std::sync::Arc;

// Flags as typed; `None` means "not given", so the global config can fill it in.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct NetworkArgs {
    /// port to listen on [default: 0]
    #[arg(long)]
    pub port: Option<u16>,
    /// hostname to listen on [default: 127.0.0.1]
    #[arg(long)]
    pub hostname: Option<String>,
    /// enable mDNS service discovery (defaults hostname to 0.0.0.0)
    #[arg(long, num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    pub mdns: Option<bool>,
    #[arg(long = "no-mdns", hide = true)]
    pub no_mdns: bool,
    /// custom domain name for mDNS service (default: opencode.local)
    #[arg(long = "mdns-domain")]
    pub mdns_domain: Option<String>,
    /// additional domains to allow for CORS
    #[arg(long)]
    pub cors: Vec<String>,
}

impl NetworkArgs {
    // The first network flag that was given, for commands that reject them.
    pub fn given(&self) -> Option<&'static str> {
        [
            ("--port", self.port.is_some()),
            ("--hostname", self.hostname.is_some()),
            ("--mdns", self.mdns.is_some()),
            ("--no-mdns", self.no_mdns),
            ("--mdns-domain", self.mdns_domain.is_some()),
            ("--cors", !self.cors.is_empty()),
        ]
        .into_iter()
        .find_map(|(name, set)| set.then_some(name))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Network {
    pub hostname: String,
    pub port: u16,
    pub mdns: bool,
    pub mdns_domain: String,
    pub cors: Vec<String>,
}

// Explicit flag, then the global config's `server.*`, then the default.
pub fn resolve(args: &NetworkArgs, config: &Value) -> Network {
    let server = config.get("server");
    let text = |key: &str| server.and_then(|s| s.get(key)).and_then(Value::as_str).map(str::to_string);
    let explicit_mdns = if args.no_mdns { Some(false) } else { args.mdns };
    let mdns = explicit_mdns.or_else(|| server.and_then(|s| s.get("mdns")).and_then(Value::as_bool)).unwrap_or(false);
    let hostname = match (&args.hostname, text("hostname")) {
        (Some(hostname), _) => hostname.clone(),
        (None, None) if mdns => "0.0.0.0".to_string(),
        (None, configured) => configured.unwrap_or_else(|| opencode_server::DEFAULT_HOSTNAME.to_string()),
    };
    let port = args.port.or_else(|| server.and_then(|s| s.get("port")).and_then(Value::as_u64).and_then(|port| u16::try_from(port).ok())).unwrap_or(0);
    let mut cors: Vec<String> = server.and_then(|s| s.get("cors")).and_then(Value::as_array).into_iter().flatten().filter_map(|origin| origin.as_str().map(str::to_string)).collect();
    cors.extend(args.cors.iter().cloned());
    Network { hostname, port, mdns, mdns_domain: args.mdns_domain.clone().or_else(|| text("mdnsDomain")).unwrap_or_else(|| "opencode.local".to_string()), cors }
}

async fn listen(paths: &Paths, args: &NetworkArgs) -> Result<(opencode_server::Server, Network)> {
    let network = resolve(args, &config::load_global(paths).unwrap_or(Value::Null));
    let mut options = ServerOptions::from_env();
    options.port = network.port;
    options.hostname = network.hostname.clone();
    options.cors = network.cors.clone();
    let server = opencode_server::serve(options, Arc::new(Engine::new(paths.clone())?)).await?;
    if network.mdns {
        tracing::warn!("mDNS service discovery is not implemented in the Rust build; the server is reachable by address only");
    }
    Ok((server, network))
}

fn password_set() -> bool {
    std::env::var("OPENCODE_SERVER_PASSWORD").is_ok_and(|value| !value.is_empty())
}

pub async fn serve(paths: &Paths, args: &NetworkArgs) -> Result<i32> {
    if !password_set() {
        println!("Warning: OPENCODE_SERVER_PASSWORD is not set; server is unsecured.");
    }
    let (server, _) = listen(paths, args).await?;
    println!("{}", server.listening_line());
    tokio::signal::ctrl_c().await?;
    server.shutdown().await;
    Ok(0)
}

// Non-internal IPv4 addresses, without Docker's 172.* bridges.
fn network_ips() -> Vec<String> {
    let interfaces = if_addrs::get_if_addrs().unwrap_or_default();
    interfaces
        .into_iter()
        .filter(|interface| !interface.is_loopback())
        .filter_map(|interface| match interface.ip() {
            std::net::IpAddr::V4(ip) => Some(ip.to_string()),
            std::net::IpAddr::V6(_) => None,
        })
        .filter(|ip| !ip.starts_with("172."))
        .collect()
}

pub async fn web(paths: &Paths, args: &NetworkArgs) -> Result<i32> {
    if !password_set() {
        ui::println(&format!("{WARNING_BOLD}!  OPENCODE_SERVER_PASSWORD is not set; server is unsecured."));
    }
    let (server, network) = listen(paths, args).await?;
    ui::empty();
    ui::println(&ui::logo("  "));
    ui::empty();
    let port = server.addr.port();
    let open = if network.hostname == "0.0.0.0" {
        let local = format!("http://localhost:{port}");
        ui::println(&format!("{INFO_BOLD}  Local access:       {NORMAL} {local}"));
        for ip in network_ips() {
            ui::println(&format!("{INFO_BOLD}  Network access:     {NORMAL} http://{ip}:{port}"));
        }
        if network.mdns {
            ui::println(&format!("{INFO_BOLD}  mDNS:               {NORMAL} {}:{port}", network.mdns_domain));
        }
        local
    } else {
        // `URL.toString()` ends a bare origin with a slash.
        let url = format!("{}/", server.url().trim_end_matches('/'));
        ui::println(&format!("{INFO_BOLD}  Web interface:     {NORMAL} {url}"));
        url
    };
    ui::println(&format!("{WARNING_BOLD}!  {NORMAL}The Rust build does not embed the web UI: this address serves the HTTP API only."));
    if std::env::var("OPENCODE_TEST_NO_BROWSER").is_err() {
        let _ = opencode_mcp::browser::open(&open).await;
    }
    tokio::signal::ctrl_c().await?;
    server.shutdown().await;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn flags_beat_the_global_config_which_beats_the_defaults() {
        let none = NetworkArgs::default();
        let defaults = resolve(&none, &Value::Null);
        assert_eq!(defaults, Network { hostname: "127.0.0.1".into(), port: 0, mdns: false, mdns_domain: "opencode.local".into(), cors: vec![] });

        let config = json!({"server": {"port": 4100, "hostname": "10.0.0.1", "mdns": true, "mdnsDomain": "team.local", "cors": ["https://a.test"]}});
        let configured = resolve(&none, &config);
        assert_eq!(configured, Network { hostname: "10.0.0.1".into(), port: 4100, mdns: true, mdns_domain: "team.local".into(), cors: vec!["https://a.test".into()] });

        let args = NetworkArgs { port: Some(5000), hostname: Some("localhost".into()), no_mdns: true, mdns_domain: Some("x.local".into()), cors: vec!["https://b.test".into()], ..Default::default() };
        let explicit = resolve(&args, &config);
        assert_eq!(explicit, Network { hostname: "localhost".into(), port: 5000, mdns: false, mdns_domain: "x.local".into(), cors: vec!["https://a.test".into(), "https://b.test".into()] });

        // mDNS widens the default hostname only when nothing names one.
        let mdns = NetworkArgs { mdns: Some(true), ..Default::default() };
        assert_eq!(resolve(&mdns, &Value::Null).hostname, "0.0.0.0");
        assert_eq!(resolve(&mdns, &json!({"server": {"hostname": "10.0.0.1"}})).hostname, "10.0.0.1");
        assert_eq!(resolve(&none, &json!({"server": {"mdns": true}})).hostname, "0.0.0.0");
        assert_eq!((mdns.given(), none.given(), args.given()), (Some("--mdns"), None, Some("--port")));
    }
}
