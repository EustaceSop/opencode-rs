// Console accounts (`account` / `account_state`, `opencode/src/account/repo.ts`). Tokens are stored in plain text.
use rusqlite::{OptionalExtension, params};

use crate::id::now_ms;
use crate::store::{Result, Store};

const ACCOUNT_STATE_ID: i64 = 1;

// `AccountV2.Info`: `active_org_id` is only known for the active account.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountInfo {
    pub id: String,
    pub email: String,
    pub url: String,
    pub active_org_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccountRow {
    pub id: String,
    pub email: String,
    pub url: String,
    pub access_token: String,
    pub refresh_token: String,
    // Epoch milliseconds.
    pub token_expiry: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct NewAccount {
    pub id: String,
    pub email: String,
    pub url: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expiry: i64,
    pub org_id: Option<String>,
}

// `normalizeServerUrl`: no query or fragment, no trailing slashes on the path.
pub fn normalize_server_url(input: &str) -> std::result::Result<String, url::ParseError> {
    let mut url = url::Url::parse(input)?;
    url.set_query(None);
    url.set_fragment(None);
    let path = url.path().trim_end_matches('/').to_string();
    let origin = url.origin().ascii_serialization();
    Ok(if path.is_empty() { origin } else { format!("{origin}{path}") })
}

fn state(connection: &rusqlite::Connection, account_id: &str, org_id: Option<&str>) -> Result<()> {
    connection.execute(
        "INSERT INTO account_state (id, active_account_id, active_org_id) VALUES (?1, ?2, ?3)
         ON CONFLICT(id) DO UPDATE SET active_account_id = excluded.active_account_id, active_org_id = excluded.active_org_id",
        params![ACCOUNT_STATE_ID, account_id, org_id],
    )?;
    Ok(())
}

impl Store {
    pub fn account_active(&self) -> Result<Option<AccountInfo>> {
        let connection = self.lock();
        let found = connection
            .query_row(
                "SELECT a.id, a.email, a.url, s.active_org_id FROM account_state s JOIN account a ON a.id = s.active_account_id WHERE s.id = ?1",
                [ACCOUNT_STATE_ID],
                |row| Ok(AccountInfo { id: row.get(0)?, email: row.get(1)?, url: row.get(2)?, active_org_id: row.get(3)? }),
            )
            .optional()?;
        Ok(found)
    }

    pub fn account_list(&self) -> Result<Vec<AccountInfo>> {
        let connection = self.lock();
        let mut statement = connection.prepare("SELECT id, email, url FROM account ORDER BY rowid")?;
        let rows = statement.query_map([], |row| Ok(AccountInfo { id: row.get(0)?, email: row.get(1)?, url: row.get(2)?, active_org_id: None }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn account_row(&self, account_id: &str) -> Result<Option<AccountRow>> {
        let connection = self.lock();
        let found = connection
            .query_row(
                "SELECT id, email, url, access_token, refresh_token, token_expiry FROM account WHERE id = ?1",
                [account_id],
                |row| {
                    Ok(AccountRow {
                        id: row.get(0)?,
                        email: row.get(1)?,
                        url: row.get(2)?,
                        access_token: row.get(3)?,
                        refresh_token: row.get(4)?,
                        token_expiry: row.get(5)?,
                    })
                },
            )
            .optional()?;
        Ok(found)
    }

    pub fn account_remove(&self, account_id: &str) -> Result<()> {
        self.transaction(|tx| {
            tx.execute("UPDATE account_state SET active_account_id = NULL, active_org_id = NULL WHERE active_account_id = ?1", [account_id])?;
            tx.execute("DELETE FROM account WHERE id = ?1", [account_id])?;
            Ok(())
        })
    }

    pub fn account_use(&self, account_id: &str, org_id: Option<&str>) -> Result<()> {
        state(&self.lock(), account_id, org_id)
    }

    pub fn account_persist_token(&self, account_id: &str, access_token: &str, refresh_token: &str, expiry: Option<i64>) -> Result<()> {
        self.lock().execute(
            "UPDATE account SET access_token = ?2, refresh_token = ?3, token_expiry = ?4, time_updated = ?5 WHERE id = ?1",
            params![account_id, access_token, refresh_token, expiry, now_ms() as i64],
        )?;
        Ok(())
    }

    // Inserts or updates the account and makes it the active one.
    pub fn account_persist(&self, input: &NewAccount) -> Result<()> {
        let url = normalize_server_url(&input.url).unwrap_or_else(|_| input.url.clone());
        let now = now_ms() as i64;
        self.transaction(|tx| {
            tx.execute(
                "INSERT INTO account (id, email, url, access_token, refresh_token, token_expiry, time_created, time_updated)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
                 ON CONFLICT(id) DO UPDATE SET email = excluded.email, url = excluded.url, access_token = excluded.access_token,
                   refresh_token = excluded.refresh_token, token_expiry = excluded.token_expiry, time_updated = excluded.time_updated",
                params![input.id, input.email, url, input.access_token, input.refresh_token, input.expiry, now],
            )?;
            state(tx, &input.id, input.org_id.as_deref())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str, email: &str, org: Option<&str>) -> NewAccount {
        NewAccount {
            id: id.into(),
            email: email.into(),
            url: "https://console.test/x/?q=1#f".into(),
            access_token: format!("at-{id}"),
            refresh_token: format!("rt-{id}"),
            expiry: 10,
            org_id: org.map(str::to_string),
        }
    }

    #[test]
    fn urls_are_normalized_like_the_original() {
        assert_eq!(normalize_server_url("https://opencode.ai/console").unwrap(), "https://opencode.ai/console");
        assert_eq!(normalize_server_url("https://opencode.ai/console///?a=1#x").unwrap(), "https://opencode.ai/console");
        assert_eq!(normalize_server_url("http://127.0.0.1:4000/").unwrap(), "http://127.0.0.1:4000");
        assert!(normalize_server_url("not a url").is_err());
    }

    #[test]
    fn persist_use_and_remove_track_the_active_account() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.account_active().unwrap(), None);
        store.account_persist(&account("a1", "a@x", Some("o1"))).unwrap();
        store.account_persist(&account("a2", "b@x", None)).unwrap();
        let active = store.account_active().unwrap().unwrap();
        assert_eq!((active.id.as_str(), active.url.as_str(), active.active_org_id), ("a2", "https://console.test/x", None));
        let listed: Vec<String> = store.account_list().unwrap().into_iter().map(|a| a.email).collect();
        assert_eq!(listed, ["a@x", "b@x"]);

        store.account_use("a1", Some("o2")).unwrap();
        assert_eq!(store.account_active().unwrap().unwrap().active_org_id.as_deref(), Some("o2"));
        store.account_persist_token("a1", "new", "next", Some(99)).unwrap();
        let row = store.account_row("a1").unwrap().unwrap();
        assert_eq!((row.access_token.as_str(), row.refresh_token.as_str(), row.token_expiry), ("new", "next", Some(99)));

        // Removing the active account clears the state row; removing another leaves it alone.
        store.account_remove("a2").unwrap();
        assert!(store.account_active().unwrap().is_some());
        store.account_remove("a1").unwrap();
        assert_eq!(store.account_active().unwrap(), None);
        assert!(store.account_list().unwrap().is_empty());
    }
}
