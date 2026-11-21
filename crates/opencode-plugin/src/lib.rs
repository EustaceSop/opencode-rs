//! Optional Bun sidecar that runs opencode JS/TS plugins and custom tool files (v1 hook API, opencode v1.19.5).
//!
//! Nothing is spawned unless plugin specs or tool files are passed to [`PluginHost::start`]. The host script
//! (`js/host.mjs`, embedded) is written to `<cache_dir>/plugin-host/host.mjs` and run as
//! `bun --no-install run host.mjs` with the instance directory as cwd.
//!
//! # Protocol
//! Newline-delimited JSON-RPC 2.0 over the sidecar's stdin/stdout; stderr is free-form log text. stdout lines
//! that are not JSON-RPC objects (plugin `Bun.$` output etc.) are forwarded to the log callback as `[stdout] ...`.
//! `console.log/info/debug` inside the sidecar are redirected to stderr.
//!
//! Rust → sidecar requests:
//! - `init {directory, worktree, project, serverUrl, headers?, plugins: [{spec, options?, npm?: {name, dir}}],
//!   toolFiles, resolveDirs, sdkPath?, zodPath?, opencodeVersion?}` →
//!   `{bun, sdk, plugins: [{index, spec, id, hooks, tools, auth, provider}], hooks, tools, workspaces, errors}`
//! - `hook.trigger {name, input, output}` → `{output}` (every plugin's hook awaited in load order on one object)
//! - `config {config}` → `{config}`
//! - `tool.list` → `[{id, description, parameters, converter}]`
//! - `tool.execute {id, args, context: {sessionID, messageID, agent, callID}}` →
//!   `{title, output, metadata, attachments?}`; error `-32602` with `data.kind = "invalid_arguments"` when the
//!   Zod `safeParse` fails
//! - `auth.list`, `auth.authorize {provider, method, inputs?}` (function `callback` → `handle`),
//!   `auth.callback {handle, code?}`, `auth.loader {provider, auth, providerInfo}` → `{options, functions}`
//!   (function-valued option keys → handles), `provider.list`, `provider.models {id, provider, auth?}`
//! - `fetch.proxy {handle, stream, url, method, headers: [[k, v]], body?: base64}` →
//!   `{status, statusText, headers: [[k, v]]}`, followed by sidecar notifications
//!   `fetch.chunk {stream, data: base64}`* and one `fetch.end {stream, error?}`; `stream` is chosen by the caller
//! - `shutdown` → `null` after running `dispose` hooks (the process is then killed by the host; it also exits on
//!   stdin EOF)
//!
//! Rust → sidecar notifications: `event {event: {id, type, properties}}`, `$/cancel {id}` (aborts the request's
//! `AbortSignal`, i.e. `ToolContext.abort`), `fetch.cancel {stream}`.
//!
//! Sidecar → Rust requests: `tool.ask {requestID, callID, sessionID, permission, patterns, always, metadata}`
//! (answered from the calling tool's `ToolContext.permissions`), `auth.get {provider}` (the loader's `getAuth`).

mod discover;
mod hooks;
mod host;
mod tool;

pub use discover::{
    BUN_INSTALL_HINT, DEPRECATED_PLUGIN_PACKAGES, Discovered, InstallOptions, InstallOutcome, InstallStatus, NpmTarget,
    PluginSpec, discover, find_bun, install, is_deprecated_plugin, is_path_plugin_spec, npm_target,
    parse_plugin_specifier, sanitize,
};
pub use hooks::*;
pub use host::{
    AuthFn, AuthLoaded, FetchRequest, FetchResponse, HOST_SCRIPT, InitResult, LoadError, LoadedPlugin, LogFn,
    PluginError, PluginHost, PluginOptions, ToolInfo, basic_auth_header,
};
pub use tool::PluginTool;
