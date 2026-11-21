# TODO

## Desktop
- [ ] Own app icon for the desktop build (window, taskbar, installer, Start menu) instead of the official opencode icon.

## Compatibility with an installed opencode 2.2.7 (from the v2 survey, 2026-11-11)
- [x] Logins: read the active rows of the `credential` table in `opencode.db` (read-only) when `auth.json` lacks a provider; warn when they disagree. MCP OAuth too (`mcp-auth.json` first).
- [x] Import from 2.2.7: fold `session_part` (and `blob`/`blob_ref`) into assistant messages; map tool names back (`shell`→`bash`, `subagent`→`task`, `patch`→`apply_patch`).
- [x] Config: convert v2 `permissions` rules to v1 `permission` instead of rejecting the config (also agent `.md` frontmatter).
- [ ] `plugins` written by v2 `plugin add` stay unloaded: v2 plugin modules default-export `{id, effect|setup}`, which the v1 plugin host cannot run.
- [ ] 2.x well-known logins (`credential` rows keyed by URL) lack the env var name 1.x stored and are not used.
- [x] TUI settings: keybinds, leader timeout, scroll speed, mouse and diff view are read from `cli.json` (the newer of it and the global `tui.json` wins).

## Features worth porting from v2
- [x] Steering: a message sent while the agent works redirects the current task (read at the next step boundary; `Delivery::Queue` / TUI `<leader>return` (`prompt_queue`) waits for the run to end; desktop shows a send button beside stop).
- [x] Compaction: 15% (min 16k) headroom, fixed summary sections, incremental updates, last 15k tokens kept verbatim, staged shortening of the summary input.
- [x] Shell `background: true` with a completion notice (`<shell ...>` synthetic message; wakes an idle session; killed on session delete, engine shutdown and app exit, Windows job object for crashes).
- [x] Tool input repair (JSON-in-string, numbers as strings, nulls).
- [x] `/btw`, `/goal`, `/reload`, `/stats`, `/clear`, aliases `/resume` `/continue` `/q` `/thinking` `/effort`.
- [ ] Compaction leftovers: the template nudge retry, the shrink steps after a provider "too long" rejection, background precompute, summarizing the kept tail too (v2 sends the whole conversation for cache reuse; this build sends the history before the tail as text).
- [ ] Steering leftovers: no queued-prompt list/editor (v2 `session.queued_prompts`, steer/queue toggling of pending items); the 1.x HTTP API (remote TUI) has no delivery field, so a queued prompt from a remote TUI is sent as a steer.
- [ ] `/goal` leftovers: goals live in memory only (v2 stores them and pauses those of a restarted process), no 30-minute wait limit, no compaction when the reviewer's request is too large (the goal pauses), no pinned goal display in the TUI/desktop, judge usage not added to session cost.
- [ ] `/btw` and `/stats` with a remote server (`attach`): need server endpoints; `/stats` has no activity calendar.
- [ ] Not ported: `/usage` (OpenCode Console usage limits API), `/cd` (v2 session move/Location infrastructure); the `bash`→`shell` alias of the input repair (tool names differ in this build).

## Earlier open items
- [ ] Web project picker: a typed full path shows "找不到資料夾".
- [ ] Import phase 2: first-run tick dialog in desktop/TUI and a settings entry.
- [ ] CLI stubs: `upgrade`, `github install/run`, `debug v2`, `debug agent --tool`.
- [ ] OpenCode Go: verify after `opencode console login`.
