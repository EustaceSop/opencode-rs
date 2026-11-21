// The provider registry of an instance (`provider.ts` state): models.dev catalog, built-in discovery, plugin hooks.
use crate::Engine;
use crate::plugins::{PluginAuth, Plugins};
use opencode_core::auth;
use opencode_llm::catalog::{self, Source};
use opencode_llm::hooks::{self, AuthLoader, Endpoints, Overrides};
use opencode_llm::provider::{self, Provider};
use serde_json::{Value, json};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::sync::{Arc, Mutex};

pub(crate) struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// `ModelsDev.get()`: read once, kept until a background refresh replaced the file.
#[derive(Default)]
pub(crate) struct CatalogState {
    source: Option<Source>,
    value: Option<Arc<Value>>,
    // Bumped whenever `value` changes, so cached overrides know which catalog they were built from.
    generation: u64,
    refresh: Option<AbortOnDrop>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

// What one set of overrides was computed from.
pub(crate) fn key(generation: u64, plugins: Option<&Arc<Plugins>>, config: &Value, entries: &auth::Entries) -> u64 {
    let mut hasher = DefaultHasher::new();
    generation.hash(&mut hasher);
    plugins.map(|p| Arc::as_ptr(p) as usize).hash(&mut hasher);
    config.to_string().hash(&mut hasher);
    serde_json::to_string(entries).unwrap_or_default().hash(&mut hasher);
    hasher.finish()
}

// Built-in discovery, then the plugins' `provider.models` hooks and auth loaders (pipeline steps 4 and 8).
pub(crate) async fn overrides(
    http: &reqwest::Client,
    loader: &dyn AuthLoader,
    catalog: &Value,
    config: &Value,
    entries: &auth::Entries,
    auth_file: &Path,
) -> Overrides {
    let found = hooks::discover(http, catalog, config, entries, auth_file, &Endpoints::default()).await;
    let mut overrides = hooks::load(loader, catalog, config, entries, found).await;
    config_loaders(loader, catalog, config, entries, &mut overrides).await;
    overrides
}

// The `database` entry a config-only provider has when the plugin auth pass reaches it (`provider.ts`, "extend database
// from config"): name and env from config or the id and none, the config options, source "config".
fn config_info(id: &str, entry: &Value, built: Option<&Provider>) -> Value {
    let mut info = provider::info(id, &json!({"name": entry.get("name"), "env": entry.get("env")}), built.map_or(&[], |p| &p.models));
    info["source"] = "config".into();
    info["options"] = entry.get("options").filter(|o| o.is_object()).cloned().unwrap_or_else(|| json!({}));
    info
}

// `hooks::load` visits catalog providers only; the original's auth-loader pass also reaches providers defined in config.
async fn config_loaders(loader: &dyn AuthLoader, catalog: &Value, config: &Value, entries: &auth::Entries, overrides: &mut Overrides) {
    let disabled: Vec<&str> = config.get("disabled_providers").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    let configured = config.get("provider").and_then(Value::as_object).into_iter().flatten();
    let ids: Vec<(&String, &Value)> = configured.filter(|(id, _)| catalog.get(id.as_str()).is_none() && !disabled.contains(&id.as_str())).collect();
    let mut built: Option<Vec<Provider>> = None;
    for (id, entry) in ids {
        let Some((_, stored)) = entries.iter().find(|(key, _)| key == id) else { continue };
        let providers = built.get_or_insert_with(|| provider::build_with(catalog, config, entries, overrides));
        let info = config_info(id, entry, providers.iter().find(|p| &p.id == id));
        let Some(loaded) = loader.options(id, stored, &info).await else { continue };
        if loaded.options.as_object().is_some_and(|o| !o.is_empty()) {
            overrides.options.push((id.clone(), loaded.options));
        }
        if let Some(handle) = loaded.fetch {
            overrides.fetch.retain(|(key, _)| key != id);
            overrides.fetch.push((id.clone(), handle));
        }
    }
}

impl Engine {
    // Where the models.dev catalog comes from; defaults to `Source::from_env`. Replacing it drops the loaded catalog.
    pub fn set_catalog_source(&self, source: Source) {
        let mut state = lock(&self.catalog);
        *state = CatalogState { source: Some(source), value: None, generation: state.generation + 1, refresh: None };
    }

    fn catalog_source(&self) -> Source {
        lock(&self.catalog).source.clone().unwrap_or_else(|| Source::from_env(&self.paths))
    }

    // The models.dev catalog the providers are built from: the loaded one, else loaded now as the first prompt would
    // (a download only when there is no cache and fetching is enabled).
    pub async fn catalog_value(&self) -> Arc<Value> {
        self.catalog().await.0
    }

    // The loaded catalog, else the cache file as it is; never fetches.
    pub(crate) fn cached_catalog(&self) -> (Arc<Value>, u64) {
        if let Some(found) = self.loaded_catalog() {
            return found;
        }
        match catalog::read(&self.catalog_source()) {
            Some(read) => self.keep_catalog(read),
            None => (Arc::new(json!({})), u64::MAX),
        }
    }

    fn loaded_catalog(&self) -> Option<(Arc<Value>, u64)> {
        let state = lock(&self.catalog);
        state.value.clone().map(|value| (value, state.generation))
    }

    fn keep_catalog(&self, value: Value) -> (Arc<Value>, u64) {
        let mut state = lock(&self.catalog);
        if state.value.is_none() {
            state.value = Some(Arc::new(value));
            state.generation += 1;
        }
        (state.value.clone().unwrap_or_default(), state.generation)
    }

    // `catalog::load` on first use (a fetch when there is no cache), then the background refresh of the original:
    // once at start and every `REFRESH_INTERVAL`, replacing the loaded catalog after each successful download.
    pub(crate) async fn catalog(&self) -> (Arc<Value>, u64) {
        if let Some(found) = self.loaded_catalog() {
            return found;
        }
        let _first = self.catalog_load.lock().await;
        if let Some(found) = self.loaded_catalog() {
            return found;
        }
        let source = self.catalog_source();
        let loaded = catalog::load(&self.http, &source).await;
        let found = self.keep_catalog(loaded);
        let mut state = lock(&self.catalog);
        if state.refresh.is_none() && !source.disabled && state.source.as_ref().is_none_or(|current| *current == source) {
            let (http, shared) = (self.http.clone(), self.catalog.clone());
            state.refresh = Some(AbortOnDrop(tokio::spawn(async move {
                loop {
                    if matches!(catalog::refresh(&http, &source, false).await, Ok(true)) {
                        let mut state = lock(&shared);
                        state.value = None;
                        state.generation += 1;
                    }
                    tokio::time::sleep(catalog::REFRESH_INTERVAL).await;
                }
            })));
        }
        found
    }

    pub(crate) fn stop_catalog_refresh(&self) {
        lock(&self.catalog).refresh = None;
    }

    // The directory's overrides for `key`, when they were computed.
    pub(crate) fn cached_overrides(&self, directory: &Path, key: u64) -> Option<Arc<Overrides>> {
        lock(&self.registries).get(directory).filter(|(at, _)| *at == key).map(|(_, overrides)| overrides.clone())
    }

    // Computed once per instance directory and set of inputs, like the original's per-instance provider state.
    pub(crate) async fn instance_overrides(&self, directory: &Path, plugins: Option<&Arc<Plugins>>, catalog: &Value, config: &Value, entries: &auth::Entries, key: u64) -> Arc<Overrides> {
        if let Some(found) = self.cached_overrides(directory, key) {
            return found;
        }
        let loader = PluginAuth::new(plugins);
        let computed = Arc::new(overrides(&self.http, &loader, catalog, config, entries, &self.paths.auth_file()).await);
        lock(&self.registries).insert(directory.to_path_buf(), (key, computed.clone()));
        computed
    }

    pub(crate) fn forget_overrides(&self, directory: Option<&Path>) {
        let mut registries = lock(&self.registries);
        match directory {
            Some(directory) => drop(registries.remove(directory)),
            None => registries.clear(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::future::BoxFuture;
    use opencode_llm::hooks::{HttpRequest, HttpResponse, Loaded};
    use std::time::{Duration, SystemTime};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Default)]
    struct Fake {
        asked: Mutex<Vec<String>>,
        infos: Mutex<Vec<Value>>,
    }

    impl AuthLoader for Fake {
        fn options<'a>(&'a self, provider_id: &'a str, stored: &'a auth::Info, info: &'a Value) -> BoxFuture<'a, Option<Loaded>> {
            Box::pin(async move {
                lock(&self.asked).push(format!("options {provider_id} {}", info["models"].as_object().map_or(0, |m| m.len())));
                lock(&self.infos).push(info.clone());
                let auth::Info::Api { key, .. } = stored else { return None };
                Some(Loaded { options: json!({"apiKey": format!("loaded-{key}")}), fetch: Some(format!("fn-{provider_id}")) })
            })
        }

        fn fetch<'a>(&'a self, _handle: &'a str, _request: HttpRequest) -> BoxFuture<'a, Result<HttpResponse, opencode_llm::Error>> {
            Box::pin(async { Err(opencode_llm::Error::Config("unused".into())) })
        }

        fn models<'a>(&'a self, provider_id: &'a str, _info: &'a Value, stored: Option<&'a auth::Info>) -> BoxFuture<'a, Option<Value>> {
            Box::pin(async move {
                lock(&self.asked).push(format!("models {provider_id} {}", stored.is_some()));
                (provider_id == "cat").then(|| json!({"plugged": {"name": "Plugged", "api": {"npm": "@ai-sdk/openai-compatible"}, "limit": {"context": 10, "output": 5}}}))
            })
        }
    }

    #[tokio::test]
    async fn hooks_replace_models_and_reach_config_providers() {
        let catalog = json!({"cat": {"id": "cat", "env": [], "models": {"orig": {"limit": {"context": 1, "output": 1}}}}});
        let config = json!({"provider": {"mine": {"models": {"m": {}}}, "off": {"models": {"m": {}}}}, "disabled_providers": ["off"]});
        let api = |key: &str| auth::Info::Api { key: key.into(), metadata: None };
        let entries = vec![("cat".to_string(), api("c")), ("mine".to_string(), api("m")), ("off".to_string(), api("o"))];
        let loader = Fake::default();
        let found = overrides(&reqwest::Client::new(), &loader, &catalog, &config, &entries, Path::new("auth.json")).await;
        // Catalog providers get `provider.models` then their loader; a config-only provider only its loader; disabled ones neither.
        assert_eq!(*lock(&loader.asked), ["models cat true", "options cat 1", "options mine 1"]);
        let providers = provider::build_with(&catalog, &config, &entries, &found);
        let cat = providers.iter().find(|p| p.id == "cat").unwrap();
        assert_eq!(cat.models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["plugged"]);
        assert_eq!((cat.options["apiKey"].as_str(), cat.fetch.as_deref()), (Some("loaded-c"), Some("fn-cat")));
        let mine = providers.iter().find(|p| p.id == "mine").unwrap();
        assert_eq!((mine.options["apiKey"].as_str(), mine.fetch.as_deref()), (Some("loaded-m"), Some("fn-mine")));
        assert!(providers.iter().all(|p| p.id != "off"));
        // A config-only provider is shown to its loader as its `database` entry: config name, env and options.
        let info = &lock(&loader.infos)[1];
        assert_eq!((&info["id"], &info["name"], &info["source"], &info["env"], &info["options"]), (&json!("mine"), &json!("mine"), &json!("config"), &json!([]), &json!({})));
        assert!(info["models"]["m"].is_object());
    }

    #[tokio::test]
    async fn config_loader_sees_config_options_and_loses_to_them() {
        let catalog = json!({});
        let config = json!({"provider": {"mine": {"name": "Mine", "env": ["MINE_KEY"], "options": {"baseURL": "http://x", "apiKey": "config"}, "models": {"m": {}}}}});
        let entries = vec![("mine".to_string(), auth::Info::Api { key: "stored".into(), metadata: None })];
        let loader = Fake::default();
        let found = overrides(&reqwest::Client::new(), &loader, &catalog, &config, &entries, Path::new("auth.json")).await;
        let info = lock(&loader.infos)[0].clone();
        assert_eq!((&info["name"], &info["env"], &info["options"]), (&json!("Mine"), &json!(["MINE_KEY"]), &json!({"baseURL": "http://x", "apiKey": "config"})));
        // The loader's options are merged before config is re-applied, so config keys win.
        let providers = provider::build_with(&catalog, &config, &entries, &found);
        let mine = providers.iter().find(|p| p.id == "mine").unwrap();
        assert_eq!((mine.options["apiKey"].as_str(), mine.options["baseURL"].as_str(), mine.fetch.as_deref()), (Some("config"), Some("http://x"), Some("fn-mine")));
        // Without a stored credential the loader never runs.
        let none = Fake::default();
        overrides(&reqwest::Client::new(), &none, &catalog, &config, &Vec::new(), Path::new("auth.json")).await;
        assert!(lock(&none.asked).is_empty());
    }

    fn engine(root: &Path) -> Engine {
        let paths = opencode_core::paths::Paths {
            home: root.join("home"),
            data: root.join("data"),
            cache: root.join("cache"),
            config: root.join("config"),
            state: root.join("state"),
            tmp: root.join("tmp"),
        };
        Engine::with_store(paths, opencode_storage::Store::open_in_memory().unwrap())
    }

    // Serves `body` as `/api.json` and counts requests.
    async fn models_server(body: &'static str) -> (String, Arc<Mutex<u32>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(Mutex::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buffer = [0u8; 4096];
                let _ = socket.read(&mut buffer).await;
                *lock(&counter) += 1;
                let reply = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                let _ = socket.write_all(reply.as_bytes()).await;
            }
        });
        (url, hits)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn catalog_is_loaded_once_and_refreshed_when_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = engine(tmp.path());
        let (url, hits) = models_server(r#"{"fresh":{"id":"fresh","models":{}}}"#).await;
        let file = tmp.path().join("cache").join("models.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, r#"{"stale":{"id":"stale","models":{}}}"#).unwrap();
        let old = SystemTime::now() - catalog::TTL - Duration::from_secs(5);
        std::fs::OpenOptions::new().write(true).open(&file).unwrap().set_modified(old).unwrap();
        let source = Source { url, file: file.clone(), locks: tmp.path().join("state").join("locks"), override_file: None, disabled: false };
        engine.set_catalog_source(source.clone());

        // The stale cache is served at once; the background refresh downloads and replaces it.
        let (first, generation) = engine.catalog().await;
        assert!(first.get("stale").is_some());
        for _ in 0..100 {
            if engine.loaded_catalog().is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let (second, next) = engine.catalog().await;
        assert!(second.get("fresh").is_some() && next != generation, "{second}");
        assert_eq!(*lock(&hits), 1);
        assert!(catalog::fresh(&file));

        // Disabled fetching never reaches the network, even without a cache.
        std::fs::remove_file(&file).unwrap();
        engine.set_catalog_source(Source { disabled: true, ..source });
        assert_eq!(*engine.catalog().await.0, json!({}));
        assert_eq!(*engine.cached_catalog().0, json!({}));
        assert!(lock(&engine.catalog).refresh.is_none());
        assert_eq!(*lock(&hits), 1);
        engine.shutdown().await;
    }
}
