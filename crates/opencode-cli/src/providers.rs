// `opencode providers ...` (`opencode/src/cli/cmd/providers.ts`).
use crate::prompt::{self, Choice};
use crate::ui::{self, DIM, fail};
use anyhow::Result;
use opencode_core::{auth, config, paths::Paths};
use opencode_llm::{catalog, codex, copilot, provider};
use opencode_session::Engine;
use serde_json::Value;
use std::time::Duration;

fn kind(info: &auth::Info) -> &'static str {
    match info {
        auth::Info::Oauth { .. } => "oauth",
        auth::Info::Api { .. } => "api",
        auth::Info::Wellknown { .. } => "wellknown",
    }
}

fn name_of<'a>(database: &'a Value, id: &'a str) -> &'a str {
    database.get(id).and_then(|p| p.get("name")).and_then(Value::as_str).filter(|name| !name.is_empty()).unwrap_or(id)
}

pub fn list(paths: &Paths) -> Result<i32> {
    ui::empty();
    let file = paths.auth_file();
    ui::intro(&format!("Credentials {DIM}{}", ui::shorten(&file, &paths.home)));
    let entries = auth::read_all(&file);
    let own = if std::env::var_os("OPENCODE_AUTH_CONTENT").is_some() { entries.clone() } else { auth::read_file(&file) };
    let database = provider::load_catalog(paths);
    for (id, info) in &entries {
        // Rust build: logins read from the opencode 2.x database are marked; logout cannot remove them.
        let source = if own.iter().any(|(key, _)| key == id) { "" } else { " (opencode.db, read-only)" };
        ui::info(&format!("{} {DIM}{}{source}", name_of(&database, id), kind(info)));
    }
    ui::outro(&format!("{} credentials", entries.len()));

    let mut active = Vec::new();
    for (id, entry) in database.as_object().into_iter().flatten() {
        for var in entry.get("env").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            if std::env::var(var).is_ok_and(|value| !value.is_empty()) {
                active.push((name_of(&database, id), var));
            }
        }
    }
    if !active.is_empty() {
        ui::empty();
        ui::intro("Environment");
        for (provider, var) in &active {
            ui::info(&format!("{provider} {DIM}{var}"));
        }
        ui::outro(&format!("{} environment variable{}", active.len(), if active.len() == 1 { "" } else { "s" }));
    }
    Ok(0)
}

fn save(paths: &Paths, key: &str, info: auth::Info) -> Result<()> {
    Ok(auth::set(&paths.auth_file(), key, info)?)
}

async fn wellknown(paths: &Paths, url: &str) -> Result<i32> {
    let url = url.trim_end_matches('/');
    let prefix = format!("Failed to load auth provider metadata from {url}: ");
    let response = reqwest::get(format!("{url}/.well-known/opencode")).await.map_err(|e| fail(format!("{prefix}{e}")))?;
    let metadata: Value = serde_json::from_str(&response.text().await.map_err(|e| fail(format!("{prefix}{e}")))?).map_err(|e| fail(format!("{prefix}{e}")))?;
    let command: Vec<String> = metadata["auth"]["command"].as_array().into_iter().flatten().filter_map(|part| part.as_str().map(str::to_string)).collect();
    let Some((program, rest)) = command.split_first() else {
        return Err(fail(format!("{prefix}missing auth.command")));
    };
    ui::info(&format!("Running `{}`", command.join(" ")));
    let output = tokio::process::Command::new(program)
        .args(rest)
        .stdin(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .output()
        .await
        .map_err(|e| fail(format!("Failed to run auth provider command: {e}")))?;
    if !output.status.success() {
        ui::failure("Failed");
        ui::outro("Done");
        return Ok(0);
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let key = metadata["auth"]["env"].as_str().unwrap_or_default().to_string();
    save(paths, url, auth::Info::Wellknown { key, token })?;
    ui::success(&format!("Logged into {url}"));
    ui::outro("Done");
    Ok(0)
}

enum Flow {
    // The index and description of an external plugin's method.
    Plugin(u64, Value),
    // Flows the original ships as built-in plugins.
    Copilot,
    ChatgptBrowser,
    ChatgptHeadless,
    ApiKey,
}

struct Method {
    label: String,
    flow: Flow,
}

// `plugin/openai/codex.ts`: the PKCE flow with a local callback server.
async fn chatgpt_browser(paths: &Paths) -> Result<()> {
    let failed = |e: opencode_llm::Error| fail(format!("Failed to authorize: {e}"));
    let login = codex::login_start(codex::ISSUER).await.map_err(failed)?;
    ui::info(&format!("Go to: {}", login.url));
    ui::info(login.instructions);
    ui::step("Waiting for authorization...");
    let info = login.wait(&reqwest::Client::new()).await.map_err(failed)?;
    save(paths, codex::PROVIDER_ID, info)?;
    ui::done("Login successful", true);
    Ok(())
}

// `plugin/openai/codex.ts`: the device-code flow.
async fn chatgpt_headless(paths: &Paths) -> Result<()> {
    let failed = |e: opencode_llm::Error| fail(format!("Failed to authorize: {e}"));
    let http = reqwest::Client::new();
    let login = codex::device_start(&http, codex::ISSUER).await.map_err(failed)?;
    ui::info(&format!("Go to: {}", login.url));
    ui::info(&login.instructions());
    ui::step("Waiting for authorization...");
    loop {
        match codex::device_poll(&http, &login).await.map_err(failed)? {
            copilot::Poll::Success(info) => {
                save(paths, codex::PROVIDER_ID, info)?;
                ui::done("Login successful", true);
                return Ok(());
            }
            copilot::Poll::Pending { wait_ms } => tokio::time::sleep(Duration::from_millis(wait_ms)).await,
            copilot::Poll::Failed => {
                ui::done("Failed to authorize", false);
                return Ok(());
            }
        }
    }
}

// The device flow the original ships as a built-in plugin (`plugin/github-copilot/copilot.ts`).
async fn copilot_login(paths: &Paths) -> Result<()> {
    let deployment = prompt::select("Select GitHub deployment type", vec![Choice::hint("GitHub.com", "Public"), Choice::hint("GitHub Enterprise", "Data residency or self-hosted")], 0, 2)?;
    let enterprise = match deployment {
        1 => Some(prompt::text("Enter your GitHub Enterprise URL or domain", Some("company.ghe.com or https://company.ghe.com"), None)?),
        _ => None,
    };
    let http = reqwest::Client::new();
    let device = copilot::device_start(&http, enterprise.as_deref()).await.map_err(|e| fail(format!("Failed to authorize: {e}")))?;
    ui::info(&format!("Go to: {}", device.verification_uri));
    ui::info(&device.instructions());
    ui::step("Waiting for authorization...");
    loop {
        match copilot::device_poll(&http, &device).await.map_err(|e| fail(format!("Failed to authorize: {e}")))? {
            copilot::Poll::Success(info) => {
                save(paths, copilot::PROVIDER_ID, info)?;
                ui::done("Login successful", true);
                return Ok(());
            }
            copilot::Poll::Pending { wait_ms } => tokio::time::sleep(Duration::from_millis(wait_ms)).await,
            copilot::Poll::Failed => {
                ui::done("Failed to authorize", false);
                return Ok(());
            }
        }
    }
}

// A plugin's `auth` hook, driven the way `handlePluginAuth` drives it. False when the method is not one this build can run.
async fn plugin_login(paths: &Paths, engine: &Engine, provider_id: &str, index: u64, method: &Value) -> Result<bool> {
    let directory = std::env::current_dir()?;
    let mut inputs = serde_json::Map::new();
    for item in method.get("prompts").and_then(Value::as_array).into_iter().flatten() {
        let key = item["key"].as_str().unwrap_or_default().to_string();
        if let Some(when) = item.get("when").filter(|when| when.is_object()) {
            let Some(value) = inputs.get(when["key"].as_str().unwrap_or_default()) else { continue };
            let equal = value == &when["value"];
            if equal != (when["op"] == "eq") {
                continue;
            }
        }
        let message = item["message"].as_str().unwrap_or_default();
        if item["type"] == "select" {
            let options: Vec<&Value> = item["options"].as_array().into_iter().flatten().collect();
            let choices = options
                .iter()
                .map(|option| Choice { label: option["label"].as_str().unwrap_or_default().to_string(), hint: option["hint"].as_str().map(str::to_string) })
                .collect::<Vec<_>>();
            let page = choices.len();
            let picked = prompt::select(message, choices, 0, page)?;
            inputs.insert(key, options[picked]["value"].clone());
            continue;
        }
        inputs.insert(key, prompt::text(message, item["placeholder"].as_str(), None)?.into());
    }
    match method["type"].as_str() {
        Some("oauth") => {
            let failed = |e: opencode_session::ProviderAuthError| fail(format!("Failed to authorize: {e}"));
            let Some(shown) = engine.provider_auth_authorize(&directory, provider_id, index, Some(Value::Object(inputs))).await.map_err(failed)? else {
                return Ok(false);
            };
            if let Some(url) = shown["url"].as_str().filter(|url| !url.is_empty()) {
                ui::info(&format!("Go to: {url}"));
            }
            let code = if shown["method"] == "code" {
                Some(prompt::text("Paste the authorization code here: ", None, Some(|x| x.is_empty().then_some("Required")))?)
            } else {
                if let Some(instructions) = shown["instructions"].as_str().filter(|text| !text.is_empty()) {
                    ui::info(instructions);
                }
                ui::step("Waiting for authorization...");
                None
            };
            match engine.provider_auth_callback(&directory, provider_id, code.as_deref()).await {
                Ok(()) => ui::success("Login successful"),
                Err(opencode_session::ProviderAuthError::OauthCallbackFailed) => ui::failure("Failed to authorize"),
                Err(error) => return Err(failed(error)),
            }
            ui::outro("Done");
            Ok(true)
        }
        Some("api") => {
            let key = prompt::password("Enter your API key")?;
            let metadata = (!inputs.is_empty()).then_some(inputs);
            save(paths, provider_id, auth::Info::Api { key, metadata })?;
            ui::outro("Done");
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn pick_method(provider_id: &str, methods: &[Method], wanted: Option<&str>) -> Result<usize> {
    match wanted {
        Some(name) => methods.iter().position(|m| m.label.to_lowercase() == name.to_lowercase()).ok_or_else(|| {
            let available: Vec<&str> = methods.iter().map(|m| m.label.as_str()).collect();
            fail(format!("Unknown method \"{name}\" for {provider_id}. Available: {}", available.join(", ")))
        }),
        None if methods.len() <= 1 => Ok(0),
        None => prompt::select("Login method", methods.iter().map(|m| Choice::new(&m.label)).collect(), 0, methods.len()),
    }
}

fn priority(id: &str) -> u32 {
    match id {
        "opencode" => 0,
        "openai" => 1,
        "github-copilot" => 2,
        "google" => 3,
        "anthropic" => 4,
        "openrouter" => 5,
        "vercel" => 6,
        _ => 99,
    }
}

fn valid_id(input: &str) -> Option<&'static str> {
    let ok = !input.is_empty() && input.chars().all(|c| c.is_ascii_digit() || c.is_ascii_lowercase() || c == '-');
    (!ok).then_some("a-z, 0-9 and hyphens only")
}

pub async fn login(paths: &Paths, url: Option<String>, wanted: Option<String>, method: Option<String>) -> Result<i32> {
    ui::empty();
    ui::intro("Add credential");
    if let Some(url) = url {
        return wellknown(paths, &url).await;
    }
    let _ = catalog::refresh(&reqwest::Client::new(), &catalog::Source::from_env(paths), true).await;
    let cwd = std::env::current_dir()?;
    let opts = config::LoadOptions { worktree: config::find_worktree(&cwd), cwd: cwd.clone() };
    let info = config::load_with(paths, &opts, &opencode_core::flag::process_env)?.info()?;
    let database = provider::load_catalog(paths);
    let enabled = info.enabled_providers.as_ref();
    let mut options: Vec<(String, String, Option<&str>)> = database
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(id, _)| enabled.is_none_or(|list| list.contains(id)) && !info.disabled_providers.contains(id))
        .map(|(id, _)| {
            let hint = match id.as_str() {
                "opencode" => Some("recommended"),
                "openai" => Some("ChatGPT Plus/Pro or API key"),
                _ => None,
            };
            (id.clone(), name_of(&database, id).to_string(), hint)
        })
        .collect();
    options.sort_by(|a, b| priority(&a.0).cmp(&priority(&b.0)).then_with(|| a.1.cmp(&b.1)));

    // Auth hooks of external plugins; none when plugins are off or Bun is missing.
    let engine = Engine::new(paths.clone())?;
    let hooks = engine.provider_auth_methods(&cwd).await.unwrap_or_else(|_| Value::Object(Default::default()));
    for id in hooks.as_object().into_iter().flatten().map(|(id, _)| id) {
        let listed = options.iter().any(|option| &option.0 == id);
        if !listed && !info.disabled_providers.contains(id) && enabled.is_none_or(|list| list.contains(id)) {
            let name = info.provider.get(id).and_then(|p| p.name.as_deref()).unwrap_or(id).to_string();
            options.push((id.clone(), name, Some("plugin")));
        }
    }

    let mut provider_id = match &wanted {
        Some(input) => {
            let found = options.iter().find(|o| &o.0 == input).or_else(|| options.iter().find(|o| o.1.to_lowercase() == input.to_lowercase()));
            found.ok_or_else(|| fail(format!("Unknown provider \"{input}\"")))?.0.clone()
        }
        None => {
            let mut choices: Vec<Choice> = options.iter().map(|(_, name, hint)| Choice { label: name.clone(), hint: hint.map(str::to_string) }).collect();
            choices.push(Choice::new("Other"));
            let picked = prompt::select("Select provider", choices, 0, 8)?;
            options.get(picked).map_or_else(|| "other".to_string(), |option| option.0.clone())
        }
    };

    let methods_of = |id: &str| -> Vec<Method> {
        let mut methods: Vec<Method> = hooks
            .get(id)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(index, method)| Method { label: method["label"].as_str().unwrap_or_default().to_string(), flow: Flow::Plugin(index as u64, method.clone()) })
            .collect();
        if methods.is_empty() && id == copilot::PROVIDER_ID {
            methods.push(Method { label: "Login with GitHub Copilot".into(), flow: Flow::Copilot });
        }
        if methods.is_empty() && id == codex::PROVIDER_ID {
            methods.push(Method { label: "ChatGPT Pro/Plus (browser)".into(), flow: Flow::ChatgptBrowser });
            methods.push(Method { label: "ChatGPT Pro/Plus (headless)".into(), flow: Flow::ChatgptHeadless });
            methods.push(Method { label: "Manually enter API Key".into(), flow: Flow::ApiKey });
        }
        methods
    };
    let run_methods = async |id: &str| -> Result<bool> {
        let methods = methods_of(id);
        if methods.is_empty() {
            return Ok(false);
        }
        let chosen = &methods[pick_method(id, &methods, method.as_deref())?];
        match &chosen.flow {
            Flow::Plugin(index, spec) => return plugin_login(paths, &engine, id, *index, spec).await,
            Flow::Copilot => copilot_login(paths).await?,
            Flow::ChatgptBrowser => chatgpt_browser(paths).await?,
            Flow::ChatgptHeadless => chatgpt_headless(paths).await?,
            Flow::ApiKey => {
                let key = prompt::password("Enter your API key")?;
                save(paths, id, auth::Info::Api { key, metadata: None })?;
            }
        }
        ui::outro("Done");
        Ok(true)
    };
    if run_methods(&provider_id).await? {
        engine.shutdown().await;
        return Ok(0);
    }

    if provider_id == "other" {
        provider_id = prompt::text("Enter provider id", None, Some(valid_id))?;
        if let Some(reason) = valid_id(&provider_id) {
            return Err(fail(reason));
        }
        provider_id = provider_id.trim_start_matches("@ai-sdk/").to_string();
        if run_methods(&provider_id).await? {
            engine.shutdown().await;
            return Ok(0);
        }
        ui::warn(&format!("This only stores a credential for {provider_id} - you will need configure it in opencode.json, check the docs for examples."));
    }
    engine.shutdown().await;

    match provider_id.as_str() {
        "amazon-bedrock" => ui::info(
            "Amazon Bedrock authentication priority:\n  1. Bearer token (AWS_BEARER_TOKEN_BEDROCK or /connect)\n  2. AWS credential chain (profile, access keys, IAM roles, EKS IRSA)\n\nConfigure via opencode.json options (profile, region, endpoint) or\nAWS environment variables (AWS_PROFILE, AWS_REGION, AWS_ACCESS_KEY_ID, AWS_WEB_IDENTITY_TOKEN_FILE).",
        ),
        "opencode" => ui::info("Create an api key at https://opencode.ai/auth"),
        "vercel" => ui::info("You can create an api key at https://vercel.link/ai-gateway-token"),
        "cloudflare" | "cloudflare-ai-gateway" => ui::info(
            "Cloudflare AI Gateway can be configured with CLOUDFLARE_GATEWAY_ID, CLOUDFLARE_ACCOUNT_ID, and CLOUDFLARE_API_TOKEN environment variables. Read more: https://opencode.ai/docs/providers/#cloudflare-ai-gateway",
        ),
        _ => {}
    }
    let key = prompt::password("Enter your API key")?;
    save(paths, &provider_id, auth::Info::Api { key, metadata: None })?;
    ui::outro("Done");
    Ok(0)
}

pub fn logout(paths: &Paths, wanted: Option<String>) -> Result<i32> {
    ui::empty();
    let file = paths.auth_file();
    let credentials = auth::read_file(&file);
    ui::intro("Remove credential");
    if credentials.is_empty() {
        ui::failure("No credentials found");
        return Ok(0);
    }
    let database = provider::load_catalog(paths);
    let chosen = match &wanted {
        Some(input) => credentials.iter().map(|(key, _)| key).find(|key| *key == input || name_of(&database, key).to_lowercase() == input.to_lowercase() && database.get(key.as_str()).is_some()),
        None => {
            let choices = credentials.iter().map(|(key, info)| Choice::new(format!("{}{DIM} ({}){}", name_of(&database, key), kind(info), ui::NORMAL))).collect();
            Some(&credentials[prompt::select("Select provider", choices, 0, 8)?].0)
        }
    };
    let Some(key) = chosen else {
        return Err(fail(format!("Unknown configured provider \"{}\"", wanted.unwrap_or_default())));
    };
    auth::remove(&file, key)?;
    ui::outro("Logout successful");
    Ok(0)
}
