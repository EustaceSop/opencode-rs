// Injection points for a plugin host: a replaceable HTTP executor, auth-loader options and replacement model lists.
use crate::provider::Model;
use crate::{Error, provider};
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use opencode_core::auth;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    // Body chunks in arrival order.
    pub body: BoxStream<'static, Result<Vec<u8>, Error>>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub async fn text(mut self) -> Result<String, Error> {
        let mut out = Vec::new();
        while let Some(chunk) = self.body.next().await {
            out.extend(chunk?);
        }
        Ok(String::from_utf8_lossy(&out).to_string())
    }

    // A fully buffered response.
    pub fn buffered(status: u16, headers: Vec<(String, String)>, body: Vec<u8>) -> Self {
        HttpResponse { status, headers, body: futures_util::stream::iter([Ok(body)]).boxed() }
    }
}

// Sends one prepared request and resolves when the response head arrives; the body streams afterwards.
pub trait Executor: Send + Sync {
    fn execute<'a>(&'a self, request: HttpRequest) -> BoxFuture<'a, Result<HttpResponse, Error>>;
}

impl Executor for reqwest::Client {
    fn execute<'a>(&'a self, request: HttpRequest) -> BoxFuture<'a, Result<HttpResponse, Error>> {
        Box::pin(async move {
            let method = reqwest::Method::from_bytes(request.method.as_bytes()).map_err(|e| Error::Config(e.to_string()))?;
            let mut builder = self.request(method, &request.url).body(request.body);
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            let response = builder.send().await?;
            let status = response.status().as_u16();
            let headers = response.headers().iter().filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string()))).collect();
            Ok(HttpResponse { status, headers, body: response.bytes_stream().map(|chunk| chunk.map(|b| b.to_vec()).map_err(Error::Http)).boxed() })
        })
    }
}

// What a plugin's auth loader returned for one provider.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Loaded {
    // Merged into the provider's options (`apiKey`, `baseURL`, `headers`, ...).
    pub options: Value,
    // Handle of the loader's `fetch`; when set, every request of the provider goes through `AuthLoader::fetch`.
    pub fetch: Option<String>,
}

// Implemented by the session crate on top of the plugin host.
pub trait AuthLoader: Send + Sync {
    // The loader options for `provider_id`, or None when no plugin handles it. `info` is `provider::info` of the provider.
    fn options<'a>(&'a self, provider_id: &'a str, stored: &'a auth::Info, info: &'a Value) -> BoxFuture<'a, Option<Loaded>>;

    // Performs `request` through the plugin `fetch` behind `handle`.
    fn fetch<'a>(&'a self, handle: &'a str, request: HttpRequest) -> BoxFuture<'a, Result<HttpResponse, Error>>;

    // A plugin `provider.models` hook: the replacement model map (`{ id: Provider.Model }`), or None to keep the list.
    fn models<'a>(&'a self, _provider_id: &'a str, _info: &'a Value, _stored: Option<&'a auth::Info>) -> BoxFuture<'a, Option<Value>> {
        Box::pin(async { None })
    }
}

// An `Executor` that routes through a plugin fetch handle.
pub struct PluginFetch<'a> {
    pub loader: &'a dyn AuthLoader,
    pub handle: &'a str,
}

impl Executor for PluginFetch<'_> {
    fn execute<'a>(&'a self, request: HttpRequest) -> BoxFuture<'a, Result<HttpResponse, Error>> {
        self.loader.fetch(self.handle, request)
    }
}

// Per-provider results of plugin hooks and built-in discovery, applied by `provider::build_with`.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    // Replacement model lists (pipeline step 4).
    pub models: Vec<(String, Vec<Model>)>,
    // Auth-loader options (step 8).
    pub options: Vec<(String, Value)>,
    // Fetch handles of auth loaders.
    pub fetch: Vec<(String, String)>,
}

impl Overrides {
    pub fn models_for(&self, provider_id: &str) -> Option<&Vec<Model>> {
        self.models.iter().rev().find(|(id, _)| id == provider_id).map(|(_, models)| models)
    }

    pub fn set_models(&mut self, provider_id: &str, models: Vec<Model>) {
        self.models.retain(|(id, _)| id != provider_id);
        self.models.push((provider_id.to_string(), models));
    }
}

// Hosts the built-in discovery talks to; overridable for tests and proxies.
#[derive(Debug, Clone, PartialEq)]
pub struct Endpoints {
    // None derives the Copilot host from the login (public or enterprise).
    pub copilot: Option<String>,
    pub digitalocean: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints { copilot: None, digitalocean: crate::digitalocean::API_BASE.into() }
    }
}

// The model lists the built-in provider plugins compute over the network: Copilot's `/models` for an OAuth login
// (on failure the catalog list stays) and DigitalOcean's Inference Routers. Run before `provider::build_with`.
pub async fn discover(http: &reqwest::Client, catalog: &Value, config: &Value, entries: &auth::Entries, auth_file: &std::path::Path, endpoints: &Endpoints) -> Overrides {
    use crate::{copilot, digitalocean};
    let mut out = Overrides::default();
    let disabled: Vec<&str> = config.get("disabled_providers").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    let stored = |id: &str| entries.iter().find(|(key, _)| key == id).map(|(_, info)| info).filter(|_| !disabled.contains(&id));
    if let (Some(entry), Some(auth::Info::Oauth { refresh, enterprise_url, .. })) = (catalog.get(copilot::PROVIDER_ID), stored(copilot::PROVIDER_ID)) {
        let base = endpoints.copilot.clone().unwrap_or_else(|| copilot::base(enterprise_url.as_deref()));
        let existing = provider::catalog_models(copilot::PROVIDER_ID, entry);
        if let Ok(models) = copilot::discover(http, &base, &[], refresh, &existing).await {
            out.set_models(copilot::PROVIDER_ID, models);
        }
    }
    if let (Some(entry), Some(info @ auth::Info::Api { .. })) = (catalog.get(digitalocean::PROVIDER_ID), stored(digitalocean::PROVIDER_ID)) {
        let existing = provider::catalog_models(digitalocean::PROVIDER_ID, entry);
        let models = digitalocean::models(http, &endpoints.digitalocean, auth_file, Some(info), existing).await;
        out.set_models(digitalocean::PROVIDER_ID, models);
    }
    out
}

// Runs the plugin hooks for every catalog provider: `models` first, then the auth loader of each provider with a stored entry.
pub async fn load(loader: &dyn AuthLoader, catalog: &Value, config: &Value, entries: &auth::Entries, base: Overrides) -> Overrides {
    let mut out = base;
    let disabled: Vec<&str> = config.get("disabled_providers").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    for (id, entry) in catalog.as_object().into_iter().flatten() {
        if disabled.contains(&id.as_str()) {
            continue;
        }
        let stored = entries.iter().find(|(key, _)| key == id).map(|(_, info)| info);
        let current = out.models_for(id).cloned().unwrap_or_else(|| provider::catalog_models(id, entry));
        let info = provider::info(id, entry, &current);
        if let Some(replacement) = loader.models(id, &info, stored).await {
            let models = replacement.as_object().into_iter().flatten().map(|(key, m)| provider::model_from_info(id, key, m)).collect();
            out.set_models(id, models);
        }
        if let Some(stored) = stored
            && let Some(loaded) = loader.options(id, stored, &info).await
        {
            if loaded.options.as_object().is_some_and(|o| !o.is_empty()) {
                out.options.push((id.clone(), loaded.options));
            }
            if let Some(handle) = loaded.fetch {
                out.fetch.retain(|(key, _)| key != id);
                out.fetch.push((id.clone(), handle));
            }
        }
    }
    out
}
