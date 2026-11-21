# 06 — User-facing clients (CLI, TUI, Desktop shell, App UI)

Source: opencode v1.19.5, read-only checkout `D:\Download\opecode\opencode-ts`. All `path:line` references are relative to `opencode-ts/packages/` unless prefixed otherwise. Target: `opencode-rs` — `clap` CLI, `ratatui` TUI, native GUI on gpui-kit (GPUI + gpui-component) replacing Electron + SolidJS.

Conventions: `UNKNOWN` = not determinable from the files read; each such item says how to resolve it. "Rust mapping" blocks are **suggestions**, not decisions. Server routes/SDK types/config schema are owned by sibling specs; this file references them only where a client depends on them.

## 0. Client topology (summary)

| Client | Entry | Server connection | Ref |
|---|---|---|---|
| `opencode [project]` (TUI) | `opencode/src/cli/cmd/tui.ts:111` → `@opencode-ai/tui` `run()` | in-process server in a Bun **Worker**, HTTP requests tunnelled over worker RPC to virtual origin `http://opencode.internal`; events via RPC channel `global.event`. Real HTTP only with `--port/--hostname/--mdns` | `opencode/src/cli/cmd/tui.ts:249-288`, `opencode/src/cli/tui/worker.ts:30-80` |
| `opencode attach <url>` | `opencode/src/cli/cmd/attach.ts:7` | HTTP + Basic auth to remote server | `attach.ts:114-146` |
| `opencode run` | `opencode/src/cli/cmd/run.ts:126` | in-process `Server.Default().app.fetch` (no socket) or `--attach` HTTP | `run.ts:933-951` |
| `opencode --mini` | `opencode/src/cli/cmd/run/*` | same as `run` (split-footer interactive) | `run.ts:868-931` |
| `opencode serve/web/acp` | `serve.ts`, `web.ts`, `acp.ts` | starts HTTP server (`Server.listen`) | §1.2 |
| V2 preview CLI (`lildax`) | `cli/src/index.ts` | background daemon registered in `<state>/server.json` | §1.6 |
| Desktop (Electron) | `desktop/src/main` | spawns server sidecar, renderer = `packages/app` | §3 |
| Web UI | `app` served by `opencode web` | browser → HTTP/SSE | §4 |

Porting order recommendation (suggestion): (1) CLI skeleton + `serve`/`run --format json` (testable headless parity), (2) TUI against the in-process router, (3) desktop native GUI P0 slice (§4 P0 table), (4) everything else.

## 1. CLI

Two CLIs exist in v1.19.5:

| Binary | Package | Parser | Status |
|---|---|---|---|
| `opencode` | `opencode/src/index.ts` | yargs (`opencode/src/index.ts:44-116`) | shipping v1 CLI — **port target** |
| `lildax` / `$OPENCODE_CLI_NAME` | `cli/src/index.ts` | `effect/unstable/cli` (`cli/src/commands/commands.ts:1`) | "OpenCode 2.0 preview command line interface" (`cli/src/commands/commands.ts:7`); bin name `lildax` (`cli/package.json:7-9`), overridden by build define `OPENCODE_CLI_NAME` (`cli/script/build.ts:87`) |

Launcher shims (Node scripts that locate and `spawn` the platform binary with `stdio: inherit`, forward SIGINT/SIGTERM/SIGHUP, propagate exit code/signal): `opencode/bin/opencode` (env `OPENCODE_BIN_PATH` override `:46`, cached `.opencode` next to script `:52`, AVX2 probe `:89-106`, musl probe via `ldd --version` `:139`, resolve order `:189`); `cli/bin/lildax.cjs` (same scheme, `:32-38`, package `@opencode-ai/cli-<platform>-<arch>`).

> Rust mapping (suggestion): one `clap` binary; the shim becomes unnecessary when distributing a native binary (keep `OPENCODE_BIN_PATH` only if npm distribution is kept). `cargo-dist` style per-target archives replace `@opencode-ai/cli-<os>-<arch>` packages.

### 1.1 Global behavior (`opencode`)

| Item | Value | Ref |
|---|---|---|
| script name | `opencode` | `opencode/src/index.ts:47` |
| wrap width | 100 | `:48` |
| `--help`, `-h` | show help | `:49-50` |
| `--version`, `-v` | prints `InstallationVersion` | `:51-52` |
| `--print-logs` | boolean; sets `OPENCODE_PRINT_LOGS=1` | `:53-56`, `:67` |
| `--log-level` | string, choices `DEBUG\|INFO\|WARN\|ERROR`; sets `OPENCODE_LOG_LEVEL` | `:57-61`, `:68` |
| `--pure` | boolean "run without external plugins"; sets `OPENCODE_PURE=1` | `:62-65`, `:69-71` |
| middleware env (always) | `AGENT=1`, `OPENCODE=1`, `OPENCODE_PID=<pid>`; starts `Heap.start()` | `:66-78` |
| `completion` | yargs shell completion script generator | `:80` |
| parser config | `populate--: true` (args after `--` land in `args["--"]`) | `:45` |
| strict | unknown args/commands are errors | `:116` |
| help rendering | if help text does not start with `"opencode "`, logo printed to **stderr** before help (`show()`) | `:35-43`, `:118-124` |

Command registration order (= help order): `acp, mcp, $0 (tui), attach, run, generate, debug, console, providers, agent, upgrade, uninstall, serve, web, models, stats, export, import, github, pr, session, plugin, db` (`opencode/src/index.ts:81-103`).

**Exit codes** (`opencode/src/index.ts:104-142`):
- yargs failure starting with `Unknown argument` / `Not enough non-option arguments` / `Invalid values:` → help printed, then `exit(1)` (`:104-115`).
- Any thrown error → `FormatError(e)` (`opencode/src/cli/error.ts:35`) printed via `UI.error` (`Error: ` prefix in bold red, `opencode/src/cli/ui.ts:121-125`); unknown errors print `Unexpected error` + message; `exitCode = 1` (`index.ts:129-136`).
- `CliError{message, exitCode?}` (`opencode/src/cli/effect-cmd.ts:13-18`) → custom exit code (default 1) (`error.ts:42-45`).
- `UICancelledError` (user cancelled a prompt) → empty message (`error.ts:121-124`), exit 1.
- `finally { process.exit() }` always forces exit to kill hanging subprocesses (`index.ts:137-142`).
- Formatted error kinds (port text verbatim): `MCPFailed`, `AccountServiceError`, `AccountTransportError`, `ProviderModelNotFoundError` (with suggestions), `ProviderInitError`, `ConfigJsonError`, `ConfigDirectoryTypoError`, `ConfigFrontmatterError`, `ConfigRemoteAuthError`, `ConfigInvalidError` (issues rendered `↳ msg path.joined`) — `error.ts:47-125`.

**Instance model** — `effectCmd` (`opencode/src/cli/effect-cmd.ts:69-96`): unless `instance:false`, the handler runs inside a loaded project `InstanceContext` for `directory` (default `process.cwd()`), disposed in `finally`. Commands with `instance:false`: `serve`, `web`, `db`, `db path`, `providers list`, `providers logout`, `console *`, `debug v2`. `run` uses `instance: !args.attach` (`run.ts:131`); `providers login` uses `instance: !args.url` (`providers.ts:303`).

**Shared network options** `withNetworkOptions` (`opencode/src/cli/network.ts:5-38`), used by `$0`, `acp`, `serve`, `web`:

| Flag | Type | Default | Ref |
|---|---|---|---|
| `--port` | number | `0` (random) | `network.ts:6-10` |
| `--hostname` | string | `127.0.0.1` | `:11-15` |
| `--mdns` | boolean | `false` (when true and hostname not set → `0.0.0.0`) | `:16-20`, `:69-73` |
| `--mdns-domain` | string | `opencode.local` | `:21-25` |
| `--cors` | string[] | `[]` (merged with config `server.cors`) | `:26-31`, `:74-76` |

Precedence: explicit CLI flag (detected by scanning `process.argv` before `--`, `network.ts:40-53`) > global config `server.{port,hostname,mdns,mdnsDomain,cors}` > yargs default (`network.ts:61-79`).

**Server auth for clients** (`opencode/src/server/auth.ts:18-45`): HTTP Basic, username `OPENCODE_SERVER_USERNAME` default `opencode`, password `OPENCODE_SERVER_PASSWORD`; `Authorization: Basic base64(user:pass)`.

### 1.2 Command reference (`opencode`)

Notation: `pos` = positional; `[]` optional, `<>` required; **H** = hidden.

#### `opencode [project]` — default command (TUI) — `opencode/src/cli/cmd/tui.ts:111-346`
| Arg/flag | Alias | Type | Default | Notes |
|---|---|---|---|---|
| `project` pos | | string | — | relative to `$PWD` (`tui.ts:105-109`) |
| network opts | | | | §1.1 |
| `--model` | `-m` | string | — | `provider/model` |
| `--continue` | `-c` | bool | — | |
| `--session` | `-s` | string | — | validated via `GET session` before start (`opencode/src/cli/tui/validate-session.ts:7-29`) |
| `--fork` | | bool | — | requires `-c` or `-s` (`tui.ts:231-235`) |
| `--prompt` | | string | — | stdin is concatenated if piped (`tui.ts:98-103`) |
| `--agent` | | string | — | |
| `--auto` | | bool | false | auto-approve non-denied permissions |
| `--yolo` H, `--dangerously-skip-permissions` H | | bool | false | aliases of `--auto` (`tui.ts:332`) |
| `--mini` | | bool | false | split-footer mode → `runMini()` (`tui.ts:191-215`); rejects network flags (`:192-199`) |
| `--replay` H / `--no-replay` | | bool | — | `--replay` is an error (`:184-188`); `--no-replay`, `--replay-limit <n>`, `--demo` H require `--mini` (`:217-226`) |
| `--replay-limit` | | number | — | |

Behavior: chdir to project, spawn a **Worker thread** (`OPENCODE_WORKER_PATH` define or `cli/tui/worker.{js,ts}`, `tui.ts:91-96`) that hosts the server; TUI talks to it via RPC-tunneled fetch at virtual base URL `http://opencode.internal` and an RPC event channel `global.event` (`tui.ts:26-52`, `:277-288`). If `--port`/`--hostname`/`--mdns` is given, the worker starts a real HTTP server (`rpc.server`) and the TUI uses HTTP + Basic auth (`tui.ts:272-283`). Worker RPC methods: `fetch, snapshot, server, checkUpgrade, reload, shutdown` (`opencode/src/cli/tui/worker.ts:30-78`). `SIGUSR2` → `reload` (config invalidate + dispose instances) (`tui.ts:255-258`, `worker.ts:63-71`). Shutdown timeout 5000 ms (`tui.ts:265`). Update check 1 s after start then every 10 min (`tui.ts:54-89`), never auto-installs (`opencode/src/cli/upgrade.ts:7-37`).

#### `opencode attach <url>` — `opencode/src/cli/cmd/attach.ts:7-148`
Flags: `--dir` (string; chdir if exists locally, else passed to server), `-c/--continue`, `-s/--session`, `--fork`, `-p/--password` (default `OPENCODE_SERVER_PASSWORD`), `-u/--username` (default `OPENCODE_SERVER_USERNAME` or `opencode`), `--mini`, `--replay` H, `--no-replay`, `--replay-limit`. Same validations as default command (`:63-112`). Runs the TUI with `url` directly (no worker).

#### `opencode run [message..]` — `opencode/src/cli/cmd/run.ts:126-954`
| Flag | Alias | Type | Default | Notes / ref |
|---|---|---|---|---|
| `message` pos | | string[] | `[]` | joined with spaces; args after `--` appended (`:273`, `:289-291`); piped stdin appended after message (`:40-50`, `:417-419`) |
| `--command` | | string | | runs slash command, `message` = arguments (`:835-851`) |
| `--continue` | `-c` | bool | | picks first root session from `session.list` (`:493`) |
| `--session` | `-s` | string | | `Session not found` → exit 1 (`:458-468`) |
| `--fork` | | bool | | requires `-c`/`-s` (`:426-429`) |
| `--share` H | | bool | | deprecated; prints "Session sharing has been discontinued" (`:536-538`) |
| `--model` | `-m` | string | | split at first `/` → `{providerID, modelID}` (`:31-38`) |
| `--agent` | | string | | unknown/subagent → warning + default agent (`:585-658`) |
| `--format` | | `default\|json` | `default` | §1.3 |
| `--file` | `-f` | string[] | | local: `file://` URL, mime `text/plain` or `application/x-directory`; attach mode: inlined `data:` URL, ≤10 MiB, directories rejected (`:59`, `:358-415`) |
| `--title` | | string | | `""` → first 50 chars of prompt + `...` (`:451-455`) |
| `--attach` | | string (URL) | | use remote server (`:347-356`) |
| `--password` | `-p` | string | env | Basic auth |
| `--username` | `-u` | string | env/`opencode` | |
| `--dir` | | string | | local: chdir; attach: remote path (`:335-346`) |
| `--port` | | number | | **declared but unused in handler** (`:209-212`) |
| `--variant` | | string | | reasoning effort variant |
| `--thinking` | | bool | `false` (non-interactive) / `true` (mini) | print reasoning (`:276`) |
| `--mini` H | | bool | false | only valid via `opencode --mini` (`:297-299`) |
| `--replay` H | | bool | true | |
| `--replay-limit` H | | number | | positive int, requires mini (`:309-318`) |
| `--interactive` | `-i` | bool | false | **declared but unused** — interactive is keyed on `args.mini` (`:237-242`, `:274`) |
| `--auto`, `--yolo` H, `--dangerously-skip-permissions` H | | bool | false | |
| `--demo` H | | bool | false | requires mini (`:301-303`) |

Non-interactive flow: in-process server via `Server.Default().app.fetch` at `http://opencode.internal` (`:938-951`) or SDK at `--attach`. New sessions get permission rules `question/plan_enter/plan_exit → deny *` (`:431-449`). Subscribes to event stream, then `session.prompt` (or `session.command`), consumes events until `session.status idle` for the session (`:783-789`). Permissions for the session tree (children via `session.created.parentID`, `:693-695`) are auto-`once` with `--auto` else auto-`reject` with warning (`:791-811`). Exit code 1 on prompt/command error or any `session.error` (`:829-866`). Validation errors use `die()` → `exit(1)` (`:277-280`).

Default-format output: header `> {agent} · {modelID}` once (`:697-708`); completed tools rendered inline/block via `run/tool.ts` `toolInlineInfo` (`:90-124`); text parts printed when `time.end` set (raw line when stdout not a TTY, `:743-754`); reasoning as dim italic `Thinking: …` (`:756-768`).

#### `opencode --mini` interactive split-footer mode — `opencode/src/cli/cmd/run/*` (18.5k lines)
Scrollback (immutable transcript) + mutable footer (prompt, status, permission/question UI, subagent tabs) (`run/types.ts:1-6`). Local mode uses in-process fetch (`run.ts:897-931`); attach mode via `attach --mini` (`attach.ts:81-95`). Footer slash commands: `/editor`, `/new`, `/skills`, `/exit` (`run/footer.prompt.tsx:415-435`); `/exit`, `/quit`, `:q` exit (`run/prompt.shared.ts:48`). Palette actions: `model, editor, skill, queued, subagent, variant.cycle, variant.list, slash, exit` (`run/footer.command.tsx:16-24`). Keybinds are read from the **TUI keybind config** (`tuiConfig.keybinds.get("prompt.clear" | "session.interrupt" | "prompt.editor" | "prompt.history.previous/next" | "command.palette.show" | "variant.cycle" | "session.background" | "session.child.first" | "session.queued_prompts")`, `run/footer.prompt.tsx:983-1051`, `run/footer.view.tsx:505-563`; autocomplete group `run.prompt.autocomplete`, `run/footer.prompt.tsx:1148`). Env: `OPENCODE_DIRECT_TRACE` (`run/trace.ts`), `OPENCODE_SHOW_TTFD` (`run/runtime.ts`). Demo slash commands `/help /permission /question /fmt` (`run/demo.ts:1148-1175`).

#### Server / protocol commands
| Command | Flags | Behavior | Ref |
|---|---|---|---|
| `serve` | network opts | warns (stdout) if `OPENCODE_SERVER_PASSWORD` unset; prints `opencode server listening on http://{host}:{port}`; runs forever | `opencode/src/cli/cmd/serve.ts:6-24` |
| `web` | network opts | as `serve` + logo; if host `0.0.0.0` prints `Local access`, `Network access` (non-internal IPv4, skips `172.*`), `mDNS` lines and opens `http://localhost:{port}`; else prints/open `server.url` | `opencode/src/cli/cmd/web.ts:9-84` |
| `acp` | network opts + `--cwd` (string, default `process.cwd()`) | sets `OPENCODE_CLIENT=acp`, starts HTTP server, speaks ACP ndjson over stdin/stdout via `@agentclientprotocol/sdk` `AgentSideConnection` | `opencode/src/cli/cmd/acp.ts:9-73` |
| `generate` (no describe → hidden) | — | prints OpenAPI JSON (prettier, printWidth 120) with `x-codeSamples` | `opencode/src/cli/cmd/generate.ts:5-54` |

#### Session/data commands
| Command | Args / flags | Output | Ref |
|---|---|---|---|
| `session list` | `-n/--max-count` number; `--format table\|json` (default `table`) | table `Session ID / Title / Updated` (min widths 20/25), piped to `less -R -S` when TTY & table & no `-n` (Windows: finds `less.exe` via `OPENCODE_GIT_BASH_PATH` or git install); json array `{id,title,updated,created,projectId,directory}` | `opencode/src/cli/cmd/session.ts:17-40`, `:70-147` |
| `session delete <sessionID>` | | | `session.ts:51-68` |
| `export [sessionID]` | `--sanitize` bool (redacts transcript/file data) | stdout `{"info":Session,"messages":[...]}` pretty JSON; status on stderr; interactive autocomplete picker when no id; `Session not found: <id>` exit 1 | `opencode/src/cli/cmd/export.ts:222-292` |
| `import <file>` | | reads JSON, rejects `http(s)://` share URLs, upserts session/message/part rows into SQLite; prints `Imported session: <id>` | `opencode/src/cli/cmd/import.ts:31-115` |
| `stats` | `--days` number (0 = today); `--tools` number; `--models` (bool or number, untyped); `--project` string (`""` = current) | box-drawn tables OVERVIEW / COST & TOKENS / … | `opencode/src/cli/cmd/stats.ts:49-393` |
| `db [query]` | `--format json\|tsv` (default `tsv`) | without query spawns `sqlite3 <db path>` interactively | `opencode/src/cli/cmd/db.ts:8-44` |
| `db path` | | prints DB path | `db.ts:45-52` |

#### Provider/model/agent/MCP/plugin commands
| Command | Args / flags | Ref |
|---|---|---|
| `providers` (alias `auth`) | requires subcommand | `opencode/src/cli/cmd/providers.ts:239-246` |
| `providers list` (alias `ls`) | | `providers.ts:248-297` |
| `providers login [url]` | `url` = well-known auth server (runs `auth.command`, stores token under `auth.env`); `-p/--provider` id/name; `-m/--method` method label | `providers.ts:299-489` |
| `providers logout [provider]` | | `providers.ts:491-534` |
| `models [provider]` | `--verbose` (JSON per model incl. cost); `--refresh` (refresh models.dev cache). Output `provider/model` per line, providers starting with `opencode` first | `opencode/src/cli/cmd/models.ts:8-66` |
| `agent create` | `--path`, `--description`, `--mode all\|primary\|subagent`, `--permissions` (alias `--tools`, comma list of `bash,read,edit,glob,grep,webfetch,task,todowrite,websearch,lsp,skill`), `-m/--model`. Fully non-interactive only if path+description+mode+permissions given; writes `<scope>/agents/*.md` (global = config dir, project = `<worktree>/.opencode`) | `opencode/src/cli/cmd/agent.ts:19-31`, `:33-232` |
| `agent list` | | `agent.ts:234-252` |
| `mcp add [name]` | `--url`; `--env KEY=VALUE` (array); `--header KEY=VALUE` (array); local command after `--`; name required when non-interactive | `opencode/src/cli/cmd/mcp.ts:429-657` |
| `mcp list` (alias `ls`) | | `mcp.ts:109-168` |
| `mcp auth [name]` / `mcp auth list` (alias `ls`) | OAuth | `mcp.ts:170-334` |
| `mcp logout [name]` | | `mcp.ts:336-427` |
| `mcp debug <name>` | OAuth connection debug | `mcp.ts:659-840` |
| `plugin <module>` (alias `plug`) | `-g/--global` (default false), `-f/--force` (default false) — install npm plugin + update config | `opencode/src/cli/cmd/plug.ts:178-230` |

#### Account / install / integration
| Command | Args / flags | Ref |
|---|---|---|
| `console` (hidden: `describe:false`) → `login [url]`, `logout [email]`, `switch`, `orgs`, `open` | | `opencode/src/cli/cmd/account.ts:177-264` |
| `upgrade [target]` | `target` like `0.1.48`/`v0.1.48`; `-m/--method` choices `curl,npm,pnpm,bun,brew,choco,scoop`; `--major` (install OpenCode 2, remove 1). Interactive (`@clack/prompts`) | `opencode/src/cli/cmd/upgrade.ts:7-105` |
| `uninstall` | `-c/--keep-config`, `-d/--keep-data`, `--dry-run`, `-f/--force` (all default false). Removes `Global.Path.{data,cache,config,state}` and `.opencode/bin` PATH lines from shell rc files (`fish`, `zsh`, `bash`, `ash`, `sh` lists honoring `XDG_CONFIG_HOME`) | `opencode/src/cli/cmd/uninstall.ts:25-353`, `:92-95`, `:238-256`, `:294-301` |
| `github install` | | `opencode/src/cli/cmd/github.ts:7-15` |
| `github run` | `--event` (mock event), `--token` (PAT). Env: `MODEL`, `VARIANT`, `GITHUB_RUN_ID`, `USE_GITHUB_TOKEN`, `GITHUB_TOKEN`, `OIDC_BASE_URL`, `PROMPT`, `MENTIONS` (default `/opencode,/oc`) | `github.ts:17-35`; `github.handler.ts:406,469,651-726` |
| `pr <number>` | runs `gh pr checkout N --branch pr/N --force`, adds fork remote for cross-repo PRs, then spawns `opencode` with inherited stdio; non-zero child exit → defect (exit 1) | `opencode/src/cli/cmd/pr.ts:8-92` |

#### `debug` subtree — `opencode/src/cli/cmd/debug/index.ts:19-87`
`config` (resolved config, `debug/config.ts`), `lsp diagnostics <file>` / `lsp symbols <query>` / `lsp document-symbols <uri>` (`debug/lsp.ts:8-50`), `rg files [--query --glob --limit]` / `rg search <pattern> [--glob[] --limit]` (`debug/ripgrep.ts:9-79`), `file search <query>` / `file read <path>` / `file list <path>` (`debug/file.ts:17-73`), `scrap` (list projects JSON, `debug/scrap.ts`), `skill` (`debug/skill.ts`), `snapshot track` / `snapshot patch <hash>` / `snapshot diff <hash>` (`debug/snapshot.ts:7-50`), `startup` (prints `performance.now()`), `agent <name> [--tool <id>] [--params <json|js>]` (`debug/agent.ts:5-27`), `v2` (catalog JSON, `debug/v2.ts:10-42`), `info` (version/os/terminal/plugins, `index.ts:49-77`), `paths` (prints `Global.Path` entries, `index.ts:79-87`), `wait` (sleep 1 day).

### 1.3 `run --format json` event schema

One JSON object per line on stdout (`opencode/src/cli/cmd/run.ts:668-681`):
```
{"type": <string>, "timestamp": <ms epoch>, "sessionID": <string>, ...payload}
```
| `type` | Payload | Emitted when | Ref |
|---|---|---|---|
| `tool_use` | `part: ToolPart` | tool part with `state.status` `completed` or `error` | `run.ts:714-715` |
| `step_start` | `part: StepStartPart` | | `:735-737` |
| `step_finish` | `part: StepFinishPart` | | `:739-741` |
| `text` | `part: TextPart` | text part with `time.end` set | `:743-744` |
| `reasoning` | `part: ReasoningPart` | reasoning part with `time.end` **and** `--thinking` | `:756-757` |
| `error` | `error: <session error object>` | `session.error` for this session, or prompt/command call error | `:779`, `:845`, `:862` |

Part shapes: `sdk/js/src/v2/gen/types.gen.ts` (`TextPart:378`, `ReasoningPart:410`, `ToolPart:534`, `StepStartPart:547`, `StepFinishPart:555`). Only parts of the root `sessionID` are emitted (`run.ts:712`). No explicit "done" event; stream ends at idle. Header line and permission warnings are suppressed only for the header (`args.format !== "json"`, `:701`) — permission auto-reject warnings still go to stdout via `UI.println` (`:801-805`) **(porting note: this pollutes JSON stdout; keep for parity or fix deliberately)**.

### 1.4 Environment variables read by the CLI layer

Flag registry: `core/src/flag/flag.ts:15-78` (truthy = `"true"` or `"1"`, `:3-6`).

| Var | Effect | Ref |
|---|---|---|
| `OPENCODE_SERVER_PASSWORD` / `OPENCODE_SERVER_USERNAME` | server Basic auth / client defaults | `core/src/flag/flag.ts:32-33`, `opencode/src/server/auth.ts:18-19` |
| `OPENCODE_CONFIG`, `OPENCODE_CONFIG_CONTENT`, `OPENCODE_CONFIG_DIR`, `OPENCODE_TUI_CONFIG`, `OPENCODE_DISABLE_PROJECT_CONFIG` | config sources | `flag.ts:21-22,54-65` |
| `OPENCODE_PURE` | no external plugins (`--pure`) | `flag.ts:66-68` |
| `OPENCODE_PRINT_LOGS`, `OPENCODE_LOG_LEVEL` | set by global flags | `index.ts:67-68` |
| `OPENCODE_DISABLE_AUTOUPDATE`, `OPENCODE_ALWAYS_NOTIFY_UPDATE` | update notice | `flag.ts:23-24`, `cli/upgrade.ts:23-30` |
| `OPENCODE_AUTO_HEAP_SNAPSHOT` | heap snapshot when RSS > 2 GiB (`heap-<pid>-<iso>.heapsnapshot`) | `flag.ts:19`, `opencode/src/cli/heap.ts:6-39` |
| `OPENCODE_GIT_BASH_PATH` | Windows `less.exe` lookup for pager | `flag.ts:20`, `session.ts:30` |
| `OPENCODE_DISABLE_MOUSE`, `OPENCODE_DISABLE_TERMINAL_TITLE`, `OPENCODE_EXPERIMENTAL_DISABLE_COPY_ON_SELECT` (default on for win32) | TUI behavior | `flag.ts:26,30,43-44` |
| `OPENCODE_SHOW_TTFD`, `OPENCODE_DIRECT_TRACE` | mini-mode diagnostics | `flag.ts:27`, `run/trace.ts` |
| `OPENCODE_CLIENT` | client id (`cli` default; `acp` set by `acp`) | `flag.ts:75-77`, `acp.ts:23` |
| `OPENCODE_DB`, `OPENCODE_MODELS_URL`, `OPENCODE_MODELS_PATH`, `OPENCODE_DISABLE_MODELS_FETCH`, `OPENCODE_DISABLE_AUTOCOMPACT`, `OPENCODE_DISABLE_PRUNE`, `OPENCODE_PERMISSION`, `OPENCODE_EXPERIMENTAL*`, `OPENCODE_WORKSPACE_ID`, `OPENCODE_FAKE_VCS`, `OPENCODE_DISABLE_FFF`, `OTEL_EXPORTER_OTLP_*` | core behavior (owned by core spec) | `flag.ts:16-50` |
| `PWD` | base for relative `[project]`, `--dir` | `tui.ts:105`, `run.ts:334` |
| `TERM`, `TERM_PROGRAM`, `TERM_PROGRAM_VERSION` | `debug info` | `debug/index.ts:56-59` |
| `SHELL`, `XDG_CONFIG_HOME` | `uninstall` rc-file detection | `uninstall.ts:238` |
| `OPENCODE_BIN_PATH` | launcher override | `opencode/bin/opencode:46` |

Global paths (`core/src/global.ts:10-23`): `data=$XDG_DATA_HOME/opencode`, `cache=$XDG_CACHE_HOME/opencode`, `config=$XDG_CONFIG_HOME/opencode` (overridable by `OPENCODE_CONFIG_DIR`, `:64`), `state=$XDG_STATE_HOME/opencode`, `bin=cache/bin`, `log=data/log`; home overridable via `OPENCODE_TEST_HOME` (`:18-19`). `xdg-basedir` semantics on Windows/macOS are the same XDG fallbacks (`~/.local/share`, etc.) — Rust must **not** use `dirs::data_dir()` platform-native paths or existing installs break.

### 1.5 Terminal output conventions
ANSI styles `UI.Style` (`opencode/src/cli/ui.ts:14-29`): highlight `96`, dim `90`, warning `93`, danger `91`, success `92`, info `94`, each with `_BOLD` (`;1`). Logo glyphs `opencode/src/cli/logo.ts`, rendered by `UI.logo(pad)` (`ui.ts:48`). Interactive prompts (intro/select/text/autocomplete/spinner/outro) via `@clack/prompts` (`opencode/src/cli/cmd/upgrade.ts:3`) in `agent`, `export`, `upgrade`, `uninstall`, `providers`, `mcp`.

### 1.6 V2 preview CLI (`packages/cli`) — `cli/src/commands/commands.ts:6-52`
| Command | Params | Behavior | Ref |
|---|---|---|---|
| (default) | — | ensure daemon, run TUI over HTTP (legacy 404 fallbacks for `/config/providers`, `/provider`, `/agent`, `/config`) | `cli/src/commands/handlers/default.ts`, `cli/src/tui.ts:7-37` |
| `api <operation \| method path>` | variadic 1..2; `-d/--data`; `-H/--header name:value` (≤100); `--param k=v` | resolves operationId via `GET /openapi.json`, else raw `METHOD /path`; unused params → query string; prints body | `commands.ts:9-24`, `handlers/api.ts:17-85` |
| `debug agents` | | `client.v2.agent.list` JSON | `handlers/debug/agents.ts` |
| `migrate` | | logs "No migrations to run." | `handlers/migrate.ts` |
| `service start\|restart\|status\|stop` | | daemon control; status prints `running <url>` or `stopped` | `commands.ts:30-36`, `handlers/service/*.ts` |
| `service password [value]` | | get/set password (setting stops daemon) | `handlers/service/password.ts` |
| `serve` | `--hostname` (default `127.0.0.1`), `--port` (int, optional → first free from 4096..65535), `--register` (default false) | prints `server listening on <addr>` | `commands.ts:43-50`, `handlers/serve.ts:65-96` |

Daemon protocol (`cli/src/services/daemon.ts`): registration file `<state>/server.json` `{id, version, url, pid}` mode 0600 (`:23-28`, `:40`, `:164-186`); password file `<state>/password` (32 random bytes base64url, 0600, `:41-56`); health = `client.v2.health.get` with 2 s timeout, `healthy === true` (`:66-72`); version must equal client version (`:74-78`); start spawns `<exe> [entry] serve --register` detached, polls 50 ms × 100 (`:110-135`); stop = SIGTERM, poll, SIGKILL (`:91-108`); registered server self-terminates if `server.json` is overwritten by another id (checked every 10 s, `:174-179`).

### 1.7 Rust mapping (suggestions)
- `clap` derive with one `enum Command`; default subcommand = TUI via `#[command(args_conflicts_with_subcommands = true)]` + `Option<Command>`; positional `[project]` on root. Hidden flags via `#[arg(hide = true)]`; `--no-replay` must be a distinct flag (yargs negation semantics: `--no-replay` sets `replay=false`).
- Replicate yargs quirks deliberately: `-v` = version (not verbose); `--yolo`/`--dangerously-skip-permissions` aliases; `providers` alias `auth`; `plugin` alias `plug`; `list` alias `ls`; args after `--` captured (`trailing_var_arg` / `last = true`).
- `completion` → `clap_complete` (yargs emits bash/zsh only; output format differs — `UNKNOWN` whether anyone parses it).
- Network flag precedence requires knowing "explicitly set" → use `ArgMatches::value_source() == CommandLine`.
- Interactive prompts → `cliclack` (closest to `@clack/prompts` look) or `inquire`.
- `run --format json`: serialize SDK part structs with `serde_json` exactly as server JSON (field order irrelevant, names must match).
- TUI/server split: replace the Bun Worker + RPC tunnel with an in-process `tokio` task exposing the same HTTP router through a `tower::Service` (no socket) + a broadcast channel for `global.event`; keep the external-HTTP path for `--port/--hostname/--mdns`.
- Exit: map `CliError{exit_code}` → `std::process::exit(code)`; always force-exit at end (child MCP processes may hang) — use `kill_on_drop` on child processes instead.

## 2. TUI

Scope: `tui/` (package `@opencode-ai/tui` v1.19.5, `tui/package.json:3-4`), TUI config/keybinds/themes, and the three launchers (`opencode/src/cli/cmd/tui.ts`, `opencode/src/cli/cmd/attach.ts`, `cli/src/tui.ts`). All paths are relative to `packages/`. "Purpose" columns are inferred from names and exports (medium confidence) unless a `path:line` is given.

### 2.1 Architecture

| Item | Fact | Ref |
|---|---|---|
| Render framework | OpenTUI (`@opentui/core`, `@opentui/solid`, `@opentui/keymap`), all pinned to `0.4.5` in the root catalog; reactive layer is SolidJS `1.9.10` (JSX → OpenTUI renderables) | `tui/package.json:55-57`, root `package.json:43-45,92` |
| Other deps | `clipboardy 4.0.0`, `diff`, `fuzzysort`, `opentui-spinner`, `remeda`, `strip-ansi 7.1.2`, `effect` | `tui/package.json:52-66` |
| Public entry | `tui/src/index.tsx:1` re-exports `run` and `TuiInput` from `app.tsx` | `tui/src/index.tsx:1-2` |
| `run` | `Effect.fn("Tui.run")`: creates the renderer, keymap, plugin runtime, mounts the provider tree, waits on the renderer `destroy` event, then writes the epilogue to stdout / error to stderr | `tui/src/app.tsx:168-347` |
| Renderer options | `createCliRenderer({ externalOutputMode:"passthrough", targetFps:60, exitOnCtrlC:false, useKittyKeyboard:{}, autoFocus:false, openConsoleOnError:false, useMouse: !OPENCODE_DISABLE_MOUSE && config.mouse, consoleOptions.keyBindings:[ctrl+y → copy-selection] })` | `tui/src/app.tsx:177-189` |
| Startup | Prewarms the palette with `getPalette({size:16})`, then `waitForThemeMode(1000) ?? "dark"`; SIGHUP destroys the renderer | `tui/src/app.tsx:214-225` |
| `TuiInput` | `{ url, args, config: TuiConfig.Resolved, onSnapshot?, directory?, fetch?, headers?, events?: EventSource, pluginHost, updater? }` | `tui/src/app.tsx:140-151` |
| Root view | `<box>` (full terminal size) containing a `Switch` on `route.data.type` (`home` → `<Home/>`, `session` → `<Session/>` keyed by sessionID), the plugin route render, and the slots `app_bottom` and `app`. `<StartupLoading>` shows unless `OPENCODE_FAST_BOOT` is set | `tui/src/app.tsx:1039-1085` |

**Provider tree (outer → inner)**, `tui/src/app.tsx:228-326`: `ExitProvider` → `EpilogueProvider` → `ErrorBoundary(ErrorComponent)` → `TuiPathsProvider{cwd,home,state,worktree=data+"/worktree"}` → `TuiTerminalEnvironmentProvider{platform, multiplexer: TMUX→tmux | STY→screen, displayServer: WAYLAND_DISPLAY→wayland | DISPLAY→x11}` → `TuiStartupProvider{initialRoute=JSON(OPENCODE_ROUTE), skipInitialLoading=OPENCODE_FAST_BOOT}` → `ClipboardProvider` → `OpencodeKeymapProvider` → `ArgsProvider` → `KVProvider` → `ToastProvider` → `RouteProvider` (with `--continue` the initial route is `{type:"session",sessionID:"dummy"}`) → `TuiConfigProvider` → `PluginRuntimeProvider` → `SDKProvider` → `PermissionProvider` → `ProjectProvider` → `SyncProvider` → `DataProvider` → `ThemeProvider` → `LocalProvider` → `PromptStashProvider` → `DialogProvider` → `FrecencyProvider` → `PromptHistoryProvider` → `PromptRefProvider` → `EditorContextProvider` → `LocationProvider` → `UpdateNotificationProvider` → `App`.

`createSimpleContext` (`tui/src/context/helper.tsx:3-26`) holds back rendering of its children until `init.ready` is `true`. KV, Sync, Theme and others gate startup this way.

**Contexts (`tui/src/context/*`)**

| File | Purpose |
|---|---|
| `args.tsx:3-16` | CLI args `{model, agent, prompt, continue, sessionID, fork, auto}` |
| `clipboard.tsx` | DI wrapper around `tui/src/clipboard.ts` |
| `data.tsx:58` | "Data" store for the v2 API: session info, messages, permission and question (`sdk.client.v2.session.get/messages`, `:421-429`) |
| `directory.ts` | `useDirectory()`: home-abbreviated directory plus `:branch` from `sync.data.vcs` |
| `editor.ts:112` | "EditorContext": IDE bridge over WebSocket JSON-RPC (see 2.8) |
| `epilogue.tsx` / `exit.tsx` | Text printed after exit / exit callback that destroys the renderer |
| `event.ts:9-36` | `useEvent().on(type, handler(evt,{directory,workspace}))`; skips `payload.type==="sync"` |
| `helper.tsx` | `createSimpleContext` |
| `kv.tsx:10` | Persistent KV store (2.7) |
| `local.tsx:51` | "Local": current agent/model/variant; `model.json` and `session.json` (pins) persistence |
| `location.tsx` | Optional `LocationRef` (directory/workspace) override |
| `path-format.tsx` | Path display relative to the location or cwd |
| `permission.tsx:5-26` | Permission mode `"auto"|"normal"`; initialised from `args.auto` |
| `project.tsx:9` | Project/instance/workspace (`path.get`, `project.current`, `project.directories`, `experimental.workspace.list/status`, `:41-58`) |
| `prompt.tsx` | `PromptRef` holder |
| `route.tsx` | Router (2.3) |
| `runtime.tsx:21-23` | `TuiPaths`, `TuiTerminalEnvironment`, `TuiStartup` contexts |
| `sdk.tsx:11` | SDK client and event stream (2.2) |
| `sync.tsx:64` | "Sync": bootstrap fetches plus an event-driven store (2.2) |
| `theme.tsx:102` | Theme state (2.6) |
| `thinking.ts` | Thinking display mode `"show"|"hide"`; KV keys `thinking_mode` and `thinking_visibility` (`:34-35`) |
| `update-notification.tsx:43` | Update notice/apply state; `UpdateSource` is supplied by the CLI process |

**Routes** (`tui/src/context/route.tsx:6-23`): `HomeRoute{type:"home",prompt?}`, `SessionRoute{type:"session",sessionID,prompt?}`, `PluginRoute{type:"plugin",id,data?}`. `navigate()` replaces the store through `reconcile` (`:37-39`). There is no history stack; plugins that need to return pass a `returnRoute` param themselves (`tui/src/feature-plugins/system/diff-viewer.tsx:1065`). `OPENCODE_ROUTE` accepts the same three shapes (`route.tsx:44-53`).

**Plugin and slot system**
- Runtime: `createPluginRuntime()` returns `{Slot, routes, commands, status, update, clear, setupSlots}` (`tui/src/plugin/runtime.tsx:12-35`). The host interface is `TuiPluginHost{start({api,config,runtime,dispose}), dispose()}` (`:61-69`). `App` calls `pluginHost.start(...)` and sets `ready` once it settles (`tui/src/app.tsx:396-407`).
- Plugin routes: `createPluginRoutes()` keeps a stack per name, and the last registration wins (`tui/src/plugin/api.ts:11-38`). A missing route renders `PluginRouteMissing` (`tui/src/app.tsx:1030-1036`).
- Slots: `createSlots()` wraps OpenTUI `createSolidSlotRegistry` and `createSlot` (`tui/src/plugin/slots.tsx:25-64`). The host slot map is in `plugin/src/tui.ts:455-486`: `app`, `app_bottom`, `home_logo`, `home_prompt{ref}`, `home_prompt_right`, `session_prompt{session_id,visible,disabled,on_submit,ref}`, `session_prompt_right{session_id}`, `home_bottom`, `home_footer`, `sidebar_title{session_id,title,share_url}`, `sidebar_content{session_id}`, `sidebar_footer{session_id}`. Slots are rendered at `app.tsx:1076,1078`, `routes/home.tsx:85,92,93,96,101`, `routes/session/index.tsx:1249,1265` and `routes/session/sidebar.tsx:50,82,87`.
- Plugin API surface (`TuiPluginApi`, `plugin/src/tui.ts:582-625`): `app, attention, command?, keys, keymap, mode, route, ui, tuiConfig, kv, state, theme, client, event, renderer, slots, plugins, lifecycle`. Plugin signature: `TuiPlugin = (api, options, meta) => Promise<void>` (`plugin/src/tui.ts:628`). The legacy `api.command` shim is in `tui/src/plugin/command-shim.ts:1`.
- Builtins (`tui/src/feature-plugins/builtins.ts:21-35`), in order:

| id | File | Slot / order |
|---|---|---|
| `internal:home-footer` | `home/footer.tsx:8` | `home_footer`, order 100 |
| `internal:home-tips` | `home/tips.tsx:7` | `home_bottom`, order 100; KV `tips_hidden` |
| `internal:sidebar-context` | `sidebar/context.tsx:6` | `sidebar_content`, order 100 |
| `internal:sidebar-mcp` | `sidebar/mcp.tsx:5` | order 200 |
| `internal:sidebar-lsp` | `sidebar/lsp.tsx:5` | order 300 |
| `internal:sidebar-todo` | `sidebar/todo.tsx:6` | order 400 |
| `internal:sidebar-files` | `sidebar/files.tsx:6` | order 500 |
| `internal:sidebar-footer` | `sidebar/footer.tsx:7` | `sidebar_footer`, order 100; KV `dismissed_getting_started` |
| `internal:notifications` | `system/notifications.ts:5` | Attention on question, permission, done, subagent_done and error (`:38,52,77,85`) |
| `internal:plugin-manager` | `system/plugins.tsx:9` | Commands `plugins.list` and `plugins.install` (`:242,251`) |
| `which-key` | `system/which-key.tsx:603` | Keymap layer; KV `which_key_layout` (default `"dock"`) and `which_key_pending_preview` |
| `diff-viewer` | `system/diff-viewer.tsx:1075` | Plugin route `"diff"` (`:38`), command `diff.open` with slash `/diff` (`:1056-1058`) |

- External plugins: `opencode/src/plugin/tui/runtime.ts:1124` `createLegacyTuiPluginHost()` loads the `tui` target of each plugin module (`:679-685`). Enablement merges `tui.json` `plugin_enabled` with KV `plugin_enabled` (`:123,479-486`). `cli/src/tui.ts:14-17` passes a no-op host.

**Worker thread model** (`opencode` binary only): the main thread runs the TUI. `new Worker(target)` runs `opencode/src/cli/tui/worker.ts`, which hosts the full server (`Server.Default().app.fetch`) and forwards `GlobalBus` events (`opencode/src/cli/cmd/tui.ts:249-254`; `worker.ts:24-26`). The worker path is `OPENCODE_WORKER_PATH` (build define), else `./cli/tui/worker.js`, else `../tui/worker.ts` (`cmd/tui.ts:91-96`). The RPC is JSON over `postMessage` with message types `rpc.request`, `rpc.result` and `rpc.event` (`opencode/src/util/rpc.ts:14,27,60`). RPC methods: `fetch`, `snapshot`, `server`, `checkUpgrade`, `reload`, `shutdown` (`worker.ts:30-78`). SIGUSR2 → `reload` (config invalidate plus dispose instances) (`cmd/tui.ts:255-258`). Shutdown has a 5 s timeout and is followed by `worker.terminate()` (`:261-267`).

### 2.2 Launch paths and server connection

| Launcher | Transport | Config | Ref |
|---|---|---|---|
| `opencode [project]` (`TuiThreadCommand`, `$0 [project]`), default | In-process worker. `url="http://opencode.internal"`; `fetch` = RPC `fetch` that serialises the Request to `{url,method,headers,body}`; events = RPC `global.event` subscription. No HTTP socket | `TuiConfig.get()` (full tui.json merge) | `opencode/src/cli/cmd/tui.ts:277-288` |
| `opencode` with `--port`/`--hostname`/`--mdns` | Worker starts a real HTTP server (`rpc.server(network)`) and the TUI uses its URL with `ServerAuth.headers()`; events via SSE | same | `cmd/tui.ts:272-283`; `worker.ts:54-58` |
| `opencode attach <url>` | Remote HTTP and SSE. Basic auth from `--password/-p`, `--username/-u`, or `OPENCODE_SERVER_PASSWORD`/`OPENCODE_SERVER_USERNAME` (default user `opencode`). `--dir` is passed through even when the directory does not exist locally | same | `opencode/src/cli/cmd/attach.ts:7-148`; `opencode/src/server/auth.ts:36-48` |
| New `cli` package default command | `Daemon.transport()` returns `{url, headers}`. A daemon is spawned as `serve --register`; the registry is `<state>/server.json` and the password `<state>/password` (0600) | `TuiConfig.resolve({}, {terminalSuspend:false})`, so **no tui.json is loaded** | `cli/src/commands/handlers/default.ts:6-13`; `cli/src/tui.ts:7-19`; `cli/src/services/daemon.ts:39-41,122,137-139` |

- The `cli` path wraps fetch in `gracefulFetch`, which turns a 404 into legacy defaults for `/config/providers`, `/provider`, `/agent` and `/config` (`cli/src/tui.ts:21-37`).
- TUI CLI flags (`cmd/tui.ts:114-182`): `--model/-m`, `--continue/-c`, `--session/-s`, `--fork` (requires -c or -s), `--prompt` (stdin is prepended when piped, `:98-103`), `--agent`, `--auto` (hidden aliases `--yolo`, `--dangerously-skip-permissions`), `--mini` (→ `runMini`, incompatible with network flags), `--no-replay`, `--replay-limit`, `--demo` (hidden; all three need `--mini`). Network flags `--port` (default 0), `--hostname` (default `127.0.0.1`), `--mdns`, `--mdns-domain` (default `opencode.local`), `--cors` (`opencode/src/cli/network.ts:5-30`).
- Before launch, `validateSession` decodes the `SessionID` and calls `session.get` (`opencode/src/cli/tui/validate-session.ts:7-29`).
- SDK client: `createOpencodeClient({baseUrl:url, signal, directory, fetch, headers})` from `@opencode-ai/sdk/v2` (`tui/src/context/sdk.tsx:23-31`). The directory travels as header `x-opencode-directory`, which GET/HEAD requests rewrite into a `?directory=` query (`sdk/js/src/v2/client.ts:18-46,63-66`). Workspaces use `x-opencode-workspace`.
- Event stream: when `events` is provided (worker mode) it is subscribed to directly. Otherwise `startSSE()` loops on `sdk.global.event()`, which is `GET /global/event` (`sdk/js/src/v2/gen/sdk.gen.ts:1338`), with `sseMaxRetryAttempts:0` and exponential backoff from 1 s up to 30 s (`sdk.tsx:51-52,82-117`). Events are coalesced into a 16 ms batch for Solid `batch()` (`:68-80`). With `OPENCODE_EXPERIMENTAL_WORKSPACES`, `sdk.sync.start()` is called after subscribing (`:96-100,124-128`). Payload: `GlobalEvent{directory, workspace, payload}` (`context/event.ts:13-17`).
- Sync bootstrap calls (`tui/src/context/sync.tsx:435-605`): `config.providers`, `provider.list`, `experimental.capabilities`, `experimental.console`, `app.agents`, `config.get`, `command.list`, `mcp.status`, `experimental.resource`, `formatter.status`, `session.status`, `provider.auth`, `vcs.get`, `lsp.status`, then per session `session.get/messages/todo/diff`. Handled events (`sync.tsx:178-439`): `server.instance.disposed`, `permission.replied/asked`, `question.replied/rejected/asked`, `todo.updated`, `session.diff/deleted/updated/status`, `session.next.moved`, `message.updated/removed`, `message.part.updated/delta/removed`, `lsp.updated`, `vcs.branch.updated`.
- Server → TUI control events: `tui.prompt.append`, `tui.command.execute`, `tui.toast.show`, `tui.session.select` (`schema/src/tui-event.ts:11,14,41,53`), handled in `tui/src/app.tsx:984-1005` and `component/prompt/index.tsx:237`. The `tui.command.execute` literal set is in `schema/src/tui-event.ts:16-33`; any string is accepted and passed to `keymap.dispatchCommand`. Server endpoints in the SDK: `/tui/control/next`, `/tui/control/response`, `/tui/append-prompt`, `/tui/open-help`, `/tui/open-sessions`, `/tui/open-themes`, `/tui/open-models`, `/tui/submit-prompt`, `/tui/clear-prompt`, `/tui/execute-command`, `/tui/show-toast`, `/tui/publish`, `/tui/select-session` (`sdk/js/src/v2/gen/sdk.gen.ts:4602-5007`).
- Updates: the CLI checks 1 s after start and then every 10 min, and installs only when the user acts in the `/update` dialog (`cmd/tui.ts:54-89`).

### 2.3 Screens, dialogs, components

| File | Purpose |
|---|---|
| `routes/home.tsx:25` | Home: logo, prompt (max width `prompt.max_width`, default 75 or `max(75, 70% width)` when `"auto"`, `:37-41`), tips, footer slots |
| `routes/home/session-destination.tsx` | Context for the session a home prompt submits into |
| `routes/session/index.tsx:175` | Session view: message list, tool rows (`InlineToolRow :1849`), sidebar (wide when width > 120, `:268`), prompt, session commands |
| `routes/session/sidebar.tsx:12` | Sidebar (`sidebar_title/content/footer` slots) |
| `routes/session/footer.tsx`, `subagent-footer.tsx` | Session and subagent footers |
| `routes/session/permission.tsx:111` | Permission request prompt (fullscreen toggle `permission.prompt.fullscreen`) |
| `routes/session/question.tsx:14` | Question tool prompt |
| `routes/session/dialog-timeline.tsx`, `dialog-fork-from-timeline.tsx`, `dialog-message.tsx`, `dialog-subagent.tsx` | Jump to message, fork from message, message actions, subagent picker |
| `component/command-palette.tsx:26` | Command palette dialog (`command.palette.show`) |
| `component/prompt/index.tsx:143` | Prompt textarea. Modes `normal|shell`; `!` at an empty prompt enters shell mode (`:286,830-837`) |
| `component/prompt/autocomplete.tsx:75` | Autocomplete. `@` lists files, agents, references and MCP resources; `/` lists commands (`:61,490-492`). Uses fuzzysort |
| `component/prompt/{history,frecency,stash,move,workspace,cwd,local-attachment}` | Prompt history, file frecency, stash, move-session, workspace selection, cwd, local attachments |
| `component/dialog-agent/-model/-provider/-variant/-mcp/-skill/-tag/-stash/-status/-debug/-theme-list/-update/-console-org.tsx` | Pickers for agent, model, provider, variant, MCP toggle, skill, tag, stash, plus status, debug, theme list, update and console org |
| `component/dialog-session-list/-rename/-delete-failed/-move-session.tsx` | Session list (pin `ctrl+f`, delete `ctrl+d`), rename, delete error, move to a project copy |
| `component/dialog-workspace-create/-list/-file-changes/-unavailable.tsx` | Workspace management (experimental) |
| `component/dialog-retry-action.tsx` | Retry/upsell dialog; KV `go_upsell_*` (`routes/session/index.tsx:87-91`) |
| `component/{logo,spinner,bg-pulse,startup-loading,error-component,plugin-route-missing,todo-item,workspace-label}.tsx` | Visual primitives. Spinner frames are in `spinner.tsx:10` |
| `ui/dialog.tsx:11,182` | Dialog stack provider. Sizes `medium`=60, `large`=88, `xlarge`=116 cols (`:21-25`) |
| `ui/dialog-select.tsx:80` | Generic fuzzy select list (`dialog.select.*` keys) |
| `ui/dialog-alert/-confirm/-prompt/-help/-export-options.tsx` | Alert, confirm, text prompt, help, export options |
| `ui/toast.tsx:15,91` | Toasts |
| `ui/link.tsx`, `ui/border.ts`, `ui/spinner.ts` | OSC 8 link (assumption, not verified), border chars, spinner |

### 2.4 Commands and slash commands (palette namespace `"palette"`)

Commands are registered with `useBindings({commands})`. Each one has `name, title, category, slashName?, slashAliases?, hidden?, suggested?`. Palette and slash entries come from `keymap.tsx:266-290`.

| Command | Title | Slash (aliases) | Ref |
|---|---|---|---|
| `command.palette.show` | Show command palette | none | `app.tsx:550` |
| `session.list` | Switch session | `/sessions` (`resume`, `continue`) | `app.tsx:559-564` |
| `session.new` | New session | `/new` (`clear`) | `app.tsx:570-575` |
| `workspace.copy_path` / `workspace.list` | Copy worktree path / Manage workspaces | `/workspaces` (hidden unless `OPENCODE_EXPERIMENTAL_WORKSPACES`) | `app.tsx:584,599-603` |
| `model.list` | Switch model | `/models` (`mo`) | `app.tsx:618-624` |
| `agent.list` / `mcp.list` | Switch agent / Toggle MCPs | `/agents` / `/mcps` | `app.tsx:666-678` |
| `variant.list` | Switch model variant | `/variants` | `app.tsx:701-705` |
| `provider.connect` | Connect provider | `/connect` | `app.tsx:727-730` |
| `console.org.switch` | Switch org | `/org` (`orgs`, `switch-org`) | `app.tsx:739-743` |
| `opencode.status` / `opencode.update` / `opencode.debug` | View status / Update OpenCode / View debug info | `/status` / `/update` / `/debug` | `app.tsx:752-774` |
| `theme.switch` | Switch theme | `/themes` | `app.tsx:781-783` |
| `help.show` | Help | `/help` | `app.tsx:809-811` |
| `app.exit` | Exit the app | `/exit` (`quit`, `q`) | `app.tsx:827-830` |
| `permission.mode` | Enable/Disable auto-approve permissions | none | `app.tsx:946-950` |
| `session.rename/timeline/fork/compact/undo/redo` | Rename session / Jump to message / Fork session / Compact session / Undo previous message / Redo | `/rename` `/timeline` `/fork` `/compact` `/undo` `/redo` | `routes/session/index.tsx:465-587` |
| `session.toggle.timestamps` / `session.toggle.thinking` | Show/Hide timestamps / thinking | `/timestamps` / `/thinking` | `routes/session/index.tsx:631-651` |
| `session.copy` / `session.export` | Copy session transcript / Export session transcript | `/copy` / `/export` | `routes/session/index.tsx:852-886` |
| `prompt.editor` / `prompt.skills` | External editor / skills | `/editor` / `/skills` | `component/prompt/index.tsx:426-427,517-519` |
| `workspace.set` / `session.move` | Set workspace / Move session | `/warp` / `/move` | `component/prompt/index.tsx:538-551` |
| `diff.open` | Diff viewer | `/diff` | `feature-plugins/system/diff-viewer.tsx:1056-1058` |

Other non-slash commands appear in the `CommandMap` (2.5) and in `app.tsx:630-934` (toggles, cycles, terminal). Server-defined commands from `command.list` are listed in `/` autocomplete (`autocomplete.tsx:454-457`).

### 2.5 Keybinds

**Syntax and semantics**
- Value schema `BindingValueSchema`: `false | "none" | string | KeyStroke | BindingObject | Array<...>` (`tui/src/config/keybind.ts:28-33`). `KeyStroke{name, ctrl?, shift?, meta?, super?, hyper?}` (`:8-15`). `BindingObject{key: string|KeyStroke, event?: "press"|"release", preventDefault?, fallthrough?, ...rest}` (`:17-25`).
- String form: modifiers joined with `+` (`ctrl+alt+shift+k`). A comma separates alternative bindings (`registerCommaBindings`, `tui/src/keymap.tsx:216`). `<leader>` is a token for a timed prefix (`registerTimedLeader{trigger, name:"leader", timeoutMs: leader_timeout}`, `keymap.tsx:219-226`). Sequences are written by concatenation, e.g. `<leader>down`. Key aliases are rewritten before parsing: `enter→return`, `esc→escape`, `pgdown→pagedown`, `pgup→pageup` (`keymap.tsx:112-125`). Display aliases: `pageup→pgup`, `pagedown→pgdn`, `delete→del`, modifier `meta→alt` (`keymap.tsx:190-204`). Escape clears and backspace pops a pending sequence (`:227-228`).
- Leader: config key `leader`, default `ctrl+x` (`keybind.ts:41,46`). `leader_timeout` defaults to 2000 ms (`config/index.tsx:21`).
- Overrides: `tui.json` `keybinds.{key}`. Unknown keys throw in `parse()` (`keybind.ts:449-458`), but the loader drops them first (`opencode/src/config/tui.ts:71-81`). Resolution goes through `createBindingLookup(…, {commandMap: CommandMap})` (`config/index.tsx:123-126`).
- Platform adjustment: when `terminalSuspend` is false (always on win32, `opencode/src/config/tui.ts:217`), `terminal_suspend="none"` and `ctrl+z` is prepended to `input_undo` (`config/index.tsx:103-110`).
- Scoping: bindings are gathered per layer. `app` (mode `base`), `app.global`, `app_exit` (active only when the prompt is unfocused or empty) (`app.tsx:965-982`), `input` (managed textarea layer, `keymap.tsx:229-232`), `session.global`, and so on. A mode stack is stored in keymap data `opencode.mode`, base `"base"` (`keymap.tsx:21-24,53-100`).

**Full keybind table.** `L` is the line in `tui/src/config/keybind.ts`; the command id comes from `CommandMap` (`:256-420`). `=` means the key is itself the command id (no CommandMap entry). `none` means disabled by default.

| L | Key | Default | Command | Description (verbatim, abbreviated) |
|---|---|---|---|---|
| 46 | leader | `ctrl+x` | (leader token) | Leader key for keybind combinations |
| 48 | app_exit | `ctrl+c,ctrl+d,<leader>q` | app.exit | Exit the application |
| 49 | app_debug | none | app.debug | Toggle debug panel |
| 50 | app_console | none | app.console | Toggle console |
| 51 | app_heap_snapshot | none | app.heap_snapshot | Write heap snapshot |
| 52 | app_toggle_animations | none | app.toggle.animations | Toggle animations |
| 53 | app_toggle_file_context | none | app.toggle.file_context | Toggle file context |
| 54 | app_toggle_diffwrap | none | app.toggle.diffwrap | Toggle diff wrapping |
| 55 | app_toggle_paste_summary | none | app.toggle.paste_summary | Toggle paste summary |
| 56 | app_toggle_session_directory_filter | none | app.toggle.session_directory_filter | Toggle session directory filtering |
| 57 | command_list | `ctrl+p` | command.palette.show | List available commands |
| 58 | help_show | none | help.show | Open help dialog |
| 59 | docs_open | none | docs.open | Open documentation |
| 60 | diff_open | none | diff.open | Open diff viewer |
| 61 | diff_close | `escape,q` | diff.close | Close diff viewer |
| 62 | diff_toggle | `enter,space` | diff.toggle | Toggle diff viewer item |
| 63 | diff_expand | `right` | diff.expand | Expand item |
| 64 | diff_expand_all | `E` | diff.expand_all | Expand all folders |
| 65 | diff_collapse | `left` | diff.collapse | Collapse item |
| 66 | diff_switch_focus | `tab` | diff.switch_focus | Switch focus |
| 67 | diff_next_hunk | `]` | diff.next_hunk | Next hunk |
| 68 | diff_previous_hunk | `[` | diff.previous_hunk | Previous hunk |
| 69 | diff_next_file | `n` | diff.next_file | Next file |
| 70 | diff_previous_file | `p` | diff.previous_file | Previous file |
| 71 | diff_toggle_file_tree | `b` | diff.toggle_file_tree | Toggle file tree |
| 72 | diff_single_patch | `s` | diff.single_patch | Toggle single patch view |
| 73 | diff_switch_source | `d` | diff.switch_source | Switch diff source |
| 74 | diff_toggle_view | `v` | diff.toggle_view | Split or unified |
| 75 | diff_help | `?` | diff.help | More diff shortcuts |
| 77 | editor_open | `<leader>e` | prompt.editor | Open external editor |
| 78 | theme_list | `<leader>t` | theme.switch | List available themes |
| 79 | theme_switch_mode | none | theme.switch_mode | Switch light/dark mode |
| 80 | theme_mode_lock | none | theme.mode.lock | Lock or unlock theme mode |
| 81 | sidebar_toggle | `<leader>b` | session.sidebar.toggle | Toggle sidebar |
| 82 | scrollbar_toggle | none | session.toggle.scrollbar | Toggle session scrollbar |
| 83 | status_view | `<leader>s` | opencode.status | View status |
| 84 | debug_view | none | opencode.debug | View debug info |
| 86 | session_export | `<leader>x` | session.export | Export session to editor |
| 87 | session_copy | none | session.copy | Copy session transcript |
| 88 | session_move | none | session.move | Move session |
| 89 | session_new | `<leader>n` | session.new | Create a new session |
| 90 | session_list | `<leader>l` | session.list | List all sessions |
| 91 | session_timeline | `<leader>g` | session.timeline | Show session timeline |
| 92 | session_fork | none | session.fork | Fork session from message |
| 93 | session_rename | `ctrl+r` | session.rename | Rename session |
| 94 | session_delete | `ctrl+d` | session.delete | Delete session |
| 95 | session_share | none | session.share | Share current session |
| 96 | session_unshare | none | session.unshare | Unshare current session |
| 97 | session_interrupt | `escape` | session.interrupt | Interrupt current session |
| 98 | session_background | `ctrl+b` | session.background | Background synchronous subagents |
| 99 | session_compact | `<leader>c` | session.compact | Compact the session |
| 100 | session_toggle_timestamps | none | session.toggle.timestamps | Toggle message timestamps |
| 101 | session_toggle_generic_tool_output | none | session.toggle.generic_tool_output | Toggle generic tool output |
| 102 | session_queued_prompts | `<leader>q` | session.queued_prompts | Manage queued prompts |
| 103 | session_child_first | `<leader>down` | session.child.first | First child session |
| 104 | session_child_cycle | `right` | session.child.next | Next child session |
| 105 | session_child_cycle_reverse | `left` | session.child.previous | Previous child session |
| 106 | session_parent | `up` | session.parent | Parent session |
| 107 | session_pin_toggle | `ctrl+f` | session.pin.toggle | Pin/unpin in session list |
| 108-116 | session_quick_switch_1 … _9 | `<leader>1` … `<leader>9` | session.quick_switch.1 … .9 | Switch to session in quick slot N |
| 118 | stash_delete | `ctrl+d` | stash.delete | Delete stash entry |
| 119 | model_provider_list | `ctrl+a` | model.dialog.provider | Provider list from model dialog |
| 120 | model_favorite_toggle | `ctrl+f` | model.dialog.favorite | Toggle model favorite |
| 121 | model_list | `<leader>m` | model.list | List available models |
| 122 | model_cycle_recent | `f2` | model.cycle_recent | Next recently used model |
| 123 | model_cycle_recent_reverse | `shift+f2` | model.cycle_recent_reverse | Previous recent model |
| 124 | model_cycle_favorite | none | model.cycle_favorite | Next favorite model |
| 125 | model_cycle_favorite_reverse | none | model.cycle_favorite_reverse | Previous favorite model |
| 126 | mcp_list | none | mcp.list | List MCP servers |
| 127 | provider_connect | none | provider.connect | Connect provider |
| 128 | console_org_switch | none | console.org.switch | Switch console organization |
| 129 | agent_list | `<leader>a` | agent.list | List agents |
| 130 | agent_cycle | `tab` | agent.cycle | Next agent |
| 131 | agent_cycle_reverse | `shift+tab` | agent.cycle.reverse | Previous agent |
| 132 | variant_cycle | `ctrl+t` | variant.cycle | Cycle model variants |
| 133 | variant_list | none | variant.list | List model variants |
| 135 | messages_page_up | `pageup,ctrl+alt+b` | session.page.up | Scroll up one page |
| 136 | messages_page_down | `pagedown,ctrl+alt+f` | session.page.down | Scroll down one page |
| 137 | messages_line_up | `ctrl+alt+y` | session.line.up | Scroll up one line |
| 138 | messages_line_down | `ctrl+alt+e` | session.line.down | Scroll down one line |
| 139 | messages_half_page_up | `ctrl+alt+u` | session.half.page.up | Scroll up half page |
| 140 | messages_half_page_down | `ctrl+alt+d` | session.half.page.down | Scroll down half page |
| 141 | messages_first | `ctrl+g,home` | session.first | First message |
| 142 | messages_last | `ctrl+alt+g,end` | session.last | Last message |
| 143 | messages_next | none | session.message.next | Next message |
| 144 | messages_previous | none | session.message.previous | Previous message |
| 145 | messages_last_user | none | session.messages_last_user | Last user message |
| 146 | messages_copy | `<leader>y` | messages.copy | Copy message |
| 147 | messages_undo | `<leader>u` | session.undo | Undo message |
| 148 | messages_redo | `<leader>r` | session.redo | Redo message |
| 149 | messages_toggle_conceal | `<leader>h` | session.toggle.conceal | Toggle code block concealment |
| 150 | tool_details | none | session.toggle.actions | Toggle tool details |
| 151 | display_thinking | none | session.toggle.thinking | Toggle thinking blocks |
| 153 | prompt_submit | none | prompt.submit | Submit prompt |
| 154 | prompt_editor_context_clear | none | prompt.editor_context.clear | Clear editor context |
| 155 | prompt_skills | none | prompt.skills | Open skill selector |
| 156 | prompt_stash | none | prompt.stash | Stash prompt |
| 157 | prompt_stash_pop | none | prompt.stash.pop | Pop stashed prompt |
| 158 | prompt_stash_list | none | prompt.stash.list | List stashed prompts |
| 159 | workspace_set | none | workspace.set | Set workspace |
| 161 | input_clear | `ctrl+c` | prompt.clear | Clear input field |
| 162 | input_paste | `{key:"ctrl+v",preventDefault:false}` | prompt.paste | Paste from clipboard |
| 163 | input_submit | `return` | input.submit | Submit input |
| 164 | input_newline | `shift+return,ctrl+return,alt+return,ctrl+j` | input.newline | Insert newline |
| 165 | input_move_left | `left,ctrl+b` | input.move.left | Cursor left |
| 166 | input_move_right | `right,ctrl+f` | input.move.right | Cursor right |
| 167 | input_move_up | `up` | input.move.up | Cursor up |
| 168 | input_move_down | `down` | input.move.down | Cursor down |
| 169 | input_select_left | `shift+left` | input.select.left | Select left |
| 170 | input_select_right | `shift+right` | input.select.right | Select right |
| 171 | input_select_up | `shift+up` | input.select.up | Select up |
| 172 | input_select_down | `shift+down` | input.select.down | Select down |
| 173 | input_line_home | `ctrl+a` | input.line.home | Start of line |
| 174 | input_line_end | `ctrl+e` | input.line.end | End of line |
| 175 | input_select_line_home | `ctrl+shift+a` | input.select.line.home | Select to line start |
| 176 | input_select_line_end | `ctrl+shift+e` | input.select.line.end | Select to line end |
| 177 | input_visual_line_home | `alt+a` | input.visual.line.home | Start of visual line |
| 178 | input_visual_line_end | `alt+e` | input.visual.line.end | End of visual line |
| 179 | input_select_visual_line_home | `alt+shift+a` | input.select.visual.line.home | Select to visual line start |
| 180 | input_select_visual_line_end | `alt+shift+e` | input.select.visual.line.end | Select to visual line end |
| 181 | input_buffer_home | `home` | input.buffer.home | Start of buffer |
| 182 | input_buffer_end | `end` | input.buffer.end | End of buffer |
| 183 | input_select_buffer_home | `shift+home` | input.select.buffer.home | Select to buffer start |
| 184 | input_select_buffer_end | `shift+end` | input.select.buffer.end | Select to buffer end |
| 185 | input_delete_line | `ctrl+shift+d` | input.delete.line | Delete line |
| 186 | input_delete_to_line_end | `ctrl+k` | input.delete.to.line.end | Delete to end of line |
| 187 | input_delete_to_line_start | `ctrl+u` | input.delete.to.line.start | Delete to start of line |
| 188 | input_backspace | `backspace,shift+backspace` | input.backspace | Backspace |
| 189 | input_delete | `ctrl+d,delete,shift+delete` | input.delete | Delete character |
| 190 | input_undo | `ctrl+-,super+z` | input.undo | Undo (win32: `ctrl+z` prepended) |
| 191 | input_redo | `ctrl+.,super+shift+z` | input.redo | Redo |
| 192 | input_word_forward | `alt+f,alt+right,ctrl+right` | input.word.forward | Word forward |
| 193 | input_word_backward | `alt+b,alt+left,ctrl+left` | input.word.backward | Word backward |
| 194 | input_select_word_forward | `alt+shift+f,alt+shift+right` | input.select.word.forward | Select word forward |
| 195 | input_select_word_backward | `alt+shift+b,alt+shift+left` | input.select.word.backward | Select word backward |
| 196 | input_delete_word_forward | `alt+d,alt+delete,ctrl+delete` | input.delete.word.forward | Delete word forward |
| 197 | input_delete_word_backward | `ctrl+w,ctrl+backspace,alt+backspace` | input.delete.word.backward | Delete word backward |
| 198 | input_select_all | `super+a` | input.select.all | Select all |
| 199 | history_previous | `up` | prompt.history.previous | Previous history item |
| 200 | history_next | `down` | prompt.history.next | Next history item |
| 202 | dialog.select.prev | `up,ctrl+p` | = | Previous dialog item |
| 203 | dialog.select.next | `down,ctrl+n` | = | Next dialog item |
| 204 | dialog.select.page_up | `pageup` | = | Page up in dialog |
| 205 | dialog.select.page_down | `pagedown` | = | Page down in dialog |
| 206 | dialog.select.home | `home` | = | First dialog item |
| 207 | dialog.select.end | `end` | = | Last dialog item |
| 208 | dialog.select.submit | `return` | = | Submit selected item |
| 209 | dialog.prompt.submit | `return` | = | Submit dialog prompt |
| 210 | dialog.mcp.toggle | `space` | = | Toggle MCP in MCP dialog |
| 211 | dialog.move_session.new | `ctrl+m` | = | New project copy |
| 212 | dialog.move_session.delete | `ctrl+d` | = | Delete project copy |
| 213 | dialog.move_session.refresh | `ctrl+r` | = | Refresh project copies |
| 214 | prompt.autocomplete.prev | `up,ctrl+p` | = | Previous autocomplete item |
| 215 | prompt.autocomplete.next | `down,ctrl+n` | = | Next autocomplete item |
| 216 | prompt.autocomplete.hide | `escape` | = | Hide autocomplete |
| 217 | prompt.autocomplete.select | `return` | = | Select autocomplete item |
| 218 | prompt.autocomplete.complete | `tab` | = | Complete autocomplete item |
| 219 | permission.prompt.fullscreen | `ctrl+f` | = | Toggle permission prompt fullscreen |
| 220 | plugins.toggle | `space` | = | Toggle plugin |
| 221 | dialog.plugins.install | `shift+i` | = | Install plugin from dialog |
| 223 | terminal_suspend | `ctrl+z` | terminal.suspend | Suspend terminal (forced `none` on win32) |
| 224 | terminal_title_toggle | none | terminal.title.toggle | Toggle terminal title |
| 225 | tips_toggle | `<leader>h` | tips.toggle | Toggle tips on home screen |
| 226 | plugin_manager | none | plugins.list | Open plugin manager dialog |
| 227 | plugin_install | none | plugins.install | Install plugin |
| 229 | which_key_toggle | `ctrl+alt+k` | which-key.toggle | Toggle which-key panel |
| 230 | which_key_layout_toggle | `ctrl+alt+shift+k` | which-key.layout.toggle | Switch which-key layout |
| 231 | which_key_pending_toggle | `ctrl+alt+shift+p` | which-key.pending.toggle | Toggle pending preview |
| 232 | which_key_group_previous | `ctrl+alt+left,ctrl+alt+[` | which-key.group.previous | Previous group |
| 233 | which_key_group_next | `ctrl+alt+right,ctrl+alt+]` | which-key.group.next | Next group |
| 234 | which_key_scroll_up | `ctrl+alt+up,ctrl+alt+p` | which-key.scroll.up | Scroll up |
| 235 | which_key_scroll_down | `ctrl+alt+down,ctrl+alt+n` | which-key.scroll.down | Scroll down |
| 236 | which_key_page_up | `ctrl+alt+pageup` | which-key.page.up | Page up |
| 237 | which_key_page_down | `ctrl+alt+pagedown` | which-key.page.down | Page down |
| 238 | which_key_home | `ctrl+alt+home` | which-key.home | First binding |
| 239 | which_key_end | `ctrl+alt+end` | which-key.end | Last binding |

Non-configurable hardcoded keys: `!` enters shell mode (`component/prompt/index.tsx:831`). The renderer console uses `ctrl+y` for copy-selection (`app.tsx:187`).

### 2.6 Themes

- **JSON format** (`ThemeJson`, `tui/src/theme/index.ts:120-128`): `{ $schema?, defs?: Record<name, "#hex"|ref>, theme: Record<ThemeColor, ColorValue> & { selectedListItemText?, backgroundMenu?, thinkingOpacity?: number } }`. Bundled files set `"$schema": "https://opencode.ai/theme.json"` (`theme/assets/opencode.json:2`).
- **ColorValue resolution** (`resolveTheme`, `theme/index.ts:241-299`): `"transparent"`/`"none"` → RGBA(0,0,0,0); `"#hex"`; a reference name is looked up in `defs` first and then in `theme` keys, recursively, and throws on a cycle or a missing name; a number is treated as an ANSI 0-255 index (`ansiToRgba`, `:301`: 16 base, 6×6×6 cube, grayscale); `{dark, light}` picks by mode. Defaults: `selectedListItemText` → `background` (flag `_hasSelectedListItemText`), `backgroundMenu` → `backgroundElement`, `thinkingOpacity` → `0.6` (`:278-292`).
- **ThemeColor keys** (`Theme`, `theme/index.ts:36-90`): `primary secondary accent error warning success info text textMuted selectedListItemText background backgroundPanel backgroundElement backgroundMenu border borderActive borderSubtle diffAdded diffRemoved diffContext diffHunkHeader diffHighlightAdded diffHighlightRemoved diffAddedBg diffRemovedBg diffContextBg diffLineNumber diffAddedLineNumberBg diffRemovedLineNumberBg markdownText markdownHeading markdownLink markdownLinkText markdownCode markdownBlockQuote markdownEmph markdownStrong markdownHorizontalRule markdownListItem markdownListEnumeration markdownImage markdownImageText markdownCodeBlock syntaxComment syntaxKeyword syntaxFunction syntaxVariable syntaxString syntaxNumber syntaxType syntaxOperator syntaxPunctuation` plus `thinkingOpacity`. Tree-sitter syntax rules are generated from these keys (`generateSyntax`/`generateSubtleSyntax`/`getSyntaxRules`, `:556-600`).
- **Built-in themes** (33; `DEFAULT_THEMES` at `theme/index.ts:130-164`; files `tui/src/theme/assets/<file>.json`, imports `:2-34`): `aura`, `ayu`, `carbonfox`, `catppuccin`, `catppuccin-frappe`, `catppuccin-macchiato`, `cobalt2`, `cursor`, `dracula`, `everforest`, `flexoki`, `github`, `gruvbox`, `kanagawa`, `lucent-orng`, `material`, `matrix`, `mercury`, `monokai`, `nightowl`, `nord`, `one-dark`, `opencode` (default), `orng`, `osaka-jade`, `palenight`, `rosepine`, `solarized`, `synthwave84`, `tokyonight`, `vercel`, `vesper`, `zenburn`. Each theme name equals its asset basename.
- **Merge priority**: defaults < plugin themes (`addTheme`/`upsertTheme`) < custom files < generated `system` (`theme/index.ts:171-183,220-239`).
- **Custom discovery** (`tui/src/context/theme.tsx:37-61`): the directories are `Global.Path.config` (`$XDG_CONFIG_HOME/opencode`, `core/src/global.ts:13`) and then `<dir>/.opencode` for every ancestor from cwd up to the filesystem root. Glob `themes/*.json` (dot and symlink allowed); the theme name is the file basename. A later directory overwrites an earlier one, so an ancestor `.opencode` beats the nearer one (as coded; intent unverified). A file is accepted when `isTheme` holds, i.e. a `theme` object exists (`theme/index.ts:194-198`). Reload triggers: SIGUSR2 (`theme.tsx:46-48`) and delayed refreshes `[250,1000]` ms (`:82`).
- **Active theme**: `tui.json` `theme` ?? KV `theme` ?? `"opencode"` (`theme.tsx:121-122`). Selecting a theme writes KV `theme` (`theme.tsx:296`). An unknown theme falls back to `opencode`.
- **Mode**: KV `theme_mode_lock` ?? `renderer.themeMode` ?? the startup mode from `waitForThemeMode(1000)`, else `"dark"` (`theme.tsx:116-118`; `app.tsx:225`). OpenTUI `CliRenderEvents.THEME_MODE` events follow the terminal unless the mode is locked (`theme.tsx:223-226`). Lock and unlock write KV `theme_mode_lock` and `theme_mode`.
- **`system` theme**: `renderer.getPalette({size:16})` → `generateSystem(colors, mode)` (`theme/index.ts:360-470`). The background is transparent (`:416-417`), grays derive from the terminal bg/fg, and mode comes from bg luminance > 0.5 → light (`terminalMode`, `:353-358`). If no palette is returned, `system` is removed and the active theme falls back to `opencode` (`theme.tsx:158-177`).

### 2.7 TUI config, KV and state files

**Files and precedence** (`opencode/src/config/tui.ts:171-210`, `opencode/src/config/paths.ts:10-45`); each later source deep-merges over the earlier (`mergeDeep`):
1. Global: `$XDG_CONFIG_HOME/opencode/tui.json`, then `tui.jsonc` (`fileInDirectory`, `paths.ts:43-45`).
2. `OPENCODE_TUI_CONFIG` (explicit file).
3. Project: `tui.jsonc` / `tui.json` found walking up from cwd, applied root-first (`paths.ts:10-21`). Skipped when `OPENCODE_DISABLE_PROJECT_CONFIG` is set.
4. `.opencode/` directories (ancestors of cwd, and `~/.opencode`) plus `OPENCODE_CONFIG_DIR`: `tui.json`, then `tui.jsonc`.

- Parsing: JSONC with `{env:…}`/`{file:…}` substitution (`ConfigVariable.substitute`, `tui.ts:101-104`). A nested `{"tui":{…}}` is flattened (`:55-69`). An invalid file logs a warning and contributes `{}` (`:120-129`). `attention.sounds` paths resolve relative to the config file (`opencode/src/config/tui-host-attention.ts:6-21`).
- Migration: `theme`, `keybinds` and the legacy `tui` keys inside `opencode.json` are moved into a sibling `tui.json` when none exists, and the source is backed up and stripped (`opencode/src/config/tui-migrate.ts:25-67`).
- Plugins listed in a config `.opencode` directory trigger an `npm install @opencode-ai/plugin@<version>` into that directory (`tui.ts:234-250`).
- Schema URL `https://opencode.ai/tui.json` (`tui-migrate.ts:11`), generated by `opencode/script/schema.ts:76` from `TuiConfig.Info`.

**Schema `TuiConfig.Info`** (`tui/src/config/index.tsx:61-75`), with defaults from `resolve()` (`:101-136`):

| Key | Type | Default | Used at |
|---|---|---|---|
| `$schema` | string | – | – |
| `theme` | string | KV/`opencode` | `context/theme.tsx:121` |
| `keybinds` | `KeybindOverrides` (2.5) | per-key defaults | `config/keybind.ts:245-252` |
| `plugin` | `Array<string \| [string, Record<string,unknown>]>` | – | `config/index.tsx:18-19` |
| `plugin_enabled` | `Record<string, boolean>` | – | `opencode/src/plugin/tui/runtime.ts:479` |
| `leader_timeout` | int > 0 (ms) | `2000` | `config/index.tsx:21-24` |
| `attention.enabled` / `.notifications` / `.sound` | bool | `false` / `true` / `true` | `:115-118` |
| `attention.volume` | 0..1 | `0.4` | `:119` |
| `attention.sound_pack` | string | `"opencode.default"` | `:120` |
| `attention.sounds` | `{default,question,permission,error,done,subagent_done}` → path | `{}` | `:8-15,42` |
| `prompt.max_height` | int > 0 | `max(6, floor(height/3))` | `component/prompt/index.tsx:1345` |
| `prompt.max_width` | int > 0 \| `"auto"` | `75` | `routes/home.tsx:37-41` |
| `scroll_speed` | number ≥ 0.001 | `3` (`CustomSpeedScroll`) | `util/scroll.ts:18-27` |
| `scroll_acceleration.enabled` | bool | – (`MacOSScrollAccel` when true, which overrides `scroll_speed`) | `util/scroll.ts:19-21` |
| `diff_style` | `"auto"` \| `"stacked"` | auto | `routes/session/index.tsx:2331`; `diff-viewer.tsx:142` |
| `cursor.style` | `block\|underline\|line\|default` | `block` (only when `cursor` is set) | `config/index.tsx:33-40,129-134` |
| `cursor.blinking` | bool | `true` | same |
| `mouse` | bool | `true` | `app.tsx:185` |

**KV store** (`tui/src/context/kv.tsx:10-66`): file `<state>/kv.json`, where `<state>` is `$XDG_STATE_HOME/opencode` (`core/src/global.ts:14`; `paths.state` passed at `app.tsx:239-246`). Cross-process lock `tui-kv:<file>` via `Flock` and atomic writes (`writeJsonAtomic`). Writes are queued per process.

| KV key | Default | Ref |
|---|---|---|
| `theme`, `theme_mode`, `theme_mode_lock` | `opencode` / – / – | `context/theme.tsx:116-121` |
| `animations_enabled`, `file_context_enabled` | `true` | `app.tsx:894,903` |
| `diff_wrap_mode` | `"word"` (`word\|none`) | `app.tsx:912`; `routes/session/index.tsx:264` |
| `paste_summary_enabled` | `!config.experimental.disable_paste_summary` | `app.tsx:437` |
| `session_directory_filter_enabled` | `true` | `app.tsx:935` |
| `terminal_title_enabled` | `true` | `app.tsx:435,885` |
| `timestamps` | `"hide"` | `routes/session/index.tsx:260` |
| `tool_details_visibility` / `assistant_metadata_visibility` / `scrollbar_visible` / `generic_tool_output_visibility` | `true` / `true` / `false` / `false` | `routes/session/index.tsx:261-266` |
| `thinking_mode`, `thinking_visibility` | – | `context/thinking.ts:34-35` |
| `tips_hidden` | – | `feature-plugins/home/tips.tsx:18` |
| `dismissed_getting_started` | – | `feature-plugins/sidebar/footer.tsx:17` |
| `skipped_version`, `update_dismissed_until` | – | `context/update-notification.tsx:66` |
| `go_upsell_last_seen_at`, `go_upsell_dont_show`, `go_upsell_account_rate_limit_last_seen_at`, `go_upsell_account_rate_limit_dont_show` | – (window 24 h) | `routes/session/index.tsx:87-91` |
| `attention_sound_pack` | – | `attention.ts:43` |
| `diff_viewer_show_file_tree`, `diff_viewer_single_patch`, `diff_viewer_view` | – | `feature-plugins/system/diff-viewer.tsx:43-45` |
| `which_key_layout` (`"dock"`), `which_key_pending_preview` (`false`) | | `feature-plugins/system/which-key.tsx:25-26,534-535` |
| `plugin_enabled` | `{}` | `opencode/src/plugin/tui/runtime.ts:123` |

Other `<state>` files: `model.json` `{recent, favorite, variant}` (`context/local.tsx:164,176-178`), `session.json` `{pinned}` (`local.tsx:420-432`), `prompt-history.jsonl` (`prompt/history.tsx:53`), `frecency.jsonl` (`prompt/frecency.tsx:42`), `prompt-stash.jsonl` (`prompt/stash.tsx:36`).

**Locale util** (`tui/src/util/locale.ts`): `titlecase`, `time`, `datetime`, `todayTimeOrDateTime`, `number`, `duration`, `truncate`, `truncateLeft`, `truncateMiddle(max=35)`, `pluralize` (`:1-81`). Locale and Intl options were not inspected.

### 2.8 Clipboard, editor, attention, Windows, mouse

- **Clipboard write** (`tui/src/clipboard.ts:121-125`): the OSC 52 sequence `ESC]52;c;<b64>BEL` is always emitted, wrapped in a tmux DCS passthrough under `TMUX`/`STY` (`:23-28`). Native tools follow, in order: macOS `osascript`; Linux on Wayland `wl-copy`, then `xclip -selection clipboard`, then `xsel --clipboard --input`; Windows `powershell.exe … Set-Clipboard` with UTF-8 stdin; fallback `clipboardy` (`:77-119`).
- **Clipboard read** (`:30-75`): images first (macOS osascript `PNGf` → `$TMPDIR/opencode-clipboard.png`; Windows/WSL PowerShell `System.Windows.Forms.Clipboard.GetImage` → base64 PNG; Linux `wl-paste -t image/png`, then `xclip … -t image/png -o`), then text via clipboardy. Returns `{data, mime}`.
- **Copy-on-select**: on mouse-up the selection is copied, unless `OPENCODE_EXPERIMENTAL_DISABLE_COPY_ON_SELECT` is set. That flag defaults to true on win32 (`core/src/flag/flag.ts:8,43-44`). When it is set, a right-click copies instead and `Selection.handleSelectionKey` intercepts keys at priority 1 (`app.tsx:411-417,1047-1056`).
- **External editor** (`tui/src/editor.ts:26-54`): `$VISUAL || $EDITOR`; nothing happens when both are unset. Writes `$TMPDIR/<Date.now()>.md`, runs `renderer.suspend()`, spawns `editor.split(" ")` plus the file with inherited stdio (`shell:true` on win32), reads the file back and deletes it, then resumes and re-renders. `normalizePromptContent` strips a single trailing newline from single-line content (`:12-24`).
- **IDE bridge** (`tui/src/context/editor.ts:112-408`, `editor.ts:56-101`): port from `CLAUDE_CODE_SSE_PORT` or `OPENCODE_EDITOR_SSE_PORT`, else discovery of `~/.claude/ide/<port>.lock` (JSON `{transport:"ws", workspaceFolders, authToken}`, best workspace match). Connects `ws://127.0.0.1:<port>` with header `x-claude-code-ide-authorization`, uses JSON-RPC (`initialize` → `notifications/initialized`), and receives `selection_changed` and `at_mentioned`. Zed: `ZED_TERM=true` or `TERM_PROGRAM=zed`; selection is read from the Zed sqlite DB (`OPENCODE_ZED_DB` or `~/Library/Application Support/Zed/db/0-stable/db.sqlite` or `~/.local/share/zed/db/0-stable/db.sqlite`) (`editor-zed.ts:187-198`).
- **Attention** (`tui/src/attention.ts`): desktop notification via `renderer.triggerNotification(message, title)` (`:185`; the escape sequence lives inside OpenTUI, see 2.11). Gating uses renderer `focus`/`blur` with `when` = `blurred` for notifications and `always` for sounds (`:107-112,179,197`). Sounds are bundled mp3s from `@opencode-ai/ui/audio/*` (`:17-22`: bip-bop-01, bip-bop-03, staplebops-06, nope-03, yup-01) and played via OpenTUI `Audio` (`tui/src/audio.ts:7-53`). Triggers are in the notifications plugin (2.1).
- **Terminal title**: `OpenCode`, `OC | <session title truncated to 40>`, or `OC | <plugin id>`. Disabled by `OPENCODE_DISABLE_TERMINAL_TITLE` or KV `terminal_title_enabled` (`app.tsx:440-464`).
- **Suspend**: `renderer.suspend()`, then `process.kill(0,"SIGTSTP")`, with resume on SIGCONT (`app.tsx:867-876`). Disabled on win32.
- **Windows** (`tui/src/terminal-win32.ts`): Bun FFI into `kernel32.dll` (`GetStdHandle`, `GetConsoleMode`, `SetConsoleMode`, `FlushConsoleInputBuffer`, `:7-13`). `win32DisableProcessedInput` clears `ENABLE_PROCESSED_INPUT (0x0001)` so Ctrl+C arrives as a key (`:30-42`; called at `app.tsx:197`). `win32InstallCtrlCGuard` hooks `stdin.setRawMode` and polls every 100 ms to keep the flag cleared (`:69-130`; installed at `opencode/src/cli/cmd/tui.ts:228`). `win32FlushInputBuffer` drains queued mouse and key events on exit (`:47-54`; `app.tsx:343`).
- **Mouse**: enabled when `config.mouse` (default true) is set and `OPENCODE_DISABLE_MOUSE` is not (`app.tsx:185`). Wheel speed and acceleration per 2.7. Kitty keyboard protocol is requested (`useKittyKeyboard:{}`, `app.tsx:182`).

**Environment variables read by the TUI**: `OPENCODE_DISABLE_MOUSE`, `OPENCODE_DISABLE_TERMINAL_TITLE`, `OPENCODE_EXPERIMENTAL_DISABLE_COPY_ON_SELECT`, `OPENCODE_EXPERIMENTAL_WORKSPACES`, `OPENCODE_FAST_BOOT`, `OPENCODE_ROUTE`, `OPENCODE_SHOW_TTFD`, `OPENCODE_EDITOR_SSE_PORT`, `CLAUDE_CODE_SSE_PORT`, `OPENCODE_ZED_DB`, `ZED_TERM`, `TERM_PROGRAM`, `VISUAL`, `EDITOR`, `TMUX`, `STY`, `WAYLAND_DISPLAY`, `DISPLAY`. Launcher and config: `OPENCODE_TUI_CONFIG`, `OPENCODE_CONFIG_DIR`, `OPENCODE_DISABLE_PROJECT_CONFIG`, `OPENCODE_SERVER_PASSWORD`, `OPENCODE_SERVER_USERNAME` (`core/src/flag/flag.ts:26-64`).

### 2.9 Rust mapping (suggestions, not source facts)

- **Stack**: `ratatui` with `crossterm` (raw mode, `KeyboardEnhancementFlags` for the Kitty protocol, mouse capture, `SetTitle`, bracketed paste). Use `tokio` for the SSE and HTTP client (`reqwest` + `eventsource-stream`) and `tokio::sync::mpsc` for UI events, with a 16 ms coalescing tick that mirrors `sdk.tsx:68-80`.
- **Architecture**: an Elm-style `App { route: Route, dialogs: Vec<Box<dyn Dialog>>, toasts, stores }`. Model `Route` as an enum `{Home{prompt}, Session{id,prompt}, Plugin{id,data}}`. Replace the Solid contexts with one `AppState` plus sub-stores (sync, data, local, kv, theme).
- **Transport trait**: `trait Transport { async fn request(..); fn events(&self) -> impl Stream<GlobalEvent> }`. Implement it twice: an in-process channel (replacing the worker plus RPC fetch shim and the `http://opencode.internal` URL) and HTTP+SSE (`/global/event`, Basic auth, `x-opencode-directory`).
- **Keybind parser**: lexer over `,` (alternatives), then the sequence `<token>`/key, then `+`-joined modifiers (`ctrl|alt|meta→alt|shift|super|hyper`). Apply aliases (`enter, esc, pgup, pgdown`) before parsing. Represent bindings as `enum Binding { Disabled, Seq(Vec<KeyStroke>), Obj{…} }`. The leader is a timed pending state machine (`leader_timeout`, escape clears, backspace pops). Layers carry a priority and an `enabled` predicate; commands come from a `HashMap<&str, CommandId>` generated from the 2.5 table (codegen from `keybind.ts` is recommended).
- **Theme loader**: `serde_json::Value`, then a resolver with cycle detection (`Vec<String>` chain), ANSI-256 → RGB, `{dark,light}` variants, and the defaults 2.6 lists. Embed the 33 assets with `include_str!`. Detect the system theme with OSC 10/11/4 queries (palette plus default fg/bg) and a 1 s timeout. The `terminal-colorsaurus` crate is one option to evaluate.
- **Config**: `serde` with `#[serde(deny_unknown_fields)]` off for `keybinds` (drop unknown keys with a warning), JSONC via `jsonc-parser` or `json5`, `mergeDeep` semantics where objects merge and arrays replace (verify against remeda), and `{env:}`/`{file:}` substitution shared with the main config port.
- **KV and state**: `serde_json` with atomic rename (`tempfile::persist`) and an advisory lock (`fs4`/`fd-lock`) on `kv.json`. JSONL for history, frecency and stash.
- **Clipboard**: OSC 52 is always written; `arboard` (text and image) or the same native-tool subprocess chain.
- **Windows**: `windows-sys` `SetConsoleMode` to clear `ENABLE_PROCESSED_INPUT`. Crossterm raw mode already does this, so confirm before porting the 100 ms guard.
- **Text input**: the prompt is a multi-line editor with visual-line navigation, undo/redo, extmarks and paste summaries. Candidates are `tui-textarea` (forked) or a custom rope (`ropey`) widget.
- **Markdown and syntax**: OpenTUI uses tree-sitter (`tui/src/parsers-config.ts`, 386 lines). In Rust, use `tree-sitter` with `tree-sitter-highlight`, plus `pulldown-cmark` for markdown.
- **Audio** (optional): `rodio`. Notifications: OSC 9/777 or `notify-rust`.

### 2.10 Porting risks

1. **OpenTUI has no ratatui equivalent.** Flexbox layout (Yoga), a retained renderable tree, the `@opentui/keymap` engine (layers, modes, comma bindings, timed leader, binding expanders) and slot registries all have to be rebuilt by hand. The keymap semantics live in `@opentui/keymap@0.4.5`, outside this repo.
2. **Plugin and slot API.** TUI plugins are JS/Solid components (`TuiPluginApi`, 12 builtins). A Rust TUI cannot load them. Either port the builtins natively and drop third-party TUI plugins, or define a new extension protocol. This breaks compatibility with existing `tui.json` `plugin` entries.
3. **Keybind conflicts are resolved by scope, not uniqueness.** `<leader>q` is bound to both `app_exit` and `session_queued_prompts`, `<leader>h` to both `messages_toggle_conceal` and `tips_toggle`, and `ctrl+d` to `app_exit`, `session_delete`, `input_delete`, `stash_delete` and `dialog.move_session.delete`. Precedence comes from layer priority and focus and must be reproduced exactly.
4. **Two launch paths behave differently.** The `opencode` binary loads `tui.json` and uses the in-process worker. `packages/cli` ignores `tui.json` (`resolve({})`), uses a daemon, and remaps 404s to legacy defaults. The port has to choose one.
5. **Terminal capability detection.** Theme mode (`waitForThemeMode`), palette queries for `system`, Kitty keyboard, focus events and `triggerNotification` are all implemented inside OpenTUI. Their escape sequences and fallbacks are not visible in this repo.
6. **State compatibility.** `kv.json` (about 30 keys), `model.json`, `session.json` and the `*.jsonl` files under `$XDG_STATE_HOME/opencode` are shared with the running TS TUI through `Flock`. A Rust port that coexists with it needs the same lock name and file semantics.
7. **The prompt editor carries most of the complexity** (`component/prompt/index.tsx` is 1716 lines and `routes/session/index.tsx` is 2641): shell mode, autocomplete, stash, history, frecency, IDE mentions, image paste and extmarks.

### 2.11 UNKNOWN / to verify

| Item | How to determine |
|---|---|
| `session.share` / `session.unshare`: keybinds exist (`keybind.ts:95,96`), the id is in the `tui.command.execute` literal set (`schema/src/tui-event.ts:20`) and the server TUI handler map (`opencode/src/server/routes/instance/httpapi/handlers/tui.ts:13`), but no TUI command is registered (sharing discontinued, cf. `opencode/src/cli/cmd/run.ts:536-538`) | Treat as dead; keep the config keys accepted for compatibility. Resolved by grep (assembler). `session.queued_prompts` **is** used — by the `--mini` footer (`opencode/src/cli/cmd/run/footer.view.tsx:557-563`) |
| Exact OpenTUI keymap semantics: sequence string grammar, `fallthrough`, `event:"release"`, conflict priority | Read `node_modules/@opentui/keymap` (v0.4.5) source: `addons/opentui`, `extras` |
| Escape sequences behind `triggerNotification`, `getPalette`, `waitForThemeMode`, `setTerminalTitle` | Read `@opentui/core` renderer source |
| Theme JSON Schema (`https://opencode.ai/theme.json`) is not in the repo | Fetch the URL, or search `packages/web`/`console` build scripts |
| `remeda.mergeDeep` array semantics for config merge (replace or concat) | Read the remeda source or run a quick test |
| Defaults for KV `thinking_mode` / `thinking_visibility` | `sed -n 25,67p tui/src/context/thinking.ts` |

## 3. Desktop shell (Electron main/preload)

Scope: `desktop/src/{main,preload,renderer}`, `desktop/electron-builder.config.ts`, `desktop/electron.vite.config.ts`, `desktop/scripts/*`, `desktop/AGENTS.md`, `desktop/README.md`. All paths are relative to `packages/`; repo-root files are prefixed `../`. Versions: Electron `42.3.3` (`desktop/package.json:51`), electron-builder `26.15.2` (:52), electron-updater `6.8.9` (:32), electron-store `11.0.2` (:31), electron-window-state `^5.0.3` (:33), electron-context-menu `4.1.2` (:29), electron-log `^5` (:30). Desktop package version `1.19.5` (:4).

House rules in `desktop/AGENTS.md`: renderer may only call `window.api` from preload (:3); IPC handlers live in `src/main/ipc.ts` (:4); every user-visible native string goes through i18n `nativeT(...)` (:5, :8).

### 3.1 Main-process module inventory (`desktop/src/main/`)

| File | Role |
|---|---|
| `index.ts` | Entry point (Effect program `main`, :115). Sets app name/AppUserModelId/userData, the single-instance lock, deep-link events, proxy/CA setup, sidecar spawn (v1/v2), updater timer, and window restore. |
| `ipc.ts` | Registers every non-WSL `ipcMain` handler (`registerIpcHandlers`, :57). Also `sendMenuCommand` (:302) and `sendDeepLinks` (:306). |
| `windows.ts` | BrowserWindow factory (`createMainWindow`, :168), `oc://` protocol (:291), titlebar overlay, zoom, fullscreen events, navigation policy, crash/unresponsive recovery dialogs, and the window registry. |
| `window-registry.ts` | Persisted window-id list and last-focused tracking. Decides whether a close removes the id or keeps it (:35-45). |
| `window-state.ts` | `safeWindowURL`/`safeWebContentsURL` logging helpers. |
| `server.ts` | v1 sidecar launcher over `utilityProcess` (`spawnLocalServer`, :57). Also `checkHealth` (:186), `preferAppEnv` (:44), and the default-server-URL store accessors (:30-42). |
| `sidecar.ts` | Entry script of the utility process. Imports `virtual:opencode-server` and calls `Server.listen` (:57-65). |
| `background-cli.ts` | v2 sidecar: drives the bundled `opencode-cli` binary through `service status/start/get password` (:19-58). |
| `shell-env.ts` | Probes the login shell environment (`$SHELL -il -c "env -0"`, :37). |
| `store.ts` / `store-keys.ts` | electron-store factory (`getStore`, :15) and key constants. |
| `store-cleanup.ts` | Removes empty or stale `opencode.draft.*.dat` and `opencode.workspace.*.dat` files at startup (:17-62). |
| `draft-store.ts` | SQLite (`node:sqlite` + drizzle) store for prompt drafts and blobs (:16-82). |
| `migrate.ts` | One-time migration of Tauri `.dat` files into electron-store (:69-91). |
| `onboarding.ts` / `install-state.ts` | First-launch onboarding flags and "Default Project" creation. `hasExistingAppState` heuristic. |
| `updater.ts` / `updater-controller.ts` / `updater-subscriptions.ts` | electron-updater configuration, the updater state machine, and per-webContents subscriptions. |
| `menu.ts` / `desktop-menu-actions.ts` | macOS native menu built from the shared `DESKTOP_MENU`. Action dispatcher for menu actions. |
| `logging.ts` | electron-log setup, crashReporter, netLog, and the debug-log zip export. |
| `unresponsive.ts` | Samples JS call stacks while the renderer is hung (1 s interval for 15 s, :5-6). |
| `debug.ts` | `setForceFocus` drives CDP through `webContents.debugger` to force `:focus` styles. |
| `apps.ts` | `checkAppExists` (macOS `/Applications` lookup plus `which`) and `resolveAppPath` (Windows `where` plus `.cmd`/`.bat` unwrapping). |
| `attachment-picker.ts` | Token-scoped authorization for picked files. 20 MiB budget (`MAX_ATTACHMENT_BYTES`, :5). |
| `external-url.ts` | Allow-list for opening URLs: `http:`, `https:`, `mailto:` (:6). Local files must be `file:` URLs with no host (:13). |
| `native-translations.ts` | Holds the active `DesktopNativeBundle`. `nativeT` (:22). |
| `initialization.ts` | `forwardInitializationFailure` passes a sidecar startup failure to the `serverReady` Deferred. |
| `constants.ts` | `CHANNEL` comes from `import.meta.env.OPENCODE_CHANNEL` (:4-5). `UPDATER_ENABLED = app.isPackaged && CHANNEL !== "dev"` (:7). |
| `env.d.ts` | Declares `virtual:opencode-server` (Server, Config, bootstrap) from `opencode/dist/types/src/node` (:9-19). |
| `wsl/ipc.ts` | WSL IPC handlers. Windows only; other platforms get stubs that throw (:9-12, :70-104). |
| `wsl/servers.ts` | WSL server controller: state, jobs, persistence (`createWslServersController`, :62). |
| `wsl/runtime.ts` | `wsl.exe` and PowerShell wrappers, distro parsing, interactive install over node-pty. |
| `wsl/sidecar.ts` | Spawns `opencode serve` inside a distro (`spawnWslSidecar`, :17). |
| `wsl/startup.ts` / `wsl/policy.ts` | Version check, health polling, IPC argument validation, terminal arguments. |

### 3.2 App identity, windows, lifecycle

| Item | Value | Source |
|---|---|---|
| App name per channel | `OpenCode Dev` / `OpenCode Beta` / `OpenCode`. Unpackaged builds always use `OpenCode Dev`. | `desktop/src/main/index.ts:53-57,141` |
| AppUserModelId / app id | `ai.opencode.desktop.dev` / `.beta` / `ai.opencode.desktop` | `index.ts:58-62,125,142` |
| userData | `join(app.getPath("appData"), appId)`. With `OPENCODE_TEST_ONBOARDING=1`, a temp root is used instead, plus `OPENCODE_DB=:memory:` and `XDG_*` overrides. | `index.ts:126-147` |
| Working directory | `process.chdir(homedir())`, because ripgrep misbehaves when the app runs from `/` on macOS | `index.ts:118-121` |
| Single instance | `app.requestSingleInstanceLock()`; quit if not acquired | `index.ts:198-201` |
| `second-instance` | Filters argv for `opencode://`, calls `emitDeepLinks`, then shows and focuses the last-focused window | `index.ts:205-216` |
| Window size | `electron-window-state`, `defaultWidth: 1280`, `defaultHeight: 800`, file `window-state-<id>.json`. **No minWidth/minHeight is set.** | `windows.ts:169-173,281-283` |
| BrowserWindow options | `show:false`, `autoHideMenuBar:true`, `title:"OpenCode"`, icon `icon.ico` (win) or `icon.png`, `backgroundColor` from the renderer or the oc-2 theme `background-base` | `windows.ts:176-185,92-103` |
| macOS chrome | `titleBarStyle:"hidden"`, `trafficLightPosition:{x:14,y:14}`. No vibrancy. | `windows.ts:186-191` |
| Windows chrome | `frame:false`, `titleBarStyle:"hidden"`, `titleBarOverlay {color:"#00000000", symbolColor white/black, height: max(40, 40*zoom)}` | `windows.ts:192-198,105-112,64` |
| Linux chrome | Default frame, no extra options | `windows.ts:186-198` (no linux branch) |
| webPreferences | `contextIsolation:true`, `nodeIntegration:false`, `sandbox:true`, `preload: ../preload/index.js` | `windows.ts:199-204` |
| Show | Shown on `ready-to-show` | `windows.ts:229-231` |
| Multi-window | Yes. Each window has a UUID id; ids are persisted in `windowIds`; all are restored on launch (`restoreMainWindows`). A new window comes from the `window.new` action. | `windows.ts:157-160,270-279`, `desktop-menu-actions.ts:22-24` |
| Close semantics | A deliberate close of a non-last window removes its id and deletes `window-state-<id>.json` and `opencode.window.<id>.dat`. Closing the last window, or closing while quitting, keeps the id. | `window-registry.ts:35-45`, `windows.ts:56-63,287-289` |
| Quit | `window-all-closed` quits except on darwin. `activate` with no windows triggers `restoreMainWindows`. | `index.ts:411-418` |
| Quit hooks | `before-quit`/`will-quit` call `setAppQuitting()` and `stopSidecars()`. SIGINT/SIGTERM do the same, then `app.quit()`. | `index.ts:224-232,246-251` |
| Windows logoff | `session-end` marks quitting (`windows.ts:277`) and flushes drafts | `ipc.ts:63` |
| Relaunch | `setAppQuitting`, stop sidecars, `app.relaunch()`, `app.quit()` | `index.ts:171-177` |
| Tray | **None** (no `Tray` usage in `desktop/src`) | grep |
| Dock icon | macOS `dock.png` from the icons directory | `windows.ts:162-166` |
| Background color | Renderer sends `--background-base` to `set-background-color`, which is applied to all windows | `renderer/index.tsx:364-371`, `windows.ts:76-82` |
| Titlebar theme | `set-titlebar {mode, scheme}`. On darwin it sets `nativeTheme.themeSource`; on win32 it updates the overlay. | `windows.ts:114-129` |
| Renderer origin | Privileged scheme `oc://renderer/index.html` serving `out/renderer`, with path-traversal guard. Dev uses `ELECTRON_RENDERER_URL`. | `windows.ts:21-23,35-45,291-343` |
| CORS shim | `onBeforeSendHeaders` and `onHeadersReceived` force `Access-Control-Allow-Origin: *` and `Allow-Headers: *` | `windows.ts:211-221,502-506` |
| Permissions | Only `clipboard-sanitized-write` and `notifications`, from the trusted renderer URL | `windows.ts:24-26,481-496` |
| Navigation | `setWindowOpenHandler` and `will-navigate`: non-renderer URLs are denied and opened through `openExternalURL` | `windows.ts:256-268` |
| Recovery | On `did-fail-load`, `did-fail-provisional-load` (except code -3), `render-process-gone`, or `unresponsive`, a message box offers Relaunch / Export logs / Keep waiting or Quit | `windows.ts:345-472` |
| Chromium switches | `proxy-bypass-list=<-loopback>`; `enable-features=DocumentPolicyIncludeJSCallStacksInCrashReports`; `remote-debugging-port=9222` when unpackaged | `index.ts:193-196,65` |
| Network env | Adds `127.0.0.1,localhost,::1` to `NO_PROXY`/`no_proxy`; `http.setGlobalProxyFromEnv()`; system and default CA certificates merged | `index.ts:72-113,179-183` |

### 3.3 IPC contract (preload `contextBridge.exposeInMainWorld("api", api)` — `desktop/src/preload/index.ts:138`)

Types are in `desktop/src/preload/types.ts`: `ElectronAPI` (:45-116), `ServerReadyData {url, username|null, password|null}` (:19-23), `TitlebarTheme` (:33-36), `FatalRendererError` (:37-43), `LinuxDisplayBackend = "wayland"|"auto"` (:32).

Direction legend: **I** = renderer `invoke` / main `handle`; **S** = renderer `send` / main `on`; **E** = main `webContents.send` / renderer `on`.

| Channel | Dir | Args | Return | API method (preload line) | Handler |
|---|---|---|---|---|---|
| `kill-sidecar` | I | – | void | `killSidecar` (:14) | `main/ipc.ts:65` |
| `install-cli` | I | – | string | `installCli` (:15) | **No handler** (UNKNOWN / dead). Caller: `renderer/cli.ts:7` |
| `await-initialization` | I | – | `ServerReadyData` | `awaitInitialization` (:16) | `ipc.ts:66` → `index.ts:286-294` |
| `consume-initial-deep-links` | I | – | string[] | `consumeInitialDeepLinks` (:59) | `ipc.ts:67` (`pendingDeepLinks.splice(0)`, `index.ts:295`) |
| `get-default-server-url` / `set-default-server-url` | I | url\|null | string\|null / void | (:60/:61) | `ipc.ts:68/69` |
| `is-first-launch-onboarding-pending` | I | – | bool | (:62) | `ipc.ts:72` |
| `finish-first-launch-onboarding` | I | createDefaultProject: bool | string\|null | (:63) | `ipc.ts:73` |
| `is-old-layout-eligible` | I | – | bool | (:65) | `ipc.ts:76` |
| `get-display-backend` / `set-display-backend` | I | backend\|null | null / undefined (**stubbed**, `index.ts:301-302`) | (:66/:67) | `ipc.ts:77/78` |
| `check-app-exists` | I | appName | bool | (:68) | `ipc.ts:81` |
| `resolve-app-path` | I | appName | string\|null | (:69) | `ipc.ts:82` |
| `updater-subscribe` / `updater-unsubscribe` | I | – | void | `updater.subscribe` (:40-55) | `ipc.ts:83/94` |
| `updater-state` | E | `UpdaterState` | – | listener (:44) | `ipc.ts:89` |
| `updater-check` / `updater-install` | I | – | UpdaterState / void | `updater.check/install` (:56/:57) | `ipc.ts:95/96` |
| `set-background-color` | I | color | void | (:131) | `ipc.ts:97` |
| `export-debug-logs` | I | – | zip path | (:132) | `ipc.ts:98` |
| `set-force-focus` | I | enabled | void | (:133) | `ipc.ts:99` |
| `record-fatal-renderer-error` | I | `FatalRendererError` | void | (:134) | `ipc.ts:102` |
| `set-native-translations` | I | `DesktopNativeBundle` (sender and mainFrame validated) | void | (:135) | `ipc.ts:105-113` |
| `store-get` | I | name, key | string\|null (non-strings are JSON-stringified) | `storeGet` (:70) | `ipc.ts:114` |
| `store-set` / `store-delete` / `store-clear` | I | name, key[, value] | void | (:71-73) | `ipc.ts:124/127/131` |
| `store-keys` / `store-length` | I | name | string[] / number | (:74/:75) | `ipc.ts:135/139` |
| `draft-get` / `draft-set` / `draft-delete` | I | key[, value] | string\|null / void | (:76-78) | `ipc.ts:143-145` |
| `draft-blob-put` / `draft-blob-get` | I | ArrayBuffer / id | sha256 id / ArrayBuffer\|null | (:79/:80) | `ipc.ts:146/147` |
| `open-directory-picker` | I | {multiple?, title?, defaultPath?} | string\|string[]\|null | (:94) | `ipc.ts:152-163` |
| `open-file-picker` | I | {multiple?, title?, defaultPath?, extensions?} | {token, files[{path,name,size}]}\|null | (:95) | `ipc.ts:165-189` |
| `read-picked-file` / `release-picked-files` | I | token, path / token | ArrayBuffer / void | (:96/:97) | `ipc.ts:191/195` |
| `save-file-picker` | I | {title?, defaultPath?} | string\|null | (:99) | `ipc.ts:199-209` |
| `open-external` | S | url | – | (:100) | `ipc.ts:211` |
| `open-local-file` | S | file URL | – | (:101) | `ipc.ts:215` |
| `open-path` | I | path, app? (darwin: `open -a app path`; else `execFile(app,[path])`) | void | (:102) | `ipc.ts:219-226` |
| `reveal-path` | I | path | bool | (:103) | `ipc.ts:228` |
| `read-clipboard-image` | I | – | {buffer(PNG), width, height}\|null | (:104) | `ipc.ts:238` |
| `get-window-id` | I | – | string | (:82) | `ipc.ts:246` |
| `get-window-focused` / `get-window-fullscreen` | I | – | bool | (:105/:106) | `ipc.ts:254/259` |
| `window-fullscreen-changed` | E | bool | – | (:107) | `windows.ts:536` |
| `set-window-focus` / `show-window` | I | – | void | (:112/:113) | `ipc.ts:264/269` |
| `relaunch` | S | – | – | (:114) | `ipc.ts:274` |
| `get-zoom-factor` / `set-zoom-factor` | I | factor | number / void | (:115/:116) | `ipc.ts:278/279` |
| `zoom-factor-changed` | E | number | – | (:124) | `windows.ts:549` |
| `get-pinch-zoom-enabled` / `set-pinch-zoom-enabled` | I | enabled | bool / void | (:117/:118) | `ipc.ts:285/286` |
| `pinch-zoom-enabled-changed` | E | bool | – | (:119) | `windows.ts:135` |
| `set-titlebar` | I | `TitlebarTheme` | void | (:129) | `ipc.ts:289` |
| `run-desktop-menu-action` | I | `DesktopMenuAction` | void | (:130) | `ipc.ts:294` |
| `menu-command` | E | command id string | – | `onMenuCommand` (:83) | `ipc.ts:303` |
| `deep-link` | E | string[] | – | `onDeepLink` (:88) | `ipc.ts:307` |
| `wsl-servers-subscribe` / `-unsubscribe` | I | – | void | `wslServers.subscribe` (:19-27) | `wsl/ipc.ts:27/42` |
| `wsl-servers-event` | E | `{type:"state", state}` | – | (:21) | `wsl/ipc.ts:37` |
| `wsl-servers-get-state` | I | – | `WslServersState` | (:18) | `wsl/ipc.ts:43` |
| `wsl-servers-probe-runtime` / `-refresh-distros` / `-install-wsl` | I | – | void | (:28-30) | `wsl/ipc.ts:44-46` |
| `wsl-servers-install-distro` / `-install-opencode` / `-open-terminal` | I | distro name | void | (:31,:33,:34) | `wsl/ipc.ts:47,53,56` |
| `wsl-servers-probe-addable` | I | string[] | void | (:32) | `wsl/ipc.ts:50` |
| `wsl-servers-add` / `-remove` / `-start` | I | distro / id / id | config / void | (:35-37) | `wsl/ipc.ts:59,62,65` |

Not IPC: `getPathForFile` uses `webUtils.getPathForFile` (`preload/index.ts:98`).

**Platform abstraction** (`app/src/context/platform.tsx`). The type is `Platform = PlatformBase & ({platform:"web"} | {platform:"desktop", os?, openDirectoryPickerDialog})` (:126-134). Members and line numbers: `version` :33, `openExternal` :36, `openPath?` :39, `openLocalFile?` :42, `revealPath?` :45, `restart` :48, `notify` :51, `openAttachmentPickerDialog?` :54, `getPathForFile?` :60, `saveFilePickerDialog?` :63, `storage?` :66, `draftStore?` :69, `windowID?` :72, `updater?` (`UpdaterPlatform {state, check, install}`, `app/src/updater.ts:13-17`) :75, `fetch?` :78, `getDefaultServer?` :81, `setDefaultServer?` :84, `wslServers?` :87, `getDisplayBackend?` :90, `setDisplayBackend?` :93, `webviewZoom?` :96, `windowFullscreen?` :99, `getPinchZoomEnabled?` :102, `setPinchZoomEnabled?` :105, `runDesktopMenuAction?` :108, `checkAppExists?` :111, `readClipboardImage?` :114, `exportDebugLogs?` :117, `setForceFocus?` :120, `recordFatalRendererError?` :123. `UpdaterState` statuses are `disabled|idle|checking|downloading|ready|up-to-date|installing|error` (`app/src/updater.ts:3-11`).

### 3.4 Sidecar (local opencode server)

The mode is selected by `OPENCODE_SIDECAR_V2 === "1"`: v2 if set, v1 otherwise (`index.ts:64`).

**v1 (default): in-process Node bundle inside an Electron `utilityProcess`.**

| Aspect | Detail | Source |
|---|---|---|
| Binary/entry | `out/main/sidecar.js`, built from `src/main/sidecar.ts` (vite input `sidecar`). `utilityProcess.fork(sidecar, [], {cwd, env, serviceName:"opencode server", stdio:"pipe"})` | `server.ts:63-69,19`; `electron.vite.config.ts:41` |
| Server code | `virtual:opencode-server` resolves to `../opencode/dist/node/node.js`. `.wasm` files are copied into `out/main/chunks/`. | `electron.vite.config.ts:6,64-79`; `opencode/src/node.ts:1-4` |
| Start protocol | Parent sends `{type:"start", hostname, port, password, userDataPath}`. The child replies `{type:"ready"}` or `{type:"error", error:{message,stack}}`. Stop is `{type:"stop"}`, answered by `{type:"stopped"}`. | `server.ts:132-138,12-15`; `sidecar.ts:13-27,41-49` |
| Listen | `Server.listen({port, hostname, username:"opencode", password, cors:["oc://renderer"]})` | `sidecar.ts:59-65` |
| Host/port | `127.0.0.1`. Port comes from `OPENCODE_PORT`, otherwise an ephemeral port from `net.createServer().listen(0,"127.0.0.1")`. | `index.ts:350-374` |
| Auth | `password = randomUUID()`, username `opencode`. The child sets `OPENCODE_SERVER_USERNAME` and `OPENCODE_SERVER_PASSWORD`. | `index.ts:375,389`; `sidecar.ts:83-89` |
| Env | Parent env is copied with `DEBUG` removed and `LD_PRELOAD` removed on Linux. `preferAppEnv` merges the login-shell env (non-Windows) and sets `OPENCODE_EXPERIMENTAL_ICON_DISCOVERY=true`, `OPENCODE_EXPERIMENTAL_FILEWATCHER=true`, `OPENCODE_CLIENT=desktop`, and `XDG_STATE_HOME ??= userData`. Also `OPENCODE_DISABLE_EMBEDDED_WEB_UI=true`. | `server.ts:213-220,44-55`; `index.ts:123,203` |
| Shell env probe | `$SHELL` → `userInfo().shell` → `/bin/sh`. Runs `-il -c "env -0"` with a 5 s timeout, falling back to `-l`. Nushell is skipped. | `shell-env.ts:12-23,5,36-93` |
| Readiness | Start stall timeout 60 s (`server.ts:20,101-106`). Credentials are resolved to the renderer **before** the health check (`index.ts:387-391`). | |
| Health | Polls every 100 ms: `GET /api/health`, then `/global/health`, with `Authorization: Basic base64("opencode:"+password)` and a 3 s timeout per request. The outer timeout of 30 s only logs. | `server.ts:144-163,186-211`; `index.ts:397-404` |
| Logs | stdout goes to log scope `server`; stderr goes to `server` at warn; exit goes to `utility` | `index.ts:381-383` |
| Kill | `stop()` posts `stop`, then `child.kill()` after 6 s | `server.ts:21,165-180` |

**v2: external CLI daemon** (`background-cli.ts`).

| Aspect | Detail | Source |
|---|---|---|
| Binary | Packaged: `process.resourcesPath/opencode-cli[.exe]`. Dev: `resources/opencode-cli[.exe]`. | `:20-22,123-125` |
| Staging | When packaged, the binary is copied to `userData/cli/<version>/opencode-cli` (chmod 755 off Windows) | `:60-78` |
| Commands | `--version`; `service status` (output parsed as `running <url>` or a bare URL) across state-home candidates (`XDG_STATE_HOME`, the shell's `XDG_STATE_HOME`, `appData/ai.opencode.desktop{,.beta,.dev}`); `service start` prints the URL on stdout; `service get password` (redacted in logs). Username is `opencode`. | `:24-57,12,110-115` |
| Provenance | `scripts/utils.ts` downloads npm `@opencode-ai/cli-<os>-<arch>[-baseline]@0.0.0-next-16350` and copies `bin/opencode2[.exe]` to `resources/opencode-cli`. It is bundled only for channel `dev`. | `scripts/utils.ts:6,16-53,72-92`; `electron-builder.config.ts:60-68`; `scripts/prebuild.ts:11` |

**WSL sidecars (Windows only)** start after the local sidecar (`index.ts:342-344,393-395`).

| Aspect | Detail | Source |
|---|---|---|
| Runtime detection | `wsl --version`, `wsl --list --verbose` (parsed with regex, `*` marks the default), `wsl --list --online` | `wsl/runtime.ts:210-247,342-370` |
| Encoding | Auto-detects UTF-16LE (BOM or zero-byte heuristic) | `wsl/runtime.ts:193-200` |
| Timeouts | 20 s default, 15 min for installs | `wsl/runtime.ts:34-35` |
| Install WSL | PowerShell `Start-Process wsl.exe -Verb RunAs --install --no-distribution` | `wsl/runtime.ts:249-256` |
| Install distro | node-pty ConPTY: `System32\wsl.exe --install -d <name> --web-download --no-launch` | `wsl/runtime.ts:116-125,258-265` |
| Install opencode | `bash -lc "curl -fsSL https://opencode.ai/install \| bash -s -- --version <appVersion>"`. The installed version must equal the app version. | `wsl/runtime.ts:267-277`; `wsl/startup.ts:7-16` |
| Binary lookup | `$HOME/.opencode/bin/opencode` | `wsl/runtime.ts:310-320` |
| Spawn | `wsl -d <distro> -- bash -se`, script on stdin: strips `/mnt/*` from PATH, `WSLENV=`, `OPENCODE_EXPERIMENTAL_DISABLE_FILEWATCHER=true`, `OPENCODE_CLIENT=desktop`, `OPENCODE_SERVER_USERNAME/PASSWORD`, `XDG_STATE_HOME=$HOME/.local/state`, then `exec opencode --print-logs --log-level WARN\|INFO serve --hostname 0.0.0.0 --port <port>`. Port is allocated on Windows loopback. | `wsl/sidecar.ts:24-44,93-107` |
| Health | Same `checkHealth`, polled every 100 ms with a 30 s timeout. URL `http://127.0.0.1:<port>`. | `wsl/sidecar.ts:60-73`; `wsl/startup.ts:20-25` |
| Identity | Server id `wsl:<distro>` (`wsl/servers.ts:58-60`). Persisted servers auto-start on initialize (`:305-309`). | |
| Terminal | `cmd.exe /c start "" wsl -d <distro>` | `wsl/policy.ts:19-21`; `wsl/runtime.ts:327-340` |
| Path conversion | **No** Windows↔WSL path conversion (`wslpath`) in `desktop/src` | grep, UNKNOWN |

The renderer adds WSL servers whose `runtime.kind==="ready"` to the server list (`renderer/wsl/connections.ts:3-21`). If the default server is a `wsl:` key that is not ready, it falls back to `"sidecar"` (:23-28).

### 3.5 Persistent stores (electron-store, `userData`)

`getStore(name="opencode.settings")` builds `new Store({name, cwd: userData, fileExtension:"", accessPropertiesByDotNotation:false})` (`store.ts:15-24`). The renderer can open **any** store by name through `store-*` IPC; values are strings (`ipc.ts:114-142`).

| Store (file) | Key | Type / default | Source |
|---|---|---|---|
| `opencode.settings` | `defaultServerUrl` | string, absent by default | `store-keys.ts:2`; `server.ts:30-42` |
| 〃 | `firstLaunchOnboardingComplete` | bool, absent means pending | `store-keys.ts:3`; `onboarding.ts:27-44` |
| 〃 | `oldLayoutEligible` | bool, set once from `hasExistingAppState` | `store-keys.ts:4`; `onboarding.ts:12-25`; `install-state.ts:1-8` |
| 〃 | `wslServers` | `{servers: {id, distro}[]}`, default `[]` | `store-keys.ts:5`; `wsl/servers.ts:438-451` |
| 〃 | `pinchZoomEnabled` | bool, default false | `store-keys.ts:6`; `windows.ts:131-143` |
| 〃 | `windowIds` | string[] | `store-keys.ts:7`; `windows.ts:56-58` |
| 〃 | `tauriMigrated` | bool | `migrate.ts:9,70,80,90` |
| `opencode.updater` | `ready` | `{version}` | `updater.ts:11,28,49-57` |
| `default.dat` (legacy), `opencode.global.dat`, `opencode.window.<id>.dat`, `opencode.workspace.<head>.<sum>.dat`, `opencode.draft.<head>.<sum>.dat` | renderer-defined | JSON string values | `app/src/utils/persist.ts:27-29,352,358,363`; default name at `renderer/index.tsx:157` |

Other userData artifacts:
- `drafts.sqlite` (WAL), with tables `document(key,value)` and `blob(id,data)`. Writes are debounced 500 ms, and orphan blobs are garbage-collected on open (`ipc.ts:58`; `draft-store.ts:7-36,55-57`).
- `window-state-<id>.json`.
- `logs/<stamp>/`.
- `Crashpad/`.
- `cli/<ver>/`.

Cleanup rules (`store-cleanup.ts:4-6,42-52`):
- Empty `draft`/`workspace` stores (128 bytes or less) are deleted.
- Drafts older than 30 days are deleted.
- At most 100 drafts are kept.

Tauri migration reads `.dat` files from the old Tauri app directory (`migrate.ts:13-32`):
- `opencode.settings.dat` → `opencode.settings`.
- Other `*.dat` files keep their name (`migrate.ts:51`).

Renderer localStorage key: `opencode.desktop.window.<windowID>.last-active-url` (`renderer/index.tsx:85-87`).

### 3.6 Updater

| Item | Value | Source |
|---|---|---|
| Library | `electron-updater` `autoUpdater`, logger = electron-log | `updater.ts:2,10,15` |
| Runtime config | `channel="latest"`, `allowPrerelease=false`, `allowDowngrade=true`, `autoDownload=false`, `autoInstallOnAppQuit=false` | `updater.ts:16-20` |
| Enabled | `app.isPackaged && CHANNEL !== "dev"`; otherwise state is `disabled` | `constants.ts:7`; `updater-controller.ts:27` |
| Feed (prod) | `{provider:"github", owner:"anomalyco", repo:"opencode", channel:"latest"}` | `electron-builder.config.ts:152` |
| Feed (beta) | `{provider:"github", owner:"anomalyco", repo:"opencode-beta", channel:"latest"}`. Beta is a separate app id and repo, not an electron-updater channel. | `electron-builder.config.ts:141` |
| Schedule | `updater.start()` at boot, then `check()` every 10 min (timer unref'd) | `index.ts:315-318` |
| Flow | `check` moves `checking` → (none: `up-to-date`, clears `ready`) or `downloading` → `downloadUpdate()` → persist `{version}` → `ready`. `install` moves to `installing`, stops sidecars, calls `quitAndInstall()` (sets the quitting flag first), and returns to `ready` on error. | `updater-controller.ts:38-93`; `updater.ts:35-47` |
| UI | Renderer subscribes over `updater-state`. The menu "Check for Updates" opens native dialogs: error / up to date / ready → Restart or Later. | `updater.ts:63-94`; `menu.ts:44` |
| Feed artifacts | `latest.yml` (win x64+arm64 merged), `latest-mac.yml` (arm64+x64 merged), `latest-linux.yml`, `latest-linux-arm64.yml` | `scripts/finalize-latest-yml.ts:86,95,99,106` |
| Legacy feed | `latest.json` (Tauri updater format, signed with `bunx @tauri-apps/cli signer sign`) with keys such as `windows-x86_64-nsis` and alias `windows-x86_64` | `scripts/finalize-latest-json.ts:129,173,185,208` |
| Windows signature check | `verifyUpdateCodeSignature: false` | `electron-builder.config.ts:98` |

### 3.7 Deep links, protocols, file associations

- Scheme `opencode`. It is registered at runtime with `app.setAsDefaultProtocolClient("opencode")` (`index.ts:271`) and in the packaging config (`protocols {name:"OpenCode"|"OpenCode Beta", schemes:["opencode"]}`, `electron-builder.config.ts:88-91,140,151`). Dev and beta share the same scheme.
- Delivery paths:
  - macOS `open-url` (`index.ts:218-222`).
  - `second-instance` argv (`index.ts:205-216`).
  - Both feed `pendingDeepLinks`, which the renderer pulls with `consume-initial-deep-links` and then follows over `deep-link` events (`index.ts:70,81-86`; `renderer/index.tsx:80-83`).
  - Cold-start argv on Windows/Linux is **not** read (only `second-instance` argv is parsed). UNKNOWN whether this is intended.
- Inside the renderer, links become `window.__OPENCODE__.deepLinks` plus a `CustomEvent("opencode:deep-link")` (`renderer/index.tsx:66-78`). Two link types are handled:
  - `opencode://open-project?directory=…`
  - `opencode://new-session?directory=…&prompt=…`
  
  Parsing is in `app/src/pages/layout/deep-links.ts:13-31`.
- Internal privileged scheme: `oc://renderer` (`windows.ts:21-45`).
- File associations: **none** (no `fileAssociations` in `electron-builder.config.ts`).

### 3.8 Menus, context menu, notifications, PTY, watcher, dialogs, shell

**Native menu.** The menu is built from the shared `DESKTOP_MENU` (`app/src/desktop-menu.ts:74-298`):
- macOS: built natively (`menu.ts:21-35`, with `if (process.platform !== "darwin") return` at :22).
- Windows: drawn by the renderer (`app/src/components/windows-app-menu.tsx:83`).
- Linux: no app menu is set (UNKNOWN: whether Electron's default menu appears on Alt with `autoHideMenuBar`).

Menu items:
- `command` items send `menu-command`, which the renderer dispatches through `useCommand().trigger(id)` (`renderer/index.tsx:320-323,358-360`).
- `action` items go to `runDesktopMenuAction` (`desktop-menu-actions.ts:10-78`).
- `href` items go to `openExternalURL`.

| Menu (`labelKey`) | Items → command/action/role [mac accel / win accel] |
|---|---|
| app (macOS only) :75-98 | about; checkForUpdates→`app.checkForUpdates` (enabled only if updater enabled); settings→`settings.open` [Cmd+,]; reloadWebview→`view.reload`; restart→`app.relaunch`; exportLogs→`logs.export`; hide/hideOthers/unhide; quit |
| file :99-131 | newSession→`session.new` [Shift+Cmd+S]; openProject→`project.open` [Cmd+O]; settings (windows only) [Ctrl+,]; newWindow→`window.new` [Cmd+Shift+N / Ctrl+Shift+N]; closeWindow→`window.close` (role close) |
| edit :132-181 | undo/redo/cut/copy/paste/delete/selectAll → `edit.*` [win Ctrl+Z/Y/X/C/V/A] |
| view :182-232 | toggleSidebar→`sidebar.toggle`; toggleTerminal→`terminal.toggle` [mac Ctrl+\`]; toggleFileTree→`fileTree.toggle`; reload; toggleDevTools; actualSize/zoomIn/zoomOut [win Ctrl+0 / Ctrl++ / Ctrl+-]; toggleFullScreen |
| go :233-266 | back→`common.goBack` [Cmd+[]; forward→`common.goForward` [Cmd+]]; prev/next session→`session.previous/next` [Option+Up/Down]; prev/next project→`project.previous/next` [Cmd+Option+Up/Down] |
| window :267-277 | role `windowMenu`; minimize, maximize (`window.toggleMaximize`), close |
| help :278-297 | documentation→`https://opencode.ai/docs`; supportForum→`https://discord.com/invite/opencode`; exportLogs; shareFeedback / reportBug → GitHub issue templates (:289,:294) |

**Other platform features:**
- Zoom range is 0.2–10 in steps of 0.2 (`windows.ts:65-66`; `desktop-menu-actions.ts:80-83`). Renderer-side Ctrl/Cmd `-` `=` `0` and Ctrl+wheel pinch handling is in `renderer/webview-zoom.ts:107-136`.
- Context menu: `electron-context-menu({showSaveImageAs:true, showLookUpSelection:false, showSearchWithGoogle:false})` (`index.ts:116`).
- Notifications use the HTML5 `Notification` in the renderer, shown only when the window is unfocused. The icon is `https://opencode.ai/favicon-96x96-v3.png`, and a click calls `show-window` and `set-window-focus` (`renderer/index.tsx:254-268`). The main process has no notification code. No dock badge or `setBadgeCount` anywhere (grep).
- Terminal PTY: **not in the desktop shell**. It lives in the server (`core/src/pty/pty.node.ts:1`, `@lydell/node-pty`, loaded in the sidecar; `opencode/script/build-node.ts:21` keeps it external). The desktop main uses node-pty only for interactive WSL installs (`wsl/runtime.ts:4,116-125`). Vite narrows `@lydell/node-pty` to `@lydell/node-pty-${platform}-${arch}` (`electron.vite.config.ts:15,54-63`).
- File watching: **in the server**: `core/src/filesystem/watcher.ts:26-35` requires `@parcel/watcher-<platform>-<arch>[-libc]`. Desktop only sets the env flags (`server.ts:50`; `wsl/sidecar.ts:33`). Optional deps are listed in `desktop/package.json`.
- Dialogs: open directory / open file / save file (`ipc.ts:152-209`), recovery message boxes (`windows.ts:385-392`), updater message boxes (`updater.ts:63-94`).
- Shell: `shell.openExternal` behind an allow-list (`windows.ts:236-243`), `shell.openPath` (`windows.ts:245-254`; `ipc.ts:220`), `shell.showItemInFolder` (`ipc.ts:234`; `logging.ts:66`).

### 3.9 Onboarding, logging, crash, Sentry, telemetry

Onboarding (`renderer/onboarding.tsx:13-51`):
1. Waits for server and tabs readiness.
2. Calls `isOldLayoutEligible`, which sets the settings flags.
3. If the server is local and onboarding is pending, it triggers only when all of these hold: not an existing install, `initialUrl==="/"`, no tabs, and only built-in servers.
4. `finishFirstLaunchOnboarding(true)` creates `Documents/Default Project` (`main/onboarding.ts:10,33-44`).
5. A draft tab is opened in that directory.

Logging:
- electron-log writes to `userData/logs/<YYYYMMDDTHHMMSS>/<scope>.log`. Max size is 5 MB, renderer console is spied, and the console transport is disabled when packaged (`logging.ts:22-34,101-116,191-206`).
- Run directories older than 7 days are removed (`:9,118-131`).
- Scopes in use: `main`, `renderer`, `server`, `utility`, `window`, `protocol`, `crash`, `network`, `pty`, `preload`, `onboarding`.

Crash and network capture:
- `crashReporter.start({uploadToServer:false, compress:true})` with dumps in `userData/Crashpad` (`:36-42`).
- `netLog` writes `network.netlog`, 20 MB cap (`:13,44-49`).

Debug export (`logging.ts:51-73`):
- Output: `Downloads/opencode-debug-<stamp>.zip`.
- Contents: `manifest.json` plus files changed in the last 24 h, under these prefixes:
  - `desktop/` (desktop logs)
  - `server-N/` (`$XDG_DATA_HOME/opencode/log` and `userData/opencode/log`)
  - `crashpad/`
- Exclusions: files over 50 MB and `.heapsnapshot` files.

Sentry:
- Renderer only, enabled when `VITE_SENTRY_DSN` is set. Environment is `VITE_SENTRY_ENVIRONMENT` and release is `VITE_SENTRY_RELEASE` or `desktop@<ver>`.
- Breadcrumbs are dropped. On prod, GlobalHandlers and BrowserApiErrors are dropped too (`renderer/index.tsx:40-61`).
- Sourcemap upload happens when `SENTRY_AUTH_TOKEN`, `SENTRY_ORG`, and `SENTRY_PROJECT` are set (`electron.vite.config.ts:17-32`).
- No main-process Sentry or other telemetry in `desktop/src`.

### 3.10 Build & packaging

| Item | Value | Source |
|---|---|---|
| Channel | `OPENCODE_CHANNEL` ∈ dev/beta/prod, default dev (vite also maps `latest` to prod). Baked into main as `import.meta.env.OPENCODE_CHANNEL`. | `electron.vite.config.ts:8-13,37`; `electron-builder.config.ts:32-36` |
| appId / productName | dev `ai.opencode.desktop.dev`/`OpenCode Dev`; beta `ai.opencode.desktop.beta`/`OpenCode Beta`; prod `ai.opencode.desktop`/`OpenCode` | `electron-builder.config.ts:38-42,126-156` |
| Artifact name | `opencode-desktop-${os}-${arch}.${ext}`, output `dist/` | `:45-49` |
| Files | `out/**/*`, `resources/**/*`, `!resources/opencode-cli*` | `:58` |
| extraResources | `opencode-cli*` (dev only) and `native/` → `native/` filtered to `index.js`, `index.d.ts`, `build/Release/mac_window.node`, `swift-build/**`. **The `desktop/native` directory is absent in this checkout.** | `:59-74` |
| macOS | `dmg` + `zip`, hardenedRuntime, `notarize:true`, entitlements `resources/entitlements.plist` (allow-jit, unsigned-exec-memory, disable-exec-page-protection, dyld-env, disable-library-validation, audio-input), `dmg.sign:true`, category developer-tools | `:75-87`; `desktop/resources/entitlements.plist:5-16` |
| Windows | `nsis` (`oneClick:true`, `perMachine:false`). Signed through `../script/sign-windows.ps1` only when `GITHUB_ACTIONS=true`. | `:92-105,11,21-30` |
| Linux | `AppImage`, `deb`, `rpm`; `executableName = appId`; `StartupWMClass = appId`; `desktopName = <appId>.desktop`; AppStream metainfo is generated by `scripts/copy-metainfo.ts`. rpm packageName is `opencode`/`opencode-beta`/`opencode-dev`. Prod also installs the legacy hidden `opencode-desktop.desktop`. | `:106-118,55-57,15-16,132,143,154`; `scripts/copy-metainfo.ts:6-46` |
| CI notarization env | `CSC_KEYCHAIN`, `APPLE_API_KEY`, `APPLE_API_KEY_ID`, `APPLE_API_ISSUER` | `../.github/workflows/publish.yml:427-430` |
| `scripts/predev.ts` | `bun run install-electron` (script UNKNOWN), copy icons for the channel, `cd ../opencode && bun script/build-node.ts`, `downloadCliToResources()` | `:4-9` |
| `scripts/prebuild.ts` | Copy icons and metainfo, build the opencode node bundle, download the CLI if dev | `:6-11` |
| `scripts/prepare.ts` | Runs prebuild, then sets `package.json` version from `Script.version` | `:4-9` |
| `copy-icons.ts` / `copy-bundles.ts` | `icons/<channel>` → `resources/icons`; `dist/*` → `dist/bundles` (requires `RUST_TARGET`) | `copy-icons.ts:7-11`; `copy-bundles.ts:6-12` |

### 3.11 Renderer (`desktop/src/renderer`)

- `index.html` mounts `#root` and loads `oc-theme-preload.js` (`index.html:14,18`). `publicDir` is `../../../app/public` (`electron.vite.config.ts:95`).
- `index.tsx` is a thin host for `@opencode-ai/app`:
  - `createPlatform()` adapts `window.api` to `Platform` (`:113-318`).
  - `<PlatformProvider>` + `<AppBaseProviders onNativeTranslations>` + `<AppInterface>` (`:427-436,404-423`).
  - Uses a `MemoryRouter` per window, restored from the last-active URL (`:105-111`).
  - The server list is the local sidecar (`type:"sidecar"`, `variant:"base"`, http Basic credentials) plus ready WSL servers (`:382-399`).
- Behavior notes:
  - `restart` = `killSidecar` + `relaunch` (`:249-252`).
  - On Windows, `openPath` resolves the app via `resolveAppPath` (`:217-223`).
  - Zoom actions are handled locally (`:123-137`).
  - Theme background is pushed to main (`:364-371`).
- `initialization.ts` strips the Electron remote-method error prefix and tags `localServerStartup` (`:6-15`).
- `onboarding.tsx` covers §3.9. `webview-zoom.ts` and `window-fullscreen.ts` expose zoom and fullscreen signals.
- `cli.ts` calls the unimplemented `install-cli`. `i18n/*.ts` holds desktop renderer strings for 50+ locales (`i18n/index.ts`).

### 3.12 Rust mapping (suggestions — not from source)

- **Removed in Rust:** preload, `contextBridge`, all IPC, the `oc://` protocol, the CORS header shims, and the CDP `set-force-focus`. The `Platform` trait becomes a Rust service (`trait DesktopPlatform`) called directly by GPUI views. Treat the §3.3 table as the method checklist.
- **In-process Rust:**
  - Settings store: `serde_json` files at the **same** `userData/<appId>/opencode.settings` path and keys, for upgrade continuity.
  - Drafts: `rusqlite` on `drafts.sqlite`, same schema.
  - Window registry and window state: own JSON with the same `window-state-<id>.json` shape (x, y, width, height, isMaximized, isFullScreen — assumption: electron-window-state format; verify on disk).
  - Logging: `tracing` + `tracing-appender` into `logs/<stamp>/<scope>.log`.
  - Debug export: `zip` crate.
  - Shell-env probe: `std::process::Command`.
  - Proxy and CA: `reqwest` with system proxy plus `rustls-platform-verifier`.
  - App lookup: `which` crate / `where`.
  - Health polling and updater state machine: tokio.
  - WSL `wsl.exe` wrappers: `std::process` with UTF-16LE decode via `encoding_rs`; ConPTY via `portable-pty`.
- **Remains a sidecar:** the opencode server (TS). The v1 path requires a Node/Electron runtime that the Rust app cannot provide. Either:
  - spawn the standalone CLI `opencode serve --hostname 127.0.0.1 --port <p>` with `OPENCODE_SERVER_USERNAME`/`OPENCODE_SERVER_PASSWORD` (same contract as the WSL sidecar, `wsl/sidecar.ts:35-38`), or
  - adopt v2 `opencode-cli service start` / `service get password`.
  
  Keep `/global/health` polling with Basic auth. PTY and file watching stay inside the server.
- **GPUI** (assumption: verify against the pinned gpui/gpui-component revision):
  - `cx.open_window(WindowOptions{ window_bounds, titlebar: Some(TitlebarOptions{appears_transparent:true, traffic_light_position: Some(point(px(14.),px(14.)))}), window_min_size, .. })`.
  - On Windows, a custom-drawn titlebar (gpui-component `TitleBar`) replaces `titleBarOverlay` and the renderer-drawn menu.
  - `cx.set_menus(...)` with actions bound via `cx.bind_keys` for the §3.8 accelerators.
  - Dialogs: `cx.prompt_for_paths` / `cx.prompt_for_new_path`. Prompts: `window.prompt`.
  - Shell and clipboard: `cx.open_url`, `cx.reveal_path`, `cx.open_with_system`, `cx.read_from_clipboard`.
  - Window ops: `window.toggle_fullscreen`, `minimize_window`, `zoom_window`.
  - App lifecycle: `cx.on_open_urls` (macOS deep links), `cx.on_reopen` (= `activate`).
  - Tray: not needed (none today).
- **Single instance + deep links:** use the `single-instance` crate or a named-pipe/Unix-socket listener that forwards argv. Register `opencode://` through the NSIS registry, macOS Info.plist `CFBundleURLTypes`, and Linux `.desktop` `MimeType=x-scheme-handler/opencode`. Also parse cold-start argv, which is a current gap.
- **Updater:** options are `velopack`, `cargo-packager-updater`, or `self_update` against GitHub releases. Bridging requires one last Electron release whose `latest*.yml` points at a Rust-app installer, plus Tauri `latest.json` for the still-older Tauri installs.
- **Notifications:** `notify-rust` (Linux/macOS) / `tauri-winrt-notification` (Windows), gated on window focus as today.
- **Crash reporting:** `crash-handler` + `minidumper`, or `sentry` with the `minidump` feature.

### 3.13 Porting risks

1. **Sidecar runtime swap.** v1 runs `opencode/dist/node/node.js` inside an Electron utilityProcess (Node ABI, `.wasm` chunks, `@lydell/node-pty` and `@parcel/watcher` native modules). A Rust host must ship and spawn a standalone binary, which changes startup latency, code signing (notarizing an extra executable), and Windows Defender behavior. v2 (`service start`) makes a daemon that outlives the app and is shared across channels (`background-cli.ts:12,27-43`).
2. **User-data continuity.** Files without extensions (`opencode.settings`, `opencode.updater`), renderer `*.dat` stores whose names are hashed in `app/src/utils/persist.ts`, `drafts.sqlite`, and window ids/state all live under `appData/<appId>`. Any rename loses sessions, tabs, and drafts. A Tauri→Electron migration already exists (`migrate.ts`), so stores carry mixed provenance.
3. **Update path.** Existing installs only understand electron-updater GitHub feeds (`latest*.yml`, NSIS oneClick per-user, mac zip). The cut-over must be shipped as an Electron update whose payload installs the Rust app at a compatible location/appId. Beta uses a separate repo (`opencode-beta`).
4. **Windows chrome.** Frameless window, a titlebar overlay that scales with zoom, and a renderer-drawn menu (`windows-app-menu.tsx`). GPUI's Windows/Linux backends need their custom titlebar, snap layouts, and DPI behavior validated.
5. **WSL feature parity.** Elevation through PowerShell, ConPTY interactive installs, UTF-16LE output detection, `wsl.exe` hang timeouts, and strict version equality between the desktop and the in-distro CLI (`wsl/startup.ts:7-16`).
6. **Environment fidelity.** The login-shell `env -0` probe, the `NO_PROXY` loopback injection, `setGlobalProxyFromEnv`, merged system CAs, and `XDG_STATE_HOME=userData`. Dropping any of these breaks tools or corporate-proxy users.
7. **Whole UI rewrite.** `packages/app` (SolidJS) is the real product surface. The desktop shell is thin, so most porting cost sits outside this section: menu command ids, i18n `DesktopNativeBundle` keys (`app/src/i18n/desktop-native.ts:224-359`), and deep-link semantics must be re-implemented.
8. **Diagnostics regression.** Crashpad dumps, netlog, unresponsive JS-stack sampling, and the debug zip layout have no 1:1 equivalent. Support workflows that depend on `opencode-debug-*.zip` will change.

### 3.14 UNKNOWN / to verify

| # | Item | How to determine |
|---|---|---|
| 1 | `bun run install-electron` (`scripts/predev.ts:4`) is not defined in `desktop/package.json` or the root `package.json` | `rg -n "install-electron" -g package.json -g "*.toml" ..` across the monorepo; inspect `bunfig.toml` |
| 2 | `desktop/native` (`mac_window.node`, `swift-build`) is referenced (`electron-builder.config.ts:69-74`, `package.json:23`) but absent | Check upstream at tag v1.19.5: `git ls-tree v1.19.5 packages/desktop/native`; grep `require("native")` / `mac_window` |
| 3 | `install-cli` IPC has no `ipcMain.handle`, so is `installCli` reachable? | `rg -n "installCli\|renderer/cli" packages/app packages/desktop` |
| 4 | Windows↔WSL path translation for projects in WSL distros | `rg -n "wsl\|\\\\\\\\wsl" packages/app/src packages/opencode/src` |
| 5 | Beta build pipeline (publish.yml hard-codes `OPENCODE_CHANNEL: prod` at :401/:410/:425/:438) | Inspect other workflows and `../script/` for `beta`; check the `opencode-beta` repo releases |
| 6 | Linux: default Electron menu shown? Cold-start `opencode://` argv on Windows/Linux ignored? | Runtime test: launch `OpenCode opencode://open-project?directory=/tmp` with no running instance |
| 7 | Exact `appData` per OS and the `window-state-*.json` schema | Runtime: `app.getPath("appData")`; inspect a file in userData |

## 4. App UI (packages/app, ui, session-ui)

Source: opencode `v1.19.5` (`app/package.json:3`). All refs are `path:line` relative to `packages/`; `../package.json` is the monorepo root (dependency catalog). File counts measured with `find`: app/src 481 files, ui 1697 (incl. ~1200 icon SVGs), session-ui 118.
Ref shorthand: `context/…`, `utils/…`, `pages/…`, `components/…`, `constants/…` without a package prefix are under `app/src/`; a bare `:N` refers to the file named last in the same cell/bullet; `en.ts:N` = `app/src/i18n/en.ts`.

### 4.0 Package map

| Package | Role | Key deps (version source) |
|---|---|---|
| `app` | Routes, pages, contexts, dialogs, settings, terminal, prompt composer | `solid-js 1.9.10` (`../package.json:92`), `@solidjs/router 0.15.4` (`../package.json:88`), `@kobalte/core 0.13.11` (`../package.json:49`), `@tanstack/solid-query 5.91.4` (`app/package.json:80`), `@tanstack/solid-virtual 3.13.32` (`../package.json:46`), `ghostty-web` git `83c0a07` (`app/package.json:86`), `@pierre/trees 1.0.0-beta.4` (`app/package.json:63`), `@dnd-kit/solid 0.5.0` (`app/package.json:55`), `@thisbeyond/solid-dnd 0.7.5` (`app/package.json:82`), `tailwindcss 4.1.11` (`../package.json:85`) |
| `ui` | Design system: primitives (v1 + `v2/`), theme engine, markdown parser, i18n for UI strings, icons, fonts, audio | `@pierre/diffs 1.2.10` (`../package.json:58`), `shiki 4.2.0` (`../package.json:83`), `marked 18.0.7` (`../package.json:73`), `marked-shiki 1.2.1` (`../package.json:74`), `katex 0.16.27` (`ui/package.json:94`), `dompurify 3.3.1` (`ui/package.json:92`), `morphdom 2.7.8` (`ui/package.json:98`), `remend 1.3.0` (`../package.json:75`) |
| `session-ui` | Session-specific rendering: message parts, tool renderers, markdown streaming worker, file/diff viewer (pierre), review panel, v2 prompt input | `@pierre/diffs` (`session-ui/package.json:45`), `@shikijs/stream 4.2.0` (`../package.json:47`), `motion 12.34.5` (`session-ui/package.json:59`) |

### 4.1 Architecture

**Entry / bootstrap**
- Web entry `app/src/entry.tsx`: builds a `Platform` object (`entry.tsx:119-131`), computes server URL (`getCurrentUrl` `entry.tsx:99-104`: `http://localhost:4096` when hosted on opencode.ai; dev uses `VITE_OPENCODE_SERVER_HOST/PORT`; else `location.origin`), reads `?auth_token=` (base64 `user:pass`, `app/src/utils/server.ts:10-19`) and strips it from the URL (`entry.tsx:112-117`), then renders `PlatformProvider > AppBaseProviders > AppInterface` (`entry.tsx:166-176`).
- Library exports for the desktop shell: `app/src/index.ts:1-30` (`AppBaseProviders`, `AppInterface`, `PlatformProvider`, `useCommand`, `useSettings`, `ServerConnection`, …).
- Provider order (outer→inner): `MetaProvider > Font > ThemeProvider > LanguageProvider > UiI18nBridge > ErrorBoundary > QueryProvider > WslServersProvider > DialogProvider > FileComponentProvider` (`app/src/app.tsx:400-425`); then `ServerProvider > GlobalProvider > SettingsProvider > ConnectionGate > Router(root: TabsProvider > PermissionProvider > NotificationProvider > ServerShell(QueryProvider > CommandProvider > HighlightsProvider))` (`app/src/app.tsx:580-611`, `313-320`). Server-bound subtree: `ServerSDKProvider > ServerSyncProvider` (`app.tsx:167-173`), then `LayoutProvider > ModelsProvider` (`app.tsx:354-359`), directory scope `ModelsProvider > SDKProvider > DirectoryDataProvider` (`app.tsx:219-221`), draft scope `FileProvider > PromptProvider > CommentsProvider` (`app.tsx:383-390`).

**Routes** (`@solidjs/router`; `newLayoutDesigns` toggles the route set, `app/src/app.tsx:615-644`)

| URL pattern | Component | Layout | Ref |
|---|---|---|---|
| `/` | `NewHome` (projects + sessions grid) | new | `app.tsx:639` |
| `/server/:serverKey/session/:id` | `TargetSessionRoute` (serverKey = base64 server key, `utils/session-route.ts:5-7`) | new | `app.tsx:641` |
| `/:dir/session/:id` | redirect to server-scoped session URL | new | `app.tsx:640` |
| `/new-session?draftId=` | `DraftRoute` (prompt-only page) | both | `app.tsx:643`, `app.tsx:185-201` |
| `/` | `LegacyHome` | legacy | `app.tsx:628` |
| `/server/:serverKey/session/:id` | `LegacyTargetSessionRoute` | legacy | `app.tsx:629` |
| `/:dir` → `/:dir/session/:id?` | `DirectoryLayout` + `SessionRoute` (`dir` = base64 path, `utils/session-route.ts:9-11`) | both | `app.tsx:633-635` |

Legacy layout is retired: `oldInterfaceSunset = new Date(2026, 8, 14)` (`app/src/context/settings.tsx:63`) and `resolveNewLayoutDesigns(retired=true) → true` (`settings.tsx:129-132`). **Port target = new layout** (`app/src/pages/layout-new.tsx:9-49`: `Titlebar` + `<main>` + `TabsInfoPopup` + `ToastRegion v2`).

**Contexts** (`app/src/context/*`, one line each)

| Context | Ref | Responsibility |
|---|---|---|
| Platform | `context/platform.tsx:138` | Host abstraction (see below) |
| Server | `context/server.tsx:255` | Server list, projects per server, last project, recently closed; persisted `server` |
| Global | `context/global.tsx:13` | Server health polling, settings-dialog target server |
| Settings | `context/settings.tsx:228` | All user prefs (§4.5) |
| Language | `context/language.tsx:167` | Locale, dict loading, RTL |
| Command | `context/command.tsx:264` | Command registry, keybind parse/dispatch, palette, slash triggers |
| Highlights | `context/highlights.tsx:140` | Release-notes dialog from `https://opencode.ai/changelog.json` (`highlights.tsx:10`) |
| Tabs | `context/tabs.tsx:54` | Titlebar tabs (`session` / `draft` types, `tabs.tsx:17-31`), recent/closed |
| Permission | `context/permission.tsx:55` | Pending permission requests, auto-accept per session |
| Notification | `context/notification.tsx:113` | Unseen/idle/error tracking, OS notify + sounds |
| ServerSDK | `context/server-sdk.tsx:387` | SDK/API clients per server + global SSE loop + event bus |
| ServerSync | `context/server-sync.tsx:741` | Server-global store (config, providers, projects, sessions index) |
| Layout | `context/layout.tsx:160` | Panel sizes/open state, review diff style, per-session view state |
| Models | `context/models.tsx:25` | User/recent model list, variants |
| SDK | `context/sdk.tsx:7` | Directory-scoped client |
| Local | `context/local.tsx:59` | Per-workspace agent/model selection |
| Prompt | `context/prompt.tsx:74` | Prompt draft state (+ `prompt-state.ts`) |
| File | `context/file.tsx:56` | File tree, content cache, watcher (`context/file/*`) |
| Comments | `context/comments.tsx:210` | Line comments on files/diffs attached to prompts |
| Terminal | `context/terminal.tsx:458` | PTY list per workspace, buffers, titles |
| Sync | `context/sync.tsx:113` (`useSync`) | Directory message/part store with optimistic updates |
| (non-provider) | `directory-sync.ts:24`, `mcp.ts:6`, `server-session.ts:188`, `server-session-v2-reducer.ts`, `global-sync/*` | Dir sync, MCP toggle, session loaders, event reducer |
| WslServers | `wsl/context.tsx:10` | WSL sidecar servers (Windows desktop only) |

**Server transport**
- Two protocols auto-detected: `GET /global/health` → `{healthy:true}` = v1; `GET /api/health` → `{pid:number}` = v2; default v2 (`app/src/utils/server-protocol.ts:24-35`, 5 s timeout `:16`).
- Clients: v1 `createOpencodeClient` from `@opencode-ai/sdk/v2/client` (`app/src/utils/server.ts:1,21-42`); v2 `OpenCode.make` from vendored `@opencode-ai/client` tgz `1.17.13-v2` (`app/package.json:59`, `utils/server.ts:44-60`). Auth = HTTP Basic, username default `opencode` (`utils/server.ts:6-8,30`).
- Event stream: v1 `eventSdk.global.event()` → `GET /global/event` (`context/server-sdk.tsx:278`, path `sdk/js/src/v2/gen/sdk.gen.ts:1338`); v2 `eventApi.event.subscribe()` → `GET /api/event` (`server-sdk.tsx:279`, `client/src/generated/client.ts:814`). Events routed by `directory` (`server-sdk.tsx:285`), coalesced in a queue (`server-sdk.tsx:56-131`), yield every 8 ms (`server-sdk.tsx:219`), reconnect loop (`server-sdk.tsx:268-300`).
- Event types reduced (`app/src/context/global-sync/event-reducer.ts:126-474`): `server.instance.disposed`, `session.created|updated|deleted|renamed|usage.updated|moved|diff|status`, `todo.updated`, `message.updated|removed`, `message.part.updated|removed|delta`, `vcs.branch.updated`, `permission.asked|replied`, `question.asked|replied|rejected`, `lsp.updated`, `reference.updated`; plus `global.disposed`/`server.connected`/`project.updated` (`event-reducer.ts:43-48`).
- Bootstrap REST calls (`app/src/context/global-sync/bootstrap.ts`): `config.get` (:113,:384), `provider.list` (:233,:238), `app.agents` (:269), `command.list` (:282), `path.get` (:309), `session.status` (:390), `project.current` (:415), `vcs.get` (:429), `permission.list` (:447), `question.list` (:483).
- Multi-server: `ServerConnection.Any = Http | Sidecar(base|wsl) | Ssh` (`context/server.tsx:181-222`), key = URL / `sidecar` / `wsl:<distro>` / `ssh:<host>` (`server.tsx:224-235`). Persisted state is scoped per server (`Persist.serverGlobal/serverWorkspace`, `app/src/utils/persist.ts:501-516`). Web default server override: localStorage `opencode.settings.dat:defaultServerUrl` (`entry.tsx:15`).
- Deep links: `opencode://open-project?directory=` and `opencode://new-session?directory=&prompt=` (`app/src/pages/layout/deep-links.ts:13-31`).

**Platform boundary** (`app/src/context/platform.tsx:31-134`; implemented by Electron shell — covered elsewhere): `openExternal`, `openPath`, `openLocalFile`, `revealPath`, `restart`, `notify`, `openAttachmentPickerDialog`, `getPathForFile`, `saveFilePickerDialog`, `storage(name)`, `draftStore`, `windowID`, `updater`, `fetch`, `get/setDefaultServer`, `wslServers`, `get/setDisplayBackend`, `webviewZoom`, `windowFullscreen`, `get/setPinchZoomEnabled`, `runDesktopMenuAction`, `checkAppExists`, `readClipboardImage`, `exportDebugLogs`, `setForceFocus`, `recordFatalRendererError`; desktop variant adds `openDirectoryPickerDialog` (`platform.tsx:129-133`). Titlebar color sync via `window.api?.setTitlebar` (`app.tsx:404`). In Rust these collapse into direct native calls.

**Data flow (for the Rust port)**
1. Connect: probe protocol → build clients → `ConnectionGate` health check (`app.tsx:430`, `utils/server-health.ts:17` 30 s timeout; skipped on web via `disableHealthCheck`, `entry.tsx:172`).
2. Bootstrap server-global + per-directory stores (`global-sync/bootstrap.ts`), lazily per opened directory (`createDirSdkContext`, `server-sdk.tsx:412-434`).
3. One SSE stream per server → queue/coalesce (`server-sdk.tsx:56-131`) → emit on `directory` channel (`:241`) → `event-reducer.ts` mutates stores (`message.part.delta` appends text, `:364`).
4. Views are pure projections of stores; user actions call REST (prompt submit `components/prompt-input/submit.ts`, permission reply via `permission.respond` `context/permission.tsx:155`).
5. UI-only state goes through `persisted()` (§4.6); prompt drafts through `draftStore`.

### 4.2 Layout inventory (new layout)

| Area | Source | Notes |
|---|---|---|
| Titlebar + tab strip | `app/src/components/titlebar.tsx`, `titlebar-tab-strip.tsx`, `titlebar-tab-popover.tsx`, `titlebar-history.ts` | Home toggle, back/forward, session/draft tabs (drag-reorder `@dnd-kit`, `titlebar-tab-strip.tsx:4-9`), update button, status popover (`status-popover.tsx`) |
| Home | `app/src/pages/home.tsx:11-50`, `pages/home/home-projects*.tsx`, `home-sessions*.tsx`, `home-session-search-controller.ts` | 2-col grid `280px / ≤720px`, max 1080 px (`home.tsx:34-35`); project list reorderable (`home-projects-view.tsx:3-7`) |
| Legacy sidebar (projects/workspaces/sessions) | `app/src/pages/layout.tsx` (2444 lines), `pages/layout/sidebar-*.tsx` | Legacy only; width 344 (`context/layout.tsx:30`) |
| New-session draft | `app/src/pages/new-session.tsx`, `pages/new-session/*` | Project/workspace picker + composer |
| Session page | `app/src/pages/session.tsx` (2391 lines) | Timeline + composer + side panel |
| Message timeline | `app/src/pages/session/timeline/message-timeline.tsx` | Virtualized (`createVirtualizer`, `message-timeline.tsx:18,417`) |
| Turn / message rendering | `session-ui/src/components/session-turn.tsx`, `message-part.tsx` | §4.3 |
| Composer region + docks | `app/src/pages/session/composer/session-composer-region.tsx`, `session-permission-dock.tsx`, `session-question-dock.tsx`, `session-todo-dock.tsx`, `session-revert-dock.tsx`, `session-followup-dock.tsx` | Permission replies `once/always/reject` (`session-permission-dock.tsx:11`); questions support multi-select + custom answer (`session-question-dock.tsx:77-103`) |
| Prompt input | `app/src/components/prompt-input-v2.tsx` → `session-ui/src/v2/components/prompt-input/index.tsx`; legacy `components/prompt-input.tsx` | §4.4 |
| Side panel (review / files / context) | `app/src/pages/session/session-side-panel.tsx`, `pages/session/v2/review-panel-v2.tsx`, `v2/session-file-browser-tab.tsx`, `v2/session-file-list-v2.tsx`, `components/session/session-context-tab.tsx` | Review diff style `unified|split` (`context/layout.tsx:88`, default `split` `:285`) |
| File tree | `app/src/components/file-tree-v2.tsx` (virtualized, `:18`), legacy `file-tree.tsx` | Tabs `changes|all` (`context/layout.tsx:291`) |
| File viewer / diff viewer | `session-ui/src/components/file.tsx`, `file-media.tsx`, `file-search.tsx`, `session-ui/src/pierre/*` | `@pierre/diffs` render, find-in-file, line selection → comments |
| Terminal panel | `app/src/pages/session/terminal-panel-v2.tsx`, `components/terminal.tsx` | Height 280 default (`context/layout.tsx:33`); §4.8 |
| Session header / open-in-app | `app/src/components/session/session-header.tsx`, `open-in-app-v2.tsx` | Opens dir in Finder/IDE (`open.app` pref) |
| Context usage | `app/src/components/session-context-usage.tsx`, `components/session/session-context-*.ts` | Token/context breakdown |
| Command palette | `app/src/components/dialog-command-palette-v2.tsx`, `command-palette.ts` | Filters hidden/disabled/`file.open` (`context/command.tsx:90-95`) |
| Settings dialog | `app/src/components/settings-v2/dialog-settings-v2.tsx:60-108` | Tabs: general, shortcuts, servers, providers, models |
| Other dialogs | `components/dialog-select-model.tsx`, `dialog-select-model-unpaid-v2.tsx`, `dialog-manage-models.tsx`, `dialog-connect-provider.tsx`, `dialog-custom-provider.tsx`, `dialog-select-mcp.tsx`, `dialog-select-file.tsx`, `dialog-select-directory-v2.tsx` (`@pierre/trees`, `:2`), `dialog-select-server.tsx`, `dialog-edit-project-v2.tsx`, `dialog-fork.tsx`, `dialog-release-notes.tsx`, `dialog-usage-exceeded.tsx` | Dialog stack via `ui/src/context/dialog.tsx` |
| Toasts | `ui/src/components/toast.tsx:118` (`showToast`), `:166` (`showPromiseToast`), Kobalte toaster (`toast.tsx:1`); v2 region `app/src/utils/toast` | Used in 36 app files |
| Error page | `app/src/pages/error.tsx` | ErrorBoundary fallback (`app.tsx:411-414`) |

### 4.3 Message & part rendering (session-ui)

- Part dispatch table `PART_MAPPING` (`session-ui/src/components/message-part.tsx:250`), extension point `registerPartComponent` (`:932`). Registered: `tool` (`:1534`), `compaction` (`:1649`), `text` (`:1654`, paced streaming markdown `PacedMarkdown` `:336`), `reasoning` (`:1759`, hidden unless `showReasoningSummaries`, `renderable()` `:711`).
- User message: `UserMessageDisplay` (`:1181`) with highlighted `@file`/`@agent` references (`HighlightedText` `:1396`), attached comments (`:1153`), file chips (`message-file.ts:27` labels PDFs).
- Assistant: `AssistantParts` (`:724`), consecutive context tools grouped (`ContextToolGroup` `:1043`; read/list/glob/grep `:828-905`).
- Tool renderers (`ToolRegistry.register`): `read` `:1776`, `list` `:1816`, `glob` `:1842`, `grep` `:1872`, `webfetch` `:1905`, `websearch` `:1951` (Exa output `:906`), `task` (sub-agent, links to child session) `:1978`, `shell` (alias `bash` `:523-524`) `:2085`, `edit` `:2155`, `write` `:2261`, `patch`/`apply_patch` `:2321`, `todowrite` `:2525`, `question` `:2576`, `skill` `:2621`; title/icon table `getToolInfo` `:469-570`; unknown tools → generic `basic-tool.tsx`/`basic-tool-v2.tsx`. Diagnostics block `:136-206`. Error card `tool-error-card(-v2).tsx`. Retry banner `session-retry.tsx`.
- Media: images/audio/SVG preview in `session-ui/src/components/file-media.tsx:188-253` (`<img>`, `<audio>`, sanitized SVG); PDF only as attachment chip (no PDF renderer found; `grep application/pdf` hits only MIME handling).

### 4.4 Prompt input

| Feature | Ref |
|---|---|
| Accepted attachments: png/jpeg/gif/webp, pdf, `text/*`, json/toml/yaml/xml + 43 source extensions | `app/src/constants/file-picker.ts:1-56` |
| Paste (files + text) | `app/src/components/prompt-input/attachments.ts:94-113`, `paste.ts` |
| Global drag-over / drop overlay | `prompt-input/attachments.ts:207`, `drag-overlay.tsx` |
| Image thumbnails | `prompt-input/image-attachments.tsx` |
| `@` mentions: agents (`type:"agent"`) and files (`type:"file"`) | `app/src/components/prompt-input.tsx:605-639` |
| `/` slash menu: built-ins from commands with `slash` + server custom commands (`custom.<name>`) | `prompt-input.tsx:694-715`, `prompt-input/slash-popover.tsx` (badges skill/mcp/custom `:341-353`) |
| Built-in slash triggers | `/new` `/undo` `/redo` `/compact` `/fork` `/export` `/open` `/terminal` `/mcp` (`app/src/pages/session/use-session-commands.tsx:342,355,363,371,379,387,411,440,518`), `/model` `/agent` (`use-composer-commands.tsx:55,70`), `/workspace` (`pages/layout.tsx:992`) |
| Shell mode: `!` at empty normal prompt | `prompt-input.tsx:1258` |
| History (normal + shell), persisted via draft store | `prompt-input/history-store.ts:44-49` |
| Model/variant/agent controls in composer | `prompt-input-v2.tsx:62,393-401`, `pages/session/composer/prompt-model-selection.ts` |
| Follow-up queue vs steer while busy | `pages/session.tsx:1757`, `composer/session-followup-dock.tsx` |
| Context items (comments, selections) | `prompt-input/context-items.tsx`, `build-request-parts.ts` |

### 4.5 Settings (every option)

Main store: `persisted("settings.v3", …)` (`app/src/context/settings.tsx:233`), defaults `settings.tsx:183-222` (Default column `:N` = `settings.tsx`, Label column `:N` = `en.ts`). UI rows: legacy dialog `app/src/components/settings-general.tsx`, v2 dialog `app/src/components/settings-v2/general.tsx`. Labels from `app/src/i18n/en.ts`.

| Key | Type | Default | UI label (en.ts) | UI row |
|---|---|---|---|---|
| `general.autoSave` | bool | `true` (:185) | — | none found (no consumer found by grep) |
| `general.releaseNotes` | bool | `true` (:186) | "Release notes" (:969) | `settings-general.tsx:680` |
| `general.followup` | `queue\|steer` | `steer` (:187) | "Follow-up behavior" (:926) | label exists, no row found; read at `pages/session.tsx:1757` |
| `general.showFileTree` | bool | `false` (:188) | "File tree" (:930) | `settings-general.tsx:395`, `settings-v2/general.tsx:395` |
| `general.showNavigation` | bool | `false` (:189) | "Navigation controls" (:932) | `settings-general.tsx:407`; used `titlebar.tsx:123` |
| `general.showSearch` | bool | `false` (:190) | "Command palette" (:934) | `settings-general.tsx:419` |
| `general.showStatus` | bool | `false` (:191) | "Server status" (:938) | `settings-general.tsx:431` |
| `general.showTerminal` | bool | `false` (:192) | "Terminal" (:936) | none found |
| `general.showReasoningSummaries` | bool | `false` (:193) | "Show reasoning summaries" (:946) | `settings-general.tsx:351` |
| `general.shellToolPartsExpanded` | bool | `false` (:194) | "Expand shell tool parts" (:948) | `settings-general.tsx:363` |
| `general.editToolPartsExpanded` | bool | `false` (:195) | "Expand edit tool parts" (:951) | `settings-general.tsx:375` |
| `general.showCustomAgents` | bool | `false` (:196) | "Show agent" (:943) | `settings-general.tsx:443` |
| `general.mobileTitlebarPosition` | `top\|bottom` | `top` (:197) | "Bottom navigation" (:940) | `settings-v2/general.tsx:374-380` |
| `general.newLayoutDesigns` | bool? | resolved, `true` (:61) | "New layout" (:954) | `settings-general.tsx:261` (forced true after sunset) |
| `general.layoutTransitionEligible`, `agentVisibilityInitialized`, `newInterfaceNoticeDismissed`, `shouldDisplayTabsToast` | bool? | unset (:38-41) | internal | notice row `settings-general.tsx:288` |
| `appearance.fontSize` | number | `14` (:200) | — | none found |
| `appearance.sans` | string | `""` → "System Sans" (:58,:202) | "UI Font" (:924) | `settings-general.tsx:506` |
| `appearance.mono` | string | `""` → "System Mono" (:57,:201) | "Code Font" (:920) | `settings-general.tsx:529` |
| `appearance.terminal` | string | `""` → "JetBrainsMono Nerd Font Mono" (:59,:203) | "Terminal Font" (:922) | `settings-general.tsx:552` |
| `keybinds` | `Record<id,combo>` | `{}` (:205) | "Shortcuts" tab (:895) | `components/settings-keybinds.tsx` |
| `permissions.autoApprove` | bool | `false` (:207) | — | none found (setter `settings.tsx:498`) |
| `notifications.agent/permissions/errors` | bool | `true/true/false` (:210-212) | "Agent"/"Permissions"/"Errors" (:1029,:1032,:1034) | `settings-general.tsx:583,595,607` |
| `sounds.agentEnabled` + `sounds.agent` | bool + id | `true`, `staplebops-01` (:215-216) | "Agent" (:1037) | `settings-general.tsx:627` |
| `sounds.permissionsEnabled` + `sounds.permissions` | bool + id | `true`, `staplebops-02` (:217-218) | "Permissions" (:1039) | `settings-general.tsx:642` |
| `sounds.errorsEnabled` + `sounds.errors` | bool + id | `true`, `nope-03` (:219-220) | "Errors" (:1041) | `settings-general.tsx:657` |

Font stacks: sans `ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif` (`settings.tsx:136`), mono `ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono", "Courier New", monospace` (`:134-135`), user font prepended (`:153-157`).

Settings outside `settings.v3`:

| Setting | Storage | Default | Label / row |
|---|---|---|---|
| Theme id | localStorage `opencode-theme-id` (`ui/src/theme/context.tsx:15`) | `oc-2` (`context.tsx:180`) | "Theme" (en.ts:918) / `settings-general.tsx:481` |
| Color scheme `light\|dark\|system` | localStorage `opencode-color-scheme` (`context.tsx:16`) | `system` (`context.tsx:181`) | "Color scheme" (en.ts:916) / `settings-general.tsx:463` |
| Resolved theme CSS cache | `opencode-theme-css-light` / `-dark` (`context.tsx:17-18`) | — | non-oc-2 only (`context.tsx:139-141`) |
| Language | `Persist.global("language")` → `opencode.global.dat:language` (`language.tsx:143,172`) + cookie `oc_locale` (`language.tsx:38-40`) | navigator detect | "Language" (en.ts:908) / `settings-general.tsx:303` |
| Terminal shell | server config `config.shell` (`settings-general.tsx:158`) | auto | "Terminal shell" (en.ts:910) / `:329` |
| Pinch-to-zoom (desktop) | `platform.setPinchZoomEnabled` (`settings-general.tsx:199-203`) | platform | en.ts:961 / `:710` |
| Native Wayland (Linux desktop) | `platform.setDisplayBackend` (`settings-general.tsx:191-197`) | `auto` | en.ts:964 / `:720` |
| Check for updates (action) | `platform.updater` | — | en.ts:974 / `:692` |
| Servers / Providers / Models tabs | server list (`server` store), server config, `settings-v2.models.providers` (`settings-v2/models.tsx:27`) | — | `dialog-settings.tsx:48-65` |

Settings section headings: Appearance, Advanced, System notifications, Updates, Sound effects, Display (`en.ts:900-906`).

### 4.6 Persistence layer (all `persisted(` call sites)

Mechanics (`app/src/utils/persist.ts`): wrapper over `@solid-primitives/storage makePersisted` (`:684`), defaults deep-merged on read (`:175-215`), legacy-key migration (`:234-347`). Physical names: global `opencode.global.dat` (`:28`), window `opencode.window.<id>.dat` (`:361-364`, web id `browser` `:533`), workspace `opencode.workspace.<first12>.<checksum(dir)>.dat` (`:349-353`), draft `opencode.draft.<first12>.<checksum>.dat` (`:355-359`). Web: localStorage key `<storageName>:<key>` (`:388-391`), raw key when no storage name (`:592`); desktop: `platform.storage(name)` (`:591`); prompt drafts/history → `platform.draftStore` (web IndexedDB `opencode-drafts`, `app/src/utils/draft-store.ts:98`). In-memory LRU 500 entries / 8 MiB (`persist.ts:33-34`); quota eviction (`:112-165`). Server-scoped keys use `ScopedKey.from(scope,key)` for non-local servers (`:501-504`).

| Key | Scope helper | Ref |
|---|---|---|
| `settings.v3`, `app-version.v1`, `highlights.v1` | raw | `context/settings.tsx:233,235`, `context/highlights.tsx:147` |
| `language`, `model`, `server`, `command.catalog.v1`, `open.app`, `home.servers`, `new-session.provider-tip`, `go-upsell`, `review-panel-v2` | global | `language.tsx:172`, `models.tsx:32`, `server.tsx:265`, `command.tsx:278`, `components/session/open-in-app.tsx:166`, `pages/home/home-projects-controller.tsx:27`, `pages/new-session/new-session-view.tsx:102`, `pages/session/usage-exceeded-dialogs.tsx:44`, `pages/session/v2/review-panel-v2-state.ts:13` |
| `prompt-history`, `prompt-history-shell` | global + draftStore | `components/prompt-input/history-store.ts:45,49` |
| `layout` (legacy `layout.v6`), `layout.page`, `notification`, `permission`, `settings-v2.models.providers` | serverGlobal | `context/layout.tsx:270`, `pages/layout.tsx:89`, `notification.tsx:236`, `permission.tsx:193`, `settings-v2/models.tsx:27` |
| `model-selection`, `followup`, `terminal` | serverWorkspace | `context/local.tsx:77`, `pages/session.tsx:607`, `context/terminal.tsx:117` |
| `comments`, `file-view`, `prompt` | serverScoped (session or workspace) | `context/comments.tsx:185`, `context/file/view-cache.ts:41`, `context/prompt-state.ts:241`, list `context/layout.tsx:321-325` |
| `tabs`, `tabs.recent`, `tabs.info`, `tabs.closed` | window | `context/tabs.tsx:62,67-69` |

`layout` store shape (`context/layout.tsx:273-306`): `sidebar{opened,width=344,workspaces,workspacesDefault}`, `terminal{height=280,opened}`, `review{diffStyle="split",panelOpened=false}`, `fileTree{opened,width=200,tab="changes"}`, `session{width=600}`, `mobileSidebar`, `sessionTabs`, `sessionView`, `handoff`, `home.selection`; max 50 session keys (`:313`).

### 4.7 Rendering stack

| Concern | Implementation | Ref |
|---|---|---|
| Markdown parser | `marked` + `marked-shiki` + custom KaTeX extension (`katex.renderToString`) | `ui/src/context/marked-parser.tsx:1-3,23,64` |
| Streaming markdown | Web Worker (`markdown.worker.ts?worker`), `remend` repairs partial markdown, block projection, `@shikijs/stream` `ShikiStreamTokenizer` | `session-ui/src/components/markdown-worker.ts:1,120`, `markdown.worker.ts:3-16`, `markdown-stream.ts:1-2` |
| DOM update / safety | `morphdom` diffing (`markdown.tsx:615`), DOMPurify sanitize (`markdown-cache.tsx` `sanitizeMarkdown`, imported `markdown.tsx:33`) | `session-ui/src/components/markdown.tsx` |
| Syntax highlighting | Shiki via `@pierre/diffs getSharedHighlighter({themes:["OpenCode"], preferredHighlighter:"shiki-wasm"})`, languages lazy-loaded from `bundledLanguages`, unknown → `text` | `ui/src/context/marked.tsx:14-26` |
| Code theme | Single custom theme `OpenCode` whose colors are CSS vars (`--syntax-*`, `--text-base`, `--color-background-stronger`) | `ui/src/context/marked-theme.tsx:3-12`; registered `marked-theme-register.tsx:1-2` |
| Diff/file viewer | `@pierre/diffs` (SSR preload, virtualizer, worker pool `WorkerPoolManager` + shiki worker) | `session-ui/src/components/file.tsx:20-46`, `pierre/worker.ts:1-14`, `pierre/virtualizer.ts:1` |
| Diff summary chips | `ui/src/components/diff-changes.tsx`, `v2/components/diff-changes-v2.tsx` | — |
| Mermaid | Not rendered (only a language id in `session-ui/src/components/markdown-inline-code-kind.ts:706`) | — |
| Images | `<img>`/`<audio>`/SVG in `file-media.tsx:231-253`; `ui/src/components/image-preview.tsx` | — |
| PDF preview | none found | — |
| Virtualization | `@tanstack/solid-virtual` in timeline (`message-timeline.tsx:18`), file tree v2 (`file-tree-v2.tsx:18`), file list v2, directory picker v2 | — |
| Animation | `motion` (`ui/package.json:99`), `ui/src/styles/animations.css` (141 lines) | — |

### 4.8 Terminal

- Emulator: `ghostty-web` (WASM; lazy `import("ghostty-web")` + `Ghostty.load()`, `app/src/components/terminal.tsx:33-38`), `FitAddon` (`:7`), options `cursorBlink:true, fontSize:14, fontFamily=terminalFontFamily(settings), scrollback:10_000` (`terminal.tsx:401-410`).
- Colors: derived from theme — fg/cursor = `text-stronger`, bg = `v2-background-bg-base` (new layout) or `background-stronger`, selection = fg @ 0.25 dark / 0.2 light (`terminal.tsx:265-287`); no ANSI palette override found (only these 4 keys).
- Transport: WebSocket `ws(s)://<server>/pty/<id>/connect?directory=&cursor=` (v1) or `/api/pty/<id>/connect?location[directory]=&cursor=` (v2) (`app/src/utils/terminal-websocket-url.ts:15-23`); auth via `ticket` from `pty.connectToken` (header `x-opencode-ticket: 1`, `terminal.tsx:560-575`) else `auth_token` query (v1, `terminal-websocket-url.ts:28-33`). `binaryType="arraybuffer"`; binary frame starting `0x00` = JSON control `{cursor}` (`terminal.tsx:645-660`); resize via REST `pty.update` (`terminal.tsx:243-256`); reconnect with backoff (`terminal.tsx:595-604`, `:602`). Buffer/cursor/scroll restored from persisted `terminal` store (`terminal.tsx:197-223`).
- Files: `app/src/components/terminal.tsx` (757 lines), `app/src/context/terminal.tsx` (546), `context/terminal-title.ts`, `pages/session/terminal-panel-v2.tsx`, `pages/session/terminal-label.ts`, tabs `components/session/session-sortable-terminal-tab-v2.tsx`.

### 4.9 Drag & drop, paste, notifications, sounds, favicon

- DnD: tab strip & home projects via `@dnd-kit` (`titlebar-tab-strip.tsx:4-9`, `home-projects-view.tsx:3-7`); legacy sidebar via `@thisbeyond/solid-dnd` (`pages/layout/sidebar-shell.tsx:9`); file drop on prompt (`prompt-input/attachments.ts:207`); project icon drop (`dialog-edit-project-v2.tsx:54`).
- Paste: prompt (`prompt-input/attachments.ts:94`), terminal (`terminal.tsx:97-125`), desktop clipboard image via `platform.readClipboardImage` (`platform.tsx:114`).
- OS notifications: `platform.notify(title, desc, onClick)`; web impl uses `Notification` API, suppressed when focused, icon `https://opencode.ai/favicon-96x96-v3.png` (`entry.tsx:58-81`). Triggers: `session.idle` → agent (`context/notification.tsx:346-359,401-405`), `session.error` → errors (`:377-394`), permission asked (`pages/layout.tsx:446-454`).
- Sounds: `SOUND_OPTIONS` (`app/src/utils/sound.ts:13-60`), lazy-loaded `.aac` from `ui/src/assets/audio/*.aac` (`sound.ts:6`); each also ships `.mp3`. Families: `alert-01..10`, `bip-bop-01..10`, `nope-01..12`, `staplebops-01..07`, `yup-01..06` (`ui/src/assets/audio/`). Labels `sound.option.<id-without-dash>` (`sound.ts:14-40`).
- Favicon/manifest: `ui/src/components/favicon.tsx:3-12` (`/favicon-96x96-v3.png`, `/favicon-v3.ico`, `/apple-touch-icon-v3.png`, `/site.webmanifest`); assets `ui/src/assets/favicon/`. No dock badge / unread counter in title found (`grep setBadge|document.title` → none).

### 4.10 Keyboard shortcuts

Model: commands register `{id, title, keybind, slash?, when?}`; combo grammar `mod|ctrl|control|meta|cmd|command|alt|option|shift + key`, comma = alternatives; `mod` = Cmd on macOS else Ctrl (`app/src/context/command.tsx:131-174`); key names normalized `,`→`comma`, `+`→`plus`, space→`space` (`:44-49`). User override = `settings.v3.keybinds[id]` (`:282-287`), edited in Settings → Shortcuts grouped General/Session/Navigation/Model and agent/Terminal/Prompt (`components/settings-keybinds.tsx:24-36,634`). Collisions: first candidate whose `when(event)` passes, else first without `when` (`command.tsx:97-99`). Inside editable targets only palette, `terminal.toggle|terminal.new|file.attach`, modified or Tab combos fire (`command.tsx:16,406`).

| id | Default | Title (en.ts) | Ref |
|---|---|---|---|
| `command.palette` | `mod+k,mod+shift+p` | Command palette (:42) | `context/command.tsx:14` |
| `settings.open` | `mod+comma` | Open settings (:35) | `components/settings-dialog.tsx:37` |
| `common.goBack` / `common.goForward` | `mod+[` / `mod+]` | Navigate back/forward (:237-238) | `components/titlebar.tsx:159,166` |
| `home.toggle` | `mod+b` | Home (:651) | `titlebar.tsx:321` |
| `tab.new` | `mod+t,mod+n` | New session (:52) | `titlebar.tsx:335` |
| `tab.close` | `mod+w` | Close tab (:54) | `titlebar.tsx:343`, `pages/session/use-session-commands.tsx:418` |
| `tab.reopenClosed` | `mod+shift+t` | Reopen closed tab (:55) | `titlebar.tsx:353` |
| `tab.prev` / `tab.next` (hidden) | `mod+option+ArrowLeft,ctrl+shift+tab` / `mod+option+ArrowRight,ctrl+tab` | — | `components/titlebar-tab-strip.tsx:236,244` |
| `tab.1`…`tab.9` (hidden) | `mod+1`…`mod+9` | — | `titlebar-tab-strip.tsx:411` |
| `home.sessions.search.focus` | `mod+f` | (search placeholder) | `pages/home/home-session-search-controller.ts:57` |
| `input.focus` | `ctrl+l` | Focus input (:58) | `use-session-commands.tsx:470`, `pages/new-session/use-new-session-commands.tsx:32` |
| `project.select` | `mod+shift+o` | Search projects (:727) | `use-new-session-commands.tsx:39` |
| `model.choose` | `mod+'` | Choose model (:70) | `pages/session/use-composer-commands.tsx:54` |
| `model.variant.cycle` | `shift+mod+d` | Cycle thinking effort (:78) | `use-composer-commands.tsx:62` |
| `agent.cycle` / `agent.cycle.reverse` | `mod+.` / `shift+mod+.` | Cycle agent (:74,:76) | `use-composer-commands.tsx:69,78` |
| `session.new` | `mod+shift+s` | New session (:52) | `use-session-commands.tsx:341` |
| `session.archive` | `mod+shift+backspace` | Archive session (:40) | `use-session-commands.tsx:394` |
| `file.open` | `mod+p` | Open file (:53) | `use-session-commands.tsx:410` |
| `context.addSelection` | `mod+shift+l` | Add selection to context (:56) | `use-session-commands.tsx:429` |
| `terminal.toggle` | ``ctrl+` `` | Toggle terminal (:59) | `use-session-commands.tsx:439` |
| `review.toggle` | `mod+shift+r` | Toggle review (:61) | `use-session-commands.tsx:454` |
| `fileTree.toggle` | `mod+\` | Toggle file tree (:60) | `use-session-commands.tsx:462` |
| `terminal.close` | `mod+w` | Close terminal (:795) | `use-session-commands.tsx:479` |
| `terminal.new` | `ctrl+alt+t` | New terminal (:62) | `use-session-commands.tsx:488` |
| `message.previous` / `message.next` | `mod+alt+[` / `mod+alt+]` | Previous/Next message (:66,:68) | `use-session-commands.tsx:498,506` |
| `mcp.toggle` | `mod+;` | Toggle MCPs (:72) | `use-session-commands.tsx:517` |
| `permissions.autoaccept` | `mod+shift+a` | Auto-accept permissions (:82) | `use-session-commands.tsx:529` |
| `file.attach` | `mod+u` | Add files (:301) | `components/prompt-input.tsx:450`, `prompt-input-v2.tsx:418` |
| `prompt.mode.shell` / `prompt.mode.normal` | `mod+shift+x` / `mod+shift+e` | Shell / Prompt (:80,:81) | `prompt-input.tsx:442-443`, `prompt-input-v2.tsx:426,434` |
| Legacy-layout only: `sidebar.toggle` `mod+b`, `project.open` `mod+o`, `project.previous/next` `mod+alt+arrowup/down`, `session.previous/next` `alt+arrowup/down`, `session.previous/next.unseen` `shift+alt+arrowup/down`, `workspace.new` `mod+shift+w`, `theme.cycle` `mod+shift+t`, `theme.scheme.cycle` `mod+shift+s` | — | :28-46 | `pages/layout.tsx:904,911,918,925,951,958,965,972,979,1014,1050` |
| Slash-only (no key) | `session.undo` `/undo`, `session.redo` `/redo`, `session.compact` `/compact`, `session.fork` `/fork`, `session.export` `/export` | :86-98 | `use-session-commands.tsx:352-387` |

Hard-coded prompt keys: Enter = submit (`prompt-input.tsx:1378`), Shift+Enter = newline (`:1309`), IME-safe Enter (`:1315`), Escape = close popover → leave shell mode → abort running turn → blur (`:1268-1296`), ArrowUp/Down = history (`:1359-1369`), Tab/Arrows/Enter navigate popovers (`:1322-1337,1405-1416`), Backspace at pos 0 exits shell mode (`:1300`).

Default-combo collisions to resolve in port: `mod+shift+t` (`tab.reopenClosed` vs legacy `theme.cycle`), `mod+shift+s` (`session.new` vs legacy `theme.scheme.cycle`), `mod+b` (`home.toggle` vs legacy `sidebar.toggle`), `mod+w` (`tab.close` vs `terminal.close`, disambiguated by `when`).

### 4.11 i18n

- Library: plain TS dict objects + `@solid-primitives/i18n` (`app/package.json:70`) for app; UI strings via Kobalte `I18nProvider` bridge (`ui/src/context/i18n.tsx:2`). Interpolation `{{name}}` (e.g. `app/src/i18n/en.ts:32`). Plurals: key suffix `.<Intl.PluralRules category>` (`ui/src/context/i18n.tsx:26-39`; e.g. `ui/src/i18n/en.ts:62-63`).
- Files: `app/src/i18n/<locale>.ts` and `ui/src/i18n/<locale>.ts`, merged per locale at load (`app/src/context/language.tsx:52-…`, lazy `import()`); English app dict spreads `DESKTOP_NATIVE_ENGLISH` (`app/src/i18n/en.ts:1-4`, `desktop-native.ts:224-323`). Non-en files type-checked against en keys (`app/src/i18n/de.ts:1-5`), parity test `app/src/i18n/parity.test.ts`.
- Locales (62, `app/src/i18n/desktop-native.ts:1-63`): en zh zht ko de es fr da ja pl ru uk bs ar no br th tr hi nl id vi it ur pa az fi sv am bg bn ca cs dv dz el et fa fo hr hu hy is ka km lo lt lv mk mn ms my ne ro si sk sl sq sr tg tk uz. Labels `desktop-native.ts:68`, BCP-47 tags `:133`. RTL: ar, ur, pa, fa, dv (`language.tsx:23`).
- Key counts (regex `^\s+"key":` line count; approx): `app/src/i18n/en.ts` 1031 + desktop-native 90 + `ui/src/i18n/en.ts` 193 ≈ **1314**.
- Selection/persistence: `props.locale ?? stored ?? navigator.languages` (`language.tsx:170`, detect `:131`, `desktop-native.ts:198`); stored in `opencode.global.dat:language` (`language.tsx:143`) + cookie `oc_locale` 1 year (`language.tsx:38-40`). Web entry also has zh-only pre-detect for the root-error string (`entry.tsx:17-31`).

### 4.12 Feature inventory by priority

| Feature | Source path | Pri | Notes |
|---|---|---|---|
| Server connect + protocol detect + Basic auth | `app/src/utils/server.ts`, `utils/server-protocol.ts`, `context/server.tsx` | P0 | Pick v2 first; keep v1 fallback |
| SSE event loop + reducer | `context/server-sdk.tsx:260-300`, `context/global-sync/event-reducer.ts` | P0 | Coalescing, per-directory routing |
| Bootstrap (config/providers/agents/commands/sessions) | `context/global-sync/bootstrap.ts` | P0 | |
| Home: projects + sessions list | `pages/home.tsx`, `pages/home/*` | P0 | |
| Titlebar tabs (session/draft) | `components/titlebar*.tsx`, `context/tabs.tsx` | P0 | Reorder = P1 |
| Session timeline (virtualized) | `pages/session/timeline/*` | P0 | |
| Text/reasoning/tool part renderers | `session-ui/src/components/message-part.tsx` | P0 | Generic fallback for unknown tools |
| Streaming markdown + code highlight | `session-ui/src/components/markdown*.ts*`, `ui/src/context/marked*.tsx` | P0 | KaTeX = P2 |
| Prompt input (send, abort, multiline, history) | `components/prompt-input-v2.tsx`, `session-ui/src/v2/components/prompt-input/*` | P0 | |
| Model / agent / variant picker | `components/dialog-select-model.tsx`, `pages/session/use-composer-commands.tsx` | P0 | |
| Permission dock | `pages/session/composer/session-permission-dock.tsx` | P0 | once/always/reject |
| Question dock | `composer/session-question-dock.tsx` | P0 | |
| New session / draft + project picker | `pages/new-session*`, `dialog-select-directory-v2.tsx` | P0 | |
| Theme engine (oc-2 light/dark) + tokens | `ui/src/theme/*`, `ui/src/v2/styles/*` | P0 | Other themes P1 |
| Settings: general core rows | `components/settings-v2/general.tsx` | P0 | |
| Provider connect/config | `components/dialog-connect-provider.tsx`, `settings-v2/providers.tsx` | P0 | |
| Error page / toasts | `pages/error.tsx`, `ui/src/components/toast.tsx` | P0 | |
| @-mentions (file/agent), slash menu | `components/prompt-input/slash-popover.tsx` | P1 | |
| Attachments: paste/drop/images/PDF | `components/prompt-input/attachments.ts`, `constants/file-picker.ts` | P1 | |
| Todo dock / todowrite render | `composer/session-todo-dock.tsx`, `message-part.tsx:2525` | P1 | |
| Review panel (diffs, split/unified) | `pages/session/v2/review-panel-v2.tsx`, `session-ui/src/v2/components/session-review-v2.tsx` | P1 | Largest custom widget |
| File tree + file viewer + find | `components/file-tree-v2.tsx`, `session-ui/src/components/file.tsx` | P1 | |
| Terminal panel (PTY over WS) | `components/terminal.tsx`, `context/terminal.tsx` | P1 | |
| Command palette + keybinds + customization | `components/dialog-command-palette-v2.tsx`, `context/command.tsx`, `settings-keybinds.tsx` | P1 | |
| Notifications + sounds | `context/notification.tsx`, `utils/sound.ts` | P1 | |
| Undo/redo/compact/fork/export/archive | `pages/session/use-session-commands.tsx` | P1 | |
| Follow-up queue/steer, revert dock | `composer/session-followup-dock.tsx`, `session-revert-dock.tsx` | P1 | |
| i18n (62 locales) | `app/src/i18n`, `ui/src/i18n` | P1 | en first (P0) |
| Multi-server list, servers tab | `settings-v2/servers.tsx`, `dialog-select-server.tsx` | P1 | |
| MCP toggle dialog | `components/dialog-select-mcp.tsx` | P1 | |
| All 37 themes, font choices | `ui/src/theme/themes/*.json` | P1 | |
| Line comments on diffs → prompt context | `context/comments.tsx`, `session-ui/src/components/line-comment*.tsx` | P2 | |
| Context-usage breakdown tab | `components/session-context-usage.tsx`, `components/session/session-context-*` | P2 | |
| Release notes / highlights | `context/highlights.tsx`, `dialog-release-notes.tsx` | P2 | |
| Open-in-app (Finder/IDE) | `components/session/open-in-app-v2.tsx` | P2 | |
| Deep links `opencode://` | `pages/layout/deep-links.ts` | P2 | |
| WSL/SSH servers | `app/src/wsl/*`, `context/server.tsx:197-217` | P2 | Windows/desktop only |
| KaTeX math | `ui/src/context/marked-parser.tsx:23-64` | P2 | |
| Usage-exceeded / upsell dialogs | `pages/session/usage-exceeded-dialogs.tsx` | P2 | |
| Mobile titlebar / mobile sidebar | `settings.tsx:197`, `context/layout.tsx:296` | P2 | N/A on desktop |
| Legacy layout (sidebar, LegacyHome) | `pages/layout.tsx`, `pages/home/legacy-home.tsx` | — | Retired; do not port |

### 4.13 Rust mapping (suggestions — B-level, verify against pinned gpui-component rev)

- **Widgets** (`ui/src/v2/components/*` → gpui-component): ButtonV2/IconButtonV2/SplitButtonV2 → `Button` (+ dropdown); CheckboxV2/SwitchV2/RadioV2/SegmentedControlV2 → `Checkbox`/`Switch`/`Radio`/button group; TextInputV2/TextareaV2/FieldV2 → `Input` (multi-line); SelectV2/MenuV2 → `Select`/`PopupMenu`/`ContextMenu`; DialogV2 → modal/dialog layer; TooltipV2 → `Tooltip`; ToastV2 → `Notification`; TabsV2 → `TabBar`; AccordionV2 → `Accordion`; resize handles → `Resizable` panels; virtualized lists → `uniform_list`/`list` (GPUI) for timeline/file tree; KeybindV2 → `Kbd`; AvatarV2/ProjectAvatarV2/BadgeV2 → `Avatar`/`Badge`; LoaderV2/ProgressCircleV2 → spinner/progress; TextShimmerV2 → custom animated text.
- **Prompt editor**: custom GPUI `Element` over a rope (contenteditable with inline `@file`/`@agent` pills, `session-ui/src/v2/components/prompt-input/store.ts`, `machine.ts`). gpui-component `Input` lacks inline atoms → expect custom.
- **Markdown**: `comrak` (GFM tables/tasklists/strikethrough/autolinks, closest to marked GFM) or `pulldown-cmark`; port `remend` behavior (close unterminated fences/emphasis during streaming) and the block projection cache (`session-ui/src/components/markdown-stream.ts`, `markdown-projection.ts`) so only the trailing block re-parses.
- **Highlighting**: `syntect` (TextMate grammars, same family as Shiki → closest token fidelity) or `tree-sitter-highlight` (faster incremental, fewer languages). Map scopes to `--syntax-*` tokens using `ui/src/context/marked-theme.tsx` scope table (19 syntax vars referenced).
- **Diff**: `similar` for client-side line diffs; render split/unified with GPUI `uniform_list` (port `session-ui/src/pierre/*` selection/comment logic).
- **Math**: no native KaTeX; P2 fallback = monospace TeX block, or `latex2mathml` + custom layout.
- **Terminal**: `alacritty_terminal` (proven with GPUI in Zed's terminal crate) or `wezterm-term`; WS via `tokio-tungstenite`; implement 0x00 control-frame + cursor resume (§4.8).
- **HTTP/SSE**: `reqwest` + `eventsource-stream`/`reqwest-eventsource`; Basic auth header identical (`utils/server.ts:30`).
- **i18n**: convert TS dicts → JSON once (they are flat `"key": "value"` objects; strip `export const dict =`, resolve `...DESKTOP_NATIVE_ENGLISH`), load with `rust-i18n` or a 30-line custom loader; keep `{{var}}` syntax with a custom formatter; plurals via `intl_pluralrules` + `.one/.other` suffix lookup; RTL via locale set (`language.tsx:23`).
- **Themes**: `ui/src/theme/themes/*.json` load as-is with `serde` (schema `ui/src/theme/desktop-theme.schema.json`); port `ui/src/theme/color.ts` (OKLCH conversions, 12-step `generateScale` `:133-169`) using `palette` crate `Oklch`, then `resolve.ts` + `v2/resolve.ts`; implement a `var(--x)` resolver for override values.
- **Sounds**: `rodio` (use the shipped `.mp3`s to avoid AAC decoder dependency). **OS notifications**: `notify-rust` / platform APIs. **Persistence**: JSON KV files named after the storage names in §4.6 (`opencode.global.dat`, …) to keep key compatibility.

### 4.14 `ui` / `session-ui` component inventory (port checklist)

Exports: `@opencode-ai/ui/*` → `ui/src/components/*.tsx`, `ui/v2/*` → `ui/src/v2/components/*.tsx` (`ui/package.json:37,55`); `@opencode-ai/session-ui/*` → `session-ui/src/components/*.tsx`, `v2/*` (`session-ui/package.json:8-25`). Each component has a sibling `.css` using `data-component`/`data-slot` attributes (e.g. `ui/src/components/toast.tsx:15-54`) — the CSS is the visual spec to replicate.

| Group | v1 (`ui/src/components/`) | v2 (`ui/src/v2/components/`) | gpui-component target (suggestion) |
|---|---|---|---|
| Buttons | `button`, `icon-button` | `button-v2`, `icon-button-v2`, `split-button-v2` | `Button` (+ dropdown) |
| Inputs | `text-field`, `inline-input`, `checkbox`, `switch`, `radio-group`, `select` | `text-input-v2`, `textarea-v2`, `field-v2`, `inline-input-v2`, `checkbox-v2`, `switch-v2`, `radio-v2`, `segmented-control-v2`, `select-v2` | `Input`, `Checkbox`, `Switch`, `Radio`, `Select` |
| Overlays | `dialog`, `popover`, `hover-card`, `tooltip`, `dropdown-menu`, `context-menu`, `toast` | `dialog-v2`, `menu-v2`, `tooltip-v2`, `toast-v2` | modal layer, `Popover`, `Tooltip`, `PopupMenu`, `Notification` |
| Structure | `tabs`, `accordion`, `collapsible`, `sticky-accordion-header`, `card`, `list`, `scroll-view`, `resize-handle`, `dock-surface` | `tabs-v2`, `tab-state-indicator`, `accordion-v2`, `divider-v2` | `TabBar`, `Accordion`, `Resizable`, scrollable `div` |
| Display | `avatar`, `tag`, `keybind`, `progress`, `progress-circle`, `spinner`, `logo`, `favicon`, `app-icon`, `provider-icon`, `file-icon`, `icon`, `image-preview`, `diff-changes` | `avatar-v2`, `project-avatar-v2`, `badge-v2`, `keybind-v2`, `loader-v2`, `progress-circle-v2`, `diff-changes-v2`, `wordmark-v2`, `icon` | `Avatar`, `Badge`, `Kbd`, `Progress`, spinner, `svg()` |
| Text effects | `text-shimmer`, `text-reveal`, `text-strikethrough`, `typewriter`, `animated-number`, `motion-spring` | `text-shimmer-v2` | custom animation (GPUI `with_animation`) |
| Session (session-ui) | `message-part`, `session-turn`, `session-review`, `file`, `file-media`, `file-search`, `markdown`, `basic-tool`, `tool-*`, `line-comment*`, `message-nav`, `dock-prompt`, `session-retry` | `prompt-input/*`, `session-review-v2`, `session-file-panel-v2`, `basic-tool-v2`, `attachment-card-v2`, `comment-card-v2`, `session-progress-indicator-v2`, `tool-error-card-v2` | custom GPUI elements |

## 5. Shared design tokens

### 5.1 Sources

| What | Path | Notes |
|---|---|---|
| Theme JSON (37) | `ui/src/theme/themes/*.json` (98,669 bytes total) | Loaded via `import.meta.glob` (`ui/src/theme/context.tsx:26-30`) |
| Theme schema / types | `ui/src/theme/desktop-theme.schema.json`, `ui/src/theme/types.ts:9-50` | `DesktopTheme {id,name,light,dark}`; variant = `palette` or `seeds` + `overrides` + `v2Overrides` |
| v1 token resolver | `ui/src/theme/resolve.ts:4` (`resolveThemeVariant`), `:536` (`themeToCss`) | 245 distinct token names: icon 59, surface 56, border 38, text 32, syntax 19, markdown 14, avatar 12, input 6, button 5, background 4 (grep of quoted names) |
| Color math | `ui/src/theme/color.ts` (OKLCH `:49-131`, `generateScale` `:133`, `generateNeutralScale` `:171`, `generateAlphaScale` `:218`, `withAlpha` `:296`) | |
| v2 token resolver | `ui/src/theme/v2/resolve.ts:109-153`, `v2/mapping.ts:164` (semantics), `v2/default-primitives.ts`, `v2/avatar.ts` | Agent colors `v2-agent-{plan,build,explore,review,writer}-*` (`v2/mapping.ts:6-25`) |
| v1 static fallback CSS | `ui/src/styles/theme.css` (631 lines; 586 vars; dark block `@media (prefers-color-scheme: dark)` `:360`) | typography/radii/shadows `:1-90` |
| v1 colors (Tailwind) | `ui/src/styles/colors.css` (772), `ui/src/styles/tailwind/colors.css` (286), `tailwind/index.css:38-63` | |
| v2 primitives | `ui/src/v2/styles/colors.css` (147 vars: `--v2-grey-50..1200`, `--v2-{red,orange,yellow,green,blue,purple,pink}-100..1200`, `--v2-alpha-dark-N`, `--v2-alpha-light-N`) | |
| v2 semantics | `ui/src/v2/styles/theme.css` (347 vars; light `:root` `:14`, `[data-color-scheme="light"]` `:261`, `[data-color-scheme="dark"]` `:383`) | |
| Typography utilities | `ui/src/styles/utilities.css:46-111` | `.text-12-regular|medium|mono`, `.text-14-regular|medium|mono`, `.text-16-medium`, `.text-20-medium` |

### 5.2 Theme pipeline

1. Read theme id (`opencode-theme-id`, default `oc-2`, `oc-1` → `oc-2` `context.tsx:87-89`) and scheme (`system` → `prefers-color-scheme`, `context.tsx:127-130`).
2. Pick `light`/`dark` variant → `resolveThemeVariant(variant,isDark)`: from palette (`neutral, ink, primary, success, warning, error, info, accent?, interactive?, diffAdd?, diffDelete?`, `types.ts:21-32`) build 12-step OKLCH scales (light L steps `0.993…0.27`, dark `0.118…0.984`, chroma multipliers per step, `color.ts:137-155`), map to tokens, apply `overrides`.
3. `resolveThemeVariantV2` → `--v2-*` primitives + semantics, apply `v2Overrides`.
4. Inject `<style id="oc-theme">:root{color-scheme; --text-mix-blend-mode: multiply|plus-lighter; …}</style>`, set `html[data-theme=<id>][data-color-scheme=<mode>]`, page bg `#fafafa` light / `#080808` dark (`context.tsx:132-160`).
5. Overrides may reference other tokens: `"text-diff-add-base": "var(--v2-state-fg-success)"` (`ui/src/theme/themes/oc-2.json:24`). oc-2 also has a CSS patch for non-new-layout body (`ui/src/v2/styles/theme.css:2-12`).

### 5.3 Built-in themes (37; ids = file names; display names `ui/src/theme/context.tsx:46-84`)

oc-2 "OC-2" (default), amoled, aura, ayu, carbonfox, catppuccin, catppuccin-frappe, catppuccin-macchiato, cobalt2, cursor, dracula, everforest, flexoki, github, gruvbox, kanagawa, lucent-orng, material, matrix, mercury, monokai, nightowl "Night Owl", nord, one-dark, onedarkpro, opencode, orng, osaka-jade, palenight, rosepine "Rose Pine", shadesofpurple, solarized, synthwave84 "Synthwave '84", tokyonight, vercel, vesper, zenburn — each at `ui/src/theme/themes/<id>.json`. Theme cycling/preview: `previewTheme`/`cancelPreview` (`context.tsx:327-345`).

### 5.4 Token naming & key values

v1 scheme: `--{category}-{role}[-{state}]`, categories background/surface/text/border/icon/input/button/syntax/markdown/avatar; roles `base|weak|weaker|strong|stronger|interactive|success|warning|critical|info|diff-add|diff-delete`; states `hover|active|selected|focus|disabled`. Static fallback values (`ui/src/styles/theme.css`):

| Token | Light | Dark |
|---|---|---|
| `--background-base` | `#f8f8f8` (:94) | `#101010` (:365) |
| `--background-weak` | `#f3f3f3` (:95) | `#1e1e1e` (:366) |
| `--background-stronger` | `#fcfcfc` (:97) | `#151515` (:368) |
| `--surface-base` | `rgba(0,0,0,0.031)` (:98) | `rgba(255,255,255,0.031)` (:369) |
| `--surface-raised-base` | `rgba(0,0,0,0.031)` (:109) | `rgba(255,255,255,0.059)` (:380) |
| `--text-strong` | `#171717` (:167) | `rgba(255,255,255,0.936)` (:438) |
| `--text-base` | `#6f6f6f` (:164) | `rgba(255,255,255,0.618)` (:435) |
| `--text-weak` | `#8f8f8f` (:165) | `rgba(255,255,255,0.422)` (:436) |
| `--text-interactive-base` | `#034cff` (:172) | `#9dbefe` (:443) |
| `--border-base` | `rgba(0,0,0,0.162)` (:200) | `rgba(255,255,255,0.195)` (:471) |
| `--border-weak-base` | `#e5e5e5` (:206) | `#282828` (:477) |
| `--icon-base` | `#8f8f8f` (:243) | `#7e7e7e` (:514) |
| `--button-primary-base` | `#171717` (:195) | `#ededed` (:466) |
| `--input-base` | `#fcfcfc` (:158) | `#1c1c1c` (:429) |
| `--surface-diff-add-base` | `#e3fae1` (:148) | `#1a2919` (:419) |
| `--surface-diff-delete-base` | `#feefeb` (:153) | `#42120b` (:424) |
| `--syntax-string` | `#006656` (:315) | `#00ceb9` (:586) |

v2 scheme: `--v2-{group}-{group}-{role}` (doubled group prefix is literal), groups `background-bg`, `text-text`, `icon-icon`, `border-border`, `state-{bg,fg,border}-{success,warning,danger,info}`, `overlay-simple-*`, `elevation-*`, `illustration-*`, `agent-*`, `avatar-*`.

| v2 token | Light (`v2/styles/theme.css`) | Dark (`:383+`) |
|---|---|---|
| `--v2-background-bg-base` | `grey-50` `#ffffff` (:18) | `grey-1100` `#161616` (:386) |
| `--v2-background-bg-deep` | `grey-100` `#fafafa` (:19) | `grey-1200` `#080808` (:387) |
| `--v2-background-bg-layer-01..04` | `grey-100/200/300/400` (:20-23) | `grey-1000/900/800/700` (:388-391) |
| `--v2-background-bg-accent` | `blue-600` `#3b5cf6` (:27) | `blue-600` (:395) |
| `--v2-text-text-base` | `grey-1100` (:30) | `grey-100` (:397) |
| `--v2-text-text-muted` / `-faint` | `grey-700` / `grey-600` (:31-32) | `grey-500` / `grey-600` (:398-399) |
| `--v2-border-border-base` | `alpha-dark-10` `#0000001a` (:49) | `alpha-light-10` `#ffffff1a` (:414) |
| `--v2-border-border-focus` | `blue-500` (:52) | `blue-500` (:417) |
| `--v2-state-fg-success` | `green-800` (:68) | `green-500` (:431) |
| `--v2-state-fg-danger` | `red-800` (:74) | `red-500` (:437) |
| grey ramp 50…1200 | `#ffffff #fafafa #f2f2f2 #eeeeee #dbdbdb #aeaeae #808080 #5c5c5c #3a3a3a #2e2e2e #242424 #161616 #080808` (`v2/styles/colors.css:4-16`) | same primitives |

v2 hue primitives (steps 100…1200; sample 100/300/600/800, `ui/src/v2/styles/colors.css`): red `:61-68` = `#fcecebff #f2bbb7ff #f1484fff #b82d35ff`; orange `:75-82` = `#fdf2edff #ffd8c6ff #ff8648ff #d16427ff`; yellow `:89-96` = `#fefaecff #f7e5b5ff #f6c251ff #cb9f34ff`; green `:103-110` = `#e7f9eaff #b8e9c1ff #49c970ff #198b43ff`; blue `:131-138` = `#ecf1feff #c3d4fdff #3b5cf6ff #2c47c8ff`; purple `:145-152` = `#ebecfeff #b9b8f5ff #7152f4ff #5230c2ff`; pink `:159-166` = `#fdecf3ff #fabcd8ff #f64aabff #c83d8bff`. Alpha ramps `--v2-alpha-dark-{0,2,4,6,8,10,12,14,16,20,24,30,40…100}` = black at N% (`:19-37`), `--v2-alpha-light-*` = white at N% (`:40-58`).

Default theme seeds (`ui/src/theme/themes/oc-2.json`): light palette `neutral #f7f7f7, ink #171311, primary #dcde8d, success #12c905, warning #ffdc17, error #fc533a, info #a753ae, interactive #034cff, diffAdd #9ff29a, diffDelete #fc533a` (`:6-17`); dark palette `neutral #1f1f1f, ink #f1ece8, primary #fab283, success #12c905, warning #fcd53a, error #fc533a, info #edb2f1, interactive #034cff, diffAdd #c8ffc4, diffDelete #fc533a` (`:242-253`); light overrides e.g. `text-strong #171717, text-base #6F6F6F, text-weak #8F8F8F, border-weak-base #DBDBDB` (`:18-26`), dark `text-strong #EDEDED, text-base #A0A0A0` (`:255-256`). oc-2 additionally pins the full v2 grey/hue ramps via `v2Overrides` (`:48+`).

Typography: `--font-size-small 13px`, `base 14px`, `large 16px`, `x-large 20px`; weights 400/500; line-heights 130/150/180/200 %; letter-spacing tight `-0.16`, tightest `-0.32` (`ui/src/styles/theme.css:8-21`). Spacing unit `--spacing: 0.25rem` (`:23`). Radii `xs .125rem, sm .25rem, md .375rem, lg .5rem, xl .625rem` (`:45-49`); new-layout cards use `rounded-[10px]` (`app/src/pages/home.tsx:20`). Shadows `--shadow-xs|md|lg` with `light-dark()` alpha pairs (`theme.css:51-62`) and border-shadows `--shadow-xs-border-*` (`:63-90`); v2 elevations `raised|floating|overlay|button-neutral|button-contrast|elements|switch-off|switch-on` (`v2/styles/theme.css:114-134`).

### 5.5 Fonts

- Files: `ui/src/assets/fonts/Inter.ttf` (variable, `font-weight: 100 900`), `ui/src/assets/fonts/JetBrainsMonoNerdFontMono-Regular.woff2`; `@font-face` in `app/src/index.css:6-18` (families `"Inter"`, `"JetBrainsMono Nerd Font Mono"`).
- v1 UI uses system stacks (`ui/src/styles/theme.css:2-5`); v2 sets `--font-family-text` / `--v2-font-family-sans: "Inter", sans-serif` (`ui/src/v2/styles/theme.css:144-145`). Terminal default family `JetBrainsMono Nerd Font Mono` (`settings.tsx:59`). `ui/src/components/font.tsx` renders nothing (`export const Font = () => null`).

### 5.6 Icons

- UI icons: inline SVG path strings, 20×20 viewBox, `currentColor` stroke, `stroke-linecap="square"` — `ui/src/components/icon.tsx` (96 entries) and v2 `ui/src/v2/components/icon.tsx` (37 entries, 16×16 `viewBox` per icon). Extractable as-is to `.svg` files for GPUI `svg()`.
- Sprite sheets (`vite-plugin-icons-spritesheet`, `ui/vite.config.ts:10-18`): file-type icons `ui/src/assets/icons/file-types/` (1089 SVGs → `ui/src/components/file-icons`), provider logos `ui/src/assets/icons/provider/` (106), app icons `ui/src/assets/icons/app/` (16: android-studio, antigravity, cursor, file-explorer, finder.png, ghostty, iterm2, powershell, sublimetext, terminal.png, …).
- Brand: `ui/src/components/logo.tsx` (`Splash`), `v2/components/wordmark-v2.tsx`, favicons `ui/src/assets/favicon/`.

### Porting risks (§4–5)

- **Dual protocol + vendored client**: app speaks v1 (`/global/*`) and v2 (`/api/*`) with event adaptation (`server-sdk.tsx:276-286`); the app pins a vendored `@opencode-ai/client 1.17.13-v2` tgz (`app/package.json:59`) that may differ from `client/src` — wire format must be confirmed against the shipped server, not the TS source.
- **Computed theming**: tokens are generated at runtime (OKLCH 12-step scales + 245 v1 + ~350 v2 vars + `var()` cross-references in overrides). Approximating instead of porting `color.ts`/`resolve.ts` bit-for-bit will visibly drift on all 36 non-default themes.
- **Streaming markdown performance**: current path uses a worker, `remend`, block projection caching, `@shikijs/stream`, and `morphdom`; naive re-parse/re-highlight per delta in GPUI will stall on long code-heavy turns.
- **Diff/file viewer**: `@pierre/diffs` (virtualized split/unified, shiki worker pool, line selection → comments, find) has no Rust counterpart; it is the largest single custom widget.
- **Rich prompt editor**: contenteditable with inline mention atoms, IME handling (`prompt-input.tsx:1315`), shell mode, history; gpui text input needs a custom element.
- **Keybinding semantics**: same combos bound to different ids with `when` predicates and editable-target suppression rules (`command.tsx:97-99,406`); default collisions listed in §4.10.
- **Layout bifurcation**: many components branch on `newLayoutDesigns` (e.g. `pages/session/session-side-panel.tsx:291-330`); porting from the wrong branch produces legacy visuals.
- **State compatibility**: persisted names embed `checksum()` from `@opencode-ai/core/util/encode` (`persist.ts:351`); migrating existing user state requires replicating that function and the Electron `platform.storage` format.

### UNKNOWN / to verify (§4–5)

1. v2 SSE/PTY paths in the vendored client tgz — `tar -xzf app/vendor/opencode-ai-client-1.17.13-v2.tgz -C <tmp> && grep -rn "/api/event\|/api/pty" <tmp>`.
2. Which `pages/layout.tsx` commands are registered under the new layout — run the app with new layout and dump `useCommand().options` (or open palette).
3. Consumers of `general.autoSave`, `general.showTerminal`, `appearance.fontSize`, `permissions.autoApprove` — `grep -rn "autoSave\|showTerminal\|fontSize\|autoApprove" app/src session-ui/src` (none found outside `settings.tsx`).
4. Full v2 dark semantic set (only samples listed in §5.4) — `sed -n 383,503p ui/src/v2/styles/theme.css`; also the purpose of the second light/dark pair at `:261`/`:292` vs `:14`/`:383`.
5. Electron `platform.storage(name)` on-disk format/location — see Electron section (`desktop/`).

## 6. Cross-cutting porting risks (ranked)

1. **Server runtime for the desktop app.** Electron hosts the server's Node bundle in a `utilityProcess` (§3.4); a native app must either link the Rust server in-process (target end state) or spawn a standalone `opencode serve` with the Basic-auth + `/global/health` contract.
2. **OpenTUI → ratatui.** Layout, keymap engine (layers, modes, timed leader), slots and JS plugin API live in `@opentui/*@0.4.5`, outside the repo (§2.10). Same-key bindings are disambiguated by layer/focus, not uniqueness.
3. **Computed themes.** GUI themes are generated at runtime from seed colours (OKLCH ramps, `ui/src/theme/color.ts`, `resolve.ts`, §5.2); TUI themes are a separate JSON format with 33 assets (§2.6). Two theme systems, both need exact ports.
4. **Streaming markdown + diff viewer.** No Rust/GPUI equivalent of the worker-based streaming markdown pipeline or `@pierre/diffs` (§4.7).
5. **On-disk state continuity.** XDG paths on every OS (§1.4), `<state>/kv.json` + `*.jsonl` (§2.7), `appData/<appId>/opencode.settings`, `*.dat`, `drafts.sqlite` (§3.5). Renames lose user state.
6. **Two protocol generations.** v1 (`/global/*`) and v2 (`/api/*`) clients coexist in the app (§4.1); the V2 preview CLI and daemon (§1.6) differ from the shipping CLI. Decide the target per client before porting.
7. **Updater cut-over** from electron-updater GitHub feeds (§3.6) and CLI self-upgrade methods (`curl,npm,pnpm,bun,brew,choco,scoop`, §1.2).
8. **CLI parity quirks** to preserve or fix deliberately: unused `run --port` / `--interactive`, permission warnings on stdout under `--format json` (§1.3), forced `process.exit()`.

Section-specific UNKNOWN lists: §2.11, §3.14, end of §5.
