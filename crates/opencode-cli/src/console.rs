// `opencode console *`, after `opencode/src/cli/cmd/account.ts`.
use crate::prompt::{self, Choice};
use crate::ui;
use anyhow::Result;
use opencode_core::paths::Paths;
use opencode_session::Engine;
use opencode_session::account::{Accounts, Poll};
use opencode_storage::account::AccountInfo;
use std::time::{Duration, Instant};

pub const DEFAULT_URL: &str = "https://opencode.ai/console";

fn dim(value: &str) -> String {
    format!("{}{value}{}", ui::DIM, ui::NORMAL)
}

fn active_suffix(active: bool) -> String {
    if active { dim(" (active)") } else { String::new() }
}

fn accounts(paths: &Paths) -> Result<Accounts> {
    let engine = Engine::new(paths.clone())?;
    Ok(Accounts::new(engine.store.clone(), reqwest::Client::new()))
}

async fn open_browser(url: &str) {
    if std::env::var("OPENCODE_TEST_NO_BROWSER").is_err() {
        let _ = opencode_mcp::browser::open(url).await;
    }
}

pub async fn login(paths: &Paths, url: Option<String>) -> Result<i32> {
    let service = accounts(paths)?;
    ui::empty();
    ui::intro("Log in");
    let login = service.login(url.as_deref().unwrap_or(DEFAULT_URL)).await?;
    ui::info(&format!("Go to: {}", login.url));
    ui::info(&format!("Enter code: {}", login.user));
    open_browser(&login.url).await;
    ui::step("Waiting for authorization...");
    let deadline = Instant::now() + login.expiry;
    let mut wait = login.interval;
    let result = loop {
        if Instant::now() + wait > deadline {
            break Poll::Expired;
        }
        tokio::time::sleep(wait).await;
        match service.poll(&login).await? {
            Poll::Pending => {}
            Poll::Slow => wait += Duration::from_secs(5),
            other => break other,
        }
    };
    Ok(match result {
        Poll::Success { email } => {
            ui::success(&format!("Logged in as {email}"));
            ui::outro("Done");
            0
        }
        Poll::Expired => fail("Device code expired"),
        Poll::Denied => fail("Authorization denied"),
        Poll::Error(cause) => fail(&format!("Error: {cause}")),
        Poll::Pending | Poll::Slow => fail("Unexpected state"),
    })
}

fn fail(message: &str) -> i32 {
    ui::failure(message);
    1
}

pub async fn logout(paths: &Paths, email: Option<String>) -> Result<i32> {
    let service = accounts(paths)?;
    ui::empty();
    let list = service.list()?;
    if list.is_empty() {
        ui::println("Not logged in");
        return Ok(0);
    }
    if let Some(email) = email {
        let Some(found) = list.iter().find(|a| a.email == email) else {
            ui::println(&format!("Account not found: {email}"));
            return Ok(0);
        };
        service.remove(&found.id).await?;
        ui::outro(&format!("Logged out from {email}"));
        return Ok(0);
    }
    let active = service.active()?.map(|a| a.id);
    ui::intro("Log out");
    let choices = list
        .iter()
        .map(|a| Choice::new(format!("{} {}{}", a.email, dim(&a.url), active_suffix(active.as_deref() == Some(a.id.as_str())))))
        .collect();
    let index = prompt::select("Select account to log out", choices, 0, 10)?;
    let selected = &list[index];
    service.remove(&selected.id).await?;
    ui::outro(&format!("Logged out from {}", selected.email));
    Ok(0)
}

fn is_active(active: &Option<AccountInfo>, account_id: &str, org_id: &str) -> bool {
    active.as_ref().is_some_and(|a| a.id == account_id && a.active_org_id.as_deref() == Some(org_id))
}

pub async fn switch(paths: &Paths) -> Result<i32> {
    let service = accounts(paths)?;
    ui::empty();
    let groups = service.orgs_by_account().await?;
    if groups.is_empty() {
        ui::println("Not logged in");
        return Ok(0);
    }
    let active = service.active()?;
    let options: Vec<(String, String, String, String)> = groups
        .iter()
        .flat_map(|(account, orgs)| {
            orgs.iter().map(|org| {
                let label = format!("{} ({}){}", org.name, account.email, active_suffix(is_active(&active, &account.id, &org.id)));
                (account.id.clone(), org.id.clone(), org.name.clone(), label)
            })
        })
        .collect();
    if options.is_empty() {
        ui::println("No orgs found");
        return Ok(0);
    }
    ui::intro("Switch org");
    let index = prompt::select("Select org", options.iter().map(|o| Choice::new(o.3.clone())).collect(), 0, 10)?;
    let (account_id, org_id, name, _) = &options[index];
    service.use_org(account_id, Some(org_id))?;
    ui::outro(&format!("Switched to {name}"));
    Ok(0)
}

pub async fn orgs(paths: &Paths) -> Result<i32> {
    let service = accounts(paths)?;
    ui::empty();
    let groups = service.orgs_by_account().await?;
    if groups.is_empty() {
        ui::println("No accounts found");
        return Ok(0);
    }
    if !groups.iter().any(|(_, orgs)| !orgs.is_empty()) {
        ui::println("No orgs found");
        return Ok(0);
    }
    let active = service.active()?;
    for (account, orgs) in &groups {
        for org in orgs {
            let on = is_active(&active, &account.id, &org.id);
            let dot = if on { format!("{}●{}", ui::SUCCESS_BOLD, ui::NORMAL) } else { " ".to_string() };
            let name = if on { format!("{}{}{}", ui::INFO_BOLD, org.name, ui::NORMAL) } else { org.name.clone() };
            ui::println(&format!("  {dot} {name}  {}  {}  {}", dim(&account.email), dim(&account.url), dim(&org.id)));
        }
    }
    Ok(0)
}

pub async fn open(paths: &Paths) -> Result<i32> {
    let service = accounts(paths)?;
    ui::empty();
    let Some(active) = service.active()? else {
        ui::println("No active account");
        return Ok(0);
    };
    open_browser(&active.url).await;
    ui::outro(&format!("Opened {}", active.url));
    Ok(0)
}
