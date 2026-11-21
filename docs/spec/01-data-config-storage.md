# 01 — Data, Config & Storage (opencode v1.19.5 → Rust)

Source root: `opencode-ts/packages/` (all refs relative to it). Version: `opencode/package.json:3` = `1.19.5`.
Rule: every identifier below is copied from source with `path:line`. Library behaviour that is not in the repo is tagged `LIB` with the package version from `bun.lock`. Unknowns are `UNKNOWN` + how to resolve. "Rust mapping" lines are suggestions only.

---

## 0. Runtime topology (what v1.19.5 actually executes)

| Concern | Shipped runtime path (authoritative) | Parallel / newer path also loaded | Notes |
|---|---|---|---|
| CLI binary | `opencode/package.json:18-19` bin `opencode` → `packages/opencode` | `packages/cli` (`@opencode-ai/cli`, bin `lildax`, `cli/package.json:7-8`) — not the shipped `opencode` | |
| Global paths | `core/src/global.ts` (`Global.Path`, static) | `Global.Service.make()` (`core/src/global.ts:59-72`) overrides `config` with `OPENCODE_CONFIG_DIR` | §1 |
| Main config | V1 loader `opencode/src/config/config.ts` (`Config.Service` `"@opencode/Config"`, :136) | V2 loader `core/src/config.ts` (`"@opencode/v2/Config"`, :133), per *location*, read-only; consumed by `core/src/location-services.ts:6` (PTY shell `core/src/pty.ts:167`, watcher, snapshot, tool-output-store, session runner) | §3; both read the same files with **different precedence** |
| TUI config | `opencode/src/config/tui.ts` (`tui.json[c]`) | — | §3.10 |
| DB | `core/src/database/database.ts` + `core/src/database/migration.ts` (custom journal `migration`) | old drizzle journal `__drizzle_migrations` only read for seeding | §4 |
| Provider credentials | `opencode/src/auth/index.ts` → `<data>/auth.json` | `core/src/credential.ts` → SQLite `credential` table (used by `core/src/integration.ts`) | §7 |
| v1 sessions | tables `session`, `message`, `part`, `todo`, `session_share`, `permission` | v2 event-sourced tables `event`, `event_sequence`, `session_message`, `session_input`, `session_context_epoch` (`core/src/event.ts`, `core/src/session/*.ts`) | §4 |
| Legacy JSON storage | `opencode/src/storage/storage.ts` — only writer left: `session_diff` (`opencode/src/session/revert.ts:77`) | — | §5.1 |

Observed on the operator machine (A-level, tool-executed, read-only):
- `~/.local/share/opencode`, `~/.config/opencode`, `~/.local/state/opencode`, `~/.cache/opencode` exist on **Windows** → confirms XDG-style paths on win32 (§1).
- Installed build is `@opencode/cli` `2.1.2` (`%APPDATA%/npm/node_modules/@opencode/cli/package.json`), not 1.19.5. Its `opencode.db` has migration ids up to `20261030054400_job`, lacks `data_migration`, `session_input`, `session_context_epoch`, and has extra tables (`session_v2`, `session_inbox`, `session_part`, `blob`, `blob_ref`, `job`, `schedule`, `kv`, `worktree`, `instruction_*`). **A v1.19.5-faithful Rust build is not drop-in for this machine's DB** — see §9 risk R1.

---

## 1. Global paths

### 1.1 Resolution — `core/src/global.ts`

| Key | Expression | Ref |
|---|---|---|
| app name | `"opencode"` | `global.ts:10` |
| `data` | `path.join(xdgData!, "opencode")` | `:11` |
| `cache` | `path.join(xdgCache!, "opencode")` | `:12` |
| `config` | `path.join(xdgConfig!, "opencode")` | `:13` |
| `state` | `path.join(xdgState!, "opencode")` | `:14` |
| `tmp` | `path.join(os.tmpdir(), "opencode")` | `:15` |
| `home` (getter) | `process.env.OPENCODE_TEST_HOME ?? os.homedir()` | `:18-20` |
| `bin` | `<cache>/bin` | `:22` |
| `log` | `<data>/log` | `:23` |
| `repos` | `<data>/repos` | `:24` |
| `Global.Service.config` | `Flag.OPENCODE_CONFIG_DIR ?? Path.config` (only the Effect service; static `Global.Path.config` is never overridden) | `:64` |

`xdgData/xdgCache/xdgConfig/xdgState` come from `xdg-basedir@5.1.0` (`bun.lock:360`, `global.ts:3`). LIB behaviour (not in repo; confirmed on win32 by observed dirs): `XDG_DATA_HOME || ~/.local/share`, `XDG_CONFIG_HOME || ~/.config`, `XDG_STATE_HOME || ~/.local/state`, `XDG_CACHE_HOME || ~/.cache`, **identical on Linux, macOS, Windows** (no `%APPDATA%`, no `~/Library`). `~` = Node `os.homedir()` (win32: `USERPROFILE`). Note `OPENCODE_TEST_HOME` does **not** move XDG dirs.

Effective defaults:

| OS | data | config | state | cache |
|---|---|---|---|---|
| Linux/macOS | `~/.local/share/opencode` | `~/.config/opencode` | `~/.local/state/opencode` | `~/.cache/opencode` |
| Windows | `%USERPROFILE%\.local\share\opencode` | `%USERPROFILE%\.config\opencode` | `%USERPROFILE%\.local\state\opencode` | `%USERPROFILE%\.cache\opencode` |

### 1.2 Side effects at module load
- `Flock.setGlobal({ state })` (`global.ts:33`) → lock root `<state>/locks` (`core/src/util/flock.ts:21`).
- `mkdir -p` of `data, config(XDG), state, tmp, log, bin, repos` (`global.ts:35-43`). `cache` itself is created implicitly via `bin`.

### 1.3 Files & dirs (by root)

| Path | Producer / consumer | Ref |
|---|---|---|
| `<data>/opencode.db` (+`-wal`,`-shm`) | SQLite (§4.1 naming rules) | `core/src/database/database.ts:43-55` |
| `<data>/auth.json` | provider auth (§7.1) | `opencode/src/auth/index.ts:10` |
| `<data>/mcp-auth.json` | MCP OAuth (§7.2) | `opencode/src/mcp/auth.ts:37` |
| `<data>/storage/` | legacy JSON storage (§5.1) | `opencode/src/storage/storage.ts:224` |
| `<data>/log/opencode.log` | append-only log (§5.6) | `core/src/observability/logging.ts:49` |
| `<data>/log/direct/<stamp>-<pid>.jsonl`, `<data>/log/direct/latest.json` | `OPENCODE_DIRECT_TRACE` | `opencode/src/cli/cmd/run/trace.ts:31-37` |
| `<data>/log/heap-<pid>-<iso>.heapsnapshot` | `OPENCODE_AUTO_HEAP_SNAPSHOT` | `opencode/src/cli/heap.ts:28-31` |
| `<data>/tool-output/tool_<id>` | truncated tool output, 7-day retention by mtime | `opencode/src/tool/truncation-dir.ts:4`, `opencode/src/tool/truncate.ts:12,53-66,69`; core twin `core/src/tool-output-store.ts:15,17,130` |
| `<data>/snapshot/<projectID>/<sha1hex(worktree)>` | snapshot git dir | `opencode/src/snapshot/index.ts:71`, `core/src/snapshot.ts:98` |
| `<data>/worktree/<projectID>/…` | managed git worktrees | `opencode/src/worktree/index.ts:208` |
| `<data>/plans/<time_created>-<slug>.md` | plan files when project has **no** VCS; otherwise `<worktree>/.opencode/plans/…` | `opencode/src/session/session.ts:331-336` |
| `<data>/repos/<host split on ':'>/<segments…>` | cloned reference repos | `opencode/src/util/repository.ts:209-211` |
| `<cache>/bin/` | downloaded LSP servers, `rg[.exe]`; also appended to PATH lookup | `opencode/src/lsp/server.ts` (many), `core/src/ripgrep/binary.ts:97`, `core/src/util/which.ts:7` |
| `<cache>/models.json` or `<cache>/models-<sha1hex(url)>.json` | models.dev cache, TTL 5 min (mtime) | `core/src/models-dev.ts:160-165` |
| `<cache>/packages/<sanitize(pkg)>` | npm installs | `core/src/npm.ts:87` |
| `<cache>/skills` / `<cache>/skills/<Bun.hash(base).toString(16)>` | remote skill cache | `opencode/src/skill/discovery.ts:35`, `core/src/skill/discovery.ts:113` |
| `<state>/locks/<sha1hex(key)>.lock/` | cross-process locks (§5.5) | `core/src/util/flock.ts:321` |
| `<state>/model.json` | recent/favorite models (CLI + TUI) | `opencode/src/cli/cmd/run/variant.shared.ts:19`, `opencode/src/provider/provider.ts:2038`, `tui/src/context/local.tsx:164` |
| `<state>/kv.json` | TUI KV store | `tui/src/context/kv.tsx:15` |
| `<state>/session.json` | TUI | `tui/src/context/local.tsx:420` |
| `<state>/prompt-history.jsonl`, `prompt-stash.jsonl`, `frecency.jsonl` | TUI prompt | `tui/src/prompt/history.tsx:53`, `stash.tsx:36`, `frecency.tsx:42` |
| `<state>/plugin-meta.json` | plugin metadata (override `OPENCODE_PLUGIN_META_FILE`) | `opencode/src/plugin/meta.ts:49` |
| `<config>/…` | config files & dirs (§3.2) | — |
| `<tmp>` | whitelisted for agents | `opencode/src/agent/agent.ts:110`, `core/src/plugin/agent.ts:101` |

Observed on disk but **no writer found in v1.19.5 sources**: `<config>/cli.json`, `<config>/service.json`, `<state>/service.json`, `<state>/latest/{locks,tui}`, `<data>/shell/`, `<cache>/version`, `<cache>/package.json`, `<cache>/bun.lock`. `UNKNOWN` — produced by other versions (likely 2.x / desktop). Rust must leave them untouched.

**Rust mapping (suggestion):** do NOT use `dirs`/`directories` platform defaults. Implement xdg-basedir semantics by hand: `env::var_os("XDG_*_HOME").filter(non-empty)` else `home.join(".local/share")` etc. on every OS; home = `std::env::home_dir()` (Rust ≥1.85 uses `USERPROFILE` on Windows) or `etcetera::home_dir()`. `etcetera::base_strategy::Xdg` is equivalent if you verify empty-string handling.

---

## 2. Environment variables

### 2.1 Parsing families (behaviour differs — must be replicated per variable)

| Family | Semantics | Ref |
|---|---|---|
| `truthy(k)` | `process.env[k]?.toLowerCase()` ∈ {`"true"`,`"1"`}; anything else false | `core/src/flag/flag.ts:3-6` |
| `enabledByExperimental(k)` (core) | if `k` unset → `truthy("OPENCODE_EXPERIMENTAL")`, else `truthy(k)` | `flag.ts:11-13` |
| Effect `Config.boolean(k)` + `withDefault(false)` | LIB `effect@4.0.0-beta.83` (verified from local copy `src/Config.ts:939-967`): **case-sensitive** true ∈ {`true`,`yes`,`on`,`1`,`y`}, false ∈ {`false`,`no`,`off`,`0`,`n`}; missing → default; **any other value is a ConfigError (default does not apply)** → `RuntimeFlags.node` uses `Layer.orDie` (`opencode/src/effect/runtime-flags.ts:75`) ⇒ fatal at startup | `runtime-flags.ts:4` |
| runtime `enabledByExperimental(k)` | `Config.boolean(k)` optional; if absent → `bool("OPENCODE_EXPERIMENTAL")` | `runtime-flags.ts:10-14` |
| `positiveInteger(k)` | `Config.number`, keep only integer > 0, invalid → `undefined` | `runtime-flags.ts:5-9` |
| raw string | `process.env[k]` | — |
| eager vs lazy | `Flag` fields without `get` are read **once at module load**; getters re-read each access (`flag.ts:52-77`) | |

### 2.2 `OPENCODE_*` (runtime-read)

| Var | Parse | Default | Effect | Ref |
|---|---|---|---|---|
| `OPENCODE_CONFIG` | raw path | — | extra config file merged after global (§3.3 step 3); also suppresses default global file creation | `flag.ts:21`, `opencode/src/config/config.ts:264,415-418` |
| `OPENCODE_CONFIG_CONTENT` | raw JSONC text | — | inline config, merged after `.opencode` dirs, scope `local`, substitution base dir = instance dir | `flag.ts:22`, `config.ts:482-490` |
| `OPENCODE_CONFIG_DIR` | raw path (getter) | — | extra config dir (highest of dirs); replaces `config` in `Global.Service` (v2 loader) | `flag.ts:63-65`, `global.ts:64`, `opencode/src/config/paths.ts:39` |
| `OPENCODE_DISABLE_PROJECT_CONFIG` | truthy (getter) | false | skip project `opencode.json[c]`, project `.opencode` dirs, project `tui.json[c]`, instructions | `flag.ts:54-56`, `config.ts:420`, `paths.ts:27`, `opencode/src/config/tui.ts:176`, `core/src/instruction-context.ts:48` |
| `OPENCODE_TUI_CONFIG` | raw path (getter) | — | extra TUI config file | `flag.ts:60-62`, `tui.ts:189` |
| `OPENCODE_PERMISSION` | JSON (getter) | — | `mergeDeep(result.permission ?? {}, JSON.parse(v))`; invalid JSON → warning | `flag.ts:69-71`, `config.ts:559-565` |
| `OPENCODE_DB` | raw | — | DB path override (§4.1) | `flag.ts:47`, `database.ts:44-46` |
| `OPENCODE_DISABLE_CHANNEL_DB` | `=== "1"` or `=== "true"` (exact) | — | force `opencode.db` filename | `database.ts:50-51` |
| `OPENCODE_AUTH_CONTENT` | JSON | — | replaces `auth.json` content entirely (no per-entry validation); invalid JSON → falls back to file | `opencode/src/auth/index.ts:59-63` |
| `OPENCODE_TEST_HOME` | raw | `os.homedir()` | `Global.Path.home` only | `global.ts:19` |
| `OPENCODE_TEST_MANAGED_CONFIG_DIR` | raw (`\|\|`, empty ignored) | system dir | managed config dir override | `opencode/src/config/managed.ts:32` |
| `OPENCODE_PLUGIN_META_FILE` | raw (getter) | `<state>/plugin-meta.json` | | `flag.ts:72-74`, `opencode/src/plugin/meta.ts:49` |
| `OPENCODE_MODELS_URL` | raw (`\|\|`) | `https://models.opencode.ai` | models source; cache file `models-<sha1(url)>.json` if non-default | `flag.ts:45`, `core/src/models-dev.ts:160-163` |
| `OPENCODE_MODELS_PATH` | raw | — | read models JSON from this file instead of cache | `flag.ts:46`, `models-dev.ts:184` |
| `OPENCODE_DISABLE_MODELS_FETCH` | truthy | false | no network fetch | `flag.ts:29`, `models-dev.ts:222,255` |
| `OPENCODE_LOG_LEVEL` | upper-cased ∈ {DEBUG,INFO,WARN,ERROR} | INFO | log level; CLI `--log-level` writes it | `core/src/observability/logging.ts:56-65`, `opencode/src/index.ts:68` |
| `OPENCODE_PRINT_LOGS` | `=== "1"` | — | also log to stderr; CLI `--print-logs` writes it | `logging.ts:68`, `opencode/src/index.ts:67` |
| `OPENCODE_PURE` | truthy (core getter) **and** `Config.boolean` (runtime) | false | no external plugins; CLI `--pure` sets `"1"` | `flag.ts:66-68`, `runtime-flags.ts:18`, `opencode/src/index.ts:69-71` |
| `OPENCODE_CLIENT` | raw | `"cli"` | client id in User-Agent `opencode/<channel>/<version>/<client>` | `flag.ts:75-77`, `runtime-flags.ts:56`, `core/src/models-dev.ts:23` |
| `OPENCODE_SERVER_PASSWORD` / `OPENCODE_SERVER_USERNAME` | raw | username `"opencode"` (per CLI help) | server basic auth | `flag.ts:32-33`, `opencode/src/cli/cmd/attach.ts:38,43` |
| `OPENCODE_WORKSPACE_ID` | raw | — | workspace fence for HTTP API | `flag.ts:49`, `opencode/src/server/routes/instance/httpapi/middleware/fence.ts:15` |
| `OPENCODE_FAKE_VCS` | decoded as `Project.Vcs` (`"git"`) | — | tests: pretend VCS | `flag.ts:31`, `opencode/src/project/project.ts:142` |
| `OPENCODE_GIT_BASH_PATH` | raw | — | Windows bash override | `flag.ts:20`, `core/src/shell.ts:125` |
| `OPENCODE_DISABLE_AUTOUPDATE` | truthy | false | | `flag.ts:23`, `opencode/src/cli/upgrade.ts:23` |
| `OPENCODE_ALWAYS_NOTIFY_UPDATE` | truthy | false | | `flag.ts:24` |
| `OPENCODE_DISABLE_PRUNE` | truthy | false | forces `compaction.prune=false` | `flag.ts:25`, `config.ts:596-598` |
| `OPENCODE_DISABLE_AUTOCOMPACT` | truthy | false | forces `compaction.auto=false` | `flag.ts:28`, `config.ts:593-595` |
| `OPENCODE_DISABLE_TERMINAL_TITLE` | truthy | false | | `flag.ts:26` |
| `OPENCODE_DISABLE_MOUSE` | truthy | false | | `flag.ts:30` |
| `OPENCODE_SHOW_TTFD` | truthy | false | | `flag.ts:27` |
| `OPENCODE_AUTO_HEAP_SNAPSHOT` | truthy | false | | `flag.ts:19` |
| `OPENCODE_DISABLE_FFF` | truthy if set | **win32: true**, else false | file search backend | `flag.ts:9,34` |
| `OPENCODE_EXPERIMENTAL_DISABLE_COPY_ON_SELECT` | truthy if set | **win32: true**, else false | | `flag.ts:8,43-44` |
| `OPENCODE_EXPERIMENTAL` | truthy / Config.boolean | false | master switch for `enabledByExperimental` vars + Exa | `flag.ts:12`, `runtime-flags.ts:10` |
| `OPENCODE_EXPERIMENTAL_WORKSPACES` | enabledByExperimental (both families) | ← EXPERIMENTAL | | `flag.ts:50`, `runtime-flags.ts:50` |
| `OPENCODE_EXPERIMENTAL_REFERENCES` | enabledByExperimental (both) | ← EXPERIMENTAL | | `flag.ts:57-59`, `runtime-flags.ts:42` |
| `OPENCODE_EXPERIMENTAL_FILEWATCHER` / `_DISABLE_FILEWATCHER` | Config.boolean, default false | false | | `flag.ts:37-42` |
| `OPENCODE_AUTO_SHARE` | Config.boolean | false | | `runtime-flags.ts:17` |
| `OPENCODE_DISABLE_DEFAULT_PLUGINS` / `_EMBEDDED_WEB_UI` / `_EXTERNAL_SKILLS` / `_LSP_DOWNLOAD` | Config.boolean | false | | `runtime-flags.ts:19-22` |
| `OPENCODE_DISABLE_CLAUDE_CODE` (+`_PROMPT`, `_SKILLS`) | Config.boolean, OR-combined | false | `.claude` prompt/skills discovery off | `runtime-flags.ts:23-30` |
| `OPENCODE_ENABLE_EXA` / `OPENCODE_EXPERIMENTAL_EXA` | Config.boolean OR EXPERIMENTAL | false | | `runtime-flags.ts:31-35`, core twin `core/src/tool/websearch.ts:79` (truthy) |
| `OPENCODE_ENABLE_PARALLEL` / `OPENCODE_EXPERIMENTAL_PARALLEL` | Config.boolean OR | false | | `runtime-flags.ts:36-39`, `core/src/tool/websearch.ts:80` |
| `OPENCODE_WEBSEARCH_PROVIDER` | `"exa"`\|`"parallel"` | — | | `core/src/tool/websearch.ts:76-77`, `opencode/src/tool/websearch.ts:31` |
| `OPENCODE_ENABLE_EXPERIMENTAL_MODELS`, `OPENCODE_ENABLE_QUESTION_TOOL`, `OPENCODE_EXPERIMENTAL_LSP_TY`, `OPENCODE_EXPERIMENTAL_NATIVE_LLM`, `OPENCODE_EXPERIMENTAL_WEBSOCKETS` | Config.boolean | false | | `runtime-flags.ts:40,41,44,54,55` |
| `OPENCODE_EXPERIMENTAL_BACKGROUND_SUBAGENTS`, `_LSP_TOOL`, `_OXFMT`, `_PLAN_MODE`, `_CODE_MODE`, `_EVENT_SYSTEM`, `_ICON_DISCOVERY` | runtime enabledByExperimental | ← EXPERIMENTAL | `_ICON_DISCOVERY` triggers favicon scan into `project.icon_url` | `runtime-flags.ts:43,45-49,51`, `opencode/src/project/project.ts:233` |
| `OPENCODE_EXPERIMENTAL_OUTPUT_TOKEN_MAX`, `OPENCODE_EXPERIMENTAL_BASH_DEFAULT_TIMEOUT_MS` | positiveInteger | undefined | | `runtime-flags.ts:52-53` |
| `OPENCODE_REPO_CLONE_GITHUB_BASE_URL` | raw | — | | `core/src/repository.ts:175`, `opencode/src/util/repository.ts:87` |
| `OPENCODE_CALLER` | `"vscode"`/`"vscode-insiders"` | — | IDE detection | `opencode/src/ide/index.ts:33` |
| `OPENCODE_ACP_PROFILE` | `=== "1"` | — | | `opencode/src/acp/profile.ts:1` |
| `OPENCODE_DIRECT_TRACE` | truthy-ish (any non-empty) | — | | `opencode/src/cli/cmd/run/trace.ts:58` |
| `OPENCODE_EDITOR_SSE_PORT` (fallback after `CLAUDE_CODE_SSE_PORT`) | raw | — | | `tui/src/context/editor.ts:117` |
| `OPENCODE_FAST_BOOT` | `Boolean(v)` | — | | `tui/src/app.tsx:261` |
| `OPENCODE_ROUTE` | JSON | — | TUI initial route | `tui/src/app.tsx:260` |
| `OPENCODE_ZED_DB` | raw path | — | Zed editor DB | `tui/src/editor-zed.ts:189` |
| `OPENCODE_API_KEY` | presence | — | opencode provider key | `core/src/plugin/provider/opencode.ts:176` |
| `OPENCODE_CONSOLE_TOKEN` | **written** by config loader from active account token | — | | `opencode/src/config/config.ts:505-506` |

Build-time defines (string constants injected by `bun build --define`, **not** env at runtime): `OPENCODE_VERSION` (default `"local"`), `OPENCODE_CHANNEL` (default `"local"`; release builds `"latest"` — `script/src/index.ts:26-31`, `opencode/script/build.ts:195,199`), `OPENCODE_LIBC` (`core/src/filesystem/watcher.ts:20`), `OPENCODE_MODELS_DEV` (bundled models snapshot, `core/src/models-dev.ts:136`), `OPENCODE_WORKER_PATH` (`opencode/src/cli/cmd/tui.ts:21`), `OPENCODE_CLI_NAME` (`cli/src/commands/commands.ts:4`). Refs: `core/src/installation/version.ts:1-8`.

Env vars **set** for child processes: `AGENT="1"`, `OPENCODE="1"`, `OPENCODE_PID=<pid>` (`opencode/src/index.ts:75-77`); PTY: `TERM="xterm-256color"`, `OPENCODE_TERMINAL="1"`, win32 `LC_ALL/LC_CTYPE/LANG="C.UTF-8"` (`core/src/pty.ts:170-179`); workspace server: `OPENCODE_AUTH_CONTENT`, `OPENCODE_WORKSPACE_ID`, `OPENCODE_EXPERIMENTAL_WORKSPACES="true"`, OTEL passthrough (`opencode/src/control-plane/workspace.ts:529-535`).

String constants that look like env vars but are not: `OPENCODE_SKILL_PATTERN` (`opencode/src/skill/index.ts:24`), `OPENCODE_MODE_KEY`/`OPENCODE_BASE_MODE` (`tui/src/keymap.tsx:24`).

### 2.3 Non-`OPENCODE_*` env reads (first read site)

Paths/system: `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME` (LIB xdg-basedir; `XDG_CONFIG_HOME` also `opencode/src/cli/cmd/uninstall.ts:238`), `ProgramData` (`opencode/src/config/managed.ts:25`), `PATH`/`PATHEXT` (`core/src/util/which.ts:6,11`), `SHELL` (`core/src/shell.ts:207`), `COMSPEC` (`core/src/shell.ts:101`), `PWD` (`opencode/src/cli/cmd/run.ts:334`), `EDITOR`/`VISUAL` (`tui/src/editor.ts:27`), `TERM`/`TERM_PROGRAM`/`TERM_PROGRAM_VERSION` (`opencode/src/cli/cmd/debug/index.ts:56-59`), `TMUX`/`STY`/`DISPLAY`/`WAYLAND_DISPLAY` (`tui/src/app.tsx:250-253`), `ZED_TERM` (`tui/src/context/editor.ts:121`), `CLAUDE_CODE_SSE_PORT` (`tui/src/context/editor.ts:117`), `GIT_ASKPASS` (`opencode/src/ide/index.ts:24`), `VIRTUAL_ENV` (`opencode/src/lsp/server.ts:445`), `DOTNET_CLI_HOME` (`:779`), `VSCODE_EXTENSIONS` (`:789`).
Telemetry: `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS` (`core/src/flag/flag.ts:16-17`), `OTEL_RESOURCE_ATTRIBUTES` (`core/src/observability/otlp.ts:21`).
Providers (owned by provider spec; listed for completeness): `AWS_PROFILE`, `AWS_REGION`, `AWS_BEARER_TOKEN_BEDROCK`, `AWS_CONTAINER_CREDENTIALS_FULL_URI`, `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI` (`core/src/plugin/provider/amazon-bedrock.ts:93-100`), `AWS_ACCESS_KEY_ID`, `AWS_WEB_IDENTITY_TOKEN_FILE` (`opencode/src/provider/provider.ts:351,366`), `AZURE_RESOURCE_NAME`, `AZURE_COGNITIVE_SERVICES_RESOURCE_NAME` (`core/src/plugin/provider/azure.ts:23,63`), `CLOUDFLARE_ACCOUNT_ID`, `CLOUDFLARE_GATEWAY_ID`, `CLOUDFLARE_API_TOKEN`, `CF_AIG_TOKEN` (`core/src/plugin/provider/cloudflare-ai-gateway.ts:53-58`), `CLOUDFLARE_API_KEY` (`cloudflare-workers-ai.ts:65`), `GOOGLE_VERTEX_PROJECT`, `GOOGLE_CLOUD_PROJECT`, `GCP_PROJECT`, `GCLOUD_PROJECT`, `GOOGLE_VERTEX_LOCATION`, `GOOGLE_CLOUD_LOCATION`, `VERTEX_LOCATION` (`core/src/plugin/provider/google-vertex.ts:10-22`), `GITLAB_INSTANCE_URL`, `GITLAB_TOKEN` (`gitlab.ts:19-20`), `AICORE_SERVICE_KEY`, `AICORE_DEPLOYMENT_ID`, `AICORE_RESOURCE_GROUP` (`sap-ai-core.ts:15,34`), `SNOWFLAKE_CORTEX_TOKEN`, `SNOWFLAKE_CORTEX_PAT` (`snowflake-cortex.ts:74-75`), `SNOWFLAKE_ACCOUNT` (`opencode/src/provider/provider.ts:960`), `EXA_API_KEY`, `PARALLEL_API_KEY` (`core/src/tool/websearch.ts:81-82`), `MODAL_PROXY_TOKEN` (`opencode/src/plugin/modal/modal.ts:9`), plus every name listed in a provider's `env` array (models.dev / config `provider.<id>.env`, `core/src/v1/config/provider.ts:101`).
GitHub action command: `GITHUB_TOKEN`, `GITHUB_RUN_ID`, `USE_GITHUB_TOKEN`, `MODEL`, `VARIANT`, `PROMPT`, `MENTIONS`, `OIDC_BASE_URL` (`opencode/src/cli/cmd/github.handler.ts:406-726`).
Config text substitution `{env:NAME}` reads **any** variable (§3.5).

**Rust mapping (suggestion):** one `flags` module with three explicit parsers (`truthy`, `effect_bool` — case-sensitive, error on invalid, `positive_int`); keep eager vs lazy split (tests mutate the getter vars). Treat invalid `Config.boolean` values as a startup error to match TS, or document the deviation.

---

## 3. Configuration

### 3.1 Loaders
1. **V1 loader** (authoritative): `opencode/src/config/config.ts` — per instance (`InstanceState`), global part cached forever (`:295-303`, `Duration.infinity`, invalidated by `updateGlobal`).
2. **V2 loader**: `core/src/config.ts` — per location, read once, silently ignores invalid files (§3.9).
3. **TUI loader**: `opencode/src/config/tui.ts` (§3.10).

### 3.2 File names & locations

| Scope | Files | Ref |
|---|---|---|
| Global (V1) | `<config>/config.json`, `<config>/opencode.json`, `<config>/opencode.jsonc` (merged in that order); legacy TOML `<config>/config` (migrated, see §3.8) | `config.ts:272-290` |
| Global file chosen for writes | first existing of `opencode.jsonc`, `opencode.json`, `config.json`; default `opencode.jsonc` | `config.ts:140-148` |
| `OPENCODE_CONFIG` | that file | `config.ts:415-418` |
| Project | `opencode.json`, `opencode.jsonc` in every dir from **worktree root down to cwd** (`up()` from cwd to worktree, reversed) | `opencode/src/config/paths.ts:10-21`, `config.ts:420-424` |
| Config dirs | `[Global.Path.config, …".opencode" dirs found walking cwd→worktree (nearest first), …~/.opencode, OPENCODE_CONFIG_DIR]`, `unique()` | `paths.ts:23-41` |
| In `.opencode` / `OPENCODE_CONFIG_DIR` | `opencode.json`, then `opencode.jsonc` | `config.ts:438-448` |
| Managed (admin) | `opencode.json`, `opencode.jsonc` in `/Library/Application Support/opencode` (darwin), `%ProgramData%\opencode` (fallback `C:\ProgramData\opencode`) (win32), `/etc/opencode` (else) | `opencode/src/config/managed.ts:20-29`, `config.ts:530-536` |
| macOS MDM | `/Library/Managed Preferences/<user>/ai.opencode.managed.plist`, then `/Library/Managed Preferences/ai.opencode.managed.plist` via `plutil -convert json -o - <plist>`; keys `PayloadDisplayName, PayloadIdentifier, PayloadType, PayloadUUID, PayloadVersion, _manualProfile` stripped | `managed.ts:8,11-18,43-69` |
| Remote well-known | for each `auth.json` entry with `type:"wellknown"`: GET `<key without trailing />/.well-known/opencode` → `{config?, remote_config?{url, headers}}`; `remote_config.url` fetched (JSON; `{config:{…}}` envelope unwrapped); HTML response → `ConfigRemoteAuthError` | `config.ts:370-410`, `core/src/v1/config/config.ts:22-25` |
| Console org config | active account with `active_org_id` → `accountSvc.config(accountID, orgID)`; source label `<url>/api/config` | `config.ts:492-528` |
| `OPENCODE_CONFIG_CONTENT` | inline | `config.ts:482-490` |
| TUI | `tui.json`, `tui.jsonc` (§3.10) | `opencode/src/config/tui.ts:184-209` |

Search algorithm `FSUtil.up` (`core/src/fs-util.ts:168-181`): at each dir, test each target in order with `exists`; stop after processing `stop` dir or at filesystem root. No worktree (TUI) → walks to filesystem root. For the global (non-VCS) project, worktree = `"/"`.

### 3.3 V1 precedence (low → high), `loadInstanceState` (`config.ts:328-612`)
1. Remote well-known configs (scope `global`) — `:370-410`.
2. Global (`config.json` < `opencode.json` < `opencode.jsonc`, merged with plain `mergeDeep`) — `:412-413`. If any well-known set auth env vars, global is reloaded uncached with those vars.
3. `OPENCODE_CONFIG` file — `:415-418`.
4. Project `opencode.json[c]`, root → cwd, `json` before `jsonc` within a dir (unless `OPENCODE_DISABLE_PROJECT_CONFIG`) — `:420-424`.
5. For each dir in §3.2 "Config dirs" order (nearest `.opencode` **first**, so a farther `.opencode` overrides a nearer one; then `~/.opencode`; then `OPENCODE_CONFIG_DIR`): `opencode.json`, `opencode.jsonc` (only for dirs ending `.opencode` or equal to `OPENCODE_CONFIG_DIR`), then markdown/plugin scans (§3.4) for **every** dir including `Global.Path.config` — `:438-480`.
6. `OPENCODE_CONFIG_CONTENT` (scope `local`) — `:482-490`.
7. Active console org config (scope `global`) — `:492-528`.
8. Managed dir files — `:530-536`.
9. macOS MDM plist (highest) — `:538-548`.
Post-processing (`:550-598`): `mode.<name>` merged into `agent.<name>` with `mode:"primary"`; `OPENCODE_PERMISSION` deep-merged; legacy `tools` → `permission` (`write|edit|patch` → `edit`; `true`→`allow`, `false`→`deny`; existing `permission` wins: `mergeDeep(perms, result.permission)`); `username` default `os.userInfo().username || "user"`; `autoshare===true && !share` → `share="auto"`; env compaction overrides. Defaults ensured: `agent ??= {}`, `mode ??= {}`, `plugin ??= []` (`:426-428,444-446`).

### 3.4 Directory scans per config dir (glob, `dot:true`, `symlink:true`, absolute)

| Pattern | Produces | Name rule | Ref |
|---|---|---|---|
| `{command,commands}/**/*.md` | `command.<name>` (`template` = body trimmed) — invalid → `ConfigInvalidError` (fatal) | path relative to dir, strip `command/`\|`commands/` prefix, strip extension; `\` → `/` | `opencode/src/config/command.ts:15-40`, `entry-name.ts` |
| `{agent,agents}/**/*.md` | `agent.<name>` (`prompt` = body trimmed) — invalid → `ConfigInvalidError` | same with `agent/`\|`agents/` | `opencode/src/config/agent.ts:11-32` |
| `{mode,modes}/*.md` | `agent.<name>` with `mode:"primary"`; invalid silently skipped | same | `agent.ts:34-58` |
| `{plugin,plugins}/*.{ts,js}` | plugin specs as `file://` URLs | — | `opencode/src/config/plugin.ts:18-30` |
| `{tool,tools}/*.{js,ts}` | custom tools | — | `opencode/src/tool/registry.ts:185` |
| `{skill,skills}/**/SKILL.md` | skills | — | `opencode/src/skill/index.ts:24,205-208` |
| `themes/*.json` (TUI, `Global.Path.config` + dirs) | themes keyed by basename | — | `tui/src/context/theme.tsx:39,55-57` |
Also external skills: `.claude/skills/**/SKILL.md` (unless disabled) and `.agents/skills/**/SKILL.md` walking cwd→worktree and in home (`opencode/src/skill/index.ts:21-23,186-201`); `skills.paths` (`~/` expanded with `Global.Path.home`, relative to cwd) (`:211-216`).

Markdown frontmatter: `gray-matter@4.0.3` (`bun.lock:347`, `core/src/config/markdown.ts:3-10`); on YAML error retry after `sanitize()` which rewrites top-level `key: value-with-colon` lines into `key: |-` block scalars (`markdown.ts:22-36`). Frontmatter fields spread before `prompt`/`template`, `name` derived from path first then overridable by frontmatter.

### 3.5 Parse pipeline per file (`config.ts:227-258`)
1. Read: `readFileStringSafe` → `undefined` on NotFound/PermissionDenied (`core/src/fs-util.ts:62-67`); empty/missing → `{}`.
2. **Substitution** (`opencode/src/config/variable.ts:34-90`), on raw text before parsing:
   - `{env:NAME}` regex `/\{env:([^}]+)\}/g` → `(env[NAME] ?? process.env[NAME]) || ""`, inserted **raw (no JSON escaping)**, also inside comments (`:36-38`). `env` = vars collected from well-known auth (`authEnv`).
   - `{file:PATH}` regex `/\{file:[^}]+\}/g` (`:40`): skipped if the text from line start to token, trimmed, starts with `//` (`:53-59`); `~/` → `os.homedir()` (`:62-64`); relative → resolved against config file dir (virtual sources: given `dir`) (`:66`); content `.trim()`ed then JSON-string-escaped `JSON.stringify(c).slice(1,-1)` (`:83-85`). Missing file → `ConfigInvalidError` "bad file reference" (main config) or `""` when `missing:"empty"` (TUI, `tui.ts:102`).
3. JSONC parse: `jsonc-parser@3.3.1` `parse(text, errors, { allowTrailingComma: true })`; any error → `ConfigJsonError` with formatted line/column (`opencode/src/config/parse.ts:8-33`).
4. `normalizeLoadedConfig`: delete top-level `theme`, `keybinds`, `tui` (moved to tui.json) (`config.ts:54-63`).
5. `ConfigV2Compat.lower` (`opencode/src/config/v2-compat.ts:91-133`) — accepts V2-native keys, emits diagnostics (logged as warnings):
   - `permissions` (top-level or in `agents|agent|mode.<n>`) → **hard error** `ConfigInvalidError` "V2 permissions are not supported by OpenCode V1…" (`:93-114`).
   - unsupported, dropped with diagnostic: `plugins`, `providers`, `websearch`, `warming` (`:117-118`), `experimental.portable_shell_scanner`.
   - `snapshots`→`snapshot`; `media`→`attachment`; `skills: string[]` → `{paths: non-http, urls: http(s)}`; `compaction.keep.tokens`→`preserve_recent_tokens`; `compaction.buffer`→`reserved`; `experimental.subagent_depth`→`subagent_depth`; `model` as `"provider/model#variant"` or `{providerID, model, variant}` → `"provider/model"` (variant dropped with diagnostic); `agents.<n>` → `agent.<n>` (`system`→`prompt`, `disabled`→`disable`, `request.body`→`options`, model selection lowered); `commands`→`command`; `mcp` `servers` envelope / `disabled` / `codemode` / `timeout{startup,catalog,execution}` / snake_case oauth → V1 shape (`enabled`, `timeout` only when `catalog==execution` and no `startup`, `clientId/clientSecret/scope/callbackPort/redirectUri`); `mcp.timeout` global → `experimental.mcp_timeout`; custom `lsp` entries without `extensions` dropped (`:135-423`).
   - Conflict rule `preferLegacy`: existing V1 key wins; differing value → diagnostic `conflict` (`:425-436`).
6. Schema decode `ConfigV1.Info` with `{ errors:"all", onExcessProperty:"ignore", propertyOrder:"original" }` (`parse.ts:35-61`) → unknown keys dropped, **key order preserved** (permission precedence depends on it, `core/src/v1/config/permission.ts:14-16`). Failure → `ConfigInvalidError{path, issues[]}`.
7. Path-based files only: plugin specs that are paths are resolved relative to the declaring file into `file://` URLs (`config.ts:102-110`, `opencode/src/config/plugin.ts:42-60`).
8. **`$schema` write-back**: if decoded config lacks `$schema`, set it to `"https://opencode.ai/config.json"` and rewrite the file: `text.replace(/^\s*\{/, '{\n  "$schema": "https://opencode.ai/config.json",')` using the **pre-substitution** text; write errors ignored (`config.ts:245-249`). Applies to every path-loaded file (global, `OPENCODE_CONFIG`, project, `.opencode`, managed).

Error names (`core/src/v1/config/error.ts:14-36`): `ConfigJsonError`, `ConfigInvalidError`, `ConfigFrontmatterError`, `ConfigDirectoryTypoError`, `ConfigRemoteAuthError`. Global cached load failure → logged, `{}` used (`config.ts:295-303`); instance load failure is fatal (`:614-618`, `orDie`).

### 3.6 Merge semantics
- `mergeDeep` from `remeda` (`config.ts:7,42-44`). LIB behaviour (verify, remeda not installed locally): result = `{...target, ...source}`, recursing only when **both** sides are plain objects; arrays and scalars replaced by source; `undefined` in source overwrites.
- `mergeConfigConcatArrays`: after deep merge, if both have `instructions` → ordered union `Array.from(new Set([...a, ...b]))` (`config.ts:46-52`). Used for every instance-level merge step; **not** inside `loadGlobal` (plain merge).
- `plugin`: after each merge, plugin list recomputed from accumulated `plugin_origins` deduped by identity (npm package name via `parsePluginSpecifier(spec).pkg`, or exact `file://` URL); **last occurrence wins, ordered by last occurrence** (`opencode/src/config/plugin.ts:64-77`, `config.ts:344-368`). Scope: `http(s)` source or explicit → `global`; `OPENCODE_CONFIG_CONTENT` → `local`; inside instance dir/worktree → `local`; else `global` (`config.ts:337-342`). `plugin_origins` is in-memory only (stripped before writes, `:164-167`).
- Markdown-derived `command`/`agent` merged with `mergeDeep` into accumulated result (`:473-475`).

### 3.7 Agent normalisation (decode transform, `core/src/v1/config/agent.ts:62-88`)
Unknown agent keys (not in `KNOWN_KEYS` `:43-60`) are moved into `options`; `tools` map → `permission` (same rules as §3.3) then explicit `permission` assigned over it; `steps = steps ?? maxSteps`.
Permission decode: a bare action string `"ask"|"allow"|"deny"` becomes `{ "*": action }` (`permission.ts:40-46`).

### 3.8 Writes performed by the config subsystem (side effects Rust must reproduce or consciously drop)

| Trigger | Write | Ref |
|---|---|---|
| Global load, no `OPENCODE_CONFIG`/`_DIR`/`_CONTENT`, global file missing | create `<config>/opencode.jsonc` with `{\n  "$schema": "https://opencode.ai/config.json"\n}` (2-space JSON) | `config.ts:264-271` |
| Any path-loaded file lacking `$schema` | in-place text insertion (§3.5 step 8) | `config.ts:245-249` |
| Legacy TOML `<config>/config` exists | import as TOML; `provider`+`model` → `model:"provider/model"`; merge rest; write merged result to `<config>/config.json` (2-space JSON); delete `config`; errors ignored | `config.ts:276-290` |
| Every config dir (incl. `<config>`, `OPENCODE_CONFIG_DIR` — `ensureDir` creates it) | `.gitignore` if missing: lines `node_modules`, `package.json`, `package-lock.json`, `bun.lock`, `.gitignore` joined by `\n` (no trailing newline); PermissionDenied ignored | `config.ts:309-326,450` |
| Every config dir | background `npm install` of `@opencode-ai/plugin@<InstallationVersion>` (unversioned for local builds) via `@npmcli/arborist` → `package.json`, `node_modules/`, lock files | `config.ts:452-471`, `core/src/npm.ts:88-95` |
| `Config.update(config)` (API) | writes `<instance directory>/config.json` = `mergeDeep(existing parsed JSONC or decoded, config)` as 2-space JSON. **Note:** this file is not read by the V1 loader | `config.ts:638-650` |
| `Config.updateGlobal(config)` | target = §3.2 write target. `.jsonc`: per-leaf `jsonc-parser.modify` + `applyEdits` with `{insertSpaces:true, tabSize:2}` (comments preserved). Other: parse, validate, `mergeDeep`, `JSON.stringify(_, null, 2)`. `shell:""` → removed. Only writes if changed; invalidates cache | `config.ts:150-162,169-174,656-680` |
| TUI start | migrates `theme`, `keybinds`, `tui.{scroll_speed,scroll_acceleration,diff_style}` from each `opencode.json[c]` (global, findUp from cwd root-first, config dirs, `OPENCODE_CONFIG`) into sibling `tui.json` (`$schema: "https://opencode.ai/tui.json"`) **only if `tui.json` absent**; backup `<file>.tui-migration.bak` (once); strips keys via jsonc `modify` | `opencode/src/config/tui-migrate.ts:11,29-131` |

### 3.9 V2 loader precedence (`core/src/config.ts:135-219`)
- If location dir == global config dir → no discovery (`:174-177`).
- `up({targets: [".opencode","opencode.jsonc","opencode.json"], start: location.directory, stop: location.project.directory})`.
- Final order (low→high, `latest(entries, key)` = last document defining key, `:122-126`): `<Global.Service.config>/opencode.json`, `/opencode.jsonc` → project files root→cwd (`json` before `jsonc`) → `.opencode` dirs **farthest→nearest** (`json`,`jsonc` each). **Differs from V1** (V1 merges `.opencode` nearest-first; V2 global dir honours `OPENCODE_CONFIG_DIR`, V1 global does not).
- Parse: JSONC (allowTrailingComma); parse error or decode failure → file silently ignored; no `{env:}`/`{file:}` substitution; no write-back.
- V1 detection: object containing any of `logLevel, server, command, reference, snapshot, plugin, autoshare, disabled_providers, enabled_providers, small_model, mode, agent, provider, permission, tools, attachment, layout` (`core/src/v1/config/migrate.ts:10-28,30-33`) → decode V1 then `ConfigMigrateV1.migrate` → V2 `Config.Info` (`core/src/config.ts:29-107`).
- `experimental.policies` collected in **reverse** document order (global rules override repo rules) (`:204-211`).

### 3.10 TUI loader (`opencode/src/config/tui.ts:83-221`)
Order: `<config>/tui.json`, `<config>/tui.jsonc` → `OPENCODE_TUI_CONFIG` → project `tui.json`/`tui.jsonc` root-first (walk to filesystem root, unless project config disabled) → `.opencode` dirs & `OPENCODE_CONFIG_DIR` (`tui.json`, `tui.jsonc`). Each file: substitution with `missing:"empty"`, JSONC, nested `"tui": {…}` flattened (`:55-70`), unknown keybinds dropped, schema `TuiConfig.Info` (`tui/src/config/index.tsx`), `attention.sounds` paths resolved relative to file; any failure → warning + skip (never fatal). Merge `mergeDeep`; plugins deduped as §3.6 (scope `local` iff file inside cwd). Schema URL `https://opencode.ai/tui.json`.

### 3.11 Config schema — `ConfigV1.Info` (`core/src/v1/config/config.ts:32-190`)
All keys optional. Types: `int+` = `PositiveInt` (int > 0), `int0` = `NonNegativeInt` (`schema/src/schema.ts:1-2`). Defaults shown are those stated in source descriptions or applied in code.

| Key | Type | Default / note | Line |
|---|---|---|---|
| `$schema` | string | written as `https://opencode.ai/config.json` | 33 |
| `shell` | string | | 36 |
| `logLevel` | `"DEBUG"\|"INFO"\|"WARN"\|"ERROR"` | | 37 |
| `server` | `{port?: int+, hostname?: string, mdns?: bool, mdnsDomain?: string (default "opencode.local"), cors?: string[]}` | `server.ts:6-17` | 38 |
| `command` | `Record<string, {template: string, description?, agent?, model?, variant?: string, subtask?: bool}>` | `command.ts:5-12` | 41 |
| `skills` | `{paths?: string[], urls?: string[]}` | `skills.ts:5-12` | 44 |
| `references` / `reference` (deprecated) | `Record<string, string \| {repository, branch?, description?, hidden?: bool} \| {path, description?, hidden?: bool}>` | `core/src/config/reference.ts:5-22` | 45-50 |
| `watcher` | `{ignore?: string[]}` | | 51 |
| `snapshot` | bool | true | 52 |
| `plugin` | `(string \| [string, Record<string,unknown>])[]` | `plugin.ts:5-8` | 56 |
| `share` | `"manual"\|"auto"\|"disabled"` | | 57 |
| `autoshare` | bool (deprecated → `share:"auto"`) | | 61 |
| `autoupdate` | `bool \| "notify"` | | 64 |
| `disabled_providers` / `enabled_providers` | string[] | | 68-73 |
| `model` / `small_model` | string `provider/model` | | 74-79 |
| `default_agent` | string | falls back to `build` | 80 |
| `subagent_depth` | int0 | 1 | 84 |
| `username` | string | system username | 87 |
| `mode` (deprecated) | `{build?, plan?, [k]: Agent}` | merged into `agent` | 90 |
| `agent` | `{plan?, build?, general?, explore?, title?, summary?, compaction?, [k]: Agent}` | | 96 |
| `provider` | `Record<string, Provider>` | below | 110 |
| `mcp` | `Record<string, McpLocal \| McpRemote \| {enabled: bool}>` | below | 113 |
| `formatter` | `bool \| Record<string, {disabled?: bool, command?: string[], environment?: Record<string,string>, extensions?: string[]}>` | `formatter.ts:5-12` | 116 |
| `lsp` | `bool \| Record<string, {disabled: true} \| {command: string[], extensions?: string[], disabled?: bool, env?: Record<string,string>, initialization?: Record<string,unknown>}>`; custom ids (not in `builtinServerIds` `lsp.ts:22-61`) require `extensions` | `lsp.ts:5-80` | 120 |
| `instructions` | string[] | concatenated-unique across sources | 124 |
| `layout` (deprecated) | `"auto"\|"stretch"` | | 127 |
| `permission` | `Action \| {read?, edit?, glob?, grep?, list?, bash?, task?, external_directory?, lsp?, skill?: Rule; todowrite?, question?, webfetch?, websearch?, doom_loop?: Action; [k]: Rule}`; `Action = "ask"\|"allow"\|"deny"`, `Rule = Action \| Record<string, Action>` | `permission.ts:5-46` | 128 |
| `tools` | `Record<string, bool>` (deprecated → permission) | | 129 |
| `attachment` | `{image?: {auto_resize?: bool (true), max_width?: int+ (2000), max_height?: int+ (2000), max_base64_bytes?: int+ (5242880)}}` | `attachment.ts:6-25` | 130 |
| `enterprise` | `{url?: string}` | | 133 |
| `tool_output` | `{max_lines?: int+ (2000), max_bytes?: int+ (51200)}` | code: `opencode/src/tool/truncate.ts:14-15` | 136 |
| `compaction` | `{auto?: bool (true), prune?: bool (false), tail_turns?: int0, preserve_recent_tokens?: int0, reserved?: int0}` | | 149 |
| `experimental` | `{disable_paste_summary?, batch_tool?, openTelemetry?, continue_loop_on_deny?: bool; primary_tools?: string[]; mcp_timeout?: int+; policies?: {action: "provider.use", effect: "allow"\|"deny", resource: string}[]}` | `core/src/config/experimental.ts:9-18`, `core/src/policy.ts:8-15`, `core/src/catalog.ts:20` | 169 |

**Agent** (`agent.ts:12-41`, `StructWithRest` — extra keys allowed → moved to `options`): `model?, variant?, prompt?, description?: string; temperature?, top_p?: finite number; tools?: Record<string,bool>; disable?, hidden?: bool; mode?: "subagent"|"primary"|"all"; options?: Record<string,any>; color?: "#RRGGBB" | "primary"|"secondary"|"accent"|"success"|"warning"|"error"|"info"; steps?, maxSteps?: int+; permission?: Permission`.

**Provider** (`provider.ts:98-147`): `api?, name?, id?, npm?: string; env?, whitelist?, blacklist?: string[]; options?: {apiKey?, baseURL?, enterpriseUrl?: string; setCacheKey?: bool; timeout?, headerTimeout? (300000), chunkTimeout? (300000): int+ | false; [k]: any}; models?: Record<string, Model>`.
**Model** (`provider.ts:24-96`): `id?, name?, family?, release_date?: string; attachment?, reasoning?, temperature?, tool_call?, experimental?: bool; reasoning_options?: ({type:"effort", values:(string|null)[]} | {type:"toggle"} | {type:"budget_tokens", min?, max?})[]; interleaved?: bool | string | {field: string}; cost?: {input, output, cache_read?, cache_write?, context_over_200k?: {input, output, cache_read?, cache_write?}}; limit?: {context, input?, output}; modalities?: {input?, output?: ("text"|"audio"|"image"|"video"|"pdf")[]}; status?: "alpha"|"beta"|"deprecated"|"active"; provider?: {npm?, api?}; options?: Record<string,any>; headers?: Record<string,string>; variants?: Record<string, {disabled?: bool, [k]: any}>`.
**McpLocal** (`mcp.ts:6-23`): `type:"local", command: string[], cwd?, environment?: Record<string,string>, enabled?: bool, timeout?: int+ (5000)`. **McpRemote** (`:44-60`): `type:"remote", url, enabled?, headers?, oauth?: false | {clientId?, clientSecret?, scope?, callbackPort?: 1..65535 (19876), redirectUri? (http://127.0.0.1:19876/mcp/oauth/callback)}, timeout?: int+ (5000)`.

### 3.12 JSON Schema availability
No generated JSON Schema is checked in (`core/schema.json` is the **drizzle DB snapshot**, `core/script/migration.ts:11`). Generator: `bun opencode/script/schema.ts <config-out> [tui-out]` (`opencode/script/schema.ts:68-77`) → Effect `Schema.toJsonSchemaDocument(ConfigV1.Info)`, draft 2020-12, null-variants stripped, integer `maximum = Number.MAX_SAFE_INTEGER`, `model`/`small_model` get `$ref: https://models.dev/model-schema.json#/$defs/Model`, `allowComments`/`allowTrailingCommas: true`. Published at `https://opencode.ai/config.json` / `https://opencode.ai/tui.json` (content for 1.19.5 `UNKNOWN`; obtain by running the generator at tag v1.19.5 after `bun install`).

**Rust mapping (suggestion):** `serde_json` with `preserve_order` (IndexMap) everywhere config is touched (permission precedence). JSONC: dprint `jsonc-parser` crate (`allow_trailing_commas`; `cst` feature for comment-preserving `updateGlobal` edits). Typed structs with `#[serde(flatten)] rest: IndexMap<String, Value>` for `StructWithRest`, `#[serde(untagged)]` unions, but keep the raw `Value` pipeline (substitute → parse → v2-compat lower → decode) to reproduce diagnostics. Deep-merge as a hand-written `Value` function matching remeda. Frontmatter: `serde_yaml`/`yaml-rust2` + port `sanitize()`; js-yaml (YAML 1.2 core schema) vs Rust YAML differences (`yes/no`, octal) must be tested. Glob: `globset` (brace expansion) + `walkdir` (`follow_links(true)`, include dotfiles). Embed the generated `config.json` schema as a test oracle.

---

## 4. SQLite

### 4.1 File
`Database.path()` (`core/src/database/database.ts:43-55`):
1. `OPENCODE_DB` set: `":memory:"` or absolute → as-is; else `join(<data>, OPENCODE_DB)`.
2. else if `InstallationChannel` ∈ {`latest`,`beta`,`prod`} or `OPENCODE_DISABLE_CHANNEL_DB` ∈ {`"1"`,`"true"`} → `<data>/opencode.db`.
3. else `<data>/opencode-<channel with /[^a-zA-Z0-9._-]/g → "-">.db` (local builds: `opencode-local.db`).
`opencode db path` prints it; `opencode db [query] [--format json|tsv]` (`opencode/src/cli/cmd/db.ts`).

### 4.2 Connection
- Driver: Bun binary → `bun:sqlite` (`core/src/database/sqlite.bun.ts`), Node build → `node:sqlite` (`sqlite.node.ts`, `enableForeignKeyConstraints: true` `:155`) — import map `core/package.json` `"#sqlite"`.
- Open: `create: true`, `readwrite: true`; driver sets `PRAGMA journal_mode = WAL;` (`sqlite.bun.ts:164`).
- One native connection per process; Effect semaphore (1 permit) serialises all statements and transactions (`sqlite.bun.ts:121-130`).
- PRAGMAs on open, in order (`database.ts:27-32`): `journal_mode = WAL`, `synchronous = NORMAL`, `busy_timeout = 5000`, `cache_size = -64000`, `foreign_keys = ON`, `wal_checkpoint(PASSIVE)`; then migrations (`:33`).
- Transactions: `begin deferred` by default (`effect-drizzle-sqlite/src/sqlite-core/effect/session.ts:145`), nested → `savepoint effect_sql_<n>`; project ID migration uses `{ behavior: "immediate" }` (`opencode/src/project/project.ts:190`).

### 4.3 Migration mechanism (`core/src/database/migration.ts`)
Journal table: `migration (id TEXT PRIMARY KEY, time_completed INTEGER NOT NULL)` (`:30,46`). Migration list: ordered array in `migration.gen.ts` (38 entries, §4.6); each `{ id, up(tx) }`.
`apply(db)` (`:18-41`, process-wide lock):
1. `tables = SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'`.
2. If `session` ∈ tables → `applyOnly(db, migrations)`.
3. Else if any table exists → die `"Database is not empty and has no session table"`.
4. Else (fresh DB), one transaction: run full DDL `schema.gen.ts` (§4.4), `CREATE TABLE migration …`, insert **all** 38 ids with `Date.now()`.
`applyOnly` (`:43-107`):
1. `CREATE TABLE IF NOT EXISTS migration …`.
2. If journal empty and `__drizzle_migrations` exists (seed from old drizzle journal; table left in place):
   - if it has column `name`: `INSERT OR IGNORE INTO migration SELECT name, now FROM __drizzle_migrations WHERE name IS NOT NULL`;
   - else: for each row with `created_at`: `prefix = strftime('%Y%m%d%H%M%S', created_at/1000, 'unixepoch')` (UTC); find migration whose id starts with `prefix + "_"`; none → die `Legacy migration timestamp <ms> does not match any known migration`; insert it.
3. For each migration in list order not in journal: own transaction: `up(tx)` then `INSERT INTO migration (id, time_completed) VALUES (id, Date.now())`. Unknown ids already in the journal (newer versions) are ignored — no downgrade detection.
Note: several migrations issue `PRAGMA foreign_keys=OFF/ON` inside the transaction; per SQLite docs this PRAGMA is a no-op inside a transaction (B-level, verify). Replicate statements verbatim anyway.

### 4.4 Final schema (fresh DB) — verbatim from `core/src/database/schema.gen.ts:7-271`
```sql
CREATE TABLE `workspace` (
  `id` text PRIMARY KEY,
  `type` text NOT NULL,
  `name` text DEFAULT '' NOT NULL,
  `branch` text,
  `directory` text,
  `extra` text,
  `project_id` text NOT NULL,
  `time_used` integer NOT NULL,
  CONSTRAINT `fk_workspace_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE
);
CREATE TABLE `data_migration` (
  `name` text PRIMARY KEY,
  `time_completed` integer NOT NULL
);
CREATE TABLE `account_state` (
  `id` integer PRIMARY KEY,
  `active_account_id` text,
  `active_org_id` text,
  CONSTRAINT `fk_account_state_active_account_id_account_id_fk` FOREIGN KEY (`active_account_id`) REFERENCES `account`(`id`) ON DELETE SET NULL
);
CREATE TABLE `account` (
  `id` text PRIMARY KEY,
  `email` text NOT NULL,
  `url` text NOT NULL,
  `access_token` text NOT NULL,
  `refresh_token` text NOT NULL,
  `token_expiry` integer,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL
);
CREATE TABLE `control_account` (
  `email` text NOT NULL,
  `url` text NOT NULL,
  `access_token` text NOT NULL,
  `refresh_token` text NOT NULL,
  `token_expiry` integer,
  `active` integer NOT NULL,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  CONSTRAINT `control_account_pk` PRIMARY KEY(`email`, `url`)
);
CREATE TABLE `credential` (
  `id` text PRIMARY KEY,
  `integration_id` text,
  `label` text NOT NULL,
  `value` text NOT NULL,
  `connector_id` text,
  `method_id` text,
  `active` integer,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL
);
CREATE TABLE `event_sequence` (
  `aggregate_id` text PRIMARY KEY,
  `seq` integer NOT NULL,
  `owner_id` text
);
CREATE TABLE `event` (
  `id` text PRIMARY KEY,
  `aggregate_id` text NOT NULL,
  `seq` integer NOT NULL,
  `type` text NOT NULL,
  `data` text NOT NULL,
  CONSTRAINT `fk_event_aggregate_id_event_sequence_aggregate_id_fk` FOREIGN KEY (`aggregate_id`) REFERENCES `event_sequence`(`aggregate_id`) ON DELETE CASCADE
);
CREATE TABLE `permission` (
  `id` text PRIMARY KEY,
  `project_id` text NOT NULL,
  `action` text NOT NULL,
  `resource` text NOT NULL,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  CONSTRAINT `fk_permission_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE
);
CREATE TABLE `project_directory` (
  `project_id` text NOT NULL,
  `directory` text NOT NULL,
  `type` text,
  `strategy` text,
  `time_created` integer NOT NULL,
  CONSTRAINT `project_directory_pk` PRIMARY KEY(`project_id`, `directory`),
  CONSTRAINT `fk_project_directory_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE
);
CREATE TABLE `project` (
  `id` text PRIMARY KEY,
  `worktree` text NOT NULL,
  `vcs` text,
  `name` text,
  `icon_url` text,
  `icon_url_override` text,
  `icon_color` text,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  `time_initialized` integer,
  `sandboxes` text NOT NULL,
  `commands` text
);
CREATE TABLE `message` (
  `id` text PRIMARY KEY,
  `session_id` text NOT NULL,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  `data` text NOT NULL,
  CONSTRAINT `fk_message_session_id_session_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE
);
CREATE TABLE `part` (
  `id` text PRIMARY KEY,
  `message_id` text NOT NULL,
  `session_id` text NOT NULL,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  `data` text NOT NULL,
  CONSTRAINT `fk_part_message_id_message_id_fk` FOREIGN KEY (`message_id`) REFERENCES `message`(`id`) ON DELETE CASCADE
);
CREATE TABLE `session_context_epoch` (
  `session_id` text PRIMARY KEY,
  `baseline` text NOT NULL,
  `snapshot` text NOT NULL,
  `baseline_seq` integer NOT NULL,
  CONSTRAINT `fk_session_context_epoch_session_id_session_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE
);
CREATE TABLE `session_input` (
  `id` text PRIMARY KEY,
  `session_id` text NOT NULL,
  `prompt` text NOT NULL,
  `delivery` text NOT NULL,
  `admitted_seq` integer NOT NULL,
  `promoted_seq` integer,
  `time_created` integer NOT NULL,
  CONSTRAINT `fk_session_input_session_id_session_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE
);
CREATE TABLE `session_message` (
  `id` text PRIMARY KEY,
  `session_id` text NOT NULL,
  `type` text NOT NULL,
  `seq` integer NOT NULL,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  `data` text NOT NULL,
  CONSTRAINT `fk_session_message_session_id_session_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE
);
CREATE TABLE `session` (
  `id` text PRIMARY KEY,
  `project_id` text NOT NULL,
  `workspace_id` text,
  `parent_id` text,
  `slug` text NOT NULL,
  `directory` text NOT NULL,
  `path` text,
  `title` text NOT NULL,
  `version` text NOT NULL,
  `share_url` text,
  `summary_additions` integer,
  `summary_deletions` integer,
  `summary_files` integer,
  `summary_diffs` text,
  `metadata` text,
  `cost` real DEFAULT 0 NOT NULL,
  `tokens_input` integer DEFAULT 0 NOT NULL,
  `tokens_output` integer DEFAULT 0 NOT NULL,
  `tokens_reasoning` integer DEFAULT 0 NOT NULL,
  `tokens_cache_read` integer DEFAULT 0 NOT NULL,
  `tokens_cache_write` integer DEFAULT 0 NOT NULL,
  `revert` text,
  `permission` text,
  `agent` text,
  `model` text,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  `time_compacting` integer,
  `time_archived` integer,
  CONSTRAINT `fk_session_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE
);
CREATE TABLE `todo` (
  `session_id` text NOT NULL,
  `content` text NOT NULL,
  `status` text NOT NULL,
  `priority` text NOT NULL,
  `position` integer NOT NULL,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  CONSTRAINT `todo_pk` PRIMARY KEY(`session_id`, `position`),
  CONSTRAINT `fk_todo_session_id_session_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE
);
CREATE TABLE `session_share` (
  `session_id` text PRIMARY KEY,
  `id` text NOT NULL,
  `secret` text NOT NULL,
  `url` text NOT NULL,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  CONSTRAINT `fk_session_share_session_id_session_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE
);
CREATE UNIQUE INDEX `event_aggregate_seq_idx` ON `event` (`aggregate_id`,`seq`);
CREATE INDEX `event_aggregate_type_seq_idx` ON `event` (`aggregate_id`,`type`,`seq`);
CREATE UNIQUE INDEX `permission_project_action_resource_idx` ON `permission` (`project_id`,`action`,`resource`);
CREATE INDEX `message_session_time_created_id_idx` ON `message` (`session_id`,`time_created`,`id`);
CREATE INDEX `part_message_id_id_idx` ON `part` (`message_id`,`id`);
CREATE INDEX `part_session_idx` ON `part` (`session_id`);
CREATE INDEX `session_input_session_pending_delivery_seq_idx` ON `session_input` (`session_id`,`promoted_seq`,`delivery`,`admitted_seq`);
CREATE UNIQUE INDEX `session_input_session_admitted_seq_idx` ON `session_input` (`session_id`,`admitted_seq`);
CREATE UNIQUE INDEX `session_input_session_promoted_seq_idx` ON `session_input` (`session_id`,`promoted_seq`);
CREATE UNIQUE INDEX `session_message_session_seq_idx` ON `session_message` (`session_id`,`seq`);
CREATE INDEX `session_message_session_type_seq_idx` ON `session_message` (`session_id`,`type`,`seq`);
CREATE INDEX `session_message_session_time_created_id_idx` ON `session_message` (`session_id`,`time_created`,`id`);
CREATE INDEX `session_message_time_created_idx` ON `session_message` (`time_created`);
CREATE INDEX `session_project_idx` ON `session` (`project_id`);
CREATE INDEX `session_workspace_idx` ON `session` (`workspace_id`);
CREATE INDEX `session_parent_idx` ON `session` (`parent_id`);
CREATE INDEX `todo_session_idx` ON `todo` (`session_id`);
```
(+ `migration` journal table, §4.3.) Drizzle source of truth for TS-side typing: `core/src/session/sql.ts:22-176`, `core/src/project/sql.ts`, `core/src/event/sql.ts`, `core/src/permission/sql.ts`, `core/src/control-plane/workspace.sql.ts`, `core/src/credential/sql.ts`, `core/src/account/sql.ts`, `core/src/share/sql.ts`, `core/src/data-migration.sql.ts`. drizzle-kit config: `core/drizzle.config.ts` (`schema: ["./src/**/*.sql.ts", "./src/**/sql.ts"]`).

### 4.5 Column codecs (TS side; Rust must encode identically)

| Codec | Columns | Encoding | Ref |
|---|---|---|---|
| `text({mode:"json"})` | `session.summary_diffs, metadata, revert, permission, model`; `message.data`; `part.data`; `session_message.data`; `session_input.prompt`; `session_context_epoch.snapshot`; `event.data`; `project.commands`; `workspace.extra`; `credential.value` | `JSON.stringify(v)` (compact) / `JSON.parse`; SQL `NULL` for null/undefined | drizzle (LIB) |
| `integer({mode:"boolean"})` | `credential.active`, `control_account.active` | `1`/`0` | drizzle (LIB) |
| `absoluteColumn` | `project.worktree`, `project_directory.directory` | win32: `\` → `/` on write; must match `^[A-Za-z]:/` or start `//` (else POSIX-absolute) or throw `Path is not absolute`; on read (win32, windows-style) `/` → `\` | `core/src/database/path.ts:5-41` |
| `directoryColumn` | `session.directory` | as absolute, but empty string allowed (legacy) | `path.ts:43-59` |
| `pathColumn` | `session.path` | win32: `\` → `/` both directions (stays `/` in memory) | `path.ts:61-75` |
| `absoluteArrayColumn` | `project.sandboxes` | `JSON.stringify(paths.map(absolute))`; read → JSON.parse, each `toPlatform` | `path.ts:77-91` |
| `Timestamps` | every `time_created` (`$default(Date.now)`) / `time_updated` (`$onUpdate(Date.now)`) | epoch **milliseconds** integer. drizzle LIB: `$onUpdate` also fills on insert when no value; any `update()` without explicit `time_updated` sets now (session project re-pointing explicitly preserves it via `sql\`time_updated\``, `opencode/src/project/project.ts:179`) | `core/src/database/schema.sql.ts:3-10` |
| `workspace.time_used` | `$default(Date.now)` | ms | `core/src/control-plane/workspace.sql.ts:17-19` |
| `project.vcs` | text `"git"` or NULL | | `opencode/src/project/project.ts:262` |
| `event.type` | `"<type>.<version>"` | `schema/src/event.ts:94-96`, `core/src/event.ts:343` |
| `project_directory.type` | `"main"\|"root"\|"git_worktree"` or NULL | `core/src/project/sql.ts:28` |

Row ↔ domain mapping for sessions: `opencode/src/session/session.ts:59-160` (`fromRow`/`toRow`). JSON shapes of `message.data` / `part.data`: `schema/src/v1/session.ts` (`User`/`Assistant` discriminated by `role` `:332,453,490`; part `type` ∈ `text, reasoning, file, agent, compaction, subtask, retry, step-start, step-finish, tool, snapshot, patch` `:87-357`), stored **without** `id`/`sessionID` (message) and `id`/`sessionID`/`messageID` (part) (`core/src/session/sql.ts:18-20`). `session_message.data` omits `type`,`id` (`:18`). Full payload schemas belong to the session spec.

### 4.6 Migration list (`core/src/database/migration.gen.ts:5-42`; each file `core/src/database/migration/<id>.ts`, `id` at line 5)

| # | id | Effect |
|---|---|---|
| 1 | `20260127222353_familiar_lady_ursula` | create `project`(no `icon_url_override`,`commands`), `message`, `part`, `permission`(old: `project_id` PK, `data`), `session`(base cols), `todo`, `session_share`; indexes `message_session_idx`, `part_message_idx`, `part_session_idx`, `session_project_idx`, `session_parent_idx`, `todo_session_idx` |
| 2 | `20260211171708_add_project_commands` | `project ADD commands text` |
| 3 | `20260213144116_wakeful_the_professor` | create `control_account` |
| 4 | `20260225215848_workspace` | create `workspace(id, branch, project_id, config NOT NULL)` |
| 5 | `20260227213759_add_session_workspace_id` | `session ADD workspace_id`; index `session_workspace_idx` |
| 6 | `20260228203230_blue_harpoon` | create `account`(+`selected_org_id`), `account_state(id integer PRIMARY KEY NOT NULL, active_account_id, FK … ON UPDATE no action ON DELETE set null)` |
| 7 | `20260303231226_add_workspace_fields` | `workspace ADD type text NOT NULL, name, directory, extra`; `DROP COLUMN config` |
| 8 | `20260309230000_move_org_to_state` | `account_state ADD active_org_id`; copy from `account.selected_org_id`; drop it |
| 9 | `20260312043431_session_message_cursor` | drop `message_session_idx`, `part_message_idx`; create `message_session_time_created_id_idx`, `part_message_id_id_idx` |
| 10 | `20260323234822_events` | create `event_sequence(aggregate_id, seq)`, `event` |
| 11 | `20260410174513_workspace-name` | rebuild `workspace` via `__new_workspace` with `name DEFAULT '' NOT NULL` (copies `name` if column exists else `''`) |
| 12 | `20260413175956_chief_energizer` | create `session_entry` + 3 indexes |
| 13 | `20260423070820_add_icon_url_override` | single `tx.run` with **two statements**: `ADD icon_url_override`; `UPDATE … SET icon_url_override = icon_url WHERE icon_url IS NOT NULL` (whether bun executes the 2nd statement: `UNKNOWN`, §10) |
| 14 | `20260427172553_slow_nightmare` | create `session_message`(no `seq`) + indexes; drop `session_entry` + its indexes |
| 15 | `20260428004200_add_session_path` | `session ADD path` |
| 16 | `20260501142318_next_venus` | `session ADD agent, model` |
| 17 | `20260504145000_add_sync_owner` | `event_sequence ADD owner_id` |
| 18 | `20260507164347_add_workspace_time` | `workspace ADD time_used integer NOT NULL DEFAULT 0` |
| 19 | `20260510033149_session_usage` | `session ADD cost real DEFAULT 0 NOT NULL, tokens_* integer DEFAULT 0 NOT NULL`; backfill sums via `json_extract(message.data,'$.cost' / '$.tokens.input|output|reasoning|cache.read|cache.write')` where `$.role='assistant'` |
| 20 | `20260511000411_data_migration_state` | create `data_migration` |
| 21 | `20260511173437_session-metadata` | `session ADD metadata` unless column already exists (shipped earlier as `20260530232709_lovely_romulus`) |
| 22 | `20260601010001_normalize_storage_paths` | Windows paths: `REPLACE(…, char(92), '/')` on `project.worktree`, `project.sandboxes` (`\\`→`/` in JSON), `session.directory`, `session.path` (guarded by drive/UNC patterns) |
| 23 | `20260601202201_amazing_prowler` | `DROP TABLE permission` |
| 24 | `20260602002951_lowly_union_jack` | create new `permission(id, project_id, action, resource, timestamps)` + unique index |
| 25 | `20260602182828_add_project_directories` | create `project_directory(type text NOT NULL)` |
| 26 | `20260603001617_session_message_projection_indexes` | drop 2 session_message idx; create `event_aggregate_seq_idx`(non-unique), `session_message_session_time_created_id_idx`, `session_message_session_type_time_created_id_idx` |
| 27 | `20260603040000_session_message_projection_order` | `DELETE FROM session_message`; `ADD COLUMN seq integer NOT NULL`; new seq indexes |
| 28 | `20260603141458_session_input_inbox` | create `session_input(seq AUTOINCREMENT PK, id UNIQUE, …)` + index |
| 29 | `20260603160727_jittery_ezekiel_stane` | index reshuffle (`IF NOT EXISTS`) |
| 30 | `20260604172448_event_sourced_session_input` | wipe `session_input, session_message, event, event_sequence, workspace`; null `session.workspace_id`; unique `event_aggregate_seq_idx`, `session_message_session_seq_idx`; rebuild `session_input` (text PK, `admitted_seq`) + 3 indexes |
| 31 | `20260605003541_add_session_context_snapshot` | create `session_context_epoch(+replacement_seq, revision DEFAULT 0)` |
| 32 | `20260605042240_add_context_epoch_agent` | `ADD agent text DEFAULT 'build' NOT NULL` |
| 33 | `20260611035744_credential` | create `credential(connector_id NOT NULL, method_id NOT NULL, active DEFAULT false NOT NULL)` + partial unique index `credential_connector_active_idx … WHERE "credential"."active" = 1` |
| 34 | `20260611192811_lush_chimera` | drop index + table `credential`; recreate in final shape |
| 35 | `20260612174303_project_dir_strategy` | `ADD strategy`; rebuild `project_directory` with nullable `type` |
| 36 | `20260622142730_simplify_session_context_epoch` | drop columns `agent`, `replacement_seq`, `revision` |
| 37 | `20260622170816_reset_v2_session_state` | wipe v2 session/event/workspace state |
| 38 | `20260622202450_simplify_session_input` | same wipe again |

Note `opencode/migration/20260511173437_session-metadata/{migration.sql,snapshot.json}` is a stale drizzle-kit artefact, not loaded at runtime.

### 4.7 Fresh-vs-migrated drift (both shapes exist in the wild)
- `workspace.time_used`: fresh `integer NOT NULL` (no default); migrated `… NOT NULL DEFAULT 0` (#18).
- `account_state`: fresh `id integer PRIMARY KEY` + named FK; migrated `id integer PRIMARY KEY NOT NULL` + unnamed FK `ON UPDATE no action` (#6).
- **Column order differs** for `session` (migrated: base cols…`time_archived`, then `workspace_id, path, agent, model, cost, tokens_*, metadata`), `project` (`commands`, `icon_url_override` appended), `session_message` (`seq` last), `event_sequence`, `account_state`. Never use positional `SELECT *`.
- Migrated DBs keep `__drizzle_migrations` if they came from the drizzle era.

### 4.8 v1→v2 data migration
- `data_migration(name, time_completed)` table is created (#20, `core/src/data-migration.sql.ts:3-6`) but **no code in v1.19.5 reads or writes it** (repo-wide grep: only schema files reference `DataMigrationTable`).
- No JSON-storage → SQLite importer exists in v1.19.5 (only the legacy JSON-to-JSON migrations of §5.1).
- Credentials are **not** migrated from `auth.json` into `credential` in 1.19.5 (newer DBs show `20260805200742_import_legacy_credentials`, not in this tree).
- V2 session tables were wiped by #30, #37, #38 — v2 state is disposable in this version.

**Rust mapping (suggestion):** `rusqlite` with `bundled` (needs ≥3.35 for `DROP COLUMN`, JSON1 for `json_extract`), single `Mutex<Connection>` (mirrors the 1-permit semaphore), same PRAGMAs, `busy_timeout(5000)`. Embed `schema.gen.ts` DDL + every migration's SQL verbatim keyed by id; reproduce the journal seeding logic. Add a guard: if `migration` contains ids unknown to the binary, refuse writes (TS does not — deliberate deviation recommended, see R1). `sqlx` is workable but async-pool semantics diverge from the single-connection model.

---

## 5. Non-DB storage

### 5.1 Legacy JSON storage (`opencode/src/storage/storage.ts`)
- Root `<data>/storage`; key `string[]` → `path.join(root, ...key) + ".json"` (`:63-65`). Writes `JSON.stringify(v, null, 2)` via `writeWithDirs` (mkdir on ENOENT) (`:251-253`); reads `JSON.parse`; in-process `TxReentrantLock` per file (no cross-process lock).
- Marker `<data>/storage/migration` = decimal count of completed steps (`:225-240`); `MIGRATIONS.length = 2`; a failing step logs and stops.
  - Step 1 (`:82-181`): from `<data>/project/<dir>/storage/session/{info,message,part}/…` (pre-1.0 layout) → `storage/project/<rootCommit>.json` `{id, vcs:"git", worktree, time:{created, initialized}}`, `storage/session/<projectID>/<sessionID>.json`, `storage/message/<sessionID>/<msgID>.json`, `storage/part/<msgID>/<partID>.json`; project id = first sorted line of `git rev-list --max-parents=0 --all` in the worktree found via any message's `path.root`.
  - Step 2 (`:182-210`): for `session/*/*.json` with `summary.diffs` → `session_diff/<sessionID>.json` (array) and rewrite session with `summary:{additions, deletions}`.
- Only live key in 1.19.5: `["session_diff", sessionID]` written by revert (`opencode/src/session/revert.ts:77`). Observed on disk: `storage/migration` = `2`, `storage/session_diff/`.

### 5.2 TUI/state files
`kv.json` (JSON object), `model.json`, `session.json`: written by `writeJsonAtomic` = write `<file>.<pid>.<uuid>.tmp` (compact JSON) then `rename` (`tui/src/util/persistence.ts:22-33`); `kv.json` guarded by Flock key `tui-kv:<file>` (`tui/src/context/kv.tsx:16`). `*.jsonl` files appended line-wise (`persistence.ts:17-20`). Field shapes: TUI spec (`tui/src/context/local.tsx`, `tui/src/prompt/*.tsx`).

### 5.3 Locks (`core/src/util/flock.ts`, `core/src/util/effect-flock.ts`)
Lock = **directory** `<state>/locks/<sha1hex(key)>.lock` created with `mkdir` (mode `0o700`) (`flock.ts:148-155,321`), containing `heartbeat` (empty file; mtime touched every `max(100, staleMs/3)` ms) and `meta.json` `{token, pid, hostname, createdAt(ISO)}` (2-space JSON, `wx`) (`:206-221,225-231`). Stale if newest of heartbeat/meta/dir mtime older than `staleMs` (default 60 000) (`:25-30,127-146`); stale breaking serialised through sibling dir `<lock>.breaker` (`:165-197`). Release verifies `token` (`:241-262`). Backoff 100 ms ×1.7 up to 2 000 ms, timeout 5 min. Keys in use: `mcp-auth:<path>`, `models-dev:<path>`, `npm-install:<dir>`, `tui-kv:<path>` (+ others outside scope).

### 5.4 Logs
`<data>/log/opencode.log`, append (`flag:"a"`), one line per entry: space-separated `key=value`, first keys `timestamp`, `level`, `run`, then message/cause/spans/annotations flattened with dotted keys; value printed raw if `/^[^\s="\\]+$/` else `JSON.stringify` (`core/src/observability/logging.ts:6-51`). No rotation in this code.

**Rust mapping (suggestion):** port the mkdir-lock protocol exactly (not `flock(2)`/`fs4`) so TS and Rust processes interlock; `sha1` crate for names; `tempfile::NamedTempFile::persist` for atomic writes (compatible with readers). Keep 2-space pretty JSON where TS uses it (diff-friendly, and `updateGlobal` compares serialized text).

---

## 6. IDs, project identity, timestamps

### 6.1 Sortable ID algorithm
Two identical implementations with **separate counters**: `schema/src/identifier.ts:1-30` (used by `@opencode-ai/schema` IDs and core) and `opencode/src/id/id.ts:16-70` (used by `opencode/src`).
```
create(descending, ts = Date.now()):
  if ts != lastTimestamp { lastTimestamp = ts; counter = 0 }
  counter += 1
  cur  = ts * 0x1000 + counter                 # BigInt, unbounded
  v    = descending ? ~cur : cur               # BigInt NOT = -(cur+1)
  hex  = 6 bytes, byte_i = (v >> (40 - 8*i)) & 0xff, i=0..5, lowercase, zero-padded  → 12 chars
  rand = 14 chars, each = "0-9A-Za-z"[randomByte % 62]   (crypto RNG)
  id   = prefix + "_" + hex + rand             # 26 chars after "_"
```
- Only the **low 48 bits** of `ts*4096+counter` survive (ts*4096 ≈ 2^52.6 in 2026) → lexical order wraps every 2^36 ms (~795 days). Descending = `(2^48-1) - (cur mod 2^48)`.
- `timestamp(id)` = `parseInt(hex,16) / 4096` (integer division) — returns `ts mod 2^36`, not the real time (`core/src/id/id.ts:39-45`, `opencode/src/id/id.ts:72-78`).
- `given` IDs are accepted if they start with the prefix (`core/src/id/id.ts:24-33`).

### 6.2 Prefixes

| Entity | Prefix | Direction | Ref |
|---|---|---|---|
| Session | `ses` | **descending** | `schema/src/session-id.ts:8` |
| Message (v1 & v2) | `msg` | ascending | `schema/src/v1/session.ts:19`, `schema/src/session-message.ts:14`, `opencode/src/session/schema.ts:13` |
| Part | `prt` | ascending | `schema/src/v1/session.ts:25`, `opencode/src/session/schema.ts:22` |
| Event | `evt` | ascending | `schema/src/event.ts:11`, `opencode/src/bus/global.ts:16` |
| Permission request | `per` | ascending | `schema/src/permission.ts:12`, `schema/src/v1/permission.ts:12` |
| Saved permission (`permission.id`) | `psv` | ascending | `schema/src/permission-saved.ts:10` |
| Question | `que` | ascending | `schema/src/question.ts:13` |
| PTY | `pty` | ascending | `schema/src/pty.ts:13` |
| Workspace | `wrk` | ascending | `schema/src/workspace-id.ts:8` |
| Credential | `cred` | ascending | `schema/src/credential.ts:11` |
| Integration connection | `con` | ascending | `schema/src/integration.ts:104` |
| Background job | `job` | ascending | `core/src/background-job.ts:205` |
| Tool call / tool-output file | `tool` | ascending | `opencode/src/tool/schema.ts:12`, `core/src/tool-output-store.ts:130` |
Prefix table in code: `core/src/id/id.ts:3-14`, `opencode/src/id/id.ts:3-14`.

Non-generated IDs: `Project.ID` (§6.3), `"global"` (`schema/src/project-id.ts:6`); `account.id` = server-provided (`opencode/src/account/account.ts:444-452`); `account_state.id` = `1` (`opencode/src/account/repo.ts:13`); `session.slug` = `<adjective>-<noun>` random from fixed lists (`core/src/util/slug.ts:2-73`); default titles `"New session - " + ISO` / `"Child session - " + ISO` (`opencode/src/session/session.ts:48-49,521`); `session.version` = `InstallationVersion`.

### 6.3 Project ID derivation (`core/src/project.ts:110-122`, persisted by `opencode/src/project/project.ts:213-309`)
1. `repo = git.repo.discover(dir)`: nearest `.git` (file or dir) walking up to filesystem root; in its parent run `git rev-parse --show-toplevel`, `--git-dir`, `--git-common-dir`; fail → no repo (`core/src/git.ts:184-203`).
2. No repo → `id = "global"`, directory = filesystem root of `dir`; stored `worktree = "/"` (`opencode/src/project/project.ts:217`).
3. `previous` = trimmed contents of `<git-common-dir>/opencode` (`core/src/project.ts:65-71`).
4. `id = remote ?? previous ?? root ?? "global"`:
   - `remote`: `git remote get-url origin` → normalise → `sha1hex("git-remote:" + host.toLowerCase() + "/" + path)` (`:73-103`, `core/src/util/hash.ts:4-6`). Normalise: WHATWG `new URL()`; `file:` → none; host = `hostname`, path = `pathname` minus leading `/+`, trailing `.git` / `.git/`, trailing `/+`. If URL parse throws → scp form `^([^@/:]+@)?([^/:]+):(.+)$` (host = group 2, path = group 3).
   - `root`: lexicographically first of `git rev-list --max-parents=0 HEAD` (`core/src/git.ts:211-219`).
5. If `previous` ≠ new id (and previous ≠ `global`): in an IMMEDIATE transaction copy project row to new id (if new absent), delete `project_directory` rows of old id, re-point `session.project_id` (preserving `time_updated`) and `workspace.project_id`, delete old project row (`opencode/src/project/project.ts:144-192`).
6. Upsert project row; sandboxes = existing ∪ {repo worktree if ≠ stored worktree}, filtered by `exists` (`:225-288`); sessions with `project_id='global'` and `directory = repo worktree` re-pointed (`:291-298`); `project_directory` upserted; then write id to `<git-common-dir>/opencode` (`:306-308`, `core/src/project.ts:124-126`).
Legacy (pre-SQLite) project id = root commit via `rev-list --all` (§5.1) — explains the `previous` cache.

### 6.4 Timestamps
All persisted times are integer epoch **milliseconds** (`Date.now()`), in DB and JSON (`time.created`, `expires`, `token_expiry`, …). Exceptions: lock `meta.json.createdAt` ISO-8601; log `timestamp` (Effect logger format, `UNKNOWN` exact — read from a log line); `account` login `expiry` = `now + expires_in` ms (`opencode/src/account/account.ts:441`).

**Rust mapping (suggestion):** `u64` arithmetic: `cur = (ts << 12) + counter` (wrapping), `v = if desc { !cur } else { cur } & 0xFFFF_FFFF_FFFF`, `format!("{:012x}", v)`; one global `Mutex<(u64,u64)>`; `rand::rngs::OsRng` bytes `% 62`. `sha1` + `hex`. Git via `std::process::Command` with identical arguments (gix possible but must reproduce `rev-parse` path resolution). URL normalisation with the `url` crate (WHATWG) — verify parity for `ssh://`, scp-like and scheme-like strings such as `github.com:owner/repo` (parses as scheme `github.com:` → empty host → no remote id in TS).

---

## 7. Credentials & accounts

### 7.1 `auth.json` (provider credentials, v1.19.5 runtime) — `opencode/src/auth/index.ts`
- Path `<data>/auth.json` (`:10`). Shape: `Record<string, Info>`; key = provider ID, or well-known URL (trailing `/` stripped on `set`) (`:73-81`).
- `Info` union on `type` (`:14-36`):
  - `{"type":"oauth","refresh":string,"access":string,"expires":int≥0,"accountId"?:string,"enterpriseUrl"?:string}`
  - `{"type":"api","key":string,"metadata"?:Record<string,string>}`
  - `{"type":"wellknown","key":string,"token":string}` (`key` = env var name exposed to config `{env:}` substitution, `config.ts:371-373`)
- Read: `JSON.parse`; missing/invalid file → `{}`; entries failing decode silently dropped (`:65-66`). `OPENCODE_AUTH_CONTENT` short-circuits (§2.2).
- Write: full-object rewrite `JSON.stringify(data, null, 2)` (non-atomic `writeFileString`), then `chmod 0o600` (`:79,88`; `core/src/fs-util.ts:110-114`). No lock. `set` deletes `key` (if different) and `norm + "/"`; `remove` deletes `key` and `norm`.
- Constant `OAUTH_DUMMY_KEY = "opencode-oauth-dummy-key"` (`:8`).

### 7.2 `mcp-auth.json` — `opencode/src/mcp/auth.ts`
Path `<data>/mcp-auth.json` (`:37`); `Record<mcpName, {tokens?:{accessToken, refreshToken?, expiresAt?:number, scope?}, clientInfo?:{clientId, clientSecret?, clientIdIssuedAt?:number, clientSecretExpiresAt?:number}, codeVerifier?, oauthState?, serverUrl?}>` (`:9-31`); invalid → `{}`; writes 2-space JSON + `0o600` under Flock `mcp-auth:<path>` (`:38,73-81`). `expiresAt` unit: `UNKNOWN` (check `opencode/src/mcp/oauth-provider.ts` writer).

### 7.3 `credential` table (v2 integrations) — `core/src/credential.ts`
`value` JSON: `{"type":"oauth","methodID":string,"refresh","access","expires":int≥0,"metadata"?:object}` | `{"type":"key","key":string,"metadata"?:object}` (`schema/src/credential.ts:15-35`). `create` = delete all rows with same `integration_id` then insert (`id` `cred_…`, `label` default `"default"`) in one transaction (`core/src/credential.ts:94-121`). Rows with NULL `integration_id` (legacy connector rows) are invisible (`:56-64`). Ordered by `time_created`.

### 7.4 Console account — `account`, `account_state` tables
Tokens stored **plaintext** (`access_token`, `refresh_token`), `token_expiry` ms; singleton `account_state` row `id=1` holds `active_account_id`, `active_org_id` (`opencode/src/account/repo.ts:13,52-66`). `control_account` is marked `// LEGACY` (`core/src/account/sql.ts:24`) — no live writer identified (`UNKNOWN`, grep `ControlAccountTable` usages to confirm).

**Rust mapping (suggestion):** `serde_json::Map` round-trip that preserves unknown entries in `auth.json` (TS drops invalid entries only from the in-memory view but rewrites the file from that view on `set` → invalid entries are **lost** on write; replicate or document). Unix perms via `std::os::unix::fs::PermissionsExt::from_mode(0o600)`; on Windows Node `chmod(0o600)` only affects the read-only attribute → no-op. Do not move secrets to an OS keyring without a dual-write path.

---

## 8. v1 / v2 transition map (what 1.19.5 uses)

| Area | v1 (used by shipped `opencode`) | v2 (present, partially wired) |
|---|---|---|
| Config | `opencode/src/config/config.ts` + `core/src/v1/config/*` schema; accepts some v2 keys via `v2-compat.ts` | `core/src/config.ts` + `core/src/config/*`; read-only per location; V1 files auto-migrated in memory |
| Config dirs precedence | `.opencode` nearest-first (farther wins) | `.opencode` farthest-first (nearer wins) |
| Global config dir | always XDG `Global.Path.config` (+ `OPENCODE_CONFIG_DIR` as extra dir) | `OPENCODE_CONFIG_DIR` **replaces** XDG dir |
| Sessions | `session`/`message`/`part` rows | `event`(+`event_sequence`) source of truth + projections `session_message`, `session_input`, `session_context_epoch` (wiped by migrations; experimental) |
| Credentials | `auth.json` | `credential` table |
| Permissions saved | `permission` table (`psv_` ids) | same table |
| IDs | `opencode/src/id/id.ts` | `schema/src/identifier.ts` (same algorithm) |
| Storage | SQLite + `storage/session_diff` | SQLite only |
| `data_migration` | created, unused | — |

---

## 9. Porting risks (ranked)

| # | Risk | Mitigation |
|---|---|---|
| R1 | Operator's real DB/data dir is from `@opencode/cli 2.1.2` (40+ newer migrations, dropped/renamed tables). A 1.19.5-schema Rust binary (and TS 1.19.5 itself) silently "accepts" the newer journal and then fails on missing tables (`session_input`, `session_context_epoch`, `data_migration`) or corrupts v2 tables. | Decide the compatibility target first (1.19.5 vs 2.x). Add an explicit "unknown migration ids → read-only/abort" guard. Test against a copy, never the live DB. |
| R2 | Two config loaders with different precedence; V1 side effects (writes `$schema`, `.gitignore`, `opencode.jsonc`, npm installs, TOML migration, tui.json migration). | Port V1 semantics byte-for-byte; gate each side effect behind a compatibility switch; golden tests comparing merged output with TS on fixture trees. |
| R3 | Path normalisation in DB on Windows (`/` stored, `\` in memory, UNC `//`), plus fresh-vs-migrated column-order drift. | Implement the 4 codecs of §4.5; always use named columns. |
| R4 | Env parsing differences (case-insensitive `truthy` vs case-sensitive fatal `Config.boolean`; win32-dependent defaults). | Per-variable parser table (§2.2) as code. |
| R5 | ID format quirks (48-bit truncation, descending sessions, `timestamp()` wrap). | Port exactly; property-test against TS output with fixed `ts`. |
| R6 | XDG paths on macOS/Windows (not platform dirs). | Hand-rolled resolver (§1). |
| R7 | `{env:}` raw insertion can produce invalid JSON; `{file:}` escaping; comment-skip only for `//` on the same line. | Port the regex pipeline literally before JSONC parse. |
| R8 | Cross-process locking uses mkdir+heartbeat directories; Rust `flock` would not interlock with TS. | Port §5.3 protocol. |
| R9 | `Bun.hash` (wyhash) used in `<cache>/skills/<hash>` dir names (`core/src/skill/discovery.ts:113`). | Port Bun's wyhash variant or accept cache miss (cache only). |
| R10 | Non-atomic writes of `auth.json`/config by TS; Rust atomic rename changes inode/ACLs (Windows) and file mode. | Re-apply `0o600` after rename; keep same file name. |

---

## 10. Required additional data / verification list

| Item | Status | How to resolve |
|---|---|---|
| xdg-basedir@5.1.0 exact empty-string handling for `XDG_*_HOME` | LIB, not in repo | `bun install` then read `node_modules/xdg-basedir/index.js` |
| remeda `mergeDeep` exact semantics (undefined, class instances) | LIB | read `node_modules/remeda/dist/mergeDeep.js` at the catalog version |
| drizzle `$onUpdate` firing on insert | LIB | read drizzle-orm `column-builder` at catalog version or run `bun -e` insert test |
| Migration #13: does `bun:sqlite` `query()` execute the second statement of a multi-statement string? | UNKNOWN | `bun -e 'import {Database} from "bun:sqlite"; const d=new Database(":memory:"); d.run("create table t(a)"); d.query("insert into t values(1); insert into t values(2)").all(); console.log(d.query("select count(*) c from t").get())'` |
| Published `https://opencode.ai/config.json` content for 1.19.5 | UNKNOWN | `bun install && bun packages/opencode/script/schema.ts /tmp/config.json /tmp/tui.json` at tag v1.19.5 |
