# 05 — Integration Subsystems (opencode v1.19.5 → Rust)

Source root: `D:\Download\opecode\opencode-ts` (read-only). Path prefixes are relative to `packages/`
(e.g. `opencode/src/mcp/index.ts:38` = `packages/opencode/src/mcp/index.ts` line 38). Repo-root files are prefixed `/`
(e.g. `/github/action.yml`). Confidence tags: **[src]** read directly from source; **[lib]** behaviour lives inside an npm
dependency that is not vendored in the repo (no `node_modules`) — must be verified; **[assumption]** inference, marked inline.
`UNKNOWN` items are collected in §20.

Global paths used throughout (`core/src/global.ts:10-29`, `xdg-basedir@5.1.0` [lib]):

| Symbol | Value | Notes |
|---|---|---|
| `Global.Path.data` | `$XDG_DATA_HOME/opencode` (default `~/.local/share/opencode`) [lib default] | snapshots, mcp-auth.json, worktrees, repos, logs |
| `Global.Path.cache` | `$XDG_CACHE_HOME/opencode` (default `~/.cache/opencode`) | npm package cache `packages/`, `bin/` |
| `Global.Path.bin` | `<cache>/bin` | LSP/ripgrep downloads; **appended to PATH** by `which()` (`core/src/util/which.ts:5-13`) |
| `Global.Path.config` | `$XDG_CONFIG_HOME/opencode`, overridden by `OPENCODE_CONFIG_DIR` in `Global.make` (`core/src/global.ts:64`) | |
| `Global.Path.state` | `$XDG_STATE_HOME/opencode` | file locks `<state>/locks/<sha1(key)>.lock` (`core/src/util/flock.ts:21,321`) |
| `Global.Path.log` | `<data>/log` | `opencode.log` (`core/src/observability/logging.ts:49`) |
| `Global.Path.repos` | `<data>/repos` | reference repo cache (§17.3) |
| `Global.Path.tmp` | `os.tmpdir()/opencode` | |

All dirs are `mkdir -p`'d at module load (`core/src/global.ts:35-43`).

---

## 0. Subsystem map

| Subsystem | TS location | Default state in 1.19.5 | Rust mapping (suggested) |
|---|---|---|---|
| MCP client | `opencode/src/mcp/*` | on (servers from config) | `rmcp` client (stdio/streamable-HTTP/SSE) + own OAuth store, or hand-rolled JSON-RPC |
| LSP client | `opencode/src/lsp/*` | **OFF unless `lsp` config key is truthy** (`opencode/src/lsp/lsp.ts:151`) | `lsp-types` + `async-lsp` (client mode) or custom Content-Length framing over `tokio::process` |
| Formatters | `opencode/src/format/*` | **OFF unless `formatter` key truthy** (`opencode/src/format/index.ts:120`) | `tokio::process::Command` |
| Plugins v1 (hooks) | `opencode/src/plugin/*`, `plugin/src/index.ts` | on | **Bun sidecar** (§4.12) |
| Plugins v2 (draft/transform) | `core/src/plugin*.ts`, `plugin/src/v2/*` | on (core location services) | sidecar, draft-diff protocol (§4.10) |
| Snapshot | `opencode/src/snapshot/index.ts`, `core/src/snapshot.ts` | on when project VCS is git and `snapshot !== false` | shell out to `git` (keep CLI semantics) |
| PTY | `core/src/pty*.ts` | on (HTTP API) | `portable-pty` |
| File watcher | `core/src/filesystem/watcher.ts` | `.git` dir only; worktree watch needs `OPENCODE_EXPERIMENTAL_FILEWATCHER` | `notify` + `globset` |
| ACP | `opencode/src/acp/*`, `opencode/src/cli/cmd/acp.ts` | `opencode acp` | `agent-client-protocol` crate (verify version, §20) |
| IDE | `opencode/src/ide/index.ts` | **dead code** (only referenced by `opencode/test/ide/ide.test.ts`) | drop / port as utility |
| GitHub agent | `opencode/src/cli/cmd/github*.ts`, `/github/action.yml` | `opencode github run|install` | `octocrab` + `reqwest` |
| Share | `opencode/src/share/session.ts` | **discontinued** (share always fails) | stub |
| Installation/upgrade | `opencode/src/installation/index.ts`, `opencode/src/cli/upgrade.ts` | notify-only check | `reqwest` + `semver` |
| Observability | `core/src/observability*.ts` | file log always; OTLP only if `OTEL_EXPORTER_OTLP_ENDPOINT` | `tracing` + `opentelemetry-otlp` (http-json) |
| CodeMode | `codemode/*`, `opencode/src/tool/code-mode.ts` | off; `OPENCODE_EXPERIMENTAL_CODE_MODE` | embed JS engine (`rquickjs`/`boa`) or sidecar |
| ripgrep | `core/src/ripgrep/binary.ts` | system `rg` else download 15.1.0 | same logic, `reqwest`+`zip`/`flate2`+`tar` |
| fzf | — | **not used** (no `fzf` reference in `opencode/src`, `core/src`); fuzzy finding uses `@ff-labs/fff-bun@0.10.5` (`core/package.json:89`, `core/src/filesystem/fff.bun.ts:1-13`) | `nucleo` / `fuzzy-matcher` |

---

## 1. MCP

### 1.1 Config schema [src] (`core/src/v1/config/mcp.ts`, `core/src/v1/config/config.ts:113-115,182-184`)

`mcp: Record<name, Local | Remote | { enabled: boolean }>` — the bare `{enabled}` shape exists so a layered config can
toggle an entry defined elsewhere (configs are deep-merged); an entry that still has no `type` after merge is logged
"Ignoring MCP config entry without type" and skipped (`opencode/src/mcp/index.ts:119-121,509-512`).

| Key | Local | Remote | Semantics |
|---|---|---|---|
| `type` | `"local"` | `"remote"` | discriminator |
| `command` | `string[]` (required) | — | `[cmd, ...args]` |
| `cwd` | optional | — | resolved against instance directory (`index.ts:346`) |
| `environment` | `Record<string,string>` | — | merged over `process.env` (`index.ts:351-356`) |
| `url` | — | required | must pass `URL.canParse`, else status failed `Invalid MCP URL for "<key>"` (`index.ts:243-249`) |
| `headers` | — | `Record<string,string>` | sent as `requestInit.headers` on both transports |
| `oauth` | — | `OAuth | false` | `false` disables OAuth auto-detection; object fields below |
| `oauth.clientId` / `clientSecret` / `scope` | — | strings | no clientId ⇒ RFC 7591 dynamic registration [lib] |
| `oauth.callbackPort` | — | int 1..65535 | shorthand ⇒ `http://127.0.0.1:<port>/mcp/oauth/callback`; ignored if `redirectUri` set |
| `oauth.redirectUri` | — | string | default `http://127.0.0.1:19876/mcp/oauth/callback` |
| `enabled` | bool | bool | `false` ⇒ status `disabled`, not connected at boot |
| `timeout` | PositiveInt ms | PositiveInt ms | connect + tools/list timeout and per-request timeout (see 1.5) |
| `experimental.mcp_timeout` | global | global | fallback per-request timeout |

Schema description says timeout "Defaults to 5000" (`mcp.ts:21,57`) — **code uses 30 000** (`opencode/src/mcp/index.ts:38`,
`opencode/src/mcp/catalog.ts:11`). Port the code value.

### 1.2 Lifecycle & state [src]

- Per-instance (per directory) state `{config, status, clients, defs, instructions}` (`index.ts:142-148`), built lazily by
  `InstanceState.make` (`index.ts:492-558`): every configured entry is connected **concurrently** (`concurrency: "unbounded"`).
- Status union (`index.ts:83-107`): `connected | disabled | failed{error} | needs_auth | needs_client_registration{error}`.
- Client identity: `new Client({ name: "opencode", version: InstallationVersion }, { capabilities: { roots: {} } })`
  (`index.ts:39-50,75-82`). `sampling`, `elicitation`, `tasks` intentionally **not** advertised (comments cite issues
  #11948, #23066, #28567). `roots/list` handler returns `[{ uri: file://<instance directory> }]`.
- After connect: `tools/list` only if `serverCapabilities.tools` (`index.ts:391`); `instructions` = `client.getInstructions()?.trim()`.
- Runtime ops: `add(name, cfg)` (runtime-only entry in `s.config`), `connect(name)` (re-creates with `enabled:true`),
  `disconnect(name)` (close, status `disabled`) (`index.ts:641-659`).
- Events: `mcp.tools.changed {server}` on close and on `notifications/tools/list_changed`; `mcp.browser.open.failed {mcpName,url}`
  (`schema/src/mcp-event.ts:7,14`). TUI toast via `TuiEvent.ToastShow` on auth-required (`index.ts:304-322`).

### 1.3 Transports [src]

**Local (stdio)** (`index.ts:340-370`): `StdioClientTransport({ stderr: "pipe", command, args, cwd, env })`.
`env = {...process.env, ...(cmd === "opencode" ? {BUN_BE_BUN: "1"} : {}), ...environment}` — a config of
`command: ["opencode", ...]` relies on the compiled Bun binary behaving as `bun` (porting risk R6). stderr is piped but
not consumed in this file (`UNKNOWN` whether the SDK drains it [lib]).

**Remote** (`index.ts:236-336`): ordered attempt list, first success wins:
1. `StreamableHTTPClientTransport(url, { authProvider, requestInit: { headers } })` (`:271`)
2. `SSEClientTransport(url, same)` (`:278`)

Error classification per attempt: `UnauthorizedError` or (authProvider && message contains `"OAuth"`) ⇒ auth error;
if message contains `"registration"` or `"client_id"` ⇒ `needs_client_registration` ("Server does not support dynamic client
registration. Please provide clientId in config."), else `needs_auth` and the transport is stored in
`pendingOAuthTransports` (`:312`). Auth errors **stop** the fallback loop (`:331`). Otherwise `failed{error.message}`.
`authProvider` is created unless `oauth === false` (`:251-265`), so OAuth discovery is attempted on 401 for every remote.

**Shutdown** (`index.ts:530-555`): for stdio clients, on non-Windows collect descendants recursively via
`pgrep -P <pid>` (`:418-440`), `SIGTERM` each, then `client.close()`. Windows: no descendant kill.
`onclose` handler marks status `failed{"Connection closed"}` and publishes `mcp.tools.changed` (`:442-456`).

### 1.4 Timeouts [src]

| Operation | Value | Ref |
|---|---|---|
| connect (`client.connect` incl. initialize) | `mcp.timeout ?? 30000` via `withTimeout` | `index.ts:226,286,359` |
| tools/list (each page) | `mcp.timeout ?? 30000` | `catalog.ts:38-40` |
| tools/call, prompts/*, resources/* | `requestTimeout = runtime cfg.timeout ?? config timeout ?? experimental.mcp_timeout`; may be `undefined` ⇒ SDK default (`UNKNOWN`, §20) | `index.ts:661-664` |
| tools/call progress | `resetTimeoutOnProgress: true`, `onprogress: () => {}` (forces progressToken) | `catalog.ts:59-66` |
| OAuth callback wait | 5 min | `oauth-callback.ts:24` |

### 1.5 Pagination & tolerant listing [src] (`catalog.ts:18-36,145-168`)

`paginate()` loops ≤1000 pages, throws on duplicate cursor. `tools/list` failures whose message matches
`/can't resolve reference|resolves to more than one schema|outputSchema|schema.*reference|reference.*schema/i` are retried
as raw `tools/list` with a schema that **omits `outputSchema`**. List failures make `defs()` return `undefined` ⇒ server
create fails "Failed to get tools".

### 1.6 Exposure to the model [src]

- **Tool name**: `sanitize(server) + "_" + sanitize(tool)` with `sanitize = s.replace(/[^a-zA-Z0-9_-]/g, "_")` (`catalog.ts:117-119`).
  Collisions are last-write-wins in a plain object (`index.ts:681-683`).
- **Schema**: `{...inputSchema, type:"object", properties: inputSchema.properties ?? {}, additionalProperties:false}` (`catalog.ts:43-48`),
  then `ProviderTransform.schema(model, …)` (`opencode/src/session/tools.ts:393-397`, see spec 03 §5.7).
- **Permission**: `ctx.ask({ permission: <toolName>, patterns:["*"], always:["*"] })` (`session/tools.ts:408`); tools hidden via
  `Permission.visibleTools` in registry (`opencode/src/tool/registry.ts:286`).
- **Call**: `client.callTool({name, arguments: args||{}}, CallToolResultSchema, {resetTimeoutOnProgress, signal, timeout, onprogress})`.
  `isError` ⇒ throw Error(joined non-empty text blocks, `"\n\n"`) or `"MCP tool returned an error"`. If `content` empty and
  `structuredContent` present ⇒ content = `[{type:"text", text: JSON.stringify(structuredContent)}]` (`catalog.ts:53-81`).
- **Result mapping** (`session/tools.ts:426-470`): `text` → joined `"\n\n"`; `image` → file attachment `data:<mime>;base64,…`;
  `resource` → `text` appended, `blob` attached only if mime ∈ {`application/pdf`,`image/gif`,`image/jpeg`,`image/png`,`image/webp`}
  and ≤10 MiB, else placeholder text `[Binary MCP resource omitted: …]` (`session/tools.ts:32-39`). Output passes `truncate.output`.
- Hooks `tool.execute.before/after` fire around MCP calls (`session/tools.ts:402-425`); `after` receives the raw `CallToolResult`.
- When `OPENCODE_EXPERIMENTAL_CODE_MODE` is on, per-tool MCP exposure is skipped (`session/tools.ts:388`) and only the
  `execute` CodeMode tool is offered (§15).
- **Resource tools** (only if some connected server advertises `resources`, `session/tools.ts:136-139`):
  `list_mcp_resources`, `list_mcp_resource_templates`, `read_mcp_resource` (`session/tools.ts:27-31`). Permission `read` with
  patterns `mcp:<server>:*` (list) or `mcp:<server>:<uri>` + always `mcp:<server>:*` (read) (`session/tools.ts:173,343-347`).
  Resource keys are `escape(server) + ":" + uri` where escape = `%`→`%25`, `:`→`%3A`; prompt keys `sanitize(server)+":"+sanitize(name)` (`catalog.ts:103-111`).
- **Prompts → slash commands** (`opencode/src/command/index.ts:105-127`): command name = prompt key, `source:"mcp"`, template is
  lazily `getPrompt(client, name, {arg_i: "$<i+1>"})` and joins text message contents with `"\n"`; hints `$1..$n`.
- **User-attached resources**: file parts with `source.type === "resource"` are read via `readResource` at prompt time
  (`session/prompt.ts:703-716`).
- **Server instructions** in system prompt (`opencode/src/session/system.ts:121-137`): `<mcp_instructions>` / `<server name="…">`
  block, servers sorted by name, omitted if every tool of that server is permission-disabled.
- Logging notifications are mapped to log levels (`index.ts:474-490`); not surfaced to the model.

### 1.7 OAuth for remote MCP [src] (`oauth-provider.ts`, `oauth-callback.ts`, `auth.ts`, `index.ts:806-975`)

Client metadata (`oauth-provider.ts:43-53`):
```json
{ "redirect_uris": ["<redirectUrl>"], "client_name": "OpenCode", "client_uri": "https://opencode.ai",
  "grant_types": ["authorization_code","refresh_token"], "response_types": ["code"],
  "token_endpoint_auth_method": "client_secret_post" | "none", "scope": "<cfg.scope, if set>" }
```
Discovery of authorization server / protected-resource metadata, PKCE, registration and token exchange are performed by
`@modelcontextprotocol/sdk@1.29.0` (`opencode/package.json:83`) [lib] — exact endpoint discovery order `UNKNOWN` (§20).
Non-http(s) authorization URLs rejected (`oauth-provider.ts:127-133`).

**Storage** `<data>/mcp-auth.json`, mode `0600`, guarded by flock key `mcp-auth:<filepath>` (`auth.ts:207-208,246-252`):
```ts
Record<mcpName, { tokens?: {accessToken, refreshToken?, expiresAt? /*unix seconds*/, scope?},
                  clientInfo?: {clientId, clientSecret?, clientIdIssuedAt?, clientSecretExpiresAt?},
                  codeVerifier?, oauthState?, serverUrl? }>
```
Tokens/clientInfo are only honoured when `entry.serverUrl === configured url` (`auth.ts:259-265`, `getForUrl`). Expired
`clientSecretExpiresAt` ⇒ re-register (`oauth-provider.ts:67-69`). `expiresAt = now/1000 + expires_in`.

**Callback server** (`oauth-callback.ts`): Node `http` on `127.0.0.1:<port>` (default 19876, path `/mcp/oauth/callback`);
if the port is already in use it **assumes another instance owns it and does not listen** (`:116-119`). Validates
`state` presence (400), `error` param (rejects pending), `code` presence, and `state ∈ pendingAuths` (CSRF guard). Responses are
`OauthCallbackPage.success/error({provider:"MCP"})` HTML (`core/src/oauth/page`). Server stops when no pending auths.

**Flows**:
1. Implicit (on connect): 401 ⇒ `needs_auth`, toast "Run: opencode mcp auth <name>".
2. `startAuth(name)` (`index.ts:806-870`): ensure callback server for effective redirect URI; generate 32-byte hex state and persist
   it; build `McpOAuthPendingProvider` (tokens/clientInfo held in memory until `commit()`); `client.connect` over
   StreamableHTTP. Success without redirect ⇒ already authorised (commit, return empty URL). `UnauthorizedError` + captured URL ⇒
   store pending transport, return `{authorizationUrl, oauthState}`.
3. `authenticate(name)` (`:872-916`): `startAuth` → `waitForCallback(state)` → open browser (`McpBrowser.open`, failure ⇒
   `mcp.browser.open.failed`) → compare stored state ("OAuth state mismatch - potential CSRF attack") → `finishAuth`.
4. `finishAuth(name, code)` (`:918-942`): `transport.finishAuth(code)` [lib token exchange] → `provider.commit()` writes
   `{tokens, clientInfo (only if DCR)}` + `serverUrl` → clear verifier → reconnect.
5. `removeAuth(name)` deletes entry, cancels pending callback.
6. `getAuthStatus` ⇒ `authenticated | expired | not_authenticated` (`:960-970`).

Browser open: `openUrl(url)` (`core/src/open`), considered success if the launcher did not exit non-zero within 500 ms
(`opencode/src/mcp/browser.ts:14-34`).

CLI (`opencode/src/cli/cmd/mcp.ts`): `mcp list`, `mcp auth [name]`, `mcp auth list`, `mcp logout [name]`, `mcp add [name]`
(`--url`, env `KEY=VALUE`, header `KEY=VALUE`), `mcp debug <name>` (`:96-664`).
HTTP (`opencode/src/server/routes/instance/httpapi/groups/mcp.ts:32-39`): `GET/POST /mcp`, `POST|DELETE /mcp/:name/auth`,
`POST /mcp/:name/auth/callback`, `POST /mcp/:name/auth/authenticate`, `POST /mcp/:name/connect`, `POST /mcp/:name/disconnect`.

### 1.8 Rust mapping
- `rmcp` provides client + `TokioChildProcess` stdio transport and streamable-HTTP client; SSE-client support and OAuth
  (`auth` feature) must be verified against the pinned crate version (§20). If gaps exist, implement SSE (`GET` event-stream +
  `POST` endpoint event) and OAuth manually with `oauth2` crate; keep the **on-disk `mcp-auth.json` format byte-compatible**.
- Keep fallback order StreamableHTTP→SSE and the auth-error short-circuit.
- Implement `roots/list` handler; do not advertise sampling/elicitation.
- Kill process tree: on Unix use process groups (`setsid` + `killpg`) — superset of `pgrep -P`; on Windows use a Job Object
  (improvement over TS, which leaks grandchildren).
- `cmd == "opencode"` + `BUN_BE_BUN`: map to the bundled Bun sidecar binary (`bun <args>`), see R6.

---

## 2. LSP

### 2.1 Enablement & config [src]

- `lsp` absent/false ⇒ **no LSP at all** ("all LSPs are disabled", `opencode/src/lsp/lsp.ts:151-152`). `true` ⇒ all built-ins.
  Object ⇒ built-ins + overrides. Schema `core/src/v1/config/lsp.ts:5-78`:
  `Record<id, {disabled:true} | {command: string[], extensions?: string[], disabled?: bool, env?: Record, initialization?: Record}>`;
  custom ids (not in `builtinServerIds`, `lsp.ts:22-61`) **must** declare `extensions`.
- Override merge (`opencode/src/lsp/lsp.ts:160-181`): `disabled` ⇒ remove; else replace `spawn` with
  `command[0] command[1..]`, `cwd=root`, `env={...process.env,...env}`, `initialization` = item.initialization; `root` kept from the
  built-in or defaults to instance dir; `extensions` = item ?? built-in ?? `[]` (empty = matches every file, `lsp.ts:255`).
- `ty` vs `pyright` mutually exclusive: `OPENCODE_EXPERIMENTAL_LSP_TY` ⇒ drop pyright, else drop ty (`lsp.ts:98-108`).
- `OPENCODE_DISABLE_LSP_DOWNLOAD` gates most auto-installs (`opencode/src/effect/runtime-flags.ts:22`) — **not** typescript,
  biome (they call `Npm.which` unconditionally; `Npm.which` auto-installs, `core/src/npm.ts:200-249`).

### 2.2 Built-in servers [src] (`opencode/src/lsp/server.ts`; line = `export const` line)

Root helpers (`server.ts:31-79`): `NearestRoot(include, exclude?)` walks up from file dir to instance dir; exclude hit ⇒ no server;
no include hit ⇒ **instance directory**. `StrictNearestRoot` ⇒ `undefined` when not found. "lockfiles" = `package-lock.json,
bun.lockb, bun.lock, pnpm-lock.yaml, yarn.lock`.

| id (line) | extensions | root | command | install source if missing |
|---|---|---|---|---|
| `deno` (88) | .ts .tsx .js .jsx .mjs | strict `deno.json`/`deno.jsonc` | `deno lsp` | none |
| `typescript` (115) | .ts .tsx .js .jsx .mjs .cjs .mts .cts | lockfiles, excl. deno.json(c) | `<npm-cache>/typescript-language-server --stdio`; requires `typescript/lib/tsserver.js` resolvable from instance dir; init `{tsserver:{path}}` | npm `typescript-language-server` (ungated) |
| `vue` (144) | .vue | lockfiles | `vue-language-server --stdio` | npm `@vue/language-server` |
| `eslint` (173) | ts/js family + .vue | lockfiles | `node <bin>/vscode-eslint/server/out/eslintServer.js --stdio`; requires `eslint` resolvable | `https://github.com/microsoft/vscode-eslint/archive/refs/heads/main.zip` → extract → `npm install` + `npm run compile` (`:183-208`) |
| `oxlint` (224) | ts/js family + .vue .astro .svelte | `.oxlintrc.json`, lockfiles, `package.json` | `node_modules/.bin/oxlint[.cmd]` (root→worktree) or PATH; if `--help` mentions `--lsp` ⇒ `oxlint --lsp`, else `oxc_language_server` | none |
| `biome` (296) | ts/js family, .json .jsonc .vue .astro .svelte .css .graphql .gql .html | `biome.json(c)`, lockfiles | `<root>/node_modules/.bin/biome` / PATH / npm `biome` (only if `biome` resolvable) + `lsp-proxy --stdio` | npm (ungated) |
| `gopls` (358) | .go | `go.work` else `go.mod`/`go.sum` | `gopls` | `go install golang.org/x/tools/gopls@latest` with `GOBIN=<bin>` (requires `go`) |
| `ruby-lsp` (392) | .rb .rake .gemspec .ru | `Gemfile` | `rubocop --lsp` | `gem install rubocop --bindir <bin>` |
| `ty` (424) | .py .pyi | pyproject/… (`:427-436`) | `ty server`; venv lookup `$VIRTUAL_ENV`, `<root>/.venv`, `<root>/venv`; init `{pythonPath}` | none (flag-gated) |
| `pyright` (485) | .py .pyi | `pyproject.toml setup.py setup.cfg requirements.txt Pipfile pyrightconfig.json` | `pyright-langserver --stdio`; init `{pythonPath}` | npm `pyright` bin `pyright-langserver` |
| `elixir-ls` (529) | .ex .exs | `mix.exs`,`mix.lock` | `elixir-ls` or `<bin>/elixir-ls-master/release/language_server.{sh,bat}` | `https://github.com/elixir-lsp/elixir-ls/archive/refs/heads/master.zip`, then `mix deps.get`, `mix compile`, `mix elixir_ls.release2 -o release` (`MIX_ENV=prod`) |
| `zls` (585) | .zig .zon | `build.zig` | `zls` | GitHub API `https://api.github.com/repos/zigtools/zls/releases/latest`, asset `zls-{aarch64,x86_64,x86}-{linux,macos,windows}.{tar.xz,zip}` (8 combos `:627-634`) → `<bin>/zls[.exe]`; requires `zig` |
| `csharp` (687) | .cs .csx | `.slnx .sln .csproj global.json` | `roslyn-language-server --stdio --autoLoadProjects` | `dotnet tool install --global roslyn-language-server --prerelease` (`:756`) |
| `razor` (703) | .razor .cshtml | same | roslyn + `--razorDesignTimePath=<…/Microsoft.NET.Sdk.Razor.DesignTime.targets>` (`:715-730`) | same |
| `fsharp` (823) | .fs .fsi .fsx .fsscript | `.slnx .sln .fsproj global.json` | `fsautocomplete` | `dotnet tool install fsautocomplete --tool-path <bin>` |
| `sourcekit-lsp` (856) | .swift .objc `objcpp` (sic, no dot) | `Package.swift *.xcodeproj *.xcworkspace` | `sourcekit-lsp` or `xcrun --find sourcekit-lsp` | none |
| `rust` (890) | .rs | nearest `Cargo.toml`/`Cargo.lock`, then climb to a `Cargo.toml` containing `[workspace]` within worktree | `rust-analyzer` | none |
| `clangd` (935) | .c .cpp .cc .cxx .c++ .h .hpp .hh .hxx .h++ | `compile_commands.json compile_flags.txt .clangd` | `clangd --background-index --clang-tidy` (PATH, `<bin>/clangd`, `<bin>/clangd_*/bin/clangd`) | `https://api.github.com/repos/clangd/clangd/releases/latest`, asset name contains platform token (`mac`/`linux`/`windows`) and tag, prefer .zip then .tar.xz → `<bin>/clangd_<tag>/bin/clangd`, symlink `<bin>/clangd` |
| `svelte` (1069) | .svelte | lockfiles | `svelteserver --stdio` | npm `svelte-language-server` |
| `astro` (1096) | .astro | lockfiles | `astro-ls --stdio`; requires tsserver; init `{typescript:{tsdk}}` | npm `@astrojs/language-server` |
| `jdtls` (1147) | .java | gradle/maven/eclipse markers via StrictNearestRoot chain (`:1149-1186`) | `java -jar <launcher> -configuration config_{mac,linux,win} -data <mkdtemp opencode-jdtls-data> …` (`:1249-1268`); requires Java ≥21 (parsed from `java -version`) | `https://www.eclipse.org/downloads/download.php?file=/jdtls/snapshots/jdt-language-server-latest.tar.gz` → `<bin>/jdtls` |
| `kotlin-ls` (1273) | .kt .kts | settings.gradle(.kts) → gradlew → build.gradle(.kts) → pom.xml | `<bin>/kotlin-ls/kotlin-lsp.{sh,cmd} --stdio` | version from `https://api.github.com/repos/Kotlin/kotlin-lsp/releases/latest` `.name` (strip `v`); download `https://download-cdn.jetbrains.com/kotlin-lsp/<ver>/kotlin-lsp-<ver>-{mac,linux,win}-{x64,aarch64}.zip` |
| `yaml-ls` (1361) | .yaml .yml | lockfiles | `yaml-language-server --stdio` | npm `yaml-language-server` |
| `lua-ls` (1387) | .lua | `.luarc.json` etc. (`:1389-1397`) | `lua-language-server` | GitHub `LuaLS/lua-language-server` latest, asset `lua-language-server-<tag>-{darwin,linux,win32}-{arm64,x64,ia32}.{tar.gz,zip}` → `<bin>/lua-language-server-<arch>-<platform>/bin/` |
| `php intelephense` (1515) | .php | `composer.json composer.lock .php-version` | `intelephense --stdio`; init `{telemetry:{enabled:false}}` | npm `intelephense` |
| `prisma` (1546) | .prisma | `schema.prisma prisma/schema.prisma prisma`, excl. `package.json` | `prisma language-server` | none |
| `dart` (1563) | .dart | `pubspec.yaml analysis_options.yaml` | `dart language-server --lsp` | none |
| `ocaml-lsp` (1580) | .ml .mli | `dune-project dune-workspace .merlin opam` | `ocamllsp` | none |
| `bash` (1596) | .sh .bash .zsh .ksh | instance dir | `bash-language-server start` (`:1609`) | npm `bash-language-server` |
| `terraform` (1622) | .tf .tfvars | `.terraform.lock.hcl terraform.tfstate *.tf` | `terraform-ls serve`; init `{experimentalFeatures:{prefillRequiredFields:true, validateOnSave:true}}` | `https://api.releases.hashicorp.com/v1/releases/terraform-ls/latest` → `builds[]` match `{arch: arm64|amd64, os: windows|<platform>}` → zip |
| `texlab` (1695) | .tex .bib | `.latexmkrc latexmkrc .texlabroot texlabroot` | `texlab` | GitHub `latex-lsp/texlab` latest, asset `texlab-{aarch64,x86_64}-{macos,windows,linux}.{tar.gz,zip}` |
| `dockerfile` (1774) | `.dockerfile`, `"Dockerfile"` | instance dir | `docker-langserver --stdio` | npm `dockerfile-language-server-nodejs` |
| `gleam` (1800) | .gleam | `gleam.toml` | `gleam lsp` | none |
| `clojure-lsp` (1817) | .clj .cljs .cljc .edn | `deps.edn project.clj shadow-cljs.edn bb.edn build.boot` | `clojure-lsp listen` | none |
| `nixd` (1837) | .nix | `flake.nix` (if ≠ instance dir) → worktree → instance dir | `nixd` | none |
| `tinymist` (1867) | .typ .typc | `typst.toml` | `tinymist` | GitHub `Myriad-Dreamin/tinymist` latest, asset `tinymist-{aarch64,x86_64}-{apple-darwin,pc-windows-msvc,unknown-linux-gnu}.{tar.gz,zip}` |
| `haskell-language-server` (1951) | .hs .lhs | `stack.yaml cabal.project hie.yaml *.cabal` | `haskell-language-server-wrapper --lsp` | none |
| `julials` (1968) | .jl | `Project.toml Manifest.toml *.jl` | `julia --startup-file=no --history-file=no -e "using LanguageServer; runserver()"` | none |

Extraction tools: `Archive.extractZip` = PowerShell `Expand-Archive -Force` on Windows, `unzip -o -q` elsewhere
(`opencode/src/util/archive.ts`); tarballs via system `tar -xf/-xzf`. Executables `chmod 0755`.
Quirk: file extension lookup is `path.parse(file).ext || file` (`lsp.ts:213`) ⇒ the `"Dockerfile"` entry only matches when
`file` is literally `Dockerfile` (relative) — absolute paths never match [src, medium confidence on intent].

### 2.3 Client protocol [src] (`opencode/src/lsp/client.ts`)

- Transport: `vscode-jsonrpc@8.2.1` `createMessageConnection(StreamMessageReader(stdout), StreamMessageWriter(stdin))`.
- `initialize` (`:211-255`, timeout **45 s**): `rootUri`, `processId` = **server's** pid (sic), `workspaceFolders=[{name:"workspace", uri: root}]`,
  `initializationOptions = handle.initialization`, capabilities:
  `window.workDoneProgress`, `workspace.configuration`, `workspace.didChangeWatchedFiles.dynamicRegistration`,
  `workspace.diagnostics.refreshSupport=false`, `textDocument.synchronization{didOpen,didChange}`,
  `textDocument.diagnostic{dynamicRegistration, relatedDocumentSupport}`, `textDocument.publishDiagnostics.versionSupport=false`.
- Then `initialized`; if initialization present ⇒ `workspace/didChangeConfiguration {settings: initialization}` (`:260-266`).
- Server→client handlers: `textDocument/publishDiagnostics` (push cache; `typescript` seeds on first push, `:119-121`),
  `window/workDoneProgress/create` → null, `workspace/configuration` → dotted lookup into initialization (`:107-114`),
  `client/registerCapability|unregisterCapability` (tracks only `textDocument/diagnostic`), `workspace/workspaceFolders`,
  `workspace/diagnostic/refresh` → null.
- Document sync `notify.open(path)` (`:554-622`): languageId from `LANGUAGE_EXTENSIONS` (`opencode/src/lsp/language.ts`, copy verbatim)
  else `plaintext`. First open: `workspace/didChangeWatchedFiles{type:1 Created}` then `didOpen{version:0, text}`.
  Re-open: `didChangeWatchedFiles{type:2 Changed}` + `didChange{version+1}` — **always full text**; for incremental servers it is
  sent as a single range edit covering the old document.
- Diagnostics: push + pull. Pull uses `textDocument/diagnostic` (with registration `identifier`s) and `workspace/diagnostic`
  (`previousResultIds:[]`), each with 3 s timeout (`:293-345`). Wait modes (`:13-16,499-540`): debounce 150 ms after a fresh
  push for the same version; `document` mode ≤5 s; `full` mode ≤10 s. Merged push+pull, deduped.
- `shutdown()`: `connection.end(); dispose(); Process.stop(proc)` — **no LSP `shutdown`/`exit` requests** sent.

### 2.4 Manager & usage [src] (`opencode/src/lsp/lsp.ts`)

- Client key = `root + serverID`; concurrent spawns deduped via `spawning` map; spawn/initialize failure ⇒ key in `broken` set for
  the instance lifetime (`:208-297`). Each new client publishes `lsp.updated` (`schema/src/lsp-event.ts:5`).
- Files outside the instance (`containsPath`) get no clients (`:210`).
- Features used: `touchFile` + diagnostics; `hover`, `definition`, `references` (includeDeclaration), `implementation`,
  `documentSymbol`, `workspace/symbol` (filtered to kinds Class/Function/Method/Interface/Variable/Constant/Struct/Enum, top 10
  per server, `:58-96,433-440`), call hierarchy (`prepareCallHierarchy` + `incoming/outgoingCalls`). No completion, rename, code
  actions, formatting.
- Call sites: `edit` (`opencode/src/tool/edit.ts:197-201`), `write` (`opencode/src/tool/write.ts:75-95`, up to 5 other files'
  diagnostics `MAX_PROJECT_DIAGNOSTICS_FILES`), `apply_patch` (`opencode/src/tool/apply_patch.ts:269-271`) — all `touchFile(file,"document")`
  then `diagnostics()`; `read` warms the server without waiting (`opencode/src/tool/read.ts:119`, forked); `lsp` tool behind
  `OPENCODE_EXPERIMENTAL_LSP_TOOL` (`opencode/src/tool/lsp.ts:77-95`); `opencode debug lsp …`.
- Report format to the model (`opencode/src/lsp/diagnostic.ts`): only severity 1 (ERROR), ≤20 per file + `... and N more`,
  `<diagnostics file="…">\nERROR [line:col] message\n</diagnostics>` (1-based). Appended as
  `"LSP errors detected in this file, please fix:\n…"`.
- Bootstrap: `LSP.init` runs at instance bootstrap concurrently with format/vcs/snapshot/project (`opencode/src/project/bootstrap.ts:34-38`);
  servers spawn lazily on first touch.
- HTTP: `GET /lsp` status (`groups/instance.ts:54,169`).

### 2.5 Rust mapping
- Client: `lsp-types` for payloads; framing via `async-lsp` (client role) or ~200 lines of Content-Length codec. Keep the
  push/pull diagnostic wait machinery and timeouts verbatim — the model output depends on it.
- Server registry as data (`struct ServerDef { id, extensions, root: RootStrategy, spawn: fn }`); port root heuristics exactly.
- npm-based servers need `node` on PATH (`.bin` shims); the TS build has the same requirement except where BUN_BE_BUN applies.
- Downloads: `reqwest` + `zip` + `tar` + `xz2`/`flate2`; preserve `<cache>/bin` layout so existing installs are reused.

---

## 3. Formatters [src] (`opencode/src/format/`)

Enablement mirrors LSP: `formatter` absent/false ⇒ none (`index.ts:120-128`); `true` ⇒ built-ins; object ⇒ overrides
(`core/src/v1/config/formatter.ts`: `{disabled?, command?: string[], environment?: Record, extensions?: string[]}`).

Built-ins (`formatter.ts`, map key = `name`):

| name (line) | extensions | enabled when | command |
|---|---|---|---|
| gofmt (18) | .go | `gofmt` on PATH | `gofmt -w $FILE` |
| mix (28) | .ex .exs .eex .heex .leex .neex .sface | `mix` | `mix format $FILE` |
| prettier (38) | js/ts family, .html .htm .css .scss .sass .less .vue .svelte .json .jsonc .yaml .yml .toml .xml .md .mdx .graphql .gql | any `package.json` (dir→worktree) lists `prettier` in deps/devDeps | `<npm-cache prettier bin> --write $FILE`, env `BUN_BE_BUN=1` |
| oxfmt (87) | .js .jsx .mjs .cjs .ts .tsx .mts .cts | `OPENCODE_EXPERIMENTAL_OXFMT` + `oxfmt` dep | `<npm oxfmt> $FILE`, `BUN_BE_BUN=1` |
| biome (110) | same as prettier | `biome.json`/`biome.jsonc` found upward | `<npm @biomejs/biome> format --write $FILE`, `BUN_BE_BUN=1` |
| zig (156) | .zig .zon | `zig` | `zig fmt $FILE` |
| clang-format (166) | .c .cc .cpp .cxx .c++ .h .hh .hpp .hxx .h++ .ino .C .H | `.clang-format` upward + binary | `clang-format -i $FILE` |
| ktlint (179) | .kt .kts | `ktlint` | `ktlint -F $FILE` |
| ruff (189) | .py .pyi | `ruff` + (`[tool.ruff]` in pyproject.toml, or `ruff.toml`/`.ruff.toml`, or "ruff" substring in requirements.txt/pyproject.toml/Pipfile) | `ruff format $FILE` |
| air (218) | .R | `air --help` first line contains "R language" and "formatter" | `air format $FILE` |
| uv (236) | .py .pyi | ruff not enabled + `uv format --help` exits 0 | `uv format -- $FILE` |
| rubocop (249) | .rb .rake .gemspec .ru | `rubocop` | `rubocop --autocorrect $FILE` |
| standardrb (259) | same | `standardrb` | `standardrb --fix $FILE` |
| htmlbeautifier (269) | .erb .html.erb | binary | `htmlbeautifier $FILE` |
| dart (279) | .dart | `dart` | `dart format $FILE` |
| ocamlformat (289) | .ml .mli | binary + `.ocamlformat` upward | `ocamlformat -i $FILE` |
| terraform (300) | .tf .tfvars | `terraform` | `terraform fmt $FILE` |
| latexindent (310) | .tex | binary | `latexindent -w -s $FILE` |
| gleam (320) | .gleam | `gleam` | `gleam format $FILE` |
| shfmt (330) | .sh .bash | `shfmt` | `shfmt -w $FILE` |
| nixfmt (340) | .nix | `nixfmt` | `nixfmt $FILE` |
| rustfmt (350) | .rs | `rustfmt` | `rustfmt $FILE` |
| pint (360) | .php | `laravel/pint` in composer.json require(-dev) | `./vendor/bin/pint $FILE` |
| ormolu (376) | .hs | binary | `ormolu -i $FILE` |
| cljfmt (386) | .clj .cljs .cljc .edn | binary | `cljfmt fix --quiet $FILE` |
| dfmt (396) | .d | binary | `dfmt -i $FILE` |

Execution semantics (`index.ts:43-116`):
- **Every** enabled formatter whose extensions include `path.extname(file)` runs, **sequentially** (e.g. a JS project with both
  prettier dep and biome.json runs both). `$FILE` replaced in every arg; cwd = instance directory; env = formatter env merged over
  process env; stdio ignored; non-zero exit only logged. Returns `true` if any formatter matched.
- `enabled()` result cached per instance **only when it is a command**; `false` is re-evaluated each call (`:44-48`).
- Config overrides (`:134-157`): `ruff` or `uv` disabled ⇒ both removed; `disabled` ⇒ removed; else
  `mergeDeep(builtIn ?? {extensions:[]}, item)` where `builtIn = Formatter[<config key>]` — looked up by **export identifier**, not
  `name`. Keys whose export name differs (`clang-format`→`clang`, `air`→`rlang`, `uv`→`uvformat`) get no built-in detection: they
  only work with an explicit `command` (quirk, [src]). Custom entry without command ⇒ never enabled.
- Called after file writes in `write` (`opencode/src/tool/write.ts:65`), `edit` (`edit.ts:112,156`), `apply_patch`
  (`apply_patch.ts:253`); on success BOM is re-synced (`write.ts:66`). Then `file.edited` and `file.watcher.updated` events.
- HTTP `GET /formatter` status `{name, extensions, enabled}`.
- Rust mapping: data table + `tokio::process`; keep "run all matches" and the key-lookup quirk (or fix it deliberately and document).

---

## 4. Plugins

### 4.1 Two plugin systems are live [src]
1. **v1 hooks** (`@opencode-ai/plugin` root export, `plugin/src/index.ts`) — loaded by `opencode/src/plugin/index.ts` from
   v1 config key `plugin` + auto-discovered files. This is what every existing third-party plugin targets.
2. **v2 domains** (`@opencode-ai/plugin/v2/effect|promise`, `plugin/src/v2/*`) — loaded by `core/src/config/plugin/external.ts`
   from v2 config key `plugins` + the same `{plugin,plugins}/*.{ts,js}` globs; runs inside core location services
   (`core/src/location-services.ts:42-79`, used by `opencode/src/agent/agent.ts:104`, `session/system.ts`, PTY/file handlers).

A v1 file plugin is also seen by the v2 loader and vice-versa; mismatched modules fail decode/validation and are ignored/logged
(`external.ts:81,87`; `opencode/src/plugin/shared.ts:509-541`).

### 4.2 Discovery (v1) [src]

Spec forms (`core/src/v1/config/plugin.ts`): `string | [string, Record<string, unknown>]` (options passed as 2nd arg).

Sources, merged in this order (`opencode/src/config/config.ts:370-490`), later wins on dedupe:
1. remote `.well-known/opencode` configs (scope global), 2. global config dir, 3. `OPENCODE_CONFIG`, 4. project `opencode.json[c]`
   files from instance dir up to worktree (scope local), 5. for every config directory
   (`opencode/src/config/paths.ts:23-41`: `Global.Path.config`, each `.opencode` dir from instance dir up to worktree, `~/.opencode`,
   `OPENCODE_CONFIG_DIR`): its `opencode.json[c]` (only `.opencode` dirs / `OPENCODE_CONFIG_DIR`) and **auto-discovered**
   `{plugin,plugins}/*.{ts,js}` (`opencode/src/config/plugin.ts:18-30`, glob with `dot:true, symlink:true`, as `file://` URLs),
   6. `OPENCODE_CONFIG_CONTENT`.
- Path specs (`file://`, `.`-prefixed, absolute incl. `C:\`) are resolved relative to the declaring config file (`plugin.ts:42-60`).
- Dedupe identity: exact `file://` URL, else npm package name (`plugin.ts:64-77`); last declaration wins, order preserved.
- Scope: source under instance ⇒ `local`, else `global` (`config.ts:337-342`).
- `OPENCODE_PURE` ⇒ no external plugins; `OPENCODE_DISABLE_DEFAULT_PLUGINS` ⇒ no internal plugins (`opencode/src/plugin/index.ts:170,181`).
- Every config directory gets a `.gitignore` (if absent) with `node_modules package.json package-lock.json bun.lock .gitignore`
  (`config.ts:309-326`) and a **background** `Npm.install(dir, add @opencode-ai/plugin@<InstallationVersion>)` (unversioned for
  local builds) (`config.ts:452-471`) so `.opencode/plugin/*.ts` can `import "@opencode-ai/plugin"`. `Npm.install` no-ops on non-writable dirs and reifies only if `node_modules` is missing or a declared dep is absent from `package-lock.json` (`core/src/npm.ts:147-198`). Loaders call
  `config.waitForDependencies()` before importing file plugins/tools.
- Custom tool files: `{tool,tools}/*.{js,ts}` in every config dir (`opencode/src/tool/registry.ts:183-197`); each export that has
  `args`,`description`,`execute` becomes a tool named `<file>` (default export) or `<file>_<export>`.
- TUI plugins: separate `tui.json[c]` `plugin` key (`opencode/src/config/tui.ts:176-207`), `kind:"tui"` entry; run inside the
  TS TUI with `@opentui/*` (`plugin/src/tui.ts:581-634`) — out of scope for a non-JS TUI.

### 4.3 Resolution & install [src] (`opencode/src/plugin/loader.ts`, `shared.ts`, `core/src/npm.ts`)

Pipeline per candidate (parallel across plugins, `loader.ts:208-236`): `resolvePluginTarget` → `createPluginEntry` →
compatibility → `import()`.
- Deprecated packages silently skipped: `opencode-openai-codex-auth`, `opencode-copilot-auth` (substring match, `shared.ts:247-251`).
- **npm specs**: bare name ⇒ `<name>@latest` (`shared.ts:444-450`); `Npm.add(spec)` installs into
  `<cache>/packages/<sanitize(spec)>/` (`npm.ts:87`; on Windows `<>:"|?*` and control chars → `_`) using `@npmcli/arborist@9.4.0`
  `reify({add:[spec], save:true, saveType:"prod"})` with `binLinks:true, savePrefix:"", ignoreScripts:true` and npm config from
  `@npmcli/config` (respects `.npmrc`, registry default `https://registry.npmjs.org`) (`npm.ts:88-121`, `npm-config.ts`).
  Cache hit = `node_modules/<name>` exists (no update check — `@latest` is sticky until cache cleared). Lock
  `npm-install:<dir>` via flock.
- **path specs**: file ⇒ `file://` URL; directory ⇒ has `package.json` ⇒ dir, else first of
  `index.ts index.tsx index.js index.mjs index.cjs`, else error "missing package.json or index file" (retried once after deps).
- Entrypoint for `server`: `package.json.exports["./server"]` (string or `{import|default}`), else `main` (server only), else
  directory index (file plugins with `exports`), else target itself; resolved entry must stay inside the package dir
  (`shared.ts:326-406`).
- Compatibility (npm only): `package.json.engines.opencode` semver range vs `InstallationVersion`; skipped when version invalid or
  major 0 (`shared.ts:431-442`).
- Errors become `session.error` events: `Failed to install plugin <pkg>@<ver>: …`, `Plugin <spec> skipped: …`,
  `Failed to load plugin <spec>: …` (`opencode/src/plugin/index.ts:193-215`).

### 4.4 Module shape [src] (`opencode/src/plugin/index.ts:88-125`, `shared.ts:501-560`)
- **v1 module**: `export default { id?: string, server: Plugin }` (detect mode: default is an object with `id`/`server`/`tui`).
  `server` and `tui` exclusive. File plugins **must** export `id`; npm plugins default to `package.json.name`.
- **Legacy module**: every distinct export value must be a function (or `{server}`); each is called as a plugin; any other export
  throws "Plugin export is not a function" and the whole module is skipped.
- `Plugin = (input: PluginInput, options?: Record<string, unknown>) => Promise<Hooks>`.

### 4.5 Runtime context contract (`PluginInput`) [src] (`plugin/src/index.ts:56-66`, `opencode/src/plugin/index.ts:143-168`)

| Field | Value in 1.19.5 | Sidecar requirement |
|---|---|---|
| `client` | `createOpencodeClient` from **v1** `@opencode-ai/sdk` with `baseUrl = Server.url ?? "http://localhost:4096"`, `directory = instance dir`, `headers = ServerAuth.headers()`; when no server is listening, `fetch` is routed in-process to `Server.Default().app.fetch` | Rust must always expose the v1 HTTP API on a loopback port (other spec) and pass URL + credentials to the sidecar |
| ↳ SDK behaviour | adds header `x-opencode-directory: encodeURIComponent(dir)`; for GET/HEAD rewrites it into `?directory=` (`sdk/js/src/client.ts:17-56`); fetch with `timeout=false` | bundle `@opencode-ai/sdk@1.19.5` in sidecar unchanged |
| ↳ auth | `Authorization: Basic base64(user:pass)` iff `OPENCODE_SERVER_PASSWORD` set; user default `opencode` (`opencode/src/server/auth.ts:36-48`) | same env |
| `project` | v1 `Project` (id, worktree, vcs, name?, icon?, time, sandboxes, commands?) (`ctx.project`) | JSON snapshot at init |
| `directory`, `worktree` | instance dir / project worktree | strings |
| `experimental_workspace.register(type, adapter)` | registers a control-plane workspace adapter for the project (`opencode/src/control-plane/adapters/index.ts:37`) | RPC callback; adapter methods `configure/create/remove/target` invoked from Rust |
| `serverUrl` | getter, `Server.url ?? new URL("http://localhost:4096")` | URL |
| `$` | `Bun.$` (undefined under Node) (`index.ts:166-167`) | **requires Bun runtime** |

Bun version pinned by repo: `packageManager: bun@1.4.4` (`/package.json:7`). Zod in host: `4.1.8` (`/package.json:80`).

### 4.6 Hook reference [src] (`plugin/src/index.ts:222-335`)

Trigger semantics (`opencode/src/plugin/index.ts:284-297`): for each loaded `Hooks` object **in load order**
(internal plugins first, then external in config order), `await fn(input, output)`; hooks mutate `output` in place; return value
ignored; the same `output` object is returned to the caller. A rejected promise becomes an Effect **defect**
(`Effect.promise`) ⇒ the calling operation fails (e.g. tool call errors) [src].

| Hook | input | output (mutable) | Fired at |
|---|---|---|---|
| `chat.message` | `{sessionID, agent?, model?{providerID,modelID}, messageID?, variant?}` | `{message: UserMessage, parts: Part[]}` | `opencode/src/session/prompt.ts:1000` (new user message) |
| `chat.params` | `{sessionID, agent, model, provider: ProviderContext, message}` | `{temperature, topP, topK, maxOutputTokens, options}` | `session/llm/request.ts:114` |
| `chat.headers` | same | `{headers}` | `session/llm/request.ts:134` |
| `command.execute.before` | `{command, sessionID, arguments}` | `{parts}` | `session/prompt.ts:1461` |
| `tool.execute.before` | `{tool, sessionID, callID}` | `{args}` | `session/tools.ts:106,175,258,338,402`, `session/prompt.ts:308` (task), `tool/code-mode.ts:142` |
| `tool.execute.after` | `{tool, sessionID, callID, args}` | `{title, output, metadata}` (+`attachments`; raw `CallToolResult` for MCP) | `session/tools.ts:121,208,291,373,420`, `prompt.ts:390`, `code-mode.ts:181` |
| `shell.env` | `{cwd, sessionID?, callID?}` | `{env}` | shell tool `tool/shell.ts:417`, `!` commands `prompt.ts:555`, PTY create `handlers/pty.ts:71`, `plugin/pty-environment.ts:18` |
| `tool.definition` | `{toolID}` | `{description, parameters, jsonSchema}` (internal adds `jsonSchema`) | `tool/registry.ts:318` per tool per request |
| `experimental.chat.system.transform` | `{sessionID?, model}` | `{system: string[]}` | `request.ts:69`, `agent/agent.ts:381` (agent generation) |
| `experimental.chat.messages.transform` | `{}` | `{messages: {info, parts}[]}` (full history) | `prompt.ts:1255`, `compaction.ts:379` |
| `experimental.session.compacting` | `{sessionID}` | `{context: string[], prompt?}` | `compaction.ts:374` |
| `experimental.compaction.autocontinue` | `{sessionID, agent, model, provider, message, overflow}` | `{enabled}` | `compaction.ts:501` |
| `experimental.text.complete` | `{sessionID, messageID, partID}` | `{text}` | `session/processor.ts:531` (each text-end) |
| `experimental.provider.small_model` | `{provider}` | `{model?}` | `provider/provider.ts:1978` |
| `permission.ask` | `Permission` | `{status}` | **never triggered in 1.19.5** (no call site in `opencode/src`, `core/src`) |

Non-trigger members:
- `event({event:{id,type,properties}})` — every bus event whose `location.directory === instance dir`, fire-and-forget (`index.ts:255-262`).
- `config(cfg)` — called once after load with the **live config object**; mutations persist (providers read config after it,
  `provider/provider.ts:1491-1494`).
- `tool: Record<id, ToolDefinition>` — merged into registry (`registry.ts:199-204`); id used verbatim.
- `auth: {provider, loader?, methods[]}` — provider auth UI + `loader(getAuth, provider)` returns provider SDK options merged into
  provider config (`provider/provider.ts:1663-1681`); options may contain **functions** (e.g. `fetch`) (`plugin/github-copilot/copilot.ts:102`).
- `provider: {id, models(provider, {auth})}` — replaces model list (`provider/provider.ts:1505-1530`).
- `dispose()` — on instance teardown (`index.ts:265-278`).

### 4.7 Plugin tools [src] (`plugin/src/tool.ts`, `opencode/src/tool/registry.ts:124-178,351-381`)
- Definition `{description, args: ZodRawShape, execute(args, ctx) → string | {title?, output, metadata?, attachments?}}`.
- JSON Schema: if every arg is Zod v4 (`"_zod" in value`) ⇒ `z.toJSONSchema(z.object(args), {io:"input"})`, `$defs`→`definitions`;
  otherwise args treated as raw JSON-Schema properties, all required (`legacyJsonSchema`). Args validated with `safeParse`.
- Context passed: `{sessionID, messageID, agent, abort, callID, extra{model,bypassAgentCheck,promptOps}, messages, metadata, ask,
  directory, worktree}` (spread of internal `Tool.Context`, `session/tools.ts:59-70`). `ask` bridged to Promise; **`metadata` is
  not bridged** and returns an un-run Effect ⇒ calls are effectively no-ops [src, medium confidence].
- Output truncated by `truncate.output` (metadata gains `truncated`, `outputPath`).

### 4.8 Internal (built-in) v1 plugins [src] (`opencode/src/plugin/index.ts:67-86`)
`CodexAuthPlugin` (with `experimentalWebSockets` true on channels local/dev/beta or `OPENCODE_EXPERIMENTAL_WEBSOCKETS`),
`CopilotAuthPlugin`, `ModalPlugin`, `GitlabAuthPlugin` (npm `opencode-gitlab-auth`), `PoeAuthPlugin` (npm `opencode-poe-auth`),
`CloudflareWorkersAuthPlugin`, `CloudflareAIGatewayAuthPlugin`, `AzureAuthPlugin`, `DigitalOceanAuthPlugin`,
`SnowflakeCortexAuthPlugin`, `XaiAuthPlugin`, `CerebrasPlugin`. Hooks used (grep, partial): codex & copilot `chat.headers`,
`chat.params`, `auth`, `provider`; copilot `experimental.provider.small_model`; cerebras `chat.params`; others `auth`/`provider`.
Porting: implement natively in Rust (provider spec 03) **or** run the two third-party npm ones in the sidecar.

### 4.9 v2 plugin API (core) [src]
- Module: `export default { id, effect(ctx) }` (Effect) or `{ id, setup(ctx) }` (Promise, adapted by `core/src/plugin/promise.ts`)
  (`core/src/config/plugin/external.ts:15-30`).
- Sources: v2 config `plugins: (string | {package, options})[]` (relative `./`/`../` resolved to config dir) and
  `{plugin,plugins}/*.{ts,js}` in directory entries (sorted); npm via `Npm.add(...).entrypoint` (`external.ts:39-89`).
- Context (`plugin/src/v2/promise/context.ts`): `options`, `agent|catalog|command|integration|reference|skill` each with
  `transform(cb(draft))` + `reload()`, `aisdk.sdk(cb)`/`aisdk.language(cb)` runtime hooks, `integration.connection.{active,resolve}`,
  `plugin.{add,remove}`. Drafts are **synchronous mutable editors** (`plugin/src/v2/effect/catalog.ts:9-25`,
  `integration.ts:46-56`); `aisdk` hooks set live JS objects (`event.sdk`, `event.language: LanguageModelV3`).
- Internal v2 plugin boot order (`core/src/plugin/internal.ts:108-122`): config-reference, agent, command, skill, models-dev,
  config-agent, config-command, config-skill, provider plugins (`core/src/plugin/provider/*`), config-plugin (external),
  config-provider, variant.

### 4.10 Porting v2 plugins
`transform` callbacks can be run in a sidecar by sending a JSON snapshot of the domain, running the callback against a JS draft
implementation, and returning a patch (list/get/update/remove ops are serialisable). `aisdk.sdk/language` hooks return live AI-SDK
objects and **cannot** cross a process boundary — only usable if the LLM call itself is proxied through the sidecar (R2).

### 4.11 Rust mapping — Bun sidecar plugin host (design, [assumption] throughout)

Process: Rust spawns `bun <bundled plugin-host.js>` per instance (directory) on first need; stdio newline-delimited JSON-RPC 2.0
(stderr → log). Sidecar bundles `@opencode-ai/sdk@1.19.5`, `@opencode-ai/plugin@1.19.5`, `zod@4.1.8`, `@npmcli/arborist@9.4.0`,
`@npmcli/config@10.8.1`, `npm-package-arg@13.0.2`, `semver`, and a copy of `shared.ts`/`loader.ts` logic so resolution stays identical.
Use Bun so `.ts/.tsx` plugins import without a build step and `$ = Bun.$` exists.

Host→sidecar:
| Method | Params | Result |
|---|---|---|
| `init` | `{directory, worktree, project, serverUrl, authHeader?, plugins: Origin[], disableInternal, pure}` | `{plugins:[{id, spec, hooks:string[], tools:{[id]:{description, jsonSchema}}, auth?:{provider, methods:[{type,label,prompts}]}, provider?:{id}}], errors:[{stage,spec,message}]}` |
| `trigger` | `{name, input, output}` | `{output}` (sidecar iterates all plugins sequentially; host runs Rust-native hooks before forwarding to keep "internal first" order) |
| `event` | `{event:{id,type,properties}}` (notification) | — |
| `config` | `{config}` | `{config}` (diff-merge into live config) |
| `tool.execute` | `{pluginIndex, toolID, args, ctx:{sessionID,messageID,agent,callID,directory,worktree}}` | `{title, output, metadata, attachments?}` |
| `tool.abort` | `{callID}` | — (sidecar aborts `AbortController`) |
| `auth.loader` | `{provider, providerInfo}` | `{options, functions:["fetch",…]}` — function-valued keys replaced by handles |
| `auth.authorize` / `auth.callback` | method index, inputs / code | `AuthOAuthResult` minus functions (callback handle) |
| `provider.models` | `{provider, auth}` | `Record<string, ModelV2>` |
| `fetch.invoke` | `{handle, url, init}` | streamed response (status, headers, body chunks) — needed for loader `fetch` overrides |
| `workspace.*` | adapter calls | per `WorkspaceAdapter` |
| `dispose` | — | — |

Sidecar→host: `tool.ask {callID, permission, patterns, always, metadata}` → resolves/rejects; `tool.metadata` (implement properly);
`log`. Payloads must be JSON; drop `extra.promptOps`; send `messages` only if a plugin's tool reads it (lazy RPC `session.messages`).
Performance: `experimental.chat.messages.transform` ships whole histories per LLM call — skip the RPC entirely when no loaded plugin
declares that hook (sidecar reports hook names at `init`).

---

## 5. Snapshot / revert [src] (`opencode/src/snapshot/index.ts`)

### 5.1 Storage
- Separate bare-ish git dir: `<data>/snapshot/<project.id>/<sha1(worktree)>` (`index.ts:71`; `Hash.fast` = SHA-1 hex,
  `core/src/util/hash.ts`). Work tree = project worktree. v2 core uses the **same path** (`core/src/snapshot.ts:98`).
- Enabled iff `project.vcs === "git"` and config `snapshot !== false` (`index.ts:167-170`).
- Project ID (`core/src/project.ts:65-121`): `sha1("git-remote:" + host.lower + "/" + path-without-.git)` from `origin`
  (http(s) or scp syntax, `file:` ignored) → else cached ID in `<git-common-dir>/opencode` file → else first root commit hash →
  else `global`. Old IDs are migrated in DB (`opencode/src/project/project.ts:144-190`). Port exactly or snapshot dirs orphan.
- Git flag sets (`index.ts:25-27`): `core=-c core.longpaths=true -c core.symlinks=true`, `cfg = -c core.autocrlf=false + core`,
  `quote = cfg + -c core.quotepath=false`. All commands use `--git-dir <gitdir> --work-tree <worktree>`.
- One semaphore per gitdir serialises all operations (`index.ts:56-63`).

### 5.2 Operations
| Op | Git commands |
|---|---|
| init (first `track`) | `git init` (env `GIT_DIR`,`GIT_WORK_TREE`); `config core.autocrlf false`, `core.longpaths true`, `core.symlinks true`, `core.fsmonitor false`, `feature.manyFiles true`, `index.version 4`, `index.threads true`, `core.untrackedCache true`; then `seed` (`:318-342`) |
| seed | `rev-parse --path-format=absolute --git-common-dir` (source repo); write `<gitdir>/objects/info/alternates` = source `objects` + its existing alternates; copy source `index` → `<gitdir>/index` (`:198-233`) |
| sync excludes | `rev-parse --path-format=absolute --git-path info/exclude` of source; write `<gitdir>/info/exclude` = source exclude + `/<large-untracked>` lines (`:172-196`) |
| add (before track/patch/diff) | parallel `diff-files --name-only -z -- .` and `ls-files --full-name --others --exclude-standard -z -- .` (cwd instance dir); candidates → `check-ignore --no-index --stdin -z` **against the source repo** (`--git-dir <worktree>/.git`, leading `:` protected as `./:`); ignored ⇒ `rm --cached -f --ignore-unmatch --pathspec-from-file=- --pathspec-file-nul`; untracked files >2 MiB are excluded via info/exclude; rest `add --all --sparse --pathspec-from-file=- --pathspec-file-nul` with `:(top,literal)<path>` NUL list (`:102-298`) |
| track | add + `write-tree` ⇒ tree hash (`:318-347`) |
| patch(hash) | add + `diff --cached --no-ext-diff --name-only <hash> -- .`; drop ignored; return absolute paths with `/` (`:349-380`) |
| diff(hash) | add + `diff --cached --no-ext-diff <hash> -- .` (`:526-544`) |
| diffFull(from,to) | `diff --no-ext-diff --name-status --no-renames`, `diff --numstat`, contents via `cat-file --batch` (fallback per-file `show <rev>:<file>`); structured patch via `diff` npm (`:546-760`) |
| restore(tree) | `read-tree <tree>` + `checkout-index -a -f` (`:382-406`) |
| revert(patches) | per file (dedup, first patch wins): `checkout <hash> -- <file>`; on failure `ls-tree <hash> -- <rel>` ⇒ exists: keep, else delete file. Adjacent files of same hash without path overlap batched ≤100: `ls-tree --name-only` then one `checkout` (`:408-524`) |
| cleanup | `gc --prune=7.days`, loop every 1 h after 1 min delay (`:300-316,761-765`) |

### 5.3 Session integration [src]
- `track()` at loop start (`session/processor.ts:102`), at `step-start` if none (`:425`), at `step-finish` (`:436`);
  `patch(ctx.snapshot)` at step-finish / end produces `patch` parts `{hash, files}` (`:472,555`).
- `revert({sessionID, messageID, partID?})` (`opencode/src/session/revert.ts:38-89`): collect `patch` parts after the target;
  `rev.snapshot = existing revert snapshot ?? track()`; if re-reverting, `restore(old snapshot)` first; `snap.revert(patches)`;
  `rev.diff = diff(rev.snapshot)`; recompute session diff (stored `["session_diff", id]`, event `session.diff`); `setRevert`.
- `unrevert`: `restore(revert.snapshot)` + clear (`:91-100`). `cleanup` (on next prompt) deletes reverted messages/parts (`:102-124`).
- Rust mapping: shell out to `git` with identical argv (alternates, index v4, pathspec-from-file, `check-ignore --no-index`
  are poorly covered by `gix`/`git2`). Keep the SHA-1 dir naming.

---

## 6. PTY [src]

- Backend selection by package `imports["#pty"]` (`core/package.json:31-35`): Bun ⇒ `bun-pty@0.4.8` (`core/src/pty/pty.bun.ts`),
  Node ⇒ `@lydell/node-pty@1.2.0-beta.12` with `useConptyDll:true` on Windows (`pty.node.ts`).
- `create(input)` (`core/src/pty.ts:165-245`): id `pty_<ascending>`; command = input or `Shell.preferred(config shell)`
  (`core/src/shell.ts:205-210`: config → `$SHELL` → Windows `[pwsh, powershell, git-bash, %COMSPEC%|cmd.exe][0]` / macOS `/bin/zsh`
  / `bash` / `/bin/sh`; `OPENCODE_GIT_BASH_PATH` overrides git-bash); appends `-l` for login shells (bash, dash, fish, ksh, sh, zsh);
  env `{...process.env, ...input.env, TERM:"xterm-256color", OPENCODE_TERMINAL:"1"}` + Windows `LC_ALL/LC_CTYPE/LANG=C.UTF-8`;
  title default `Terminal <last4>`.
- Output ring buffer 2 MiB (`:14`), cursor = **JS string length (UTF-16 code units)** cumulative (`:205`). Exited sessions retained,
  max 25 (`:17`). Events `pty.created|updated|exited|deleted` (`schema/src/pty.ts:34-37`).
- HTTP (`opencode/src/server/routes/instance/httpapi/groups/pty.ts:17-37`): `GET /pty/shells`, `GET /pty` (running only),
  `POST /pty` (merges `shell.env` hook env), `GET|PUT|DELETE /pty/:ptyID`, `POST /pty/:ptyID/connect-token` (requires header
  `x-opencode-ticket: 1` + allowed Origin; returns `{ticket: uuid, expires_in: 60}`, TTL 60 s, cap 10 000,
  `core/src/pty/ticket.ts:9-10`), WebSocket `GET /pty/:ptyID/connect?ticket=&cursor=` (`opencode/src/server/shared/pty-ticket.ts`).
- WS protocol (`core/src/pty/protocol.ts`): server sends replay (from cursor; `-1` = tail, absent = whole buffer) in 64 KiB string
  chunks, then a binary control frame `0x00 + JSON {"cursor": n}`, then live UTF-8 chunks; close 1000 on exit, 4404 "session not found".
  Client frames: text or binary UTF-8 (invalid UTF-8 dropped) written to the PTY.
- Rust mapping: `portable-pty` (ConPTY on Windows). **Cursor must be counted in UTF-16 units** to stay compatible with existing
  web/desktop clients that resume by cursor (R5).

---

## 7. File watcher [src] (`core/src/filesystem/watcher.ts`)

- Library `@parcel/watcher@2.5.1` via platform binding `@parcel/watcher-<platform>-<arch>[-glibc|musl]`; backends
  `windows`, `fs-events`, `inotify`; other platforms unsupported (`:26-45`). Subscribe timeout 10 s.
- Disabled entirely by `OPENCODE_EXPERIMENTAL_DISABLE_FILEWATCHER`.
- Worktree subscription only when `location.vcs` and `OPENCODE_EXPERIMENTAL_FILEWATCHER` (`:110-114`) with ignore =
  `Ignore.PATTERNS` + config `watcher.ignore` + protected paths.
- Git dir subscription always for git repos: the resolved git dir (realpath), ignoring every top-level entry **except `HEAD`**
  (`:115-124`) — used by VCS branch tracking (`opencode/src/project/vcs.ts:319-330` → `vcs.branch.updated`).
- Ignore patterns (`core/src/filesystem/ignore.ts`): folders `node_modules bower_components .pnpm-store vendor .npm dist build out
  .next target bin obj .git .svn .hg .vscode .idea .turbo .output desktop .sst .cache .webkit-cache __pycache__ .pytest_cache
  mypy_cache .history .gradle`; files `**/*.swp **/*.swo **/*.pyc **/.DS_Store **/Thumbs.db **/logs/** **/tmp/** **/temp/** **/*.log
  **/coverage/** **/.nyc_output/**`. Protected (never watched): macOS `~/Music … ~/Library/{Mail,Messages,…}`, `/.Spotlight-V100` …;
  Windows `~/{AppData,Downloads,Desktop,Documents,Pictures,Music,Videos,OneDrive}` (`core/src/filesystem/protected.ts`).
- Event `file.watcher.updated {file, event: add|change|unlink}` (`schema/src/filesystem-watcher.ts:6-12`); also emitted synthetically
  by write/edit/apply_patch tools.
- Rust mapping: `notify` (RecommendedWatcher uses the same OS APIs) + `globset` for patterns; replicate the HEAD-only `.git` watch.

---

## 8. Git, VCS, worktrees [src]
- v1 git helper (`opencode/src/git/index.ts:6-18`) always prefixes `--no-optional-locks -c core.autocrlf=false -c core.fsmonitor=false
  -c core.longpaths=true -c core.symlinks=true -c core.quotepath=false`; API `branch, prefix, defaultBranch, hasHead, mergeBase,
  show, status, diff, stats, patch*, applyPatch` (`:75-90`).
- v2 `core/src/git.ts` (`GitV2`): repo discover/clone (`clone --depth 100` default, `:261-270`)/create, remote, history
  (`rootCommits` for project ID), sync, change capture/apply, worktree, index, tree ops (used by v2 snapshot).
- Worktrees (`opencode/src/worktree/index.ts`): dir `<data>/worktree/<projectID>/<slug>`, branch `opencode/<name>` unless detached,
  `git worktree add --no-checkout [-b <branch>|--detach <dir> HEAD]` then `reset --hard`; remove = `worktree remove --force` +
  `branch -D`; reset uses `clean -ffdx` (`:183-596`). Exposed to plugins as built-in workspace adapter `worktree`
  (`opencode/src/control-plane/adapters/index.ts:6`), workspaces gated by `OPENCODE_EXPERIMENTAL_WORKSPACES`.

---

## 9. ACP (Agent Client Protocol) [src]

- `opencode acp [--cwd] [network opts]` (`opencode/src/cli/cmd/acp.ts:9-73`): sets `OPENCODE_CLIENT=acp`, starts the HTTP server,
  creates a **v2 SDK client** to `http://<host>:<port>` with `ServerAuth.headers()`, and runs `AgentSideConnection` over
  `ndJsonStream(stdout, stdin)` from `@agentclientprotocol/sdk@0.21.0` (`opencode/package.json:57`). ACP is a pure client of the
  HTTP API + SSE (`sdk.global.event`, `acp/event.ts:153`).
- Agent methods (`acp/agent.ts:32-86`): `initialize, authenticate, newSession, loadSession, listSessions, resumeSession, closeSession,
  unstable_forkSession, setSessionConfigOption, setSessionMode, unstable_setSessionModel, prompt, cancel`.
- `initialize` response (`acp/service.ts:111-135`): `protocolVersion: 1`, capabilities `loadSession:true`,
  `mcpCapabilities{http:true, sse:true}`, `promptCapabilities{embeddedContext:true, image:true}`,
  `sessionCapabilities{close,fork,list,resume}`, one auth method "Login with opencode" (with `_meta["terminal-auth"] =
  {command:"opencode", args:["auth","login"], label:"OpenCode Login"}` when client advertises it), `agentInfo{name:"OpenCode", version}`.
- Client-supplied `mcpServers` are registered through `sdk.mcp.add({directory, name, config})`, deduped per session
  (`acp/service.ts:1010-1050`).
- Event mapping (`acp/event.ts:95-103,235-385`): `message.part.delta/updated` → `agent_message_chunk` / `agent_thought_chunk`,
  tool parts → `tool_call` / `tool_call_update`; `permission.asked` → `requestPermission` with options `once/allow_once`,
  `always/allow_always`, `reject/reject_once` (`acp/permission.ts:21-23`), reply via `sdk.permission.reply`; for edit permissions
  with `filepath`+`diff` metadata the proposed content is pushed with `writeTextFile` (`acp/permission.ts:99-115`).
  Config options ids `model`, `effort`, `mode` (`acp/config-option.ts:38,59,80`).
- Rust mapping: `agent-client-protocol` crate (Zed) — verify it supports protocol v1 incl. `session/list|resume|close|fork` and
  unstable model selection at the pinned version (§20). Keep the "ACP is an HTTP-API client" architecture so behaviour matches.

---

## 10. IDE integration [src]
`opencode/src/ide/index.ts`: detects `TERM_PROGRAM=vscode` + `GIT_ASKPASS` containing one of Windsurf / Visual Studio Code -
Insiders / Visual Studio Code / Cursor / VSCodium; installs extension `sst-dev.opencode` via `<cmd> --install-extension`
(`windsurf|code-insiders|code|cursor|codium`); `alreadyInstalled` ⇔ `OPENCODE_CALLER ∈ {vscode, vscode-insiders}`. Event
`ide.installed` (`schema/src/ide-event.ts:7`). **No production caller** in 1.19.5 (only `opencode/test/ide/ide.test.ts`) — low priority.

---

## 11. GitHub agent [src]

- Composite action `/github/action.yml`: inputs `model` (req), `agent`, `share` (ignored), `prompt`, `use_github_token`,
  `mentions`, `variant`, `oidc_base_url`; resolves latest tag via `https://api.github.com/repos/anomalyco/opencode/releases/latest`,
  caches `~/.opencode/bin`, installs with `curl -fsSL https://opencode.ai/install | bash`, runs `opencode github run` with env
  `MODEL AGENT PROMPT USE_GITHUB_TOKEN MENTIONS VARIANT OIDC_BASE_URL`.
- `opencode github install|run [--event] [--token]` (`opencode/src/cli/cmd/github.ts`), implementation
  `github.handler.ts` (1575 lines) using `@actions/core@1.11.1`, `@actions/github@6.0.1`, `@octokit/rest`, `@octokit/graphql@9.0.2`
  (`opencode/package.json:55-56,84-85`). Session runs **in-process** via `SessionPrompt.Service` (`:381-382`).
- Events: user `issue_comment, pull_request_review_comment, issues, pull_request`; repo `schedule, workflow_dispatch` (`:149-151`).
  Mentions default `/opencode,/oc` (`:726`). Bot `opencode-agent[bot]`, reaction `eyes`, workflow file `.github/workflows/opencode.yml` (`:142-144`).
- Token: `USE_GITHUB_TOKEN=true` ⇒ `GITHUB_TOKEN`; else OIDC `core.getIDToken("opencode-github-action")` (`:965`) exchanged at
  `<OIDC_BASE_URL|https://api.opencode.ai>/exchange_github_app_token` (Bearer OIDC) or `/exchange_github_app_token_with_pat`
  (Bearer PAT, body `{owner, repo}`) → `{token}` (`:975-999`); revoked via `DELETE https://api.github.com/installation/token` (`:1565`).
  Install flow checks `https://api.opencode.ai/get_github_app_installation?owner=&repo=` and points to
  `https://github.com/apps/opencode-agent` (`:288,324`). Git auth via `http.https://github.com/.extraheader` config (`:1006,1024`).
- Server side of those endpoints lives in `function/src/api.ts` (Cloudflare Worker, Hono): `/share_*`, `/feishu`,
  `/exchange_github_app_token[_with_pat]`, `/get_github_app_installation` (`:118-388`) — not part of the client port.
- Rust mapping: `octocrab` (REST+GraphQL) + reqwest; `@actions/core` replacements: read `GITHUB_EVENT_PATH`/`GITHUB_*` env, OIDC via
  `ACTIONS_ID_TOKEN_REQUEST_URL` + `ACTIONS_ID_TOKEN_REQUEST_TOKEN` [assumption: standard Actions OIDC protocol, not in repo].

---

## 12. Share [src]
`opencode/src/share/session.ts:19-27`: "Session sharing was discontinued" — `share()` always fails, `unshare()` clears the local
link. Config keys `share`, `autoshare` still parsed (`core/src/v1/config/config.ts:57-62`); `autoshare:true` ⇒ `share:"auto"`
(`opencode/src/config/config.ts:589-591`). Rust: keep endpoints returning the same error.

---

## 13. Installation & upgrade [src] (`opencode/src/installation/index.ts`, `core/src/installation/version.ts`)

- Build constants `OPENCODE_VERSION`, `OPENCODE_CHANNEL` (fallback `"local"`); `InstallationLocal = channel === "local"`.
- Method detection (`:259-291`): execPath contains `.opencode/bin` or `.local/bin` ⇒ `curl`; else run (ordered so names found in
  execPath go first) `npm list -g --depth=0`, `yarn global list`, `pnpm list -g --depth=0`, `bun pm ls -g`,
  `brew list --formula opencode`, `scoop list opencode`, `choco list --limit-output opencode`; output contains `opencode-ai`
  (npm family) or `opencode` (brew/scoop/choco) ⇒ method; else `unknown`.
- Latest version (`:222-230,295-305`, 10 s timeout): `GET <npm registry>/opencode-ai/<NPM_TAG>` (`NPM_TAG = channel` or `latest`
  for local, `:57`), `isUpdate` only within the same major (`:49-53`); brew core formula (no `/`) ⇒ no update; choco requires
  `https://community.chocolatey.org/api/v2/Packages?$filter=Id eq 'opencode' and Version eq '<v>'&$select=Version`.
- Upgrade commands (`:306-376`): curl ⇒ `GET https://opencode.ai/install` piped to `bash` (or `sh`) stdin with `VERSION=<target>`;
  `npm|pnpm|bun install -g opencode-ai@<v>`, `yarn global add opencode-ai@<v>`, brew `tap anomalyco/tap` + `upgrade <formula>`
  (formula `anomalyco/tap/opencode` or `opencode`), `choco upgrade opencode --version=<v> -y`, `scoop install opencode@<v>`.
  Targets with major ≥2 refused (OpenCode 2 = `@opencode/cli`, docs `https://opencode.ai/v2/docs`, `:101`). `--major` path:
  curl `https://opencode.ai/v2/install`, brew `anomalyco/tap/opencode-v2`, npm family remove `opencode-ai` + add `@opencode/cli` (`MAJOR_INSTALL`, `:78-99,188-220`).
- Notify-only (`opencode/src/cli/upgrade.ts`): disabled by global config `autoupdate:false` or `OPENCODE_DISABLE_AUTOUPDATE`;
  `OPENCODE_ALWAYS_NOTIFY_UPDATE` forces; called from TUI worker `checkUpgrade` (`opencode/src/cli/tui/worker.ts:59-61`).
- Rust mapping: `reqwest` + `semver`; when the Rust binary is distributed, decide whether it keeps the `opencode-ai` npm name
  (install-method detection depends on it).

---

## 14. Observability / telemetry [src]

- Always: structured logfmt-like file log `<data>/log/opencode.log` (append), keys `timestamp level run message … spans annotations`,
  `run` = random 8-char id (`core/src/observability/logging.ts`, `shared.ts`). Level `OPENCODE_LOG_LEVEL` (DEBUG/INFO/WARN/ERROR,
  default INFO); `OPENCODE_PRINT_LOGS=1` also writes stderr.
- OTLP only if `OTEL_EXPORTER_OTLP_ENDPOINT` set (`core/src/observability/otlp.ts:7-79`): logs → `<endpoint>/v1/logs`
  (Effect `OtlpLogger`, JSON serialization), traces → `<endpoint>/v1/traces` (`@opentelemetry/exporter-trace-otlp-http@0.214.0`,
  `BatchSpanProcessor`). Headers from `OTEL_EXPORTER_OTLP_HEADERS` (`k=v,k=v`). Resource: `service.name=opencode`,
  `service.version`, `deployment.environment.name=<channel>`, `opencode.client=<OPENCODE_CLIENT>`, `opencode.run`,
  `service.instance.id`, plus `OTEL_RESOURCE_ATTRIBUTES` (URL-decoded).
- AI-SDK telemetry spans only if config `experimental.openTelemetry` (`opencode/src/session/llm.ts:207-210,344-346`).
- **No phone-home analytics** found in `opencode/src` or `core/src` (grep for posthog/analytics/telemetry endpoints returned none
  besides the above) [src, high confidence]. Network calls that do happen: models.dev (spec 03), npm registry (plugins, update check),
  GitHub release APIs (LSP/ripgrep), `opencode.ai/install` on upgrade only.
- Rust mapping: `tracing` + `tracing-appender` for the file log (keep format if log scraping matters), `opentelemetry-otlp`
  with HTTP/JSON for parity.

---

## 15. CodeMode [src] (`codemode/`, `opencode/src/tool/code-mode.ts`)

- `@opencode-ai/codemode` (private): "Effect-native confined code execution over schema-described tools" — a **custom JavaScript
  interpreter** (acorn 8.15.0 parser + `src/interpreter/runtime.ts`, 3465 lines) with its own stdlib (`src/stdlib/*`), no ambient
  fs/process/network; tools are Effect functions; OpenAPI→tools adapter; budgeted catalog (default 2000 est. tokens) in instructions.
- In opencode: enabled by `OPENCODE_EXPERIMENTAL_CODE_MODE` (or `OPENCODE_EXPERIMENTAL`) (`opencode/src/effect/runtime-flags.ts:11-14,48`);
  replaces per-tool MCP exposure with tool `execute` (`code-mode.ts:12`) param `{code}`; MCP tools grouped by server
  (`tools.<server>.<local>`), each call goes through `tool.execute.before/after` hooks and permission key = MCP tool name
  (`code-mode.ts:120-185`).
- Rust mapping: experimental ⇒ defer; options: run the TS package in the sidecar (cheapest exact parity) or re-host semantics on
  `rquickjs` with a capability-free global object (behavioural drift).

---

## 16. Binary downloads (ripgrep etc.) [src]

- ripgrep (`core/src/ripgrep/binary.ts`): system `rg[.exe]` via `which` (PATH + `<cache>/bin`) → `<cache>/bin/rg[.exe]` → download
  `https://github.com/BurntSushi/ripgrep/releases/download/15.1.0/ripgrep-15.1.0-<target>.<ext>`; targets: `aarch64-apple-darwin`,
  `aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`, `x86_64-unknown-linux-musl` (tar.gz); `aarch64-pc-windows-msvc`,
  `i686-pc-windows-msvc`, `x86_64-pc-windows-msvc` (zip). Extract with PowerShell `Expand-Archive` / `tar -xzf` into a temp dir under
  `<cache>/bin`, copy `ripgrep-15.1.0-<target>/rg[.exe]`, chmod 0755.
- fzf: not used. Fuzzy file search: `@ff-labs/fff-bun@0.10.5` native module (tools spec).
- npm-hosted tool binaries (LSP servers, prettier, biome, oxfmt): `Npm.which(pkg, bin?)` ⇒ `<cache>/packages/<pkg>/node_modules/.bin/<bin>`;
  if missing deletes `package-lock.json` and runs `Npm.add` (`core/src/npm.ts:200-249`).
- Rust mapping: `reqwest` + `zip` + `flate2`/`tar`; or embed `grep`/`ignore` crates (ripgrep's own libraries) and drop the download.

---

## 17. Other core modules in scope [src]

### 17.1 Locks (`core/src/util/flock.ts`)
Directory locks `<state>/locks/<sha1(key)>.lock/` with `meta.json` + `heartbeat`, stale after 60 s, wait timeout 5 min, backoff
100 ms→2 s (`:21-31,127-171,321`). Keys used here: `mcp-auth:<path>`, `npm-install:<dir>`. **If Rust and TS builds may run
concurrently against the same data dirs, implement the same lock protocol.**

### 17.2 Image normalization (`core/src/image.ts`, `image/photon.ts`)
Config `attachments.image.{auto_resize=true, max_width=2000, max_height=2000, max_base64_bytes=5 MiB}`; Photon WASM
(`@silvia-odwyer/photon-node@0.3.4`) Lanczos3 resize, candidate sizes shrinking ×0.75 (≤32), encodings PNG then JPEG q
80/85/70/55/40, first fitting wins. Rust: `image` crate (`fast_image_resize` optional).

### 17.3 References / repository cache (`core/src/reference.ts`, `repository-cache.ts`, `repository.ts`, `config/reference.ts`)
v2 config `references: Record<name, string | {repository, branch?, description?, hidden?} | {path, description?, hidden?}>`; remote
repos cloned (`--depth 100`) to `<data>/repos/<host segments>/<path segments>[@<urlencoded branch>]` (`repository.ts:126-129`),
refreshed by fetch + hard reset under flock. Gated by `OPENCODE_EXPERIMENTAL_REFERENCES` [assumption: flag name from
`runtime-flags.ts:42`, consumer not traced].

### 17.4 Integration (`core/src/integration.ts`)
v2 credential/connection registry (oauth/key/env methods, attempts) backing v2 plugin `integration` domain; overlaps provider auth
(spec 03). Not needed for v1-compatible behaviour beyond what spec 03 covers.

### 17.5 Control plane (`core/src/control-plane/*`, `opencode/src/control-plane/*`)
Workspaces (`workspace.sql.ts`), session move between workspaces (`move-session.ts`), adapters (`worktree` built-in + plugin
registered). Experimental (`OPENCODE_EXPERIMENTAL_WORKSPACES`). Workspace env forwards `OTEL_*` vars (`opencode/src/control-plane/workspace.ts:533-535`).

---

## 18. Environment flags touched by these subsystems [src]

| Flag | Effect | Ref |
|---|---|---|
| `OPENCODE_PURE` | no external plugins | `runtime-flags.ts:18` |
| `OPENCODE_DISABLE_DEFAULT_PLUGINS` | no internal plugins | `:19` |
| `OPENCODE_DISABLE_LSP_DOWNLOAD` | gate LSP auto-install | `:22` |
| `OPENCODE_EXPERIMENTAL` | default for every `enabledByExperimental` flag | `:10-14` |
| `OPENCODE_EXPERIMENTAL_LSP_TY` | ty instead of pyright | `:44` |
| `OPENCODE_EXPERIMENTAL_LSP_TOOL` | `lsp` tool | `:45` |
| `OPENCODE_EXPERIMENTAL_OXFMT` | oxfmt formatter | `:46` |
| `OPENCODE_EXPERIMENTAL_CODE_MODE` | CodeMode `execute` tool | `:48` |
| `OPENCODE_EXPERIMENTAL_WORKSPACES` | control plane | `:50` |
| `OPENCODE_EXPERIMENTAL_WEBSOCKETS` | Codex plugin websockets | `:55` |
| `OPENCODE_EXPERIMENTAL_FILEWATCHER` / `_DISABLE_FILEWATCHER` | watcher | `core/src/flag/flag.ts:37-40` |
| `OPENCODE_DISABLE_AUTOUPDATE`, `OPENCODE_ALWAYS_NOTIFY_UPDATE` | update notice | `flag.ts:23`, `cli/upgrade.ts:30` |
| `OPENCODE_CONFIG`, `OPENCODE_CONFIG_DIR`, `OPENCODE_CONFIG_CONTENT`, `OPENCODE_DISABLE_PROJECT_CONFIG`, `OPENCODE_TUI_CONFIG` | plugin/config discovery | `opencode/src/config/config.ts:415-488`, `paths.ts` |
| `OPENCODE_SERVER_PASSWORD`, `OPENCODE_SERVER_USERNAME` | Basic auth for plugin/ACP SDK clients | `server/auth.ts:18-48` |
| `OPENCODE_GIT_BASH_PATH` | Windows bash for PTY/shell | `core/src/shell.ts:125` |
| `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_RESOURCE_ATTRIBUTES` | OTLP | `otlp.ts` |
| `OPENCODE_LOG_LEVEL`, `OPENCODE_PRINT_LOGS` | logging | `logging.ts:57,68` |
| `OPENCODE_CLIENT` | resource attr; `acp` set by ACP | `cli/cmd/acp.ts:23` |
| `BUN_BE_BUN=1` | set for MCP cmd `opencode` and prettier/biome/oxfmt | `mcp/index.ts:354`, `format/formatter.ts:40,89,112` |

---

## 19. Porting risks (ranked)

1. **R1 JS/TS plugin compatibility** — plugins import `@opencode-ai/plugin`, use `Bun.$`, call the v1 HTTP API through the SDK,
   mutate hook outputs in place, mutate the live config in `config()`, return Zod tool schemas, and may return function-valued
   provider options (`fetch`) from `auth.loader`. Requires a Bun sidecar + always-on loopback HTTP API + function-handle proxying
   for `fetch`; a throwing hook currently fails the operation (keep or deliberately soften).
2. **R2 v2 plugins & AI-SDK hooks** — `aisdk.sdk/language` hooks exchange live AI-SDK objects; impossible across processes unless
   LLM calls for such providers are delegated to the sidecar. Draft transforms need a snapshot/patch protocol.
3. **R3 Snapshot exactness** — depends on project-ID derivation, SHA-1 dir naming, git alternates/index seeding, `check-ignore
   --no-index` against the user repo, and 2 MiB untracked cutoff; any drift orphans or corrupts revert history.
4. **R4 MCP feature parity** — StreamableHTTP→SSE fallback, OAuth DCR/PKCE/metadata discovery done by the TS SDK, tolerant
   `tools/list`, progress-reset timeouts, `mcp-auth.json` format + flock, callback port 19876 behaviour when busy.
5. **R5 PTY cursor units** — cursors are UTF-16 code-unit offsets; Rust byte offsets break resume for existing clients.
6. **R6 Self-invocation as Bun** — MCP `command: ["opencode", …]` and npm-bin formatters rely on `BUN_BE_BUN=1` turning the opencode
   binary into Bun; the Rust binary cannot. Map to the bundled Bun.
7. **R7 LSP/formatter opt-in defaults & quirks** — both disabled unless config key set; keep quirks (Dockerfile match, formatter
   key-name lookup, run-all-matching formatters, full-text didChange) or document changes; 15+ download recipes with
   platform-specific asset names.
8. **R8 npm install layout** — Arborist semantics (`ignoreScripts`, exact save, `.npmrc`) and `<cache>/packages/<spec>` layout must
   be preserved so existing caches/`.opencode/node_modules` keep working; easiest by running Arborist inside the sidecar.
9. **R9 Lock interop** — concurrent TS/Rust processes on shared `mcp-auth.json`/npm dirs need the flock protocol.
10. **R10 CodeMode** — 3.5k-line bespoke interpreter; parity only via sidecar.

---

## 20. Required additional data (UNKNOWN)

| # | Item | How to determine |
|---|---|---|
| 1 | MCP SDK default request timeout when `timeout` undefined (tools/call etc.) | `bun install` in a scratch copy, then `grep -rn "DEFAULT_REQUEST_TIMEOUT" node_modules/@modelcontextprotocol/sdk/dist/esm/shared/protocol.js` (v1.29.0) |
| 2 | MCP OAuth discovery order (RFC 9728 `/.well-known/oauth-protected-resource`, RFC 8414 / OIDC fallbacks, `WWW-Authenticate resource_metadata`) | read `node_modules/@modelcontextprotocol/sdk/dist/esm/client/auth.js` (`auth()`, `discoverOAuthMetadata*`) |
| 3 | `xdg-basedir@5.1.0` defaults on Windows (assumed `~/.local/share`, `~/.cache`, `~/.config`, `~/.local/state`) | `cat node_modules/xdg-basedir/index.js`; cross-check provider spec's observed `C:\Users\<u>\.cache\opencode\models.json` |
| 4 | `rmcp` / `agent-client-protocol` crate coverage (SSE client, OAuth, ACP v1 session list/resume/fork/close) at chosen versions | `cargo add rmcp --features client,transport-sse-client-reqwest,transport-streamable-http-client-reqwest,auth` then check docs.rs; same for `agent-client-protocol` vs `@agentclientprotocol/sdk@0.21.0` schema |
| 5 | Purpose of `BUN_BE_BUN=1` in formatter env (npm `.bin` shims normally run under `node`) | test compiled opencode: `BUN_BE_BUN=1 opencode --version` and run a formatter with `node` absent from PATH |
| 6 | Whether MCP stdio `stderr: "pipe"` is drained by the SDK (risk of child blocking on full pipe) | read `node_modules/@modelcontextprotocol/sdk/dist/esm/client/stdio.js` |
| 7 | When the v2 watcher location node is first instantiated in the v1 CLI (lazy per directory?) | trace `locationServiceMapLayer` users (`opencode/src/server/routes/instance/httpapi/server.ts:102`) at runtime with `OPENCODE_LOG_LEVEL=DEBUG` and look for "watcher backend" |
| 8 | Bun `$` escaping/quoting rules plugins depend on | Bun 1.4.4 docs `bun.sh/docs/runtime/shell`; moot if sidecar is Bun |
