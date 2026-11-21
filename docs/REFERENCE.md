# opencode-rs: technical reference

Crates, build details, the compatibility contract and command status. For installation see the [README](../README.md).

A Rust port of [opencode](https://github.com/anomalyco/opencode), tracking the TypeScript original at tag
**v1.19.5**. The port follows the specs in `docs/spec/` (written against that tag) and reads the original's
source for behaviour and output text. The workspace version in `Cargo.toml` follows the tracked tag; there is no
published package and no update channel.

## Crates

| Crate | Role |
|---|---|
| `opencode-core` | Paths (XDG layout on every OS), config loading and JSONC updates, `auth.json`, agents, permissions, skills, slash commands, instructions, env flags |
| `opencode-storage` | SQLite store (`opencode-rs.db`): projects, sessions, messages, parts, todos; ids; project identity |
| `opencode-llm` | models.dev catalog cache, provider resolution, request shaping, streaming clients (chat completions, Anthropic messages, OpenAI responses, Gemini, Bedrock), Copilot and ChatGPT token handling |
| `opencode-tools` | Built-in tools: `bash`, `read`, `write`, `edit`, `apply_patch`, `glob`, `grep`, `webfetch`, `todowrite`, `lsp`, `invalid`; ripgrep lookup |
| `opencode-lsp` | Per-directory workspace services: language servers, formatters, snapshots (`snapshot-rs`) |
| `opencode-mcp` | MCP client (stdio, streamable HTTP, SSE), OAuth, `mcp-auth.json` |
| `opencode-plugin` | Optional Bun sidecar that runs the original's JS/TS plugins and custom tool files; npm plugin install into `<cache>/packages` |
| `opencode-session` | The agent loop (prompt, tools, compaction, subagents via `task`, `skill`, `question`), slash commands, revert, fork |
| `opencode-server` | HTTP server speaking the original's v1 API on top of the engine |
| `opencode-tui` | Terminal UI (ratatui) running the engine in-process |
| `opencode-desktop` | Native desktop app (GPUI) |
| `opencode-cli` | The `opencode` binary |

## Build and run

The toolchain is pinned in `rust-toolchain.toml`. The original's checkout must sit next to this workspace as
`../opencode-ts` (prompts and the OpenAPI document are compiled in from it).

```sh
cargo build --release -p opencode-cli -p opencode-desktop
target/release/opencode --help
target/release/opencode                      # terminal UI in the current directory
target/release/opencode run "explain this repository"
target/release/opencode serve --port 4096    # HTTP API
target/release/opencode-desktop
cargo test -p opencode-cli                   # CLI integration tests (temp dirs, scripted provider, no network)
```

`[profile.release]` (workspace `Cargo.toml`): thin LTO, 4 codegen units, debug info stripped, symbol names kept,
`panic = "unwind"` (the async runtime and the TUI rely on unwinding).

Packaging for the current host, into `dist/`:

```sh
powershell -ExecutionPolicy Bypass -File scripts\package.ps1    # Windows
scripts/package.sh                                              # Linux, macOS
```

Both produce the original's CLI release asset names and layout (`docs/spec/07-peripheral-build.md` A.4.2, A.4.3):
`opencode-<linux|darwin|windows>-<x64|arm64>[-baseline][-musl].<tar.gz|zip>` with only `opencode[.exe]` at the
archive root, plus the staged `dist/<name>/bin/` and `package.json` (`name, version, preferUnplugged, os, cpu, libc?`).
x64 is packaged a second time under its `-baseline` name (same binary). The desktop binary is archived as
`opencode-desktop-<win|mac|linux>-<arch>.<zip|tar.gz>`: the name follows the original's `opencode-desktop-${os}-${arch}`
pattern, but it is a bare executable, not an NSIS/dmg/AppImage installer. Nothing is signed or uploaded.
`OPENCODE_VERSION`, when set at build time, becomes the output of `opencode --version` and the package version.

## Compatibility contract

The Rust build is meant to run beside an installed TypeScript opencode, on the same directories
(`$XDG_DATA_HOME/opencode`, `$XDG_CONFIG_HOME/opencode`, `$XDG_CACHE_HOME/opencode`, `$XDG_STATE_HOME/opencode`,
with the `~/.local/share` style fallbacks on every OS).

Shared with the original (same files, same formats):

- `auth.json` (provider credentials) and `mcp-auth.json` (MCP OAuth)
- config: `opencode.json` / `opencode.jsonc`, global and per project, `.opencode/` directories
- skills, slash commands, agents, MCP server definitions
- plugins: run through Bun by `opencode-plugin`, installed where the original installs them (`<cache>/packages`)
- the models cache `<cache>/models.json`
- logs: appended to `<data>/log/opencode.log`

Owned by the Rust build (the original never reads or writes these):

- `<data>/opencode-rs.db` (+ `-wal`, `-shm`): sessions. The original's `opencode.db` is never opened.
- `<data>/snapshot-rs/`: file snapshots
- `<state>/desktop-rs.json`: desktop app state

> **`opencode uninstall` deviates from the original on purpose.** The original deletes the whole data, cache,
> config and state directories. Here those are shared, so `uninstall` removes only the three Rust-owned items above
> (`--keep-data` keeps the database and snapshots, `--keep-config` keeps the desktop state, `--dry-run` only lists).
> It never removes `auth.json`, the config directory, the cache, `opencode.db`, shell rc lines, or the binary, and
> it never calls a package manager.

## Status

Taken from the code in this workspace.

### CLI commands

| Command | Status |
|---|---|
| `opencode [project]` (TUI) | Implemented: `--model`, `--continue`, `--session`, `--prompt`, `--agent`, `--fork`, `--auto`, network flags (server started alongside), `--mini` (core only: no palette, subagent tabs, shell mode or markdown in scrollback) |
| `run` | Implemented (default and `--format json`, `--command`, `--file`, `--title`, `--variant`, `--thinking`, `--auto`, `--fork`, `--dir`, `--attach`) |
| `serve` | Implemented; flags override the global config's `server.*`. `--mdns` is accepted but nothing is advertised |
| `web` | Starts the server with the original web UI embedded (built from `opencode-ts/packages/app`; usable from a phone browser with `--hostname 0.0.0.0` and `OPENCODE_SERVER_PASSWORD`) |
| `session list` / `delete` | Implemented (table with pager on a terminal, `--format json`, `-n`) |
| `export` / `import` | Implemented, including `--sanitize`. Without a terminal `export` takes the latest session instead of showing the picker |
| `stats` | Implemented (`--days`, `--tools`, `--models`, `--project`) |
| `db [query]`, `db path` | Implemented (`--format tsv|json`; no query starts `sqlite3` on `opencode-rs.db`) |
| `models` | Implemented (`--verbose`, `--refresh`) |
| `providers list` / `login` / `logout` (alias `auth`) | Implemented: API keys, well-known URL login, GitHub Copilot device flow, ChatGPT Pro/Plus login (browser callback on port 1455, or headless device code), auth methods of external plugins. The OAuth flows are not covered by tests (they need the real services) |
| `agent list` / `create` | Implemented. `create` asks the model for a JSON object without schema enforcement; front matter strings that are not plain YAML scalars are double-quoted |
| `mcp add` / `list` / `auth` / `logout` / `debug` | Implemented without the interactive pickers. `mcp debug` reports stored OAuth state and one `initialize` probe; it does not run OAuth discovery |
| `plugin <module>` (alias `plug`) | Implemented: Bun install into `<cache>/packages`, then `plugin` in `opencode.json[c]` / `tui.json[c]`. The `plugin` array is rewritten as a whole (comments inside it are lost) |
| `pr <number>` | Implemented (`gh` + `git`), then runs this binary |
| `uninstall` | Implemented with the narrowed scope described above |
| `generate` | Prints the original's OpenAPI document with `x-codeSamples`; formatting is not prettier's |
| `completion [shell]` | Implemented (clap's scripts, not yargs') |
| `debug config`, `paths`, `info`, `skill`, `scrap`, `startup`, `wait`, `agent <name>`, `rg files|search`, `file read|list|search`, `snapshot track|patch|diff`, `lsp diagnostics|symbols|document-symbols` | Implemented. `debug agent` lists built-in tools only |
| `attach`, `acp`, `console *` | Implemented |
| `import` | Scans opencode, Claude Code, Codex, Cursor, Windsurf, Grok Build, DeepSeek Harness (read-only) and imports selected MCP servers, agents and skills into `~/.config/opencode-rs/` and conversations into the Rust DB; `--dry-run`, `--yes`, `--source`, `--category` |
| `setup bash` | Downloads the official portable bash (Git for Windows MinGit) into the Rust cache after consent |
| `upgrade`, `github install`, `github run`, `debug v2`, `debug agent --tool` | Registered with the original's flags; exit 1 with `... is not implemented in the Rust build yet` |

Global behaviour: `-v`/`--version` prints the bare version; `--print-logs`, `--log-level`, `--pure`; `AGENT=1`,
`OPENCODE=1`, `OPENCODE_PID` are set at startup; usage errors exit 1; help goes to stdout in clap's layout, with
the commands in the original's order. Prompts use `inquire`, not `@clack/prompts`; text and password prompts read
one line from stdin when it is piped. Dates are printed in the en-US shapes only.

### Engine

| Area | Status |
|---|---|
| HTTP API | All 128 legacy (v1) routes and 58 of 61 v2 `/api/*` routes are served; none answer "not implemented". PTY over WebSocket, event streams (`/event`, `/global/event`, `/api/event`) and the embedded web UI catch-all are implemented |
| Tools | Default `agent` profile shows the model `Read`, `Write`, `Edit`, `Glob`, `Grep`, `Bash`/`PowerShell`/`Cmd` (named after the real shell), `WebFetch`, `TodoWrite`, `Task` with original descriptions (`crates/opencode-session/prompts/agent/`); internally and in permissions/plugins/API the original names (`bash`, `read`, `edit`, ...) are kept. `"tool_profile": "opencode"` restores the original tools and prompts. Also `apply_patch`, `lsp`, `skill`, `question`, MCP and plugin tools. No `websearch` tool |
| Shell (Windows) | Git Bash → PowerShell 7 → managed portable bash → Windows PowerShell 5.1 (UTF-8 output forced, `&&` hint) → cmd; config `shell` overrides. Output lines that are not UTF-8 are decoded with the console code page (Big5, GBK, …); `GITHUB_TOKEN`/`GH_TOKEN` lifts GitHub's anonymous limit for the bash download |
| Look | Warm "clay" theme is the default in TUI and desktop (other themes unchanged). The TUI keeps its theme choice under `rs.theme` in `kv.json`, leaving the original's `theme` alone. Tool calls draw as `● Name(args)` with the result under `⎿`; `"rs.style": "opencode"` in `kv.json` restores the original layout and rows. CJK text wraps between characters; times are local |
| TUI | In-process, or against a server through `attach` and the network flags (`crates/opencode-cli/src/remote.rs`); `--mini` as listed above |
| Web UI | The original SolidJS app, embedded at build time when `opencode-ts/packages/app/dist` exists |
| Hosted services | Console account login implemented; GitHub agent and self-upgrade not ported |
| Release pipeline | Local packaging scripts only; no signing, npm, Docker, Homebrew or AUR publishing |
