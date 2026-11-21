// Shared pieces of the built-in login flows: randomness, PKCE, form bodies, and a loopback callback listener.
use crate::Error;
use base64::Engine;
use opencode_core::auth;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub use crate::copilot::Poll;

pub const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);
pub const POLL_MARGIN_MS: u64 = 3000;
const VERIFIER_CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";

pub(crate) fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub fn random_bytes(len: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; len];
    if getrandom::fill(&mut bytes).is_err() {
        bytes.iter_mut().for_each(|b| *b = crate::flock::random() as u8);
    }
    bytes
}

pub fn random_string(len: usize) -> String {
    random_bytes(len).into_iter().map(|b| VERIFIER_CHARS[b as usize % VERIFIER_CHARS.len()] as char).collect()
}

pub fn base64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

pub fn challenge(verifier: &str) -> String {
    base64url(&crate::sigv4::sha256(verifier.as_bytes()))
}

pub fn pkce(len: usize) -> Pkce {
    let verifier = random_string(len);
    Pkce { challenge: challenge(&verifier), verifier }
}

// application/x-www-form-urlencoded, as URLSearchParams serializes it.
pub fn form(pairs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (key, value) in pairs {
        if !out.is_empty() {
            out.push('&');
        }
        for (i, text) in [key, value].into_iter().enumerate() {
            if i == 1 {
                out.push('=');
            }
            for byte in text.bytes() {
                match byte {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => out.push(byte as char),
                    b' ' => out.push('+'),
                    _ => out.push_str(&format!("%{byte:02X}")),
                }
            }
        }
    }
    out
}

pub fn parse_query(query: &str) -> Vec<(String, String)> {
    reqwest::Url::parse(&format!("http://localhost/?{query}")).map(|u| u.query_pairs().map(|(k, v)| (k.to_string(), v.to_string())).collect()).unwrap_or_default()
}

// Stores a finished login under `provider_id` in auth.json.
pub fn store(auth_file: &Path, provider_id: &str, info: auth::Info) -> Result<(), Error> {
    auth::set(auth_file, provider_id, info).map_err(|e| Error::Config(format!("failed to save {provider_id} credentials: {e}")))
}

// `exp` (seconds) of a JWT at or before now + skew.
pub fn jwt_expiring(token: &str, skew_ms: u64) -> bool {
    let exp = crate::codex::jwt_claims(token).and_then(|c| c.get("exp").and_then(serde_json::Value::as_f64));
    exp.is_some_and(|exp| exp * 1000.0 <= (now_ms() + skew_ms) as f64)
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Callback {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub body: String,
}

impl Callback {
    pub fn param(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

pub struct Reply {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
}

impl Reply {
    pub fn html(status: u16, title: &str, detail: &str) -> Self {
        let escape = |text: &str| text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
        let body = format!("<!doctype html><html><head><meta charset=\"utf-8\"><title>opencode</title></head><body><h1>{}</h1><p>{}</p></body></html>", escape(title), escape(detail));
        Reply { status, content_type: "text/html; charset=utf-8", body }
    }

    pub fn json(status: u16, body: serde_json::Value) -> Self {
        Reply { status, content_type: "application/json", body: body.to_string() }
    }

    pub fn text(status: u16, body: &str) -> Self {
        Reply { status, content_type: "text/plain", body: body.to_string() }
    }
}

// Loopback HTTP listener for OAuth redirects. Dropping it frees the port.
pub struct Listener {
    listener: tokio::net::TcpListener,
    pub port: u16,
}

impl Listener {
    pub async fn bind(host: &str, port: u16) -> Result<Self, Error> {
        let listener = tokio::net::TcpListener::bind((host, port)).await.map_err(|e| Error::Config(format!("cannot listen on {host}:{port} for the OAuth callback: {e}")))?;
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
        Ok(Listener { listener, port })
    }

    // Serves requests until `handle` returns a value; its reply is always sent first.
    pub async fn serve<T>(&self, mut handle: impl FnMut(&Callback) -> (Reply, Option<T>)) -> Result<T, Error> {
        loop {
            let (mut socket, _) = self.listener.accept().await.map_err(|e| Error::Config(format!("OAuth callback listener failed: {e}")))?;
            let Some(request) = read_request(&mut socket).await else { continue };
            let (reply, done) = handle(&request);
            let head = format!("HTTP/1.1 {} OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", reply.status, reply.content_type, reply.body.len());
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(reply.body.as_bytes()).await;
            let _ = socket.shutdown().await;
            if let Some(value) = done {
                return Ok(value);
            }
        }
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<Callback> {
    let mut raw = Vec::new();
    let mut buffer = [0u8; 8192];
    let read = async {
        let (head_end, length) = loop {
            let n = socket.read(&mut buffer).await.ok().filter(|n| *n > 0)?;
            raw.extend_from_slice(&buffer[..n]);
            if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&raw[..end]).to_lowercase();
                let length = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0);
                break (end + 4, length.min(1 << 20));
            }
            if raw.len() > 1 << 16 {
                return None;
            }
        };
        while raw.len() < head_end + length {
            let n = socket.read(&mut buffer).await.ok().filter(|n| *n > 0)?;
            raw.extend_from_slice(&buffer[..n]);
        }
        Some(head_end)
    };
    let head_end = tokio::time::timeout(Duration::from_secs(10), read).await.ok()??;
    let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
    let mut line = head.lines().next()?.split(' ');
    let method = line.next()?.to_string();
    let target = line.next()?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    Some(Callback { method, path: path.to_string(), query: parse_query(query), body: String::from_utf8_lossy(&raw[head_end..]).to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_forms_and_jwt_expiry() {
        // RFC 7636 appendix B.
        assert_eq!(challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        let codes = pkce(43);
        assert_eq!((codes.verifier.len(), codes.challenge.len()), (43, 43));
        assert!(codes.verifier.bytes().all(|b| VERIFIER_CHARS.contains(&b)));
        assert_ne!(random_string(32), random_string(32));
        assert_eq!(form(&[("scope", "openid profile"), ("redirect_uri", "http://localhost:1455/auth/callback"), ("x", "a~b*c")]), "scope=openid+profile&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback&x=a%7Eb*c");
        assert_eq!(parse_query("code=a%20b&state=s+1"), [("code".to_string(), "a b".to_string()), ("state".to_string(), "s 1".to_string())]);
        let soon = crate::codex::tests::jwt(&serde_json::json!({"exp": now_ms() / 1000 + 60}));
        let later = crate::codex::tests::jwt(&serde_json::json!({"exp": now_ms() / 1000 + 3600}));
        assert!(jwt_expiring(&soon, 120_000) && !jwt_expiring(&later, 120_000) && !jwt_expiring("opaque", 120_000));
    }
}
