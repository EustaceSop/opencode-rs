# 02 — API surface: HTTP / SSE / WebSocket (opencode v1.19.5)

Source root: `opencode-ts/packages/` (all paths relative to it). Version: `opencode/package.json:3` = `1.19.5`.

Path abbreviations used in `src` columns and refs:

| Abbrev | Path |
|---|---|
| `G/` | `opencode/src/server/routes/instance/httpapi/groups/` (legacy "v1" route contracts) |
| `H/` | `opencode/src/server/routes/instance/httpapi/handlers/` |
| `M/` | `opencode/src/server/routes/instance/httpapi/middleware/` |
| `HA/` | `opencode/src/server/routes/instance/httpapi/` |
| `P/` | `protocol/src/groups/` (v2 `/api/*` route contracts) |
| `S/` | `server/src/` (v2 handlers/middleware) |
| `shared/` | `opencode/src/server/shared/` |

Evidence levels: statements with `path:line` are read from source. `assumption:` = inference, not verified. `UNKNOWN` = not determinable from this checkout (no `node_modules`; Effect `4.0.0-beta.83` source not available, root `package.json:66`). §8 lists the load-bearing `UNKNOWN`s with commands to resolve them.

---

## 0. Key findings

1. **One listener, one router.** The shipped `opencode` binary serves everything from a single Effect `HttpRouter` built in `HA/server.ts:269-311`: legacy root routes, legacy `/event` SSE, legacy PTY WebSocket, legacy instance routes, the v2 `/api/*` API (from `packages/server` + `packages/protocol`), `GET /doc`, and a catch-all UI route (`HA/server.ts:274-282`). There is no separate v2 port.
2. **`packages/server/src/routes.ts` is not what `opencode serve` runs.** Its `createRoutes` (`S/routes.ts:39-63`) serves only `/api/*` plus `/openapi.json` (`S/routes.ts:54`). Its consumers are the `lildax` binary (`cli/package.json` bin `lildax`, `cli/src/commands/handlers/serve.ts:10,41`) and the in-process `sdk-next` (`sdk-next/src/opencode.ts:6,24`).
3. **`openapi.json` is complete and current for routes.** `packages/docs/openapi.json` is a 19-byte file whose content is `../sdk/openapi.json` (a symlink checked out as text). `packages/sdk/openapi.json` has 188 operations and 472 component schemas (OpenAPI `3.1.0`). The source has 188 `HttpApiEndpoint.<method>(` definitions in `G/*.ts` + `P/*.ts`. The set of 188 `identifier:` operationIds diffs to zero against the openapi.json `operationId`s. Spot-checked query/body/param schemas match. **The wire behavior differs from openapi.json in ten documented places (§2.1).** Use it for type generation, not for error shapes or streaming.
4. **Two error vocabularies, chosen by path prefix**:
   - Legacy paths use NamedError `{name,data}`, empty-body 401s, and `{"_tag":"BadRequest"}` built-ins.
   - `/api/*` uses Effect `TaggedError` `{_tag,message,...}` with a JSON 401.
   See §2.7.
5. **Four event streams** (§3):
   - `/event`: per directory.
   - `/global/event`: all directories; the web app uses this one.
   - `/api/event`: v2 envelope.
   - `/api/session/{id}/event`: durable replay.

   The legacy streams emit in-band `server.heartbeat` events every 10 s. They have no `id:` fields and no server-side replay.
6. **PTY WebSocket** (§4): raw UTF-8 text frames, plus one binary control frame `0x00 + {"cursor":n}`. Short-lived single-use tickets (`?ticket=`) let browsers skip Basic auth. The cursor unit is JS string length (UTF-16 code units).
7. **Unknown paths return the web UI (HTML 200) for any method.** Clients rely on this: the v2 SDK treats `text/html` as "server too old" (`sdk/js/src/v2/client.ts:84-90`). The web app picks its protocol by probing `/global/health` (§6).

---

## 1. Server topology, startup, transport policy

### 1.1 Which process serves HTTP

| Entry | Source | Listener | Notes |
|---|---|---|---|
| `opencode serve` | `opencode/src/cli/cmd/serve.ts:6-24` | `Server.listen` (`opencode/src/server/server.ts:73-81`) | If `OPENCODE_SERVER_PASSWORD` is unset, it prints `Warning: OPENCODE_SERVER_PASSWORD is not set; server is unsecured.` (`serve.ts:15-17`). It then prints `opencode server listening on http://<hostname>:<port>` (`serve.ts:20`). |
| `opencode web` | `opencode/src/cli/cmd/web.ts:31-80` | `Server.listen` | Prints a banner and opens a browser. With `0.0.0.0` it shows `http://localhost:<port>` and the LAN IPs. |
| `opencode acp` | `opencode/src/cli/cmd/acp.ts:25` | `Server.listen` | |
| `opencode` (TUI) default | `opencode/src/cli/cmd/tui.ts:273-287`, `opencode/src/cli/tui/worker.ts:30-58` | **none** | The worker thread calls `Server.Default().app.fetch` over RPC. The base URL is `http://opencode.internal` (`tui.ts:285`). Events arrive over RPC `global.event`, which forwards raw GlobalBus objects (`worker.ts:24-26`). |
| `opencode` (TUI) with `--port`/`--hostname`/`--mdns` | `tui.ts:273-280`, `worker.ts:54-57` | `Server.listen` in the worker | The TUI then uses HTTP and SSE `/global/event` (`tui/src/context/sdk.tsx:91`). |
| `opencode run` | `opencode/src/cli/cmd/run.ts:906,944` | none (in-process `Server.Default().app.fetch`) | |
| Desktop (Electron) | `desktop/src/main/sidecar.ts:51-66` | in-process `Server.listen({port,hostname,username:"opencode",password,cors:["oc://renderer"]})` | The env sets `OPENCODE_SERVER_USERNAME=opencode` and `OPENCODE_SERVER_PASSWORD=<uuid>` (`sidecar.ts:83-89`). |
| Desktop WSL | `desktop/src/main/wsl/sidecar.ts:38` | spawns `opencode --print-logs --log-level <WARN\|INFO> serve --hostname 0.0.0.0 --port <N>` | |
| JS SDK `createOpencodeServer` | `sdk/js/src/v2/server.ts:22-100` | spawns `opencode serve --hostname=<h> --port=<p> [--log-level=…]` (defaults `127.0.0.1`, `4096`) | Sets env `OPENCODE_CONFIG_CONTENT=<json>` (`:38`). Parses stdout lines that start with `opencode server listening` using regex `/on\s+(https?:\/\/[^\s]+)/` (`:56-57`). |
| `lildax serve` (not shipped as `opencode`) | `cli/src/commands/handlers/serve.ts:15-46` | v2-only routes | Scans ports 4096..65535 (`:30-37`). Prints `server listening on …` (`:23`). |

### 1.2 CLI flags, config, env

Network options (`opencode/src/cli/network.ts:5-32`), used by `serve`, `web`, `acp`, and the TUI:

| Flag | Type | Default |
|---|---|---|
| `--port` | number | `0` |
| `--hostname` | string | `"127.0.0.1"` |
| `--mdns` | boolean | `false` |
| `--mdns-domain` | string | `"opencode.local"` |
| `--cors` | string[] (repeatable) | `[]` |

Resolution (`network.ts:61-78`):
- An explicitly passed flag wins. Explicit means it is present in argv before `--`; `hasArg` / `hasBooleanArg` live at `:40-53`.
- Otherwise config `server.{port,hostname,mdns,mdnsDomain}` applies (schema `core/src/v1/config/server.ts:6-18`). Otherwise the flag default applies.
- `hostname` becomes `0.0.0.0` when mDNS is on and no config hostname exists.
- `cors` is the config list followed by the flag list.

Port selection (`opencode/src/server/server.ts:117-122`):
- `port == 0` tries 4096 first and falls back to an OS-assigned port. Tests: `opencode/test/server/httpapi-listen.test.ts:356-374`.
- `port != 0` binds that port or fails.

Global CLI options: `--print-logs`, `--log-level`, `--pure` (`opencode/src/index.ts:53-62`).

Env vars relevant to the API:

| Var | Effect | Ref |
|---|---|---|
| `OPENCODE_SERVER_PASSWORD` | Enables auth when set and non-empty | `opencode/src/server/auth.ts:18,24-26` |
| `OPENCODE_SERVER_USERNAME` | Expected Basic username. Default `opencode` | `auth.ts:19` |
| `OPENCODE_DISABLE_EMBEDDED_WEB_UI` | Proxies the UI from `app.opencode.ai` instead of serving the embedded copy | `opencode/src/effect/runtime-flags.ts:20`, `opencode/src/server/shared/ui.ts:43-48` |
| `OPENCODE_WORKSPACE_ID` | "Server runs inside a workspace" mode: forces local routing and enables the `x-opencode-sync` fence header | `core/src/flag/flag.ts:49`, `M/fence.ts:15`, `M/workspace-routing.ts:65-67` |
| `OPENCODE_EXPERIMENTAL_WORKSPACES` | Makes the TUI call `POST /sync/start` | `core/src/flag/flag.ts:50`, `tui/src/context/sdk.tsx:96-100,125-128` |

mDNS (`opencode/src/server/server.ts:155-170`, `opencode/src/server/mdns.ts:6-19`):
- Published only if `mdns` is on and hostname is not one of `127.0.0.1`, `localhost`, `::1`.
- Bonjour service name `opencode-<port>`, type `http`, `host` = mdnsDomain (default `opencode.local`), TXT `{path:"/"}`.
- Unpublished on stop.

Shutdown:
- Node `http.Server` with `gracefulShutdownTimeout: "1 second"` (`server.ts:214`).
- `stop(true)` also calls `closeAllConnections()` (`server.ts:195-224`).
- Open WebSockets get close `1001 "server closing"` (`HA/websocket-tracker.ts:5,31-44`).

### 1.3 Route tree and precedence

`createRoutes` merges these layers (`HA/server.ts:274-282`; the design comment is at `:129-134`):

| Layer | API object | Paths | Auth middleware | Routing middleware |
|---|---|---|---|---|
| `rootApiRoutes` (`:140-144`) | `RootHttpApi` (`HA/api.ts:54-59`) = Control + ControlPlane + Global | `/auth/*`, `/log`, `/global/*`, `/experimental/control-plane/*` | legacy `Authorization` (API-level) | none |
| `eventApiRoutes` (`:145-148`) | `EventApi` (`G/event.ts`) | `GET /event` | legacy `Authorization` | WorkspaceRouting + InstanceContext |
| `ptyConnectApiRoutes` (`:149-152`) | `PtyConnectApi` (`G/pty.ts:140-172`) | `GET /pty/:ptyID/connect` | `PtyConnectAuthorization` (ticket-aware) | WorkspaceRouting + InstanceContext |
| `instanceRoutes` (`:153-175`) | `InstanceHttpApi` (`HA/api.ts:61-77`), 15 groups | all other legacy paths | legacy `Authorization` (per group) | WorkspaceRouting + InstanceContext |
| `serverRoutes` (`:176-180`) | `Api` = `makeDefaultApi` (`S/api.ts:1-8`, `protocol/src/api.ts:26-64`) | `/api/*` and `/experimental/project/:projectID/copy[/refresh]` | v2 `Authorization` (`S/middleware/authorization.ts`) | `LocationMiddleware` / `SessionLocationMiddleware` |
| `docRoute` (`:189-191`) | raw | `GET /doc` | router-level auth | — |
| `uiRoute` (`:193-202`) | raw `router.add("*", "/*")` | everything else, any method | router-level auth (public manifest paths exempt) | — |

Precedence:
- Declared routes win over the UI fallback (test `opencode/test/server/httpapi-ui.test.ts:359-366`).
- Static path segments must win over params. Required pairs: `/session/status` vs `/session/:sessionID`, `/pty/shells` vs `/pty/:ptyID`, `/api/session/active` vs `/api/session/:sessionID`, `/experimental/workspace/{adapter,status,sync-list,warp}` vs `/experimental/workspace/:id`.
- assumption: the Effect router resolves static-before-param, as find-my-way does.
- The OpenAPI document is built from `OpenCodeHttpApi` (`HA/api.ts:79-94`). It uses `ServerApi = makeApi({definitions: EventManifest.Latest…})` (`HA/api.ts:48-52`), which has the same paths as the runtime `Api`.

### 1.4 Middleware pipeline

Global middleware, provided to all layers (`HA/server.ts:283-291`): `errorLayer`, `compressionLayer`, `corsVaryFix`, `fenceLayer`, `cors(corsOptions)`.

The app-level `disposeMiddleware` is passed to `HttpRouter.serve` (`opencode/src/server/server.ts:101-104`).

The exact nesting order of the global layers is `UNKNOWN`. Observable behavior:
- CORS preflight is answered without auth (tests `httpapi-cors.test.ts:46-60`, `httpapi-ui.test.ts:443-454`).
- CORS headers are present on 401 responses (`httpapi-cors.test.ts:63-82`).

Per-group order is declared as `.middleware(InstanceContext).middleware(WorkspaceRouting).middleware(Authorization)` (e.g. `G/session.ts:452-454`). assumption: the last declared is outermost, so execution is Auth → WorkspaceRouting (may proxy) → InstanceContext → handler. A Rust port must authenticate before proxying in any case.

### 1.5 Authentication

**Activation.** Auth is active iff `OPENCODE_SERVER_PASSWORD` is set and non-empty (`opencode/src/server/auth.ts:24-26`). Otherwise every middleware is a no-op (`M/authorization.ts:104,122,138`).

**Credential sources, in order** (`M/authorization.ts:77-83`; v2 `S/middleware/authorization.ts:29-36`):
1. Query `?auth_token=<base64("user:pass")>`. It wins over the header (test `httpapi-authorization.test.ts:127-136`).
2. Header `Authorization: Basic <base64("user:pass")>`, matched case-insensitively by regex `^Basic\s+(.+)$`.

**Decoding.** The decoded string is split at the first `:`, so passwords may contain `:` (`M/authorization.ts:57-71`; test `httpapi-ui.test.ts:410-421`). A base64 failure or a missing `:` yields an empty credential, which gets a 401 (test `httpapi-authorization.test.ts:157-162`).

**Check.** `username === OPENCODE_SERVER_USERNAME (default "opencode") && password === OPENCODE_SERVER_PASSWORD`. It uses plain `===`, not constant-time (`auth.ts:28-34`).

**401 shapes:**

| Scope | Response | Ref |
|---|---|---|
| Legacy root/instance/PTY routes | status 401, **empty body** (`HttpApiError.UnauthorizedNoContent`), header `www-authenticate: Basic realm="Secure Area"` | `M/authorization.ts:14,19-24,40-55` |
| Router-level (`/doc`, UI) | 401, empty body, same header | `M/authorization.ts:85-99,101-116` |
| v2 (`/api/*`, `/experimental/project/:id/copy*`) | 401 JSON `{"_tag":"UnauthorizedError","message":"Authentication required"}` + same header | `S/middleware/authorization.ts:52-58`; test `httpapi-authorization.test.ts:164-172` |

**Bypasses:**
- `GET /site.webmanifest`, `/web-app-manifest-192x192.png`, and `/web-app-manifest-512x512.png` skip router-level auth (`opencode/src/server/shared/public-ui.ts:4-12`).
- A PTY connect whose matched route path is the connect path and that carries a non-empty `?ticket=` skips credential auth. The ticket is validated in the handler (`M/authorization.ts:134-153`, `S/middleware/authorization.ts:48-51`).
- CORS preflight (see above).

The in-process TUI worker injects `Authorization` from env if it is missing (`opencode/src/cli/tui/worker.ts:32-36`).

### 1.6 CORS

The origin predicate is `isAllowedCorsOrigin(origin, {cors})` (`server/src/cors.ts:11-20`). An origin is allowed if any of these holds:
- No `Origin` header.
- The origin starts with `http://localhost:` or `http://127.0.0.1:`.
- The origin starts with `oc://renderer`.
- The origin equals `tauri://localhost`, `http://tauri.localhost`, or `https://tauri.localhost`.
- The origin matches regex `^https://([a-z0-9-]+\.)*opencode\.ai$`.
- The origin is in the `cors` list (exact string match).

Middleware: Effect `HttpMiddleware.cors({ allowedOrigins: predicate, maxAge: 86_400 })`, global (`HA/server.ts:120-127`). Observed behavior:

| Case | Response | Test |
|---|---|---|
| Allowed origin, preflight | 204. `access-control-allow-origin` echoes the request origin. `access-control-allow-headers` echoes `access-control-request-headers`. | `httpapi-cors.test.ts:46-60` |
| Disallowed origin, preflight | still 204, without the ACAO header | `httpapi-cors.test.ts:118-119` |

`corsVaryFix` appends `Origin` to `Vary` whenever ACAO is set and is not `*` (`M/cors-vary.ts:13-29`).

UNKNOWN Effect defaults: `access-control-allow-methods`, `allow-credentials`, `expose-headers`. One per-route expose header is set manually: `Access-Control-Expose-Headers: Link, X-Next-Cursor` (`H/session.ts:140`).

PTY origin check `isAllowedRequestOrigin(origin, host)` (`server/src/cors.ts:22-26`): allowed if there is no Origin, or the Origin's `host` equals the `Host` header, or the CORS predicate allows it.

### 1.7 Request context: directory / workspace selection

**Legacy instance routes** (WorkspaceRouting `M/workspace-routing.ts:148-235` + InstanceContext `M/instance-context.ts:14-35`):

1. **Session lookup.** A session ID is taken from the path `^/session/([^/]+)(/|$)` or `^/experimental/session/([^/]+)/background$`. `/session/status` is excluded (`opencode/src/server/shared/workspace-routing.ts:20-29`). The session is loaded; NotFound and defects are ignored (`M/workspace-routing.ts:222-231`).
2. **Workspace selection.** `workspaceID` = `session.workspaceID` if set, else `?workspace=` (`M/workspace-routing.ts:69-72`). The header `x-opencode-workspace` is **not** read on legacy paths.
3. **Env override.** If env `OPENCODE_WORKSPACE_ID` is set, the request is always local and `workspaceID` = the env value (`:166,177-184`).
4. **Missing workspace.** A workspace ID that is not found returns `500 text/plain; charset=utf-8` with body `Workspace not found: <id>` (`:102-107,173-175`).
5. **Workspace present.** If a workspace exists and the route is not control-plane-local, an adapter target is resolved (`:177-179`):
   - Remote target → proxy (§1.8).
   - Local target → `directory = target.directory`.
6. **Control-plane-local routes** (`shared/workspace-routing.ts:5-18`, `M/workspace-routing.ts:90-92`):
   - Any method on `/experimental/workspace[/…]`.
   - `GET /session` (exact path or prefix).
   - Any path starting with `/console`.
   - `/session/status` is explicitly forwarded.
7. **Directory fallback** (`M/workspace-routing.ts:86-88,181-184`), first non-empty wins:
   - `session.directory`
   - `?directory=`
   - header `x-opencode-directory`
   - `process.cwd()`
8. **Instance load.** InstanceContext applies `decodeURIComponent` to the chosen directory, falling back to the raw string on error, then calls `InstanceStore.load({directory})` (`M/instance-context.ts:15-21,29`). Consequence: the `?directory=` value is percent-decoded twice (once by URL parsing, once here). Replicate this.

Client behavior:
- The SDKs set header `x-opencode-directory: encodeURIComponent(dir)` (`sdk/js/src/client.ts:46-51`, `sdk/js/src/v2/client.ts:63-68`).
- For GET/HEAD they move it to `?directory=` (`client.ts:17-31`).
- The v2 SDK also moves `x-opencode-workspace` to `?workspace=`. For `/api/` paths it adds `location[directory]` and `location[workspace]` too (`v2/client.ts:18-48`).

**v2 `/api/*` routes.** These do no workspace proxying.
- `LocationMiddleware` (`S/location.ts:29-60`):
  - directory = query `location[directory]` ‖ `decodeURIComponent(header x-opencode-directory)` ‖ `process.cwd()`.
  - workspace = query `location[workspace]` ‖ header `x-opencode-workspace`.
  - Both are passed to `LocationServiceMap.get`.
- `SessionLocationMiddleware` (`S/middleware/session-location.ts:24-67`):
  - Location comes from the DB row of `:sessionID`.
  - A malformed ID returns 400 `InvalidRequestError{message:"Invalid session ID",field:"sessionID"}`.
  - A missing row returns 404 `SessionNotFoundError{sessionID,message:"Session not found: <id>"}`.
- Location-scoped responses are wrapped `{ "location": LocationInfo, "data": … }` (`S/location.ts:15-27`). `LocationInfo` = `{directory, workspaceID?, project:{id,directory}}` (openapi component `LocationInfo`).

### 1.8 Remote-workspace proxy and sync fence

| Behavior | Detail | Ref |
|---|---|---|
| Workspace not syncing | `503 text/plain` `broken sync connection for workspace: <id>` | `M/workspace-routing.ts:121-127` |
| Target URL | `target.url` + request path. Query is copied minus `workspace` and `directory`. | `shared/workspace-routing.ts:31-45` |
| Request headers | Removes hop-by-hop headers (`connection`, `keep-alive`, `proxy-*`, `te`, `trailer`, `transfer-encoding`, `upgrade`), `host`, `accept-encoding`, `x-opencode-directory`, `x-opencode-workspace`, then applies the adapter's `target.headers` | `opencode/src/server/proxy-util.ts:1-30` |
| WebSocket | Bidirectional relay. `sec-websocket-protocol` is forwarded. Upstream close is mirrored; proxy failure sends close `1011 "proxy error"`. | `M/proxy.ts:14-77`, `proxy-util.ts:32-47` |
| HTTP response | Strips `content-encoding` and `content-length`. Upstream ≥500: body is buffered (logged, first 2000 chars) and forwarded with the same status and content type. Otherwise the body is streamed. A transport error returns an empty 500. | `M/proxy.ts:83-129` |
| Fence | If the upstream response has `x-opencode-sync`, the control plane waits until local sync reaches those sequence numbers. On failure it returns `503` text. | `M/workspace-routing.ts:131-143`, `shared/fence.ts:34-60` |
| Fence emit | Only when env `OPENCODE_WORKSPACE_ID` is set, and only for non-GET/HEAD/OPTIONS requests. Header `x-opencode-sync: {"<aggregateID>":<seq>,…}` carries the event-sequence rows changed by the request; a removed row gets `-1`. | `M/fence.ts:7-22`, `shared/fence.ts:8-34` |

### 1.9 Compression, error mapping, post-response lifecycle

**Compression** (`M/compression.ts`). The response is gzip- or deflate-compressed only if all of these hold:
- The body is an in-memory `Uint8Array` (streams are never compressed) of at least 1024 bytes (`:14,40-42`).
- The method is not HEAD.
- There is no existing `content-encoding` or `transfer-encoding` header.
- `cache-control` does not contain `no-transform`.
- The path is not `/event` or `/global/event`, and the request is not `POST /session/<id>/message` or `POST /session/<id>/prompt_async` (`:11-12,47-49`).
- The content type matches the compressible regex (`:6-7`). `text/event-stream` is excluded.
- `Accept-Encoding` contains the substring `gzip` (chosen first) or `deflate`. q-values are ignored (`:18-24`).

**Defects** (`M/error.ts:7-44`) are unhandled failures, not typed errors:
- Config errors (`ConfigErrorV1` Json/Invalid/Frontmatter/DirectoryTypo/RemoteAuth) → `400` with `error.toObject()` = `{name,data}`.
- Anything else → `500` `{"name":"UnknownError","data":{"message":"Unexpected server error. Check server logs for details.","ref":"err_<first 8 chars of a UUID>"}}`. NamedError shape: `core/src/util/error.ts:55-60`.

**Schema rejection (400):**
- Legacy paths (`M/schema-error.ts:24-41`): `{"name":"BadRequest","data":{"message":<reason>,"kind":<kind>}}`. The reason is truncated to 1024 chars plus `… (N more chars)` (U+2026). `kind` is one of `Params|Headers|Query|Body|Payload` (enum from `HA/public.ts:198`).
- `/api/*` (`S/middleware/schema-error.ts:7-20`): `{"_tag":"InvalidRequestError","message":<reason>,"kind":<kind>}`. The truncation suffix uses ASCII `...`.

**Post-response lifecycle** (`HA/lifecycle.ts`):
- `markInstanceForDisposal`: the instance is disposed after the response has been produced. Used by `PATCH /config` (`H/config.ts:18-22`) and `POST /instance/dispose` (`H/instance.ts:24-27`).
- `markInstanceForReload`: the instance is reloaded before the response is returned. Used by `POST /project/git/init` (`H/project.ts:23-34`).
- Disposal emits `server.instance.disposed` on the GlobalBus (`opencode/src/project/instance-store.ts:79-92`).

### 1.10 Rust mapping (§1)

- **Stack.** `axum` 0.8 + `hyper` 1 + `tower-http`.
  - Use one `Router` with sub-routers per layer.
  - Implement the UI catch-all with `Router::fallback` for all methods.
  - `matchit` supports static-over-param priority, which covers `/session/status` vs `/session/{id}`.
  - Express the PTY wildcard as `/api/fs/read/{*path}`.
- **Auth.** Write a custom `tower::Layer` with three flavors (legacy-empty-401, v2-JSON-401, router-level with public-path bypass) plus the ticket bypass. Run the CORS preflight layer outside auth.
- **CORS.** Use `tower_http::cors::CorsLayer` with `AllowOrigin::predicate`, `allow_headers(AllowHeaders::mirror_request())`, `max_age(86400)`, and a custom `Vary: Origin` merge.
- **Compression.** Use `tower_http::compression::CompressionLayer` with a custom `Predicate`: size ≥1024, the content-type regex, the path exclusions, skip streamed bodies. Restrict it to gzip/deflate, and reproduce the substring-based encoding pick if byte-identical behavior matters.
- **Context extractors.** Write a `RouteContext` extractor that implements §1.7 exactly (including the double decode). Write a `Location` extractor for v2.
- **Process.** Print the exact stdout line `opencode server listening on http://{hostname}:{port}` (§1.1). Port policy: try 4096, then bind `:0`. Use `mdns-sd` for mDNS.
- **Shutdown.** Use `axum::serve(...).with_graceful_shutdown`, a 1 s deadline, and a WebSocket registry that sends close 1001.

---

## 2. Route table

### 2.1 openapi.json status and its known divergences from runtime

**Provenance.** `packages/sdk/openapi.json` is produced by `bun dev generate > ../sdk/openapi.json` (`script/generate.ts:7`). That command runs `Server.openapi()` = `OpenApi.fromApi(PublicApi)` (`opencode/src/server/server.ts:67-69`) and adds `x-codeSamples` (`opencode/src/cli/cmd/generate.ts:8-31`). `PublicApi` applies `matchLegacyOpenApi` (`HA/public.ts:82-178`).

**Not served at runtime.** The runtime `/doc` serves the same transform without `x-codeSamples` (`HA/server.ts:187-191`). The combined server does not serve `/openapi.json`.

**Divergences of runtime behavior from openapi.json** (all must be handled manually):

| # | openapi.json says | Runtime does | Ref |
|---|---|---|---|
| D1 | Legacy 400 = `InvalidRequestError` (`_tag`) or `BadRequestError` | Legacy schema rejection → `{name:"BadRequest",data:{message,kind}}`. Handler-raised `HttpApiError.BadRequest` → `{"_tag":"BadRequest"}`; this encoding is UNKNOWN, see the component `effect_HttpApiError_BadRequest`. | `M/schema-error.ts:27-37` |
| D2 | No 401 on legacy paths (stripped) | Empty-body 401 + `www-authenticate`. `/experimental/project/{id}/copy*` (v2 group, non-`/api` path) also lacks 401 in the spec but returns JSON 401. | `HA/public.ts:146-153,180-182` |
| D3 | `/api/event` data = `V2Event` (88 types, full `EventManifest.Latest`) | The encoder is `OpenCodeEvent` = `EventManifest.ServerDefinitions` + `server.connected` (59 types: 58 + `server.connected`). See §3.4 column "/api/event runtime". | `HA/api.ts:48-52` vs `S/handlers/event.ts:2,16`, `P/event.ts:52-54`, `schema/src/event-manifest.ts:57-61` |
| D4 | Event unions have no heartbeat | `/event` and `/global/event` emit `type:"server.heartbeat"`. `/api/event` emits the SSE comment `: heartbeat`. | `H/event.ts:63-66`, `H/global.ts:35-37`, `S/handlers/event.ts:37` |
| D5 | Optional fields: `null` arms stripped | Output omits absent keys. Explicit `null` on input for `Schema.optional` fields: UNKNOWN. Genuinely nullable fields are re-added: `Workspace.{branch,directory,extra}`, `GlobalSession.project`, `SyncEventSessionUpdated.data.info.*`, `POST /experimental/workspace {branch,extra}`, `POST /experimental/workspace/warp {id}`. | `HA/public.ts:92-97,116-139,260-279` |
| D6 | Legacy request bodies are never `required` | `POST /session` and `POST /session/{id}/fork` accept an empty body. Other payload routes reject a missing body with 400 (D1 shape). | `HA/public.ts:110-113`, `H/session.ts:159-176,218-224` |
| D7 | Some query params typed `number`/`boolean` | The wire is a string. `Schema.NumberFromString` applies (exact parse semantics UNKNOWN). Booleans accept exactly `"true"` or `"false"` (`G/query.ts:3-8`; test `httpapi-query-schema-drift.test.ts:163-178`). | `HA/public.ts:58-74` |
| D8 | No response headers documented | `Link`, `X-Next-Cursor`, `Access-Control-Expose-Headers` (`GET /session/{id}/message`); `x-next-cursor` (`GET /experimental/session`); `x-opencode-sync` (fence) | `H/session.ts:131-143`, `H/experimental.ts:151-156` |
| D9 | `POST /session/{id}/message` and `/command` 200 = `{info:AssistantMessage, parts:Part[]}` | This schema is a hand override of the Effect-derived one. At runtime `/message` writes `JSON.stringify(promptSvc.prompt(...))` (`H/session.ts:300-308`). assumption: the shape is identical to `{info,parts}`. | `HA/public.ts:375-391` |
| D10 | `GlobalEvent.directory` required | `directory` is omitted when the event has no location and no InstanceRef. `server.connected` and `server.heartbeat` on `/global/event` have no `directory`. | `opencode/src/event-v2-bridge.ts:39-43`, `H/global.ts:37,41` |

### 2.2 Routes absent from openapi.json (combined server)

| Method | Path | Ref | Behavior |
|---|---|---|---|
| GET | `/doc` | `HA/server.ts:187-191` | `application/json` OpenAPI document (lazily built, cached bytes). Router-level auth. |
| any | `/*` (fallback) | `HA/server.ts:193-202` | Web UI (§5) |
| OPTIONS | any | `HA/server.ts:120-127` | CORS preflight (§1.6) |

Only on the `lildax` / `packages/server` variant: `GET /openapi.json` (`S/routes.ts:54`). The `packages/cli` API command fetches it (`cli/src/commands/handlers/api.ts:69`).

### 2.3 Conventions for the tables below

- Rows are generated from `packages/sdk/openapi.json`. The `src` column is the `HttpApiEndpoint.<m>(` line.
- `query` omits routing params:
  - Every legacy instance route accepts `directory` and `workspace` (`M/workspace-routing.ts:22-27`). `/log` also declares them but does no routing (`G/control.ts:12-15`).
  - Every v2 location route accepts `location[directory]` and `location[workspace]` (deepObject style, `P/location.ts:5-27`).
- `!` = required.
- Names in `body` and `2xx` are openapi component names. `{a,b?}` = inline object.
- Path params use `{name}` (openapi). Source uses `:name`.
- ID path params carry prefix patterns:

| Param | Pattern |
|---|---|
| `sessionID` | `^ses` |
| `messageID` | `^msg` |
| `partID` | `^prt` |
| `ptyID` | `^pty` |
| `permissionID` | `^per` |
| `requestID` | `^per` (permission) / `^que` (question) |
| workspace `id` | `^wrk` |

  A pattern mismatch → 400 (D1 or §2.7 shape by prefix).

### 2.4 Legacy root routes (no directory routing; `RootHttpApi`)

| M | Path | operationId | src | query | body | 2xx | non-2xx | notes |
|---|---|---|---|---|---|---|---|---|
| PUT | `/auth/{providerID}` | auth.set | G/control.ts:39 | - | Auth | 200 boolean | 400 |  |
| DELETE | `/auth/{providerID}` | auth.remove | G/control.ts:51 | - | - | 200 boolean | 400 |  |
| POST | `/log` | app.log | G/control.ts:62 | - | {service,level,message,extra?} | 200 boolean | 400 |  |
| POST | `/experimental/control-plane/move-session` | experimental.controlPlane.moveSession | G/control-plane.ts:22 | - | {sessionID,destination,moveChanges?} | 204 ∅ | 400 |  |
| GET | `/global/health` | global.health | G/global.ts:79 | - | - | 200 {healthy,version} | 400 | `version`=InstallationVersion (`"local"` in dev) core/src/installation/version.ts:6 |
| GET | `/global/event` | global.event | G/global.ts:88 | - | - | 200 `text/event-stream` GlobalEvent | 400 | SSE, §3 |
| GET | `/global/config` | global.config.get | G/global.ts:97 | - | - | 200 Config | 400 |  |
| PATCH | `/global/config` | global.config.update | G/global.ts:106 | - | Config | 200 Config | 400 | if changed: async dispose-all + `global.disposed` (H/global.ts:78) |
| POST | `/global/dispose` | global.dispose | G/global.ts:117 | - | - | 200 boolean | 400 | dispose all instances, emit `global.disposed` (opencode/src/server/global-lifecycle.ts:16) |
| POST | `/global/upgrade` | global.upgrade | G/global.ts:126 | - | {target} | 200 {success,version} \| {success,error} | 400 | raw JSON: 400 `{success:false,error:"Unknown installation method"}`, 500 `{success:false,error}`; success emits `installation.updated` on GlobalBus (H/global.ts:89-116) |

### 2.5 Legacy instance routes (`InstanceHttpApi` + `/event` + PTY connect)

| M | Path | operationId | src | query | body | 2xx | non-2xx | notes |
|---|---|---|---|---|---|---|---|---|
| GET | `/event` | event.subscribe | G/event.ts:14 | - | - | 200 `text/event-stream` Event | - | SSE per-directory, §3 |
| GET | `/config` | config.get | G/config.ts:16 | - | - | 200 Config | 400 |  |
| PATCH | `/config` | config.update | G/config.ts:26 | - | Config | 200 Config | 400 | returns request payload (not merged); instance disposed after response (H/config.ts:18-22) |
| GET | `/config/providers` | config.providers | G/config.ts:38 | - | - | 200 {providers,default} | 400 |  |
| GET | `/experimental/capabilities` | experimental.capabilities.get | G/experimental.ts:108 | - | - | 200 ExperimentalCapabilities | 400 |  |
| GET | `/experimental/console` | experimental.console.get | G/experimental.ts:118 | - | - | 200 ConsoleState | 400,500 |  |
| GET | `/experimental/console/orgs` | experimental.console.listOrgs | G/experimental.ts:129 | - | - | 200 {orgs} | 400,500 |  |
| POST | `/experimental/console/switch` | experimental.console.switchOrg | G/experimental.ts:140 | - | {accountID,orgID} | 200 boolean | - |  |
| GET | `/experimental/tool` | tool.list | G/experimental.ts:152 | provider!,model! | - | 200 ToolList | 400 |  |
| GET | `/experimental/tool/ids` | tool.ids | G/experimental.ts:164 | - | - | 200 ToolIDs | 400 |  |
| GET | `/experimental/worktree` | worktree.list | G/experimental.ts:176 | - | - | 200 string[] | 400 |  |
| POST | `/experimental/worktree` | worktree.create | G/experimental.ts:187 | - | WorktreeCreateInput | 200 Worktree | 400 |  |
| DELETE | `/experimental/worktree` | worktree.remove | G/experimental.ts:200 | - | WorktreeRemoveInput | 200 boolean | 400 |  |
| POST | `/experimental/worktree/reset` | worktree.reset | G/experimental.ts:212 | - | WorktreeResetInput | 200 boolean | 400 |  |
| GET | `/experimental/session` | experimental.session.list | G/experimental.ts:224 | roots,start,cursor,search,limit,archived | - | 200 GlobalSession[] | 400 | default limit 100; header `x-next-cursor`=last item `time.updated` when more (H/experimental.ts:138-156) |
| POST | `/experimental/session/{sessionID}/background` | experimental.session.background | G/experimental.ts:235 | - | - | 200 boolean | 400 |  |
| GET | `/experimental/resource` | experimental.resource.list | G/experimental.ts:248 | - | - | 200 Record<McpResource> | 400 |  |
| GET | `/find` | find.text | G/file.ts:108 | pattern! | - | 200 {path,lines,line_number,absolute_offset,submatches}[] | 400 |  |
| GET | `/find/file` | find.files | G/file.ts:118 | query!,dirs,type,limit | - | 200 string[] | 400 |  |
| GET | `/find/symbol` | find.symbols | G/file.ts:128 | query! | - | 200 Symbol[] | 400 |  |
| GET | `/file` | file.list | G/file.ts:138 | path! | - | 200 FileNode[] | 400 |  |
| GET | `/file/content` | file.read | G/file.ts:148 | path! | - | 200 FileContent | 400 |  |
| GET | `/file/status` | file.status | G/file.ts:158 | - | - | 200 File[] | 400 |  |
| POST | `/instance/dispose` | instance.dispose | G/instance.ts:62 | - | - | 200 boolean | 400 | instance disposed after response is sent (lifecycle.ts:43) |
| GET | `/path` | path.get | G/instance.ts:72 | - | - | 200 Path | 400 |  |
| GET | `/vcs` | vcs.get | G/instance.ts:83 | - | - | 200 VcsInfo | 400 |  |
| GET | `/vcs/status` | vcs.status | G/instance.ts:94 | - | - | 200 VcsFileStatus[] | 400 |  |
| GET | `/vcs/diff` | vcs.diff | G/instance.ts:104 | mode!,context | - | 200 VcsFileDiff[] | 400 |  |
| GET | `/vcs/diff/raw` | vcs.diff.raw | G/instance.ts:114 | - | - | 200 `text/x-diff; charset=utf-8` string | 400 |  |
| POST | `/vcs/apply` | vcs.apply | G/instance.ts:127 | - | {patch} | 200 {applied} | 400 |  |
| GET | `/command` | command.list | G/instance.ts:139 | - | - | 200 Command[] | 400 |  |
| GET | `/agent` | app.agents | G/instance.ts:149 | - | - | 200 Agent[] | 400 |  |
| GET | `/skill` | app.skills | G/instance.ts:159 | - | - | 200 {name,description?,location,content}[] | 400 |  |
| GET | `/lsp` | lsp.status | G/instance.ts:169 | - | - | 200 LSPStatus[] | 400 |  |
| GET | `/formatter` | formatter.status | G/instance.ts:179 | - | - | 200 FormatterStatus[] | 400 |  |
| GET | `/mcp` | mcp.status | G/mcp.ts:45 | - | - | 200 Record<MCPStatus> | 400 |  |
| POST | `/mcp` | mcp.add | G/mcp.ts:55 | - | {name,config} | 200 Record<MCPStatus> | 400 |  |
| POST | `/mcp/{name}/auth` | mcp.auth.start | G/mcp.ts:67 | - | - | 200 {authorizationUrl,oauthState} | 400,404 |  |
| DELETE | `/mcp/{name}/auth` | mcp.auth.remove | G/mcp.ts:105 | - | - | 200 {success} | 400,404 |  |
| POST | `/mcp/{name}/auth/callback` | mcp.auth.callback | G/mcp.ts:79 | - | {code} | 200 MCPStatus | 400,404 |  |
| POST | `/mcp/{name}/auth/authenticate` | mcp.auth.authenticate | G/mcp.ts:93 | - | - | 200 MCPStatus | 400,404 |  |
| POST | `/mcp/{name}/connect` | mcp.connect | G/mcp.ts:117 | - | - | 200 boolean | 400,404 |  |
| POST | `/mcp/{name}/disconnect` | mcp.disconnect | G/mcp.ts:128 | - | - | 200 boolean | 400,404 |  |
| GET | `/project` | project.list | G/project.ts:22 | - | - | 200 Project[] | 400 |  |
| GET | `/project/current` | project.current | G/project.ts:32 | - | - | 200 Project | 400 |  |
| POST | `/project/git/init` | project.initGit | G/project.ts:42 | - | - | 200 Project | 400 | instance reloaded before response if project changed (H/project.ts:23) |
| PATCH | `/project/{projectID}` | project.update | G/project.ts:52 | - | {name?,icon?,commands?} | 200 Project | 400,404 |  |
| GET | `/project/{projectID}/directories` | project.directories | G/project.ts:65 | - | - | 200 ProjectDirectories | 400 |  |
| POST | `/experimental/project/{projectID}/copy/generate-name` | experimental.projectCopy.generateName | G/project-copy.ts:15 | - | {context?} | 200 {name} | 400 |  |
| GET | `/pty/shells` | pty.shells | G/pty.ts:44 | - | - | 200 {path,name,acceptable}[] | 400 |  |
| GET | `/pty` | pty.list | G/pty.ts:54 | - | - | 200 Pty[] | 400 | legacy hides non-running PTYs (H/pty.ts:66) |
| POST | `/pty` | pty.create | G/pty.ts:64 | - | {command?,args?,cwd?,title?,env?} | 200 Pty | 400 |  |
| GET | `/pty/{ptyID}` | pty.get | G/pty.ts:76 | - | - | 200 Pty | 400,404 | non-running → 404 PtyNotFoundError (H/pty.ts:95) |
| PUT | `/pty/{ptyID}` | pty.update | G/pty.ts:88 | - | {title?,size?} | 200 Pty | 400,404 |  |
| DELETE | `/pty/{ptyID}` | pty.remove | G/pty.ts:101 | - | - | 200 boolean | 400,404 |  |
| POST | `/pty/{ptyID}/connect-token` | pty.connectToken | G/pty.ts:113 | - | - | 200 PtyTicketConnectToken | 400,403,404 | requires header `x-opencode-ticket: 1` and allowed Origin else 403 (H/pty.ts:146) |
| GET | `/question` | question.list | G/question.ts:22 | - | - | 200 QuestionRequest[] | 400 |  |
| POST | `/question/{requestID}/reply` | question.reply | G/question.ts:32 | - | {answers} | 200 boolean | 400,404 |  |
| POST | `/question/{requestID}/reject` | question.reject | G/question.ts:45 | - | - | 200 boolean | 400,404 |  |
| GET | `/permission` | permission.list | G/permission.ts:21 | - | - | 200 PermissionRequest[] | 400 |  |
| POST | `/permission/{requestID}/reply` | permission.reply | G/permission.ts:31 | - | {reply,message?} | 200 boolean | 400,404 |  |
| GET | `/provider` | provider.list | G/provider.ts:38 | - | - | 200 {all,default,connected} | 400 |  |
| GET | `/provider/auth` | provider.auth | G/provider.ts:48 | - | - | 200 Record<ProviderAuthMethod[]> | 400 |  |
| POST | `/provider/{providerID}/oauth/authorize` | provider.oauth.authorize | G/provider.ts:58 | - | {method,inputs?} | 200 ProviderAuthAuthorization | 400 | raw body decode; no result → JSON `null` (H/provider.ts:81-93) |
| POST | `/provider/{providerID}/oauth/callback` | provider.oauth.callback | G/provider.ts:71 | - | {method,code?} | 200 boolean | 400 |  |
| GET | `/session` | session.list | G/session.ts:111 | scope,path,roots,start,search,limit | - | 200 Session[] | 400 | always served locally even with `workspace` (shared/workspace-routing.ts:8) |
| POST | `/session` | session.create | G/session.ts:203 | - | {parentID?,title?,agent?,model?,metadata?,permission?,workspaceID?} | 200 Session | 400 | empty/whitespace body accepted = `{}` (H/session.ts:159-176) |
| GET | `/session/status` | session.status | G/session.ts:121 | - | - | 200 Record<SessionStatus> | 400 | always forwarded to workspace target (shared/workspace-routing.ts:7) |
| GET | `/session/{sessionID}` | session.get | G/session.ts:132 | - | - | 200 Session | 400,404 |  |
| DELETE | `/session/{sessionID}` | session.delete | G/session.ts:215 | - | - | 200 boolean | 400,404 |  |
| PATCH | `/session/{sessionID}` | session.update | G/session.ts:227 | - | {title?,metadata?,permission?,time?} | 200 Session | 400,404 |  |
| GET | `/session/{sessionID}/children` | session.children | G/session.ts:144 | - | - | 200 Session[] | 400,404 |  |
| GET | `/session/{sessionID}/todo` | session.todo | G/session.ts:156 | - | - | 200 Todo[] | 400,404 |  |
| GET | `/session/{sessionID}/diff` | session.diff | G/session.ts:168 | messageID | - | 200 SnapshotFileDiff[] | 400 |  |
| GET | `/session/{sessionID}/message` | session.messages | G/session.ts:179 | limit,before | - | 200 {info,parts}[] | 400,404 | `limit` absent/0 → all; `before` w/o `limit` → 400; paged response headers `Link`, `X-Next-Cursor`, `Access-Control-Expose-Headers` (H/session.ts:106-143) |
| POST | `/session/{sessionID}/message` | session.prompt | G/session.ts:316 | - | {messageID?,model?,agent?,noReply?,tools?,format?,system?,variant?,parts} | 200 {info,parts} | 400,404 | blocks until turn ends; body written as stream (chunked) `application/json`; compression skipped (H/session.ts:295-309) |
| GET | `/session/{sessionID}/message/{messageID}` | session.message | G/session.ts:191 | - | - | 200 {info,parts} | 400,404 |  |
| DELETE | `/session/{sessionID}/message/{messageID}` | session.deleteMessage | G/session.ts:409 | - | - | 200 boolean | 400,404,409 |  |
| POST | `/session/{sessionID}/fork` | session.fork | G/session.ts:240 | - | {messageID?} | 200 Session | 400,404 | empty body accepted (H/session.ts:218) |
| POST | `/session/{sessionID}/abort` | session.abort | G/session.ts:253 | - | - | 200 boolean | 400 |  |
| POST | `/session/{sessionID}/init` | session.init | G/session.ts:265 | - | {modelID,providerID,messageID} | 200 boolean | 400,404 |  |
| POST | `/session/{sessionID}/share` | session.share | G/session.ts:279 | - | - | 200 Session | 400,404,500 |  |
| DELETE | `/session/{sessionID}/share` | session.unshare | G/session.ts:291 | - | - | 200 Session | 400,404,500 |  |
| POST | `/session/{sessionID}/summarize` | session.summarize | G/session.ts:303 | - | {providerID,modelID,auto?} | 200 boolean | 400,404 |  |
| POST | `/session/{sessionID}/prompt_async` | session.prompt_async | G/session.ts:329 | - | {messageID?,model?,agent?,noReply?,tools?,format?,system?,variant?,parts} | 204 ∅ | 400,404 | 204 immediately; failure → `session.error` event (H/session.ts:311-327) |
| POST | `/session/{sessionID}/command` | session.command | G/session.ts:343 | - | {messageID?,agent?,model?,arguments,command,variant?,parts?} | 200 {info,parts} | 400,404 |  |
| POST | `/session/{sessionID}/shell` | session.shell | G/session.ts:356 | - | {messageID?,agent,model?,command} | 200 {info,parts} | 400,404,409 | 409 SessionBusyError (H/session-errors.ts:10) |
| POST | `/session/{sessionID}/revert` | session.revert | G/session.ts:369 | - | {messageID,partID?} | 200 Session | 400,404,409 |  |
| POST | `/session/{sessionID}/unrevert` | session.unrevert | G/session.ts:383 | - | - | 200 Session | 400,404,409 |  |
| POST | `/session/{sessionID}/permissions/{permissionID}` | permission.respond | G/session.ts:395 | - | {response} | 200 boolean | 400,404 |  |
| DELETE | `/session/{sessionID}/message/{messageID}/part/{partID}` | part.delete | G/session.ts:422 | - | - | 200 boolean | 400,404 |  |
| PATCH | `/session/{sessionID}/message/{messageID}/part/{partID}` | part.update | G/session.ts:433 | - | Part | 200 Part | 400,404 | 400 if body id/messageID/sessionID ≠ path (H/session.ts:397-407) |
| POST | `/sync/start` | sync.start | G/sync.ts:49 | - | - | 200 boolean | 400 |  |
| POST | `/sync/replay` | sync.replay | G/sync.ts:59 | - | {directory,events} | 200 {sessionID} | 400 |  |
| POST | `/sync/steal` | sync.steal | G/sync.ts:71 | - | {sessionID} | 200 {sessionID} | 400 |  |
| POST | `/sync/history` | sync.history.list | G/sync.ts:83 | - | Record<integer> | 200 {id,aggregate_id,seq,type,data}[] | 400 |  |
| POST | `/tui/append-prompt` | tui.appendPrompt | G/tui.ts:56 | - | {text} | 200 boolean | 400 |  |
| POST | `/tui/open-help` | tui.openHelp | G/tui.ts:68 | - | - | 200 boolean | 400 |  |
| POST | `/tui/open-sessions` | tui.openSessions | G/tui.ts:78 | - | - | 200 boolean | 400 |  |
| POST | `/tui/open-themes` | tui.openThemes | G/tui.ts:88 | - | - | 200 boolean | 400 |  |
| POST | `/tui/open-models` | tui.openModels | G/tui.ts:98 | - | - | 200 boolean | 400 |  |
| POST | `/tui/submit-prompt` | tui.submitPrompt | G/tui.ts:108 | - | - | 200 boolean | 400 |  |
| POST | `/tui/clear-prompt` | tui.clearPrompt | G/tui.ts:118 | - | - | 200 boolean | 400 |  |
| POST | `/tui/execute-command` | tui.executeCommand | G/tui.ts:128 | - | {command} | 200 boolean | 400 | alias map; unknown → `command` undefined (H/tui.ts:11,75) |
| POST | `/tui/show-toast` | tui.showToast | G/tui.ts:140 | - | {title?,message,variant,duration?} | 200 boolean | 400 |  |
| POST | `/tui/publish` | tui.publish | G/tui.ts:151 | - | EventTuiPromptAppend \| EventTuiCommandExecute \| EventTuiToastShow \| EventTuiSessionSelect | 200 boolean | 400 |  |
| POST | `/tui/select-session` | tui.selectSession | G/tui.ts:163 | - | {sessionID} | 200 boolean | 400,404 |  |
| GET | `/tui/control/next` | tui.control.next | G/tui.ts:175 | - | - | 200 {path,body} | 400 | long-poll on in-memory queue; no producer in codebase (shared/tui-control.ts:14) |
| POST | `/tui/control/response` | tui.control.response | G/tui.ts:185 | - | {} | 200 boolean | 400 |  |
| GET | `/experimental/workspace/adapter` | experimental.workspace.adapter.list | G/workspace.ts:53 | - | - | 200 {type,name,description}[] | 400 |  |
| GET | `/experimental/workspace` | experimental.workspace.list | G/workspace.ts:63 | - | - | 200 Workspace[] | 400 | `/experimental/workspace*` never proxied (shared/workspace-routing.ts:6) |
| POST | `/experimental/workspace` | experimental.workspace.create | G/workspace.ts:73 | - | {id?,type,branch?,extra?} | 200 Workspace | 400 |  |
| POST | `/experimental/workspace/sync-list` | experimental.workspace.syncList | G/workspace.ts:85 | - | - | 204 ∅ | 400 |  |
| GET | `/experimental/workspace/status` | experimental.workspace.status | G/workspace.ts:95 | - | - | 200 WorkspaceEventConnectionStatus[] | 400 |  |
| DELETE | `/experimental/workspace/{id}` | experimental.workspace.remove | G/workspace.ts:105 | - | - | 200 Workspace | 400 |  |
| POST | `/experimental/workspace/warp` | experimental.workspace.warp | G/workspace.ts:117 | - | {id,sessionID,copyChanges?} | 204 ∅ | 400,404 |  |
| GET | `/pty/{ptyID}/connect` | pty.connect | G/pty.ts:144 | cursor,ticket | - | 200 boolean | 403,404 | WebSocket upgrade (101 at runtime; openapi 200 is nominal), §4 |

### 2.6 v2 routes (`Api` from `packages/protocol` via `packages/server`)

| M | Path | operationId | src | query | body | 2xx | non-2xx | notes |
|---|---|---|---|---|---|---|---|---|
| GET | `/api/health` | v2.health.get | P/health.ts:5 | - | - | 200 {healthy} | 400,401 |  |
| GET | `/api/location` | v2.location.get | P/location.ts:30 | - | - | 200 LocationInfo | 400,401 |  |
| GET | `/api/agent` | v2.agent.list | P/agent.ts:8 | - | - | 200 {location,data} | 400,401 |  |
| GET | `/api/session` | v2.session.list | P/session.ts:109 | limit,order,search,project,subpath,cursor | - | 200 SessionsResponse | 400,401 | also query `directory` (absolute path) and `workspace` (`^wrk`) as **filters**, not location selectors (P/session.ts:25-33,98-104); default page = newest 50; `cursor` = opaque base64url JSON (P/session.ts:65-79) |
| POST | `/api/session` | v2.session.create | P/session.ts:129 | - | {id?,agent?,model?,location?} | 200 {data} | 400,401 |  |
| GET | `/api/session/active` | v2.session.active | P/session.ts:146 | - | - | 200 {data} | 400,401 |  |
| GET | `/api/session/{sessionID}` | v2.session.get | P/session.ts:158 | - | - | 200 {data} | 400,401,404 |  |
| POST | `/api/session/{sessionID}/agent` | v2.session.switchAgent | P/session.ts:173 | - | {agent} | 204 ∅ | 400,401,404 |  |
| POST | `/api/session/{sessionID}/model` | v2.session.switchModel | P/session.ts:189 | - | {model} | 204 ∅ | 400,401,404 |  |
| POST | `/api/session/{sessionID}/prompt` | v2.session.prompt | P/session.ts:205 | - | {id?,prompt,delivery?,resume?} | 200 {data} | 400,401,404,409 |  |
| POST | `/api/session/{sessionID}/compact` | v2.session.compact | P/session.ts:226 | - | - | 204 ∅ | 400,401,404,503 |  |
| POST | `/api/session/{sessionID}/wait` | v2.session.wait | P/session.ts:241 | - | - | 204 ∅ | 400,401,404,503 |  |
| POST | `/api/session/{sessionID}/revert/stage` | v2.session.revert.stage | P/session.ts:256 | - | {messageID,files?} | 200 {data} | 400,401,404,500 |  |
| POST | `/api/session/{sessionID}/revert/clear` | v2.session.revert.clear | P/session.ts:272 | - | - | 204 ∅ | 400,401,404,500 |  |
| POST | `/api/session/{sessionID}/revert/commit` | v2.session.revert.commit | P/session.ts:281 | - | - | 204 ∅ | 400,401,404 |  |
| GET | `/api/session/{sessionID}/context` | v2.session.context | P/session.ts:292 | - | - | 200 {data} | 400,401,404,500 |  |
| GET | `/api/session/{sessionID}/history` | v2.session.history | P/session.ts:307 | limit,after | - | 200 SessionHistory | 400,401,404 |  |
| GET | `/api/session/{sessionID}/event` | v2.session.events | P/session.ts:327 | after | - | 200 `text/event-stream` {id,event,data} | 400,401,404 | Effect StreamSse, §3 |
| POST | `/api/session/{sessionID}/interrupt` | v2.session.interrupt | P/session.ts:345 | - | - | 204 ∅ | 400,401,404 |  |
| GET | `/api/session/{sessionID}/message/{messageID}` | v2.session.message | P/session.ts:360 | - | - | 200 {data} | 400,401,404 |  |
| GET | `/api/session/{sessionID}/message` | v2.session.messages | P/message.ts:26 | limit,order,cursor | - | 200 SessionMessagesResponse | 400,401,404,500 |  |
| GET | `/api/model` | v2.model.list | P/model.ts:10 | - | - | 200 {location,data} | 400,401,503 |  |
| GET | `/api/provider` | v2.provider.list | P/provider.ts:10 | - | - | 200 {location,data} | 400,401,503 |  |
| GET | `/api/provider/{providerID}` | v2.provider.get | P/provider.ts:25 | - | - | 200 {location,data} | 400,401,404,503 |  |
| GET | `/api/integration` | v2.integration.list | P/integration.ts:12 | - | - | 200 {location,data} | 400,401 |  |
| GET | `/api/integration/{integrationID}` | v2.integration.get | P/integration.ts:26 | - | - | 200 {location,data} | 400,401 |  |
| POST | `/api/integration/{integrationID}/connect/key` | v2.integration.connect.key | P/integration.ts:41 | - | {key,label?} | 204 ∅ | 400,401 |  |
| POST | `/api/integration/{integrationID}/connect/oauth` | v2.integration.connect.oauth | P/integration.ts:61 | - | {methodID,inputs,label?} | 200 {location,data} | 400,401 |  |
| GET | `/api/integration/attempt/{attemptID}` | v2.integration.attempt.status | P/integration.ts:82 | - | - | 200 {location,data} | 400,401 |  |
| DELETE | `/api/integration/attempt/{attemptID}` | v2.integration.attempt.cancel | P/integration.ts:114 | - | - | 204 ∅ | 400,401 |  |
| POST | `/api/integration/attempt/{attemptID}/complete` | v2.integration.attempt.complete | P/integration.ts:97 | - | {code?} | 204 ∅ | 400,401 |  |
| PATCH | `/api/credential/{credentialID}` | v2.credential.update | P/credential.ts:8 | - | {label} | 204 ∅ | 400,401 |  |
| DELETE | `/api/credential/{credentialID}` | v2.credential.remove | P/credential.ts:24 | - | - | 204 ∅ | 400,401 |  |
| GET | `/api/permission/request` | v2.permission.request.list | P/permission.ts:23 | - | - | 200 {location,data} | 400,401 |  |
| GET | `/api/permission/saved` | v2.permission.saved.list | P/permission.ts:37 | projectID | - | 200 {data} | 400,401 |  |
| DELETE | `/api/permission/saved/{id}` | v2.permission.saved.remove | P/permission.ts:49 | - | - | 204 ∅ | 400,401 |  |
| POST | `/api/session/{sessionID}/permission` | v2.session.permission.create | P/permission.ts:63 | - | {id?,action,resources,save?,metadata?,source?,agent?} | 200 {data} | 400,401,404 |  |
| GET | `/api/session/{sessionID}/permission` | v2.session.permission.list | P/permission.ts:89 | - | - | 200 {data} | 400,401,404 |  |
| GET | `/api/session/{sessionID}/permission/{requestID}` | v2.session.permission.get | P/permission.ts:104 | - | - | 200 {data} | 400,401,404 |  |
| POST | `/api/session/{sessionID}/permission/{requestID}/reply` | v2.session.permission.reply | P/permission.ts:119 | - | {reply,message?} | 204 ∅ | 400,401,404 |  |
| GET | `/api/fs/read/*` | v2.fs.read | P/fs.ts:22 | - | - | 200 `application/octet-stream` string | 400,401 | raw: path = URL-decoded remainder after `/api/fs/read/`; `content-type` = file mime (server/src/handlers/fs.ts:12-20) |
| GET | `/api/fs/list` | v2.fs.list | P/fs.ts:36 | path | - | 200 {location,data} | 400,401 |  |
| GET | `/api/fs/find` | v2.fs.find | P/fs.ts:50 | query!,type,limit | - | 200 {location,data} | 400,401 |  |
| GET | `/api/command` | v2.command.list | P/command.ts:9 | - | - | 200 {location,data} | 400,401 |  |
| GET | `/api/skill` | v2.skill.list | P/skill.ts:9 | - | - | 200 {location,data} | 400,401 |  |
| GET | `/api/event` | v2.event.subscribe | P/event.ts:35 | - | - | 200 `text/event-stream` V2Event | 400,401 | SSE global, §3 |
| GET | `/api/pty` | v2.pty.list | P/pty.ts:16 | - | - | 200 {location,data} | 400,401 |  |
| POST | `/api/pty` | v2.pty.create | P/pty.ts:30 | - | {command?,args?,cwd?,title?,env?} | 200 {location,data} | 400,401 |  |
| GET | `/api/pty/{ptyID}` | v2.pty.get | P/pty.ts:45 | - | - | 200 {location,data} | 400,401,404 |  |
| PUT | `/api/pty/{ptyID}` | v2.pty.update | P/pty.ts:61 | - | {title?,size?} | 200 {location,data} | 400,401,404 |  |
| DELETE | `/api/pty/{ptyID}` | v2.pty.remove | P/pty.ts:78 | - | - | 204 ∅ | 400,401,404 |  |
| POST | `/api/pty/{ptyID}/connect-token` | v2.pty.connectToken | P/pty.ts:94 | - | - | 200 {location,data} | 400,401,403,404 | requires `x-opencode-ticket: 1` + allowed Origin else 403 ForbiddenError (server/src/handlers/pty.ts:121-125) |
| GET | `/api/pty/{ptyID}/connect` | v2.pty.connect | P/pty.ts:112 | location[directory],location[workspace],cursor,ticket | - | 200 boolean | 400,401,403,404 | WebSocket upgrade (101 at runtime; openapi 200 is nominal), §4 |
| GET | `/api/question/request` | v2.question.request.list | P/question.ts:20 | - | - | 200 {location,data} | 400,401 |  |
| GET | `/api/session/{sessionID}/question` | v2.session.question.list | P/question.ts:37 | - | - | 200 {data} | 400,401,404 |  |
| POST | `/api/session/{sessionID}/question/{requestID}/reply` | v2.session.question.reply | P/question.ts:52 | - | QuestionV2Reply | 204 ∅ | 400,401,404 |  |
| POST | `/api/session/{sessionID}/question/{requestID}/reject` | v2.session.question.reject | P/question.ts:68 | - | - | 204 ∅ | 400,401,404 |  |
| GET | `/api/reference` | v2.reference.list | P/reference.ts:9 | - | - | 200 {location,data} | 400,401 |  |
| POST | `/experimental/project/{projectID}/copy` | v2.projectCopy.create | P/project-copy.ts:25 | - | {strategy,directory,name?} | 200 ProjectCopyCopy | 400 | v2 Api group but NOT under `/api`; v2 auth (JSON 401) though openapi drops 401 |
| DELETE | `/experimental/project/{projectID}/copy` | v2.projectCopy.remove | P/project-copy.ts:36 | - | {directory,force} | 204 ∅ | 400 |  |
| POST | `/experimental/project/{projectID}/copy/refresh` | v2.projectCopy.refresh | P/project-copy.ts:47 | - | - | 204 ∅ | 400 |  |

### 2.7 Error body shapes

| Status | Body | Where | Ref |
|---|---|---|---|
| 400 | `{"name":"BadRequest","data":{"message":string,"kind":"Params"\|"Headers"\|"Query"\|"Body"\|"Payload"}}` | legacy schema rejection | `M/schema-error.ts:31-37` |
| 400 | `{"_tag":"BadRequest"}` | legacy handler `new HttpApiError.BadRequest({})` (e.g. bad `before` cursor, prompt failure, `part.update` mismatch) | openapi component `effect_HttpApiError_BadRequest`; runtime encoding UNKNOWN |
| 400 | `{"_tag":"InvalidRequestError","message","kind"?,"field"?}` | `/api/*` schema rejection; session-ID decode | `protocol/src/errors.ts:3-11`, `S/middleware/session-location.ts:35-39` |
| 400 | `{"_tag":"InvalidCursorError","message"}` | `/api/session`, `/api/session/{id}/message` | `protocol/src/errors.ts:74-78` |
| 400 | `{"name":"<ConfigError name>","data":…}` | config parse defects | `M/error.ts:19-27` |
| 400 | `{"name":"MoveSessionError","data":{"message"}}` | `POST /experimental/control-plane/move-session` | `G/control-plane.ts:9-17` |
| 400/500 | `{"success":false,"error":string}` | `POST /global/upgrade` | `H/global.ts:91-107` |
| 401 | empty + `www-authenticate: Basic realm="Secure Area"` | legacy, `/doc`, UI | `M/authorization.ts:14,48-51,92-97` |
| 401 | `{"_tag":"UnauthorizedError","message":"Authentication required"}` + `www-authenticate` | v2 | `S/middleware/authorization.ts:52-58` |
| 403 | `{"_tag":"PtyForbiddenError","message":"Invalid PTY connect token request"}` | legacy `POST /pty/{id}/connect-token` | `H/pty.ts:146-147` |
| 403 | `{"_tag":"ForbiddenError","message":"Invalid PTY connect token request"}` | v2 connect-token | `S/handlers/pty.ts:121-125` |
| 403/404/400 | empty body | WebSocket connect pre-upgrade failures | `H/pty.ts:187-200`, `S/handlers/pty.ts:143-156` |
| 404 | `{"name":"NotFoundError","data":{"message"}}` | legacy storage not-found (`ApiNotFoundError`) | `HA/errors.ts:178-193`, `H/session-errors.ts:6-8` |
| 404 | `{"_tag":"<X>NotFoundError",<id field>,"message"}` | `SessionNotFoundError{sessionID}`, `MessageNotFoundError{sessionID,messageID}`, `PtyNotFoundError{ptyID}`, `PermissionNotFoundError{requestID}`, `QuestionNotFoundError{requestID}`, `ProviderNotFoundError{providerID}`, `McpServerNotFoundError{name}`, `ProjectNotFoundError{projectID}` | `HA/errors.ts:91-176`, `protocol/src/errors.ts:46-111` |
| 409 | `{"_tag":"SessionBusyError","sessionID","message":"Session is busy: <id>"}` | legacy shell/revert/unrevert/deleteMessage | `H/session-errors.ts:10-21` |
| 409 | `{"_tag":"ConflictError","message","resource"?}` | `POST /api/session/{id}/prompt` | `protocol/src/errors.ts:19-26` |
| 500 | `{"name":"UnknownError","data":{"message","ref"}}` | defects | `M/error.ts:29-40` |
| 500 | `{"_tag":"InternalServerError"}` | legacy share/unshare failure | `H/session.ts` share/unshare (`HttpApiError.InternalServerError`); openapi component `effect_HttpApiError_InternalServerError` |
| 500 | `text/plain` `Workspace not found: <id>` | legacy routing | `M/workspace-routing.ts:102-107` |
| 503 | `{"_tag":"ServiceUnavailableError","message","service"?}` | `/api/model`, `/api/provider*`, compact/wait | `protocol/src/errors.ts:28-35` |
| 503 | `text/plain` | workspace proxy not syncing / fence wait failure | `M/workspace-routing.ts:121-143` |

Status codes come from `httpApiStatus` on each error class. Every TaggedError body includes `_tag` (openapi components show `_tag` as required).

### 2.8 Rust mapping (§2)

- **Types.**
  - Generate types from `packages/sdk/openapi.json` `components.schemas` with `typify`. The file is OpenAPI 3.1 / JSON Schema 2020-12. It contains 174 `anyOf`, 25 numeric `exclusiveMinimum`, 1 `prefixItems`, 1 `patternProperties`, and no `discriminator`.
  - `progenitor` is client-only and weak on 3.1, so do not use it for the server.
  - Hand-write the tagged unions that matter on the wire: events (§3) and `Part` / message unions, using `#[serde(tag="type")]`.
  - Serialization rules: never emit `null` for absent optionals (`#[serde(skip_serializing_if="Option::is_none")]`); accept `null` on input as absent.
- **Routes.** Write axum handlers manually per the tables above. Optionally verify them in CI by diffing a generated route list against `openapi.json` paths and operationIds.
- **`/doc`.** Serve the original JSON verbatim. Embed `packages/sdk/openapi.json` after stripping `x-codeSamples` so that byte-level parity holds. Do not regenerate it with `utoipa`; regeneration would drift.
- **Errors.** Use two error enums with distinct `IntoResponse` implementations, selected by router layer: `LegacyError` (NamedError / built-in `_tag`) and `V2Error` (TaggedError).
- **Query.** Use `serde_qs` or a hand-rolled parser for `location[directory]`. Write a custom `QueryBool` (only `"true"`/`"false"`) and a `NumberFromString` newtype. Ignore unknown query keys; whether Effect rejects them is UNKNOWN, and being lenient is client-safe.

---

## 3. Event streams (SSE)

### 3.1 Endpoints

| Path | Scope | Source of events | First frame | Heartbeat | Ends when | Ref |
|---|---|---|---|---|---|---|
| `GET /event` | one directory (+ workspace) after §1.7 routing; may be proxied to a remote workspace | `EventV2Bridge.listen`. Filter: `location.directory === instance.directory && (location.workspaceID === undefined \|\| location.workspaceID === workspaceID)`. Events without `location` are dropped. Also GlobalBus `server.instance.disposed` for the same directory. Unbounded queue. | `{"id":"evt_…","type":"server.connected","properties":{}}` | `{"id","type":"server.heartbeat","properties":{}}` every 10 s; the first comes after 10 s (`tick` + `drop(1)`) | after emitting `server.instance.disposed` for its directory (`takeUntil`); client disconnect | `H/event.ts:25-87` |
| `GET /global/event` | all | Every GlobalBus `"event"` emission: EventV2 mirror + sync mirror (`opencode/src/event-v2-bridge.ts:35-60`) and direct emits (§3.4 column "emitted via") | `{"payload":{"id","type":"server.connected","properties":{}}}` | `{"payload":{"id","type":"server.heartbeat","properties":{}}}` every 10 s, first after 10 s | client disconnect only | `H/global.ts:25-58` |
| `GET /api/event` | all locations (not filtered; test `httpapi-v2-location.test.ts:108-125`) | `EventV2.allBounded(events, 256)`: dropping queue of capacity 256. On overflow the queue fails with `SubscriberOverflowError` and the stream ends. | `{"id","type":"server.connected","data":{}}` (no `location`) | SSE comment line `: heartbeat` every 15 s. assumption: the first one is immediate, since `Stream.tick` without `drop(1)` emits at t=0; this is inferred from the `drop(1)` idiom elsewhere. | overflow; encode failure (UNKNOWN, see D3); disconnect | `S/handlers/event.ts:9-51`, `core/src/event.ts:152-164` |
| `GET /api/session/{sessionID}/event?after=<seq>` | one session | Durable session events after aggregate `seq` (exclusive), then live durable events | none documented | UNKNOWN | UNKNOWN | `P/session.ts:327-343`, `S/handlers/session.ts:357-363` |

Response headers on the three hand-built streams (`H/event.ts:77-84`, `H/global.ts:48-55`, `S/handlers/event.ts:40-46`):
- `content-type: text/event-stream`
- `Cache-Control: no-cache, no-transform`
- `X-Accel-Buffering: no`
- `X-Content-Type-Options: nosniff`

Asserted by test `httpapi-event.test.ts:52-56`.

### 3.2 Framing

- **JSON frames.** Each event is encoded by Effect `Sse.encode()` from `{_tag:"Event", event:"message", id:undefined, data: JSON.stringify(x)}` (`H/event.ts:12-19`, `H/global.ts:16-23`, `S/handlers/event.ts:11-18`). The wire frame is `data: <json>\n\n`, with no `event:`, `id:`, or `retry:` lines. Evidence: the test parses a raw chunk with `.replace(/^data: /, "")` (`opencode/test/server/httpapi-event.test.ts:23`). Compact `JSON.stringify` output has no newlines, so each event is exactly one `data:` line.
- **`/api/event` heartbeat.** It is the raw text `: heartbeat\n\n`, merged after SSE encoding (`S/handlers/event.ts:37-39`).
- **`/api/session/{id}/event`.** Uses Effect `HttpApiSchema.StreamSse({data: SessionEvent.Durable})`. openapi documents each message as `{id, event, data}` where `data` is a JSON string of `SessionDurableEvent`. `x-effect-stream: {encoding:"sse", failureEvent:"effect/httpapi/stream/failure", causeSchema: Cause[]}`. The exact `id:` and `event:` values and the failure framing are UNKNOWN.
- **Consumer tolerance.**
  - The generated promise client reads only `data:` lines, joins them with `\n`, and caps the buffer at 1 MiB (`client/src/generated/client.ts:192-246`).
  - The hey-api SDK client parses `data:`, `event:`, `id:`, and `retry:`, and sends `Last-Event-ID` when it has one (`sdk/js/src/v2/gen/core/serverSentEvents.gen.ts:96-111,166-175`).

### 3.3 Envelopes

| Stream | Envelope |
|---|---|
| `/event` | `{ "id": "evt_…", "type": string, "properties": <data> }` (`H/event.ts:40`) |
| `/global/event` | `{ "directory"?: string, "project"?: string, "workspace"?: string, "payload": P }`. `directory` is `"global"` for global emits. `P` is either `{id,type,properties}` or a sync event (§3.5). `payload.id` is auto-filled if missing: `syncEvent.id`, else a new `evt_` ascending ID (`opencode/src/bus/global.ts:14-19`). |
| `/api/event` | `{ "id": "evt_…", "type": string, "data": <data>, "metadata"?: object, "durable"?: {"aggregateID","seq","version"}, "location"?: {"directory","workspaceID"?} }` (`schema/src/event.ts:53-61`, `P/event.ts:8-27`) |
| TUI RPC `global.event` | same object as a `/global/event` frame (`opencode/src/cli/tui/worker.ts:24-26`) |

Event IDs: `evt_` + ascending identifier (`schema/src/event.ts:9-12`).

### 3.4 Event catalog

Membership is defined in `schema/src/event-manifest.ts`:
- `Definitions` / `Latest` = all types; this is the legacy and global set (`:63-83`).
- `ServerDefinitions` = foundation + feature + todo (`:57-61`).
- The data schema of each type is the openapi component `Event<Name>.properties`.

Column "/api/event runtime" values:
- `Y` = in `ServerDefinitions`, encodable.
- `N` = not encodable by `OpenCodeEvent`. Outcome is UNKNOWN: expected `encodeUnknownSync` to throw, which would end the stream (§8).

Column "emitted via" values:
- `EventV2`: published through `EventV2` / `EventV2Bridge`. Reaches `/global/event` always, `/event` when the location matches, `/api/event` when `Y`.
- `GlobalBus direct`: reaches only `/global/event` (and the TUI RPC). `server.instance.disposed` is additionally forwarded on `/event`.
- `installation.update-available`, `workspace.ready`, `workspace.failed`: schema-only. A grep of `opencode/src` found no emitter. Status UNKNOWN.

| type | durable | data fields (`?`=optional) | openapi component | def src | /api/event runtime | emitted via |
|---|---|---|---|---|---|---|
| `models-dev.refreshed` | - | {} | EventModels-devRefreshed | schema/src/models-dev.ts:6 | Y | EventV2 |
| `integration.updated` | - | {} | EventIntegrationUpdated | schema/src/integration.ts:80 | Y | EventV2 |
| `integration.connection.updated` | - | integrationID | EventIntegrationConnectionUpdated | schema/src/integration.ts:84 | Y | EventV2 |
| `catalog.updated` | - | {} | EventCatalogUpdated | schema/src/catalog.ts:5 | Y | EventV2 |
| `session.created` | v1 | sessionID, info | EventSessionCreated | schema/src/v1/session.ts:573 | Y | EventV2 |
| `session.updated` | v1 | sessionID, info | EventSessionUpdated | schema/src/v1/session.ts:581 | Y | EventV2 |
| `session.deleted` | v1 | sessionID, info | EventSessionDeleted | schema/src/v1/session.ts:589 | Y | EventV2 |
| `message.updated` | v1 | sessionID, info | EventMessageUpdated | schema/src/v1/session.ts:597 | Y | EventV2 |
| `message.removed` | v1 | sessionID, messageID | EventMessageRemoved | schema/src/v1/session.ts:605 | Y | EventV2 |
| `message.part.updated` | v1 | sessionID, part, time | EventMessagePartUpdated | schema/src/v1/session.ts:613 | Y | EventV2 |
| `message.part.removed` | v1 | sessionID, messageID, partID | EventMessagePartRemoved | schema/src/v1/session.ts:622 | Y | EventV2 |
| `session.next.agent.switched` | v1 | timestamp, sessionID, messageID, agent | EventSessionNextAgentSwitched | schema/src/session-event.ts:55 | Y | EventV2 |
| `session.next.model.switched` | v1 | timestamp, sessionID, messageID, model | EventSessionNextModelSwitched | schema/src/session-event.ts:66 | Y | EventV2 |
| `session.next.moved` | v1 | timestamp, sessionID, location, subdirectory? | EventSessionNextMoved | schema/src/session-event.ts:77 | Y | EventV2 |
| `session.next.prompted` | v1 | timestamp, sessionID, messageID, prompt, delivery | EventSessionNextPrompted | schema/src/session-event.ts:88 | Y | EventV2 |
| `session.next.prompt.admitted` | v1 | timestamp, sessionID, messageID, prompt, delivery | EventSessionNextPromptAdmitted | schema/src/session-event.ts:95 | Y | EventV2 |
| `session.next.context.updated` | v1 | timestamp, sessionID, messageID, text | EventSessionNextContextUpdated | schema/src/session-event.ts:102 | Y | EventV2 |
| `session.next.synthetic` | v1 | timestamp, sessionID, messageID, text | EventSessionNextSynthetic | schema/src/session-event.ts:113 | Y | EventV2 |
| `session.next.shell.started` | v1 | timestamp, sessionID, messageID, callID, command | EventSessionNextShellStarted | schema/src/session-event.ts:125 | Y | EventV2 |
| `session.next.shell.ended` | v1 | timestamp, sessionID, callID, output | EventSessionNextShellEnded | schema/src/session-event.ts:137 | Y | EventV2 |
| `session.next.step.started` | v1 | timestamp, sessionID, assistantMessageID, agent, model, snapshot? | EventSessionNextStepStarted | schema/src/session-event.ts:150 | Y | EventV2 |
| `session.next.step.ended` | v2 | timestamp, sessionID, assistantMessageID, finish, cost, tokens, snapshot?, files? | EventSessionNextStepEnded | schema/src/session-event.ts:163 | Y | EventV2 |
| `session.next.step.failed` | v2 | timestamp, sessionID, assistantMessageID, error | EventSessionNextStepFailed | schema/src/session-event.ts:186 | Y | EventV2 |
| `session.next.text.started` | v1 | timestamp, sessionID, assistantMessageID, textID | EventSessionNextTextStarted | schema/src/session-event.ts:199 | Y | EventV2 |
| `session.next.text.delta` | - | timestamp, sessionID, assistantMessageID, textID, delta | EventSessionNextTextDelta | schema/src/session-event.ts:211 | Y | EventV2 |
| `session.next.text.ended` | v1 | timestamp, sessionID, assistantMessageID, textID, text | EventSessionNextTextEnded | schema/src/session-event.ts:222 | Y | EventV2 |
| `session.next.reasoning.started` | v1 | timestamp, sessionID, assistantMessageID, reasoningID, providerMetadata? | EventSessionNextReasoningStarted | schema/src/session-event.ts:236 | Y | EventV2 |
| `session.next.reasoning.delta` | - | timestamp, sessionID, assistantMessageID, reasoningID, delta | EventSessionNextReasoningDelta | schema/src/session-event.ts:249 | Y | EventV2 |
| `session.next.reasoning.ended` | v1 | timestamp, sessionID, assistantMessageID, reasoningID, text, providerMetadata? | EventSessionNextReasoningEnded | schema/src/session-event.ts:260 | Y | EventV2 |
| `session.next.tool.input.started` | v1 | timestamp, sessionID, assistantMessageID, callID, name | EventSessionNextToolInputStarted | schema/src/session-event.ts:282 | Y | EventV2 |
| `session.next.tool.input.delta` | - | timestamp, sessionID, assistantMessageID, callID, delta | EventSessionNextToolInputDelta | schema/src/session-event.ts:293 | Y | EventV2 |
| `session.next.tool.input.ended` | v1 | timestamp, sessionID, assistantMessageID, callID, text | EventSessionNextToolInputEnded | schema/src/session-event.ts:302 | Y | EventV2 |
| `session.next.tool.called` | v1 | timestamp, sessionID, assistantMessageID, callID, tool, input, provider | EventSessionNextToolCalled | schema/src/session-event.ts:313 | Y | EventV2 |
| `session.next.tool.progress` | v1 | timestamp, sessionID, assistantMessageID, callID, structured, content | EventSessionNextToolProgress | schema/src/session-event.ts:332 | Y | EventV2 |
| `session.next.tool.success` | v1 | timestamp, sessionID, assistantMessageID, callID, structured, content, outputPaths?, result?, provider | EventSessionNextToolSuccess | schema/src/session-event.ts:343 | Y | EventV2 |
| `session.next.tool.failed` | v1 | timestamp, sessionID, assistantMessageID, callID, error, result?, provider | EventSessionNextToolFailed | schema/src/session-event.ts:360 | Y | EventV2 |
| `session.next.retried` | v1 | timestamp, sessionID, attempt, error | EventSessionNextRetried | schema/src/session-event.ts:388 | Y | EventV2 |
| `session.next.compaction.started` | v1 | timestamp, sessionID, messageID, reason | EventSessionNextCompactionStarted | schema/src/session-event.ts:400 | Y | EventV2 |
| `session.next.compaction.delta` | - | timestamp, sessionID, messageID, text | EventSessionNextCompactionDelta | schema/src/session-event.ts:411 | Y | EventV2 |
| `session.next.compaction.ended` | v1 | timestamp, sessionID, messageID, reason, text, recent | EventSessionNextCompactionEnded | schema/src/session-event.ts:421 | Y | EventV2 |
| `session.next.revert.staged` | v1 | timestamp, sessionID, revert | EventSessionNextRevertStaged | schema/src/session-event.ts:436 | Y | EventV2 |
| `session.next.revert.cleared` | v1 | timestamp, sessionID | EventSessionNextRevertCleared | schema/src/session-event.ts:440 | Y | EventV2 |
| `session.next.revert.committed` | v1 | timestamp, sessionID, messageID | EventSessionNextRevertCommitted | schema/src/session-event.ts:442 | Y | EventV2 |
| `message.part.delta` | - | sessionID, messageID, partID, field, delta | EventMessagePartDelta | schema/src/v1/session.ts:633 | N | EventV2 |
| `session.diff` | - | sessionID, diff | EventSessionDiff | schema/src/v1/session.ts:644 | N | EventV2 |
| `session.error` | - | sessionID?, error? | EventSessionError | schema/src/v1/session.ts:652 | N | EventV2 |
| `installation.updated` | - | version | EventInstallationUpdated | schema/src/installation-event.ts:7 | N | GlobalBus direct (opencode/src/server/routes/instance/httpapi/handlers/global.ts:108) |
| `installation.update-available` | - | version | EventInstallationUpdate-available | schema/src/installation-event.ts:14 | N | no emitter found (UNKNOWN) |
| `file.edited` | - | file | EventFileEdited | schema/src/filesystem.ts:9 | Y | EventV2 |
| `reference.updated` | - | {} | EventReferenceUpdated | schema/src/reference.ts:8 | Y | EventV2 |
| `permission.v2.asked` | - | id, sessionID, action, resources, save?, metadata?, source? | EventPermissionV2Asked | schema/src/permission.ts:43 | Y | EventV2 |
| `permission.v2.replied` | - | sessionID, requestID, reply | EventPermissionV2Replied | schema/src/permission.ts:45 | Y | EventV2 |
| `plugin.added` | - | id | EventPluginAdded | schema/src/plugin.ts:10 | Y | EventV2 |
| `project.directories.updated` | - | projectID | EventProjectDirectoriesUpdated | schema/src/project-directories.ts:7 | Y | EventV2 |
| `file.watcher.updated` | - | file, event | EventFileWatcherUpdated | schema/src/filesystem-watcher.ts:7 | Y | EventV2 |
| `pty.created` | - | info | EventPtyCreated | schema/src/pty.ts:34 | Y | EventV2 |
| `pty.updated` | - | info | EventPtyUpdated | schema/src/pty.ts:35 | Y | EventV2 |
| `pty.exited` | - | id, exitCode | EventPtyExited | schema/src/pty.ts:36 | Y | EventV2 |
| `pty.deleted` | - | id | EventPtyDeleted | schema/src/pty.ts:37 | Y | EventV2 |
| `question.v2.asked` | - | id, sessionID, questions, tool? | EventQuestionV2Asked | schema/src/question.ts:70 | Y | EventV2 |
| `question.v2.replied` | - | sessionID, requestID, answers | EventQuestionV2Replied | schema/src/question.ts:72 | Y | EventV2 |
| `question.v2.rejected` | - | sessionID, requestID | EventQuestionV2Rejected | schema/src/question.ts:80 | Y | EventV2 |
| `todo.updated` | - | sessionID, todos | EventTodoUpdated | schema/src/session-todo.ts:19 | Y | EventV2 |
| `lsp.updated` | - | {} | EventLspUpdated | schema/src/lsp-event.ts:5 | N | EventV2 |
| `permission.asked` | - | id, sessionID, permission, patterns, metadata, always, tool? | EventPermissionAsked | schema/src/v1/permission.ts:61 | N | EventV2 |
| `permission.replied` | - | sessionID, requestID, reply | EventPermissionReplied | schema/src/v1/permission.ts:63 | N | EventV2 |
| `tui.prompt.append` | - | text | Event.tui.prompt.append | schema/src/tui-event.ts:11 | N | EventV2 |
| `tui.command.execute` | - | command | Event.tui.command.execute | schema/src/tui-event.ts:14 | N | EventV2 |
| `tui.toast.show` | - | title?, message, variant, duration? | Event.tui.toast.show | schema/src/tui-event.ts:41 | N | EventV2 |
| `tui.session.select` | - | sessionID | Event.tui.session.select | schema/src/tui-event.ts:53 | N | EventV2 |
| `mcp.tools.changed` | - | server | EventMcpToolsChanged | schema/src/mcp-event.ts:7 | N | EventV2 |
| `mcp.browser.open.failed` | - | mcpName, url | EventMcpBrowserOpenFailed | schema/src/mcp-event.ts:14 | N | EventV2 |
| `command.executed` | - | name, sessionID, arguments, messageID | EventCommandExecuted | schema/src/v1/legacy-event.ts:9 | N | EventV2 |
| `project.updated` | - | id, worktree, vcs?, name?, icon?, commands?, time, sandboxes | EventProjectUpdated | schema/src/project.ts:43 | N | GlobalBus direct (opencode/src/project/project.ts:135) |
| `session.status` | - | sessionID, status | EventSessionStatus | schema/src/session-status-event.ts:36 | N | EventV2 |
| `session.idle` | - | sessionID | EventSessionIdle | schema/src/session-status-event.ts:45 | N | EventV2 |
| `question.asked` | - | id, sessionID, questions, tool? | EventQuestionAsked | schema/src/v1/question.ts:58 | N | EventV2 |
| `question.replied` | - | sessionID, requestID, answers | EventQuestionReplied | schema/src/v1/question.ts:59 | N | EventV2 |
| `question.rejected` | - | sessionID, requestID | EventQuestionRejected | schema/src/v1/question.ts:60 | N | EventV2 |
| `session.compacted` | - | sessionID | EventSessionCompacted | schema/src/session-compaction-event.ts:7 | N | EventV2 |
| `vcs.branch.updated` | - | branch? | EventVcsBranchUpdated | schema/src/vcs-event.ts:8 | N | EventV2 |
| `workspace.ready` | - | name | EventWorkspaceReady | schema/src/workspace-event.ts:14 | N | no emitter found (UNKNOWN) |
| `workspace.failed` | - | message | EventWorkspaceFailed | schema/src/workspace-event.ts:21 | N | no emitter found (UNKNOWN) |
| `workspace.status` | - | workspaceID, status | EventWorkspaceStatus | schema/src/workspace-event.ts:28 | N | GlobalBus direct (opencode/src/control-plane/workspace.ts:174) |
| `worktree.ready` | - | name, branch? | EventWorktreeReady | schema/src/worktree-event.ts:8 | N | GlobalBus direct (opencode/src/worktree/index.ts:268) |
| `worktree.failed` | - | message | EventWorktreeFailed | schema/src/worktree-event.ts:16 | N | GlobalBus direct (opencode/src/worktree/index.ts:241) |
| `server.connected` | - | {} | EventServerConnected | schema/src/server-event.ts:5 | Y | synthetic per stream |
| `global.disposed` | - | {} | EventGlobalDisposed | schema/src/server-event.ts:6 | N | GlobalBus direct (opencode/src/server/global-lifecycle.ts:7) |
| `server.instance.disposed` | - | directory | EventServerInstanceDisposed | opencode/src/server/event.ts:8 | N | GlobalBus direct (opencode/src/project/instance-store.ts:81) |

Not in any schema but present on the wire: `server.heartbeat` (§3.1).

### 3.5 Sync (durable) events on `/global/event`

For every EventV2 event with a `durable` field, the bridge emits a **second** GlobalBus frame (`opencode/src/event-v2-bridge.ts:45-60`):

```json
{"directory":"…","project":"…","workspace":"…",
 "payload":{"type":"sync","syncEvent":{"id":"evt_…","type":"<type>.<version>","seq":<n>,"aggregateID":"<id>","data":{…}},"id":"<same as syncEvent.id>"}}
```

Durable types and versions are taken from the openapi `SyncEvent*` components (`G/global.ts:17-34`):
- 7 `session.*` / `message.*` v1 types.
- 31 `session.next.*` types, all v1 except `session.next.step.ended.2` and `session.next.step.failed.2`.

Consumers:
- The web app skips `payload.type==="sync"` (`app/src/context/server-sdk.tsx:283`).
- The control-plane workspace sync loop replays them and ignores `server.heartbeat` (`opencode/src/control-plane/workspace.ts:399-420`).

### 3.6 Reconnection semantics

- **No server-side resume.** None of the hand-built streams sends `id:` or `retry:`, so `Last-Event-ID` is never populated, and no replay exists for `/event`, `/global/event`, or `/api/event`. Only `/api/session/{id}/event?after=` and `GET /api/session/{id}/history?after=&limit≤100` (`P/session.ts:87-92`) support resume by sequence.
- **Client reconnect behavior:**
  - The web app reconnects 250 ms after any stream end or error (`app/src/context/server-sdk.tsx:220,308`).
  - The TUI uses exponential backoff (`tui/src/context/sdk.tsx:82-118`).
  - The hey-api client defaults to a 3000 ms retry, doubling up to 30 s (`sdk/js/src/v2/gen/core/serverSentEvents.gen.ts:96,229-230`).
- **Instance disposal.** `/event` is closed by the server on `server.instance.disposed`. Clients must reconnect, which re-boots the instance.
- **Server ordering guarantee.** The listener is registered before the first frame is written, so no events published after connect are lost (`H/event.ts:29-33`, `S/handlers/event.ts:32-34`).

### 3.7 Rust mapping (§3)

- **Bus.** Use a single `tokio::sync::broadcast` for the internal EventV2, plus a GlobalBus `broadcast` carrying `GlobalEvent` frames.
  - Per-subscriber unbounded forwarding for `/event` and `/global/event`: wrap with an `mpsc::unbounded` task.
  - `/api/event` needs capacity-256 drop-then-close semantics: on `RecvError::Lagged`, end the stream.
- **Responses.** `axum::response::sse::Sse` with `Event::default().data(json)` produces `data: …\n\n`. Do not set `.event("message")`. Implement heartbeats as data events on legacy streams and as `Event::default().comment("heartbeat")` on `/api/event`.
  - assumption: axum writes `:heartbeat` vs `: heartbeat`; verify the exact bytes, or write a custom `Stream<Item=Bytes>` body to guarantee `: heartbeat\n\n`.
  - Disable compression and buffering for these routes.
- **Event types.** Generate one Rust `enum ServerEvent { #[serde(rename="session.created")] SessionCreated{…}, … }` with `#[serde(tag="type", content=…)]`. The content key is `properties` (legacy) or `data` (v2), so use two wrapper serializers over the same payload enum.

---

## 4. WebSocket endpoints (PTY)

There are only two WebSocket routes. A third WebSocket path exists only in the workspace proxy, which relays any upgrade on proxied routes (§1.8).

| Route | Contract | Handler |
|---|---|---|
| `GET /pty/{ptyID}/connect` (legacy) | `G/pty.ts:140-172`; constants `opencode/src/server/shared/pty-ticket.ts:1-4` | `H/pty.ts:163-272` |
| `GET /api/pty/{ptyID}/connect` (v2, openapi `x-websocket: true`) | `P/pty.ts:9-12,109-133` | `S/handlers/pty.ts:140-220` |

### 4.1 Handshake (in order)

1. **Auth.** A non-empty `?ticket=` skips credential auth. Otherwise Basic or `auth_token` is required when a password is set (§1.5).
2. **Origin check.** `isAllowedRequestOrigin(Origin, Host)`. On failure: `403` with an empty body (`H/pty.ts:187`, `S/handlers/pty.ts:143-144`).
3. **Ticket.** If a ticket is present, `tickets.consume({ticket, ptyID, directory, workspaceID})` runs; it is single-use and its scope must equal the issuance scope. On failure: `403` empty (`H/pty.ts:189-191`; core `core/src/pty/ticket.ts:27-31,48-50`).
4. **Existence.** The PTY must exist. Legacy also requires `status === "running"`. Otherwise `404` empty (`H/pty.ts:193-197`, `S/handlers/pty.ts:151-156`).
5. **Query decode.** Legacy only: query decode against `{directory?, workspace?, cursor?}`. On failure: `400` empty (`H/pty.ts:199-200`).
6. **Cursor.** `cursor` = `Number(q)` if it is a safe integer ≥ -1, else ignored (`H/pty.ts:201-205`, `S/handlers/pty.ts:158-162`). Semantics (`core/src/pty.ts:53-54,272-283`):
   - `-1` = tail from the current end.
   - absent = full retained buffer.
   - `n` = replay from absolute offset `n`, clamped at 0.
7. **Upgrade.** If the server is closing: accept, then close `1001 "server closing"` (`H/pty.ts:216-220`).

### 4.2 Frames

| Direction | Frame | Ref |
|---|---|---|
| server→client | Replay: **text** frames, each ≤ 65536 UTF-16 code units (`REPLAY_CHUNK`) | `core/src/pty/protocol.ts:13,23-27`, `H/pty.ts:243` |
| server→client | One **binary** control frame: byte `0x00` + UTF-8 JSON `{"cursor":<absolute end offset>}`, sent after the replay and before live data | `core/src/pty/protocol.ts:15-21`, `H/pty.ts:244` |
| server→client | Live output: **text** frames (raw terminal output strings) | `core/src/pty.ts:20`, `H/pty.ts:229` |
| client→server | Text, or binary decoded as strict UTF-8 (invalid input is dropped silently). Written verbatim to the PTY. There are no in-band control messages; resize goes through `PUT /pty/{id}` or `PUT /api/pty/{id}` with `{size:{rows,cols}}` (positive ints). | `core/src/pty/protocol.ts:29-37`, `H/pty.ts:259-262` |
| server close | `1000` when the PTY process ends (`onEnd`) | `H/pty.ts:230` |
| server close | `4404 "session not found"`: legacy, PTY missing or exited at attach time | `H/pty.ts:234-239` |
| server close | v2: `4404 "session not found"` if missing, `4404 "session exited"` if exited | `S/handlers/pty.ts:186-192` |
| server close | `1001 "server closing"` on shutdown | `HA/websocket-tracker.ts:5` |

**Ordering.** All outbound frames go through one FIFO queue, so the replay, the cursor frame, live data, and the close frame keep their order (`H/pty.ts:222-253`). Reader and writer race; whichever ends first detaches.

**Cursor unit.** The cursor is JS string length, i.e. UTF-16 code units: `session.cursor += chunk.length` (`core/src/pty.ts:205`). The web client advances its own cursor by `data.length` (`app/src/components/terminal.tsx:667`). Retained buffer: `2*1024*1024` UTF-16 units (`core/src/pty.ts:14`).

### 4.3 Tickets

| Endpoint | Requirements | Response | Ref |
|---|---|---|---|
| `POST /pty/{id}/connect-token` | header `x-opencode-ticket: 1` (which forces a CORS preflight); allowed origin; PTY running | `{"ticket":"<uuid v4>","expires_in":60}` | `H/pty.ts:144-150` |
| `POST /api/pty/{id}/connect-token` | same | `{location, data:{ticket,expires_in}}` | `S/handlers/pty.ts:115-139` |

Ticket store: TTL 60 s, capacity 10 000, single use, scoped to `(ptyID, directory, workspaceID)` (`core/src/pty/ticket.ts:9-10,43-46,27-31`). Tests: `httpapi-listen.test.ts:388-426`.

Web client usage (`app/src/utils/terminal-websocket-url.ts:15-34`):
- URL: `ws(s)://…/pty/<id>/connect?directory=<dir>&cursor=<n>[&ticket=…]` (v1 mode).
- Without a ticket: `auth_token=<base64>` when a password is set and the client is not same-origin.
- Reconnect: backoff `250·2^min(n,4)` ms, capped at 4 s; no retry on close code 1000 (`app/src/components/terminal.tsx:588-592,694`).

### 4.4 Rust mapping (§4)

- **Upgrade.** Use `axum::extract::ws::WebSocketUpgrade`. Do all pre-checks (403/404/400 with empty bodies) before calling `on_upgrade`.
- **Writer.** Use a single writer task fed by an `mpsc` queue. Send replay chunks as `Message::Text`, the control frame as `Message::Binary([0x00] ++ json)`, and closes as `Message::Close(Some(CloseFrame{code:4404,…}))`.
- **Cursor.** Store PTY output as `String` and maintain a UTF-16 length counter (`s.encode_utf16().count()`). Slice the replay at UTF-16 offsets, keeping surrogate pairs in mind. Chunk at 65536 UTF-16 units to match.
- **Tickets.** Use `moka::sync::Cache` with a 60 s TTL and capacity 10k. Use `remove_if`-style atomic consume.

---

## 5. Static assets, web UI, proxying

**Routing.** Every unmatched request, of any method and path, goes to `serveUIEffect` (`opencode/src/server/shared/ui.ts:77-107`). Router-level auth applies, except for the public manifest paths (§1.5).

**Embedded mode** (release builds). `opencode/script/build.ts:26-50` builds `packages/app` and maps every file in `app/dist` (excluding `*.map`) into the virtual module `opencode-web-ui.gen.ts` as `{ "<relative path>": <bunfs file path> }`. The module is skipped with `--skip-embed-web-ui`.
- **Lookup.** The request path has its leading `/` removed and is looked up in the map. A miss falls back to `index.html` (SPA fallback, status 200). If neither exists, the response is `404 {"error":"Not Found"}` (`ui.ts:50-52,63-75`).
- **Headers.** `content-type` comes from `FSUtil.mimeType(file)`. HTML responses also get `content-security-policy`:

  ```
  default-src 'self'; script-src 'self' 'wasm-unsafe-eval'[ 'sha256-<b64 sha256 of inline <script id="oc-theme-preload-script">>']; style-src 'self' 'unsafe-inline'; img-src 'self' data: https: blob:; font-src 'self' data:; media-src 'self' data:; connect-src * data: blob:
  ```

  Sources: `ui.ts:11-21,54-61`.
- **Disable switch.** `OPENCODE_DISABLE_EMBEDDED_WEB_UI` disables embedded mode (`ui.ts:43-48`).

**Proxy mode** (no embedded bundle, e.g. dev builds). The request is forwarded to `https://app.opencode.ai<path>` with the same method and body (`ui.ts:9,39-41,87-92`).
- **Request headers.** Sanitized via `ProxyUtil.headers`, with `host: app.opencode.ai`.
- **Response headers.** `content-encoding`, `content-length`, and `transfer-encoding` are dropped (`ui.ts:29-37`).
- **HTML.** Buffered, with CSP recomputed.
- **Everything else.** Streamed, with the base CSP.
- Tests: `httpapi-ui.test.ts:187-300`.

**Other proxying.**
- Remote-workspace proxy: §1.8.
- Auxiliary non-API listeners started on demand by auth flows (not part of the API, listed for completeness):
  - MCP OAuth callback: `127.0.0.1:19876/mcp/oauth/callback` (`opencode/src/mcp/oauth-provider.ts:11-12`, `opencode/src/mcp/oauth-callback.ts:6,124-126`).
  - Codex plugin: `:1455/auth/callback` (`opencode/src/plugin/openai/codex.ts:13,166`).
  - DigitalOcean plugin: `:1456` (`opencode/src/plugin/digitalocean.ts:13-14`).

**Rust mapping (§5).**
- Embed `app/dist` with `rust-embed`, or `include_dir` behind a build feature.
- Implement SPA fallback, `mime_guess`, and per-HTML CSP with a sha256 of the theme-preload script (`sha2` + `base64`).
- Proxy mode uses a `reqwest` streaming passthrough.
- The UI handler must be the router `fallback` so that unknown API paths return HTML.

---

## 6. Versioning and server detection

**Version endpoints.**
- No version header exists. A grep for `x-opencode-version` / `server-version` found none.
- `GET /global/health` returns `{"healthy":true,"version":InstallationVersion}`. `InstallationVersion` is the build-time `OPENCODE_VERSION`, or `"local"` (`H/global.ts:66-68`, `core/src/installation/version.ts:6`).
- `GET /api/health` returns only `{"healthy":true}` (`S/handlers/health.ts:6`).
- `openapi.info.version` is the constant `"1.0.0"` (`HA/public.ts:530-536`). The protocol API annotation is `"0.0.1"` (`protocol/src/api.ts:57-61`).

**Web app protocol selection** (`app/src/utils/server-protocol.ts:24-35`). A probe succeeds only with a 2xx status and a `content-type` containing `application/json`, within a 5 s timeout. The app checks in order:
1. `GET /global/health` returns `healthy===true` → `"v1"`.
2. Else `GET /api/health` has a numeric `pid` → `"v2"`.
3. Else `GET /api/health` has `healthy===true` → `"v1"`.
4. Else → `"v2"`.

In v1 mode, the app uses legacy routes and `/global/event` for events (`app/src/context/server-sdk.tsx:276-279`).

**Other clients.**
- The app health check tries `GET /api/health` through `@opencode-ai/client`, then the legacy SDK's `/global/health` (`app/src/utils/server-health.ts:87-110`).
- Desktop probes `/api/health`, then `/global/health` (`desktop/src/main/server.ts:186-206`).
- The v2 SDK rejects any `content-type: text/html` response as "not supported by this version" (`sdk/js/src/v2/client.ts:84-90`).
- The legacy v1 SDK (`sdk/js/src/gen/sdk.gen.ts`) uses 67 URLs. All of them exist in v1.19.5 after normalizing param names (`{id}` → `{sessionID}` and similar; verified by script).

**Rust mapping (§6).** Keep `/global/health` returning `application/json` with `version` = the Cargo package version string. To stay classified as `v1`, keep `/global/health` healthy. Do not add `pid` to `/api/health` unless the full v2-protocol client behavior is implemented.

---

## 7. Top porting risks

| # | Risk | Mitigation |
|---|---|---|
| R1 | Effect-framework encodings with no spec: `{"_tag":"BadRequest"}`, empty-body 401/403/404, `StreamSse` framing, `NumberFromString` parse rules, excess-query-key handling | Capture golden responses from the TS server (§8) and run byte-diff tests |
| R2 | Two error vocabularies selected by path prefix (§2.7). openapi.json misdocuments legacy 400s (D1). | Separate router layers with distinct error types |
| R3 | The UI catch-all returns HTML 200 for unknown API paths. Clients use it for compatibility detection. | `fallback` = UI; never return a JSON 404 for unknown paths |
| R4 | Directory resolution: precedence, double percent-decoding, session-ID override, workspace proxying (§1.7–1.8) | Port as a single pure function with table-driven tests |
| R5 | PTY cursor in UTF-16 units (§4.2) | UTF-16 counters |
| R6 | `/api/event` encodes only `ServerDefinitions`; the behavior on other events is UNKNOWN (D3) | Determine empirically; legacy clients do not use `/api/event` |
| R7 | Long-running `POST /session/{id}/message` (blocks for a whole LLM turn) and SSE: proxies, timeouts, and compression must not buffer them | No request timeout on these routes; exclude them from compression |
| R8 | The default TUI does not use HTTP at all (in-process RPC, `opencode/src/cli/tui/worker.ts`). A Rust server needs the TUI started in "external" mode or a new transport. | Coordinate with the TUI spec |
| R9 | The stdout readiness line is a contract (`sdk/js/src/v2/server.ts:56-57`) | Print it verbatim |
| R10 | openapi 3.1 `anyOf` unions without discriminators make generated Rust types awkward | Hand-write the event, part, and message enums |

---

## 8. UNKNOWN items / Required additional data

| # | Item | How to determine |
|---|---|---|
| U1 | Wire body of Effect built-ins `HttpApiError.BadRequest` / `InternalServerError` / `Forbidden` (assumed `{"_tag":"BadRequest"}` per openapi components) | Run the TS server (`bun run --cwd packages/opencode src/index.ts serve --port 4096`) and call `curl -i 'localhost:4096/session/ses_x/message?before=zzz'` (a `before` without `limit` triggers `H/session.ts:110`) |
| U2 | `StreamSse` framing on `/api/session/{id}/event` (`id:` / `event:` values, failure event, heartbeat) | `curl -N localhost:4096/api/session/<ses_id>/event`; or read `effect@4.0.0-beta.83` `src/unstable/httpapi/HttpApiBuilder.ts` / `HttpApiSchema.ts` after `bun install` |
| U3 | `/api/event` behavior when a non-`ServerDefinitions` event (e.g. `session.status`, `tui.toast.show`) is published | `curl -N localhost:4096/api/event` and, in parallel, `curl -XPOST localhost:4096/tui/show-toast -d '{"message":"x","variant":"info"}' -H 'content-type: application/json'` |
| U4 | Effect `HttpMiddleware.cors` defaults (allow-methods, credentials, expose-headers) and global middleware nesting order | `curl -i -X OPTIONS -H 'Origin: http://localhost:3000' -H 'Access-Control-Request-Method: PATCH' localhost:4096/config`; or read Effect `src/unstable/http/HttpMiddleware.ts` |
| U5 | Whether unknown query keys or explicit `null` for optional fields are rejected; `NumberFromString` edge cases (`""`, `"1e3"`, `" 1"`) | `curl 'localhost:4096/session?foo=1'`; `curl 'localhost:4096/session?limit=1e3'`; `curl -XPATCH .../session/<id> -d '{"title":null}'` |

Lower-impact UNKNOWNs, marked inline above:
- Emitters for `installation.update-available`, `workspace.ready`, `workspace.failed` (§3.4).
- Whether the first `/api/event` heartbeat is immediate (§3.1).
- Exact bytes of axum's SSE comment (§3.7).
