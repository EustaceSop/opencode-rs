# 04 — Session loop, data model, tools, permissions, agents, prompts

Source: opencode tag v1.19.5 (`opencode/package.json:3`, `core/package.json:3` both `"version": "1.19.5"`).
All references are `path:line` relative to `opencode-ts/packages/`. `UNKNOWN` = not determined from source; each carries a way to resolve it.
Out of scope here (other specs): MCP / LSP server management / plugins / snapshot / pty (spec by MCP agent), providers + model catalog + LLM transport (providers spec), SQL DDL (referenced by table name only).

Contents: §0 runtime path · §1 data model, persistence, events · §2 prompt loop · §3 system prompt, instructions, compaction, title · §4 tool framework + file tools (read/write/edit/apply_patch/glob/grep) · §5 bash, task, todowrite, question, skill, webfetch, websearch, lsp, plan_exit, execute · §6 permissions + questions · §7 agents, commands, skills · §8 core v2 runtime · §9 consolidated risks + open items.

## 0. Which runtime v1.19.5 executes

Two session runtimes coexist in the tree. The **legacy runtime (`opencode/src/session/*`, `opencode/src/tool/*`) is the one every shipped client drives**; the core "v2" runtime (`core/src/session/*`, `core/src/tool/*`) is an incomplete parallel implementation exposed only under `/api/session/*`.

| Evidence | Ref |
|---|---|
| Legacy HTTP group `"/session"`; `prompt` = `POST /session/:sessionID/message`, `promptAsync` = `/session/:sessionID/prompt_async`, `command`, `shell`, `abort`, `revert`, `unrevert`, `permissions/:permissionID`, `summarize`, `init` | `opencode/src/server/routes/instance/httpapi/groups/session.ts:29,78-104` |
| Handlers call `SessionPrompt.Service` (`promptSvc.prompt/loop/cancel/shell/command`) | `opencode/src/server/routes/instance/httpapi/handlers/session.ts:52,233,291,316,346` |
| `prompt` returns the final assistant message as JSON; `promptAsync` forks `prompt` into a scope, returns 204, publishes `Session.Event.Error` (NamedError.Unknown with `Cause.pretty`) on failure | `handlers/session.ts:295-330` |
| TUI submits via SDK `session.prompt(...)` / `session.command(...)` (legacy routes) | `tui/src/component/prompt/index.tsx:1083,1095` |
| Legacy tool registry imports only legacy tools from `opencode/src/tool/*` | `opencode/src/tool/registry.ts:4-31` |
| v2 API (`@opencode-ai/server/api`) is also mounted on the same server: `POST /api/session/:sessionID/prompt` etc. | `opencode/src/server/routes/instance/httpapi/server.ts:73,297-300`; `protocol/src/groups/session.ts:109-360` |
| v2 `shell`, `skill`, `compact`, `wait` return `OperationUnavailableError` | `core/src/session.ts:387-424` |
| v2 runner self-describes as a partial slice (no durable status, no retry bound, no doom-loop, no title/summary/compaction-continuation, no snapshots/patches) | `core/src/session/runner/llm.ts:43-91` |
| v2 built-in tool set lacks task, lsp, plan_exit, code mode; edit "fuzzy parity" TODO | `core/src/tool/builtins.ts:18-30` |

Shared pieces the legacy runtime *does* use from core: SQL tables (`core/src/session/sql.ts`), the event store + projector that materialises v1 events into tables (`core/src/session/projector.ts`), `MAX_STEPS_PROMPT` (`core/src/session/runner/max-steps.ts`, imported at `opencode/src/session/prompt.ts:19`), compaction `buildPrompt` (`core/src/session/compaction.ts`, imported at `opencode/src/session/compaction.ts:23`), ripgrep (`core/src/ripgrep.ts`), v1 schemas (`schema/src/v1/*` via `core/src/v1/*`).

Rust mapping (suggestion):
- Port the legacy runtime as the behavioural reference (sections 1-7). Treat v2 (section 8) as a later milestone; keep the event/projection layer shared so both runtimes write the same tables.
- Feature flags that change tool/loop behaviour are read once from env into `RuntimeFlags` (`opencode/src/effect/runtime-flags.ts:16-57`): `OPENCODE_ENABLE_QUESTION_TOOL`, `OPENCODE_ENABLE_EXA`/`OPENCODE_EXPERIMENTAL_EXA`, `OPENCODE_ENABLE_PARALLEL`, `OPENCODE_EXPERIMENTAL_{PLAN_MODE,LSP_TOOL,CODE_MODE,BACKGROUND_SUBAGENTS,EVENT_SYSTEM,REFERENCES,WORKSPACES}` (each also enabled by `OPENCODE_EXPERIMENTAL` unless explicitly set, `:11-14`), `OPENCODE_EXPERIMENTAL_OUTPUT_TOKEN_MAX`, `OPENCODE_EXPERIMENTAL_BASH_DEFAULT_TIMEOUT_MS` (positive int else unset, `:5-9`), `OPENCODE_DISABLE_CLAUDE_CODE{,_PROMPT,_SKILLS}`, `OPENCODE_DISABLE_EXTERNAL_SKILLS`, `OPENCODE_PURE`, `OPENCODE_CLIENT` (default `"cli"`). Mirror as one `RuntimeFlags` struct built at startup.
- No `batch` tool exists in v1.19.5 (no match for `"batch"`/`BatchTool` in `opencode/src` or `core/src`).

## 1. Data model, persistence, events

Source of truth for v1 (legacy, executed) shapes: `schema/src/v1/session.ts` (re-exported by `schema/src/session-v1.ts:1` and `core/src/v1/session.ts:7-46`). Legacy runtime re-uses these types (`opencode/src/session/message-v2.ts:4-17`, `opencode/src/session/session.ts:224-244`). v2 (`schema/src/session-message.ts`, `schema/src/session-event.ts`) is only noted in §1.10.

Persistence model (critical): the legacy runtime **never writes message/part/session rows directly**. `Session.updateMessage/updatePart/removeMessage/removePart/patch/createNext/remove` only `publish` events (`opencode/src/session/session.ts:535,622,631,637,746,857,869`). Durable events are committed to the `event` table and **projected** into `session`/`message`/`part` rows by `core/src/session/projector.ts` inside the same SQLite transaction (`core/src/event.ts:237-353`). Only `todo` is written directly (`opencode/src/session/todo.ts:100-119`).

### 1.1 Identifiers

| Kind | Prefix | Direction | Generator | Ref |
|---|---|---|---|---|
| Session | `ses_` | descending | `schema/src/identifier.ts` `descending()` | `schema/src/session-id.ts:5-14` |
| Message | `msg_` | ascending | legacy `Identifier.ascending("message")` | `opencode/src/session/schema.ts:10-15` |
| Part | `prt_` | ascending | legacy `Identifier.ascending("part")` | `opencode/src/session/schema.ts:19-24` |
| Event | `evt_` | ascending | `schema/src/identifier.ts` | `schema/src/event.ts:9-12` |
| GlobalBus fallback event id | `evt_` | ascending | `Identifier.create("evt","ascending")` | `opencode/src/bus/global.ts:15-17` |
| Other legacy prefixes | `job`,`per`,`que`,`pty`,`tool`,`wrk` | – | `opencode/src/id/id.ts:3-14` | |

Algorithm (identical in `opencode/src/id/id.ts:51-70` and `schema/src/identifier.ts:14-30`; each module keeps its **own** `lastTimestamp`/`counter` globals):
1. `ts = Date.now()`; if `ts != lastTimestamp` → `lastTimestamp = ts; counter = 0`; then `counter++` (first id in a ms has counter 1).
2. `v = BigInt(ts) * 0x1000 + counter`; descending → `v = ~v` (BigInt two's-complement NOT, i.e. `-(v+1)`).
3. Time field = 6 bytes, byte `i` = `(v >> (40 - 8*i)) & 0xff`, i=0..5, lowercase hex, zero-padded → 12 hex chars. **Only the low 48 bits of `v` are kept**; `ts*4096` exceeds 2^48 today, so the high bits are silently dropped (ordering wraps every 2^48/4096 ms ≈ 795 days). No guard for counter > 0xFFF.
4. Random suffix: 14 bytes from CSPRNG, each `chars[byte % 62]`, `chars = "0-9A-Za-z"` (`opencode/src/id/id.ts:41-49`).
5. Result: `prefix + "_" + hex12 + rand14` (26-char body). If caller supplies an id it must start with the prefix (else throw, `opencode/src/id/id.ts:35-37`) and is used verbatim.
- `timestamp(id)` = `BigInt("0x"+hex12) / 0x1000` (ascending only, `opencode/src/id/id.ts:73-78`).
- Session `slug` = `"<adj>-<noun>"` random from fixed 30/30 word lists (`core/src/util/slug.ts:1-74`).

Rust mapping: use `u128`/`i128` math then `& 0xFFFF_FFFF_FFFF`; descending = `(!v) & mask` on two's complement. Keep two independent monotonic states (legacy vs schema generator) or one shared — output format is identical; shared state only changes counters. Sorting relies on lexical order of ids (parts `ORDER BY id`).

### 1.2 Session.Info (`schema/src/v1/session.ts:543-568`, mirror `opencode/src/session/session.ts:224-244`)

| Field | Type | Notes / column (`core/src/session/sql.ts:22-66`) |
|---|---|---|
| `id` | SessionID | `id` PK |
| `slug` | string | `slug` |
| `projectID` | string | `project_id` (FK project, cascade) |
| `workspaceID?` | string | `workspace_id` |
| `directory` | string | `directory` (directoryColumn: on win32 `\`→`/` on write, `/`→`\` on read for drive paths; empty allowed; `core/src/database/path.ts:45-59`) |
| `path?` | string | `path` = `relative(worktree, cwd)` with `\`→`/` (`opencode/src/session/session.ts:171-173`) |
| `parentID?` | SessionID | `parent_id` |
| `summary?` | `{additions,deletions,files:number, diffs?: FileDiff[]}` | `summary_additions/_deletions/_files` (int), `summary_diffs` (JSON). Read: present iff any of the three is non-null (`session.ts:60-68`) |
| `cost?` | number | `cost` REAL default 0 (aggregated, §1.7) |
| `tokens?` | `{input,output,reasoning,cache:{read,write}}` | `tokens_input/_output/_reasoning/_cache_read/_cache_write` int default 0 |
| `share?` | `{url}` | `share_url` |
| `title` | string | `title`. Default `"New session - "` / `"Child session - "` (child if parentID) + `new Date().toISOString()` (`session.ts:48-55,521`); `isDefaultTitle` regex `session.ts:51-55` |
| `agent?` | string | `agent` |
| `model?` | `{id, providerID, variant?}` | `model` JSON |
| `version` | string | `version` = `InstallationVersion` (`OPENCODE_VERSION` build const or `"local"`, `core/src/installation/version.ts:6`) |
| `metadata?` | Record<string,any> | `metadata` JSON |
| `time` | `{created, updated, compacting?, archived?}` | `time_created`, `time_updated`, `time_compacting`, `time_archived` (archived: any finite number, negatives allowed `session.ts:198-200`) |
| `permission?` | PermissionV1.Ruleset | `permission` JSON |
| `revert?` | `{messageID, partID?, snapshot?, diff?}` | `revert` JSON (`null` when absent; `session.ts:145-152`) |

FileDiff (`schema/src/file-diff.ts:6-12`): `{file?, patch?, additions, deletions, status?: "added"|"deleted"|"modified"}`.
`GlobalInfo` = Info + `project: {id,name?,worktree}|null` (`session.ts:247-258`).

Lifecycle writes (all via `session.updated` re-publishing the full merged Info; merge rules `session.ts:734-747`: `time` shallow-merged; `share/summary/revert/permission` = `null` → removed, `undefined` → keep):
- `create` (`session.ts:499-538,667-689`): cost 0, tokens zeros, `time.created = time.updated = Date.now()`, workspaceID default from instance.
- `fork` (`session.ts:691-732`): new session titled `getForkedTitle` (`"<t> (fork #N+1)"` or `"<t> (fork #1)"`, `session.ts:161-169`), metadata deep-cloned, **no parentID**; copies messages strictly before `messageID` (all if absent/not found) with fresh `MessageID.ascending()`/`PartID.ascending()`, remaps assistant `parentID` and compaction `tail_start_id` via id map.
- `touch`, `setTitle` (no `time.updated` bump), `setArchived`, `setMetadata`, `setAgentModel` (time.updated = input.time), `setPermission`, `setRevert`, `clearRevert`, `setSummary`, `setShare`, `setWorkspace` (`session.ts:749-821`).
- `remove` (`session.ts:606-627`): cancel running background jobs whose id / `metadata.sessionId` / `metadata.parentSessionId` match, recursively remove children, publish `session.deleted`, then `events.remove(sessionID)` (deletes `event_sequence` + `event` rows for aggregate, `core/src/event.ts:514-523`). Rows for message/part/todo go via FK cascade.
- `diff()` returns `[]` (stub, `session.ts:823-826`).
- `plan` file path: `<worktree>/.opencode/plans/<created>-<slug>.md` if VCS else `<data>/plans/...` (`session.ts:331-336`).

### 1.3 Messages (`schema/src/v1/session.ts:327-355,453-500`)

`Info = User | Assistant` discriminated by `role`. Persisted in `message` table: `id`, `session_id`, `time_created` (= `info.time.created` at first insert), `time_updated`, `data` JSON = Info **minus `id`,`sessionID`** (`core/src/session/projector.ts:77-82`). Hydration re-adds them (`opencode/src/session/message-v2.ts:80-85`).

**User** (`role:"user"`)
| Field | Type |
|---|---|
| `id`, `sessionID` | MessageID, SessionID |
| `time` | `{created: number>=0}` |
| `format?` | `{type:"text"}` \| `{type:"json_schema", schema: Record<string,any>, retryCount: int (decode default 2)}` (`v1/session.ts:65-79`) |
| `summary?` | `{title?, body?, diffs: FileDiff[]}` |
| `agent` | string |
| `model` | `{providerID, modelID, variant?}` |
| `system?` | string |
| `tools?` | Record<string, boolean> |

Created in `opencode/src/session/prompt.ts:656-670` (fields set: id, role, sessionID, time.created, tools, agent, model{providerID,modelID,variant}, system, format).

**Assistant** (`role:"assistant"`)
| Field | Type |
|---|---|
| `id`, `sessionID` | |
| `time` | `{created, completed?}` |
| `error?` | AssistantError (§1.5) |
| `parentID` | MessageID (the user message it answers) |
| `modelID`, `providerID` | string |
| `mode` | string (legacy; set = agent name, `prompt.ts:1192`) |
| `agent` | string |
| `path` | `{cwd, root}` (instance directory / worktree) |
| `summary?` | boolean (true for compaction summary messages) |
| `cost` | number |
| `tokens` | `{total?, input, output, reasoning, cache:{read,write}}` |
| `structured?` | any (json_schema output) |
| `variant?` | string |
| `finish?` | string (finish reason) |

Initial assistant record `prompt.ts:1187-1201`: cost 0, zero tokens, `time.created=Date.now()`. On interrupt: `error ??= MessageAbortedError{message:"Aborted"}`, `time.completed=Date.now()` (`prompt.ts:1203-1211`).

`WithParts = {info, parts: Part[]}` (`v1/session.ts:493-500`).

### 1.4 Parts (`schema/src/v1/session.ts:81-383`)

All parts have `id: PartID`, `sessionID`, `messageID`, discriminated by `type`. Persisted in `part` table: `id`, `message_id` (FK message cascade), `session_id`, `time_created` (= event `time` at first insert), `time_updated`, `data` JSON = Part **minus `id`,`sessionID`,`messageID`** (`projector.ts:84-87`). Read order: `ORDER BY message_id, id` (`message-v2.ts:107`) / `ORDER BY id` (`message-v2.ts:512`).

| `type` | Fields (beyond base) | Ref |
|---|---|---|
| `text` | `text: string`, `synthetic?: bool`, `ignored?: bool`, `time?: {start:int, end?:int}`, `metadata?: Record` | `:102-116` |
| `reasoning` | `text`, `metadata?`, `time: {start, end?}` (required) | `:118-128` |
| `file` | `mime`, `filename?`, `url`, `source?: FilePartSource` | `:171-179` |
| `tool` | `callID`, `tool` (name), `state: ToolState`, `metadata?: Record` (e.g. `providerExecuted`) | `:315-325` |
| `step-start` | `snapshot?: string` | `:233-238` |
| `step-finish` | `reason: string`, `snapshot?`, `cost: number`, `tokens: {total?, input, output, reasoning, cache:{read,write}}` | `:240-257` |
| `snapshot` | `snapshot: string` | `:87-92` |
| `patch` | `hash: string`, `files: string[]` | `:94-100` |
| `agent` | `name`, `source?: {value, start:int, end:int}` | `:181-193` |
| `subtask` | `prompt`, `description`, `agent`, `model?: {providerID, modelID}`, `command?` | `:204-218` |
| `retry` | `attempt: int`, `error: APIError` (§1.5), `time: {created}` | `:220-231` |
| `compaction` | `auto: bool`, `overflow?: bool`, `tail_start_id?: MessageID` | `:195-202` |

FilePartSource (discriminator `type`, `:130-169`): common `text: {value, start, end}`; `file` → `path`; `symbol` → `path`, `range: {start:{line,character}, end:{line,character}}`, `name`, `kind:int`; `resource` → `clientName`, `uri`.

ToolState (discriminator `status`, `:259-313`):
| status | Fields |
|---|---|
| `pending` | `input: Record`, `raw: string` (raw streamed JSON) |
| `running` | `input`, `title?`, `metadata?`, `time: {start}` |
| `completed` | `input`, `output: string`, `title: string`, `metadata: Record`, `time: {start, end, compacted?}`, `attachments?: FilePart[]` |
| `error` | `input`, `error: string`, `metadata?`, `time: {start, end}` |

Input variants for prompt API (`:397-451`): `TextPartInput`, `FilePartInput`, `AgentPartInput`, `SubtaskPartInput` = same fields minus base, `id?: PartID` optional.

Rust mapping: `#[serde(tag="type")] enum Part` with `rename = "step-start"` etc.; `#[serde(tag="status")] enum ToolState`; keep `Option` fields as `skip_serializing_if = "Option::is_none"` (schema `optional` drops `undefined` on encode, `schema/src/schema.ts:12-18`). Note snake-case `tail_start_id` while all else camelCase. `metadata`/`input` must be `serde_json::Map` preserving key order (use `preserve_order`).

### 1.5 Errors (`{name, data}` objects; `core/src/util/error.ts:25-66`, `schema/src/v1/session.ts:29-63,385-394`)

AssistantError union (discriminator `name`):
| name | data |
|---|---|
| `ProviderAuthError` | `providerID, message` |
| `UnknownError` | `message, ref?` |
| `MessageOutputLengthError` | `{}` |
| `MessageAbortedError` | `message` |
| `StructuredOutputError` | `message, retries:int` |
| `ContextOverflowError` | `message, responseBody?` |
| `ContentFilterError` | `message` |
| `APIError` | `message, statusCode?, isRetryable: bool, responseHeaders?: Record<string,string>, responseBody?, metadata?: Record<string,string>` |

`MessageV2.fromError` mapping (`opencode/src/session/message-v2.ts:619-747`), in order: DOM `AbortError` → Aborted; OutputLengthError passthrough; `LoadAPIKeyError` → ProviderAuthError; `code=="ECONNRESET"` → APIError("Connection reset by server", retryable, metadata{code,syscall,message}); `ZlibError` → Aborted if `ctx.aborted` else APIError("Response decompression failed", retryable); `HeaderTimeoutError` → APIError(retryable, metadata{code,timeoutMs}); `ResponseStreamError` → APIError(retryable, metadata{code}); `APICallError` → `ProviderError.parseAPICallError` → ContextOverflowError if `type=="context_overflow"` else APIError(parsed fields); generic Error → UnknownError(message); else `ProviderError.parseStreamError` → overflow/APIError, fallback UnknownError(`JSON.stringify(e)`). (`ProviderError` parsing belongs to the providers spec.)
Legacy also defines `MessageError.Shared` (`opencode/src/session/message-error.ts:1-14`). `SessionBusyError{sessionID}` is a tagged API error, not persisted (`session.ts:407-409`).

### 1.6 Session status & todo

SessionStatus (`schema/src/session-status-event.ts:6-30`), **in-memory only** (per-instance `Map`, `opencode/src/session/status.ts:26-48`), default `{type:"idle"}`:
- `{type:"idle"}` | `{type:"busy"}` | `{type:"retry", attempt:int, message:string, next:int (epoch ms), action?: {reason, provider, title, message, label, link?}}`.
- `set()` publishes `session.status`; if idle also publishes deprecated `session.idle` and deletes the map entry.

Todo (`schema/src/session-todo.ts:6-15`): `{content, status, priority}` all strings (status doc: `pending|in_progress|completed|cancelled`; priority `high|medium|low`, not enforced). Table `todo` (`core/src/session/sql.ts:100-117`): PK `(session_id, position)`, columns `content,status,priority,position`. `Todo.update` (`opencode/src/session/todo.ts:99-121`): transaction delete-all-for-session + insert with `position` = array index, then publish `todo.updated {sessionID, todos}` (non-durable). `get` orders by `position`.

### 1.7 Event → table projection (`core/src/session/projector.ts:210-453`)

Durable events (have `durable: {aggregate:"sessionID", version:1}`, `schema/src/v1/session.ts:502-507`) are committed by `commitDurableEvent` (`core/src/event.ts:205-367`) in one `BEGIN IMMEDIATE` transaction:
1. read `event_sequence(aggregate_id=sessionID)` → `latest` (−1 if none); `seq = latest+1`.
2. run all registered projectors for that type (below), then optional `commit(seq)` hook.
3. upsert `event_sequence {aggregate_id, seq, owner_id}`; insert `event {id, aggregate_id, seq, type: "<type>.<version>" e.g. "message.updated.1", data: schema-encoded payload data}` (`core/src/event/sql.ts:4-25`).
4. after commit: wake durable subscribers; then notify in-process listeners (listener errors logged, not propagated for durable events, `core/src/event.ts:398-417`).

| Event (type) | Durable | Projection |
|---|---|---|
| `session.created` `{sessionID, info}` | yes | INSERT session row from `sessionRow(info)` ON CONFLICT DO NOTHING; die `SessionAlreadyProjected` if exists; bump `workspace.time_used` if workspaceID (`:214-233`) |
| `session.updated` `{sessionID, info}` | yes | UPDATE session SET all `sessionRow(info)` columns (incl. cost/tokens as carried in info) (`:234-241`) |
| `session.deleted` `{sessionID, info}` | yes | DELETE session (cascade message/part/todo/...) (`:257-259`) |
| `message.updated` `{sessionID, info}` | yes | INSERT message `{id, session_id, time_created: info.time.created, data}` ON CONFLICT(id) UPDATE SET data only (`:260-273`) |
| `message.removed` `{sessionID, messageID}` | yes | subtract usage of every step-finish part of that message from session; DELETE message (parts cascade) (`:274-292`) |
| `message.part.updated` `{sessionID, part, time}` | yes | read previous row; INSERT part `{id, message_id, session_id, time_created: event.time, data}` ON CONFLICT(id) UPDATE SET data; if previous was step-finish subtract its usage; if new is step-finish add its usage (`:310-328`) |
| `message.part.removed` `{sessionID, messageID, partID}` | yes | subtract usage if step-finish; DELETE part (`:293-309`) |
| `message.part.delta` `{sessionID, messageID, partID, field, delta}` | **no** | none (bus only; `schema/src/v1/session.ts:632-641`) |
| `session.diff` `{sessionID, diff: FileDiff[]}` | no | none |
| `session.error` `{sessionID?, error?: AssistantError}` | no | none |
| `session.status` / `session.idle` | no | none |
| `session.compacted` `{sessionID}` | no | none (`schema/src/session-compaction-event.ts:6-11`) |
| `todo.updated` `{sessionID, todos}` | no | none (table written directly) |
| `command.executed` `{name, sessionID, arguments, messageID}` | no | none (`schema/src/v1/legacy-event.ts:6-14`) |

Usage aggregation (`projector.ts:35-41,89-109`): only parts with `type=="step-finish"` having `cost` and `tokens`; `session.cost += cost*sign`, `tokens_input += tokens.input*sign`, … `tokens_cache_write += tokens.cache.write*sign`; `tokens.total` ignored; `time_updated` explicitly kept unchanged. Hence session cost/tokens are derived, not written by the runtime.
`time_updated` on message/part: column has `$onUpdate(Date.now())` and no default (`core/src/database/schema.sql.ts:3-10`) → set on insert; whether drizzle applies it in `onConflictDoUpdate` is `UNKNOWN` (verify: run an upsert with drizzle-orm version from bun.lock and inspect SQL).

Emit sites (legacy): `session.updated` from every setter; `message.updated` on every assistant state change (creation, finish, error) and user creation; `message.part.updated` on each part transition (text/reasoning start & end, tool pending→running→completed/error, step-start/finish, patch, retry, compaction…); `message.part.delta` for streamed `text`/`reasoning` with `field:"text"` (`opencode/src/session/processor.ts:294-306,512-523`) — the in-memory part accumulates and is persisted by the subsequent `message.part.updated` at `*-end`; `session.diff` (`opencode/src/session/revert.ts:78`, `opencode/src/session/summary.ts:114`); `session.error` (`processor.ts:625-634`, many in `prompt.ts`); `session.compacted` (`opencode/src/session/compaction.ts:554`). `updatePart` publishes a `structuredClone` with `time: Date.now()` (`session.ts:635-643`).

Rust mapping: implement `EventStore::publish(def, data)` that, for durable defs, runs projector closures + inserts `event`/`event_sequence` in one `IMMEDIATE` txn; event `data` must be the schema-**encoded** form (dropped `undefined` keys). Version suffix `.1` in stored type is mandatory for replay (`core/src/event.ts:50-61`).

### 1.8 Event wire format (bus / SSE)

- In-process payload: `{id: evt_…, type, data, durable?: {aggregateID, seq, version}, location?: {directory, workspaceID?, project?}, metadata?}` (`schema/src/event.ts:29-40`). `EventV2Bridge.publish` attaches `location` from the current instance (`opencode/src/event-v2-bridge.ts:19-33`).
- GlobalBus emission per event (`event-v2-bridge.ts:35-62`): `{directory, project, workspace, payload: {id, type, properties: data}}`; if durable, a **second** emission `payload: {type:"sync", syncEvent: {id, type:"<type>.<version>", seq, aggregateID, data}}` (its `id` = syncEvent.id, `opencode/src/bus/global.ts:15-17`).
- Instance SSE `/event` (`opencode/src/server/routes/instance/httpapi/handlers/event.ts:25-80`): first `{id, type:"server.connected", properties:{}}`; then events filtered by `location.directory == instance.directory` and workspace match, mapped to `{id, type, properties}`; heartbeat `server.heartbeat` every 10 s (first tick dropped); terminates after `server.instance.disposed`. SSE `event: message`, `data: JSON`.

### 1.9 Reading & model conversion (`opencode/src/session/message-v2.ts`)

Paging (`:438-480`): `ORDER BY time_created DESC, id DESC LIMIT n+1`; cursor = base64url(JSON `{id, time}`), "older" = `time_created < t OR (time_created = t AND id < id)` (`:95-96`); items reversed to chronological. `Session.messages` loops pages of 50 → full chronological list (`session.ts:828-851`); empty + missing session → NotFound.

`filterCompacted(msgs)` (`:534-585`), input iterated **newest→oldest** (`stream()` yields newest-first, `:482-503`):
1. Walk; push each msg. Assistant with `summary && finish && !error` → add its `parentID` to `completed`.
2. On a user msg in `completed`: find its `compaction` part; none → continue; no `tail_start_id` → stop (break, keep this msg); else set `retain = tail_start_id` (stop immediately if it is this msg) and keep walking until the msg with id `retain` is pushed, then stop.
3. Reverse to chronological. If last user msg with a compaction part having `tail_start_id` exists at `ci`, its summary assistant (first later assistant with `summary` and `parentID == compaction.id`) at `si > ci`, and the tail start at `ti < ci`: return `[ci..=si] ++ [ti..ci) ++ (si..]` (compaction-user, summary, retained tail, rest). Otherwise chronological list.
`latest(msgs)` (`:595-617`): newest user / assistant / finished assistant by `(time.created, id)`; `tasks` = compaction+subtask parts in messages not older-or-equal than the newest finished assistant.

`toModelMessages(input, model, {stripMedia?, toolOutputMaxChars?})` (`:131-428`) builds AI-SDK `UIMessage[]` then calls `convertToModelMessages` (`ai` 6.0.168, root `package.json:67`):
- Messages with zero parts skipped.
- **User**: `text` parts with `!ignored && text!==""` → text (synthetic text **is** included). `file` parts except `text/plain` and `application/x-directory` → `{type:"file", url, mediaType: mime, filename}`; with `stripMedia` and media (`image/*` or `application/pdf`, `opencode/src/util/media.ts:3-9`) → text `[Attached <mime>: <filename|file>]`. `compaction` → text `"What did we do so far?"`. `subtask` → text `"The following tool was executed by the user"`. Other types dropped. Empty → message omitted.
- **Assistant**: skipped entirely if `error` set, **unless** error is `MessageAbortedError` and some part is not `step-start`/`reasoning`. `differentModel` = `"<model.providerID>/<model.id>" != "<info.providerID>/<info.modelID>"`.
  - `text` → text (`""` replaced by `" "` if any reasoning part has `metadata.anthropic.signature`); `providerMetadata: part.metadata` only if same model.
  - `step-start` → `{type:"step-start"}` (AI SDK splits steps on it).
  - `reasoning` → same model: `{type:"reasoning", text, providerMetadata}`; different model: text part if `text.trim()` non-empty.
  - `tool` → part type `"tool-"+name`, `toolCallId: callID`, `input: state.input`, `providerExecuted:true` if `metadata.providerExecuted`, `callProviderMetadata` = `metadata` minus `providerExecuted` (omitted if empty or different model):
    - completed → `state:"output-available"`; output text = `"[Old tool result content cleared]"` if `time.compacted`, else output truncated to `toolOutputMaxChars` with suffix `"\n[Tool output truncated for compaction: omitted N chars]"` (`:49-53`). Attachments dropped if compacted/stripMedia; xAI drops non png/jpeg/webp images. Media attachments the provider can't take in tool results (`supportsMediaInToolResult`, `:147-163`: anthropic/openai/bedrock-mantle/vertex-anthropic all; bedrock images only for anthropic./nova/llama4; xai images; google gemini-3 only) are moved to a synthetic **user** message after the assistant: text `"Attached media from tool result:"` + file parts. Output = string, or `{text, attachments}` when attachments remain; `toModelOutput` → `{type:"text"}` / `{type:"content", value:[text?, media{mediaType, data: base64 after comma}]}` (only `data:` URLs) / `{type:"json"}`.
    - error → if `metadata.interrupted===true` and `metadata.output` is string → `output-available` with that output; else `state:"output-error", errorText: state.error`.
    - pending/running → `output-error` with `errorText:"[Tool execution was interrupted]"`.
  - Other part types (file, patch, snapshot, step-finish, retry, agent…) dropped.
- Finally messages whose parts are all `step-start` are removed before conversion.

Rust mapping: re-implement `convertToModelMessages` semantics of `ai@6.0.168` (step-start splitting into assistant + tool messages, tool-result `output` shapes) — not available in Rust; must be ported from the npm package source (`UNKNOWN` exact splitting rules here: read `node_modules/ai/src/ui/convert-to-model-messages.ts` after `bun install`).

### 1.10 v2 differs (core runtime, not used by `/session/:id/message`)

v2 stores messages in `session_message {id, session_id, type, seq, time_created, time_updated, data}` with per-session monotonic `seq` (unique), queued input in `session_input {id, session_id, prompt, delivery, admitted_seq, promoted_seq, time_created}`, and `session_context_epoch {session_id, baseline, snapshot, baseline_seq}` (`core/src/session/sql.ts:119-176`). Its events are `session.next.*` (e.g. `session.next.prompted`, `session.next.step.started`, `session.next.tool.called`, `session.next.revert.committed`; `schema/src/session-event.ts:55-442`), projected via `SessionMessageUpdater` (`core/src/session/projector.ts:111-208,329-451`). Both share the `session` row and the `event` log; v2 `revert` JSON adds `files` (`projector.ts:394-404`). v2 `Session.Info` uses `location`/`subpath` and DateTime fields (`schema/src/session.ts:19-44`).

### 1.11 Porting risks (section-local)
1. ID truncation to 48 bits + `byte % 62` bias must be byte-identical; ordering of parts/messages depends on it.
2. Persistence is event-sourced: row contents = projector output of the **encoded** event; cost/tokens are sums of step-finish parts with subtraction on update/remove. Skipping the event table breaks `sync` replay (`type.version`, seq per session).
3. `toModelMessages` + AI SDK `convertToModelMessages` behaviour (step splitting, interrupted-tool placeholders, media relocation, cross-model reasoning→text) is the prompt-cache/compat-critical path and has no Rust equivalent.

## 2. Prompt loop

Executed path in v1.19.5: legacy `SessionPrompt` (`opencode/src/session/prompt.ts`), called by the instance HTTP handlers (`opencode/src/server/routes/instance/httpapi/handlers/session.ts:295` prompt, `:311` promptAsync). Core v2 `SessionRunner` is a separate path (see §8). All refs below are relative to `opencode-ts/packages/`.

### 2.1 Entry points

| HTTP handler (`handlers/session.ts`) | Calls | Notes |
|---|---|---|
| `prompt` :295 | `SessionPrompt.prompt` | Blocks until loop ends; response = JSON of returned `WithParts` (last assistant), streamed body `:306-308`. Errors -> 400 |
| `promptAsync` :311 | `prompt` forked in handler scope | Returns 204 immediately; failure -> `session.error` event with `NamedError.Unknown{message: Cause.pretty}` `:316-327` |
| `command` :331 | `SessionPrompt.command` | §2.17 |
| `shell` :341 | `SessionPrompt.shell` | `Session.BusyError` mapped by `SessionError.mapBusy` |
| `init` :237 | `command({command: Command.Default.INIT ("init"), arguments: ""})` | model string `${providerID}/${modelID}` |
| `summarize` :273 | `revert.cleanup` -> `compaction.create({auto: payload.auto ?? false})` -> `loop` | agent = last user msg agent ?? default agent |
| `abort` :232 | `SessionPrompt.cancel` | §2.13 |
| `revert`/`unrevert` :349/:357 | `SessionRevert.*` | Busy -> error; §2.18 |

Interface: `cancel, prompt, loop, shell, command, resolvePromptParts` (`opencode/src/session/prompt.ts:102-109`).

### 2.2 Input schemas (`prompt.ts:1494-1562`)

- `PromptInput` :1499: `sessionID`, `messageID?`, `model? {providerID, modelID}`, `agent?: string`, `noReply?: bool`, `tools?: Record<string,bool>` (annotated `@deprecated`), `format?: SessionV1.Format`, `system?: string`, `variant?: string`, `parts: (TextPartInput|FilePartInput|AgentPartInput|SubtaskPartInput)[]` discriminated by `type`.
- Part inputs (`schema/src/v1/session.ts:397-451`): Text `{id?, type:"text", text, synthetic?, ignored?, time?{start,end?}, metadata?}`; File `{id?, type:"file", mime, filename?, url, source?: FilePartSource}`; Agent `{id?, type:"agent", name, source?{value,start,end}}`; Subtask `{id?, type:"subtask", prompt, description, agent, model?{providerID,modelID}, command?}`.
- `Format` (`schema/src/v1/session.ts:64-78`): `{type:"text"}` | `{type:"json_schema", schema: Record<string,any>, retryCount?: int (decode default 2)}`. `retryCount` has no consumer in `opencode/src` or `core/src` (grep) -> unused by the legacy loop.
- `LoopInput` :1523 `{sessionID}`; `ShellInput` :1527 `{sessionID, messageID?, agent: string (required), model?, command}`; `CommandInput` :1536 `{messageID?, sessionID, agent?, model?: string ("provider/model"), arguments: string, command, variant?, parts?: file-part[]}`.

### 2.3 `prompt()` and user-message creation

`prompt` (`prompt.ts:1052-1071`):
1. `sessions.get` (die if missing) -> `revert.cleanup(session)` (§2.18) :1056.
2. `createUserMessage(input)` :1057 (below) -> `sessions.touch` (patch `time.updated`, `opencode/src/session/session.ts:749`).
3. Legacy `tools` override :1060-1067: each `[tool, enabled]` -> rule `{permission: tool, action: enabled?"allow":"deny", pattern:"*"}`; if non-empty, **replaces** `session.permission` via `sessions.setPermission` (not merged).
4. `noReply === true` -> return the user message (no loop). Else `loop({sessionID})`.

`createUserMessage` (`prompt.ts:635-1050`, `Effect.scoped`):
1. Agent = `agents.get(input.agent)` or `agents.defaultInfo()`; missing -> publish `session.error` (`NamedError.Unknown "Agent not found: \"X\". Available agents: a, b"`, non-hidden agents) and throw :637-644. Same pattern repeated in loop/shell/command.
2. Model = `input.model ?? agent.model ?? currentModel(sessionID)` :646. `currentModel` :614-633: `SessionTable.model` column (`variant` dropped if `"default"`) -> newest user message having `model` (`findMessage`, newest-first, `session.ts:888`) -> `provider.defaultModel()`.
3. Variant :647-654: `input.variant`, else agent.variant only if chosen model == agent.model and the model's `variants[agent.variant]` exists.
4. `info: User = {id: input.messageID ?? MessageID.ascending(), role:"user", sessionID, time.created, tools: input.tools, agent, model{providerID, modelID, variant}, system: input.system, format: input.format}` :656-670.
5. If session's stored agent/model/variant differ -> `sessions.setAgentModel({agent, model{id, providerID, variant ?? "default"}, time})` :672-689.
6. Finalizer `instruction.clear(info.id)` :691 (Instruction service: §3).
7. Resolve each input part concurrently (`concurrency:"unbounded"`, order preserved), flatten, assign `id` (given id or `PartID.ascending()`) :995-997:

| Input | Output parts (all `synthetic:true` text unless noted) | Ref |
|---|---|---|
| file, `source.type=="resource"` (MCP) | `Reading MCP resource: {filename} ({uri})`; per content item: text -> text part; blob: unsupported mime (allowed: pdf,gif,jpeg,png,webp) -> `[Binary MCP resource omitted: {name} ({mime}, {size}) is not a supported attachment type]`; >10 MiB -> `... exceeds 10 MB]`; else `[Binary MCP resource attached: {name} ({mime})]` + file part `data:{mime};base64,{blob}`. Read failure -> `Failed to read MCP resource {filename}: {msg}`. Size = floor(len*3/4)-padding; format B/KB/MB with ceil | :703-784, :65-94 |
| file `data:` + `mime=="text/plain"` | `Called the Read tool with the following input: {"filePath":filename}` + decoded text + original file part | :787-806 |
| file `file:` text/plain | Optional range from URL query `start`/`end`; if `start==end` resolve via `lsp.documentSymbol` (symbol whose `range.start.line == start`) -> `offset=max(start,1)`, `limit=end-(offset-1)`. Runs registered `read` tool with `extra{bypassCwdCheck:true, model}` and no-op `ask`/`metadata`. Parts: `Called the Read tool with the following input: {JSON args}`, tool output text, then attachments (synthetic) or original file part (mime updated). Failure -> `session.error` + `Read tool failed to read {path} with the following error: {msg}` | :808-907 |
| file `file:` directory (`fsys.isDir` -> mime `application/x-directory`) | read tool `{filePath}`; parts: call text, output, file part | :909-947 |
| file `file:` other mime | `Called the Read tool with the following input: {"filePath":"{path}"}` + file part with `url = data:{mime};base64,{bytes}` (keeps `id`, `source`) | :949-969 |
| file other protocol (http etc.) | passthrough | :992 |
| agent | agent part + text `" Use the above message and context to generate a prompt and call the task tool with subagent: {name}"` + (if `Permission.evaluate("task", name, agent.permission)` is deny) `" . Invoked by user; guaranteed to exist."` | :974-990 |
| text / subtask | passthrough | :992 |

8. Plugin `chat.message` (input `{sessionID, agent, model, messageID, variant}`, output `{message, parts}`, mutable) :999-1009.
9. Image file parts -> `image.normalize` (resize; `ResizerUnavailableError` -> keep original) :1011-1020.
10. Schema-validate info/parts; failures are **logged only** :1022-1044.
11. Persist: `updateMessage(info)` then `updatePart` each in order :1046-1047 (emits `message.updated`, `message.part.updated`; §1).

`resolvePromptParts(template)` :157-191 (used by `command`): text part + for each `ConfigMarkdown.files` match (dedup): path `~/x` -> home, else `resolve(worktree, name)`; exists -> file part `{url:file://..., filename:name, mime: dir?"application/x-directory":"text/plain"}`; missing -> agent part if an agent of that name exists.

### 2.4 Run state, queueing, busy

- `loop` = `SessionRunState.ensureRunning(sessionID, onInterrupt=lastAssistant(sessionID), runLoop(sessionID))` (`prompt.ts:1343-1347`). One `Runner` per session per instance (`opencode/src/session/run-state.ts:52-69`), removed and status set `idle` on idle.
- Runner state machine (`opencode/src/effect/runner.ts:33-202`): `Idle | Running{run} | Shell{shell} | ShellThenRun{shell, run}`.

| Call \ State | Idle | Running / ShellThenRun | Shell |
|---|---|---|---|
| `ensureRunning(work)` :115 | fork `work` in instance scope -> Running; caller awaits `done` | **join**: caller awaits existing `done` (new `work` discarded) | queue -> ShellThenRun; run starts when shell finishes (`finishShell` :93-106) |
| `startShell(work)` :140 | `onBusy` (status busy), fork -> Shell | fail `RunnerBusy` -> `Session.BusyError` (`run-state.ts:104`) | same |
| `cancel` :171 | no-op | interrupt fiber, fail `done` with `Cancelled`, `onIdle` | stop shell (await `ready` latch, interrupt) (+fail pending run) |

- Queueing semantics: a second `prompt()` during a run persists its user message immediately (§2.3) then joins the running loop. The running loop re-reads history each iteration (§2.5 step 2) so it picks up the newer user message: exit requires `lastAssistant.parentID === lastUser.id`. All joined callers receive the same final `lastAssistant`.
- Waiters on a cancelled run get `onInterrupt` = `lastAssistant(sessionID)` :1073-1079 (newest non-user message, else newest message) instead of an error (`runner.ts:64-65`).
- Status (`opencode/src/session/status.ts:190-199`): in-memory map; `set` publishes `session.status {sessionID, status}`; for `idle` also publishes deprecated `session.idle {sessionID}` and deletes the entry. `status` union (`schema/src/session-status-event.ts:9-32`): `{type:"idle"}` | `{type:"busy"}` | `{type:"retry", attempt, message, action?{reason,provider,title,message,label,link?}, next (epoch ms)}`.

### 2.5 `runLoop` step-by-step (`prompt.ts:1081-1341`)

State: `step = 0`, `structured: unknown`, `session` loaded once at start (so `session.permission` is a snapshot except when `prompt()` mutated the same object).
Each iteration:
1. `status.set(busy)` :1089.
2. `msgs = MessageV2.filterCompactedEffect(sessionID)` :1092 — full history paged 50 at a time newest-first (`message-v2.ts:482-503`), cut at the latest completed compaction, chronological order; when the compaction part has `tail_start_id`, reordered to `[compaction-user, summary, retained tail..., rest]` (`message-v2.ts:534-585`; compaction itself: §3).
3. `MessageV2.latest(msgs)` (`message-v2.ts:595-611`): newest `user`, newest `assistant`, newest finished assistant (`finish` set) by `(time.created, id)`; `tasks` = all `compaction`/`subtask` parts in messages **after** the last finished assistant. No user -> throw.
4. `hasToolCalls` :1106-1109 = last assistant has any `tool` part that is not `metadata.providerExecuted` and not an orphan (`state.status=="error" && state.metadata.interrupted===true`, :96-100).
5. **Exit** :1111-1130 when `lastAssistant.finish` ∉ {`tool-calls`,`unknown`} AND `!hasToolCalls` AND `lastAssistant.parentID === lastUser.id`.
6. `step++`; if `step===1` fork `title(...)` (title generation, §3) :1132-1139.
7. `model = getModel(lastUser.model)`; not found -> `session.error` `Model not found: p/m. Did you mean: ...?` and die :594-612.
8. `task = tasks.pop()` (last one): `subtask` -> `handleSubtask` (§2.14), `continue`; `compaction` -> `compaction.process({messages, parentID: lastUser.id, sessionID, auto, overflow})`; result `"stop"` -> break, else continue :1142-1159.
9. Pre-step overflow: if `lastFinished && !lastFinished.summary && compaction.isOverflow({tokens: lastFinished.tokens, model})` -> `compaction.create({sessionID, agent, model: lastUser.model, auto:true})`, continue :1161-1168.
10. `agent = agents.get(lastUser.agent)` (error as §2.3). `maxSteps = agent.steps ?? Infinity`; `isLastStep = step >= maxSteps` :1178-1179.
11. `msgs = SessionReminders.apply({messages, agent, session})` :1180 (§3).
12. Create assistant message :1186-1201: `{id: ascending, parentID: lastUser.id, role:"assistant", mode: agent.name, agent: agent.name, variant: lastUser.model.variant, path{cwd: directory, root: worktree}, cost:0, tokens{input:0,output:0,reasoning:0,cache{read:0,write:0}}, modelID: model.id, providerID, time.created, sessionID}` -> `updateMessage`.
13. `processor.create` (takes a snapshot, §2.9) :1213.
14. `bypassAgentCheck` = last user message contains an `agent` part :1222-1223. Tools = `SessionTools.resolve(...)` (§2.6). If `lastUser.format.type=="json_schema"` add `StructuredOutput` tool (§2.15).
15. If `step===1` fork `summary.summarize({sessionID, messageID: lastUser.id})` (diff summary) :1252.
16. Plugin `experimental.chat.messages.transform({}, {messages: msgs})` :1255.
17. In parallel: `sys.skills(agent)`, `sys.environment(model)`, `instruction.system()`, `sys.mcp(agent, session.permission)`, `MessageV2.toModelMessagesEffect(msgs, model)` :1257-1263. `system = [...env, ...instructions, mcp?, skills?]`, + `STRUCTURED_OUTPUT_SYSTEM_PROMPT` (:82) if json_schema :1264-1271 (assembly order and agent/provider prompt prepend: §3 / §2.7).
18. `handle.process({user: lastUser, agent, permission: session.permission, sessionID, parentSessionID: session.parentID, system, messages: [...modelMsgs, ...(isLastStep ? [{role:"assistant", content: MAX_STEPS_PROMPT}] : [])], tools, model, toolChoice: json_schema ? "required" : undefined})` :1272-1286. `MAX_STEPS_PROMPT` is a TS string constant in `core/src/session/runner/max-steps.ts:1-16` (not a .txt). Tools are **not** removed on the last step; only the assistant-prefill text forbids them.
19. Post-process (§2.12), then `instruction.clear(handle.message.id)` (ensuring) :1331.
After `break`: fork `compaction.prune({sessionID})` :1338; return `lastAssistant(sessionID)` :1339.

Step counting: `step` counts every iteration including subtask/compaction iterations; it is per `runLoop` invocation (reset on each new run, not persisted).

### 2.6 Tool resolution per step (`opencode/src/session/tools.ts:41-493`)

1. `registry.tools({modelID: model.api.id, providerID, agent, permission: session.permission})` (`opencode/src/tool/registry.ts:291-336`): builtin + plugin/custom tools; filters: `websearch` only if `webSearchEnabled(providerID, {exa, parallel})`; `usePatch = modelID.includes("gpt-") && !includes("oss") && !includes("gpt-4")` -> `apply_patch` iff usePatch, `edit`/`write` iff !usePatch; `question` only if client ∈ {app,cli,desktop} or `OPENCODE_ENABLE_QUESTION_TOOL` (:207); `lsp` iff experimental LSP tool flag; `plan_exit` iff experimental plan mode && client `cli` (:230-248). Plugin hook `tool.definition` may rewrite description/parameters. Task tool description gets appended agent list (§4/§7).
2. Each tool wrapped as AI-SDK `tool({description, inputSchema: jsonSchema(ProviderTransform.schema(model, ToolJsonSchema.fromTool(item))), execute})` :98-133. `execute`: build `Tool.Context` (:59-90: `sessionID, abort: options.abortSignal, messageID: assistant id, callID: toolCallId, extra{model, bypassAgentCheck, promptOps}, agent: agent.name, messages: msgs, metadata(val)` -> updates part to `running` with `{title, metadata}`; `ask(req)` -> `permission.ask({...req, sessionID, tool{messageID, callID}, ruleset: merge(agent.permission, session.permission ?? [])})` (orDie)) -> plugin `tool.execute.before {tool, sessionID, callID} {args}` -> `item.execute(args, ctx)` -> attachments get `{id: PartID.ascending(), sessionID, messageID}` -> plugin `tool.execute.after` -> if `abortSignal.aborted` persist result directly via `completeToolCall` (AI SDK emits no `tool-result` after abort) -> return output.
3. If any MCP client has `resources` capability: add `list_mcp_resources`, `list_mcp_resource_templates`, `read_mcp_resource` (:27-31, :136-386), each asking permission `read` with patterns `mcp:{server}:*` / `mcp:{server}:{uri}`; output JSON (`JSON.stringify(...,null,2)`) through `truncate.output` (§4).
4. If `experimentalCodeMode` -> return now (MCP tools only reachable through `execute`) :388.
5. MCP tools (`mcp.tools()` keys) :390-490: schema via `ProviderTransform.schema`; execute asks permission `{permission: key, patterns:["*"], always:["*"]}`, flattens `content` (text joined `\n\n`; image -> data-URL attachment; resource text/blob with same mime/size rules), `truncate.output`, returns `{title:"", metadata{...result.metadata, truncated, outputPath?}, output, attachments, content}`.
6. Final per-request filter in `LLMRequestPrep.resolveTools` (`opencode/src/session/llm/request.ts:209-215`): drop tool if `user.tools[k] === false` or in `Permission.disabled(keys, merge(agent.permission, session.permission))` (`opencode/src/permission/index.ts:204-214`: tool maps to permission `edit` for edit/write/apply_patch, `read` for the 3 MCP-resource tools, else its own name; disabled iff the **last** matching rule (Wildcard on permission) has `pattern=="*"` and `action=="deny"`).
7. Provider tweaks (`request.ts:147-174`): `strict:false` on all tools for npm `@ai-sdk/openai|azure|amazon-bedrock/mantle`; github-copilot with zero tools but tool history -> add `_noop` tool. Tools sorted by key (`localeCompare`) :183.

### 2.7 Model call (`opencode/src/session/llm.ts`, `llm/request.ts`)

- `LLM.stream(input)` :356-380: scoped `AbortController` (aborted on scope close/interrupt); `run` :84-354 resolves language model, config, provider, auth in parallel, then `LLMRequestPrep.prepare` (`request.ts:55-207`):
  - System: one string = `[agent.prompt ?? SystemPrompt.provider(model), ...input.system, user.system?].filter(Boolean).join("\n")`; plugin `experimental.chat.system.transform` may push more; if >2 entries and first unchanged -> collapse to `[header, rest.join("\n")]` :57-77. Messages = system entries as `role:"system"` + history, except OpenAI-OAuth (system goes to `providerOptions.instructions`) and GitLab workflow models :98-111.
  - Options: `mergeDeep(base, model.options, agent.options, variants[user.model.variant])`; base = `ProviderTransform.smallOptions` if `small` else `ProviderTransform.options(...)` :79-98.
  - Params via plugin `chat.params`: `temperature = model.capabilities.temperature ? (agent.temperature ?? ProviderTransform.temperature(model)) : undefined`, `topP = agent.topP ?? ProviderTransform.topP`, `topK = ProviderTransform.topK`, `maxOutputTokens = min(model.limit.output, OPENCODE_EXPERIMENTAL_OUTPUT_TOKEN_MAX ?? 32000) || that max` (`opencode/src/provider/transform.ts:18,1481-1483`) :113-131.
  - Headers :180-205: `x-opencode-session-id`, `x-opencode-parent-session-id?`; providerID starting `opencode` -> `x-opencode-project?`, `x-opencode-session`, `x-opencode-request` (= user message id), `x-opencode-client` (OPENCODE_CLIENT, default `cli`), `User-Agent: opencode/{version}`; otherwise `x-session-affinity`, `X-Session-Id`, `User-Agent`; then `x-parent-session-id?`, `model.headers`, plugin `chat.headers` output (later wins).
- `streamText` call `llm.ts:279-352`: `temperature, topP, topK, providerOptions: ProviderTransform.providerOptions(model, options), activeTools: keys except "invalid", tools, toolChoice, maxOutputTokens, abortSignal, headers, maxRetries: input.retries ?? 0` (loop passes none -> 0; title uses 2), `messages`, model wrapped with middleware applying `ProviderTransform.message(prompt, model, options)` on stream params, `includeRawChunks` for github-copilot. No `stopWhen`/`maxSteps` passed -> `assumption: AI SDK default = 1 step per streamText call`; multi-step looping is done by `runLoop`.
- Tool-call repair `experimental_repairToolCall` :295-311: if lower-cased name exists in tools -> retry with that name; else rewrite to tool `invalid` with input `JSON.stringify({tool: originalName, error: error.message})`.
- `OPENCODE_EXPERIMENTAL_NATIVE_LLM` -> try `LLMNativeRuntime.stream` (only openai/opencode/anthropic families with API key, `llm/native-runtime.ts:46-65`), else fall back to AI SDK :225-268. GitLab workflow models get a tool executor + `workflow_tool_approval` permission bridge :118-205 (providers spec).
- AI SDK `fullStream` -> `LLMEvent` adapter (`llm/ai-sdk.ts:77-289`): synthesizes text/reasoning ids (`text-N`, `reasoning-N`) when provider omits them; `finish-step` with `rawFinishReason=="network_error"` -> fails with `ProviderError.ResponseStreamError` (retryable); finish reason normalized to `stop|length|tool-calls|content-filter|error|unknown` (`llm/src/schema/ids.ts:39`); usage mapped to `{inputTokens, outputTokens, totalTokens, reasoningTokens, cacheReadInputTokens, cacheWriteInputTokens}`; `error` event -> stream failure; `abort|source|file|tool-output-denied|tool-approval-request` dropped.

### 2.8 History -> model messages

Re-run every step on the filtered history: `MessageV2.toModelMessagesEffect(msgs, model)` (`opencode/src/session/message-v2.ts:131-428`). Full conversion rules (user/assistant/tool-part mapping, cross-model reasoning, interrupted-tool placeholders, media relocation, AI SDK `convertToModelMessages`) are specified once in §1.9.

### 2.9 Stream handling (`opencode/src/session/processor.ts`)

`create` :98-707: `initialSnapshot = snapshot.track()` before streaming :102. Context: `toolcalls{callID -> {partID, messageID, sessionID, done: Deferred}}`, `shouldBreak`, `snapshot`, `blocked`, `needsCompaction`, `currentText`, `reasoningMap` :67-75.

| LLMEvent | Effect | Ref |
|---|---|---|
| `reasoning-start` | new `reasoning` part `{text:"", time.start, metadata: providerMetadata}` -> `updatePart` (ignored if id known) | :280-292 |
| `reasoning-delta` | append text; `updatePartDelta(field:"text")` (event `message.part.delta` only, **not persisted**, `session.ts:877-885`); orphan deltas dropped | :294-306 |
| `reasoning-end` | set metadata, `time.end`, `updatePart`, forget id | :308-313,:207-214 |
| `tool-input-start/-delta/-end` | `ensureToolCall`: create `tool` part `{tool: name, callID, state{status:"pending", input:{}, raw:""}, metadata{providerExecuted}?}` once; `raw` is never filled. Throws if assistant is a `summary` message | :315-329,:216-253 |
| `tool-call` | ensure part; input = object or `{value: input}`; state -> `running{input, time.start}` (keeps existing running state); metadata = providerMetadata (+`providerExecuted`). Then doom-loop check (§2.10) | :331-381 |
| `tool-result` | error-typed result -> `failToolCall`; else `toolResultOutput` (object with string `output` -> `{title ?? name, metadata, output, attachments (valid FileParts)}`; otherwise title=name, output = string or JSON) -> normalize image attachments (failed resize: append `\n\n[N image(s) omitted: could not be resized below the image size limit.]`) -> `completed{input, output, metadata, title, time{start,end}, attachments}` | :383-414,:257-276 |
| `tool-error` | `failToolCall` -> `error{input, error: errorMessage(e), metadata (kept), time}` | :416-419,:186-205 |
| `provider-error` | throw `Error(message)` -> retry/halt path | :421 |
| `step-start` | `step-start` part `{snapshot}` (track if none) | :424-433 |
| `step-finish` | snapshot.track; close open reasoning; `Session.getUsage` (§ below); `message.finish = reason`; `message.cost += cost`; `message.tokens = tokens` (**replaced**, not summed); `step-finish` part `{reason, snapshot, tokens, cost}`; `updateMessage`; if snapshot: `snapshot.patch` -> `patch` part `{hash, files}` when files non-empty; fork `summary.summarize({messageID: parentID})`; if not summary and `isOverflow` -> `needsCompaction` | :435-498 |
| `text-start` | new `text` part `{text:"", time.start, metadata}` | :500-511 |
| `text-delta` | append; `updatePartDelta` (event only) | :513-524 |
| `text-end` | plugin `experimental.text.complete` may rewrite text; `time.end`; `updatePart` | :526-546 |
| `finish` | ignored | :548 |

- Stream consumed with `Stream.takeUntil(() => ctx.needsCompaction)` :658 (stops after the overflowing step-finish).
- Usage/cost `Session.getUsage` (`opencode/src/session/session.ts:338-405`): non-finite/negative -> 0; `cache.write` = usage.cacheWrite ?? metadata `anthropic|vertex.cacheCreationInputTokens` ?? `bedrock.usage.cacheWriteInputTokens` ?? `venice.usage.cacheCreationInputTokens`; `tokens = {total: usage.totalTokens, input: max(0, input - cacheRead - cacheWrite), output: max(0, output - reasoning), reasoning, cache{write, read}}`; price tier = highest `cost.tiers[]` with `tier.type=="context" && inputTokens > tier.size`, else `experimentalOver200K` if input > 200000, else `model.cost`; `cost = (input*in + output*out + read*cache.read + write*cache.write + reasoning*out) / 1e6` using Decimal; Copilot `metadata.copilot.totalNanoAiu` overrides -> `/1e11`.
- Overflow (`opencode/src/session/overflow.ts:20-32`): false if `compaction.auto===false` or context limit 0; `count = tokens.total || input+output+cache.read+cache.write`; `usable = limit.input ? limit.input - (compaction.reserved ?? min(20000, maxOutputTokens)) : context - maxOutputTokens`; overflow iff `count >= usable` (details §3).
- `cleanup` (ensuring, once per `process`) :553-611: flush pending patch part; close open text/reasoning with `time.end`; wait ≤250 ms per outstanding tool `done`; remaining tool parts -> `status:"error", error:"Tool execution aborted", metadata{...,interrupted:true}`; `message.time.completed = now`; `updateMessage`.

### 2.10 Doom loop and tool errors

- Doom loop (`processor.ts:353-380`): after each `tool-call`, load parts of the assistant message (ordered by part id); if the last `DOOM_LOOP_THRESHOLD = 3` (:29) parts are all `tool` parts with the same tool name, status != pending, and `JSON.stringify(state.input)` identical -> `permission.ask({permission:"doom_loop", patterns:[toolName], sessionID, metadata{tool, input}, always:[toolName], ruleset: agent.permission})`. Default agent rule `doom_loop: "ask"` (`opencode/src/agent/agent.ts:121`). Rejection fails the stream -> halt -> `stop` (derived from code path; not tested here).
- `failToolCall` :200-202: if error is `PermissionV1.RejectedError` (`core/src/v1/permission.ts:7`) or `Question.RejectedError` (`opencode/src/question/index.ts:27`) -> `ctx.blocked = ctx.shouldBreak`; `shouldBreak = config.experimental.continue_loop_on_deny !== true` :647. `blocked` -> `process` returns `stop` (loop ends). `DeniedError` (rule deny) and `CorrectedError` (reject with feedback) do **not** block: the model sees the tool error and the loop continues. `assumption: EffectBridge.promise rejects with the original error instance, so instanceof works`.

### 2.11 Errors, retries, halt

- `process` :641-697: per attempt reset `currentText`, `reasoningMap`, set status busy, run stream. Non-interrupt failures -> `Effect.retry(SessionRetry.policy)` :674-688; final failure -> `halt` :689; `cleanup` after all attempts :690. Parts from a failed attempt remain persisted (new text parts are created on retry; old partial ones keep no `time.end` until cleanup).
- Error mapping `MessageV2.fromError` (`message-v2.ts:619-747`): `AbortError` -> `MessageAbortedError`; `OutputLengthError` passthrough; `LoadAPIKeyError` -> `ProviderAuthError`; `ECONNRESET` -> `APIError{"Connection reset by server", isRetryable:true}`; `ZlibError` -> aborted ? Aborted : `APIError{"Response decompression failed", retryable}`; `HeaderTimeoutError`/`ResponseStreamError` -> retryable `APIError`; `APICallError` -> `ProviderError.parseAPICallError` -> `ContextOverflowError` or `APIError{message,statusCode,isRetryable,responseHeaders,responseBody,metadata}`; other `Error` -> `UnknownError`; else `parseStreamError` or `UnknownError{JSON}`.
- Retryable (`opencode/src/session/retry.ts:85-155`): never `ContextOverflowError`; `APIError` retried if `isRetryable` or `statusCode >= 500` or message/responseBody matches `RETRYABLE_MESSAGE_PATTERNS` (:33-41). Status message: body contains `FreeUsageLimitError` -> `Free usage exceeded, subscribe to Go` + action `{reason:"free_tier_limit", title:"Free limit reached", label:"subscribe", link:"https://opencode.ai/go"}`; `GoUsageLimitError` -> `{limitName} usage limit reached. It will reset in {d/h/m}...` + action `account_rate_limit` link `https://opencode.ai/workspace/{workspace}/go`; message containing `Overloaded` -> `Provider is overloaded`; else error message. Non-API errors: `data.message` lower contains `too_many_requests` -> `Too Many Requests`; `exhausted|unavailable` -> `Provider is overloaded`; pattern match -> message.
- Policy :183-207: stop if not retryable or `attempt > RETRY_MAX_RETRIES (5)`; `assumption: Effect Schedule metadata.attempt starts at 1` -> up to 5 retries. Before sleeping: `status.set({type:"retry", attempt, message, action, next: now + wait})`.
- Delay :47-83: if `APIError.responseHeaders` present: `retry-after-ms` (float) -> `retry-after` seconds (`ceil(s*1000)`) or HTTP-date delta -> else exponential (no 30 s cap); without headers: `min(exponential, 30000)`. `exponential = ceil(base + base*0.25*random)`, `base = 2000 * 2^(attempt-1)`; all capped at 2147483647.
- `halt(e)` :613-639: log; `ContextOverflowError`: if `compaction.auto === false` and not summary -> `message.error`, `finish="error"`, `session.error`, status idle; else `needsCompaction = true` + `session.error` (no message error). Other: `message.error = parsed`, `session.error`, status idle.

### 2.12 Step outcome and termination (`prompt.ts:1288-1336`)

`process` returns `compact` if `needsCompaction`, `stop` if `blocked || message.error`, else `continue` (`processor.ts:693-695`). Then in loop:
1. `structured !== undefined` -> `message.structured = structured`, `finish ??= "stop"`, `updateMessage`, **break**.
2. If `finish` ∉ {tool-calls, unknown} and no error: `content-filter` -> `message.error = ContentFilterError{"The response was blocked by the provider's content filter"}`, `session.error`, break; json_schema format -> `StructuredOutputError{"Model did not produce structured output", retries:0}`, break.
3. `stop` -> break. `compact` -> `compaction.create({auto:true, overflow: !message.finish})` then continue. `continue` -> next iteration (exit test §2.5 step 5 decides).
Termination summary: natural stop (finish not tool-calls/unknown and no tool parts), structured output captured, content filter, error/blocked, compaction `stop`, max-steps (model asked to answer text-only; exits when it complies), cancel.

### 2.13 Abort / cancel

`cancel(sessionID)` -> `SessionRunState.cancel` (`run-state.ts:77-86`): (1) cancel running `BackgroundJob`s whose id == session, or `metadata.sessionId`/`metadata.parentSessionId` in the cancelled set, transitively (:111-143); (2) no runner -> status idle; else `runner.cancel` (fiber interrupt). Effects of interrupt:
- LLM scope closes -> `AbortController.abort()` -> AI SDK + tool `abortSignal`.
- processor `onInterrupt` :662-669: `aborted = true`; `halt(AbortError)` -> `message.error = MessageAbortedError`, `session.error`, status idle. `cleanup`: running/pending tool parts -> error `Tool execution aborted` + `metadata.interrupted=true`; `time.completed` set.
- loop `finalizeInterruptedAssistant` :1203-1211: if not completed, `error ??= MessageAbortedError{aborted:true}`, `time.completed`.
- Subtask: aborts child via `taskAbort`, assistant `finish="tool-calls"`, running task part -> `error "Cancelled"` :360-379.
- Waiters receive `lastAssistant(sessionID)`; status -> idle via runner `onIdle`.

### 2.14 Subtask handling (`prompt.ts:255-449`)

1. Task model = `task.model` (validated by `getModel`) else current model. Create assistant `{parentID: lastUser.id, mode/agent: task.agent, variant: lastUser.model.variant, ...zero usage}`.
2. Create tool part `{tool:"task", callID: ulid(), state: running{input{prompt, description, subagent_type: task.agent, command}, time.start}}`; plugin `tool.execute.before`.
3. Unknown agent -> `session.error` + throw. Execute `TaskTool` directly with `extra{bypassAgentCheck:true, promptOps}`, `ask` using `merge(taskAgent.permission, session.permission)`; failures captured into `error` (logged).
4. plugin `tool.execute.after`; assistant `finish="tool-calls"`, `time.completed`; part -> `completed{title, metadata, output, attachments}` or `error "Tool execution failed: {msg}"`.
5. If `task.command` set: append user message (agent/model of lastUser) with synthetic text `Summarize the task tool output above and continue with your task.`.
Loop then continues (assistant finish `tool-calls` never exits) so the parent agent sees the task result. Task tool internals: §5.

### 2.15 Structured output (`prompt.ts:74-82, 1243-1250, 1565-1591`)

When `lastUser.format.type=="json_schema"`: tool `StructuredOutput` (description constant :74-80; input schema = `format.schema` minus `$schema`; execute stores args, returns `{output:"Structured output captured successfully.", title:"Structured Output", metadata{valid:true}}`, model output = text), system gets `STRUCTURED_OUTPUT_SYSTEM_PROMPT` :82, `toolChoice:"required"`. Result stored in assistant `structured` field.

### 2.16 `shell()` (`prompt.ts:451-592, 1349-1354`)

1. `state.startShell` (busy -> `Session.BusyError`); readiness latch released once messages are written.
2. `revert.cleanup` if reverted; agent required; model = `input.model ?? agent.model ?? currentModel`.
3. Persist user message `{agent, model{providerID, modelID}}` + synthetic text `The following tool was executed by the user`; assistant message (agent/mode = input.agent, zero usage, **no finish**); tool part `{tool:"bash" (ShellID.ToolID, opencode/src/tool/shell/id.ts:16), callID: ulid(), state: running{input{command}}}`.
4. Shell = `Shell.preferred(config.shell)` (`core/src/shell.ts:205-209`: config shell, else `$SHELL`; fallback win32: pwsh, powershell, Git Bash, `%COMSPEC%`/cmd.exe; darwin `/bin/zsh`; else `bash` on PATH or `/bin/sh`). Args `Shell.args(sh, command, cwd)` (`core/src/shell.ts:166-200`): zsh/bash `-l -c <script sourcing rc, cd -- "$1", eval JSON(command)> opencode <cwd>`; nu/fish `-c`; cmd `/c`; PowerShell `-NoProfile -Command`; else `-c`.
5. Spawn: `cwd = instance directory`, env = process env + plugin `shell.env` output + `TERM=dumb`, `stdin: ignore`, `forceKillAfter 3s`; stdout+stderr merged; every chunk appended and persisted as `state.metadata.output` (`updatePart` per chunk).
6. Finish: aborted -> append `\n\n<metadata>\nUser aborted the command\n</metadata>`; part -> `completed{title:"", metadata{output}, output}`; exit code not recorded; **no truncation**. Does not run the loop.

### 2.17 `command()` dispatch (`prompt.ts:1356-1481`)

Template expansion details: §6. Dispatch: unknown command -> `session.error` + throw; agent = `cmd.agent ?? input.agent`; task model = `cmd.model` -> cmd agent's model -> `input.model` -> current; validated via `getModel`. `isSubtask = (agent.mode=="subagent" && cmd.subtask !== false) || cmd.subtask === true` -> single `subtask` part `{agent, description: cmd.description ?? "", command, model, prompt: first text part}` with user agent = `input.agent ?? default` and user model = input/current; else parts = template parts (file parts already in `input.parts` removed) + `input.parts`, agent/model = command's. Plugin `command.execute.before {command, sessionID, arguments} {parts}` -> `prompt({sessionID, messageID, model, agent, parts, variant})` -> publish `command.executed {name, sessionID, arguments, messageID}` (`schema/src/v1/legacy-event.ts:9`).

### 2.18 Revert / unrevert (`opencode/src/session/revert.ts`)

- `revert({sessionID, messageID, partID?})` :38-89: `assertNotBusy` (BusyError). Walk all messages; target = message (no partID) or part. `rev.partID` kept only if earlier parts of the same message include `text`/`tool`; otherwise `rev.messageID` = latest user message at/before target (reverting an assistant reverts to its user message). Collect `patch` parts after target; `rev.snapshot = existing revert snapshot ?? snapshot.track()`; restore previous revert snapshot if any; `snapshot.revert(patches)`; `rev.diff = snapshot.diff(rev.snapshot)`; diffs of messages from `rev.messageID` on -> storage `["session_diff", id]` + event `session.diff`; `sessions.setRevert({revert: rev, summary{additions, deletions, files}})`. Messages are **not deleted** yet.
- `unrevert` :91-99: `assertNotBusy`; restore `revert.snapshot`; `clearRevert`.
- `cleanup(session)` :101-124 (called at start of `prompt`, `shell`, `summarize`): delete messages from `revert.messageID` (exclusive of it when `partID` set), delete parts of the target message from `partID` onward, `clearRevert`.

### 2.19 Rust mapping (suggestions)

- `SessionRunner` per session: `tokio::sync::Mutex<RunnerState>` enum `{Idle, Running{join: Shared<JoinHandle>, cancel: CancellationToken}, Shell{..}, ShellThenRun{..}}`; joined callers share a `futures::Shared` result; cancel resolves waiters with `last_assistant()`.
- Processor as a struct consuming a `Stream<Item = LlmEvent>`; keep part-write ordering identical (DB upsert + `message.part.updated` per write; deltas event-only).
- Run tool executions concurrently inside a step (AI SDK executes on `tool-call`), feed results back as `tool-result` events into the same handler; implement 250 ms grace in cleanup.
- Retry with `tokio::time::sleep` and explicit attempt counter; keep `delay()` math bit-exact (ceil, jitter 0.25, caps).
- Keep string literals byte-identical (synthetic texts, error messages) — they are persisted and replayed to models.

Porting risks: (1) queue/join semantics + history re-read per iteration; (2) persisted-state details on retry/abort (orphan parts, `interrupted` metadata, `tokens` replace vs `cost` accumulate); (3) dependence on AI SDK behaviors (single step per call, tool repair to `invalid`, parallel tool execution, `convertToModelMessages` output) that must be reimplemented natively.

## 3. System prompt, instructions, compaction, title

Primary (executed) path: legacy `opencode/src/session/*`. Core v2 equivalents are summarized in 3.9.
All string literals below are byte-exact quotes from source; joins are `"\n"` unless stated.

### 3.1 Prompt text inventory (embed with `include_str!`)

| File | Selected when | Ref |
|---|---|---|
| `opencode/src/session/prompt/meta.txt` | `model.api.id` contains `"muse"`; `{{MODEL_NAME}}` replaced (replaceAll) by `"Muse Glimmer"` if id contains `"muse-glimmer"`, else `"Muse Spark"` | `opencode/src/session/system.ts:29-32` |
| `opencode/src/session/prompt/beast.txt` | id contains `"gpt-4"` OR `"o1"` OR `"o3"` | `system.ts:33-34` |
| `opencode/src/session/prompt/gpt-astra.txt` | id contains `"gpt"` AND `"gpt-6"` | `system.ts:35-36` |
| `opencode/src/session/prompt/codex.txt` | id contains `"gpt"` AND `"codex"` (not gpt-6) | `system.ts:37-39` |
| `opencode/src/session/prompt/gpt.txt` | id contains `"gpt"` (other) | `system.ts:40` |
| `opencode/src/session/prompt/gemini.txt` | id contains `"gemini-"` | `system.ts:42` |
| `opencode/src/session/prompt/anthropic.txt` | id contains `"claude"` | `system.ts:43` |
| `opencode/src/session/prompt/trinity.txt` | `id.toLowerCase()` contains `"trinity"` | `system.ts:44` |
| `opencode/src/session/prompt/kimi.txt` | lowercase id contains `"kimi"` OR `providerID ∈ {"kimi-for-coding","moonshotai","moonshotai-cn"}` | `system.ts:45-49` |
| `opencode/src/session/prompt/default.txt` | fallback | `system.ts:50` |
| `opencode/src/session/prompt/plan.txt` | plan reminder (legacy plan mode), 3.5 | `reminders.ts:11,27-36` |
| `opencode/src/session/prompt/build-switch.txt` | plan→build reminder, 3.5 | `reminders.ts:12,37-47,61-63` |
| `opencode/src/session/prompt/plan-mode.txt` | experimental plan mode reminder; `${planInfo}` substituted | `reminders.ts:13,81-85` |
| `opencode/src/agent/prompt/compaction.txt` | system prompt of hidden agent `compaction` | `opencode/src/agent/agent.ts:13,219-232` |
| `opencode/src/agent/prompt/title.txt` | system prompt of hidden agent `title` (temperature 0.5) | `agent.ts:16,234-249` |
| `opencode/src/agent/prompt/summary.txt` | system prompt of hidden agent `summary`; **no call site** of `agents.get("summary")` found in `opencode/src` (grep) → effectively unused in v1.19.5 | `agent.ts:15,250-264` |
| `opencode/src/agent/prompt/explore.txt`, `opencode/src/agent/generate.txt` | explore subagent / agent generator (see section on agents) | `agent.ts:12,14,214,380` |

Inline TS constants (not .txt; copy verbatim into Rust consts or extract to .txt):
- `MAX_STEPS_PROMPT` `core/src/session/runner/max-steps.ts:1-16` — appended as an **assistant** message after history when `step >= agent.steps` (`opencode/src/session/prompt.ts:1180-1181,1280-1281`).
- `STRUCTURED_OUTPUT_SYSTEM_PROMPT` `opencode/src/session/prompt.ts:82` — pushed to end of `system` when `user.format.type === "json_schema"` (`prompt.ts:1271`).
- `SUMMARY_TEMPLATE`, `SUMMARY_UPDATE_INSTRUCTIONS` `core/src/session/compaction.ts:16-55`, assembled by `buildPrompt` `:160-174` (used by legacy compaction).

Matching notes: all checks use `model.api.id` (provider-side model id, not `model.id`), case-sensitive except trinity/kimi; order is significant (`"o1"`/`"o3"` substring test precedes gpt/gemini/claude, so any api id containing `o1`/`o3` gets beast.txt). `provider()` returns `string[]` (always length 1).

Rust mapping: `fn provider_prompt(api_id:&str, provider_id:&str)->&'static str` with the exact if-chain order; keep `str::contains` (byte substring) semantics; lowercase only for trinity/kimi.

### 3.2 System message assembly (per loop step)

Step A — `prompt.ts:1257-1271` builds `system: string[]` (fresh every step; nothing cached across steps):
1. `...env` = `SystemPrompt.environment(model)` (1–2 strings, 3.3)
2. `...instructions` = `Instruction.system()` (one string per file/URL, 3.4)
3. `mcpInstructions` if defined (`SystemPrompt.mcp(agent, session.permission)`, 3.3)
4. `skills` if defined (`SystemPrompt.skills(agent)`, 3.3)
5. `STRUCTURED_OUTPUT_SYSTEM_PROMPT` if json_schema format.
Before this, `plugin.trigger("experimental.chat.messages.transform", {}, {messages: msgs})` runs on the message list (`prompt.ts:1255`).

Step B — `opencode/src/session/llm/request.ts:57-77` (`LLMRequestPrep.prepare`):
```
system = [ [ ...(agent.prompt ? [agent.prompt] : SystemPrompt.provider(model)),
             ...input.system,                      // Step A array
             ...(user.system ? [user.system] : []) // PromptInput.system persisted on the user message
           ].filter(truthy).join("\n") ]           // => ONE string
header = system[0]
plugin.trigger("experimental.chat.system.transform", {sessionID, model}, {system})   // may mutate array
if (system.length > 2 && system[0] === header) system = [header, system.slice(1).join("\n")]
```
- `agent.prompt` **replaces** the provider prompt (does not prepend). Built-in `build`/`plan` have no prompt → provider prompt is used.
- Each element becomes a `{role:"system", content}` ModelMessage prepended to history (`request.ts:100-111`), except: OpenAI OAuth (`provider.id==="openai" && auth.type==="oauth"`) → no system messages, `options.instructions = system.join("\n")` (`request.ts:56,98`); workflow models → `workflowModel.systemPrompt = system.join("\n")` (`opencode/src/session/llm.ts:125`).
- Prompt caching: `ProviderTransform.applyCaching` marks the first 2 system messages + last 2 non-system messages (`opencode/src/provider/transform.ts:358-407,483`; provider-specific gating documented in provider spec). This is why the plugin-transform collapse keeps exactly 2 system entries.
- `user.system` comes from `PromptInput.system` (`prompt.ts:1510`) stored on `SessionV1.User.system` (`prompt.ts:667`).

Rust mapping: `fn assemble_system(agent_prompt:Option<&str>, provider:&str, step:&[String], user_system:Option<&str>) -> Vec<String>`; run plugin hook on `Vec<String>`; apply collapse rule; then emit system messages / OAuth instructions.

### 3.3 Environment, references, skills, MCP blocks (`opencode/src/session/system.ts`)

`environment(model)` `system.ts:69-105` returns:
1. Always (lines joined `"\n"`):
```
You are powered by the model named ${model.api.id}. The exact model ID is ${model.providerID}/${model.api.id}
Here is some useful information about the environment you are running in:
<env>
  Working directory: ${ctx.directory}
  Workspace root folder: ${ctx.worktree}
  Is directory a git repo: ${ctx.project.vcs === "git" ? "yes" : "no"}
  Platform: ${process.platform}
  Today's date: ${new Date().toDateString()}
</env>
```
   - `process.platform` values: `win32|darwin|linux|…` (map Rust `windows→win32`, `macos→darwin`).
   - `toDateString()` format: `Www Mmm DD YYYY` local time (e.g. `Tue Nov 10 2026`).
   - `ctx.worktree` is `"/"` for the non-git global project (`opencode/src/project/project.ts:217`).
2. Only if ≥1 reference with `description !== undefined` (`Reference.Service.list()`, `core/src/reference.ts:112-114`), sorted by `name.localeCompare`:
```
Project references provide additional directories that can be accessed when relevant.
<available_references>
  <reference>
    <name>${name}</name>
    <path>${path}</path>
    <description>${description}</description>
  </reference>
</available_references>
```
(no escaping). Sources: config key `references` (deprecated alias `reference`, `core/src/v1/config/config.ts:45-50`) converted by internal plugin `core/config-reference` (`core/src/config/plugin/reference.ts:14-40`); invalid alias names skipped; string-form entries carry no `description` and are therefore not listed. Git-reference materialisation: config/plugin spec.

`skills(agent)` `system.ts:107-119`: returns `undefined` if `Permission.disabled(["skill"], agent.permission)` contains `"skill"` (last matching rule for permission `skill` has `pattern==="*"` and `action==="deny"`; `opencode/src/permission/index.ts:204-214`). Else:
```
Skills provide specialized instructions and workflows for specific tasks.
Use the skill tool to load a skill when a task matches its description.
${Skill.fmt(skill.available(agent), {verbose:true})}
```
`Skill.fmt` verbose (`opencode/src/skill/index.ts:321-338`): only skills with `description !== undefined`; if none → literal `No skills are currently available.`; else `<available_skills>` / per skill `  <skill>`, `    <name>…</name>`, `    <description>…</description>`, `    <location>${escapeHtml(location)}</location>`, `  </skill>` / `</available_skills>`, sorted `localeCompare` by name. `escapeHtml` replaces `& < > " '` → `&amp; &lt; &gt; &quot; &#39;` (`opencode/src/util/html.ts`). Name/description not escaped.

`mcp(agent, sessionPermission)` `system.ts:121-137`: `ruleset = agent.permission ++ sessionPermission`; keep MCP server instructions where `tools.length===0` or not all tools disabled; none → `undefined`; else:
```
<mcp_instructions>
  <server name="${name}">
    ${each line of instructions, prefixed with 4 spaces}
  </server>
</mcp_instructions>
```

Rust mapping: use a locale-insensitive collation that matches JS `localeCompare` for ASCII names (ICU root collation; e.g. `icu_collator`), otherwise ordering differs for mixed case / punctuation.

### 3.4 Instruction files (`opencode/src/session/instruction.ts`)

Constants (`instruction.ts:59-67`), `flags.disableClaudeCodePrompt = OPENCODE_DISABLE_CLAUDE_CODE || OPENCODE_DISABLE_CLAUDE_CODE_PROMPT` (`opencode/src/effect/runtime-flags.ts:23-26`):
- `globalFiles = [<global.config>/AGENTS.md, <home>/.claude/CLAUDE.md (unless disableClaudeCodePrompt)]`
- `instructionFiles = ["AGENTS.md", "CLAUDE.md" (unless disableClaudeCodePrompt), "CONTEXT.md"]` (CONTEXT.md marked deprecated)
- `global.config = OPENCODE_CONFIG_DIR ?? <xdgConfig>/opencode`; `home = OPENCODE_TEST_HOME ?? os.homedir()` (`core/src/global.ts:13,18-19,64`).

`systemPaths()` `instruction.ts:109-152` → ordered `Set<abs path>` (insertion order = output order):
1. Global: first **existing** of `globalFiles` (only one is added; `break`).
2. Project (skipped if `OPENCODE_DISABLE_PROJECT_CONFIG`): for each name in `instructionFiles` order, `findUp(name, ctx.directory, ctx.worktree)`; the **first name with ≥1 hit wins**, and **all** hits for that name are added (every ancestor dir from `directory` up to and including `worktree`, nearest first). Later names are not searched.
   - `FSUtil.findUp` (`core/src/fs-util.ts:154-166`): loop `join(current,target)` exists→push; stop when `current === stop` (exact string compare) or at filesystem root. With worktree `"/"` (non-git) it walks to `/`.
3. Config `instructions: string[]` (`core/src/v1/config/config.ts:124`): skip `http://`/`https://`; `~/` → `join(home, rest)`; absolute → `glob(basename, {cwd: dirname, absolute, include:"file"})` (glob only in last segment); relative → `globUp(pattern, directory, worktree)` (glob in every ancestor, `dot:true`, `core/src/fs-util.ts:184-199`), or if `OPENCODE_DISABLE_PROJECT_CONFIG` → `globUp(pattern, global.config, global.config)`. Glob errors → `[]`. Paths `path.resolve`d.

`system()` `instruction.ts:154-168`: read all paths (concurrency 8, read error → `""`), fetch URL entries (concurrency 4; `HttpClient.filterStatusOk` + `withTransientReadRetry`; `Effect.timeout(5000)` ms; any failure → `""`). Output: for each non-empty file `Instructions from: ${path}\n${content}`, then each non-empty URL `Instructions from: ${url}\n${body}`. Re-executed every loop step (files re-read, URLs re-fetched).

Nearby-instruction injection (read tool) — `resolve(messages, filepath, messageID)` `instruction.ts:178-220`:
- `root = path.resolve(InstanceState.directory)`; start at `dirname(resolve(filepath))`; loop `while (current.startsWith(root) && current !== root)` (string prefix test; root itself excluded).
- `find(dir)` = first existing of `instructionFiles` in that dir (`:170-176`).
- Skip if `found === target`, `found ∈ systemPaths()`, `found ∈ extract(messages)` (paths in `metadata.loaded` of completed, non-compacted `read` tool parts, `:16-31`), or already claimed for this `messageID`.
- Else claim (`claims: Map<MessageID, Set<path>>`, per instance), read, push `{filepath, content: "Instructions from: ${found}\n${content}"}` (empty file: claimed but not emitted).
- Read tool appends `\n\n<system-reminder>\n${contents.join("\n\n")}\n</system-reminder>` to text output and sets `metadata.loaded = [paths]` (`opencode/src/tool/read.ts:300,315,355-356,365`); for image/PDF results only `metadata.loaded` is set, content is not emitted (`read.ts:305-316`).
- Claims cleared: after each step `Effect.ensuring(instruction.clear(handle.message.id))` (`prompt.ts:1331`) and finalizer on user message id (`prompt.ts:691`). Durable dedupe therefore relies on `metadata.loaded` persisted in tool parts; pruning (`time.compacted`) re-enables injection.

Rust mapping: `InstructionService { claims: Mutex<HashMap<MessageId, HashSet<PathBuf>>> }`; implement `find_up`/`glob_up` with exact stop semantics (string-equal stop, include stop dir); use `globset` with dotfiles enabled for `globUp`; HTTP fetch with 5 s timeout + transient retry.

### 3.5 Reminders (`opencode/src/session/reminders.ts`, applied each step at `prompt.ts:1180-1184`, after title fork, before assistant msg creation)

Target = last user message in `msgs` (post `filterCompacted`). `flags.experimentalPlanMode = OPENCODE_EXPERIMENTAL_PLAN_MODE ?? OPENCODE_EXPERIMENTAL` (`runtime-flags.ts:47`).

| Mode | Condition | Text | Persisted? |
|---|---|---|---|
| legacy | `agent.name === "plan"` | `plan.txt` | **No** — pushed into in-memory `parts` only (`reminders.ts:27-36`) |
| legacy | any assistant in msgs has `agent==="plan"` AND current `agent.name==="build"` | `build-switch.txt` | No (re-added every step while condition holds) (`:37-47`) |
| experimental | `agent.name !== "plan"` AND last assistant `agent==="plan"` | `build-switch.txt` + (if plan file exists) `\n\nA plan file exists at ${plan}. You should execute on the plan defined within it` | Yes, `sessions.updatePart` (`:51-67`) |
| experimental | `agent.name==="plan"` AND last assistant agent `!== "plan"` | `plan-mode.txt` with `${planInfo}` → exists ? `A plan file already exists at ${plan}. You can read it and make incremental edits using the edit tool.` : `No plan file exists yet. You should create your plan at ${plan} using the write tool.` (creates parent dir if missing) | Yes (`:70-88`) |

All parts: `type:"text"`, `synthetic:true`, `id: PartID.ascending()`. Plan path `Session.plan` (`opencode/src/session/session.ts:331-336`): `project.vcs` ? `<worktree>/.opencode/plans` : `<Global.Path.data>/plans`, file `${session.time.created}-${session.slug}.md`.

### 3.6 Context overflow math (`opencode/src/session/overflow.ts`)

```
OUTPUT_TOKEN_MAX = 32_000                                   // provider/transform.ts:18
maxOutputTokens(m, cap=OUTPUT_TOKEN_MAX) = min(m.limit.output, cap) || cap   // transform.ts:1481-1483
COMPACTION_BUFFER = 20_000                                  // overflow.ts:6
usable = context==0 ? 0
       : m.limit.input ? max(0, m.limit.input - (cfg.compaction.reserved ?? min(20_000, maxOutputTokens)))
       :                 max(0, m.limit.context - maxOutputTokens)          // reserved NOT applied here
isOverflow = cfg.compaction.auto !== false && context != 0 &&
             (tokens.total || tokens.input+tokens.output+tokens.cache.read+tokens.cache.write) >= usable
```
`cap` = `OPENCODE_EXPERIMENTAL_OUTPUT_TOKEN_MAX` (positive int) else 32000 (`runtime-flags.ts:52`). Reasoning tokens not in fallback sum. Env `OPENCODE_DISABLE_AUTOCOMPACT` forces `compaction.auto=false`; `OPENCODE_DISABLE_PRUNE` forces `compaction.prune=false` (`opencode/src/config/config.ts:592-597`).

Config `compaction` (`core/src/v1/config/config.ts:149-167`): `auto?: bool` (default true), `prune?: bool` (default false), `tail_turns?: NonNegativeInt`, `preserve_recent_tokens?: NonNegativeInt`, `reserved?: NonNegativeInt`.

Triggers (all create a compaction task via `compaction.create(auto:true)`):
1. Pre-step: `lastFinished && lastFinished.summary !== true && isOverflow(lastFinished.tokens)` (`prompt.ts:1161-1167`), `overflow` unset.
2. Post finish-step usage: `!assistant.summary && isOverflow(usage.tokens)` → `needsCompaction` → `process()` returns `"compact"` (`opencode/src/session/processor.ts:491-496,693`) → `create({auto:true, overflow: !handle.message.finish})` (`prompt.ts:1320-1328`).
3. Provider `ContextOverflowError`: if `compaction.auto===false && !summary` → message error, `finish:"error"`, publish `session.error`, status idle; else publish `session.error` and `needsCompaction` (`processor.ts:621-633`).
4. Manual: `POST …/summarize` → `create({auto: payload.auto ?? false})` + `loop` (`opencode/src/server/routes/instance/httpapi/handlers/session.ts:273-293`).

Token estimate everywhere: `Math.max(0, Math.round(s.length / 4))` (`core/src/util/token.ts:3-5`) — `length` is **UTF-16 code units**.

### 3.7 Compaction (`opencode/src/session/compaction.ts`)

Constants `:28-33`: `PRUNE_MINIMUM=20_000`, `PRUNE_PROTECT=40_000`, `TOOL_OUTPUT_MAX_CHARS=2_000`, `PRUNE_PROTECTED_TOOLS=["skill"]`, `MIN_PRESERVE_RECENT_TOKENS=2_000`, `MAX_PRESERVE_RECENT_TOKENS=15_000`.

`create()` `:559-582`: new user message `{id: MessageID.ascending(), role:"user", model, sessionID, agent, time.created}` + part `{type:"compaction", auto, overflow}`. The loop picks it as `tasks.pop()` (`MessageV2.latest`, `opencode/src/session/message-v2.ts:595-612`) and calls `process({parentID: lastUser.id, messages, auto, overflow})` (`prompt.ts:1149-1159`; `"stop"` breaks loop).

`process()` `:319-557`:
1. Parent must be a user message (else throw).
2. Overflow replay (`overflow===true`): find nearest earlier user msg without compaction part → `replay`; `messages = slice(0, i)`; if no remaining user content before it, cancel replay (`:340-356`).
3. Agent `compaction`; model = `agent.model` ?? user's model (`:358-361`).
4. `history` = messages minus trailing parent (if it has the compaction part); `completedCompactions` (user-with-compaction + finished, non-error `summary:true` assistant child) are hidden; `previousSummary` = last such summary's text parts trimmed, joined `"\n\n"` (`:87-113,363-366`).
5. `select()` `:223-269`: `tail_turns<=0` → no tail. `budget = preserve_recent_tokens ?? min(15000, max(2000, floor(usable*0.25)))`. Turns = non-compaction user msgs each spanning to next user. Iterate last `tail_turns` (or all) turns newest→oldest; `size = estimate(JSON.stringify(toModelMessages(slice)))`; accumulate while `total+size <= budget`; on first miss try `splitTurn` (suffix starting at `start+1..` that fits remaining) then stop. `keep` undefined or `keep.start===0` → head = all, no tail; else `head = messages[..keep.start]`, `tail_start_id = keep.id`.
6. Plugin `experimental.session.compacting` `{sessionID}` → `{context: string[], prompt?: string}`; clone head, run `experimental.chat.messages.transform`; `conversation = head.map(serialize).filter(Boolean).join("\n\n")`.
   `serialize` `:54-85`: user → `[User]: ${non-ignored text parts joined "\n"}` + `[Attached ${mime}: ${filename ?? "file"}]` per file; assistant → `[Assistant]: …`, `[Assistant reasoning]: …`, tool → `[Assistant tool call]: ${tool}(${JSON.stringify(input)})` + `[Tool result]: ${output (+attachment lines) truncated to 2000 chars + "\n[truncated]"}` or `[Old tool result content cleared]` if compacted, or `[Tool error]: ${error}`.
7. `nextPrompt = plugin.prompt ?? [buildPrompt({previousSummary, context:[conversation]}), ...plugin.context].filter(Boolean).join("\n\n")`. `buildPrompt` (`core/src/session/compaction.ts:160-174`): `Here is the conversation so far:\n\n<conversation>\n${ctx}\n</conversation>` then either (no prior) `Create a new anchored summary from the conversation history in the <conversation> tags above so another coding agent can continue the work.` + `SUMMARY_TEMPLATE`, or (prior) `Here is the summary of the conversation before the <conversation> above:\n\n<prior-summary>\n${prev}\n</prior-summary>` + `SUMMARY_UPDATE_INSTRUCTIONS` + `SUMMARY_TEMPLATE`, joined `"\n\n"`.
8. Assistant msg `{mode:"compaction", agent:"compaction", summary:true, variant: user.model.variant, cost:0, zero tokens, path{cwd,root}, modelID, providerID}` (`:393-419`); `processor.process({tools:{}, system:[], messages:[{role:"user", content:[{type:"text", text: nextPrompt (+ "\n\nThe following is the conversation history:\n\n"+conversation if plugin prompt)}]}]})`. Effective system = `compaction.txt` + `user.system` (3.2). No env/instructions/skills.
9. Result `"compact"` → error `ContextOverflowError` message `Conversation history too large to compact - exceeds model context limit` (replay) / `Session too large to compact - context exceeds model limit even after stripping media`, `finish:"error"`, return `"stop"` (`:450-459`).
10. Persist `tail_start_id` onto compaction part if changed (`:461-466`).
11. If `"continue" && auto`: replay → new user msg copying `agent, model, format, tools, system`, parts cloned with new ids (skip compaction parts; media `file` parts → text `[Attached ${mime}: ${filename ?? "file"}]`). No replay → plugin `experimental.compaction.autocontinue` (default `{enabled:true}`); if enabled: user msg `{agent, model}` + synthetic text part `metadata:{compaction_continue:true}`, `time{start,end}`, text = (overflow ? `The previous request exceeded the provider's size limit due to large media attachments. The conversation was compacted and media files were removed from context. If the user was asking about attached images or files, explain that the attachments were too large to process and suggest they try again with smaller or fewer files.\n\n` : "") + `Continue if you have next steps, or stop and ask for clarification if you are unsure how to proceed.` (`:468-550`).
12. Error → `"stop"`; `"continue"` → publish `session.compacted {sessionID}` (`schema/src/session-compaction-event.ts:6-11`, `compaction.ts:552-556`).

History after compaction — `filterCompacted` (`message-v2.ts:534-585`) over messages newest→oldest (`orderBy desc(time_created), desc(id)`, `:452`): stop at the compaction user msg whose summary assistant completed (or, if part has `tail_start_id`, continue until that message id), reverse, then reorder to `[compaction-user, summary-assistant, ...tail(tail_start..compaction), ...after summary]`. In model conversion the compaction part renders as user text `What did we do so far?` (`message-v2.ts:238-243`); compacted tool output renders `[Old tool result content cleared]` with attachments dropped (`:303-308`). `toolOutputMaxChars`/`stripMedia` options exist (`:49-53,134`) but have no callers.

Prune `:273-317` (forked after loop exit, `prompt.ts:1338`; only if `compaction.prune===true`): iterate messages newest→oldest; count user msgs, skip until 2 user turns seen (inclusive of current msg); stop at an assistant with `summary`; per completed tool part (parts reverse order) except `skill`: if already `time.compacted` → stop all; `total += estimate(output)`; once `total > 40_000` add to prune list. If `pruned > 20_000` set `state.time.compacted = Date.now()` and `updatePart` each.

### 3.8 Title generation & session diff summary

Title — `ensureTitle` `prompt.ts:193-253`, forked (non-blocking, errors ignored) at loop `step === 1` (`prompt.ts:1133-1139`):
- Skip if `session.parentID` or title not default. Default regex `^(New session - |Child session - )\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$` (`session.ts:48-55`; default titles set at `session.ts:521` with `toISOString()`).
- "Real" user msg = user msg whose parts are not all `synthetic`; require exactly 1 real user msg in (filtered) history; context = history up to it.
- Model: `title` agent `model` (config) → `provider.getSmallModel(providerID)` → user's model. `getSmallModel` (`opencode/src/provider/provider.ts:1964-2032`): `cfg.small_model` → plugin `experimental.provider.small_model` → none for azure/azure-cognitive-services → family priority (`opencode*`: `["gpt-nano"]`; `github-copilot*`: `["gpt-mini","gemini-flash","gpt-nano","claude-haiku"]`; else `["gemini-flash","gpt-nano","claude-haiku"]`, `:2073`) over models sorted by `release_date` desc, `id` desc; bedrock prefers `global.` → region `us.`/`eu.` → unprefixed.
- Messages: `[{role:"user", content:"Generate a title for this conversation:\n"}, ...]` + (all parts `subtask` ? one user msg of subtask prompts joined `"\n"` : `toModelMessages(context, mdl)`). `llm.stream({agent: title, user: firstUser, system: [], small: true, tools: {}, retries: 2})` → system = `title.txt` + `firstUser.system`; options from `ProviderTransform.smallOptions` (`request.ts:83-84`).
- Post-process: concat text deltas; remove `/<think>[\s\S]*?<\/think>\s*/g`; split `"\n"`, trim, first non-empty; if `length > 100` → `substring(0,97)+"..."`; `sessions.setTitle`.

Session diff summary — `opencode/src/session/summary.ts`, `summarize({sessionID, messageID})` called (forked) at step 1 with `lastUser.id` (`prompt.ts:1252-1253`) and on every finish-step with `assistant.parentID` (`processor.ts:485-490`):
1. `setSummary({additions:0, deletions:0, files:0})` (always zeros) + publish `session.diff {sessionID, diff: []}` (`summary.ts:106-114`; event type `schema/src/v1/session.ts:644`).
2. If `cfg.snapshot === false` return. Else take the user msg + its assistant children; `from` = first `step-start.snapshot`, `to` = last `step-finish.snapshot`; `snapshot.diffFull(from,to)` → `user.info.summary.diffs`; `updateMessage` (`:82-127`).
- `diff()` HTTP read path unquotes git C-quoted paths (octal, `\n \r \t \b \f \v \\ \"`) (`:10-64,129-142`).

### 3.9 Core v2 equivalents (not on TUI path)

- `core/src/instruction-context.ts:40-101`: only `AGENTS.md` (no CLAUDE.md/CONTEXT.md, no `~/.claude`, no config `instructions`/URLs); global `<global.config>/AGENTS.md` always first, then **all** ancestor `AGENTS.md` via `fs.up` from `location.directory` to `location.project.directory` (only if inside project and not `OPENCODE_DISABLE_PROJECT_CONFIG`), deduped; rendered as one block joined `"\n\n"`. Differential context: update text `These instructions replace all previously loaded ambient instructions.\n\n…`, removal `Previously loaded instructions no longer apply.`; unreadable discovered file → `unavailable`.
- `core/src/system-context/builtins.ts:16-40`: env block without the "You are powered by" line and without date; date is a separate source (`Today's date: …` / update `Today's date is now: …`). Registry sorts sources by key (`registry.ts:90`).
- Compaction v2 `core/src/session/compaction.ts:176-247`: defaults `auto=true`, `buffer=20_000`, `keep.tokens=8_000`, summary max 4096 output tokens; triggers when `estimate({system,messages,tools}) > context - max(output, buffer)`; emits `SessionEvent.Compaction.Started/Ended`.

Rust mapping: implement legacy semantics first; keep v2 sources behind a separate module since lookup rules differ (CLAUDE.md support, stacking rules, config instructions).

### 3.10 Porting risks (this section)

1. Byte-exact system text: single `"\n"` join of provider/agent prompt + env + instructions + skills + user.system into ONE system message (plus plugin-collapse rule) — any whitespace drift breaks prompt-cache hits and parity.
2. `Token.estimate` uses UTF-16 length/4 with JS `Math.round` (half-up for positives); compaction tail selection estimates over `JSON.stringify(toModelMessages(...))`, so Rust must reproduce the AI-SDK ModelMessage JSON shape and key order to choose the same `tail_start_id`.
3. Legacy plan reminders are in-memory only (not persisted) while experimental ones are persisted; `localeCompare` sort order for skills/references; `findUp` stop uses exact string equality (Windows drive-letter case / trailing separators).

## 4. Tool framework and file tools

Executed path: legacy tools in `opencode/src/tool/*` wired by `opencode/src/tool/registry.ts` and bound to the AI SDK in `opencode/src/session/tools.ts`. `core/src/tool/*` is the v2 runtime (separate schemas, see 4.15). jsdiff `diff@8.0.2` (root `package.json:62`) produces every persisted diff string.

### 4.1 Tool contract (`opencode/src/tool/tool.ts`)

| Item | Definition |
|---|---|
| `Tool.Context` (`tool.ts:36-46`) | `sessionID`, `messageID`, `agent: string` (agent name), `abort: AbortSignal`, `callID?`, `extra?: {[k]: unknown}`, `messages: SessionV1.WithParts[]`, `metadata({title?, metadata?})`, `ask(Omit<PermissionV1.Request,"id"\|"sessionID"\|"tool">)` |
| `ExecuteResult` (`tool.ts:48-53`) | `{ title: string, metadata: M, output: string, attachments?: Omit<FilePart,"id"\|"sessionID"\|"messageID">[] }` |
| `Def` (`tool.ts:55-65`) | `id`, `description`, `parameters` (Effect Schema decoder), `jsonSchema?` (pre-built JSON Schema, plugin tools), `execute`, `formatValidationError?` (no built-in tool defines it) |
| `Info` (`tool.ts:71-77`) | `{ id, init(): Effect<DefWithoutID> }`; `Tool.define(id, init)` (`tool.ts:151-169`), `Tool.init(info)` (`tool.ts:171-181`) |

Wrapper applied to every built-in `execute` (`tool.ts:99-149`), in order:
1. Decode args with the tool's Effect Schema. Failure -> `InvalidArgumentsError{tool, detail}` whose message is verbatim `The ${tool} tool was called with invalid arguments: ${detail}.\nPlease rewrite the input so it satisfies the expected schema.` (`tool.ts:24-34`); `detail = String(schemaError)` (Effect formatter, text UNKNOWN exactly -> golden-test against effect@4.0.0-beta.83).
2. Run tool `execute`.
3. If `result.metadata.truncated !== undefined` return as-is (tool did its own truncation: read, glob, grep). Otherwise `Truncate.output(result.output, {}, agent)` and set `metadata.truncated` (bool) plus `metadata.outputPath` when truncated (`tool.ts:131-144`). Consequence: persisted metadata of edit/write/apply_patch/etc. always carries `truncated:false|true`.
4. `Effect.orDie` + span `Tool.execute` with attrs `tool.name`, `session.id`, `message.id`, `tool.call_id` (`tool.ts:114-119,145`). All failures become defects; the AI SDK sees a rejected promise -> `tool-error` -> part `state.status="error"`, `state.error = errorMessage(err)` (`opencode/src/session/processor.ts:186-199`; `errorMessage` = `err.message` \|\| `err.name` \|\| ... `tui/src/util/error.ts:136-156`).

Rust mapping: `trait Tool { fn id(); fn description(&Agent); fn schema() -> serde_json::Value; async fn execute(Value, Ctx) -> Result<ExecuteResult, ToolError> }`; keep metadata as `serde_json::Map` (persisted verbatim in part state); centralize steps 1-4 in one `run_tool()`.

### 4.2 Registry (`opencode/src/tool/registry.ts`)

Built-in order (`registry.ts:231-249`) — this order is the order tools are sent to the model:

| # | id (source) | Condition |
|---|---|---|
| 1 | `invalid` (`invalid.ts:9-21`) | always (but excluded from `activeTools`, `session/llm.ts:316`) |
| 2 | `question` (`question.ts:14`) | `flags.client ∈ {app,cli,desktop}` OR `OPENCODE_ENABLE_QUESTION_TOOL` (`registry.ts:207`) |
| 3 | `bash` (`shell/id.ts:16`, `shell.ts:338`) | always |
| 4 | `read` (`read.ts:64-69`) | always |
| 5 | `glob` (`glob.ts:17-18`) | always |
| 6 | `grep` (`grep.ts:20-21`) | always |
| 7 | `edit` (`edit.ts:58-59`) | filtered per model, see below |
| 8 | `write` (`write.ts:27-28`) | filtered per model |
| 9 | `task` (`task.ts:24,81`) | always |
| 10 | `webfetch` (`webfetch.ts:24`) | always |
| 11 | `todowrite` (`todo.ts:14`) | always |
| 12 | `websearch` (`websearch.ts:99`) | `webSearchEnabled(providerID)`: providerID `opencode` or `opencode-go`, or `enableExa` (`OPENCODE_ENABLE_EXA`/`OPENCODE_EXPERIMENTAL_EXA`/`OPENCODE_EXPERIMENTAL`), or `enableParallel` (`registry.ts:58-65,293-295`; flags `effect/runtime-flags.ts:31-39`) |
| 13 | `skill` (`skill.ts:12`) | always |
| 14 | `apply_patch` (`apply_patch.ts:22-23`) | filtered per model |
| 15 | `execute` (`code-mode.ts:12,188`) | `OPENCODE_EXPERIMENTAL_CODE_MODE` AND MCP catalog non-empty after permission filter (`registry.ts:118-119,280-289,305-308`) |
| 16 | `lsp` (`lsp.ts:37`) | `OPENCODE_EXPERIMENTAL_LSP_TOOL` (or `OPENCODE_EXPERIMENTAL`) |
| 17 | `plan_exit` (`plan.ts:15`) | `OPENCODE_EXPERIMENTAL_PLAN_MODE` AND `client === "cli"` |
| 18+ | custom tools (file + plugin) | appended after built-ins (`registry.ts:256-259`) |

Model swap (`registry.ts:297-300`): `usePatch = modelID.includes("gpt-") && !modelID.includes("oss") && !modelID.includes("gpt-4")`; `modelID` is `model.api.id` (`session/tools.ts:92-93`). `usePatch` -> expose `apply_patch`, hide `edit`+`write`; else the reverse. No `batch` tool exists in v1.19.5 (grep of `opencode/src`, `core/src`: no match).

Per-call materialization `tools({providerID, modelID, agent, permission})` (`registry.ts:291-340`): for each visible tool run plugin hook `tool.definition` `({toolID}, {description, parameters, jsonSchema})` (mutable output); `jsonSchema` kept only if plugin didn't replace `parameters` or changed `jsonSchema` (`registry.ts:319-322`). Description = `[description, taskSuffix, codeModeSuffix].filter(Boolean).join("\n")`. Task suffix (`registry.ts:265-278`): `"Available agent types and the tools they have access to:\n"` + lines `- ${name}: ${description ?? "This subagent should only be called manually by the user."}` for agents with `mode !== "primary"` where `Permission.evaluate("task", name, agent.permission).action !== "deny"`, sorted by `localeCompare`. Permission filtering of the tool list itself is done by the session loop (section 2/6), not the registry.

Custom file tools (`registry.ts:183-197`): for each `dir` in `config.directories()` (`opencode/src/config/config.ts:132`) glob `{tool,tools}/*.{js,ts}` (dot, symlink, absolute); waits `config.waitForDependencies()`; dynamic-imports module; each export passing `isPluginTool` (has `args`,`description`,`execute`, `registry.ts:355-357`) becomes id `default` -> `<basename>` else `<basename>_<exportName>`. Plugin tools: `plugin.list()[i].tool` map, id = key (`registry.ts:199-204`). Wrapping `fromPlugin` (`registry.ts:125-181`): args all-Zod -> `z.toJSONSchema(io:"input")` normalized (drop boolean `exclusiveMaximum/Minimum`, `$defs`->`definitions`), else legacy: entries that are JSON-schema objects, `required = all keys` (`registry.ts:363-372`). Plugin ctx adds `directory`, `worktree`, promise-based `ask` (`plugin/src/tool.ts:3-17`). Result may be string or `{title?, output, metadata?, attachments?}` (`plugin/src/tool.ts:36-43`); output truncated via `Truncate.output`; `title` default `""`.

Rust mapping: JS/TS custom tools cannot be loaded natively — need an embedded JS runtime (deno_core/boa) or an out-of-process plugin host; decide explicitly (porting risk).

### 4.3 Binding to the model (`opencode/src/session/tools.ts`, `session/llm.ts`)

- Context built per call (`session/tools.ts:59-90`): `extra = {model, bypassAgentCheck, promptOps}`; `metadata()` updates the running tool part only while status ∈ {running,pending} and sets `state={title, metadata, status:"running", input:args, time:{start}}`; `ask()` -> `permission.ask({...req, sessionID, tool:{messageID, callID}, ruleset: Permission.merge(agent.permission, session.permission ?? [])})`.
- Input schema sent to provider: `ProviderTransform.schema(model, ToolJsonSchema.fromTool(item))` (`session/tools.ts:98`; `provider/transform.ts:1575`, providers spec).
- Hooks: `tool.execute.before` `({tool, sessionID, callID}, {args})`, `tool.execute.after` `({tool, sessionID, callID, args}, output)` (`session/tools.ts:105-124`). Attachments get `id=PartID.ascending()`, `sessionID`, `messageID` (`session/tools.ts:111-118`).
- Tool-call repair (`session/llm.ts:295-311`): unknown/invalid call -> if lowercase name exists use it; else rewrite to tool `invalid` with input `{"tool": <name>, "error": <error.message>}`.
- `invalid` tool: params `{tool: string, error: string}`, description `"Do not use"`, output `The arguments provided to the tool are invalid: ${error}`, title `Invalid Tool`, metadata `{}` (`invalid.ts:4-21`).
- MCP resource tools are added here too (owned by MCP spec, `session/tools.ts:27-40,135+`).

### 4.4 JSON Schema conversion (`opencode/src/tool/json-schema.ts`)

`fromTool(tool) = tool.jsonSchema ?? fromSchema(tool.parameters)` (`json-schema.ts:24-26`). `fromSchema` (`:8-22`, cached per schema): `Schema.toJsonSchemaDocument(schema, {additionalProperties:true})`, prefix `$schema: JsonSchema.META_SCHEMA_URI_DRAFT_2020_12`, attach `$defs` if any, then `normalize` -> inline local `$ref` -> drop `$defs/definitions` if no refs remain. `normalize` (`:28-88`) rules in order:
1. recurse; properties not in `required` get `stripNull` (remove `{type:"null"}` from `anyOf`);
2. delete `additionalProperties: true`;
3. `anyOf` of `number` + only non-finite enums (`"NaN"`,`"Infinity"`,`"-Infinity"`) -> plain number;
4. `anyOf` of `{type:object, no properties}` + `{type:array, no items}` -> `{type:"object", properties:{}}`;
5. single-member `anyOf` -> inline it;
6. `allOf` of records with disjoint keys -> merge;
7. `type:"integer"` without `maximum` -> add `minimum: Number.MIN_SAFE_INTEGER` (unless present) and `maximum: Number.MAX_SAFE_INTEGER` (±9007199254740991).

Rust mapping: hand-write each built-in tool's JSON Schema as a `serde_json::json!` constant generated once from the TS (golden file) rather than re-deriving from Effect Schema; `NonNegativeInt` (read offset/limit) must emit the exact integer bounds of step 7. Exact Effect output for `NonNegativeInt` (e.g. `minimum:0`) UNKNOWN -> dump `ToolJsonSchema.fromTool` for every tool with bun and freeze as fixtures.

### 4.5 Output truncation (`opencode/src/tool/truncate.ts`, `truncation-dir.ts`)

| Item | Value |
|---|---|
| Limits | `MAX_LINES=2000`, `MAX_BYTES=50*1024` (`truncate.ts:14-15`); overridable by config `tool_output.max_lines` / `tool_output.max_bytes` (positive ints, `core/src/config/tool-output.ts:7-8`; `truncate.ts:75-83`) |
| Dir | `TRUNCATION_DIR = <xdgData>/opencode/tool-output` (`truncation-dir.ts:4`, `core/src/global.ts:10-11`) |
| File name | `ToolID.ascending()` = `tool_` + 12 hex (ms*0x1000+counter, 6 bytes) + 14 base62 (`opencode/src/tool/schema.ts:6-13`, `opencode/src/id/id.ts:51-70`) |
| Retention | files starting `tool_` with mtime older than 7 days deleted; sweep 1 min after start then every 1 h (`truncate.ts:12,53-66,143-148`) |

Algorithm `output(text, {maxLines?, maxBytes?, direction="head"|"tail"}, agent?)` (`truncate.ts:85-141`): `lines = text.split("\n")`, `totalBytes = utf8 len`. If `lines.length <= maxLines && totalBytes <= maxBytes` return unchanged. Head: take lines while `i < maxLines`, each costs `utf8(line) + (i>0 ? 1 : 0)`; stop on byte overflow (`hitBytes`). Tail: same from the end. `removed = hitBytes ? totalBytes - bytes : lines.length - out.length`; `unit = hitBytes ? "bytes" : "lines"`. Full text written to file. Hint (verbatim, `truncate.ts:129-131`):
- agent has task permission (`evaluate("task","*",agent.permission).action !== "deny"`): `The tool call succeeded but the output was truncated. Full output saved to: ${file}\nUse the Task tool to have explore agent process this file with Grep and Read (with offset/limit). Do NOT read the full file yourself - delegate to save context.`
- else: `The tool call succeeded but the output was truncated. Full output saved to: ${file}\nUse Grep to search the full content or Read with offset/limit to view specific sections.`

Result: head `${preview}\n\n...${removed} ${unit} truncated...\n\n${hint}`; tail `...${removed} ${unit} truncated...\n\n${hint}\n\n${preview}`.

### 4.6 External-directory guard (`opencode/src/tool/external-directory.ts`)

`assertExternalDirectoryEffect(ctx, target?, {bypass?, kind="file"})` (`:15-45`): no-op if no target or `bypass`. On win32 `target = FSUtil.normalizePath` (resolve + `realpathSync.native`, `core/src/fs-util.ts:228-236`). Inside project if `FSUtil.contains(instance.directory, p)` or (`worktree !== "/"` and contains worktree) (`opencode/src/project/instance-context.ts:18-24`). Otherwise `dir = kind=="directory" ? p : dirname(p)`, `glob = join(dir,"*")` (win32 via `normalizePathPattern`, else `\`->`/`), then `ctx.ask({permission:"external_directory", patterns:[glob], always:[glob], metadata:{filepath, parentDir: dir}})`. Used by read, write, edit, apply_patch (each hunk + move target), glob, grep.

### 4.7 `read` (`opencode/src/tool/read.ts`, description `opencode/src/tool/read.txt`)

Params (`read.ts:28-36`): `filePath: string` (required), `offset?: NonNegativeInt` ("1-indexed"), `limit?: NonNegativeInt` (default 2000). Constants: `DEFAULT_READ_LIMIT=2000`, `MAX_LINE_LENGTH=2000`, suffix `... (line truncated to 2000 chars)`, `MAX_BYTES=50*1024` label `50 KB`, `SAMPLE_BYTES=4096`, images `image/jpeg|png|gif|webp` (`read.ts:13-19`).

Flow (`read.ts:229-377`):
1. Relative path -> `resolve(instance.directory, p)`; win32 `normalizePath`. `title = relative(worktree, filepath)`.
2. `stat` (NotFound -> undefined). External guard with `bypass = ctx.extra.bypassCwdCheck` (set when the prompt loop expands `file:` parts, `session/prompt.ts:813-826`) and kind from stat.
3. `ctx.ask({permission:"read", patterns:[relative(worktree, filepath)], always:["*"], metadata:{}})` — asked even if file is missing.
4. Missing -> suggestions: entries of `dirname` whose lowercase name contains basename or vice versa, first 3, joined absolute: `File not found: ${p}\n\nDid you mean one of these?\n${list}` else `File not found: ${p}` (`read.ts:76-99`).
5. Directory: entries sorted `localeCompare`, dirs and symlinks-to-dirs suffixed `/` (`:101-115`); `offset = params.offset || 1`; slice `[offset-1, offset-1+limit)`. Output lines: `<path>${p}</path>`, `<type>directory</type>`, `<entries>`, entries joined `\n`, then `\n(Showing ${n} of ${total} entries. Use 'offset' parameter to read beyond entry ${offset+n})` or `\n(${total} entries)`, `</entries>` — all joined by `\n`. Metadata `{preview: first 20 entries, truncated, loaded: [], display:{type:"directory", path, entries, offset, totalEntries, truncated}}`.
6. File: `loaded = instruction.resolve(ctx.messages, filepath, ctx.messageID)` (nested AGENTS.md etc., `session/instruction.ts:38`, section 3). Sample first 4096 bytes; MIME = magic sniff (`png, jpeg, gif, bmp, pdf, webp` via RIFF..WEBP, `opencode/src/util/media.ts:15-26`) else extension lookup else `application/octet-stream`.
7. Image (4 MIMEs, BMP excluded) or PDF -> output `Image read successfully` / `PDF read successfully`, attachment `{type:"file", mime, url:"data:${mime};base64,..."}`, metadata `{preview: msg, truncated:false, loaded}`.
8. Binary check (`read.ts:182-227`): extension in {.zip .tar .gz .exe .dll .so .class .jar .war .7z .doc .docx .xls .xlsx .ppt .pptx .odt .ods .odp .bin .dat .obj .o .a .lib .wasm .pyc .pyo} -> binary; empty sample -> text; any `0x00` -> binary; else `count(b<9 || 13<b<32)/len > 0.3` -> binary. Error `Cannot read binary file: ${p}`.
9. Line streaming (`read.ts:137-180`): UTF-8 streaming decode + `Stream.splitLines` (CR handling of effect@4.0.0-beta.83 `splitLines`: UNKNOWN for lone `\r` -> check effect source); counts all lines (keeps counting after limit for total); per line truncate to 2000 chars (UTF-16 units) + suffix; byte budget `utf8(line) + (n>0?1:0) <= 51200` else `cut=true` and stop.
10. `count < offset` and not (`count==0 && offset==1`) -> `Offset ${offset} is out of range for this file (${count} lines)`.
11. Output: `<path>${p}</path>\n<type>file</type>\n<content>\n` + lines `${lineNo}: ${text}` joined `\n` + one of: `\n\n(Output capped at 50 KB. Showing lines ${offset}-${last}. Use offset=${last+1} to continue.)` / `\n\n(Showing lines ${offset}-${last} of ${count}. Use offset=${last+1} to continue.)` / `\n\n(End of file - total ${count} lines)`, then `\n</content>`. If `loaded` non-empty append `\n\n<system-reminder>\n${contents joined "\n\n"}\n</system-reminder>`. LSP `touchFile` warm-up forked (`:117-120`).
12. Metadata `{preview: first 20 lines, truncated: more||cut, loaded: [paths], display:{type:"file", path, text, lineStart, lineEnd, totalLines, truncated}}`. Wrapper truncation skipped (metadata.truncated set).

### 4.8 `write` (`opencode/src/tool/write.ts`, description `opencode/src/tool/write.txt`)

Params (`write.ts:20-25`): `content: string`, `filePath: string` (order as declared). Flow (`:38-101`): relative -> `join(instance.directory, p)`; external guard; `exists = existsSafe`; old = BOM-split UTF-8 text or `""`; `desiredBom = old.bom || content.bom`; `diff = trimDiff(createTwoFilesPatch(p, p, old, new))`; `ctx.ask({permission:"edit", patterns:[relative(worktree,p)], always:["*"], metadata:{filepath, diff}})` (relative path keeps native separators — backslashes on Windows); `writeWithDirs` (mkdir -p on NotFound, `core/src/fs-util.ts:127-140`); if `format.file(p)` (formatter ran, `opencode/src/format/index.ts:188`) re-sync BOM; publish `file.edited {file}` and `file.watcher.updated {file, event: exists?"change":"add"}` (`schema/src/filesystem.ts:8-11`, `schema/src/filesystem-watcher.ts:6-12`). Output `Wrote file successfully.`; then `lsp.touchFile(p,"document")`, iterate `lsp.diagnostics()` entries: current file -> `\n\nLSP errors detected in this file, please fix:\n${block}`; other files (max 5 with non-empty blocks) -> `\n\nLSP errors detected in other files:\n${block}` (`write.ts:74-90`). Title `relative(worktree,p)`; metadata `{diagnostics, filepath, exists}` (+`truncated`).

LSP block (`opencode/src/lsp/diagnostic.ts:3-27`): only severity 1, max 20 per file: `<diagnostics file="${file}">\n${lines}${more>0 ? "\n... and ${more} more" : ""}\n</diagnostics>`; line = `${SEV} [${line+1}:${char+1}] ${message}` with SEV map 1 ERROR,2 WARN,3 INFO,4 HINT.

### 4.9 `edit` (`opencode/src/tool/edit.ts`, description `opencode/src/tool/edit.txt`)

Params (`edit.ts:47-56`): `filePath: string`, `oldString: string`, `newString: string`, `replaceAll?: boolean`.

Flow (`edit.ts:69-212`):
1. `!filePath` -> `filePath is required`; `oldString === newString` -> `No changes to apply: oldString and newString are identical.`
2. Relative -> `join(instance.directory, p)`; external guard.
3. Per-file mutex: process-global `Map<FSUtil.resolve(p), Semaphore(1)>` (never evicted) (`:35-45,88`). Permission ask happens inside the lock.
4. `oldString === ""`: file exists -> `oldString cannot be empty when editing an existing file. Provide the exact text to replace, or use write for an intentional full-file replacement.`; else create file with `newString` (BOM from newString), ask, write, format, events `file.edited` + watcher `add`.
5. Else: missing -> `File ${p} not found`; directory -> `Path is a directory, not a file: ${p}`. Read BOM-split text. `ending = text.includes("\r\n") ? "\r\n" : "\n"`; `old`/`new` = CRLF->LF then LF->ending. `replace(content, old, new, replaceAll)`; `desiredBom = source.bom || result.bom`. Diff for permission = `trimDiff(createTwoFilesPatch(p,p, LF(old), LF(new)))`; ask `edit` (same shape as write). Write, format (if formatted, re-read content), events `file.edited` + watcher `change`, recompute diff after formatting.
6. `additions/deletions` = sums of `diffLines(contentOld, contentNew)` counts (raw, not LF-normalized). `filediff = {file, patch: diff, additions, deletions}` (`schema/src/file-diff.ts:6-11`). `ctx.metadata({metadata:{diff, filediff, diagnostics:{}}})`.
7. Output `Edit applied successfully.` + optional `\n\nLSP errors detected in this file, please fix:\n${block}` (current file only). Title `relative(worktree,p)`. Metadata `{diagnostics, diff, filediff}` (+`truncated`).

`replace(content, oldString, newString, replaceAll=false)` (`edit.ts:682-729`) — exact algorithm:
```
if old == new: error "No changes to apply: ..."; if old == "": error "oldString cannot be empty ..."
notFound = true
for R in [Simple, LineTrimmed, BlockAnchor, WhitespaceNormalized, IndentationFlexible,
          EscapeNormalized, TrimmedBoundary, ContextAware, MultiOccurrence]:
  for search in R(content, oldString):            # lazy generator, in yield order
    index = content.indexOf(search); if index == -1: continue
    notFound = false
    if isDisproportionateMatch(search, oldString): error "Refusing replacement because the matched span is much larger than oldString. Re-read the file and provide the full exact oldString for the intended replacement."
    if replaceAll: return content.replaceAll(search, newString)       # JS String.replaceAll
    if index != content.lastIndexOf(search): continue                  # ambiguous -> next candidate
    return content[..index] + newString + content[index+len(search)..]
error notFound ? "Could not find oldString in the file. It must match exactly, including whitespace, indentation, and line endings."
               : "Found multiple matches for oldString. Provide more surrounding context to make the match unique."
```
`isDisproportionateMatch` (`:731-737`): `oldLines = old.split("\n").length`, `searchLines` likewise; true if `searchLines >= max(oldLines+3, oldLines*2)`; else false if `oldLines == 1`; else true if `search.trim().length > max(old.trim().length+500, old.trim().length*4)`.

Replacers (all split on `"\n"`; "offsets" = UTF-16 indices; `trim` = JS `String.prototype.trim`):

| Replacer | Yields (`edit.ts` lines) |
|---|---|
| Simple (`244-246`) | `find` itself |
| LineTrimmed (`248-286`) | drop trailing `""` from find lines; every window `i` where all `orig[i+j].trim() == find[j].trim()` -> the original substring (lines joined by `\n`, no trailing newline) |
| BlockAnchor (`288-425`) | requires ≥3 find lines (checked before popping trailing `""`); anchors = trimmed first/last line; `maxLineDelta = max(1, floor(n*0.25))`; for each `i` with first anchor, take the FIRST `j>=i+2` with last anchor, candidate if `|(j-i+1) - n| <= maxLineDelta`. Similarity over middle lines `k=1..min(n,blk)-2`: `1 - lev(a.trim(), b.trim())/max(len)` (skip if both empty). Single candidate: `sim += term/linesToCheck`, early-break once `>= 0.65`; accept `>= 0.65` (if `linesToCheck<=0` sim=1). Multiple: average (sum/linesToCheck), choose max (first wins ties, strict `>`), accept `>= 0.65`. Thresholds `SINGLE_/MULTIPLE_CANDIDATES_SIMILARITY_THRESHOLD = 0.65` (`220-221`). Levenshtein = classic DP, insert/delete/substitute cost 1 (`226-242`) |
| WhitespaceNormalized (`427-469`) | `norm = s.replace(/\s+/g," ").trim()`. Per line: if `norm(line)==norm(find)` yield line; else if `norm(line)` contains `norm(find)`: build regex of `find.trim().split(/\s+/)` words (regex-escaped) joined `\s+`, yield first match in the line. Then if find has >1 line: every window of `findLines.length` lines whose joined `norm == norm(find)` |
| IndentationFlexible (`471-497`) | `removeIndentation` strips min leading-whitespace count of non-blank lines; yield every window (size = find line count) equal after removal |
| EscapeNormalized (`499-546`) | `unescape` replaces `\\(n\|t\|r\|'\|"\|`\|\\\|\n\|\$)` with the char; yield `unescape(find)` if contained; then every window whose `unescape(block) == unescape(find)` yields the raw block |
| TrimmedBoundary (`562-586`) | only if `find.trim() != find`: yield trimmed if contained; then windows with `block.trim() == find.trim()` |
| ContextAware (`588-644`) | ≥3 lines, pop trailing `""`; for each `i` with trimmed first line, FIRST `j>=i+2` with trimmed last line: if block line count == find count and (no non-empty middle lines or ≥50% of middle lines with any content are trim-equal) yield block and break; `break` after first `j` regardless |
| MultiOccurrence (`548-560`) | `find` once per exact non-overlapping occurrence |

`trimDiff(diff)` (`edit.ts:646-680`): over lines starting `+`,`-`,` ` but not `---`/`+++`, compute min leading-whitespace of the content after the prefix (non-blank only); if >0 strip that many chars after the prefix in those lines.

Porting traps: (a) `replaceAll` is JS `String.prototype.replaceAll(string, string)` which applies GetSubstitution: `$$`->`$`, `$&`->match, `` $` ``->prefix, `$'`->suffix in `newString` (single replacement path does NOT). (b) JS `trim`/`\s` include U+FEFF and all Unicode `Zs`; Rust `char::is_whitespace` excludes U+FEFF. (c) lengths/levenshtein are UTF-16 code units. (d) `diff` text is persisted in part metadata and shown in permission prompts — must byte-match jsdiff 8.0.2 `createTwoFilesPatch` (default 4 context lines; header form `Index: f\n====...\n--- f\n+++ f` per jsdiff `formatPatch`, not in repo -> verify with golden tests).

### 4.10 `apply_patch` (`opencode/src/tool/apply_patch.ts`, `opencode/src/patch/index.ts`, description `opencode/src/tool/apply_patch.txt`)

Param (`apply_patch.ts:18-20`): `patchText: string`. Permission key `edit`.

Grammar (as accepted by legacy parser `patch/index.ts:70-241`):
```
[cat ]<<['"]?TAG['"]?\n ... \nTAG      (optional heredoc wrapper, stripped; regex patch/index.ts:178)
*** Begin Patch                         (first line whose trim() equals it)
( *** Add File: <path>
    (+<line>)*                          (non-'+' lines ignored)
| *** Delete File: <path>
| *** Update File: <path>
  [*** Move to: <path>]                 (must be the very next line)
  ( @@[ <context>]
      ( ' '<ctx> | -<old> | +<new> )*   (other lines, incl. empty "", ignored)
  )*
)*
*** End Patch                           (first line whose trim() equals it; text after ignored)
```
Parser facts: input `trim()`ed then split on `"\n"` only (CR kept in lines; only the empty-patch check normalizes CR, `apply_patch.ts:48`). Header paths `.trim()`; empty path -> line skipped. Unknown lines between hunks skipped. Update: lines before the first `@@` are skipped; `@@` text after the marker `.trim()` -> `change_context` (empty -> none). A chunk ends at next `@@` or any line starting `***`. `*** End of File` is effectively ignored: both loops stop on `startsWith("***")` before the equality check (`patch/index.ts:107,118,121`), so `is_end_of_file` is never set in legacy. Add content = `+` lines joined with `\n` (trailing newline removed; tool re-appends `\n` if non-empty, `apply_patch.ts:79-80`).

Content derivation `deriveNewContentsFromChunks(path, chunks, text)` (`patch/index.ts:307-340`): BOM split; `lines = text.split("\n")`, pop one trailing `""`. For each chunk (`computeReplacements :342-396`), `lineIndex` starts 0:
1. `change_context` -> `seekSequence(lines, [ctx], lineIndex)`; miss -> `Failed to find context '${ctx}' in ${path}`; `lineIndex = idx+1`.
2. No old lines -> insert `new_lines` at EOF (`lines.length`, or `length-1` if last line is `""`) — ignores context position.
3. Else seek `old_lines` from `lineIndex`; if miss and last old line is `""`, retry without it (and drop trailing `""` from new). Miss -> `Failed to find expected lines in ${path}:\n${old_lines.join("\n")}`. Record `[idx, oldLen, new]`, `lineIndex = idx+oldLen`.
4. Sort by start; apply in reverse with splice. Ensure trailing `""` (file ends with `\n`). BOM = original || new.

`seekSequence(lines, pattern, start, eof)` (`patch/index.ts:429-484`): empty pattern -> -1. Four passes, each over the whole range before the next: exact; `trimEnd` equal; `trim` equal; `normalizeUnicode(trim)` equal where U+2018/2019/201A/201B -> `'`, U+201C/201D/201E/201F -> `"`, U+2010..2015 -> `-`, U+2026 -> `...`, U+00A0 -> space (`:418-425`). Per pass: if `eof` try `lines.length - pattern.length` first (when `>= start`), then forward scan `i = start..=len-plen`.

Tool flow (`apply_patch.ts:30-304`):
1. Empty `patchText` -> `patchText is required`. Parse error -> `apply_patch verification failed: ${error}` (JS `String(Error)` => `Error: <msg>` prefix). No hunks: normalized text `== "*** Begin Patch\n*** End Patch"` -> `patch rejected: empty patch`, else `apply_patch verification failed: no hunks found`.
2. Per hunk: `filePath = resolve(instance.directory, hunk.path)`; external guard. add: diff vs `""`, `diffLines` counts. update: stat missing/dir -> `apply_patch verification failed: Failed to read file to update: ${p}`; derive (errors wrapped as above); move target resolved + guarded; type `move` if `move_path`. delete: read (error -> `apply_patch verification failed: ${message}`), `deletions = content.split("\n").length`, additions 0. `totalDiff += diff + "\n"`.
3. `files[] = {filePath, relativePath (relative(worktree, movePath ?? filePath), "/" separators), type, patch, additions, deletions, movePath?}`.
4. One ask: `{permission:"edit", patterns: relative(worktree, filePath) with "/" for every change, always:["*"], metadata:{filepath: patterns.join(", "), diff: totalDiff, files}}`.
5. Apply sequentially: add/update `writeWithDirs`; move = write to movePath then remove source; delete remove. Non-delete: format target, BOM re-sync, publish `file.edited`. Then publish `file.watcher.updated` for all (`add`/`change`/`unlink`; move = unlink src + add dst). Not atomic: a mid-way failure leaves earlier files written.
6. `lsp.touchFile(target,"document")` for non-deletes; output `Success. Updated the following files:\n` + lines `A <rel>` / `D <rel>` / `M <rel(target)>`; per non-delete file with errors `\n\nLSP errors detected in ${rel}, please fix:\n${block}`. `title = output` (whole text); metadata `{diff: totalDiff, files, diagnostics}` (+`truncated`).

Other exports in `patch/index.ts` (`maybeParseApplyPatch`, `maybeParseApplyPatchVerified`, `applyPatch`, `generateUnifiedDiff`) have no callers in `opencode/src` or `core/src` (grep verified); porting them is optional.

Core `core/src/patch.ts` (v2) is STRICT: throws `Invalid patch line`, `Invalid add file line`, `Invalid update file line`, `Invalid update chunk line`, empty paths, update without `@@` (`core/src/patch.ts:25-130`); honours `*** End of File` (`:113-117`); pure additions insert at `lines.length` (`:141-143`). Do not mix the two when porting the legacy behaviour.

Rust mapping: port `patch/index.ts` line-for-line (including the ignored-line leniency and dead EOF branch) behind a `LegacyPatch` module; keep `core/src/patch.ts` semantics as a separate strict parser for the v2 runtime.

### 4.11 `glob` (`opencode/src/tool/glob.ts`, description `opencode/src/tool/glob.txt`)

Params (`glob.ts:10-15`): `pattern: string`, `path?: string`. Flow (`:25-73`): ask first `{permission:"glob", patterns:[pattern], always:["*"], metadata:{pattern, path}}`; `search = path ?? instance.directory` (relative -> resolve against directory); stat says file -> `glob path must be a directory: ${search}`; external guard kind `directory`; `ripgrep.glob({cwd: search, pattern, limit: 100})` (no hidden, no follow). Output: `No files found`, or absolute paths `resolve(search, rel)` one per line in ripgrep emission order (no mtime sort), plus `\n(Results are truncated: showing first 100 results. Consider using a more specific path or pattern.)` when `count == 100`. Title `relative(worktree, search)`; metadata `{count, truncated}`.

### 4.12 `grep` (`opencode/src/tool/grep.ts`, description `opencode/src/tool/grep.txt`)

Params (`grep.ts:10-18`): `pattern: string`, `path?: string`, `include?: string`. Flow (`grep.ts:28-112`): empty pattern -> `pattern is required`; ask `{permission:"grep", patterns:[pattern], always:["*"], metadata:{pattern, path, include}}`; `requested = path` absolute or `join(directory, path ?? ".")`; external guard (kind from stat); `search = FSUtil.resolve(requested)` (realpath); `cwd = dir ? search : dirname(search)`; `ripgrep.grep({cwd, pattern, include, limit:100})` — NOTE no `file` arg, so a file path searches its whole parent directory. Rows' path = `resolve(requested-dir, entry.path)` (keeps symlink alias, test `opencode/test/tool/grep.test.ts:217-219`). No results -> output `No files found`, metadata `{matches:0, truncated:false}`. Output:
```
Found ${n} matches[ (more matches available)]      # suffix when n == 100
<abs path>:
  Line ${line}: ${text}
                                                   # "" between files (on path change)
[\n(Results truncated. Consider using a more specific path or pattern.)]   # when n == 100
```
`text` = rg JSON `lines.text`, which keeps the line terminator (verified: rg emits `"lines":{"text":"line2\n"}`), so each match line ends with `\n` before the join `\n` (extra blank line). Lines >2000 UTF-16 units cut to 2000 (dangling high surrogate removed) + `...` (`core/src/ripgrep.ts:267-270`). Order = rg emission order (no mtime sort). Title = pattern; metadata `{matches, truncated}`.

### 4.13 Ripgrep adapter (`core/src/ripgrep.ts`, `core/src/ripgrep/binary.ts`)

| Op | Args (`ripgrep.ts` lines) |
|---|---|
| `glob` (`155-186`) | `--no-config --files [--hidden] [--follow] --glob=<pattern> --glob=!**/.git/** .` |
| `find` (`187-217`) | same, `--glob` omitted when pattern `*` |
| `grep` (`218-279`) | `--no-config --json --hidden --no-messages [--glob=<include>] --glob=!**/.git/** -- <pattern> <file ?? ".">` |

Runner (`98-152`): spawn in `cwd`, `stdin: ignore`, env inherited; stderr collected up to 8 KiB; stdout split lines, non-empty, parsed; `take(limit+1)` -> if more than `limit`, return first `limit` and stop without checking exit code. Exit 1 -> `[]`; exit 2 + stderr contains `regex parse error`/`error parsing regex` -> `InvalidPatternError{message: stderr.trim()}`; exit 2 otherwise -> partial rows; other codes -> `Ripgrep.Error(stderr.trim() || "ripgrep failed with code N")`. grep JSON record > 64 KiB -> error `Ripgrep JSON record exceeded 65536 bytes`; non-`match` types ignored; submatches capped 100. Paths: strip leading `./` (repeated) and leading slashes, `\`->`/`.

Binary (`ripgrep/binary.ts:91-122`): `which("rg"|"rg.exe")` -> else `<xdgCache>/opencode/bin/rg[.exe]` -> else download `https://github.com/BurntSushi/ripgrep/releases/download/15.1.0/ripgrep-15.1.0-<triple>.<tar.gz|zip>` (triples `:15-23`), extract with `tar -xzf` or PowerShell `Expand-Archive`, copy `ripgrep-15.1.0-<triple>/rg[.exe]`, chmod 755. Cached per process.

Rust mapping: keep shelling out to `rg` (byte-identical ordering/ignore semantics) rather than the `grep`/`ignore` crates; reuse the same download/version for parity.

### 4.14 Shared helpers

- BOM (`opencode/src/util/bom.ts:7-27`): `split` strips leading U+FEFF, `join(text,bom)`, `readFile` decodes UTF-8 with `ignoreBOM:true`, `syncFile` rewrites only if BOM state differs. Invalid UTF-8 -> U+FFFD (TextDecoder default) and is written back on edit.
- `writeWithDirs` = write; on NotFound mkdir -p then retry (`core/src/fs-util.ts:127-140`).
- Events published by file tools: `file.edited {file}`, `file.watcher.updated {file, event: "add"|"change"|"unlink"}` via `EventV2Bridge`.
- Formatter: `Format.file(path) -> bool` (`opencode/src/format/index.ts:24,188`), LSP: `touchFile(path, "document"|"full")`, `diagnostics()` (`opencode/src/lsp/lsp.ts:123-124`) — owned by MCP/LSP spec.
- `core/src/file-mutation.ts` / `core/src/location-services.ts` are used only by v2 tools (`core/src/tool/{edit,write,apply-patch}.ts`), not by the legacy path.

### 4.15 v2 (`core/src/tool/*`) differences (not on the executed path)

- Framework `core/src/tool/tool.ts:71-162`: `Tool.make({description, input, output, structured?, toModelOutput?, execute})`; ctx `{sessionID, agent, assistantMessageID, toolCallID}`; invalid input -> `ToolFailure("Invalid tool input: ...")`; names `^[A-Za-z][A-Za-z0-9_-]{0,63}$`; permission via `Tool.withPermission`. Built-ins (`core/src/tool/builtins.ts:31-48`): apply_patch, bash, edit, glob, grep, question, read, skill, todowrite, webfetch, websearch, write — no task/lsp/plan_exit/execute ("TODO: edit fuzzy parity", `:26-29`).
- Descriptions inline in TS (not the `.txt` files). Param renames: read/write/edit use `path` (not `filePath`); glob/grep add `limit`; read `limit <= 2000`, `offset` PositiveInt (`core/src/tool/read-filesystem.ts:72-75`), media cap 20 MiB.
- edit: exact match only, multi-match error `Found multiple exact matches for oldString. Provide more surrounding context or set replaceAll to true.`, re-checks file unchanged after approval (`core/src/tool/edit.ts:115,172-181`); output `Edited file successfully: ...`/`Replacements: N`.
- Truncation store `core/src/tool-output-store.ts:13-17,159`: same limits/retention, different marker `... output truncated; full content saved to ${outputPath} ...`.

### 4.16 Porting risks (this section)

1. Persisted `metadata.diff`/`filediff.patch`/`files[].patch` require byte-identical jsdiff 8.0.2 `createTwoFilesPatch` + `diffLines` counts + `trimDiff`; Rust diff crates differ in hunk choice/headers. Mitigation: port jsdiff's Myers implementation + `formatPatch`, golden-test.
2. Edit fuzzy chain depends on JS string semantics (UTF-16 indices, `trim`/`\s` Unicode sets, `replaceAll` `$` substitution, regex escaping). Implement over `Vec<u16>` or verify with property tests against the TS.
3. Legacy patch parser leniency (ignored non-prefixed/empty lines, ignored pre-`@@` lines, dead `*** End of File`, CR retention, EOF-append for pure additions) is observable; strict core parser must not be substituted.
4. Tool JSON Schemas are generated by Effect Schema + `normalize`; freeze generated schemas as fixtures. `InvalidArgumentsError.detail` text is Effect-formatter-specific (UNKNOWN exact).
5. Custom `{tool,tools}/*.{js,ts}` and plugin tools need a JS host.
6. Windows: permission patterns of edit/write/read keep `\` separators, apply_patch uses `/`; `normalizePath` uses `realpathSync.native`.

## 5. Shell, task, todo, question, skill, web, lsp, plan, code-mode tools

Executed runtime: legacy `opencode/src/tool/*` registered by `opencode/src/tool/registry.ts`. The v2 (`core/src/tool/*`) differences are noted per tool as **v2 Δ**.

### 5.0 Inventory, gating, description sources

| Tool id (= default permission key) | Impl | Description source | Gate (registry) |
|---|---|---|---|
| `bash` | `opencode/src/tool/shell.ts:338` | `opencode/src/tool/shell/shell.txt` + inline sections in `opencode/src/tool/shell/prompt.ts:43-271` | always |
| `task` | `opencode/src/tool/task.ts:81` | `opencode/src/tool/task.txt` + `BACKGROUND_DESCRIPTION` (`task.ts:25`) + agent list (`registry.ts:265`) | always |
| `todowrite` | `opencode/src/tool/todo.ts:14` | `opencode/src/tool/todowrite.txt` | always |
| `question` | `opencode/src/tool/question.ts:14` | `opencode/src/tool/question.txt` | `OPENCODE_CLIENT` ∈ {`app`,`cli`,`desktop`} or `OPENCODE_ENABLE_QUESTION_TOOL` (`registry.ts:207,233`) |
| `skill` | `opencode/src/tool/skill.ts:12` | `opencode/src/tool/skill.txt` | always |
| `webfetch` | `opencode/src/tool/webfetch.ts:24` | `opencode/src/tool/webfetch.txt` | always |
| `websearch` | `opencode/src/tool/websearch.ts:99` | `opencode/src/tool/websearch.txt` (`{{year}}` → `new Date().getFullYear()`, `websearch.ts:107`) | `webSearchEnabled(providerID)` (`registry.ts:58,293`) |
| `lsp` | `opencode/src/tool/lsp.ts:37` | `opencode/src/tool/lsp.txt` | `OPENCODE_EXPERIMENTAL_LSP_TOOL` (`registry.ts:247`) |
| `plan_exit` | `opencode/src/tool/plan.ts:15` | `opencode/src/tool/plan-exit.txt` | `OPENCODE_EXPERIMENTAL_PLAN_MODE` **and** client=`cli` (`registry.ts:248`) |
| `execute` (code mode) | `opencode/src/tool/code-mode.ts:188` | inline `DESCRIPTION` (`code-mode.ts:14`) + catalog from `@opencode-ai/codemode` | `OPENCODE_EXPERIMENTAL_CODE_MODE` (`registry.ts:118,226,246`) and ≥1 visible MCP tool (`registry.ts:305-308`) |

- Builtin order emitted (`registry.ts:231-249`): `invalid, [question], bash, read, glob, grep, edit, write, task, webfetch, todowrite, websearch, skill, apply_patch, [execute], [lsp], [plan_exit]`, then custom/plugin tools. The model-facing filter for `apply_patch` vs `edit/write` is in §4.
- "enabledByExperimental" flags (`opencode/src/effect/runtime-flags.ts:11-14`): `OPENCODE_EXPERIMENTAL=true` turns on `*_LSP_TOOL`, `*_PLAN_MODE`, `*_CODE_MODE`, `*_BACKGROUND_SUBAGENTS` unless the specific var is explicitly set.
- **Not present in v1.19.5** (grep of `opencode/src` + `core/src`): `batch`, `multiedit`, `todoread`, `list`/`ls`, `patch` (only `apply_patch`), `plan_enter` (only exists as a permission key in `opencode/src/agent/agent.ts:127,149`; no tool), `repo_clone`/`repo_overview` (TODO in `core/src/tool/builtins.ts:26-29`).
- Generic wrapper (`opencode/src/tool/tool.ts:113-146`): args decoded by Effect Schema; if `result.metadata.truncated === undefined` the output is passed through `Truncate.output` (limits `tool_output.max_lines`/`max_bytes`, default 2000 lines / 51200 bytes, `opencode/src/tool/truncate.ts:14-15,75-82`). Tools that set `metadata.truncated` themselves (bash) skip it. Details in §4.
- Rust mapping: one `trait Tool { id(); description(ctx) -> String; schema() -> JsonValue; execute(args, ctx) }`; descriptions via `include_str!`; prompt fragments that are inline TS (shell profile sections, task background strings, code-mode description) must be copied verbatim into Rust `const &str` with the same `${...}`/`{{year}}` substitution.

### 5.1 `bash` (shell tool)

**Id/permission key**: `"bash"` regardless of actual shell (`opencode/src/tool/shell/id.ts:16`); shell kind `bash|pwsh|powershell|cmd`, anything else → `bash` (`shell/id.ts:1-12`).

**Parameters** (`opencode/src/tool/shell/prompt.ts:15-23`):

| field | type | notes |
|---|---|---|
| `command` | string, required | "The command to execute" |
| `timeout` | `PositiveInt` optional | ms; runtime also rejects `<0` with `Invalid timeout value: ${t}. Timeout must be a positive number.` (`shell.ts:615-617`) |
| `workdir` | string optional | resolved against instance directory (`shell.ts:612-614`) |

No `description` / `run_in_background` param in legacy. **Default timeout** = `OPENCODE_EXPERIMENTAL_BASH_DEFAULT_TIMEOUT_MS` (positive int) else `120000` (`shell.ts:347`, `runtime-flags.ts:53`). **No max timeout** in legacy.

**Shell selection** (`core/src/shell.ts`): `Shell.acceptable(cfg.shell)` (`shell.ts:600`; config key `shell`, `core/src/v1/config/config.ts:36`).
- `select(file, {acceptable})` (`core/src/shell.ts:114-121`): use `cfg.shell` or `$SHELL` if resolvable and not denied (`fish`, `nu` have `deny:true`, `core/src/shell.ts:13-23`); else Windows → first of `win()` list; else `fallback()`.
- `win()` order (`core/src/shell.ts:98-106`): `which("pwsh")`, `which("powershell")`, `gitbash()`, `$COMSPEC || "cmd.exe"` — i.e. **pwsh is preferred over git-bash on Windows**.
- `gitbash()` (`core/src/shell.ts:123-130`): `OPENCODE_GIT_BASH_PATH` if set; else `<dir(which git)>/../../bin/bash.exe` if non-empty file.
- `fallback()` (`core/src/shell.ts:132-137`): darwin `/bin/zsh`; else `which bash`; else `/bin/sh`.
- `name(file)` lowercases basename without extension on win32 (`core/src/shell.ts:139-142`); `full()` maps `bash` → git-bash on win32 (`:66-75`). Result cached per process unless cfg.shell given (`:214-218`).

**Description rendering** (`shell/prompt.ts:273-291`): `shell.txt` placeholders `${intro} ${os} ${shell} ${tmp} ${workdirSection} ${commandSection} ${gitCommands} ${toolName} ${gitCommandRestriction} ${createPrInstruction} ${createPrExample}`; missing key throws (`:28-34`). `os`=`process.platform` string (`win32|darwin|linux`), `shell`=`Shell.name`, `tmp`=`Global.Path.tmp`. Three profiles: cmd (`:224-235`), PowerShell (`:236-256`, path sep `\` on win32), default bash (`:257-270`); `${maxLines}`/`${maxBytes}`/`${defaultTimeoutMs}` interpolated from truncation limits.

**Spawn** (`shell.ts:293-310`):
- win32 + PowerShell: `spawn(shell, ["-NoLogo","-NoProfile","-NonInteractive","-Command", command])`, `detached:false`.
- otherwise: `ChildProcess.make(command, [], {shell, ...})` → cross-spawn with Node `shell` option (`core/src/cross-spawn-spawner.ts:373-381`). (assumption, Node semantics: POSIX → `[shell, "-c", command]`; win32 cmd → `cmd /d /s /c "command"`; win32 git-bash → `[bash, "-c", command]`.) `detached = platform != win32` (own process group).
- Always `stdin:"ignore"`, `windowsHide:true`; env = `process.env` ∪ plugin hook `shell.env` output `{env}` (`shell.ts:416-426`, input `{cwd, sessionID, callID}`).
- `Shell.args()` (login `-l`, sourcing rc files, `eval`) at `core/src/shell.ts:166-200` is **not** used by the tool; it's used by the session `!shell` command path (§2).

**Permission scan** (tree-sitter, `web-tree-sitter` + `tree-sitter-bash` / `tree-sitter-powershell` wasm, `shell.ts:311-336`). Parse with PS grammar iff `Shell.ps(shell)`. For every `command` node (`descendantsOfType("command")`, `:123-125`):
1. `parts()` (`:91-117`): collect children of type `command_name|command_name_expr|word|string|raw_string|concatenation`; for PS `command_elements` flatten, skipping `command_argument_sep` and `redirection`.
2. `cmd` = first token (lowercased for PS or cmd kind) (`:395`).
3. If `cmd` ∈ `FILES` (`cd chdir popd pushd push-location set-location rm cp mv mkdir touch chmod chown cat get-content set-content add-content copy-item move-item remove-item new-item rename-item`, `:28-50`) or (cmd-kind and ∈ `CMD_FILES` = `copy del dir erase md mkdir move rd ren rename rmdir type`, `:51-64`): for each path arg (`pathArgs`, `:188-218`: non-PS skips tokens starting `-`, `/`(cmd kind), `+`(chmod); PS skips switches `SWITCHES` `:66`, takes the value after `FLAGS` (`-destination -literalpath -path`, `:65`)): expand (`~`, PS `$env:X`, `${env:X}`, `$HOME/$PWD/$PSHOME`, `:135-160`), cut at first glob char `?*[` (`prefix`, `:181-186`), skip if dynamic (`$(`, `${`, backtick, `(`…, any `$` for bash, `:174-179`), PS provider prefix (`Filesystem::` ok, other `X::`/`Xx:` dropped, `:162-172`), resolve (`resolvePath` `:358-367`: win32 + POSIX shell + `/x/...` → `cygpath -w` via `shell -lc`, else `FSUtil.windowsPath` `/c/`, `/cygdrive/c/`, `/mnt/c/` → `C:/`). If outside instance (`containsPath`: within `directory`, or within `worktree` unless worktree=`/`, `opencode/src/project/instance-context.ts:18-24`) add dir (or parent dir if not a directory) to `scan.dirs`.
4. If tokens non-empty and cmd ∉ `CWD` set: `patterns += source(node)` (node text, or parent `redirected_statement` text, trimmed, `:119-121`); `always += BashArity.prefix(tokens).join(" ") + " *"` (`:407-410`).
5. `cwd` itself outside instance → added to dirs (`:626`).

Asks (`shell.ts:263-291`): (a) if dirs: `permission:"external_directory"`, patterns = always = `join(dir,"*")` (win32 `FSUtil.normalizePathPattern`), metadata `{command, directories, patterns}`; (b) if patterns: `permission:"bash"`, patterns = per-command source strings, always = arity prefixes, metadata `{command}`. A command consisting only of `cd ...` asks nothing.

**BashArity** (`opencode/src/permission/arity.ts:1-9`, table `:24-161`): longest prefix of the token list (joined by single spaces) found in `ARITY` → first `arity` tokens; else first token. Tokens include flags (e.g. `git -C x status` → `git -C *`). Copy the `ARITY` table verbatim.

**Execution / streaming** (`shell.ts:428-595`):
- Initial `ctx.metadata({metadata:{output:""}})`. Combined stdout+stderr (`handle.all`) decoded as UTF-8 text.
- Each chunk: push to ring list, drop oldest while total > `2*maxBytes` (sets `cut`); `last = preview(last+chunk)` where `preview` keeps last 30 000 chars prefixed `"...\n\n"` (`MAX_METADATA_LENGTH`, `:27,220-223`); `ctx.metadata({metadata:{output:last}})` **on every chunk** (each → part update + event).
- Spill: accumulate `full`; once `byteLength(full) > maxBytes` → `Truncate.write(full)` (file under `<data>/tool-output/`, `opencode/src/tool/truncation-dir.ts:4`) then append remaining chunks via `createWriteStream(..., {flags:"a"})`.
- Termination: race exit / `ctx.abort` / `sleep(timeout+100ms)` (`:540-546`). Abort or timeout → `handle.kill({forceKillAfter:"3 seconds"})`: POSIX SIGTERM to `-pid` group then SIGKILL after 3 s; win32 `taskkill /pid N /T /F` (`core/src/cross-spawn-spawner.ts:292-312`).
- Final output: `tail(raw, maxLines, maxBytes)` (`:225-255`, keeps last lines; a single over-long line is byte-cut on a UTF-8 boundary). If tail cut and no file yet → write `raw` to file. Empty → `"(no output)"`. If `cut && file` prefix `"...output truncated...\n\nFull output saved to: <file>\n\n"`. Append `"\n\n<shell_metadata>\n" + lines + "\n</shell_metadata>"` when timeout (`shell tool terminated command after exceeding timeout ${timeout} ms. If this command is expected to take longer and is not waiting for interactive input, retry with a larger timeout value in milliseconds.`) and/or abort (`User aborted the command`) (`:561-584`).
- Result: `title = command`; `metadata = {output: last || preview(output), exit: code|null, truncated: cut, outputPath?}`. **Exit code is NOT in model-facing output** — only `metadata.exit`. Non-zero exit is not an error. Abort/timeout still return success.
- No background-job support in the legacy bash tool (no param; `BackgroundJob` only used by `task`).

**v2 Δ** (`core/src/tool/bash.ts`): `MAX_TIMEOUT_MS=600000`, `MAX_CAPTURE_BYTES=1MiB` (`:19-21`); default shell `/bin/sh` or `COMSPEC` (`:49`); no tree-sitter/arity (TODOs `:66-77`); permission `resources:[command]`; model output adds `Command exited with code N.` (`:51-57`).

Rust mapping: `tree-sitter` + `tree-sitter-bash` + a PowerShell grammar crate (risk: grammar version must match node-type names `command_elements`, `command_parameter`, `command_argument_sep`, `redirected_statement`); `tokio::process` with `process_group(0)` + `killpg`; Windows `taskkill /T /F`; Node `shell:` option must be reproduced manually (`cmd /d /s /c` with verbatim args).

### 5.2 `task` (subagents)

**Parameters** (`task.ts:43-62`): `description` string ("A short (3-5 words)…"), `prompt` string, `subagent_type` string, `task_id?` string (resume), `command?` string, `background?` boolean. JSON schema sent to the model omits `background` unless `OPENCODE_EXPERIMENTAL_BACKGROUND_SUBAGENTS` (`task.ts:366`, uses `ToolJsonSchema.fromSchema(BaseParameters)`).

**Description** = `task.txt` (+ `"\n\n" + BACKGROUND_DESCRIPTION` if flag, `task.ts:362-364`) + `"\n"` + `describeTask(agent)` (`registry.ts:265-278, 325-331`): header `Available agent types and the tools they have access to:` then `- ${name}: ${description ?? "This subagent should only be called manually by the user."}` for agents with `mode !== "primary"` and `Permission.evaluate("task", name, callerAgent.permission).action !== "deny"`, sorted by `localeCompare`.

**Flow** (`task.ts:92-359`):
1. `background:true` without flag → error `Background subagents require OPENCODE_EXPERIMENTAL_BACKGROUND_SUBAGENTS=true`.
2. Depth = number of ancestors of calling session; if `depth >= (cfg.subagent_depth ?? 1)` → `Subagent depth limit reached (${n}). Increase "subagent_depth" to allow nested subagents.` (`:104-117`; config `core/src/v1/config/config.ts:84`). Default ⇒ subagents cannot spawn subagents.
3. Unless `ctx.extra.bypassAgentCheck` (set for user `@agent` parts / subtask parts, `opencode/src/session/prompt.ts:331,1223`): ask `permission:"task"`, `patterns:[subagent_type]`, `always:["*"]`, metadata `{description, subagent_type}`.
4. `agent.get(subagent_type)` missing → `Unknown agent type: ${t} is not a valid agent type`.
5. Session: `task_id` → `sessions.get` (errors swallowed → new session). New child: `sessions.create({parentID: ctx.sessionID, title: `${description} (@${agent.name} subagent)`, agent: name, permission})` where permission = `deriveSubagentSessionPermission` (parent rules with `permission=="external_directory"` or `action=="deny"`, plus `todowrite`/`task` `*` deny unless the subagent's own ruleset mentions them; `opencode/src/agent/subagent-permissions.ts:14-27`) + extra denies for each `cfg.experimental.primary_tools[]` (`task.ts:143-171`, deduped).
6. Model = `agent.model ?? {modelID, providerID}` of the calling assistant message; `variant` passed only if the agent has no model (`:174-210`).
7. `ctx.metadata({title: description, metadata:{parentSessionId, sessionId, model, background?}})`.
8. `runTask` (`:200-225`): `promptOps.resolvePromptParts(prompt)` then `promptOps.prompt({messageID: ascending, sessionID: child, model, variant, agent, parts})`. Fail if final assistant has `error` → `Subagent failed (task_id: ${id}): ${error.data.message ?? error.name}`; fail if **any** tool part in the returned message has `status:"error"` → same prefix + `state.error`. Success value = last `text` part's text or `""`.
9. All runs go through `BackgroundJob` (`core/src/background-job.ts`, process-local, not persisted, `:113-119`), job id = child session id, type `"task"`. If a running job with that id exists → `extend` (queues the new run) and return `BACKGROUND_UPDATED` (`:267-282`).
10. Foreground: race `background.wait` vs `waitForPromotion`; parent abort → `promptOps.cancel(child)`; interrupt → cancel + `background.cancel`. Promotion via HTTP `POST /experimental/session/:sessionID/background` (`opencode/src/server/routes/instance/httpapi/handlers/experimental.ts:159-171`) returns the "started" result immediately; completion later injects a **synthetic user text part** into the parent via `promptOps.prompt` (`inject`, `:227-254`) with summary `Background task completed: ${description}` / `Background task failed: ${description}`.

**Output format** (`renderOutput`, `task.ts:64-79`), lines joined by `\n`:
```
<task id="{childSessionID}" state="{running|completed|error}">
<summary>{summary}</summary>        ← only if summary
<task_result>                        ← <task_error> when state=error
{text}
</task_result>
</task>
```
Foreground success: state `completed`, no summary, text = child's last text. Background start: state `running`, summary `Background task started`, text `BACKGROUND_STARTED` (`task.ts:31-35`); update: summary `Background task updated`, `BACKGROUND_UPDATED` (`:36-41`). Result metadata adds `background:true, jobId`.

**v2 Δ**: no task tool in core (`core/src/tool/builtins.ts:26-27`).

Rust mapping: `promptOps` = trait object injected in tool ctx extras (`TaskPromptOps`, `task.ts:18-22`); background registry = `DashMap<SessionId, JobState>` with `tokio::sync::watch`; keep job non-durable.

### 5.3 `todowrite`

- Params (`todo.ts:6-8`): `todos: Todo.Info[]`; `Todo.Info` = `{content: string, status: string, priority: string}` (`schema/src/session-todo.ts:7-15`) — status/priority are free strings (descriptions only: `pending, in_progress, completed, cancelled` / `high, medium, low`).
- Ask `permission:"todowrite"`, patterns/always `["*"]`, metadata `{}` (`todo.ts:24-29`).
- Persist (`opencode/src/session/todo.ts:29-51`): in one transaction delete all `TodoTable` rows for session, insert rows `{session_id, content, status, priority, position=index}`; publish `todo.updated` `{sessionID, todos}` (`schema/src/session-todo.ts:18-24`).
- Result: title `${count(status !== "completed")} todos`; output `JSON.stringify(todos, null, 2)`; metadata `{todos}`.
- No `todoread` tool; reading is via HTTP `GET {root}/:sessionID/todo` (`opencode/src/server/routes/instance/httpapi/groups/session.ts:83`).
- **v2 Δ**: `core/src/tool/todowrite.ts` uses `PermissionV2.assert({action:"todowrite"})`.
- Rust mapping: `serde_json::to_string_pretty` (2-space) over a struct with field order `content,status,priority` (assumption: Effect Schema decode emits declared key order and drops unknown keys; verify by decoding `{priority,status,content,extra}` in bun).

### 5.4 `question`

- Params (`question.ts:6-8`): `questions: Question.Prompt[]`; `Prompt` = `{question: string, header: string, options: {label, description}[], multiple?: boolean}` (`schema/src/v1/question.ts:15-31`). The model cannot set `custom` (only `Info` has it; default true).
- No `ctx.ask`; calls `Question.Service.ask({sessionID, questions, tool:{messageID, callID}})` (service/events/HTTP reply: §6). Answers = `string[][]` of labels.
- Output (`question.ts:30-37`): `User has answered your questions: ` + join(`", "`) of `"${question}"="${answers[i].join(", ") || "Unanswered"}"` + `. You can now continue with the user's answers in mind.`; title `Asked ${n} question` + (`s` if n>1); metadata `{answers}`.
- Reject → `QuestionRejectedError` message `The user dismissed this question` (`opencode/src/question/index.ts:27-31`) → tool part error and `ctx.blocked = ctx.shouldBreak` (`opencode/src/session/processor.ts:200-202`) → loop stops (`processor.ts:694`).
- **v2 Δ**: `core/src/tool/question.ts` asserts `PermissionV2` action `question` (`:63-65`).

### 5.5 `skill`

- Params (`skill.ts:8-10`): `name: string`. Description does **not** list skills (the list is in the system prompt, §3/§7).
- `Skill.require(name)`; NotFound → defect `Skill "${name}" not found. Available skills: ${sorted.join(", ") || "none"}` (`opencode/src/skill/index.ts:77-79`).
- Ask `permission:"skill"`, patterns `[name]`, always `[name]`.
- Files: `rg --no-config --files --hidden --glob=!**/SKILL.md --glob=!**/.git/** .` in skill dir, first 10 (unsorted rg order) (`skill.ts:36-43`, `core/src/ripgrep.ts:187-199`).
- Output (`skill.ts:47-61`), `\n`-joined: `<skill_content name="{name}">`, `# Skill: {name}`, ``, `{content.trim()}`, ``, `Base directory for this skill: {dir}`, `Relative paths in this skill (e.g., scripts/, reference/) are relative to this base directory.`, `Note: file list is sampled.`, ``, `<skill_files>`, `<file>{abs path}</file>` lines, `</skill_files>`, `</skill_content>`. Title `Loaded skill: {name}`; metadata `{name, dir}`.
- **v2 Δ**: param description "from the available skills list" (`core/src/tool/skill.ts:18`).

### 5.6 `webfetch`

- Params (`webfetch.ts:13-22`): `url: string`; `format: "text"|"markdown"|"html"` default `markdown`; `timeout?: number` (seconds).
- URL must start `http://`/`https://` else `URL must start with http:// or https://`. Ask `permission:"webfetch"`, patterns `[url]`, always `["*"]`, metadata `{url, format, timeout}`.
- Timeout `min((timeout ?? 30) * 1000, 120000)` ms → `Request timed out` (`:50,92`). Max body 5 MiB (`MAX_RESPONSE_SIZE`, checked on `content-length` then actual bytes → `Response too large (exceeds 5MB limit)`).
- Headers (`:53-74`): browser `User-Agent` `Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36`; `Accept` per format (markdown `text/markdown;q=1.0, text/x-markdown;q=0.9, text/plain;q=0.8, text/html;q=0.7, */*;q=0.1`; text `text/plain;q=1.0, text/markdown;q=0.9, text/html;q=0.8, */*;q=0.1`; html `text/html;q=1.0, application/xhtml+xml;q=0.9, text/plain;q=0.8, text/markdown;q=0.7, */*;q=0.1`); `Accept-Language: en-US,en;q=0.9`. Non-2xx = error. Retry once with `User-Agent: opencode` iff 403 and header `cf-mitigated: challenge` (`:79-91`).
- Image (`mime` startsWith `image/` except `image/svg+xml`, `image/vnd.fastbidsheet`; `opencode/src/util/media.ts:11-13`): output `Image fetched successfully`, attachment `{type:"file", mime, url:"data:{mime};base64,..."}`.
- Else decode UTF-8; `markdown` + content-type contains `text/html` → Turndown (`headingStyle:"atx", hr:"---", bulletListMarker:"-", codeBlockStyle:"fenced", emDelimiter:"*"`, remove `script style meta link`, `:182-192`); `text` + html → htmlparser2 text extraction skipping `script style noscript iframe object embed` subtrees, `trim()` (`:158-180`); otherwise raw body. Title `${url} (${contentType})`; metadata `{}`; generic truncation applies (§4).
- Rust mapping: `reqwest` + an HTML→Markdown converter configured to match Turndown (high diff risk; consider embedding turndown via QuickJS if byte-exact output matters).

### 5.7 `websearch`

- Params (`websearch.ts:10-25`): `query: string`, `numResults?: number`, `livecrawl?: "fallback"|"preferred"`, `type?: "auto"|"fast"|"deep"`, `contextMaxCharacters?: number`.
- Enabled (`registry.ts:58-65`): providerID `opencode` or `opencode-go`, or `enableExa` (`OPENCODE_ENABLE_EXA`|`OPENCODE_EXPERIMENTAL_EXA`|`OPENCODE_EXPERIMENTAL`) or `enableParallel` (`OPENCODE_ENABLE_PARALLEL`|`OPENCODE_EXPERIMENTAL_PARALLEL`) (`runtime-flags.ts:31-39`).
- Provider (`websearch.ts:30-37`): env `OPENCODE_WEBSEARCH_PROVIDER` ∈ {exa, parallel} → that; else parallel flag → parallel; exa flag → exa; else `parseInt(checksum(sessionID), 36) % 2 == 0 ? exa : parallel`, `checksum` = FNV-1a 32-bit over UTF-16 code units rendered base36 (`core/src/util/encode.ts:22-30`) ⇒ parity of the FNV hash.
- Order: `ctx.metadata({title: `${label} "${query}"`, metadata:{provider}})` then ask `permission:"websearch"`, patterns `[query]`, always `["*"]`, metadata `{query,numResults,livecrawl,type,contextMaxCharacters,provider}`.
- Transport (`opencode/src/tool/mcp-websearch.ts`): HTTP POST JSON-RPC `{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":tool,"arguments":args}}`, `Accept: application/json, text/event-stream`, timeout 25 s (`${tool} request timed out`). Exa: URL `https://mcp.exa.ai/mcp` (+`?exaApiKey=` if `EXA_API_KEY`), tool `web_search_exa`, args `{query, type: type||"auto", numResults: numResults||8, livecrawl: livecrawl||"fallback", contextMaxCharacters?}`. Parallel: `https://search.parallel.ai/mcp`, tool `web_search`, args `{objective: query, search_queries:[query], session_id, model_name}` (model_name = `ctx.extra.model.api.id ?? model.id`, ≤100 chars), headers `User-Agent: opencode/${version}` + `Authorization: Bearer $PARALLEL_API_KEY` if set (`websearch.ts:54-81`).
- Response parse (`mcp-websearch.ts:22-41`): whole body as JSON if it starts with `{`, else each `data: ` SSE line; first `result.content[]` item with non-empty `text`.
- Result: output text or `No search results found. Please try a different query.`; title `${label}: ${query}` with label `Exa Web Search`/`Parallel Web Search`; metadata `{provider}`.
- **v2 Δ**: caps `numResults ≤ 20`, `contextMaxCharacters ≤ 50000`, response ≤ 256 KiB (`core/src/tool/websearch.ts:22-24`).

### 5.8 `lsp` (experimental)

- Params (`lsp.ts:23-35`): `operation` ∈ `goToDefinition, findReferences, hover, documentSymbol, workspaceSymbol, goToImplementation, prepareCallHierarchy, incomingCalls, outgoingCalls`; `filePath: string`; `line: int ≥1`; `character: int ≥1`; `query?: string`.
- Relative path joined to instance directory; `assertExternalDirectoryEffect` (`opencode/src/tool/external-directory.ts:15-45`: ask `external_directory` with `dir/*` pattern, metadata `{filepath, parentDir}`); ask `permission:"lsp"`, `["*"]`/`["*"]`, metadata varies by op (`:50-55`).
- Errors: `File not found: ${file}`, `No LSP server available for this file type.`; `touchFile(file,"document")`; position 0-based (`line-1`, `character-1`).
- Output `JSON.stringify(result, null, 2)` or `No results found for ${operation}`; title `${operation} ${relPath[:line:char]}` (relPath relative to worktree); metadata `{result}`. LSP client behavior: see other agent's spec.

### 5.9 `plan_exit` (experimental plan mode)

- Params: `{}` (`plan.ts:13`). Plan file: `Session.plan(info)` = `<worktree>/.opencode/plans/<time.created>-<slug>.md` if VCS project else `<data>/plans/...` (`opencode/src/session/session.ts:331-336`); shown relative to worktree.
- Asks a question (header `Build Agent`, `custom:false`, options `Yes`/`No` with fixed descriptions, `plan.ts:30-44`). `No` → `QuestionRejectedError` (blocks loop).
- `Yes`: writes a new **user** message `{agent:"build", model: last user model ?? provider.defaultModel()}` + synthetic text part `The plan at ${plan} has been approved, you can now edit files. Execute the plan` (`:53-69`). Output `User approved switching to build agent. Wait for further instructions.`, title `Switching to build agent`.
- Plan-mode system reminders (`opencode/src/session/prompt/plan-mode.txt`) are §3.

### 5.10 `execute` (code mode, experimental)

- Params `{code: string}` (`code-mode.ts:16-20`). Description = `Run a confined orchestration script with access to connected MCP tools.` + `\n` + `CodeMode.make({tools}).instructions()` catalog (`registry.ts:280-289`, `code-mode.ts:58-65`).
- Visible MCP tools = `Permission.visibleTools(mcp.tools(), merge(agent.permission, session.permission))`; grouped by server prefix (longest sanitized server name + `_`), exposed as `server.local` (`code-mode.ts:39-56`).
- Each child call: plugin `tool.execute.before`, ask `permission: <mcp tool key>` `["*"]`, MCP `callTool` (timeout per tool, `resetTimeoutOnProgress`), `isError` → error from text blocks, plugin `tool.execute.after`; callID `${ctx.callID}/${n}` (`:134-186`). Results projected (`projectMcpResult`, `:75-116`): `structuredContent` > joined text > `[N image(s)|file(s) attached to the result]` > null; binary blocks become attachments.
- Streams `metadata.toolCalls[{tool,status,input?}]`; output = string value or `JSON.stringify(value,null,2)` + optional `\n\nLogs:\n...`; abort → `Execution cancelled.`.
- Interpreter = workspace package `@opencode-ai/codemode` (`packages/codemode`): semantics `UNKNOWN` here (read `packages/codemode/src` to spec). Rust mapping: defer; gate behind same flag.

### 5.11 v2 tool output store (`core/src/tool-output-store.ts`)

- Defaults `MAX_LINES=2000`, `MAX_BYTES=51200`, retention 7 days, dir `<data>/tool-output` (`:13-17`); file `tool_<Identifier.ascending()>` written with flag `wx` (`:130`); marker `... output truncated; full content saved to ${path} ...` placed between head/tail preview (`:98-104,159`); cleanup hourly (`:176,203`). Legacy uses `opencode/src/tool/truncate.ts` instead (same dir name, §4).

### 5.12 Porting risks (this section)

1. Bash permission patterns depend on tree-sitter node types/text of two grammars + `BashArity` table; any grammar drift changes saved `always` rules and prompts.
2. Bash spawn relies on Node `shell:` option semantics and Windows default-shell order (pwsh > powershell > git-bash > cmd); output streaming emits a part update per chunk.
3. Byte-exact outputs: Turndown markdown, `JSON.stringify(…,null,2)` key order, `localeCompare` agent sort in task description, task `<task …>` envelope, question answer sentence.
4. Task/background jobs are process-local and interact with prompt queueing (`promptOps.prompt` on a busy parent session) — see §2 for queue semantics.

## 6. Permissions and questions

Executed path (TUI / SDK `/session/:id/message`): legacy `opencode/src/permission/index.ts` (service key `@opencode/Permission`, `:40`) + `opencode/src/question/index.ts`. The v2 services (`core/src/permission.ts`, `core/src/question.ts`) back only the `/api/...` v2 routes (§6.9).

### 6.1 Schema (source of truth: `schema/src/v1/permission.ts`, re-exported by `schema/src/permission-v1.ts:1`, `core/src/v1/permission.ts:4`)

| Item | Definition | Ref |
|---|---|---|
| `ID` | string, must start with `per`, brand `PermissionID`; generated `"per_" + ascending()` | `schema/src/v1/permission.ts:10-13` |
| `Action` | `"allow" \| "deny" \| "ask"` | `:16` |
| `Rule` | `{ permission: string, pattern: string, action: Action }` | `:19` |
| `Ruleset` | `Rule[]` (ordered; order = precedence) | `:24` |
| `Request` | `{ id, sessionID, permission, patterns: string[], metadata: Record<string,unknown>, always: string[], tool?: { messageID: string, callID: string } }` | `:27-35` |
| `Reply` | `"once" \| "always" \| "reject"` | `:38` |
| `ReplyBody` | `{ reply: Reply, message?: string }` | `:41` |
| `AskInput` | `Request.fields` + `id?` + `ruleset: Ruleset` | `:51` |
| `ReplyInput` | `{ requestID: ID, reply, message? }` | `:56` |
| `Approval` | `{ projectID, patterns: string[] }` (defined; no runtime use found in legacy path) | `:46` |
| Event `permission.asked` | payload = `Request` fields | `:61` |
| Event `permission.replied` | `{ sessionID, requestID, reply }` | `:62-65` |

`ascending()` ID algorithm (shared by all `per_`/`que_` IDs): `schema/src/identifier.ts:14-30` — 12 hex chars of `(timestamp_ms * 0x1000 + counter)` (48 low bits, big-endian), counter resets when ms changes, then 14 chars from `[0-9A-Za-z]` via `byte % 62`. Rust mapping: port verbatim (lexicographic order == creation order is relied upon).

Errors (`core/src/v1/permission.ts`):

| Tag | Fields | `message` (exact, becomes tool error text) | Ref |
|---|---|---|---|
| `PermissionRejectedError` | – | `The user rejected permission to use this specific tool call.` | `:7-11` |
| `PermissionCorrectedError` | `feedback` | `The user rejected permission to use this specific tool call with the following feedback: ${feedback}` | `:13-19` |
| `PermissionDeniedError` | `ruleset` | `The user has specified a rule which prevents you from using this specific tool call. Here are some of the relevant rules ${JSON.stringify(ruleset)}` | `:21-27` |
| `Permission.NotFoundError` | `requestID` | – (HTTP 404 mapping) | `:29-31` |

### 6.2 Config shape

`permission` config (`core/src/v1/config/permission.ts`):
- `Action` = `"ask"|"allow"|"deny"` (`:5`); `Object` = `Record<string, Action>` (pattern → action, `:8`); `Rule` = `Action | Object` (`:11`).
- Top-level value may be a bare `Action` → normalized to `{ "*": action }` (`:38-41`).
- Known keys typed (`:19-33`): `read, edit, glob, grep, list, bash, task, external_directory, lsp, skill` accept `Rule`; `todowrite, question, webfetch, websearch, doom_loop` accept only `Action`. Any other key (`StructWithRest`, `:35`) accepts `Rule` (MCP tool keys, `plan_enter`, `plan_exit`, `workflow_tool_approval`, …).
- **Key order is semantic**: parse uses `propertyOrder: "original"` (`:14-16`); rules are emitted in object-entry order and evaluated last-match-wins.

`fromConfig` (`opencode/src/permission/index.ts:186-198`): for each `[key, value]` in entry order: string → `{permission:key, pattern:"*", action}`; object → one rule per `[pattern, action]` with `expand(pattern)`. `expand` (`:178-184`): `~/x`→`$HOME/x`; `~`→home; `$HOME/x`→home+`/x`; `$HOMEx`→home+`x` (prefix strip of 5 chars). No other normalization.

Config-level transforms (`opencode/src/config/config.ts`):
- `mode.<name>` → merged into `agent.<name>` with `mode:"primary"` (`:550-557`).
- `OPENCODE_PERMISSION` env (JSON) → `mergeDeep(result.permission, JSON.parse(env))`; invalid JSON logged + skipped (`:559-565`).
- Legacy `tools: {name: bool}` → `perms[name] = bool ? "allow" : "deny"`, with `write|edit|patch` collapsed to `edit`; then `permission = mergeDeep(perms, permission)` (explicit `permission` wins on value; tool-derived keys keep first position) (`:567-578`).
- Same `tools` → permission normalization per agent: `core/src/v1/config/agent.ts:62-81` (`Object.assign(permission, agent.permission)` so explicit wins).
- assumption: remeda `mergeDeep` keeps destination key positions and appends new keys (JS spread semantics). Verify: remeda `mergeDeep` source.
- JS caveat: `Object.entries` orders integer-like keys (e.g. pattern `"123"`) first ascending. Rust mapping: use `IndexMap` + serde_json `preserve_order`; reproduce the integer-key quirk only if bit-exact parity is required.

### 6.3 Wildcard matching (`core/src/util/wildcard.ts:3-14`, used by legacy `permission/index.ts:4`)

```
match(input, pattern):
  input'   = input.replaceAll("\\", "/")
  p        = pattern.replaceAll("\\", "/")
  p        = escape chars [.+^${}()|[\]\\] with "\"
  p        = replace every "*" -> ".*" ; every "?" -> "."
  if p ends with " .*": p = p[..-3] + "( .*)?"      # "ls *" matches "ls" and "ls -la"
  regex "^" + p + "$", flags: "s" (dotAll); on win32 "si" (case-insensitive)
```
- No `**` semantics (`**` == `.*.*`), no char classes (`[` escaped), `*` crosses `/`.
- Both `permission` key and `pattern` are matched with this (rule.permission may itself be a wildcard, e.g. `"*"`).
- `opencode/src/util/wildcard.ts:3-57` is an older copy (same `match`, plus `all`/`allStructured` with length-sorted patterns); only used in `opencode/src/session/llm.ts:150`.
- Rust mapping: build `regex::Regex` with `(?s)` and `(?i)` on Windows; cache compiled patterns. Regex crate syntax differs from JS only in escapes already covered; `( .*)?` is fine.

### 6.4 Evaluate + rule precedence

`evaluate(permission, pattern, ...rulesets)` (`opencode/src/permission/index.ts:28-38`): flatten rulesets in order, `findLast` rule where `match(permission, rule.permission) && match(pattern, rule.pattern)`; default `{action:"ask", permission, pattern:"*"}`. **Last match wins.**

Ruleset assembly (order = precedence low→high):
1. Agent ruleset = `defaults` → agent built-in overrides → user `cfg.permission` → `cfg.agent.<name>.permission` → auto `external_directory: {<Truncate.GLOB>: allow}` (§7.1).
2. Tool calls: `Permission.merge(agent.permission, session.permission ?? [])` (`opencode/src/session/tools.ts:87`) — session rules override agent.
3. `approved` (in-memory "always" rules) appended last inside `ask` (`index.ts:73`).
- `session.permission` is persisted on the session row (`Session.setPermission`, `opencode/src/session/session.ts:778-785`). `PromptInput.tools` `{name: bool}` **replaces** it with `{permission:name, pattern:"*", action}` rules (`opencode/src/session/prompt.ts:1060-1067`).

Tool visibility (tool removed from the LLM request, not asked): `Permission.disabled(tools, ruleset)` (`index.ts:204-214`): map tool id → permission key (`edit|write|apply_patch`→`edit`; `list_mcp_resources|list_mcp_resource_templates|read_mcp_resource`→`read`; else id); `findLast` rule matching **only the permission key**; hidden iff that rule has `pattern === "*"` and `action === "deny"`. Applied in `opencode/src/session/llm/request.ts:209-215` with `merge(agent.permission, session.permission)` and also drops tools where `user.tools[k] === false`. `visibleTools` (`:216-219`) used for MCP tools (`opencode/src/tool/registry.ts:286`).

### 6.5 Ask / reply flow (legacy, `opencode/src/permission/index.ts`)

State (per instance directory via `InstanceState`, **shared across sessions**, not persisted): `pending: Map<ID,{info, deferred}>`, `approved: Rule[]` (`:23-26`).

`ask(input)` (`:67-107`):
1. For each `pattern` in `input.patterns` (in order): `rule = evaluate(permission, pattern, input.ruleset, approved)`.
   - `deny` → fail immediately `DeniedError{ruleset: input.ruleset.filter(r => match(permission, r.permission))}` (even if earlier patterns needed ask).
   - `allow` → next; `ask` → `needsAsk = true`.
2. No ask needed → return.
3. `id = input.id ?? ID.ascending()`; `Request{id, sessionID, permission, patterns, metadata, always, tool}`; insert pending; publish `permission.asked`; await deferred; `ensuring` removes pending (interrupt/abort removes silently — **no `permission.replied` emitted**).

`reply({requestID, reply, message?})` (`:109-167`):
- unknown id → `Permission.NotFoundError`.
- delete pending; publish `permission.replied {sessionID, requestID, reply}`.
- `reject`: fail with `CorrectedError{feedback: message}` if `message` truthy else `RejectedError`; then **every other pending request of the same session** is removed, `permission.replied{reply:"reject"}` published, failed with `RejectedError`.
- `once`: resolve, done.
- `always`: resolve; push `{permission, pattern, action:"allow"}` for each `info.always` entry into `approved`; then for each other pending request of the same session, if **all** its patterns evaluate to `allow` against `approved` alone → remove, publish `permission.replied{reply:"always"}`, resolve.

Instance dispose finalizer: all pending fail with `RejectedError`, no events (`:54-61`).

Who asks what (permission key → patterns / always) — details in §4/§5:

| Key | Source | patterns | always |
|---|---|---|---|
| `bash` (`ShellID.ToolID`, `opencode/src/tool/shell/id.ts:16`) | `opencode/src/tool/shell.ts:283-290` | full source text of each command node | `BashArity.prefix(tokens).join(" ") + " *"` (`shell.ts:408-409`) |
| `external_directory` | `opencode/src/tool/external-directory.ts:36`, `shell.ts:270` | dir globs | same |
| `edit` | `edit.ts:103,146`, `write.ts:55`, `apply_patch.ts:207` | see §4 | see §4 |
| `read`, `glob`, `grep`, `lsp`, `webfetch`, `websearch`, `todowrite`, `skill` | `read.ts:256`, `glob.ts:29`, `grep.ts:40`, `lsp.ts:57`, `webfetch.ts:40`, `websearch.ts:120`, `todo.ts:25`, `skill.ts:28` (all under `opencode/src/tool/`) | tool-specific (skill: `[name]`) | tool-specific |
| `task` (tool id) | `opencode/src/tool/task.ts:120-128` (skipped if `extra.bypassAgentCheck`) | `[subagent_type]` | `["*"]` |
| MCP tool key | `opencode/src/session/tools.ts:408`, `opencode/src/tool/code-mode.ts:147` | `["*"]` | `["*"]` |
| `read` (MCP resources) | `session/tools.ts:181,264,344` | `mcp:<server>:*` | same |
| `doom_loop` | `opencode/src/session/processor.ts:372-379`: last `DOOM_LOOP_THRESHOLD=3` (`:29`) parts are tool calls of same name with identical `JSON.stringify(input)`; ruleset = `agent.permission` only | `[toolName]` | `[toolName]` |
| `workflow_tool_approval` | `opencode/src/session/llm.ts:186-195`, `ruleset: []` (always asks) | tool names | same |
| `question`, `plan_enter`, `plan_exit` | never asked; only gate visibility via `disabled()` | – | – |

Bash arity table: `opencode/src/permission/arity.ts:24-161` (136 entries, e.g. `git:2`, `npm:2`, `"npm run":3`, `docker:2`, `"docker compose":3`); `prefix(tokens)` (`:1-9`): longest prefix present in table → first `arity` tokens; none → first token; empty → `[]`. Rust mapping: copy table verbatim into a `phf::Map`.

### 6.6 Effect on the loop

- `ctx.ask` errors are converted with `Effect.orDie` (`session/tools.ts:89`); tool part goes to `state.status:"error"` with `error = errorMessage(err)` (`opencode/src/session/processor.ts:185-205`; `errorMessage` = `tui/src/util/error.ts:136-156`: `err.message` → `err.name` → `data.message` → `String(err)` → `"unknown error"`).
- `PermissionRejectedError` or `QuestionRejectedError` → `ctx.blocked = ctx.shouldBreak` (`processor.ts:200-202`); `shouldBreak = config.experimental.continue_loop_on_deny !== true` (`:647`); after the stream: `blocked || assistant.error` → `"stop"` (`:694`). Config key: `core/src/v1/config/config.ts:179`.
- `CorrectedError` and `DeniedError` do **not** block: error text is returned to the model and the loop continues.

### 6.7 Questions (`opencode/src/question/index.ts`, schema `schema/src/v1/question.ts`)

| Item | Definition | Ref |
|---|---|---|
| `ID` | prefix `que`, brand `QuestionID`, `"que_" + ascending()` | `schema/src/v1/question.ts:10-13` |
| `Option` | `{ label, description }` | `:15-18` |
| `Info` | `{ question, header, options: Option[], multiple?: bool, custom?: bool }` (custom default true per description) | `:20-30` |
| `Prompt` | `Info` without `custom` | `:31` |
| `Tool` | `{ messageID, callID }` | `:32` |
| `Request` | `{ id, sessionID, questions: Info[], tool?: Tool }` | `:35-40` |
| `Answer` | `string[]` (selected labels / custom text) | `:41` |
| `Reply` | `{ answers: Answer[] }` (in question order) | `:42-46` |
| Events | `question.asked` (Request), `question.replied` `{sessionID, requestID, answers}`, `question.rejected` `{sessionID, requestID}` | `:58-66` |

Service (`opencode/src/question/index.ts`): per-instance `pending` map (`:42-44`). `ask` → new id, publish `question.asked`, await, `ensuring` delete (`:87-112`). `reply` → NotFound if missing; delete; publish `question.replied`; resolve with answers (`:114-132`). `reject` → publish `question.rejected`; fail `QuestionRejectedError` (message `The user dismissed this question`, `:27-31`) (`:134-148`). Unlike permissions, reject does **not** cascade to other pending questions. Dispose → all pending rejected, no events (`:74-81`). Callers: `opencode/src/tool/question.ts:24`, `opencode/src/tool/plan.ts:30` (§5).

### 6.8 HTTP surface (legacy instance API)

| Method / path | Body | Handler | Ref |
|---|---|---|---|
| `GET /permission` | – → `Request[]` (all sessions) | `svc.list()` | `opencode/src/server/routes/instance/httpapi/groups/permission.ts:21`, `handlers/permission.ts:12` |
| `POST /permission/:requestID/reply` | `{reply, message?}` → `true`; 404 `PermissionNotFoundError` | `svc.reply` | `groups/permission.ts:31`, `handlers/permission.ts:16-37` |
| `POST /session/:sessionID/permissions/:permissionID` (deprecated, `permission.respond`) | `{response: Reply}` (no message) | `svc.reply` | `groups/session.ts:74-76,101,395-410`, `handlers/session.ts:362-377` |
| `GET /question` | → `Request[]` | | `groups/question.ts:22` |
| `POST /question/:requestID/reply` | `{answers: string[][]}` → `true` | | `groups/question.ts:12-15,32` |
| `POST /question/:requestID/reject` | – → `true` | | `groups/question.ts:45` |

All routes accept `WorkspaceRoutingQuery` (`?directory=`/workspace routing) and require `Authorization` middleware. Clients learn about requests via the event stream (`permission.asked` / `question.asked`) — event envelope transport documented in §1.

### 6.9 v2 runtime (secondary; `/api/...` routes)

- `core/src/permission.ts`: rule shape `{action, resource, effect}` (`schema/src/permission.ts:57-62`); request `{id, sessionID, action, resources[], save?, metadata?, source?: {type:"tool", messageID, callID}}` (`:16-37`); events `permission.v2.asked` / `permission.v2.replied` (`:43-51`). Errors `PermissionV2.DeclinedError|CorrectedError|BlockedError|NotFoundError` (`core/src/permission.ts:60-72`).
- Evaluate: agent rules first checked alone for deny (`:147-162`), then `[...agentRules, ...savedRules]`; per-resource effects reduced deny > ask > allow. Missing agent → `[{action:"*",resource:"*",effect:"deny"}]` (`:15`).
- `always` persists `request.save` to SQLite table `permission` (`id, project_id, action, resource` + timestamps, unique `(project_id, action, resource)`, insert `onConflictDoNothing`) — `core/src/permission/sql.ts:6-19`, `core/src/permission/saved.ts:54-69`; scoped per project (all sessions). DDL documented by DB spec.
- Reject cascades to same-session pending like legacy (`:231-247`); `DeclinedError` from `assert` is turned into a defect (`:209`).
- Routes: `protocol/src/groups/permission.ts:23-119` (`/api/permission/request`, `/api/permission/saved[/:id]`, `/api/session/:sessionID/permission[/:requestID[/reply]]`); questions `protocol/src/groups/question.ts:20-68`; v2 question events `question.v2.*` (`schema/src/question.ts:70-79`).
- `core/src/policy.ts`: `{action, effect: allow|deny, resource}` statements from `experimental.policies` (`core/src/config.ts:210`), `findLast` wildcard match with fallback (`:36-42`); only consumer: `provider.use` in `core/src/catalog.ts:163`. Not part of the tool permission path.

Rust mapping:
- `enum PermissionReply { Once, Always, Reject }`; pending map `HashMap<PermissionId, (Request, oneshot::Sender<Result<(), PermError>>)>` behind a per-instance `Mutex`; `approved: Vec<Rule>` per instance (not per session).
- Use a drop-guard to remove pending on cancellation without emitting `permission.replied` (parity).
- Keep error `message` strings byte-identical; they are persisted into tool part `state.error`.

## 7. Agents, commands, skills

### 7.1 Agent model (`opencode/src/agent/agent.ts`)

`Agent.Info` (`:35-55`): `name, description?, mode: "subagent"|"primary"|"all", native?, hidden?, topP?, temperature?, color?, permission: Ruleset, model?: {modelID, providerID}, variant?, prompt?, options: Record<string,unknown>, steps?`.

Default ruleset `defaults` (`:119-136`), in order:
`"*": allow` · `doom_loop: ask` · `external_directory: {"*": ask, <whitelist>: allow}` · `question: deny` · `plan_enter: deny` · `plan_exit: deny` · `read: {"*": allow, "*.env": ask, "*.env.*": ask, "*.env.example": allow}`.
Whitelist (`:108-113`): `Truncate.GLOB` (= `<Global.Path.data>/tool-output/*`, `opencode/src/tool/truncate.ts:17`, `truncation-dir.ts`), `<Global.Path.tmp>/*` (`os.tmpdir()/opencode`, `core/src/global.ts:15`), `<skillDir>/*` for every discovered skill dir, `<reference.path>/*` for configured references.
`user = fromConfig(cfg.permission ?? {})` (`:138`).

Built-in agents (prompt `undefined` = provider/system prompt chosen in §3):

| name | mode | native | hidden | extra rules (between `defaults` and `user`) | prompt file | other | Ref |
|---|---|---|---|---|---|---|---|
| `build` | primary | ✓ | – | `question: allow, plan_enter: allow` | – | description "The default agent. Executes tools based on configured permissions." | `:141-155` |
| `plan` | primary | ✓ | – | `question: allow, plan_exit: allow, task: {general: deny}, external_directory: {<data>/plans/*: allow}, edit: {"*": deny, ".opencode/plans/*.md" (path.join): allow, relative(worktree, <data>/plans/*.md): allow}` | – | "Plan mode. Disallows all edit tools." | `:156-181` |
| `general` | subagent | ✓ | – | `todowrite: deny` | – | description `:184` | `:182-195` |
| `explore` | subagent | ✓ | – | `"*": deny, grep, glob, list, bash, webfetch, websearch, read: allow, external_directory: readonlyExternalDirectory` (= `{"*":ask, whitelist: allow}`) | `opencode/src/agent/prompt/explore.txt` | description `:213` | `:196-218` |
| `compaction` | primary | ✓ | ✓ | `"*": deny` | `opencode/src/agent/prompt/compaction.txt` | | `:219-233` |
| `title` | primary | ✓ | ✓ | `"*": deny` | `opencode/src/agent/prompt/title.txt` | `temperature: 0.5` | `:234-249` |
| `summary` | primary | ✓ | ✓ | `"*": deny` | `opencode/src/agent/prompt/summary.txt` | | `:250-264` |

Agent-generate prompt: `opencode/src/agent/generate.txt` (used by `Agent.generate`, `:368-436`: temperature 0.3, user message template `:408`, structured output `{identifier, whenToUse, systemPrompt}` `:58-62`; OpenAI OAuth uses `streamObject` with `instructions` provider option and no system message).

Config merge (`:267-294`) for each `cfg.agent[key]` (entry order):
- `disable: true` → delete agent (built-ins too).
- unknown key → new agent `{name:key, mode:"all", permission: merge(defaults, user), options:{}, native:false}`.
- Overwrite-if-defined: `model` (via `Provider.parseModel` "provider/model"), `variant, prompt, description, temperature, top_p→topP, mode, color, hidden, name, steps`; `options = mergeDeep(options, value.options)`; `permission = merge(item.permission, fromConfig(value.permission))` (appended → wins).
- `ConfigAgentV1.Info` normalization (`core/src/v1/config/agent.ts:43-81`): unknown frontmatter/config keys moved into `options`; `tools` → permission (see §6.2); `steps ?? maxSteps`. `color`: `#RRGGBB` or theme literal (`:7-10`).
- Post-pass (`:296-310`): every agent gets `external_directory: {<Truncate.GLOB>: allow}` appended unless it has an explicit `external_directory` deny rule with pattern exactly `Truncate.GLOB`.

Selection:
- `list()` (`:316-326`): sorted by (`name === cfg.default_agent` or `"build"` if unset) desc, then name asc.
- `defaultInfo()` (`:328-340`): `cfg.default_agent` must exist, not `subagent`, not `hidden` (throws `default agent "<x>" not found|is a subagent|is hidden`); else first agent in insertion order with `mode !== "subagent" && hidden !== true`; else throws `no primary visible agent found`.

Markdown agents (`opencode/src/config/agent.ts`):
- `load(dir)`: glob `{agent,agents}/**/*.md` (dot, symlink) in each config dir (`:13-18`); name = path relative to dir with leading `agent/` or `agents/` stripped and extension removed — nested dirs keep `/` (e.g. `agents/team/rev.md` → `team/rev`) (`opencode/src/config/entry-name.ts:8-19`); config = `{name, ...frontmatter, prompt: body.trim()}` (`:24-28`); parse failure → skipped; schema failure → error via `ConfigParse.schema`.
- `loadMode(dir)`: `{mode,modes}/*.md` (non-recursive) → forced `mode:"primary"`; invalid entries silently dropped (`:34-58`).
- Config dirs (`opencode/src/config/paths.ts:23-41`), in merge order (later `mergeDeep` wins): `Global.Path.config` (`$XDG_CONFIG_HOME/opencode`), every `.opencode` dir from cwd upward to worktree (nearest first; skipped if `OPENCODE_DISABLE_PROJECT_CONFIG`), `~/.opencode`, `OPENCODE_CONFIG_DIR`. Loop: `opencode/src/config/config.ts:438-475` (commands, agents, modes merged per dir).
- Frontmatter parser: gray-matter; on YAML error retried after `sanitize` that rewrites `key: value-with-colon` lines to `key: |-` block scalars (`core/src/config/markdown.ts:4-36`). Rust mapping: `serde_yaml` + port `sanitize` exactly.

Subagent sessions (`opencode/src/agent/subagent-permissions.ts:14-27`, `opencode/src/tool/task.ts:139-171`): child `session.permission` = parent session rules where `permission === "external_directory"` or `action === "deny"`, + `todowrite: deny` unless subagent ruleset mentions `todowrite`, + `task: deny` unless it mentions `task`, + `experimental.primary_tools[]` each `deny` (`task.ts:150-154`; config `core/src/v1/config/config.ts:176`), de-duplicated. Depth limit `cfg.subagent_depth ?? 1` (`task.ts:104-117`). Child title `"<description> (@<agent> subagent)"` (`:160`).

v2 (`core/src/agent.ts`): agents are injected by plugins via `transform`; default = configured default if selectable, else `build`, else first selectable (`:67-79`); not used by the legacy loop.

Rust mapping: `struct AgentInfo` with `permission: Vec<Rule>` built exactly in the order above; keep `IndexMap` insertion order for `defaultInfo` fallback.

### 7.2 Commands (`opencode/src/command/index.ts`)

`Command.Info` (`:22-34`): `name, description?, agent?, model?, source?: "command"|"mcp"|"skill", template: string|Promise<string>, subtask?, hints: string[]`. Event `command.executed {name, sessionID, arguments, messageID}` (`schema/src/v1/legacy-event.ts:8-16`).

Registry build order (later overwrites earlier, except skills):
1. `init` — description `guided AGENTS.md setup`, template `opencode/src/command/template/initialize.txt` with first `${path}` → worktree (`:70-78`).
2. `review` — description `review changes [commit|branch|pr], defaults to uncommitted`, template `opencode/src/command/template/review.txt` (`${path}` → worktree), `subtask: true` (`:79-88`).
3. `cfg.command[name]` (`:90-103`) — config schema `{template, description?, agent?, model?, variant?, subtask?}` (`core/src/v1/config/command.ts:5-12`; note `variant` is not copied into `Command.Info`).
4. MCP prompts (`:105-132`): key `sanitize(client) + ":" + sanitize(prompt.name)` (`opencode/src/mcp/catalog.ts:103-108`); template lazily resolved by calling `getPrompt(client, name, {argName_i: "$<i+1>"})` and joining text contents with `\n`; hints `$1..$n`.
5. Skills (`:134-152`) only if name not taken; template = skill content, or for on-disk skills `content + "\n\nBase directory for this skill: <dir>\nRelative paths in this skill (e.g., scripts/, references/) are relative to this base directory."`.

`hints(template)` (`:36-44`): unique `$\d+` matches sorted lexicographically (`$10` < `$2`), then `$ARGUMENTS` if present.

Markdown commands (`opencode/src/config/command.ts:13-39`): glob `{command,commands}/**/*.md` per config dir; name via `configEntryNameFromPath(..., ["command/","commands/"])`; `{name, ...frontmatter, template: body.trim()}`; schema failure → **throws** `InvalidError` (unlike agents/modes).

HTTP: `GET /command`, `GET /agent`, `GET /skill` (`opencode/src/server/routes/instance/httpapi/groups/instance.ts:51-53`); `POST /session/:sessionID/command` (`groups/session.ts:97,343`).

### 7.3 Command execution / templating (`SessionPrompt.command`, `opencode/src/session/prompt.ts:1356-1484`)

Input `CommandInput` (`:1540-1560`): `{messageID?, sessionID, agent?, model?: "provider/model", arguments: string, command, variant?, parts?: FilePartInput[]}`.

1. Unknown command → publish `session.error` with `NamedError.Unknown("Command not found: \"<cmd>\". Available commands: a, b")` and throw (`:1362-1368`).
2. `agentName = cmd.agent ?? input.agent`.
3. Args: `input.arguments.match(/(?:\[Image\s+\d+\]|"[^"]*"|'[^']*'|[^\s"']+)/gi)`, each stripped by `/^["']|["']$/g` (`:1372-1373`, regexes `:1592-1596`).
4. `template = await cmd.template`. `last` = max N over `/\$(\d+)/g`. Replace each `$N`: index ≥ args.length → `""`; `N === last` → `args.slice(N-1).join(" ")` (last placeholder swallows the rest); else `args[N-1]` (`:1376-1389`).
5. `$ARGUMENTS` → raw `input.arguments` (all occurrences) (`:1390-1391`).
6. No `$N` and no `$ARGUMENTS` and `arguments.trim()` non-empty → append `"\n\n" + arguments` (`:1393-1395`).
7. Shell substitution: every `` !`cmd` `` (`/!`([^`]+)`/g`) executed concurrently with `Process.text([cmd], {shell: Shell.preferred(cfg.shell), nothrow: true})`; replaced by **stdout only** (`opencode/src/util/process.ts:165-171`), in match order (`:1397-1408`). No `cwd` is passed (verified `prompt.ts:1403`, `opencode/src/util/process.ts:114-124`) → Node default = server `process.cwd()`, not the instance directory (differs when the server hosts several directories).
8. `template.trim()`.
9. Model: `cmd.model` → `agents.get(cmd.agent).model` → `input.model` → session current model (`:1411-1419`); validated via `getModel`.
10. Agent resolve: `agentName ? get(agentName) : defaultInfo()`; missing → `session.error` "Agent not found: …" + throw (`:1423-1430`).
11. `resolvePromptParts(template)` (`:157-191`): parts = `[{type:"text", text: template}]` + for each unique `@ref` matched by `FILE_REGEX = /(?<![\w`])@(\.?[^\s`,.]*(?:\.[^\s`,.]+)*)/g` (`opencode/src/config/markdown.ts:5`): path = `~/x` → home, else `path.resolve(worktree, name)`; if stat fails and an agent named `name` exists → `{type:"agent", name}`; else (exists) `{type:"file", url: file://…, filename: name, mime: "application/x-directory" | "text/plain"}`. Template file parts duplicating an input `file:` part are dropped (`:1433-1438`).
12. Subtask iff `(agent.mode === "subagent" && cmd.subtask !== false) || cmd.subtask === true` (`:1439`). Then parts = single `{type:"subtask", agent, description: cmd.description ?? "", command, model, prompt: <first text part>}`; the user message is attributed to `input.agent ?? default agent` and `input.model ?? current model`. Otherwise parts = template parts + `input.parts`, user agent = resolved agent, model = taskModel (`:1440-1458`).
13. Plugin hook `command.execute.before` `{command, sessionID, arguments}` / `{parts}` (`:1460-1464`); `prompt({...})`; publish `command.executed` (`:1474-1479`).

Rust mapping: implement with `regex` (no look-behind support → use `fancy-regex` for `FILE_REGEX` or hand-written scanner). Placeholder replacement is single-pass over the original template (replacement text is not re-scanned).

### 7.4 Skills (`opencode/src/skill/index.ts`, `opencode/src/skill/discovery.ts`)

`Skill.Info` (`:37-42`): `{name, description?, location, content}`. Built-in `customize-opencode` (location `<built-in>`, body file `core/src/plugin/skill/customize-opencode.md` (via `core/src/plugin/skill.ts:9-11`), description `:33-34`) registered first; disk skills with same name override (`:276-283`).

Discovery order (`discoverSkills`, `:173-233`):
1. Unless `OPENCODE_DISABLE_EXTERNAL_SKILLS`: external dirs `.claude` (skipped if `OPENCODE_DISABLE_CLAUDE_CODE` or `OPENCODE_DISABLE_CLAUDE_CODE_SKILLS`, `opencode/src/effect/runtime-flags.ts:21,27-30`) and `.agents`:
   - global: `~/.claude`, `~/.agents` → glob `skills/**/SKILL.md` (dot) (`:190-194`);
   - project: every `.claude`/`.agents` dir from cwd up to worktree (`FSUtil.up`) → same glob (`:196-202`).
2. Every config dir (§7.1) → `{skill,skills}/**/SKILL.md` (`:205-208`).
3. `cfg.skills.paths[]` (`core/src/v1/config/skills.ts:5-12`): `~/` expanded, relative → `path.join(cwd, p)`; must be a dir; glob `**/SKILL.md` (`:211-220`).
4. `cfg.skills.urls[]` → `Discovery.pull(url)` → glob `**/SKILL.md` in each pulled dir (`:222-227`).

Loading (`add`, `:105-140`; run with unbounded concurrency `:240-243`): parse frontmatter; error → publish `session.error` (`NamedError.Unknown`) and skip; require `name: string` and `description: string|undefined` else silently skip; **no name/dir validation** (`NameMismatchError` declared `:67-71`, unused). Duplicate name → warning, later write wins (order nondeterministic due to concurrency). `content` = markdown body (untrimmed).

Remote pull (`discovery.ts:49-132`): `GET <url>/index.json` → `{skills: [{name, files[], version?}]}`; entries without `SKILL.md` in `files` skipped; cache root `<Global.Path.cache>/skills/<name>`; if `version` undefined or equals `.opencode-version` file → download each missing file (`<url-base>/<name>/<file>`, existing files never refreshed); else download into `<root>.tmp-<uuid>`, require all ok + `SKILL.md`, write `.opencode-version`, atomic swap via `<root>.old-<uuid>` backup. Concurrency 4 skills × 8 files; transient read retry.

Exposure to the model:
- System prompt section (`opencode/src/session/system.ts:107-119`): omitted if `disabled(["skill"], agent.permission)`; else lines `Skills provide specialized instructions and workflows for specific tasks.` / `Use the skill tool to load a skill when a task matches its description.` + `Skill.fmt(list, {verbose:true})`.
- `available(agent)` (`:310-315`): sorted by `localeCompare`, filtered by `evaluate("skill", name, agent.permission).action !== "deny"` (agent ruleset only, not session).
- `fmt` (`:321-346`): skills without description excluded; none → `No skills are currently available.`; verbose → `<available_skills>` / `  <skill>` / `    <name>…</name>` / `    <description>…</description>` / `    <location>${escapeHtml(location)}</location>` / `  </skill>` / `</available_skills>`; non-verbose → `## Available Skills` + `- **name**: description`.
- Skill tool (`opencode/src/tool/skill.ts`, description `opencode/src/tool/skill.txt`): asks `skill` with `patterns/always = [name]` (`:27-32`); output format in §5.
- Skills also become slash commands (§7.2 step 5).

v2 (`core/src/skill.ts:73-117`): sources `embedded|directory|url`; glob `{*.md,**/SKILL.md}`; name = frontmatter `name` or top-level file basename; extra frontmatter `slash: bool`; sorted file order, last source wins. Not used by legacy loop.

Rust mapping: `globset` with dot-files enabled and symlink following; replicate `localeCompare` ordering with a locale-aware collator (`icu_collator`) or accept ASCII ordering as a documented deviation; run skill parsing sequentially in discovery order and document the deterministic tie-break.

## 8. Core v2 runtime (`/api/session/*`) — secondary, incomplete

Only needed for parity with the v2 HTTP API (`protocol/src/groups/session.ts:109-360`). Not driven by TUI/CLI in v1.19.5 (section 0).

### 8.1 Components

| Piece | Ref | Notes |
|---|---|---|
| `SessionV2.Service` | `core/src/session.ts:113-180` | list/create/get/messages/context/events/history/prompt/switchAgent/switchModel/resume/interrupt/revert.{stage,clear,commit}; `shell`/`skill`/`compact`/`wait` -> `Session.OperationUnavailableError{operation}` (`:387-424`) |
| Prompt admission | `core/src/session.ts:360-386`, `core/src/session/input.ts` | Prompt recorded as `SessionInput.Admitted{admittedSeq,id,sessionID,prompt,delivery,timeCreated,promotedSeq?}` (`schema/src/session-input.ts:16-24`) in `SessionInputTable` (`core/src/session/sql.ts:140`); `delivery` ∈ `"steer" \| "queue"` (`schema/src/session-delivery.ts:5`); conflicting re-submit of same messageID -> `Session.PromptConflictError` (`core/src/session.ts:104`); `resume !== false` -> `execution.wake` (`:382`) |
| Execution routing | `core/src/session/execution.ts:8-19`, `execution/local.ts:9-36` | `SessionRunCoordinator` per session key: one active drain, `run` joins, `wake` coalesces one follow-up (`pendingWake`), `interrupt` stops and awaits cleanup (`core/src/session/run-coordinator.ts:5-21`) |
| Runner | `core/src/session/runner/llm.ts:93-420` | Loop: drain while steer/queue inputs exist or `force` (`:398-402`); per step: reload projected context, `toLLMMessages` (`runner/to-llm-message.ts`), tools `ToolRegistry.materialize(agent.permissions)`, `llm.stream(request)` via `@opencode-ai/llm` |
| Step limit | `runner/llm.ts:202,220,254` | `isLastStep = agent.info.steps !== undefined && currentStep >= steps`; on last step no tools and `MAX_STEPS_PROMPT` (`runner/max-steps.ts`) appended **as an assistant message**; tool calls on last step failed with `"Tools are disabled after the maximum agent steps"` |
| Compaction | `core/src/session/compaction.ts:12-15,232-243` | constants `DEFAULT_BUFFER=20_000`, `DEFAULT_KEEP_TOKENS=8_000`, `TOOL_OUTPUT_MAX_CHARS=2_000`, `SUMMARY_OUTPUT_TOKENS=4_096`; `compactIfNeeded`: if `config.auto` and `estimate(system,messages,tools) > context - max(output, buffer)` -> compact; on provider context-overflow failure: compact once then retry (second overflow = defect); trigger call at `runner/llm.ts:224` |
| Tool materialization | `core/src/tool/registry.ts:106-122,130-133` | A tool is dropped only if the **last** rule whose `action` wildcard-matches the tool's permission key has `resource === "*"` and `effect === "deny"`; unknown tool call -> result `{type:"error", value:"Unknown tool: <name>"}` |
| Built-in v2 tools | `core/src/tool/builtins.ts:31-48` | apply_patch, bash, edit, glob, grep, question, read, skill, todowrite, webfetch, websearch, write. Missing vs legacy: task, lsp, plan_exit, code mode (TODO `:26-29`) |
| Interrupted tools | `runner/llm.ts:119-131,301,350` | On run start, unsettled tool calls in history are failed with `{type:"unknown", message:"Tool execution interrupted"}`; provider-executed tools missing a result -> `"Provider did not return a tool result"` |

### 8.2 v2 data model (summary)

`SessionMessage.Message` tagged union on `type` (`schema/src/session-message.ts:200-212`): `agent-switched`, `model-switched`, `user`, `synthetic`, `system`, `shell`, `assistant` (content: `text`/`reasoning`/`tool`; tool `state.status` ∈ pending/running/completed/error, `:82-140`), `compaction` (`reason: "auto"|"manual"`, `summary`, `recent`, `:192-198`). Assistant carries `agent, model, content[], snapshot{start?,end?,files?}?, finish?, cost?, tokens{input,output,reasoning,cache{read,write}}?, error?, time{created,completed?}` (`:165-190`). Message IDs must start with `msg_` (`:12`). Stored in `SessionMessageTable` (`core/src/session/sql.ts:119`), projected from durable `session.next.*` events (`schema/src/session-event.ts:54-442`: `agent.switched`, `model.switched`, `moved`, `prompted`, `prompt.admitted`, `context.updated`, `synthetic`, `shell.{started,ended}`, `step.{started,ended,failed}`, `text.{started,delta,ended}`, `reasoning.{started,delta,ended}`, `tool.input.{started,delta,ended}`, `tool.{called,progress,success,failed}`, `retried`, `compaction.{started,delta,ended}`, `revert.{staged,cleared,committed}`), status events in `schema/src/session-status-event.ts` (`idle`/`retry`/`busy`).

Rust mapping (suggestion):
- Defer v2 until legacy parity; when ported, reuse the same `EventStore` + projector; model v2 messages as `#[serde(tag="type", rename_all="kebab-case")] enum`.
- v2 permission rules use fields `action`/`resource`/`effect` (`core/src/tool/registry.ts:130-133`) unlike v1 `permission`/`pattern`/`action` (section 6) — keep two rule types, do not unify silently.

## 9. Consolidated porting risks and open items

### 9.1 Top porting risks (ranked)

| # | Risk | Where | Mitigation (suggestion) |
|---|---|---|---|
| 1 | Persistence is event-sourced: rows = projector output of schema-**encoded** durable events; session `cost`/`tokens_*` are running sums of step-finish parts with subtraction on update/remove; stored event type carries `.1` suffix; per-session `seq`. Deltas are bus-only. | §1.7 | One `EventStore::publish` that runs projectors + `event`/`event_sequence` insert in one `BEGIN IMMEDIATE` txn; never write rows directly (except `todo`). |
| 2 | AI SDK behaviours with no Rust equivalent: `convertToModelMessages` (step splitting, tool-result shapes), 1 step per `streamText`, parallel tool execution within a step, tool-call repair to `invalid`. The JSON of converted messages also feeds compaction token estimates. | §1.9, §2.7, §3.7 | Port `ai@6.0.168` conversion from npm source; golden-test `JSON.stringify(toModelMessages(...))` against TS fixtures. |
| 3 | Byte-exact persisted/replayed strings: system prompt join rules (single `"\n"`, plugin collapse to 2 entries), synthetic texts, tool outputs (`<task …>` envelope, read/grep/glob formats, bash `<shell_metadata>`), error messages. | §2, §3.2, §4, §5 | Keep all literals as `const &str`/`include_str!`; snapshot tests per tool. |
| 4 | jsdiff 8.0.2 `createTwoFilesPatch`/`diffLines` + `trimDiff` produce persisted `metadata.diff`/`filediff`. | §4.8-4.10 | Port jsdiff Myers + `formatPatch`; golden tests. |
| 5 | JS string semantics in edit replacers / truncation / token estimate: UTF-16 indices and lengths, Unicode `trim`/`\s` (incl. U+FEFF), `String.replaceAll` `$`-substitution, `Math.round`, `localeCompare`. | §3.6, §4.9, §5, §7 | Operate on UTF-16 (`Vec<u16>`/`encode_utf16`) where lengths matter; ICU collator for sorts; property tests vs TS. |
| 6 | Legacy apply_patch parser leniency (ignored lines, dead `*** End of File`, CR kept, add-only chunks append at EOF) differs from strict `core/src/patch.ts`. | §4.10 | Separate `LegacyPatch` module, line-for-line port. |
| 7 | Bash permission patterns from tree-sitter-bash / tree-sitter-powershell node types + `BashArity` table; saved `always` rules depend on them. | §5.1, §6.5 | Pin grammar versions matching the wasm in repo; copy arity table verbatim. |
| 8 | Run-state/queueing: concurrent `prompt()` persists then joins the running loop; cancel resolves waiters with last assistant; shell-then-run chaining; retry attempt counting. | §2.4, §2.11, §2.13 | Explicit per-session state machine; replicate join/cancel semantics; test with concurrent requests. |
| 9 | Permission state: last-match-wins over ordered rules (config key order semantic), legacy `always` approvals are per instance directory (shared by sessions, not persisted), reject cascades to same-session pending, cancel removes pending without `permission.replied`. | §6 | `IndexMap` + serde_json `preserve_order`; per-instance approved list. |
| 10 | IDs: 48-bit truncated `ts*4096+counter`, `byte % 62` suffix; ordering of parts/messages depends on lexical id order. | §1.1 | Bit-exact port with `u128` math and mask. |
| 11 | JS-hosted extension points: custom `{tool,tools}/*.{js,ts}`, plugin tools and hooks (`chat.params`, `chat.headers`, `tool.execute.*`, `experimental.chat.system.transform`, …), code mode interpreter. | §4.2, §5.10 | Decide JS host (embedded runtime vs out-of-process) before porting the loop; hook call sites are listed per section. |

### 9.2 Open items (`UNKNOWN` / assumption) and how to resolve

| Item | Section | Resolution step |
|---|---|---|
| `convertToModelMessages` exact step-splitting rules (`ai@6.0.168`) | §1.9 | `bun install` in `opencode-ts`, read `node_modules/ai/src/ui/convert-to-model-messages.ts`; dump fixtures |
| drizzle `$onUpdate` applied inside `onConflictDoUpdate` (message/part `time_updated`) | §1.7 | Run projector upsert under bun with SQL logging; inspect generated `UPDATE SET` |
| Effect `Schedule` `metadata.attempt` starts at 1 (max 5 retries) | §2.11 | Read effect@4.0.0-beta.83 `Schedule.fromStepWithMetadata`, or unit-test `SessionRetry.policy` |
| AI SDK default = one step per `streamText` when no `stopWhen` | §2.7 | Read `ai@6.0.168` `streamText` defaults (expected `stopWhen` default `stepCountIs(1)`; unverified) |
| `EffectBridge.promise` preserves error instance (`instanceof RejectedError`) | §2.10 | Read `opencode/src/effect/bridge.ts`; test reject → loop stop |
| `InvalidArgumentsError.detail` text and JSON Schema output for `NonNegativeInt` | §4.1, §4.4 | Run `ToolJsonSchema.fromTool` + invalid-arg decode for each tool under bun; freeze fixtures |
| effect `Stream.splitLines` treatment of lone `\r` (read tool) | §4.7 | Read effect@4.0.0-beta.83 `Stream.splitLines` source |
| Node `shell:` argv construction on win32 (cmd `/d /s /c`, quoting) | §5.1 | Node `child_process` docs/source for the bundled Node/Bun version; capture argv with a probe binary |
| Todo JSON key order after Effect Schema decode | §5.3 | Decode `{priority,status,content,extra}` under bun, inspect `JSON.stringify` |
| remeda `mergeDeep` key ordering | §6.2 | Read remeda `mergeDeep` source (version in `bun.lock`) |
| Code mode interpreter semantics (`packages/codemode`) | §5.10 | Read `packages/codemode/src`; out of scope while flag-gated |
