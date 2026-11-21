// HTTP server speaking the original's legacy (v1) API on top of the Rust engine.
pub mod bus;
pub mod error;
mod host;
mod pty;
mod routes;
mod state;
mod ui;
mod v2;

use axum::Router;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use opencode_session::{Engine, ServerInfo, ServerLauncher};
use routes::{event, file, instance, mcp, provider, pty as terminal, session, vcs, workspace};
use state::{AppState, Credentials, Inner};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

pub use routes::instance::openapi;
pub use routes::provider::model_objects;
pub use state::{decode_component, select_directory, session_id_from_path};

pub const DEFAULT_HOSTNAME: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 4096;
pub const DEFAULT_USERNAME: &str = "opencode";

#[derive(Debug, Clone)]
pub struct ServerOptions {
    pub hostname: String,
    // 0 tries 4096 first, then an OS-assigned port.
    pub port: u16,
    // Auth is on iff this is set and non-empty.
    pub password: Option<String>,
    pub username: Option<String>,
    // Extra allowed CORS origins (exact match).
    pub cors: Vec<String>,
    // Directory used when a request names none; defaults to the process cwd.
    pub directory: Option<String>,
    pub heartbeat: Duration,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            hostname: DEFAULT_HOSTNAME.to_string(),
            port: 0,
            password: None,
            username: None,
            cors: Vec::new(),
            directory: None,
            heartbeat: event::HEARTBEAT,
        }
    }
}

impl ServerOptions {
    // Password and username from OPENCODE_SERVER_PASSWORD / OPENCODE_SERVER_USERNAME.
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        Self { password: var("OPENCODE_SERVER_PASSWORD"), username: var("OPENCODE_SERVER_USERNAME"), ..Self::default() }
    }
}

pub struct Server {
    pub addr: SocketAddr,
    pub hostname: String,
    pub bus: bus::Bus,
    engine: Arc<Engine>,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
    // Feeds instance bus events to the plugin `event` hook.
    forward: JoinHandle<()>,
    // What the engine hands to plugins as their API endpoint while this server runs.
    info: ServerInfo,
    state: AppState,
    // Disposes instances nobody has used for a while.
    janitor: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.janitor.abort();
        self.state.stop();
        self.forward.abort();
        if self.engine.server().as_ref() == Some(&self.info) {
            self.engine.set_server(None);
        }
    }
}

// The stdout line SDKs wait for (`/on\s+(https?:\/\/[^\s]+)/`).
// Whether this build serves the original web UI (embedded at compile time, not disabled by env).
pub fn web_ui_available() -> bool {
    ui::available()
}

pub fn listening_line(hostname: &str, port: u16) -> String {
    format!("opencode server listening on http://{hostname}:{port}")
}

impl Server {
    pub fn url(&self) -> String {
        format!("http://{}:{}", self.hostname, self.addr.port())
    }

    pub fn listening_line(&self) -> String {
        listening_line(&self.hostname, self.addr.port())
    }

    // Queues a request for the TUI to pick up from `GET /tui/control/next`.
    pub fn tui_request(&self, path: &str, body: serde_json::Value) {
        self.state.tui_requests.push(serde_json::json!({"path": path, "body": body}));
    }

    // Disposes every instance directory that has gone unused for `idle`; runs by itself once a minute with
    // `INSTANCE_IDLE`.
    pub async fn dispose_idle(&self, idle: Duration) {
        self.state.dispose_idle(idle).await;
    }

    // The next answer posted to `/tui/control/response`.
    pub async fn tui_response(&self) -> serde_json::Value {
        self.state.tui_responses.next().await
    }

    // Aborts running sessions and closes terminals (sockets get 1001), stops accepting, then gives open connections
    // one second, as the original does; the engine's instances are disposed last.
    pub async fn shutdown(mut self) {
        self.janitor.abort();
        self.state.stop();
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if tokio::time::timeout(Duration::from_secs(1), &mut self.task).await.is_err() {
            self.task.abort();
        }
        self.forward.abort();
        if self.engine.server().as_ref() == Some(&self.info) {
            self.engine.set_server(None);
        }
        // MCP server processes belong to the engine's instances; they end with the server.
        self.engine.shutdown().await;
    }
}

// The private API server plugins get when the process runs none: loopback only, OS-assigned port, random password.
struct PluginServer {
    engine: std::sync::Weak<Engine>,
    running: tokio::sync::Mutex<Option<Server>>,
}

#[async_trait::async_trait]
impl ServerLauncher for PluginServer {
    async fn launch(&self) -> Result<ServerInfo, String> {
        let mut running = self.running.lock().await;
        if let Some(server) = running.as_ref() {
            return Ok(server.info.clone());
        }
        let engine = self.engine.upgrade().ok_or("the engine is gone")?;
        let listener = TcpListener::bind((DEFAULT_HOSTNAME, 0)).await.map_err(|e| e.to_string())?;
        let password = format!("{}{}", bus::event_id(), bus::event_id());
        let options = ServerOptions { password: Some(password), ..ServerOptions::default() };
        let server = serve_on(listener, options, engine.clone()).await.map_err(|e| e.to_string())?;
        engine.set_server(Some(server.info.clone()));
        let info = server.info.clone();
        *running = Some(server);
        Ok(info)
    }

    async fn stop(&self) {
        let server = self.running.lock().await.take();
        if let Some(mut server) = server {
            if let Some(tx) = server.shutdown.take() {
                let _ = tx.send(());
            }
            if tokio::time::timeout(Duration::from_secs(1), &mut server.task).await.is_err() {
                server.task.abort();
            }
        }
    }
}

// For processes without an HTTP server (`run`, desktop, TUI): plugins that load get a private one on demand, since
// the original's plugin `client` always has a server to talk to. It ends with `Engine::shutdown`.
pub fn attach_plugin_server(engine: &Arc<Engine>) {
    // Rust build: background shells and `/goal` start runs on the shared engine.
    engine.attach();
    engine.set_server_launcher(Arc::new(PluginServer { engine: Arc::downgrade(engine), running: tokio::sync::Mutex::new(None) }));
}

async fn bind(hostname: &str, port: u16) -> std::io::Result<TcpListener> {
    if port != 0 {
        return TcpListener::bind((hostname, port)).await;
    }
    match TcpListener::bind((hostname, DEFAULT_PORT)).await {
        Ok(listener) => Ok(listener),
        Err(_) => TcpListener::bind((hostname, 0)).await,
    }
}

pub async fn serve(options: ServerOptions, engine: Arc<Engine>) -> std::io::Result<Server> {
    let listener = bind(&options.hostname, options.port).await?;
    serve_on(listener, options, engine).await
}

// Serves on an already bound listener; `options.port` is ignored.
pub async fn serve_on(listener: TcpListener, options: ServerOptions, engine: Arc<Engine>) -> std::io::Result<Server> {
    engine.attach();
    let addr = listener.local_addr()?;
    let cwd = match options.directory {
        Some(directory) => directory,
        None => std::env::current_dir()?.to_string_lossy().into_owned(),
    };
    let credentials = options.password.filter(|password| !password.is_empty()).map(|password| Credentials {
        username: options.username.unwrap_or_else(|| DEFAULT_USERNAME.to_string()),
        password,
    });
    let url = format!("http://{}:{}", options.hostname, addr.port());
    let info = match &credentials {
        Some(credentials) => ServerInfo::with_password(url, Some(&credentials.username), &credentials.password),
        None => ServerInfo { url, headers: Vec::new() },
    };
    let bus = bus::Bus::new();
    let state: AppState = Arc::new(Inner {
        engine: engine.clone(),
        bus: bus.clone(),
        credentials,
        cors: options.cors,
        cwd,
        heartbeat: options.heartbeat,
        pending: Default::default(),
        questions: Default::default(),
        approved: Default::default(),
        locations: Default::default(),
        mcp_watched: Default::default(),
        pty: Default::default(),
        v2: Default::default(),
        tui_requests: Default::default(),
        tui_responses: Default::default(),
        tasks: Default::default(),
        touched: Default::default(),
        streams: Default::default(),
        connections: Default::default(),
    });
    let mirror = Arc::downgrade(&state);
    bus.set_tap(move |event| {
        if let Some(state) = mirror.upgrade() {
            state.v2_mirror(event);
        }
    });
    let terminals = Arc::downgrade(&state);
    state.pty.set_publisher(Arc::new(move |directory, kind, data| {
        if let Some(state) = terminals.upgrade() {
            let location = state.instance_location(&state.instance_for(directory, false));
            state.bus.publish(&location, kind, data);
        }
    }));
    let watcher = Arc::downgrade(&state);
    state.engine.store.watch(move |change| {
        if let Some(state) = watcher.upgrade() {
            state.on_change(change);
        }
    });
    // Events the engine raises outside the store: file edits, session diffs, language-server updates.
    let events = Arc::downgrade(&state);
    let known: std::sync::Mutex<std::collections::HashMap<String, bus::Location>> = Default::default();
    state.engine.watch_events(move |event| {
        let Some(state) = events.upgrade() else { return };
        let cached = state::lock(&known).get(&event.directory).cloned();
        let location = cached.unwrap_or_else(|| {
            let location = state.instance_location(&state.instance_for(&event.directory, false));
            state::lock(&known).insert(event.directory.clone(), location.clone());
            location
        });
        state.bus.publish(&location, &event.kind, event.properties.clone());
    });
    let app = router(state.clone());
    let idle = Arc::downgrade(&state);
    let janitor = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            let Some(state) = idle.upgrade() else { return };
            state.dispose_idle(INSTANCE_IDLE).await;
        }
    });
    let (tx, rx) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
    });
    // Plugins see every instance event of their directory; they talk back through this server.
    let mut events = bus.subscribe();
    let plugins = Arc::downgrade(&engine);
    let forward = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            let Some(engine) = plugins.upgrade() else { return };
            if let Some(directory) = event.directory.as_deref().filter(|_| !event.global_only) {
                engine.plugin_event(directory, &event.id, &event.kind, &event.properties);
            }
        }
    });
    if engine.server().is_none() {
        engine.set_server(Some(info.clone()));
    }
    Ok(Server { addr, hostname: options.hostname, bus, engine, shutdown: Some(tx), task, forward, info, state, janitor })
}

// Per-directory state is dropped after an hour without a request, as the original's location services are.
pub const INSTANCE_IDLE: Duration = Duration::from_secs(60 * 60);
// The original sets no limit; prompts carry base64 attachments, so this one is generous.
pub const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
const COMPRESSION_THRESHOLD: u64 = 1024;

impl Inner {
    // Ends everything that belongs to the running server: turns, terminals, background tasks.
    fn stop(&self) {
        for session_id in self.engine.busy() {
            self.abort(&session_id);
        }
        self.pty.shutdown();
        for task in state::lock(&self.tasks).drain(..) {
            task.abort();
        }
    }

    // Drops a directory's engine state and terminals and tells its event streams.
    pub async fn dispose_instance(&self, directory: &str) {
        let location = self.instance_location(&self.instance_for(directory, false));
        state::lock(&self.touched).remove(directory);
        self.pty.dispose_directory(directory);
        self.dispose_mcp(directory).await;
        self.bus.publish_global(Some(directory), Some(&location.project), "server.instance.disposed", serde_json::json!({"directory": directory}));
    }

    // Directories with a running turn, a terminal or an open `/event` stream count as in use.
    pub async fn dispose_idle(&self, idle: Duration) {
        let now = std::time::Instant::now();
        let stale: Vec<String> =
            state::lock(&self.touched).iter().filter(|(_, used)| now.duration_since(**used) >= idle).map(|(directory, _)| directory.clone()).collect();
        let busy: Vec<String> =
            self.engine.busy().iter().filter_map(|id| self.engine.store.get_session(id).ok().flatten()).map(|session| session.directory).collect();
        for directory in stale {
            let watched = state::lock(&self.streams).get(&directory).is_some_and(|count| *count > 0);
            if watched || busy.contains(&directory) || !self.pty.list(&directory).is_empty() {
                continue;
            }
            tracing::info!(directory = %directory, "disposing idle instance");
            self.dispose_instance(&directory).await;
        }
    }
}

fn v2_path(path: &str) -> bool {
    path.starts_with("/api/") || path.starts_with("/experimental/project/") && (path.ends_with("/copy") || path.ends_with("/copy/refresh"))
}

fn pty_connect_path(path: &str) -> bool {
    let rest = path.strip_prefix("/api").unwrap_or(path);
    rest.strip_prefix("/pty/").and_then(|rest| rest.strip_suffix("/connect")).is_some_and(|id| !id.is_empty() && !id.contains('/'))
}

fn compressible(content_type: &str) -> bool {
    let kind = content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    let Some((family, name)) = kind.split_once('/') else { return false };
    match family {
        "text" => name != "event-stream",
        "application" => {
            matches!(name, "javascript" | "json" | "xml" | "xml-dtd" | "ecmascript" | "rtf" | "tar" | "toml" | "wasm" | "x-javascript" | "x-sh" | "x-tar" | "x-www-form-urlencoded")
                || ["+json", "+text", "+xml", "+yaml"].iter().any(|suffix| name.ends_with(suffix))
        }
        _ => ["+json", "+text", "+xml", "+yaml"].iter().any(|suffix| name.ends_with(suffix)),
    }
}

// gzip (preferred) or deflate for buffered, compressible bodies of at least 1024 bytes; streams are never touched.
async fn compress(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let accepted = request.headers().get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).unwrap_or_default().to_ascii_lowercase();
    let response = next.run(request).await;
    let encoding = if accepted.contains("gzip") {
        "gzip"
    } else if accepted.contains("deflate") {
        "deflate"
    } else {
        return response;
    };
    let headers = response.headers();
    let text = |name: header::HeaderName| headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    let streaming = matches!(path.as_str(), "/event" | "/global/event")
        || method == Method::POST && path.strip_prefix("/session/").is_some_and(|rest| rest.ends_with("/message") || rest.ends_with("/prompt_async"));
    let size = axum::body::HttpBody::size_hint(response.body()).exact();
    if method == Method::HEAD
        || streaming
        || headers.contains_key(header::CONTENT_ENCODING)
        || headers.contains_key(header::TRANSFER_ENCODING)
        || text(header::CACHE_CONTROL).to_ascii_lowercase().split(',').any(|part| part.trim() == "no-transform")
        || !compressible(&text(header::CONTENT_TYPE))
        || size.is_none_or(|size| size < COMPRESSION_THRESHOLD)
    {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    use std::io::Write as _;
    let level = flate2::Compression::default();
    let packed = if encoding == "gzip" {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), level);
        encoder.write_all(&bytes).and_then(|()| encoder.finish())
    } else {
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), level);
        encoder.write_all(&bytes).and_then(|()| encoder.finish())
    };
    let Ok(packed) = packed else { return Response::from_parts(parts, axum::body::Body::from(bytes)) };
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static(encoding));
    parts.headers.append(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    Response::from_parts(parts, axum::body::Body::from(packed))
}

async fn access_log(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let started = std::time::Instant::now();
    let response = next.run(request).await;
    tracing::info!(
        target: "opencode_server::access",
        method = %method,
        path = %path,
        status = response.status().as_u16(),
        duration_ms = started.elapsed().as_millis() as u64,
        "request"
    );
    response
}

// `?auth_token=` wins over the header; both carry base64("user:pass").
fn credential(request: &Request) -> Option<(String, String)> {
    let pairs = state::query_pairs(request.uri().query());
    let encoded = match state::query_get(&pairs, "auth_token") {
        Some(token) => token.to_string(),
        None => {
            let value = request.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
            let (scheme, rest) = value.split_at_checked(5)?;
            let token = rest.trim_start();
            if !scheme.eq_ignore_ascii_case("basic") || token.len() == rest.len() || token.is_empty() {
                return None;
            }
            token.to_string()
        }
    };
    let decoded = String::from_utf8(STANDARD.decode(encoded).ok()?).ok()?;
    let (username, password) = decoded.split_once(':')?;
    Some((username.to_string(), password.to_string()))
}

async fn authorize(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if let Some(expected) = &state.credentials {
        let path = request.uri().path();
        // A connect request carrying a ticket is authorized by the ticket, which the handler consumes.
        let pairs = state::query_pairs(request.uri().query());
        let ticketed = pty_connect_path(path) && state::query_get(&pairs, "ticket").is_some_and(|ticket| !ticket.is_empty());
        let ok = ticketed
            || crate::ui::is_public_path(request.method(), path)
            || credential(&request).is_some_and(|(username, password)| username == expected.username && password == expected.password);
        if !ok {
            let challenge = [(header::WWW_AUTHENTICATE, "Basic realm=\"Secure Area\"")];
            if v2_path(path) {
                let body = serde_json::json!({"_tag": "UnauthorizedError", "message": "Authentication required"});
                return (StatusCode::UNAUTHORIZED, challenge, axum::Json(body)).into_response();
            }
            return (StatusCode::UNAUTHORIZED, challenge).into_response();
        }
    }
    next.run(request).await
}

fn opencode_origin(origin: &str) -> bool {
    let Some(host) = origin.strip_prefix("https://") else { return false };
    let Some(labels) = host.strip_suffix("opencode.ai") else { return false };
    let label = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    labels.is_empty() || labels.strip_suffix('.').is_some_and(|rest| rest.split('.').all(label))
}

pub fn is_allowed_origin(origin: Option<&str>, extra: &[String]) -> bool {
    let Some(origin) = origin.filter(|origin| !origin.is_empty()) else { return true };
    origin.starts_with("http://localhost:")
        || origin.starts_with("http://127.0.0.1:")
        || origin.starts_with("oc://renderer")
        || matches!(origin, "tauri://localhost" | "http://tauri.localhost" | "https://tauri.localhost")
        || opencode_origin(origin)
        || extra.iter().any(|allowed| allowed == origin)
}

// Outermost layer: preflights are answered without auth, and 401s still carry the CORS headers.
async fn cors(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let origin = request.headers().get(header::ORIGIN).cloned();
    let allowed = origin.as_ref().filter(|origin| is_allowed_origin(origin.to_str().ok(), &state.cors)).cloned();
    let preflight = request.method() == Method::OPTIONS;
    let requested = request.headers().get(header::ACCESS_CONTROL_REQUEST_HEADERS).cloned();
    let mut response = if preflight { StatusCode::NO_CONTENT.into_response() } else { next.run(request).await };
    let headers = response.headers_mut();
    if let Some(origin) = allowed {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        headers.append(header::VARY, HeaderValue::from_static("Origin"));
        if preflight {
            if let Some(requested) = requested {
                headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, requested);
            }
            headers.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET,HEAD,PUT,PATCH,POST,DELETE"));
            headers.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("86400"));
        }
    }
    response
}

fn router(state: AppState) -> Router {

    Router::new()
        .route("/global/health", get(instance::health))
        .route("/global/event", get(event::global))
        .route("/global/config", get(instance::global_config).patch(instance::global_config_update))
        .route("/global/dispose", post(instance::global_dispose))
        .route("/global/upgrade", post(instance::global_upgrade))
        .route("/auth/{providerID}", put(instance::auth_set).delete(instance::auth_remove))
        .route("/log", post(instance::log))
        .route("/doc", get(instance::doc))
        .route("/event", get(event::instance))
        .route("/config", get(instance::config_get).patch(instance::config_update))
        .route("/config/providers", get(provider::config_providers))
        .route("/provider", get(provider::list))
        .route("/provider/auth", get(provider::auth_methods))
        .route("/provider/{providerID}/oauth/authorize", post(provider::oauth_authorize))
        .route("/provider/{providerID}/oauth/callback", post(provider::oauth_callback))
        .route("/agent", get(instance::agents))
        .route("/command", get(instance::commands))
        .route("/skill", get(instance::skill_list))
        .route("/path", get(instance::path))
        .route("/project", get(instance::projects))
        .route("/project/current", get(instance::project_current))
        .route("/project/git/init", post(vcs::init_git))
        .route("/project/{projectID}", patch(instance::project_update))
        .route("/project/{projectID}/directories", get(vcs::project_directories))
        .route("/experimental/project/{projectID}/copy/generate-name", post(vcs::generate_name))
        .route("/experimental/project/{projectID}/copy", post(v2::copy::create).delete(v2::copy::remove))
        .route("/experimental/project/{projectID}/copy/refresh", post(v2::copy::refresh))
        .route("/session", get(session::list).post(session::create))
        .route("/session/status", get(session::status))
        .route("/session/{sessionID}", get(session::get).delete(session::remove).patch(session::update))
        .route("/session/{sessionID}/children", get(session::children))
        .route("/session/{sessionID}/todo", get(session::todo))
        .route("/session/{sessionID}/diff", get(session::diff))
        .route("/session/{sessionID}/message", get(session::messages).post(session::prompt))
        .route("/session/{sessionID}/message/{messageID}", get(session::message).delete(session::delete_message))
        .route(
            "/session/{sessionID}/message/{messageID}/part/{partID}",
            delete(session::delete_part).patch(session::update_part),
        )
        .route("/session/{sessionID}/prompt_async", post(session::prompt_async))
        .route("/session/{sessionID}/abort", post(session::abort))
        .route("/session/{sessionID}/fork", post(session::fork))
        .route("/session/{sessionID}/permissions/{permissionID}", post(session::permission_respond))
        .route("/session/{sessionID}/init", post(session::init))
        .route("/session/{sessionID}/share", post(vcs::share).delete(vcs::unshare))
        .route("/session/{sessionID}/summarize", post(session::summarize))
        .route("/session/{sessionID}/command", post(session::command))
        .route("/session/{sessionID}/shell", post(session::shell))
        .route("/session/{sessionID}/revert", post(session::revert))
        .route("/session/{sessionID}/unrevert", post(session::unrevert))
        .route("/permission", get(session::permission_list))
        .route("/permission/{requestID}/reply", post(session::permission_reply))
        .route("/question", get(session::question_list))
        .route("/question/{requestID}/reply", post(session::question_reply))
        .route("/question/{requestID}/reject", post(session::question_reject))
        .route("/instance/dispose", post(instance::instance_dispose))
        .route("/vcs", get(vcs::info))
        .route("/vcs/status", get(vcs::file_status))
        .route("/vcs/diff", get(vcs::diff))
        .route("/vcs/diff/raw", get(vcs::diff_raw))
        .route("/vcs/apply", post(vcs::apply))
        .route("/file", get(file::list))
        .route("/file/content", get(file::content))
        .route("/file/status", get(file::status))
        .route("/find", get(file::find_text))
        .route("/find/file", get(file::find_file))
        .route("/find/symbol", get(instance::find_symbol))
        .route("/lsp", get(instance::lsp_status))
        .route("/formatter", get(instance::formatter_status))
        .route("/mcp", get(mcp::status).post(mcp::add))
        .route("/mcp/{name}/auth", post(mcp::auth_start).delete(mcp::auth_remove))
        .route("/mcp/{name}/auth/callback", post(mcp::auth_callback))
        .route("/mcp/{name}/auth/authenticate", post(mcp::auth_authenticate))
        .route("/mcp/{name}/connect", post(mcp::connect))
        .route("/mcp/{name}/disconnect", post(mcp::disconnect))
        .route("/pty", get(terminal::list).post(terminal::create_legacy))
        .route("/pty/shells", get(terminal::shell_list))
        .route("/pty/{ptyID}", get(terminal::get).put(terminal::update).delete(terminal::remove))
        .route("/pty/{ptyID}/connect-token", post(terminal::connect_token))
        .route("/pty/{ptyID}/connect", get(terminal::connect_legacy))
        .route("/experimental/capabilities", get(instance::capabilities))
        .route("/experimental/console", get(vcs::console))
        .route("/experimental/console/orgs", get(vcs::console_orgs))
        .route("/experimental/console/switch", post(vcs::console_switch))
        .route("/experimental/tool", get(instance::tool_list))
        .route("/experimental/tool/ids", get(instance::tool_ids))
        .route("/experimental/worktree", get(vcs::worktree_list).post(vcs::worktree_create).delete(vcs::worktree_remove))
        .route("/experimental/worktree/reset", post(vcs::worktree_reset))
        .route("/experimental/session", get(instance::global_sessions))
        .route("/experimental/session/{sessionID}/background", post(instance::session_background))
        .route("/experimental/resource", get(mcp::resources))
        .route("/experimental/workspace", get(workspace::list).post(workspace::create))
        .route("/experimental/workspace/adapter", get(workspace::adapters))
        .route("/experimental/workspace/status", get(workspace::status))
        .route("/experimental/workspace/sync-list", post(workspace::sync_list))
        .route("/experimental/workspace/warp", post(workspace::warp))
        .route("/experimental/workspace/{id}", delete(workspace::remove))
        .route("/experimental/control-plane/move-session", post(workspace::move_session))
        .route("/sync/start", post(workspace::sync_start))
        .route("/sync/replay", post(workspace::sync_replay))
        .route("/sync/steal", post(vcs::sync_steal))
        .route("/sync/history", post(workspace::sync_history))
        .route("/tui/append-prompt", post(instance::tui_append_prompt))
        .route("/tui/open-help", post(instance::tui_open_help))
        .route("/tui/open-sessions", post(instance::tui_open_sessions))
        .route("/tui/open-themes", post(instance::tui_open_themes))
        .route("/tui/open-models", post(instance::tui_open_models))
        .route("/tui/submit-prompt", post(instance::tui_submit_prompt))
        .route("/tui/clear-prompt", post(instance::tui_clear_prompt))
        .route("/tui/execute-command", post(instance::tui_execute_command))
        .route("/tui/show-toast", post(instance::tui_show_toast))
        .route("/tui/publish", post(instance::tui_publish))
        .route("/tui/select-session", post(instance::tui_select_session))
        .route("/tui/control/next", get(vcs::tui_control_next))
        .route("/tui/control/response", post(vcs::tui_control_response))
        .route("/openapi.json", get(v2::openapi))
        .route("/api/health", get(v2::health))
        .route("/api/location", get(v2::location))
        .route("/api/agent", get(v2::agents))
        .route("/api/session", get(v2::session::list).post(v2::session::create))
        .route("/api/session/active", get(v2::session::active))
        .route("/api/session/{sessionID}", get(v2::session::get))
        .route("/api/session/{sessionID}/agent", post(v2::session::switch_agent))
        .route("/api/session/{sessionID}/model", post(v2::session::switch_model))
        .route("/api/session/{sessionID}/prompt", post(v2::session::prompt))
        .route("/api/session/{sessionID}/compact", post(v2::session::compact))
        .route("/api/session/{sessionID}/wait", post(v2::session::wait))
        .route("/api/session/{sessionID}/revert/stage", post(v2::session::revert_stage))
        .route("/api/session/{sessionID}/revert/clear", post(v2::session::revert_clear))
        .route("/api/session/{sessionID}/revert/commit", post(v2::session::revert_commit))
        .route("/api/session/{sessionID}/context", get(v2::session::context))
        .route("/api/session/{sessionID}/history", get(v2::session::history))
        .route("/api/session/{sessionID}/event", get(v2::session::events))
        .route("/api/session/{sessionID}/interrupt", post(v2::session::interrupt))
        .route("/api/session/{sessionID}/message", get(v2::session::messages))
        .route("/api/session/{sessionID}/message/{messageID}", get(v2::session::message))
        .route("/api/session/{sessionID}/permission", get(v2::session_permission_list).post(v2::session_permission_create))
        .route("/api/session/{sessionID}/permission/{requestID}", get(v2::session_permission_get))
        .route("/api/session/{sessionID}/permission/{requestID}/reply", post(v2::session_permission_reply))
        .route("/api/session/{sessionID}/question", get(v2::session_question_list))
        .route("/api/session/{sessionID}/question/{requestID}/reply", post(v2::session_question_reply))
        .route("/api/session/{sessionID}/question/{requestID}/reject", post(v2::session_question_reject))
        .route("/api/model", get(v2::models))
        .route("/api/provider", get(v2::providers))
        .route("/api/provider/{providerID}", get(v2::provider_get))
        .route("/api/integration", get(v2::integrations))
        .route("/api/integration/{integrationID}", get(v2::integration_get))
        .route("/api/integration/{integrationID}/connect/key", post(v2::connect_key))
        .route("/api/integration/{integrationID}/connect/oauth", post(v2::connect_oauth))
        .route("/api/integration/attempt/{attemptID}", get(v2::attempt_status).delete(v2::attempt_cancel))
        .route("/api/integration/attempt/{attemptID}/complete", post(v2::attempt_complete))
        .route("/api/credential/{credentialID}", patch(v2::credential_update).delete(v2::credential_remove))
        .route("/api/permission/request", get(v2::permission_requests))
        .route("/api/permission/saved", get(v2::permission_saved))
        .route("/api/permission/saved/{id}", delete(v2::permission_saved_remove))
        .route("/api/fs/read/{*path}", get(v2::fs_read))
        .route("/api/fs/list", get(v2::fs_list))
        .route("/api/fs/find", get(v2::fs_find))
        .route("/api/command", get(v2::commands))
        .route("/api/skill", get(v2::skills))
        .route("/api/event", get(v2::events))
        .route("/api/pty", get(terminal::v2_list).post(terminal::v2_create))
        .route("/api/pty/{ptyID}", get(terminal::v2_get).put(terminal::v2_update).delete(terminal::v2_remove))
        .route("/api/pty/{ptyID}/connect-token", post(terminal::v2_connect_token))
        .route("/api/pty/{ptyID}/connect", get(terminal::v2_connect))
        .route("/api/question/request", get(v2::question_requests))
        .route("/api/reference", get(v2::references))
        .fallback(instance::fallback)
        .method_not_allowed_fallback(instance::fallback)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn(compress))
        .layer(middleware::from_fn_with_state(state.clone(), authorize))
        .layer(middleware::from_fn_with_state(state.clone(), cors))
        .layer(middleware::from_fn(access_log))
        .with_state(state)
}
