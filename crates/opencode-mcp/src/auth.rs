//! `<data>/mcp-auth.json` store (`mcp/auth.ts`). Same JSON layout and `0600` mode as the original.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use indexmap::IndexMap;
use opencode_core::credential;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};
use sha1::{Digest, Sha1};
use tokio::sync::Mutex;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id_issued_at: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret_expires_at: Option<Number>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_info: Option<ClientInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_verifier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_url: Option<String>,
}

pub type AuthData = IndexMap<String, Entry>;

pub(crate) fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// JSON number as `JSON.stringify` would print it (no `.0` on integral values).
pub(crate) fn number(value: f64) -> Option<Number> {
    if value.fract() == 0.0 && value.abs() < 9e15 {
        return Some(Number::from(value as i64));
    }
    Number::from_f64(value)
}

pub(crate) fn expired(at: &Option<Number>) -> bool {
    at.as_ref()
        .and_then(Number::as_f64)
        .is_some_and(|at| at != 0.0 && at < now_seconds())
}

pub struct AuthStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl AuthStore {
    /// `data_dir` is `Global.Path.data`.
    pub fn new(data_dir: &Path) -> Self {
        Self::at(data_dir.join("mcp-auth.json"))
    }

    pub fn at(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self) -> AuthData {
        std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn write(&self, data: &AuthData) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(data).map_err(std::io::Error::other)?;
        std::fs::write(&self.path, content)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    async fn mutate(&self, update: impl FnOnce(&mut AuthData) -> bool) {
        let _guard = self.lock.lock().await;
        let mut data = self.read();
        if update(&mut data) {
            let _ = self.write(&data);
        }
    }

    pub async fn all(&self) -> AuthData {
        let _guard = self.lock.lock().await;
        self.read()
    }

    pub async fn get(&self, name: &str) -> Option<Entry> {
        self.all().await.shift_remove(name)
    }

    /// Credentials are only valid for the URL they were issued for.
    pub async fn get_for_url(&self, name: &str, server_url: &str) -> Option<Entry> {
        let Some(entry) = self.get(name).await else {
            return self.v2_entry(name, server_url);
        };
        Some(entry).filter(|entry| {
            entry
                .server_url
                .as_deref()
                .is_some_and(|url| url == server_url)
        })
    }

    /// Rust build: a server with no entry here takes the login opencode 2.x keeps in `<data>/opencode.db`
    /// (read-only). Tokens refreshed by this build are saved to this file, which then takes over.
    fn v2_entry(&self, name: &str, server_url: &str) -> Option<Entry> {
        let db = self.path.parent()?.join(credential::DATABASE_FILE);
        entry_from_credential(&credential::active_for(&db, &v2_integration_id(name, server_url))?, server_url)
    }
}

/// The integration id 2.x registers a remote server's OAuth credential under (`mcp/index.ts` `register`).
pub fn v2_integration_id(name: &str, url: &str) -> String {
    let digest = Sha1::digest(format!("{name}\u{0}{url}").as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("mcp_{}", &hex[..16])
}

/// A 2.x OAuth credential (`mcp/oauth.ts` `toCredential`) as an entry of this store.
fn entry_from_credential(value: &Value, server_url: &str) -> Option<Entry> {
    if value.get("type")?.as_str()? != "oauth" {
        return None;
    }
    let metadata = value.get("metadata").and_then(Value::as_object);
    let text = |map: Option<&Map<String, Value>>, key: &str| {
        map.and_then(|m| m.get(key))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    // `expires` is epoch milliseconds, 0 for a token without a lifetime.
    let expires = value.get("expires").and_then(Value::as_f64).filter(|ms| *ms > 0.0);
    let tokens = Tokens {
        access_token: text(value.as_object(), "access")?,
        refresh_token: text(value.as_object(), "refresh"),
        expires_at: expires.and_then(|ms| number((ms / 1000.0).floor())),
        scope: text(metadata, "scope"),
    };
    let client = metadata
        .and_then(|m| m.get("client"))
        .and_then(Value::as_object);
    let client_info = text(client, "client_id").map(|client_id| ClientInfo {
        client_id,
        client_secret: text(client, "client_secret"),
        client_id_issued_at: client
            .and_then(|c| c.get("client_id_issued_at"))
            .and_then(Value::as_f64)
            .and_then(number),
        client_secret_expires_at: client
            .and_then(|c| c.get("client_secret_expires_at"))
            .and_then(Value::as_f64)
            .and_then(number),
    });
    Some(Entry {
        tokens: Some(tokens),
        client_info,
        server_url: Some(server_url.to_string()),
        ..Entry::default()
    })
}

impl AuthStore {

    pub async fn set(&self, name: &str, mut entry: Entry, server_url: Option<&str>) {
        if let Some(url) = server_url.filter(|url| !url.is_empty()) {
            entry.server_url = Some(url.to_string());
        }
        self.mutate(|data| {
            data.insert(name.to_string(), entry);
            true
        })
        .await
    }

    pub async fn remove(&self, name: &str) {
        self.mutate(|data| {
            data.shift_remove(name);
            true
        })
        .await
    }

    async fn update(&self, name: &str, server_url: Option<&str>, apply: impl FnOnce(&mut Entry)) {
        self.mutate(|data| {
            let entry = data.entry(name.to_string()).or_default();
            apply(entry);
            if let Some(url) = server_url.filter(|url| !url.is_empty()) {
                entry.server_url = Some(url.to_string());
            }
            true
        })
        .await
    }

    async fn clear(&self, name: &str, apply: impl FnOnce(&mut Entry)) {
        self.mutate(|data| match data.get_mut(name) {
            Some(entry) => {
                apply(entry);
                true
            }
            None => false,
        })
        .await
    }

    pub async fn update_tokens(&self, name: &str, tokens: Tokens, server_url: Option<&str>) {
        self.update(name, server_url, |e| e.tokens = Some(tokens))
            .await
    }

    pub async fn update_client_info(&self, name: &str, info: ClientInfo, server_url: Option<&str>) {
        self.update(name, server_url, |e| e.client_info = Some(info))
            .await
    }

    pub async fn update_code_verifier(&self, name: &str, verifier: &str) {
        self.update(name, None, |e| e.code_verifier = Some(verifier.to_string()))
            .await
    }

    pub async fn clear_code_verifier(&self, name: &str) {
        self.clear(name, |e| e.code_verifier = None).await
    }

    pub async fn update_oauth_state(&self, name: &str, state: &str) {
        self.update(name, None, |e| e.oauth_state = Some(state.to_string()))
            .await
    }

    pub async fn get_oauth_state(&self, name: &str) -> Option<String> {
        self.get(name).await.and_then(|entry| entry.oauth_state)
    }

    pub async fn clear_oauth_state(&self, name: &str) {
        self.clear(name, |e| e.oauth_state = None).await
    }
}
