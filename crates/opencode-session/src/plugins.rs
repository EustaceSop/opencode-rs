// The Bun plugin sidecar of an instance directory (`opencode/src/plugin/index.ts`): discovery, lifetime, hook plumbing.
use crate::{Ctx, Engine, Error, Host, VERSION, plain_directory};
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use opencode_core::config::{self, LoadOptions, Loaded};
use opencode_core::{auth, flag};
use opencode_llm::hooks::{AuthLoader, HttpRequest, HttpResponse, Loaded as LoaderResult};
use opencode_llm::provider::{Model, Provider};
use opencode_plugin::{
    Event as PluginEvent, FetchRequest, HookName, PluginError, PluginHost, PluginOptions, PluginSpec, basic_auth_header, discover,
    is_deprecated_plugin, npm_target,
};
use opencode_storage::identity;
use opencode_tools::Tool;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// Where plugins reach the HTTP API (`PluginInput.client` / `serverUrl`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerInfo {
    pub url: String,
    // Extra client headers, e.g. `Authorization`.
    pub headers: Vec<(String, String)>,
}

impl ServerInfo {
    // `ServerAuth.headers()` for a password-protected server.
    pub fn with_password(url: String, username: Option<&str>, password: &str) -> Self {
        Self { url, headers: vec![("Authorization".to_string(), basic_auth_header(username, password))] }
    }
}

// Starts a private API server when plugins load and none is running (the original always has one in-process).
#[async_trait::async_trait]
pub trait ServerLauncher: Send + Sync {
    async fn launch(&self) -> Result<ServerInfo, String>;
    async fn stop(&self) {}
}

// Overrides for the sidecar; everything defaults to lookup (`bun` on PATH, packages next to the config dirs).
#[derive(Debug, Clone, Default)]
pub struct PluginSettings {
    pub bun_path: Option<PathBuf>,
    // Module exporting `createOpencodeClient`, instead of resolving `@opencode-ai/sdk`.
    pub sdk_path: Option<String>,
    // Zod v4 module for custom tool files that cannot resolve `zod` themselves.
    pub zod_path: Option<String>,
}

// What is loaded for one directory.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PluginStatus {
    // (id or spec, hook names) per loaded plugin, in load order.
    pub plugins: Vec<(String, Vec<String>)>,
    pub tools: Vec<String>,
    // Load problems, as published in `session.error` events.
    pub notices: Vec<String>,
}

pub(crate) struct Plugins {
    pub(crate) host: PluginHost,
    pub(crate) notices: Vec<String>,
    // (config as loaded, config after the plugins' `config` hook)
    config: Mutex<Option<(Value, Value)>>,
    // Started OAuth authorizations by provider id: the plugin's result with its callback handle.
    pending: Mutex<std::collections::HashMap<String, Value>>,
    // Provider ids with an `auth.loader`, and ids with a `provider.models` hook; listed once per host.
    auth_loaders: tokio::sync::OnceCell<Vec<String>>,
    model_lists: tokio::sync::OnceCell<Vec<String>>,
}

impl Plugins {
    pub(crate) fn has(&self, name: HookName) -> bool {
        self.host.has_hook(name.as_str())
    }

    async fn auth_loaders(&self) -> &[String] {
        let list = self.auth_loaders.get_or_init(|| async {
            let listed = self.host.auth_list().await.unwrap_or_default();
            let with_loader = listed.as_array().into_iter().flatten().filter(|item| item["loader"] == true);
            with_loader.filter_map(|item| item["provider"].as_str().map(str::to_string)).collect()
        });
        list.await
    }

    async fn model_lists(&self) -> &[String] {
        let list = self.model_lists.get_or_init(|| async {
            let listed = self.host.provider_list().await.unwrap_or_default();
            let with_models = listed.as_array().into_iter().flatten().filter(|item| item["models"] == true);
            with_models.filter_map(|item| item["id"].as_str().map(str::to_string)).collect()
        });
        list.await
    }

    pub(crate) fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.host.tools()
    }

    pub(crate) fn config_for(&self, loaded: &Value) -> Option<Value> {
        let cached = self.config.lock().unwrap_or_else(|e| e.into_inner());
        cached.as_ref().filter(|(before, _)| before == loaded).map(|(_, after)| after.clone())
    }
}

pub(crate) type Slot = Arc<tokio::sync::OnceCell<Option<Arc<Plugins>>>>;

// Plugin `auth` loaders, their `fetch` and `provider.models` hooks for the LLM layer; inert without a running host.
#[derive(Clone, Default)]
pub(crate) struct PluginAuth {
    pub(crate) plugins: Option<Arc<Plugins>>,
}

impl PluginAuth {
    pub(crate) fn new(plugins: Option<&Arc<Plugins>>) -> Self {
        Self { plugins: plugins.cloned() }
    }

    fn active(&self) -> Option<&Arc<Plugins>> {
        self.plugins.as_ref().filter(|plugins| plugins.host.is_active())
    }
}

impl AuthLoader for PluginAuth {
    fn options<'a>(&'a self, provider_id: &'a str, stored: &'a auth::Info, info: &'a Value) -> BoxFuture<'a, Option<LoaderResult>> {
        Box::pin(async move {
            let plugins = self.active()?;
            if !plugins.auth_loaders().await.iter().any(|id| id == provider_id) {
                return None;
            }
            let stored = serde_json::to_value(stored).ok()?;
            let loaded = plugins.host.auth_loader(provider_id, stored, info.clone()).await.ok()?;
            Some(LoaderResult { options: Value::Object(loaded.options), fetch: loaded.functions.get("fetch").cloned() })
        })
    }

    fn fetch<'a>(&'a self, handle: &'a str, request: HttpRequest) -> BoxFuture<'a, Result<HttpResponse, opencode_llm::Error>> {
        Box::pin(async move {
            let Some(plugins) = &self.plugins else {
                return Err(opencode_llm::Error::Config("the plugin host that owns this provider's fetch is not running".into()));
            };
            let body = (!request.body.is_empty()).then_some(request.body);
            let request = FetchRequest { url: request.url, method: request.method, headers: request.headers, body };
            let response = plugins.host.fetch(handle, request).await.map_err(|e| match e {
                PluginError::Crashed(_) | PluginError::Closed => opencode_llm::Error::Config(format!("plugin fetch failed: {e}")),
                other => opencode_llm::Error::Stream(format!("plugin fetch failed: {other}")),
            })?;
            let body = futures_util::stream::unfold(response.body, |mut chunks| async move {
                let chunk = chunks.recv().await?;
                Some((chunk.map_err(|e| opencode_llm::Error::Stream(format!("plugin fetch failed: {e}"))), chunks))
            });
            Ok(HttpResponse { status: response.status, headers: response.headers, body: body.boxed() })
        })
    }

    fn models<'a>(&'a self, provider_id: &'a str, info: &'a Value, stored: Option<&'a auth::Info>) -> BoxFuture<'a, Option<Value>> {
        Box::pin(async move {
            let plugins = self.active()?;
            if !plugins.model_lists().await.iter().any(|id| id == provider_id) {
                return None;
            }
            let stored = stored.and_then(|info| serde_json::to_value(info).ok());
            let models = plugins.host.provider_models(provider_id, info.clone(), stored).await.ok()?;
            models.is_object().then_some(models)
        })
    }
}

fn unknown(message: &str) -> Value {
    json!({"error": {"name": "UnknownError", "data": {"message": message}}})
}

// The v1 `Model` object hooks receive, from what the engine knows about the model.
pub(crate) fn model_json(model: &Model) -> Value {
    json!({
        "id": model.id,
        "providerID": model.provider_id,
        "api": {"id": model.api_id, "url": model.api_url, "npm": model.npm},
        "name": model.name,
        "family": model.family,
        "capabilities": {
            "temperature": model.temperature,
            "reasoning": model.reasoning,
            "attachment": model.attachment,
            "toolcall": model.toolcall,
            "input": {"text": model.input.text, "audio": model.input.audio, "image": model.input.image, "video": model.input.video, "pdf": model.input.pdf},
        },
        "limit": {"context": model.limit_context, "input": model.limit_input, "output": model.limit_output},
        "status": model.status,
        "options": model.options,
        "headers": model.headers,
        "release_date": model.release_date,
        "variants": model.variants,
    })
}

// `ProviderAuth` errors (`provider/auth.ts`), as the HTTP API names them.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ProviderAuthError {
    #[error("No pending OAuth authorization for provider {provider_id}")]
    OauthMissing { provider_id: String },
    #[error("OAuth code missing for provider {provider_id}")]
    OauthCodeMissing { provider_id: String },
    #[error("OAuth callback failed")]
    OauthCallbackFailed,
    #[error("{message}")]
    ValidationFailed { field: String, message: String },
    #[error("{0}")]
    Other(String),
}

impl Engine {
    async fn auth_plugins(&self, directory: &Path) -> Result<Option<Arc<Plugins>>, ProviderAuthError> {
        let key = plain_directory(directory.to_path_buf());
        self.ensure_plugins(&key, None).await.map_err(|e| ProviderAuthError::Other(e.to_string()))
    }

    // `ProviderAuth.methods`: the auth methods plugins offer, by provider id.
    pub async fn provider_auth_methods(&self, directory: &Path) -> Result<Value, ProviderAuthError> {
        let mut out = serde_json::Map::new();
        let Some(plugins) = self.auth_plugins(directory).await? else { return Ok(Value::Object(out)) };
        let listed = plugins.host.auth_list().await.map_err(|e| ProviderAuthError::Other(e.to_string()))?;
        for item in listed.as_array().into_iter().flatten() {
            let Some(provider) = item.get("provider").and_then(Value::as_str) else { continue };
            let methods: Vec<Value> = item
                .get("methods")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|method| {
                    let mut entry = json!({"type": method["type"], "label": method["label"]});
                    if let Some(prompts) = method.get("prompts") {
                        entry["prompts"] = prompts.clone();
                    }
                    entry
                })
                .collect();
            out.insert(provider.to_string(), methods.into());
        }
        Ok(Value::Object(out))
    }

    // `ProviderAuth.authorize`: starts a plugin's OAuth method; `None` when the method has nothing to open.
    pub async fn provider_auth_authorize(&self, directory: &Path, provider_id: &str, method: u64, inputs: Option<Value>) -> Result<Option<Value>, ProviderAuthError> {
        let missing = || ProviderAuthError::Other(format!("No auth method {method} for provider {provider_id}"));
        let plugins = self.auth_plugins(directory).await?.ok_or_else(missing)?;
        let listed = plugins.host.auth_list().await.map_err(|e| ProviderAuthError::Other(e.to_string()))?;
        let hook = listed.as_array().into_iter().flatten().rfind(|item| item["provider"] == provider_id).ok_or_else(missing)?;
        let kind = hook["methods"].get(method as usize).ok_or_else(missing)?["type"].as_str().unwrap_or_default();
        if kind != "oauth" {
            return Ok(None);
        }
        let result = match plugins.host.auth_authorize(provider_id, method as usize, inputs).await {
            Ok(result) => result,
            Err(PluginError::Remote { message, data, .. }) if data.get("kind").and_then(Value::as_str) == Some("validation_failed") => {
                let field = data.get("field").and_then(Value::as_str).unwrap_or_default().to_string();
                return Err(ProviderAuthError::ValidationFailed { field, message });
            }
            Err(error) => return Err(ProviderAuthError::Other(error.to_string())),
        };
        if !result.is_object() {
            return Ok(None);
        }
        let shown = json!({"url": result["url"], "method": result["method"], "instructions": result["instructions"]});
        plugins.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(provider_id.to_string(), result);
        Ok(Some(shown))
    }

    // `ProviderAuth.callback`: finishes the pending authorization and stores the credential in auth.json.
    pub async fn provider_auth_callback(&self, directory: &Path, provider_id: &str, code: Option<&str>) -> Result<(), ProviderAuthError> {
        let missing = || ProviderAuthError::OauthMissing { provider_id: provider_id.to_string() };
        let plugins = self.auth_plugins(directory).await?.ok_or_else(missing)?;
        let pending = plugins.pending.lock().unwrap_or_else(|e| e.into_inner()).get(provider_id).cloned().ok_or_else(missing)?;
        let by_code = pending["method"] == "code";
        if by_code && code.is_none() {
            return Err(ProviderAuthError::OauthCodeMissing { provider_id: provider_id.to_string() });
        }
        let handle = pending["handle"].as_str().unwrap_or_default();
        let result = plugins.host.auth_callback(handle, code.filter(|_| by_code)).await.map_err(|e| ProviderAuthError::Other(e.to_string()))?;
        let Some(fields) = result.as_object().filter(|fields| fields.get("type").and_then(Value::as_str) == Some("success")) else {
            return Err(ProviderAuthError::OauthCallbackFailed);
        };
        let save = |info: Value| -> Result<(), ProviderAuthError> {
            let info: auth::Info = serde_json::from_value(info).map_err(|e| ProviderAuthError::Other(e.to_string()))?;
            auth::set(&self.paths.auth_file(), provider_id, info).map_err(|e| ProviderAuthError::Other(e.to_string()))
        };
        if let Some(key) = fields.get("key") {
            let mut info = json!({"type": "api", "key": key});
            if let Some(metadata) = fields.get("metadata").filter(|m| !m.is_null()) {
                info["metadata"] = metadata.clone();
            }
            save(info)?;
        }
        if fields.contains_key("refresh") {
            let mut info = fields.clone();
            info.shift_remove("provider");
            info.insert("type".into(), "oauth".into());
            save(Value::Object(info))?;
        }
        Ok(())
    }
}

// The `env` object of a `shell.env` result.
pub(crate) fn env_pairs(output: &Value) -> Vec<(String, String)> {
    let env = output.get("env").and_then(Value::as_object);
    env.into_iter().flatten().filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_string()))).collect()
}

// `ProviderContext`: `{source, info, options}`.
pub(crate) fn provider_json(provider: &Provider) -> Value {
    let models: serde_json::Map<String, Value> = provider.models.iter().map(|m| (m.id.clone(), model_json(m))).collect();
    let source = if provider.oauth { "custom" } else { "config" };
    let info = json!({"id": provider.id, "name": provider.name, "source": source, "env": provider.env, "options": provider.options, "models": models});
    json!({"source": source, "info": info, "options": provider.options})
}

// `experimental.provider.small_model`: a plugin's pick for cheap side requests, as (provider id, model id).
pub(crate) async fn small_model(ctx: &Ctx, provider_id: &str) -> Option<(String, String)> {
    let plugins = ctx.plugins.as_ref().filter(|p| p.has(HookName::ExperimentalProviderSmallModel))?;
    let provider = ctx.providers.iter().find(|p| p.id == provider_id)?;
    let input = json!({"provider": provider_json(provider)["info"]});
    let output = plugins.host.trigger(HookName::ExperimentalProviderSmallModel, input, json!({})).await.ok()?;
    let model = output.get("model")?;
    Some((model.get("providerID")?.as_str()?.to_string(), model.get("id")?.as_str()?.to_string()))
}

impl Engine {
    // The HTTP API plugins should talk to; `opencode_server::serve` registers itself here.
    pub fn set_server(&self, server: Option<ServerInfo>) {
        *self.server.lock().unwrap_or_else(|e| e.into_inner()) = server;
    }

    pub fn server(&self) -> Option<ServerInfo> {
        self.server.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    // Used when plugins load while no server is registered.
    pub fn set_server_launcher(&self, launcher: Arc<dyn ServerLauncher>) {
        *self.launcher.lock().unwrap_or_else(|e| e.into_inner()) = Some(launcher);
    }

    pub fn set_plugin_settings(&self, settings: PluginSettings) {
        *self.plugin_settings.lock().unwrap_or_else(|e| e.into_inner()) = settings;
    }

    fn slot(&self, directory: &Path) -> Slot {
        self.plugins.lock().unwrap_or_else(|e| e.into_inner()).entry(directory.to_path_buf()).or_default().clone()
    }

    // The directory's plugins if their host is running.
    pub(crate) fn loaded_plugins(&self, directory: &Path) -> Option<Arc<Plugins>> {
        let slot = self.plugins.lock().unwrap_or_else(|e| e.into_inner()).get(directory).cloned();
        slot.and_then(|slot| slot.get().cloned().flatten())
    }

    async fn server_info(&self) -> ServerInfo {
        if let Some(server) = self.server() {
            return server;
        }
        let launcher = self.launcher.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(launcher) = launcher
            && let Ok(server) = launcher.launch().await
        {
            return server;
        }
        // `Server.url ?? "http://localhost:4096"`
        self.server().unwrap_or_else(|| ServerInfo { url: "http://localhost:4096".to_string(), headers: Vec::new() })
    }

    fn notify(&self, host: Option<&Arc<dyn Host>>, directory: &Path, message: &str) {
        self.events.publish(host, &directory.to_string_lossy(), "session.error", unknown(message));
    }

    async fn start_plugins(&self, directory: &Path, worktree: Option<&Path>, loaded: &Loaded, pure: bool, host: Option<&Arc<dyn Host>>) -> Option<Arc<Plugins>> {
        let mut found = discover(&loaded.config, &loaded.directories);
        // `OPENCODE_PURE`: no external plugins; custom tool files still load.
        if pure {
            found.specs.clear();
        }
        let mut notices: Vec<String> = Vec::new();
        let mut specs: Vec<PluginSpec> = Vec::new();
        for item in found.specs {
            if is_deprecated_plugin(&item.spec) {
                continue;
            }
            // npm plugins are only ever read from the package cache; installing is an explicit step.
            if let Some(target) = npm_target(&item.spec, &self.paths.cache)
                && !target.is_installed()
            {
                notices.push(format!(
                    "Failed to load plugin {}: package {} is not installed and plugins are never installed automatically. Run the plugin install step (opencode_plugin::install, i.e. `bun add {}` in {}) and restart.",
                    item.spec,
                    target.name,
                    target.package,
                    target.dir.display()
                ));
                continue;
            }
            specs.push(item);
        }
        let mut plugins = None;
        if !specs.is_empty() || !found.tool_files.is_empty() {
            let settings = self.plugin_settings.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let server = self.server_info().await;
            let project = identity::resolve(directory).id;
            let root = worktree.map_or_else(|| PathBuf::from("/"), Path::to_path_buf);
            let auth_file = self.paths.auth_file();
            let options = PluginOptions {
                specs,
                tool_files: found.tool_files,
                directory: directory.to_path_buf(),
                worktree: root.clone(),
                project: self.store.get_project(&project).ok().flatten().and_then(|p| serde_json::to_value(p).ok()),
                project_id: project,
                server_url: server.url,
                server_headers: server.headers.into_iter().collect::<BTreeMap<_, _>>(),
                cache_dir: self.paths.cache.clone(),
                bun_path: settings.bun_path,
                resolve_dirs: loaded.directories.clone(),
                sdk_path: settings.sdk_path,
                zod_path: settings.zod_path,
                opencode_version: Some(VERSION.to_string()),
                auth: Some(Arc::new(move |provider: &str| {
                    let entry = auth::read_all(&auth_file).into_iter().find(|(id, _)| id == provider)?;
                    serde_json::to_value(entry.1).ok()
                })),
                ..Default::default()
            };
            match PluginHost::start(options).await {
                Ok(started) => {
                    notices.extend(started.info().errors.iter().map(|error| error.session_message()));
                    plugins = Some(Arc::new(Plugins {
                        host: started,
                        notices: notices.clone(),
                        config: Mutex::new(None),
                        pending: Mutex::default(),
                        auth_loaders: Default::default(),
                        model_lists: Default::default(),
                    }));
                }
                Err(error) => notices.push(format!("Failed to start the plugin host: {error}")),
            }
        }
        for notice in &notices {
            self.notify(host, directory, notice);
        }
        if plugins.is_none() && !notices.is_empty() {
            self.plugin_failures.lock().unwrap_or_else(|e| e.into_inner()).insert(directory.to_path_buf(), notices);
        }
        plugins
    }

    // Loads the directory's plugins and custom tools on first use and runs the plugins' `config` hook.
    pub(crate) async fn ensure_plugins(&self, directory: &Path, host: Option<&Arc<dyn Host>>) -> Result<Option<Arc<Plugins>>, Error> {
        let pure = flag::Flags::from_env().map_err(|e| Error::Config(e.to_string()))?.pure;
        let slot = self.slot(directory);
        match slot.get() {
            Some(None) => return Ok(None),
            Some(Some(plugins)) if !plugins.host.has_hook("config") => return Ok(Some(plugins.clone())),
            _ => {}
        }
        let worktree = config::find_worktree(directory);
        let opts = LoadOptions { cwd: directory.to_path_buf(), worktree: worktree.clone() };
        let loaded = self.load_config(&opts)?;
        let plugins = slot.get_or_init(|| self.start_plugins(directory, worktree.as_deref(), &loaded, pure, host)).await.clone();
        let Some(plugins) = plugins else { return Ok(None) };
        if plugins.host.has_hook("config") && plugins.config_for(&loaded.config).is_none() {
            // Hook failures are logged and ignored by the original; the config then stays as loaded.
            match plugins.host.config(loaded.config.clone()).await {
                Ok(after) if after.is_object() => *plugins.config.lock().unwrap_or_else(|e| e.into_inner()) = Some((loaded.config, after)),
                Ok(_) => {}
                Err(error) => self.plugin_failed(directory, &plugins, &error, host),
            }
        }
        Ok(Some(plugins))
    }

    // Everything derived from an instance directory, with its plugins loaded.
    pub(crate) async fn instance(&self, directory: &Path, host: Option<&Arc<dyn Host>>) -> Result<Arc<Ctx>, Error> {
        self.refresh_console().await;
        self.ensure_plugins(directory, host).await?;
        self.context_async(directory).await
    }

    // A dead sidecar is reported once and not restarted until the instance is disposed.
    fn plugin_failed(&self, directory: &Path, plugins: &Arc<Plugins>, error: &PluginError, host: Option<&Arc<dyn Host>>) {
        if !matches!(error, PluginError::Crashed(_) | PluginError::Closed) {
            return;
        }
        let dropped = {
            let mut all = self.plugins.lock().unwrap_or_else(|e| e.into_inner());
            let current = all.get(directory).and_then(|slot| slot.get().cloned().flatten());
            match current {
                Some(current) if Arc::ptr_eq(&current, plugins) => {
                    all.insert(directory.to_path_buf(), Arc::new(tokio::sync::OnceCell::new_with(Some(None))));
                    true
                }
                _ => false,
            }
        };
        if dropped {
            let message = format!("Plugins are disabled for this instance: {error}");
            self.plugin_failures.lock().unwrap_or_else(|e| e.into_inner()).insert(directory.to_path_buf(), vec![message.clone()]);
            self.notify(host, directory, &message);
        }
    }

    // `Plugin.trigger`: a failing hook fails the calling operation.
    pub(crate) async fn hook(&self, ctx: &Ctx, name: HookName, input: Value, output: Value) -> Result<Value, String> {
        let Some(plugins) = &ctx.plugins else { return Ok(output) };
        if !plugins.has(name) {
            return Ok(output);
        }
        match plugins.host.trigger(name, input, output).await {
            Ok(output) => Ok(output),
            Err(error) => {
                self.plugin_failed(&ctx.cwd, plugins, &error, None);
                Err(format!("plugin hook {name} failed: {error}"))
            }
        }
    }

    // `Plugin.trigger` for work outside a turn: loads the directory's plugins and hands back `output` unchanged when
    // none has the hook. A failing hook fails the call.
    pub async fn trigger(&self, directory: &Path, name: HookName, input: Value, output: Value) -> Result<Value, Error> {
        let key = plain_directory(directory.to_path_buf());
        let Some(plugins) = self.ensure_plugins(&key, None).await? else { return Ok(output) };
        if !plugins.has(name) {
            return Ok(output);
        }
        match plugins.host.trigger(name, input, output).await {
            Ok(output) => Ok(output),
            Err(error) => {
                self.plugin_failed(&key, &plugins, &error, None);
                Err(Error::Config(format!("plugin hook {name} failed: {error}")))
            }
        }
    }

    // `shell.env` with `{cwd}`, as the server runs it for a new terminal; the variables plugins add.
    pub async fn shell_env(&self, directory: &Path, cwd: &str) -> Result<Vec<(String, String)>, Error> {
        let output = self.trigger(directory, HookName::ShellEnv, json!({"cwd": cwd}), json!({"env": {}})).await?;
        Ok(env_pairs(&output))
    }

    // The plugin `event` hook: every instance bus event of the directory, fire-and-forget.
    pub fn plugin_event(&self, directory: &str, id: &str, kind: &str, properties: &Value) {
        let key = plain_directory(PathBuf::from(directory));
        let Some(plugins) = self.loaded_plugins(&key) else { return };
        if !plugins.host.has_hook("event") {
            return;
        }
        let event = PluginEvent { id: id.to_string(), kind: kind.to_string(), properties: properties.clone() };
        if let Err(error) = plugins.host.event(&event) {
            self.plugin_failed(&key, &plugins, &error, None);
        }
    }

    // What is loaded for a directory; `None` until something used the instance, or when nothing was discovered.
    pub fn plugin_status(&self, directory: &Path) -> Option<PluginStatus> {
        let key = plain_directory(directory.to_path_buf());
        if let Some(plugins) = self.loaded_plugins(&key) {
            let info = plugins.host.info();
            return Some(PluginStatus {
                plugins: info.plugins.iter().map(|p| (p.id.clone().unwrap_or_else(|| p.spec.clone()), p.hooks.clone())).collect(),
                tools: info.tools.iter().map(|tool| tool.id.clone()).collect(),
                notices: plugins.notices.clone(),
            });
        }
        let notices = self.plugin_failures.lock().unwrap_or_else(|e| e.into_inner()).get(&key).cloned()?;
        Some(PluginStatus { notices, ..Default::default() })
    }

    // Loads a directory's plugins without running a prompt.
    pub async fn load_plugins(&self, directory: &Path) -> Result<Option<PluginStatus>, Error> {
        let key = plain_directory(directory.to_path_buf());
        self.ensure_plugins(&key, None).await?;
        Ok(self.plugin_status(&key))
    }

    pub(crate) async fn dispose_plugins(&self, directory: &Path) {
        let slot = self.plugins.lock().unwrap_or_else(|e| e.into_inner()).remove(directory);
        self.plugin_failures.lock().unwrap_or_else(|e| e.into_inner()).remove(directory);
        if let Some(plugins) = slot.and_then(|slot| slot.get().cloned().flatten()) {
            plugins.host.shutdown().await;
        }
    }

    pub(crate) async fn shutdown_plugins(&self) {
        let slots: Vec<Slot> = self.plugins.lock().unwrap_or_else(|e| e.into_inner()).drain().map(|(_, slot)| slot).collect();
        self.plugin_failures.lock().unwrap_or_else(|e| e.into_inner()).clear();
        for slot in slots {
            if let Some(Some(plugins)) = slot.get() {
                plugins.host.shutdown().await;
            }
        }
        let launcher = self.launcher.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(launcher) = launcher {
            launcher.stop().await;
        }
    }
}
