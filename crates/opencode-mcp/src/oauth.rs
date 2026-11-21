//! OAuth for remote servers: `mcp/oauth-provider.ts` plus the SDK's `client/auth.js` flow
//! (RFC 9728 / RFC 8414 discovery, RFC 7591 registration, PKCE, code exchange, refresh).

use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use indexmap::IndexMap;
use reqwest::Url;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::auth::{AuthStore, ClientInfo, Entry, Tokens, expired, now_seconds, number};
use crate::client::LATEST_PROTOCOL_VERSION;
use crate::error::McpError;

pub const OAUTH_CALLBACK_PORT: u16 = 19876;
pub const OAUTH_CALLBACK_PATH: &str = "/mcp/oauth/callback";

const KNOWN_OAUTH_ERRORS: [&str; 17] = [
    "invalid_request",
    "invalid_client",
    "invalid_grant",
    "unauthorized_client",
    "unsupported_grant_type",
    "invalid_scope",
    "access_denied",
    "server_error",
    "temporarily_unavailable",
    "unsupported_response_type",
    "unsupported_token_type",
    "invalid_token",
    "method_not_allowed",
    "too_many_requests",
    "invalid_client_metadata",
    "insufficient_scope",
    "invalid_target",
];

#[derive(Debug, Clone, Default)]
pub(crate) struct OAuthSettings {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub scope: Option<String>,
    pub callback_port: Option<u16>,
    pub redirect_uri: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum AuthOutcome {
    Authorized,
    Redirect,
}

enum Failure {
    OAuth { code: String, message: String },
    Other(String),
}

impl Failure {
    fn other(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }

    fn is_server_error(&self) -> bool {
        match self {
            Self::OAuth { code, .. } => {
                code == "server_error" || !KNOWN_OAUTH_ERRORS.contains(&code.as_str())
            }
            Self::Other(_) => false,
        }
    }
}

impl From<Failure> for McpError {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::OAuth { message, .. } | Failure::Other(message) => McpError::Other(message),
        }
    }
}

#[derive(Debug, Clone)]
struct ClientCreds {
    client_id: String,
    client_secret: Option<String>,
    auth_method: Option<String>,
}

#[derive(Default)]
struct PendingCreds {
    client: Option<Value>,
    tokens: Option<Value>,
}

#[derive(Default)]
struct Challenge {
    resource_metadata_url: Option<String>,
    scope: Option<String>,
}

struct Discovery {
    server: Url,
    metadata: Option<Value>,
    resource: Option<Value>,
}

pub(crate) struct OAuthProvider {
    name: String,
    server_url: String,
    cfg: OAuthSettings,
    store: Arc<AuthStore>,
    http: reqwest::Client,
    headers: IndexMap<String, String>,
    /// `McpOAuthPendingProvider`: credentials stay in memory until `commit`.
    pending: Option<Mutex<PendingCreds>>,
    captured: Mutex<Option<String>>,
    challenge: Mutex<Challenge>,
}

pub(crate) fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer).expect("system random source unavailable");
    buffer.iter().map(|b| format!("{b:02x}")).collect()
}

/// Port and path the callback server must listen on for a redirect URI.
pub fn parse_redirect_uri(redirect_uri: Option<&str>) -> (u16, String) {
    let fallback = (OAUTH_CALLBACK_PORT, OAUTH_CALLBACK_PATH.to_string());
    let Some(uri) = redirect_uri.filter(|uri| !uri.is_empty()) else {
        return fallback;
    };
    match Url::parse(uri) {
        Ok(url) => {
            let port = url
                .port()
                .unwrap_or(if url.scheme() == "https" { 443 } else { 80 });
            let path = match url.path() {
                "" => OAUTH_CALLBACK_PATH.to_string(),
                path => path.to_string(),
            };
            (port, path)
        }
        Err(_) => fallback,
    }
}

fn www_authenticate_field(header: &str, field: &str) -> Option<String> {
    let needle = format!("{field}=");
    let rest = &header[header.find(&needle)? + needle.len()..];
    if let Some(quoted) = rest.strip_prefix('"')
        && let Some(end) = quoted.find('"')
        && end > 0
    {
        return Some(quoted[..end].to_string());
    }
    let end = rest
        .find(|c: char| c.is_whitespace() || c == ',')
        .unwrap_or(rest.len());
    (end > 0).then(|| rest[..end].to_string())
}

fn set_query(url: &mut Url, key: &str, value: &str) {
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != key)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    url.query_pairs_mut()
        .clear()
        .extend_pairs(kept)
        .append_pair(key, value);
}

fn form_encode(params: &[(String, String)]) -> String {
    let mut scratch = Url::parse("http://localhost/").expect("static url");
    scratch.query_pairs_mut().extend_pairs(params);
    scratch.query().unwrap_or_default().to_string()
}

fn parse_error_response(status: u16, body: &str) -> Failure {
    let parsed = serde_json::from_str::<Value>(body);
    if let Ok(value) = &parsed
        && let Some(code) = value.get("error").and_then(Value::as_str)
    {
        return Failure::OAuth {
            code: code.to_string(),
            message: value
                .get("error_description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        };
    }
    let reason = match parsed {
        Err(error) => error.to_string(),
        Ok(_) => "missing error field".to_string(),
    };
    Failure::OAuth {
        code: "server_error".into(),
        message: format!("HTTP {status}: Invalid OAuth error response: {reason}. Raw body: {body}"),
    }
}

fn select_auth_method(client: &ClientCreds, supported: &[String]) -> String {
    let has_secret = client.client_secret.is_some();
    if let Some(method) = client.auth_method.as_deref().filter(|m| !m.is_empty())
        && ["client_secret_basic", "client_secret_post", "none"].contains(&method)
        && (supported.is_empty() || supported.iter().any(|s| s == method))
    {
        return method.to_string();
    }
    let has = |method: &str| supported.iter().any(|s| s == method);
    if supported.is_empty() {
        return if has_secret {
            "client_secret_basic"
        } else {
            "none"
        }
        .into();
    }
    if has_secret && has("client_secret_basic") {
        return "client_secret_basic".into();
    }
    if has_secret && has("client_secret_post") {
        return "client_secret_post".into();
    }
    if has("none") {
        return "none".into();
    }
    if has_secret {
        "client_secret_post"
    } else {
        "none"
    }
    .into()
}

fn resource_allowed(requested: &Url, configured: &Url) -> bool {
    if requested.origin() != configured.origin() || requested.path().len() < configured.path().len()
    {
        return false;
    }
    let slashed = |path: &str| {
        if path.ends_with('/') {
            path.to_string()
        } else {
            format!("{path}/")
        }
    };
    slashed(requested.path()).starts_with(&slashed(configured.path()))
}

fn tokens_from_response(tokens: &Value) -> Tokens {
    let text = |key: &str| tokens.get(key).and_then(Value::as_str).map(str::to_string);
    let expires_in = tokens
        .get("expires_in")
        .and_then(Value::as_f64)
        .filter(|v| *v != 0.0);
    Tokens {
        access_token: text("access_token").unwrap_or_default(),
        refresh_token: text("refresh_token"),
        expires_at: expires_in.and_then(|seconds| number(now_seconds() + seconds)),
        scope: text("scope"),
    }
}

fn client_info_from_registration(info: &Value) -> ClientInfo {
    let number_of = |key: &str| info.get(key).and_then(Value::as_f64).and_then(number);
    ClientInfo {
        client_id: info
            .get("client_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        client_secret: info
            .get("client_secret")
            .and_then(Value::as_str)
            .map(str::to_string),
        client_id_issued_at: number_of("client_id_issued_at"),
        client_secret_expires_at: number_of("client_secret_expires_at"),
    }
}

impl OAuthProvider {
    pub(crate) fn new(
        name: &str,
        server_url: &str,
        cfg: OAuthSettings,
        store: Arc<AuthStore>,
        http: reqwest::Client,
        headers: IndexMap<String, String>,
        pending: bool,
    ) -> Self {
        Self {
            name: name.to_string(),
            server_url: server_url.to_string(),
            cfg,
            store,
            http,
            headers,
            pending: pending.then(|| Mutex::new(PendingCreds::default())),
            captured: Mutex::new(None),
            challenge: Mutex::new(Challenge::default()),
        }
    }

    pub(crate) fn redirect_url(&self) -> String {
        if let Some(uri) = self
            .cfg
            .redirect_uri
            .as_deref()
            .filter(|uri| !uri.is_empty())
        {
            return uri.to_string();
        }
        let port = self.cfg.callback_port.unwrap_or(OAUTH_CALLBACK_PORT);
        format!("http://127.0.0.1:{port}{OAUTH_CALLBACK_PATH}")
    }

    pub(crate) fn client_metadata(&self) -> Value {
        let method = if self
            .cfg
            .client_secret
            .as_deref()
            .is_some_and(|s| !s.is_empty())
        {
            "client_secret_post"
        } else {
            "none"
        };
        let mut metadata = json!({
            "redirect_uris": [self.redirect_url()],
            "client_name": "OpenCode",
            "client_uri": "https://opencode.ai",
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": method,
        });
        if let Some(scope) = self.configured_scope() {
            metadata["scope"] = scope.into();
        }
        metadata
    }

    fn configured_scope(&self) -> Option<String> {
        self.cfg.scope.clone().filter(|scope| !scope.is_empty())
    }

    fn configured_client(&self) -> Option<ClientCreds> {
        let client_id = self.cfg.client_id.clone().filter(|id| !id.is_empty())?;
        Some(ClientCreds {
            client_id,
            client_secret: self.cfg.client_secret.clone(),
            auth_method: None,
        })
    }

    async fn client_information(&self) -> Option<ClientCreds> {
        if let Some(pending) = &self.pending {
            if let Some(client) = self.configured_client() {
                return Some(client);
            }
            let guard = pending.lock().unwrap();
            let info = guard.client.as_ref()?;
            let text = |key: &str| info.get(key).and_then(Value::as_str).map(str::to_string);
            return Some(ClientCreds {
                client_id: text("client_id")?,
                client_secret: text("client_secret"),
                auth_method: text("token_endpoint_auth_method"),
            });
        }
        if let Some(client) = self.configured_client() {
            return Some(client);
        }
        let info = self
            .store
            .get_for_url(&self.name, &self.server_url)
            .await?
            .client_info?;
        if expired(&info.client_secret_expires_at) {
            return None;
        }
        Some(ClientCreds {
            client_id: info.client_id,
            client_secret: info.client_secret,
            auth_method: None,
        })
    }

    async fn save_client_information(&self, info: &Value) {
        if let Some(pending) = &self.pending {
            pending.lock().unwrap().client = Some(info.clone());
            return;
        }
        self.store
            .update_client_info(
                &self.name,
                client_info_from_registration(info),
                Some(&self.server_url),
            )
            .await;
    }

    /// `(access_token, refresh_token)`.
    async fn tokens(&self) -> Option<(String, Option<String>)> {
        if let Some(pending) = &self.pending {
            let guard = pending.lock().unwrap();
            let tokens = guard.tokens.as_ref()?;
            let text = |key: &str| tokens.get(key).and_then(Value::as_str).map(str::to_string);
            return Some((text("access_token")?, text("refresh_token")));
        }
        let tokens = self
            .store
            .get_for_url(&self.name, &self.server_url)
            .await?
            .tokens?;
        Some((tokens.access_token, tokens.refresh_token))
    }

    pub(crate) async fn access_token(&self) -> Option<String> {
        self.tokens().await.map(|(access, _)| access)
    }

    async fn save_tokens(&self, tokens: Value) {
        if let Some(pending) = &self.pending {
            pending.lock().unwrap().tokens = Some(tokens);
            return;
        }
        self.store
            .update_tokens(
                &self.name,
                tokens_from_response(&tokens),
                Some(&self.server_url),
            )
            .await;
    }

    async fn state(&self) -> String {
        if let Some(state) = self
            .store
            .get_oauth_state(&self.name)
            .await
            .filter(|s| !s.is_empty())
        {
            return state;
        }
        let state = random_hex(32);
        self.store.update_oauth_state(&self.name, &state).await;
        state
    }

    async fn invalidate(&self, all: bool) {
        if let Some(pending) = &self.pending {
            let mut guard = pending.lock().unwrap();
            guard.tokens = None;
            if all {
                guard.client = None;
            }
            return;
        }
        if all {
            self.store.remove(&self.name).await;
            return;
        }
        if let Some(mut entry) = self.store.get(&self.name).await {
            entry.tokens = None;
            self.store.set(&self.name, entry, None).await;
        }
    }

    /// Writes the pending tokens (and dynamically registered client) to disk.
    pub(crate) async fn commit(&self) {
        let Some(pending) = &self.pending else { return };
        let entry = {
            let guard = pending.lock().unwrap();
            let Some(tokens) = &guard.tokens else { return };
            Entry {
                tokens: Some(tokens_from_response(tokens)),
                client_info: guard
                    .client
                    .as_ref()
                    .filter(|_| self.configured_client().is_none())
                    .map(client_info_from_registration),
                ..Entry::default()
            }
        };
        self.store
            .set(&self.name, entry, Some(&self.server_url))
            .await;
    }

    pub(crate) fn captured_url(&self) -> Option<String> {
        self.captured.lock().unwrap().clone()
    }

    /// Reaction to a 401: refresh if possible, otherwise prepare the authorization redirect.
    pub(crate) async fn handle_unauthorized(
        &self,
        www_authenticate: Option<&str>,
    ) -> Result<AuthOutcome, McpError> {
        {
            let mut challenge = self.challenge.lock().unwrap();
            *challenge = Challenge::default();
            if let Some(header) = www_authenticate {
                let mut parts = header.split(' ');
                let scheme = parts.next().unwrap_or_default();
                if scheme.eq_ignore_ascii_case("bearer")
                    && parts.next().is_some_and(|p| !p.is_empty())
                {
                    challenge.resource_metadata_url =
                        www_authenticate_field(header, "resource_metadata")
                            .filter(|u| Url::parse(u).is_ok());
                    challenge.scope = www_authenticate_field(header, "scope");
                }
            }
        }
        self.auth(None).await
    }

    /// `transport.finishAuth(code)`.
    pub(crate) async fn finish(&self, code: &str) -> Result<(), McpError> {
        match self.auth(Some(code)).await? {
            AuthOutcome::Authorized => Ok(()),
            AuthOutcome::Redirect => Err(McpError::Unauthorized("Failed to authorize".into())),
        }
    }

    async fn auth(&self, code: Option<&str>) -> Result<AuthOutcome, McpError> {
        match self.auth_internal(code).await {
            Err(Failure::OAuth { code: kind, .. })
                if kind == "invalid_client" || kind == "unauthorized_client" =>
            {
                self.invalidate(true).await;
                Ok(self.auth_internal(code).await?)
            }
            Err(Failure::OAuth { code: kind, .. }) if kind == "invalid_grant" => {
                self.invalidate(false).await;
                Ok(self.auth_internal(code).await?)
            }
            other => Ok(other?),
        }
    }

    async fn fetch(&self, url: Url, accept_json: bool) -> Option<reqwest::Response> {
        let mut request = self.http.get(url);
        for (key, value) in &self.headers {
            request = request.header(key, value);
        }
        request = request.header("MCP-Protocol-Version", LATEST_PROTOCOL_VERSION);
        if accept_json {
            request = request.header("Accept", "application/json");
        }
        request.send().await.ok()
    }

    async fn discover_resource_metadata(
        &self,
        server: &Url,
        metadata_url: Option<&str>,
    ) -> Option<Value> {
        let explicit = metadata_url.and_then(|url| Url::parse(url).ok());
        let has_explicit = explicit.is_some();
        let first = explicit.unwrap_or_else(|| {
            let path = server.path().trim_end_matches('/');
            let mut url = server.clone();
            url.set_path(&format!("/.well-known/oauth-protected-resource{path}"));
            url.set_fragment(None);
            url
        });
        let mut response = self.fetch(first, false).await;
        let retry = match &response {
            None => true,
            Some(r) => r.status().is_client_error() && server.path() != "/",
        };
        if !has_explicit && retry {
            let mut root = server.clone();
            root.set_path("/.well-known/oauth-protected-resource");
            root.set_query(None);
            root.set_fragment(None);
            response = self.fetch(root, false).await;
        }
        let response = response.filter(|r| r.status().is_success())?;
        let value: Value = serde_json::from_slice(&response.bytes().await.ok()?).ok()?;
        value.get("resource").and_then(Value::as_str)?;
        Some(value)
    }

    async fn discover_server_metadata(&self, server: &Url) -> Result<Option<Value>, Failure> {
        let origin = |path: &str| {
            let mut url = server.clone();
            url.set_path(path);
            url.set_query(None);
            url.set_fragment(None);
            url
        };
        let path = server.path().trim_end_matches('/');
        let candidates = if path.is_empty() {
            vec![
                (origin("/.well-known/oauth-authorization-server"), "OAuth"),
                (
                    origin("/.well-known/openid-configuration"),
                    "OpenID provider",
                ),
            ]
        } else {
            vec![
                (
                    origin(&format!("/.well-known/oauth-authorization-server{path}")),
                    "OAuth",
                ),
                (
                    origin(&format!("/.well-known/openid-configuration{path}")),
                    "OpenID provider",
                ),
                (
                    origin(&format!("{path}/.well-known/openid-configuration")),
                    "OpenID provider",
                ),
            ]
        };
        for (url, kind) in candidates {
            let Some(response) = self.fetch(url.clone(), true).await else {
                continue;
            };
            let status = response.status();
            if status.is_client_error() {
                continue;
            }
            if !status.is_success() {
                return Err(Failure::Other(format!(
                    "HTTP {} trying to load {kind} metadata from {url}",
                    status.as_u16()
                )));
            }
            let bytes = response
                .bytes()
                .await
                .map_err(|e| Failure::Other(e.to_string()))?;
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|e| Failure::Other(format!("Invalid {kind} metadata from {url}: {e}")))?;
            return Ok(Some(value));
        }
        Ok(None)
    }

    async fn discover(&self) -> Result<Discovery, Failure> {
        let server = Url::parse(&self.server_url)
            .map_err(|_| Failure::Other(format!("Invalid MCP URL for \"{}\"", self.name)))?;
        let metadata_url = self.challenge.lock().unwrap().resource_metadata_url.clone();
        let resource = self
            .discover_resource_metadata(&server, metadata_url.as_deref())
            .await;
        let authorization_server = resource
            .as_ref()
            .and_then(|r| {
                r.get("authorization_servers")?
                    .as_array()?
                    .first()?
                    .as_str()
            })
            .and_then(|s| Url::parse(s).ok())
            .unwrap_or_else(|| server.join("/").unwrap_or_else(|_| server.clone()));
        let metadata = self.discover_server_metadata(&authorization_server).await?;
        Ok(Discovery {
            server: authorization_server,
            metadata,
            resource,
        })
    }

    async fn auth_internal(&self, code: Option<&str>) -> Result<AuthOutcome, Failure> {
        let discovery = self.discover().await?;
        let server = Url::parse(&self.server_url).map_err(|e| Failure::Other(e.to_string()))?;

        let resource = match &discovery.resource {
            None => None,
            Some(metadata) => {
                let configured = metadata
                    .get("resource")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let mut requested = server.clone();
                requested.set_fragment(None);
                match Url::parse(configured) {
                    Ok(url) if resource_allowed(&requested, &url) => Some(url),
                    _ => {
                        return Err(Failure::Other(format!(
                            "Protected resource {configured} does not match expected {requested} (or origin)"
                        )));
                    }
                }
            }
        };

        let supported_scopes = discovery
            .resource
            .as_ref()
            .and_then(|r| r.get("scopes_supported")?.as_array())
            .map(|scopes| {
                scopes
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            });
        let challenge_scope = self.challenge.lock().unwrap().scope.clone();
        let scope = challenge_scope
            .filter(|s| !s.is_empty())
            .or(supported_scopes.filter(|s| !s.is_empty()))
            .or(self.configured_scope());

        let client = match self.client_information().await {
            Some(client) => client,
            None => {
                if code.is_some() {
                    return Err(Failure::other(
                        "Existing OAuth client information is required when exchanging an authorization code",
                    ));
                }
                let info = self.register(&discovery, scope.as_deref()).await?;
                self.save_client_information(&info).await;
                let text = |key: &str| info.get(key).and_then(Value::as_str).map(str::to_string);
                ClientCreds {
                    client_id: text("client_id").unwrap_or_default(),
                    client_secret: text("client_secret"),
                    auth_method: text("token_endpoint_auth_method"),
                }
            }
        };

        if let Some(code) = code {
            let verifier = self
                .store
                .get(&self.name)
                .await
                .and_then(|entry| entry.code_verifier)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    Failure::Other(format!(
                        "No code verifier saved for MCP server: {}",
                        self.name
                    ))
                })?;
            let params = vec![
                ("grant_type".to_string(), "authorization_code".to_string()),
                ("code".to_string(), code.to_string()),
                ("code_verifier".to_string(), verifier),
                ("redirect_uri".to_string(), self.redirect_url()),
            ];
            let client = self.client_information().await;
            let tokens = self
                .token_request(&discovery, params, client.as_ref(), resource.as_ref())
                .await?;
            self.save_tokens(tokens).await;
            return Ok(AuthOutcome::Authorized);
        }

        if let Some((_, Some(refresh_token))) = self
            .tokens()
            .await
            .filter(|(_, r)| r.as_deref().is_some_and(|r| !r.is_empty()))
        {
            let params = vec![
                ("grant_type".to_string(), "refresh_token".to_string()),
                ("refresh_token".to_string(), refresh_token.clone()),
            ];
            match self
                .token_request(&discovery, params, Some(&client), resource.as_ref())
                .await
            {
                Ok(mut tokens) => {
                    if let Some(map) = tokens.as_object_mut()
                        && !map.contains_key("refresh_token")
                    {
                        map.insert("refresh_token".into(), refresh_token.into());
                    }
                    self.save_tokens(tokens).await;
                    return Ok(AuthOutcome::Authorized);
                }
                Err(failure @ Failure::OAuth { .. }) if !failure.is_server_error() => {
                    return Err(failure);
                }
                Err(_) => {}
            }
        }

        let state = self.state().await;
        let (url, verifier) = self.start_authorization(
            &discovery,
            &client,
            &state,
            scope.as_deref(),
            resource.as_ref(),
        )?;
        self.store.update_code_verifier(&self.name, &verifier).await;
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(Failure::Other(format!(
                "MCP server \"{}\" returned a {}: authorization URL; only http and https are supported",
                self.server_url,
                url.scheme()
            )));
        }
        *self.captured.lock().unwrap() = Some(url.to_string());
        Ok(AuthOutcome::Redirect)
    }

    async fn register(&self, discovery: &Discovery, scope: Option<&str>) -> Result<Value, Failure> {
        let endpoint = match &discovery.metadata {
            Some(metadata) => metadata
                .get("registration_endpoint")
                .and_then(Value::as_str)
                .and_then(|endpoint| Url::parse(endpoint).ok())
                .ok_or_else(|| {
                    Failure::other(
                        "Incompatible auth server: does not support dynamic client registration",
                    )
                })?,
            None => discovery
                .server
                .join("/register")
                .map_err(|e| Failure::Other(e.to_string()))?,
        };
        let mut body = self.client_metadata();
        if let Some(scope) = scope {
            body["scope"] = scope.into();
        }
        let mut request = self.http.post(endpoint);
        for (key, value) in &self.headers {
            request = request.header(key, value);
        }
        let response = request
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| Failure::Other(e.to_string()))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(parse_error_response(status.as_u16(), &text));
        }
        let info: Value = serde_json::from_str(&text)
            .map_err(|e| Failure::Other(format!("Invalid client registration response: {e}")))?;
        if info.get("client_id").and_then(Value::as_str).is_none() {
            return Err(Failure::other(
                "Invalid client registration response: missing client_id",
            ));
        }
        Ok(info)
    }

    fn start_authorization(
        &self,
        discovery: &Discovery,
        client: &ClientCreds,
        state: &str,
        scope: Option<&str>,
        resource: Option<&Url>,
    ) -> Result<(Url, String), Failure> {
        let mut url = match &discovery.metadata {
            Some(metadata) => {
                let list = |key: &str| {
                    metadata.get(key).and_then(Value::as_array).map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect::<Vec<_>>()
                    })
                };
                if !list("response_types_supported")
                    .unwrap_or_default()
                    .iter()
                    .any(|t| t == "code")
                {
                    return Err(Failure::other(
                        "Incompatible auth server: does not support response type code",
                    ));
                }
                if let Some(methods) = list("code_challenge_methods_supported")
                    && !methods.iter().any(|m| m == "S256")
                {
                    return Err(Failure::other(
                        "Incompatible auth server: does not support code challenge method S256",
                    ));
                }
                metadata
                    .get("authorization_endpoint")
                    .and_then(Value::as_str)
                    .and_then(|endpoint| Url::parse(endpoint).ok())
                    .ok_or_else(|| {
                        Failure::other("Invalid OAuth metadata: missing authorization_endpoint")
                    })?
            }
            None => discovery
                .server
                .join("/authorize")
                .map_err(|e| Failure::Other(e.to_string()))?,
        };
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).expect("system random source unavailable");
        let verifier = URL_SAFE_NO_PAD.encode(seed);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        set_query(&mut url, "response_type", "code");
        set_query(&mut url, "client_id", &client.client_id);
        set_query(&mut url, "code_challenge", &challenge);
        set_query(&mut url, "code_challenge_method", "S256");
        set_query(&mut url, "redirect_uri", &self.redirect_url());
        if !state.is_empty() {
            set_query(&mut url, "state", state);
        }
        if let Some(scope) = scope.filter(|s| !s.is_empty()) {
            set_query(&mut url, "scope", scope);
            if scope.contains("offline_access") {
                url.query_pairs_mut().append_pair("prompt", "consent");
            }
        }
        if let Some(resource) = resource {
            set_query(&mut url, "resource", resource.as_str());
        }
        Ok((url, verifier))
    }

    async fn token_request(
        &self,
        discovery: &Discovery,
        mut params: Vec<(String, String)>,
        client: Option<&ClientCreds>,
        resource: Option<&Url>,
    ) -> Result<Value, Failure> {
        let endpoint = discovery
            .metadata
            .as_ref()
            .and_then(|m| m.get("token_endpoint")?.as_str())
            .and_then(|endpoint| Url::parse(endpoint).ok())
            .map(Ok)
            .unwrap_or_else(|| discovery.server.join("/token"))
            .map_err(|e| Failure::Other(e.to_string()))?;
        if let Some(resource) = resource {
            params.push(("resource".into(), resource.to_string()));
        }
        let mut basic = None;
        if let Some(client) = client {
            let supported: Vec<String> = discovery
                .metadata
                .as_ref()
                .and_then(|m| m.get("token_endpoint_auth_methods_supported")?.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            match select_auth_method(client, &supported).as_str() {
                "client_secret_basic" => {
                    let secret = client.client_secret.as_deref().ok_or_else(|| {
                        Failure::other(
                            "client_secret_basic authentication requires a client_secret",
                        )
                    })?;
                    basic = Some(STANDARD.encode(format!("{}:{secret}", client.client_id)));
                }
                "client_secret_post" => {
                    params.push(("client_id".into(), client.client_id.clone()));
                    if let Some(secret) = client.client_secret.as_deref().filter(|s| !s.is_empty())
                    {
                        params.push(("client_secret".into(), secret.to_string()));
                    }
                }
                _ => params.push(("client_id".into(), client.client_id.clone())),
            }
        }
        let mut request = self.http.post(endpoint);
        for (key, value) in &self.headers {
            request = request.header(key, value);
        }
        request = request
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json");
        if let Some(basic) = basic {
            request = request.header("Authorization", format!("Basic {basic}"));
        }
        let response = request
            .body(form_encode(&params))
            .send()
            .await
            .map_err(|e| Failure::Other(e.to_string()))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(parse_error_response(status.as_u16(), &text));
        }
        let tokens: Value = serde_json::from_str(&text)
            .map_err(|e| Failure::Other(format!("Invalid token response: {e}")))?;
        if tokens.get("access_token").and_then(Value::as_str).is_none() {
            return Err(Failure::other(
                "Invalid token response: missing access_token",
            ));
        }
        Ok(match tokens {
            Value::Object(map) => Value::Object(map),
            _ => Value::Object(Map::new()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_uri_parsing() {
        assert_eq!(
            parse_redirect_uri(None),
            (19876, "/mcp/oauth/callback".to_string())
        );
        assert_eq!(
            parse_redirect_uri(Some("http://127.0.0.1:4000/cb")),
            (4000, "/cb".to_string())
        );
        assert_eq!(
            parse_redirect_uri(Some("https://example.test/x")),
            (443, "/x".to_string())
        );
        assert_eq!(
            parse_redirect_uri(Some("http://example.test")),
            (80, "/".to_string())
        );
        assert_eq!(
            parse_redirect_uri(Some("not a url")),
            (19876, "/mcp/oauth/callback".to_string())
        );
    }

    #[test]
    fn www_authenticate_fields() {
        let header = r#"Bearer resource_metadata="http://h/.well-known/x", scope=a, error="insufficient_scope""#;
        assert_eq!(
            www_authenticate_field(header, "resource_metadata").unwrap(),
            "http://h/.well-known/x"
        );
        assert_eq!(www_authenticate_field(header, "scope").unwrap(), "a");
        assert_eq!(www_authenticate_field(header, "missing"), None);
    }

    #[test]
    fn auth_method_selection() {
        let secret = ClientCreds {
            client_id: "c".into(),
            client_secret: Some("s".into()),
            auth_method: None,
        };
        let public = ClientCreds {
            client_secret: None,
            ..secret.clone()
        };
        let list = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(select_auth_method(&secret, &[]), "client_secret_basic");
        assert_eq!(select_auth_method(&public, &[]), "none");
        assert_eq!(
            select_auth_method(&secret, &list(&["client_secret_post"])),
            "client_secret_post"
        );
        assert_eq!(
            select_auth_method(&public, &list(&["client_secret_post"])),
            "none"
        );
        let hinted = ClientCreds {
            auth_method: Some("client_secret_post".into()),
            ..secret
        };
        assert_eq!(select_auth_method(&hinted, &[]), "client_secret_post");
    }

    #[test]
    fn resource_matching() {
        let url = |s: &str| Url::parse(s).unwrap();
        assert!(resource_allowed(
            &url("http://h/mcp/a"),
            &url("http://h/mcp")
        ));
        assert!(resource_allowed(&url("http://h/mcp"), &url("http://h/")));
        assert!(!resource_allowed(
            &url("http://h/mcp"),
            &url("http://h/mcp/a")
        ));
        assert!(!resource_allowed(
            &url("http://h/mcp"),
            &url("http://other/mcp")
        ));
    }
}
