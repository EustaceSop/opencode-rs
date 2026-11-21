<div align="center">

# opencode-rs

**A native Rust rewrite of [opencode](https://github.com/anomalyco/opencode): terminal UI, desktop app, CLI and HTTP server.**

English · [繁體中文](README.zh-TW.md)

</div>

> [!NOTE]
> Unofficial community project, not affiliated with or endorsed by the opencode team.

## Overview

opencode-rs reimplements the TypeScript opencode (tracking tag **v1.19.5**) as native binaries that need no Node or
Bun runtime. It runs side by side with an installed official opencode (including 2.2.x) and shares its logins and
configuration. It keeps its own sessions and never touches the original's.

| Interface | Command | Built with |
|---|---|---|
| Terminal UI | `opencode` | ratatui |
| Desktop app | `opencode-desktop` | GPUI |
| CLI | `opencode run`, `models`, `providers`, `mcp`, `export`, `import`, `stats`, … | clap |
| Server / web | `opencode serve`, `opencode web` | axum. Serves the original's HTTP API and embeds its web UI, which also runs on a phone |

## Highlights

- **Runs beside the official app.** It uses the same `auth.json`, `opencode.json[c]`, skills, agents and MCP
  servers. Sessions are stored in its own `opencode-rs.db`; the original's `opencode.db` is never opened.
- **Compatible with opencode 2.x.** It can:
  - read 2.x logins (read-only);
  - import 2.x sessions;
  - accept v2 `permissions` rules and `cli.json` TUI settings.
- **Works with the agents you already use:**
  - `opencode import` brings over MCP servers, agents and skills from Claude Code, Codex, Cursor, Windsurf,
    Grok Build, DeepSeek Harness, `~/.mcp.json` and opencode.
  - It also imports conversations from Claude Code, Codex, Grok Build, DeepSeek Harness and opencode.
  - It reads `AGENTS.md` and `CLAUDE.md` instructions, and skills in `.claude/skills` and `.agents/skills`, both in
    your home folder and in the project.
  - Command names from other agents work too, e.g. `/rewind` → `/undo` and `/resume` → sessions.
  - `opencode acp` speaks the Agent Client Protocol, for editors that support it.
- **Tasks from your phone.** `opencode web` serves the web UI on your network, so you can start and follow tasks
  from a phone browser while the work runs on your PC ([how](#use-it-from-a-phone)).
- **Features ported from v2:**
  - steer a running task;
  - better compaction;
  - background shell jobs;
  - repair of malformed tool input;
  - `/btw`, `/goal`, `/stats` and `/reload`.
- **Claude-style look:**
  - warm *clay* theme;
  - `● Tool(args)` rows with `⎿ result` underneath;
  - animations in the desktop app.

  The original layout is one setting away.
- **Windows first:**
  - picks the shell in this order: Git Bash → PowerShell 7 → portable bash → Windows PowerShell 5.1 → cmd;
  - decodes Big5/GBK console output;
  - keeps the TUI aligned in the classic console (conhost) with CJK code pages.
- **Small touches:**
  - sounds in both the TUI and the desktop app;
  - the desktop app's language follows the system locale.

## Install

### Prebuilt (Windows x64)

Download from [Releases](../../releases):

| File | Contents |
|---|---|
| `opencode-desktop-win-x64.exe` | Installer: desktop app + `opencode.exe` |
| `opencode-windows-x64.zip` | `opencode.exe` only (TUI, CLI, server) |
| `SHA256SUMS` | Checksums of the zips |

**Installer:**
- Per-user; no admin rights needed.
- Installs to `%LOCALAPPDATA%\Programs\opencode-desktop-rs` and adds Start menu entries under *OpenCode*.
- Registers `opencode://` links. A handler that was registered before is restored on uninstall.
- Uninstall from *Settings → Apps* or the Start menu. Sessions and settings are kept.

The binaries are not code-signed. If SmartScreen warns, choose *More info → Run anyway*. To check a download:

```powershell
Get-FileHash .\opencode-windows-x64.zip -Algorithm SHA256   # compare with SHA256SUMS
```

**TUI from a terminal.** The installer leaves `PATH` alone. The executable has the same name as the official CLI
(`opencode.exe`), so give it a command of its own instead of putting it on `PATH` ahead of the original:

```powershell
# add to $PROFILE
function ocr { & "$env:LOCALAPPDATA\Programs\opencode-desktop-rs\opencode.exe" @args }
```

### Build from source

Requirements:
- [rustup](https://rustup.rs). It installs the pinned toolchain (1.99.1) from `rust-toolchain.toml` by itself.
- Git.
- On Windows, Visual Studio Build Tools with the *Desktop development with C++* workload.

The build compiles prompts, themes and other text from the original repository. Check that repository out at
**v1.19.5**, name the folder `opencode-ts`, and put it next to this repository:

```
workspace/
├─ opencode-ts/    github.com/anomalyco/opencode @ v1.19.5
└─ opencode-rs/    this repository
```

```powershell
mkdir workspace; cd workspace
git clone --depth 1 --branch v1.19.5 https://github.com/anomalyco/opencode.git opencode-ts
git clone https://github.com/EustaceSop/opencode-rs.git
cd opencode-rs
cargo build --release --locked -p opencode-cli -p opencode-desktop
```

The binaries land in `target\release\` as `opencode.exe` and `opencode-desktop.exe`.

| Optional | How |
|---|---|
| Web UI for `opencode web` | Build `opencode-ts/packages/app` with Bun (`bun install`, then `bun run build`) before `cargo build`, or set `OPENCODE_WEB_UI_DIR` to a built `dist`. Without it the server has no UI. The prebuilt binaries already include it |
| Release zips | `scripts\package.ps1` writes them and `SHA256SUMS` to `dist\` |
| Installer | `scripts\package-desktop-installer.ps1`. If NSIS isn't installed, it downloads a pinned NSIS into `target\tools` and checks its SHA-256 |
| Linux / macOS | `scripts/package.sh` (untested) |
| Tests | `cargo test --workspace` |

## Quick start

```powershell
opencode                          # TUI in the current directory
opencode run "explain this repo"  # one-shot prompt
opencode serve --port 4096        # HTTP API
opencode providers login          # sign in to a provider (alias: auth)
opencode import                   # MCP servers, agents, skills and chats from other agents
```

Start the desktop app from the Start menu (*OpenCode*), or run `opencode-desktop.exe`. If you're already signed in
with the official opencode, there's nothing to set up: the login is shared.

### Use it from a phone

Run the web server on your PC with a password, listening on the network:

```powershell
$env:OPENCODE_SERVER_PASSWORD = "choose-a-password"
opencode web --hostname 0.0.0.0 --port 4096
```

1. On a phone on the same Wi-Fi, open `http://<PC IP>:4096`. `ipconfig` shows the PC's IPv4 address.
2. Sign in with the user name `opencode` and your password.
3. If Windows asks, allow access on private networks.

Without a password the server warns that it is unsecured, so always set one. To reach the PC from outside your
home network, use a VPN rather than opening the port to the internet.

### TUI settings

TUI settings live in `~/.local/state/opencode/kv.json`. The original ignores these keys:

| Key | Effect |
|---|---|
| `"rs.style": "opencode"` | Original layout instead of the Claude-style one |
| `"rs.theme": "<name>"` | TUI theme (default `clay`) |
| `"rs.sounds": false` | Mute TUI sounds (also `/sounds`) |

## Coexistence

| Shared with the original | Owned by opencode-rs |
|---|---|
| `auth.json`, `mcp-auth.json` | `opencode-rs.db` (sessions) |
| `opencode.json[c]`, `.opencode/`, skills, agents, commands | `snapshot-rs/` (file snapshots) |
| Plugins, the `models.json` cache, logs | `desktop-rs.json` (desktop state), `~/.config/opencode-rs/` (imports) |

`opencode uninstall` removes only `opencode-rs.db`, `snapshot-rs/` and `desktop-rs.json`. Details are in
[docs/REFERENCE.md](docs/REFERENCE.md).

## Status and TODO

Windows 10 x64 is the tested platform. The Linux and macOS build paths exist but are untested. Builds are unsigned
and there is no auto-update.

- [ ] Own desktop icon (it currently uses the original's)
- [ ] Load plugins installed by v2 `plugin add`
- [ ] Use 2.x well-known (URL-keyed) logins
- [ ] List and edit queued prompts; queue or steer from a remote TUI
- [ ] Keep `/goal` across restarts and show the active goal
- [ ] `/btw` and `/stats` over `attach`
- [ ] `/usage`, `/cd`
- [ ] CLI: `upgrade`, `github install|run`, `debug v2`, `debug agent --tool`
- [ ] Web project picker: accept a typed full path
- [ ] First-run import dialog
- [ ] Verify OpenCode Go after `opencode console login`
- [ ] Code signing; verified Linux and macOS builds

Full list: [docs/TODO.md](docs/TODO.md).

## Project layout

```
crates/
  opencode-core       config, paths, auth, agents, permissions, skills
  opencode-storage    SQLite store
  opencode-llm        providers, streaming clients, model catalog
  opencode-tools      bash, read, write, edit, grep, webfetch, …
  opencode-session    agent loop, compaction, subagents
  opencode-server     HTTP API
  opencode-tui        terminal UI
  opencode-desktop    desktop app
  opencode-cli        the `opencode` binary
  …                   acp, client, import, lsp, mcp, plugin
docs/spec/            behaviour specs written against v1.19.5
docs/REFERENCE.md     crates, compatibility contract, command status
scripts/              packaging
```

## Acknowledgements

- **[opencode](https://github.com/anomalyco/opencode)**: the original project. Thanks to its authors for the design
  this rewrite follows.
- **[Claude Code](https://claude.com/claude-code)** by Anthropic: this project was developed with it. Commits written
  with Claude carry a `Co-Authored-By` trailer.

## License

[MIT](LICENSE). This project builds on [opencode](https://github.com/anomalyco/opencode) (MIT, © 2025 opencode). It
includes ported behaviour and text, the desktop sounds and the current icon. Prompts and themes are compiled in from
the original repository at build time.
