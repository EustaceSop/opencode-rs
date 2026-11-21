use crate::credential;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::path::Path;
use std::sync::{LazyLock, Mutex};
use std::{env, fs, io};

pub const OAUTH_DUMMY_KEY: &str = "opencode-oauth-dummy-key";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Info {
    Oauth {
        refresh: String,
        access: String,
        expires: u64,
        #[serde(rename = "accountId", default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
        #[serde(rename = "enterpriseUrl", default, skip_serializing_if = "Option::is_none")]
        enterprise_url: Option<String>,
    },
    Api {
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<Map<String, Value>>,
    },
    Wellknown {
        key: String,
        token: String,
    },
}

pub type Entries = Vec<(String, Info)>;

fn decode(text: &str) -> Entries {
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    map.into_iter()
        .filter_map(|(k, v)| serde_json::from_value(v).ok().map(|info| (k, info)))
        .collect()
}

// Missing or invalid file yields an empty set; entries that fail to decode are dropped.
pub fn read_all(file: &Path) -> Entries {
    if let Ok(content) = env::var("OPENCODE_AUTH_CONTENT") {
        return decode(&content);
    }
    let mut entries = read_file(file);
    if let Some(dir) = file.parent() {
        merge_v2(&mut entries, credential::active(&dir.join(credential::DATABASE_FILE)));
    }
    entries
}

// The entries of the file alone: what `set` and `remove` rewrite, and what a logout can remove.
pub fn read_file(file: &Path) -> Entries {
    fs::read_to_string(file).map(|t| decode(&t)).unwrap_or_default()
}

// Rust build: a provider missing from auth.json takes the login opencode 2.x selected in its database (read-only);
// tokens this build refreshes still go to auth.json through `set`. When both have one, auth.json wins.
fn merge_v2(entries: &mut Entries, rows: Vec<(String, Value)>) {
    static WARNED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);
    for (id, value) in rows {
        // 2.x keeps well-known logins under their URL without the environment variable name 1.x stored.
        if id.contains("://") {
            continue;
        }
        let Some(info) = from_credential(&value) else { continue };
        match entries.iter().find(|(key, _)| norm(key) == id) {
            Some((_, current)) if *current != info => {
                if WARNED.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone()) {
                    tracing::warn!(provider = %id, "auth.json and opencode.db disagree on this login; using auth.json");
                }
            }
            Some(_) => {}
            None => entries.push((id, info)),
        }
    }
}

// A 2.x `Credential.Value` as the auth.json entry 1.x stored: the reverse of `importLegacyCredentials`.
fn from_credential(value: &Value) -> Option<Info> {
    let metadata = value.get("metadata").and_then(Value::as_object);
    let text = |key: &str| metadata.and_then(|m| m.get(key)).and_then(Value::as_str).map(str::to_string);
    match value.get("type")?.as_str()? {
        "oauth" => Some(Info::Oauth {
            refresh: value.get("refresh")?.as_str()?.to_string(),
            access: value.get("access")?.as_str()?.to_string(),
            expires: value.get("expires")?.as_u64()?,
            account_id: text("accountID"),
            enterprise_url: text("enterpriseUrl"),
        }),
        "key" => {
            // Rows written before 2.1 kept form answers in `configuration`, which win over `metadata`.
            let mut merged = Map::new();
            for source in [metadata, value.get("configuration").and_then(Value::as_object)].into_iter().flatten() {
                merged.extend(source.iter().filter(|(_, v)| v.is_string()).map(|(k, v)| (k.clone(), v.clone())));
            }
            Some(Info::Api { key: value.get("key")?.as_str()?.to_string(), metadata: (!merged.is_empty()).then_some(merged) })
        }
        // An Azure CLI login: 1.x stored a placeholder OAuth entry naming the resource, the CLI keeps the token.
        "external" if value.get("methodID").and_then(Value::as_str) == Some("azure-cli") => Some(Info::Oauth {
            refresh: OAUTH_DUMMY_KEY.into(),
            access: OAUTH_DUMMY_KEY.into(),
            expires: 0,
            account_id: text("resourceName"),
            enterprise_url: None,
        }),
        _ => None,
    }
}

fn write_all(file: &Path, entries: &Entries) -> io::Result<()> {
    let mut map = Map::new();
    for (k, v) in entries {
        map.insert(k.clone(), serde_json::to_value(v).map_err(io::Error::other)?);
    }
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(file, serde_json::to_string_pretty(&Value::Object(map)).map_err(io::Error::other)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(file, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn norm(key: &str) -> &str {
    key.trim_end_matches('/')
}

pub fn set(file: &Path, key: &str, info: Info) -> io::Result<()> {
    let n = norm(key).to_string();
    let slashed = format!("{n}/");
    let mut entries = read_file(file);
    entries.retain(|(k, _)| (k == &n || k != key) && k != &slashed);
    match entries.iter_mut().find(|(k, _)| k == &n) {
        Some(slot) => slot.1 = info,
        None => entries.push((n, info)),
    }
    write_all(file, &entries)
}

pub fn remove(file: &Path, key: &str) -> io::Result<()> {
    let n = norm(key);
    let mut entries = read_file(file);
    entries.retain(|(k, _)| k != key && k != n);
    write_all(file, &entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_drop_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("auth.json");
        fs::write(
            &file,
            r#"{"anthropic":{"type":"api","key":"sk-x"},"bad":{"type":"nope"},
               "github-copilot":{"type":"oauth","refresh":"r","access":"a","expires":0}}"#,
        )
        .unwrap();
        let all = read_all(&file);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].0, "anthropic");
        set(&file, "https://x.dev/", Info::Wellknown { key: "K".into(), token: "t".into() }).unwrap();
        let all = read_all(&file);
        assert_eq!(all[2].0, "https://x.dev");
        remove(&file, "anthropic").unwrap();
        assert_eq!(read_all(&file).len(), 2);
        let text = fs::read_to_string(&file).unwrap();
        assert!(text.starts_with("{\n  \"github-copilot\": {\n    \"type\": \"oauth\""));
    }
}
