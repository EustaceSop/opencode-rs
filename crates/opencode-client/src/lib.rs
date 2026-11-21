// Typed client for the legacy (v1) opencode HTTP API: the routes the TUI and `run` use, Basic auth, the
// SDK's directory rules, and `/event` / `/global/event` streams with reconnect.
mod event;
mod sse;
mod types;

pub use event::{Event, Frame};
pub use sse::{EventStream, Reconnect};
pub use types::*;

use base64::Engine as _;
use opencode_storage::{MessageWithParts, Session, Todo};
use reqwest::{Method, StatusCode, Url};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

pub const DEFAULT_USERNAME: &str = "opencode";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Url(String),
    #[error("{0}")]
    Transport(#[from] reqwest::Error),
    // A non-2xx answer; `body` is the decoded JSON body, or the raw text as a string.
    #[error("{}", error_message(*status, body))]
    Status { status: u16, body: Value },
    #[error("{0}")]
    Decode(#[from] serde_json::Error),
}

impl Error {
    pub fn status(&self) -> Option<u16> {
        match self {
            Error::Status { status, .. } => Some(*status),
            _ => None,
        }
    }

    pub fn body(&self) -> Option<&Value> {
        match self {
            Error::Status { body, .. } => Some(body),
            _ => None,
        }
    }
}

// `errorMessage` of the TUI: `message`, then `data.message`, then the error's name, then the status.
fn error_message(status: u16, body: &Value) -> String {
    let text = |value: Option<&Value>| value.and_then(Value::as_str).filter(|text| !text.is_empty()).map(str::to_string);
    text(body.get("message"))
        .or_else(|| text(body.pointer("/data/message")))
        .or_else(|| text(body.get("name")))
        .or_else(|| text(body.get("_tag")))
        .or_else(|| body.as_str().filter(|text| !text.trim().is_empty()).map(str::to_string))
        .unwrap_or_else(|| format!("HTTP {status}"))
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

// HTTP Basic credentials (`ServerAuth.header`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl Credentials {
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self { username: username.into(), password: password.into() }
    }

    // An explicit password, else OPENCODE_SERVER_PASSWORD; no password (or an empty one) means no header. The
    // username is the explicit one, else OPENCODE_SERVER_USERNAME, else "opencode".
    pub fn resolve(password: Option<String>, username: Option<String>, env: &dyn Fn(&str) -> Option<String>) -> Option<Self> {
        let password = password.or_else(|| env("OPENCODE_SERVER_PASSWORD")).filter(|password| !password.is_empty())?;
        let username = username.or_else(|| env("OPENCODE_SERVER_USERNAME")).unwrap_or_else(|| DEFAULT_USERNAME.to_string());
        Some(Self { username, password })
    }

    pub fn from_env(password: Option<String>, username: Option<String>) -> Option<Self> {
        Self::resolve(password, username, &|key| std::env::var(key).ok())
    }

    pub fn header(&self) -> String {
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", self.username, self.password));
        format!("Basic {token}")
    }
}

// `encodeURIComponent`.
pub fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => out.push(byte as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    directory: Option<String>,
    auth: Option<String>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").field("base", &self.base).field("directory", &self.directory).field("auth", &self.auth.is_some()).finish()
    }
}

impl Client {
    pub fn new(base_url: &str) -> Self {
        Self { http: reqwest::Client::new(), base: base_url.trim_end_matches('/').to_string(), directory: None, auth: None }
    }

    // Every request names this instance directory: a `?directory=` query on GET/HEAD, an
    // `x-opencode-directory: encodeURIComponent(dir)` header otherwise, as the SDK sends it.
    pub fn with_directory(mut self, directory: Option<String>) -> Self {
        self.directory = directory.filter(|directory| !directory.is_empty());
        self
    }

    pub fn with_credentials(mut self, credentials: Option<Credentials>) -> Self {
        self.auth = credentials.map(|credentials| credentials.header());
        self
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    pub fn directory(&self) -> Option<&str> {
        self.directory.as_deref()
    }

    fn url(&self, method: &Method, path: &str, query: &[(&str, String)]) -> Result<Url> {
        let mut url = Url::parse(&format!("{}{path}", self.base)).map_err(|error| Error::Url(format!("{error}: {}{path}", self.base)))?;
        let directory = self.directory.as_ref().filter(|_| *method == Method::GET || *method == Method::HEAD);
        if !query.is_empty() || directory.is_some() {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in query {
                pairs.append_pair(key, value);
            }
            if let Some(directory) = directory.filter(|_| !query.iter().any(|(key, _)| *key == "directory")) {
                pairs.append_pair("directory", directory);
            }
        }
        Ok(url)
    }

    pub(crate) fn request(&self, method: Method, path: &str, query: &[(&str, String)]) -> Result<reqwest::RequestBuilder> {
        let url = self.url(&method, path, query)?;
        let mut request = self.http.request(method.clone(), url);
        if let Some(auth) = &self.auth {
            request = request.header("authorization", auth);
        }
        if let Some(directory) = self.directory.as_ref().filter(|_| method != Method::GET && method != Method::HEAD) {
            request = request.header("x-opencode-directory", encode_component(directory));
        }
        Ok(request)
    }

    async fn send(&self, method: Method, path: &str, query: &[(&str, String)], body: Option<Value>) -> Result<reqwest::Response> {
        let mut request = self.request(method, path, query)?;
        if let Some(body) = body {
            request = request.header("content-type", "application/json").body(body.to_string());
        }
        let response = request.send().await?;
        check(response).await
    }

    async fn call<T: DeserializeOwned>(&self, method: Method, path: &str, query: &[(&str, String)], body: Option<Value>) -> Result<T> {
        let response = self.send(method, path, query, body).await?;
        let bytes = response.bytes().await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.call(Method::GET, path, &[], None).await
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, body: impl Serialize) -> Result<T> {
        self.call(Method::POST, path, &[], Some(serde_json::to_value(body)?)).await
    }

    // `GET /global/health`
    pub async fn health(&self) -> Result<Health> {
        self.get("/global/health").await
    }

    // `GET /path`
    pub async fn path(&self) -> Result<PathInfo> {
        self.get("/path").await
    }

    // `GET /config`: the instance config as loaded.
    pub async fn config(&self) -> Result<Value> {
        self.get("/config").await
    }

    // `GET /config/providers`
    pub async fn config_providers(&self) -> Result<ProviderList> {
        self.get("/config/providers").await
    }

    // `GET /agent`
    pub async fn agents(&self) -> Result<Vec<Agent>> {
        self.get("/agent").await
    }

    // `GET /command`
    pub async fn commands(&self) -> Result<Vec<Command>> {
        self.get("/command").await
    }

    // `GET /session`
    pub async fn sessions(&self, query: &SessionQuery) -> Result<Vec<Session>> {
        let mut pairs: Vec<(&str, String)> = Vec::new();
        if let Some(roots) = query.roots {
            pairs.push(("roots", roots.to_string()));
        }
        if let Some(start) = query.start {
            pairs.push(("start", start.to_string()));
        }
        if let Some(search) = &query.search {
            pairs.push(("search", search.clone()));
        }
        if let Some(limit) = query.limit {
            pairs.push(("limit", limit.to_string()));
        }
        self.call(Method::GET, "/session", &pairs, None).await
    }

    // `GET /session/status`: busy and retrying sessions; absent ones are idle.
    pub async fn status(&self) -> Result<indexmap::IndexMap<String, SessionStatus>> {
        self.get("/session/status").await
    }

    // `GET /session/{id}`
    pub async fn session(&self, session_id: &str) -> Result<Session> {
        self.get(&format!("/session/{}", encode_component(session_id))).await
    }

    // `POST /session`
    pub async fn create_session(&self, input: &CreateSession) -> Result<Session> {
        self.post("/session", input).await
    }

    // `PATCH /session/{id}`
    pub async fn update_session(&self, session_id: &str, patch: &SessionPatch) -> Result<Session> {
        self.call(Method::PATCH, &format!("/session/{}", encode_component(session_id)), &[], Some(serde_json::to_value(patch)?)).await
    }

    // `DELETE /session/{id}`
    pub async fn delete_session(&self, session_id: &str) -> Result<bool> {
        self.call(Method::DELETE, &format!("/session/{}", encode_component(session_id)), &[], None).await
    }

    // `GET /session/{id}/children`
    pub async fn children(&self, session_id: &str) -> Result<Vec<Session>> {
        self.get(&format!("/session/{}/children", encode_component(session_id))).await
    }

    // `GET /session/{id}/todo`
    pub async fn todos(&self, session_id: &str) -> Result<Vec<Todo>> {
        self.get(&format!("/session/{}/todo", encode_component(session_id))).await
    }

    // `GET /session/{id}/message`; `limit` returns the newest messages, oldest first.
    pub async fn messages(&self, session_id: &str, limit: Option<usize>) -> Result<Vec<MessageWithParts>> {
        let query: Vec<(&str, String)> = limit.map(|limit| ("limit", limit.to_string())).into_iter().collect();
        self.call(Method::GET, &format!("/session/{}/message", encode_component(session_id)), &query, None).await
    }

    // `POST /session/{id}/message`: runs the turn and answers with the message it produced.
    pub async fn prompt(&self, session_id: &str, body: &PromptBody) -> Result<MessageWithParts> {
        self.post(&format!("/session/{}/message", encode_component(session_id)), body).await
    }

    // `POST /session/{id}/prompt_async`: starts the turn and answers 204 at once.
    pub async fn prompt_async(&self, session_id: &str, body: &PromptBody) -> Result<()> {
        self.send(Method::POST, &format!("/session/{}/prompt_async", encode_component(session_id)), &[], Some(serde_json::to_value(body)?)).await?;
        Ok(())
    }

    // `POST /session/{id}/command`
    pub async fn command(&self, session_id: &str, body: &CommandBody) -> Result<MessageWithParts> {
        self.post(&format!("/session/{}/command", encode_component(session_id)), body).await
    }

    // `POST /session/{id}/shell`
    pub async fn shell(&self, session_id: &str, body: &ShellBody) -> Result<MessageWithParts> {
        self.post(&format!("/session/{}/shell", encode_component(session_id)), body).await
    }

    // `POST /session/{id}/abort`
    pub async fn abort(&self, session_id: &str) -> Result<bool> {
        self.post(&format!("/session/{}/abort", encode_component(session_id)), json!({})).await
    }

    // `POST /session/{id}/fork`; without a message the whole session is copied.
    pub async fn fork(&self, session_id: &str, message_id: Option<&str>) -> Result<Session> {
        let body = match message_id {
            Some(message_id) => json!({"messageID": message_id}),
            None => json!({}),
        };
        self.post(&format!("/session/{}/fork", encode_component(session_id)), body).await
    }

    // `POST /session/{id}/summarize`
    pub async fn summarize(&self, session_id: &str, provider_id: &str, model_id: &str, auto: bool) -> Result<bool> {
        let body = json!({"providerID": provider_id, "modelID": model_id, "auto": auto});
        self.post(&format!("/session/{}/summarize", encode_component(session_id)), body).await
    }

    // `POST /session/{id}/revert`
    pub async fn revert(&self, session_id: &str, message_id: &str, part_id: Option<&str>) -> Result<Session> {
        let mut body = json!({"messageID": message_id});
        if let Some(part_id) = part_id {
            body["partID"] = json!(part_id);
        }
        self.post(&format!("/session/{}/revert", encode_component(session_id)), body).await
    }

    // `POST /session/{id}/unrevert`
    pub async fn unrevert(&self, session_id: &str) -> Result<Session> {
        self.post(&format!("/session/{}/unrevert", encode_component(session_id)), json!({})).await
    }

    // `GET /permission`
    pub async fn permissions(&self) -> Result<Vec<PermissionRequest>> {
        self.get("/permission").await
    }

    // `POST /permission/{requestID}/reply`; a rejection's `message` is feedback for the model.
    pub async fn reply_permission(&self, request_id: &str, reply: PermissionReply, message: Option<&str>) -> Result<bool> {
        let mut body = json!({"reply": reply});
        if let Some(message) = message {
            body["message"] = json!(message);
        }
        self.post(&format!("/permission/{}/reply", encode_component(request_id)), body).await
    }

    // `GET /question`
    pub async fn questions(&self) -> Result<Vec<QuestionRequest>> {
        self.get("/question").await
    }

    // `POST /question/{requestID}/reply`: one list of answers per question.
    pub async fn reply_question(&self, request_id: &str, answers: &[Vec<String>]) -> Result<bool> {
        self.post(&format!("/question/{}/reply", encode_component(request_id)), json!({"answers": answers})).await
    }

    // `POST /question/{requestID}/reject`
    pub async fn reject_question(&self, request_id: &str) -> Result<bool> {
        self.post(&format!("/question/{}/reject", encode_component(request_id)), json!({})).await
    }

    // `GET /event`: this directory's events. Fails if the first connection fails; later drops reconnect.
    pub async fn events(&self) -> Result<EventStream> {
        self.events_with(Reconnect::default()).await
    }

    pub async fn events_with(&self, reconnect: Reconnect) -> Result<EventStream> {
        sse::subscribe(self.clone(), "/event", false, reconnect).await
    }

    // `GET /global/event`: every directory's events, each frame carrying its directory.
    pub async fn global_events(&self) -> Result<EventStream> {
        self.global_events_with(Reconnect::default()).await
    }

    pub async fn global_events_with(&self, reconnect: Reconnect) -> Result<EventStream> {
        sse::subscribe(self.clone(), "/global/event", true, reconnect).await
    }
}

pub(crate) async fn check(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let text = response.text().await.unwrap_or_default();
    let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
    Err(Error::Status { status: status.as_u16(), body })
}

pub fn is_not_found(error: &Error) -> bool {
    error.status() == Some(StatusCode::NOT_FOUND.as_u16())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_follow_server_auth_header() {
        let env = |key: &str| match key {
            "OPENCODE_SERVER_PASSWORD" => Some("secret".to_string()),
            "OPENCODE_SERVER_USERNAME" => Some("admin".to_string()),
            _ => None,
        };
        let none = |_: &str| None;
        assert_eq!(Credentials::resolve(None, None, &env), Some(Credentials::new("admin", "secret")));
        assert_eq!(Credentials::resolve(Some("p".into()), Some("u".into()), &env), Some(Credentials::new("u", "p")));
        assert_eq!(Credentials::resolve(Some("p".into()), None, &none), Some(Credentials::new("opencode", "p")));
        assert_eq!(Credentials::resolve(None, Some("u".into()), &none), None);
        assert_eq!(Credentials::resolve(Some(String::new()), None, &env), None);
        assert_eq!(Credentials::new("opencode", "pw").header(), "Basic b3BlbmNvZGU6cHc=");
    }

    #[test]
    fn directory_moves_into_the_query_for_reads_only() {
        let client = Client::new("http://127.0.0.1:4096/").with_directory(Some("/work/my dir".into()));
        let get = client.url(&Method::GET, "/session", &[("roots", "true".into())]).unwrap();
        assert_eq!(get.as_str(), "http://127.0.0.1:4096/session?roots=true&directory=%2Fwork%2Fmy+dir");
        let post = client.url(&Method::POST, "/session", &[]).unwrap();
        assert_eq!(post.as_str(), "http://127.0.0.1:4096/session");
        let request = client.request(Method::POST, "/session", &[]).unwrap().build().unwrap();
        assert_eq!(request.headers()["x-opencode-directory"], "%2Fwork%2Fmy%20dir");
        let read = client.request(Method::GET, "/path", &[]).unwrap().build().unwrap();
        assert!(read.headers().get("x-opencode-directory").is_none());
        assert_eq!(encode_component("C:\\a b(1)!"), "C%3A%5Ca%20b(1)!");
    }

    #[test]
    fn error_messages_prefer_the_body() {
        let not_found = Error::Status { status: 404, body: json!({"name": "NotFoundError", "data": {"message": "Session not found: ses_x"}}) };
        assert_eq!(not_found.to_string(), "Session not found: ses_x");
        assert!(is_not_found(&not_found));
        assert_eq!(Error::Status { status: 401, body: Value::String(String::new()) }.to_string(), "HTTP 401");
        assert_eq!(Error::Status { status: 400, body: json!({"name": "BadRequest", "data": {}}) }.to_string(), "BadRequest");
    }
}
