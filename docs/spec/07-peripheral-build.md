# 07 — Peripheral packages, repo build & release

Source of truth: `D:\Download\opecode\opencode-ts` (tag v1.19.5, read-only). All `path:line` references are relative to that root.
Line counts were measured with `find … | xargs cat | wc -l` over `*.ts *.tsx *.js *.mjs *.astro *.css *.sql *.sh` (node_modules excluded). Nothing was built or executed except read-only inspection commands.

Effort scale (assumption, 1 engineer): **S** ≤ 3 days · **M** 1–2 weeks · **L** 3–6 weeks · **XL** > 6 weeks.

---

## Classification

- Scope: every package/dir that is not core runtime / API / storage / providers / integrations / clients, plus the whole build/release/test-infra surface.
- Headline conclusions:
  1. **Nothing in this scope ships inside the `opencode` binary** except build-time inputs: the models.dev snapshot, the embedded web UI bundle (`packages/app`), version/channel constants, and the config JSON Schema generator. Everything else is hosted SaaS/marketing (Cloudflare Workers via SST), CI tooling, or dev tooling.
  2. The hosted console in this repo (`packages/console`) is **already being superseded by an out-of-repo "New Console"** (`https://opencode.ai/console`, `https://opencode.ai/inference`); migrated workspaces are proxied away (`packages/console/app/src/lib/inference-proxy.ts:49,101`, `infra/console.ts:224-231`). Porting it to Rust has no product value for the CLI port.
  3. Session sharing is **discontinued in the CLI** (`packages/opencode/src/share/session.ts:19-23`), so the share backends (`packages/function` share routes, `packages/enterprise`, `packages/web` `/s/[id]`) are legacy.
  4. The release contract the Rust port must preserve is concrete and small: 12 asset names, archive layout, npm wrapper + `postinstall.mjs`, `install` script, Homebrew tap/AUR/Docker templating, `opencode --version` output, install-method detection, channel→DB-path mapping, major-version guards, desktop updater feeds.
  5. `http-recorder` cassettes (format v1, 44 product cassettes / 59 HTTP interactions) are plain JSON and **directly reusable** for Rust conformance tests of the LLM layer, with strict request-equality caveats (§A.6).

---

## Evidence

### E.1 Package inventory (scope)

| Package | Code lines | Other | Runtime / deploy target | Ships to users? |
|---|---:|---|---|---|
| `packages/console/app` | 50,719 (i18n 16,170; CSS 12,786; `routes/zen` 4,814; route TSX 10,689; `lib` 1,266) | 18 locales `src/i18n/*.ts` | SolidStart + nitro `cloudflare-module` (`packages/console/app/vite.config.ts:10-16`) → SST `sst.cloudflare.x.SolidStart("Console")` (`infra/console.ts:257-314`) | Hosted SaaS (opencode.ai root domain) |
| `packages/console/core` | 5,314 | 85 drizzle migrations (`packages/console/core/migrations/`) | Library; MySQL on PlanetScale (`packages/console/core/drizzle.config.ts`, dialect `mysql`) | No |
| `packages/console/function` | 398 | — | 3 Cloudflare Workers: `AuthApi`, `LogProcessor`, `Stat` (`infra/console.ts:61-66,252-255,320-324`) | No |
| `packages/console/mail` | 254 | fonts/images | JSX email templates (`emails/templates/InviteEmail.tsx`) | No |
| `packages/console/resource` | 104 | — | SST `Resource` shim (`resource.cloudflare.ts`, `resource.node.ts`) | No |
| `packages/console/support` | 878 | — | Local-only SolidStart tool, `"dev": "sst shell --stage production -- vite dev"` (`packages/console/support/package.json`); no SST resource defines it | No (internal) |
| `packages/function` | 450 | — | Cloudflare Worker `Api` at `api.<domain>` + Durable Object `SyncServer` (`infra/app.ts:13-50`) | Hosted API used by CLI `opencode github` |
| `packages/web` | 11,045 (share viewer 3,103) | 614 MDX (214,261 lines), 18 locales | Astro 5 + Starlight, `output: "server"`, Cloudflare adapter, `base: "/docs"` (`packages/web/astro.config.mjs:14-19`) → `sst.cloudflare.x.Astro("Web")` at `docs.<domain>` (`infra/app.ts:52-60`) | Hosted docs |
| `packages/docs` | 0 | 14 md/mdx, `docs.json` | Mintlify starter kit (`packages/docs/README.md:1`); not referenced by infra or workflows | No (unused) |
| `packages/enterprise` | 1,585 | — | SolidStart `cloudflare-module` (`packages/enterprise/vite.config.ts:11`) → `sst.cloudflare.x.SolidStart("Teams")` at `opncd.ai` (`infra/enterprise.ts:6-18`, `infra/stage.ts:36-40`) | Hosted legacy share viewer |
| `packages/stats/{app,core,server}` | 29,852 (app 23,922; core 5,861; server 69) | 8 migrations | app: SolidStart on CF at `stats.<domain>` (`infra/stats.ts:70-78`); server: ECS Fargate arm64 container (`infra/stats.ts:104-126`) | Hosted public stats site |
| `packages/slack` | 154 | `.env.example` | Local Bun process, `@slack/bolt` socket mode (`packages/slack/src/index.ts:4-8`) | No (example bot) |
| `packages/identity` | 0 | 6 PNG/SVG marks | none; no references anywhere in repo | No |
| `packages/containers` | 77 | 5 Dockerfiles | CI images pushed to `ghcr.io/anomalyco/build/<name>:24.04` (`packages/containers/script/build.ts:10-37`) | No |
| `packages/http-recorder` | 2,558 (src 1,540; test 879) | 2 cassettes | npm library `@opencode-ai/http-recorder` (public beta) | Dev dependency only |
| `packages/storybook` | 1,174 | — | Storybook over `ui`, `session-ui`, `app` stories (`packages/storybook/.storybook/main.ts:25-28`) | No |
| `packages/script` | 86 | — | Bun lib `@opencode-ai/script` (version/channel resolver) | Build-time only |
| `packages/effect-drizzle-sqlite` | 3,515 | example migration | Vendored Drizzle↔Effect SQLite adapter (`packages/effect-drizzle-sqlite/AGENTS.md:1-10`) | Linked into CLI (storage layer) |
| `packages/effect-sqlite-node` | 177 | — | Effect `SqlClient` over `node:sqlite` (`packages/effect-sqlite-node/src/index.ts:3`) | Linked into Node build (desktop) |
| `packages/sdk/*` non-JS | — | only `packages/sdk/js` + `packages/sdk/openapi.json` (1,100,371 bytes) | — | n/a; `.github/publish-python-sdk.yml` is fully commented out and references non-existent `packages/sdk/python` |
| `github/` (GitHub Action) | 1,072 (`github/index.ts`) | `action.yml` | Composite action; tags `github-v*`, `latest` (`github/script/publish`) | Yes (Action users) |
| `sdks/vscode` | ~137 | — | VS Code extension `sst-dev.opencode` (`sdks/vscode/package.json:2,6`) | Yes (Marketplace/OpenVSX) |
| `infra/` + `sst.config.ts` | 879 + ~60 | — | SST v4 (`package.json` catalog `"sst": "4.13.1"`), home `cloudflare` (`sst.config.ts:9`) | No |

`packages/docs/openapi.json` is a 19-byte git symlink text `../sdk/openapi.json` (Windows checkout).

### E.2 Hosted topology (from `infra/`)

Domain per stage: `opencode.ai` (production) / `dev.opencode.ai` / `<stage>.dev.opencode.ai` (`infra/stage.ts:1-5`); short domain `opncd.ai` / `dev.opncd.ai` / `<stage>.dev.opncd.ai` (`infra/stage.ts:36-40`). Cloudflare zone `430ba34c138cfb5360826c4909f99be8` (`infra/stage.ts:7`). AWS only for stages `production` and `dev` (`infra/stage.ts:8-9`), region `us-east-1` (`sst.config.ts:16`).

| SST resource | Type | Host | Code | Links / bindings (exact names) |
|---|---|---|---|---|
| `WebApp` (stage `v1`) | `sst.cloudflare.StaticSite` | `app.opencode.ai` | `packages/app`, `bun turbo build` → `./dist` (`sst.config.ts:35-44`) | — |
| `Api` | `sst.cloudflare.Worker` | `api.<domain>` | `packages/function/src/api.ts` (`infra/app.ts:13-15`) | `Bucket`, `GITHUB_APP_ID`, `GITHUB_APP_PRIVATE_KEY`, `ADMIN_SECRET`, `DISCORD_SUPPORT_BOT_TOKEN`, `DISCORD_SUPPORT_CHANNEL_ID`, `FEISHU_APP_ID`, `FEISHU_APP_SECRET`; DO binding `SYNC_SERVER` class `SyncServer` (`infra/app.ts:20-40`) |
| `Web` | `sst.cloudflare.x.Astro` | `docs.<domain>` | `packages/web`, env `SST_STAGE`, `VITE_API_URL` (`infra/app.ts:52-60`) | — |
| `WebApp` (non-v1) | `sst.cloudflare.StaticSite` | `app.<domain>` | `packages/app` (`infra/app.ts:62-69`) | — |
| `Database` | `sst.Linkable` over PlanetScale db `opencode`, org `anomalyco`, port 3306 (`infra/console.ts:9-42`) | — | — | branch per stage, parent `production` |
| `AuthApi` | Worker | `auth.<domain>` | `packages/console/function/src/auth.ts` (`infra/console.ts:61-66`) | `Database`, KV `AuthStorage`, `GITHUB_CLIENT_ID_CONSOLE`, `GITHUB_CLIENT_SECRET_CONSOLE`, `GOOGLE_CLIENT_ID` |
| `StripeWebhookEndpoint` | `stripe.WebhookEndpoint` → `https://<domain>/stripe/webhook` (`infra/console.ts:72-102`) | — | — | 26 enabled events |
| Stripe products | `ZenLite` "OpenCode Go" $10/mo (`infra/console.ts:104-148`); `ZenBlack` "OpenCode Black" $200/$100/$20 (`infra/console.ts:163-184`) | — | — | Linkables `ZEN_LITE_PRICE`, `ZEN_BLACK_PRICE` |
| `ZEN_MODELS1..30`, `ZEN_LIMITS`, `ZEN_SESSION_SECRET` | `sst.Secret` (`infra/console.ts:186-217,282-283`) | — | — | model routing config split over 30 secrets |
| `ConsoleMigration` | Linkable `{consoleUrl: https://<d>/console, inferenceUrl: https://<d>/inference}` (`infra/console.ts:224-231`) | — | — | only prod/dev |
| `ZenData`, `ZenDataNew` | R2 buckets (`infra/console.ts:240-241`) | — | — | — |
| `LogProcessor` | Worker (tail consumer) | — | `packages/console/function/src/log-processor.ts` (`infra/console.ts:252-255,309`) | `HONEYCOMB_API_KEY` |
| `Console` | `sst.cloudflare.x.SolidStart` | `<domain>` | `packages/console/app` (`infra/console.ts:257-314`), placement `aws:us-east-2` (`:302`), flag `global_fetch_strictly_public` (`:307`) | full list `infra/console.ts:260-292` incl. `UpstashRedisRestUrl/Token`, `AWS_SES_*`, `SALESFORCE_*`, `EMAILOCTOPUS_API_KEY`, `SUPPORT_API_KEY` |
| `Stat` | Worker, `url: true` | SST-generated URL (no custom domain) | `packages/console/function/src/stat.ts` (`infra/console.ts:320-324`) | `Database` |
| `Teams` | `sst.cloudflare.x.SolidStart` | `opncd.ai` | `packages/enterprise`, `bun run build:cloudflare` (`infra/enterprise.ts:6-18`) | `SUPPORT_API_KEY`; env `OPENCODE_STORAGE_ADAPTER=r2`, bucket `EnterpriseStorage` |
| `LakeVpc`/`LakeCluster` | AWS VPC + ECS cluster (`infra/stats.ts:6-7`) | — | — | — |
| `StatsDatabase` | PlanetScale db `opencode-stats` (`infra/stats.ts:13-52`) | — | — | — |
| `Stats` | SolidStart | `stats.<domain>` (`infra/stats.ts:70-78`) | `packages/stats/app` | `PUBLIC_URL=https://<domain>/data` |
| `R2Sql` | Linkable: accountId `15d29c8639fd3733b1b5486a2acfd968`, bucket `platform-<stage>-lake`, namespace `inference`, table `generation` (`infra/stats.ts:95-102`) | — | — | — |
| `StatsSyncService` | `sst.aws.Service`, arm64, 0.25 vCPU, 2 GB, `packages/stats/server/Dockerfile`, cmd `bun src/stat-sync.ts` (`infra/stats.ts:104-126`) | — | — | — |
| Honeycomb triggers | `honeycombio.WebhookRecipient` → `https://<domain>/honeycomb/webhook` (`infra/monitoring.ts:7-9`); triggers `infra/monitoring.ts:160-282` | — | — | prod + `vimtor` only (`sst.config.ts:52-54`) |

Deployment from this repo: only `deploy.yml` → `bun sst deploy --stage=v1` (app.opencode.ai) on push to `v1` (`.github/workflows/deploy.yml:4-6,29`). Console/stats/enterprise/api deployments to `dev`/`production` stages are **not** performed by any workflow in this tree (`sst.config.ts:34` comment says they deploy "from dev and production") → mechanism `UNKNOWN`.

### E.3 CLI/desktop ↔ hosted-service coupling (what the binary actually calls)

| Caller (`path:line`) | Endpoint | Served by (in repo?) |
|---|---|---|
| `packages/core/src/plugin/provider/opencode.ts:16,47,69` ; `packages/opencode/src/account/account.ts:214,384,413` | `POST https://opencode.ai/console/auth/device/code`, `POST …/auth/device/token` (client_id `opencode-cli`, `opencode.ts:17`) | **Not in repo** — New Console (`UNKNOWN`) |
| `packages/core/src/plugin/provider/opencode.ts:201-208`; `packages/opencode/src/account/account.ts:281,294,364` | `GET <server>/api/config`, `/api/orgs`, `/api/user` (header `x-org-id`) | **Not in repo** (`UNKNOWN`) |
| `packages/opencode/test/fixtures/recordings/session/native-zen-tool-loop.json` | `POST https://console.opencode.ai/proxy/connections/{connection}/v1/responses` | **Not in repo** (`UNKNOWN`) |
| provider `opencode` with `apiKey: "public"` fallback (`packages/opencode/src/provider/provider.ts:220,240`) | Zen gateway base URL comes from models.dev data | Zen gateway = `packages/console/app/src/routes/zen/**`; exact base URL `UNKNOWN` (models.dev) |
| `packages/opencode/script/generate.ts:10-13` (build time) | `GET ${OPENCODE_MODELS_URL:-https://models.dev}/api.json` | external |
| `packages/core/src/models-dev.ts:160,163` | `https://models.opencode.ai` | external, `UNKNOWN` |
| `packages/opencode/src/config/config.ts:246-397`; `packages/opencode/src/config/tui-migrate.ts:11` | `$schema: https://opencode.ai/config.json`, `https://opencode.ai/tui.json` | console build writes them (`packages/console/app/package.json:10`); web build too (`packages/web/astro.config.mjs:315-325`) |
| `packages/opencode/src/cli/cmd/github.handler.ts:324` | `GET https://api.opencode.ai/get_github_app_installation?owner=&repo=` | `packages/function/src/api.ts:362` |
| `packages/opencode/src/cli/cmd/github.handler.ts:676-677,965,977,984` | `POST ${OIDC_BASE_URL:-https://api.opencode.ai}/exchange_github_app_token[_with_pat]`, OIDC audience `opencode-github-action` | `packages/function/src/api.ts:262-360` (audience `:263`, issuer `:264`) |
| `packages/opencode/src/installation/index.ts:168,192` | `GET https://opencode.ai/install`, `https://opencode.ai/v2/install` | hosting route **not in repo** (`UNKNOWN`); file = root `install` (v1) |
| `packages/opencode/src/installation/index.ts:224` | `GET <npm registry>/opencode-ai/<tag>` | npm |
| `packages/opencode/src/installation/index.ts:236` | `https://community.chocolatey.org/api/v2/Packages?$filter=Id eq 'opencode' and Version eq '<v>'` | Chocolatey |
| `packages/app/src/context/highlights.tsx:10` | `GET https://opencode.ai/changelog.json` | `packages/console/app/src/routes/changelog.json.ts:12-23` (proxies `api.github.com/repos/anomalyco/opencode/releases?per_page=20`, `lib/changelog.ts:42`) |
| `packages/app/src/pages/error.tsx:357` etc. | `https://opencode.ai/desktop-feedback` | `packages/console/app/src/routes/desktop-feedback.ts:3-4` → Discord invite |
| `packages/desktop/src/main/wsl/runtime.ts:271` | `curl -fsSL https://opencode.ai/install \| bash -s -- --version <v>` | root `install` |
| `packages/desktop/src/main/updater.ts:2-20` | electron-updater, GitHub provider | GitHub releases `latest*.yml` (§A.4.10) |
| `packages/opencode/src/session/retry.ts:11,132` | `https://opencode.ai/go`, `/workspace/<id>/go` (links only) | console UI |
| `packages/opencode/src/share/session.ts:19-23` | none — share fails with "Session sharing has been discontinued" | — |
| `packages/console/app/src/routes/oauth/opencode/client.json.ts:13-38` | OAuth Client-ID Metadata Document | no caller found in `packages/opencode/src` or `packages/core/src` (grep `client.json`) |

---

## Analysis

### A.1 `packages/console` (Zen gateway + billing SaaS)

Path shorthand in this section: `routes/…`, `lib/…`, `app/src/…` are under `packages/console/app/src/` (or `packages/console/` for `app/src/…`).

**Purpose.** Marketing site + workspace console (keys, members, billing, usage, BYOK providers) + the **Zen / Go LLM gateway** at `/zen/v1/*` and `/zen/go/v1/*`.

**Entrypoints.**
- App: `packages/console/app/src/middleware.ts:6-23` (locale prefix stripping, referral cookie), SolidStart file routes under `packages/console/app/src/routes/`.
- Zen gateway routes (exact list from the tail filter `packages/console/function/src/log-processor.ts:13-20` plus route files):
  `POST /zen/v1/chat/completions`, `/zen/v1/messages`, `/zen/v1/responses`, `/zen/v1/models/<model>` (Google style), `/zen/v1/systemone`; `GET /zen/v1/models`; same under `/zen/go/v1/*` plus `GET /zen/go/v1/usage`. Core handler `packages/console/app/src/routes/zen/util/handler.ts` (1,314 lines); auth `Authorization: Bearer` (`routes/zen/v1/chat/completions.ts:9`), `x-api-key` for `/messages`, `x-goog-api-key` for Google (`lib/inference-proxy.ts:40-44`); upstream adapters `routes/zen/util/provider/{anthropic,google,openai,openai-compatible,systemone}.ts`; rate limiters (IP/key/model TPM/TPS), sticky provider, trial limiter, Upstash Redis (`routes/zen/util/redis.ts:2`).
- Migration cut-over: keys starting `oc_sk_` are New-Console keys; legacy `sk-` keys of migrated workspaces (`workspace.migrated_at` not null) are proxied to `ConsoleMigration.inferenceUrl` (`lib/inference-proxy.ts:46-66,101-125`). Auth redirects to `ConsoleMigration.consoleUrl` once migrated (`app/src/context/auth.ts:107-108,161`, `routes/auth/index.ts:12`).
- Other API routes: `POST /stripe/webhook` (399 lines), `POST /honeycomb/webhook` (→ `DISCORD_INCIDENT_WEBHOOK_URL`), `POST /api/enterprise` (EmailOctopus + SES + Salesforce), `POST /api/support/actions/{block-workspaces,create-referral,delete-account,reset-quota,unblock-workspace,unblock-workspaces}` (`SUPPORT_API_KEY`), `POST /bench/submission`, `/auth/{authorize,logout,status,[...callback]}`.
- Proxies: `/docs/*` and `/docs` → `docs.opencode.ai` / `docs.dev.opencode.ai` (`routes/docs/[...path].ts:9-10`); `/s/<id>` → docs host `/docs/s/<id>` (`routes/s/[id].ts:9-10`); `/t/*` → `https://enterprise.opencode.ai/` (`routes/t/[...path].tsx:7`); `/data/*` → `stats.opencode.ai` (`lib/stats-proxy.ts:24`); `/stats/*` redirect; `/openapi.json` → `raw.githubusercontent.com/anomalyco/opencode/refs/heads/dev/packages/sdk/openapi.json` (`routes/openapi.json.ts:2-4`); `/download/<channel>/<platform>` → `https://opencode.ai/update/api/{latest|beta}/desktop/opencode` JSON `metadata.files[<asset>].url` (`routes/download/[channel]/[platform].ts:22-40`).
- Static artefacts generated at build: `config.json`, `tui.json` into `.output/public/` via `bun ../../opencode/script/schema.ts` (`packages/console/app/package.json:10`).

**External services** (exact call sites): Stripe SDK (`packages/console/core/src/billing.ts:1,30`); AWS SES `https://email.us-east-1.amazonaws.com/v2/email/outbound-emails` (`packages/console/core/src/aws.ts:28`); EmailOctopus `https://api.emailoctopus.com/lists/<id>/contacts` (`app/src/component/email-signup.tsx:10`, `app/src/routes/api/enterprise.ts:41`); Salesforce `<instance>/services/oauth2/token` (`app/src/lib/salesforce.ts:14`); Upstash Redis (`routes/zen/util/redis.ts:2`); GitHub REST (`app/src/lib/changelog.ts:42`, `app/src/lib/github.ts:10`); OpenAuth issuer with GitHub + Google OIDC providers, Cloudflare KV storage (`packages/console/function/src/auth.ts:7-9,55-67,111`); PlanetScale serverless driver (`packages/console/core/src/drizzle/index.ts:1,4,19-25`); Honeycomb export in log-processor is **commented out** (`packages/console/function/src/log-processor.ts:57`, comment `:40`).

**DB schema** (MySQL/PlanetScale, drizzle `mysqlTable`; shared column helpers: ULID `varchar(30)`, `timestamp(fsp 3)` `time_created/time_updated/time_deleted`, `bigint` currency — `packages/console/core/src/drizzle/types.ts:4-33`):

| Table | Defined at | Key columns (exact) |
|---|---|---|
| `account` | `packages/console/core/src/schema/account.sql.ts:5` | id + timestamps |
| `auth` | `…/schema/auth.sql.ts:7` | `provider`, `subject`, `account_id` |
| `benchmark` | `…/schema/benchmark.sql.ts:5` | `model`, `agent`, `result` |
| `billing` | `…/schema/billing.sql.ts:18` | `customer_id`, `payment_method_id`, `balance`, `monthly_limit`, `monthly_usage`, `reload*`, `subscription*`, `lite_subscription_id`, `global_*` |
| `subscription` | `…/schema/billing.sql.ts:61` | `user_id`, `rolling_usage`, `fixed_usage` |
| `lite` | `…/schema/billing.sql.ts:75` | `user_id`, `rolling_usage`, `weekly_usage`, `monthly_usage` |
| `payment` | `…/schema/billing.sql.ts:91` | `customer_id`, `invoice_id`, `payment_id`, `amount`, `time_refunded`, `enrichment` |
| `usage` | `…/schema/billing.sql.ts:115` | `model`, `provider`, `input_tokens`, `output_tokens`, `reasoning_tokens`, `cache_read_tokens`, `cost`, `key_id`, `session_id` |
| `coupon` | `…/schema/billing.sql.ts:147` | `email`, `type`, `time_redeemed` |
| `ip` | `…/schema/ip.sql.ts:5` | `ip`, `usage` |
| `ip_rate_limit` | `…/schema/ip.sql.ts:15` | `ip`, `interval`, `count` |
| `key_rate_limit` | `…/schema/ip.sql.ts:25` | `key`, `interval`, `count` |
| `model_tpm_rate_limit` | `…/schema/ip.sql.ts:35` | `id`, `interval`, `count` |
| `model_tps_rate_limit` | `…/schema/ip.sql.ts:45` | `id`, `interval`, `qualify`, `unqualify` |
| `model_sticky_provider` | `…/schema/ip.sql.ts:56` | `id`, `provider_id` |
| `key` | `…/schema/key.sql.ts:6` | `name`, `key` (unique `global_key`), `user_id`, `time_used` |
| `model` | `…/schema/model.sql.ts:6` | `model` |
| `provider` | `…/schema/provider.sql.ts:6` | `provider`, `credentials` |
| `referral_code` | `…/schema/referral.sql.ts:6` | `workspace_id`, `code` |
| `referral` | `…/schema/referral.sql.ts:16` | `code`, `invitee_account_id` |
| `referral_reward` | `…/schema/referral.sql.ts:26` | `workspace_id`, `referral_id`, `amount`, `time_applied` |
| `user` | `…/schema/user.sql.ts:8` | `account_id`, `email`, `name`, `role`, `monthly_limit`, `monthly_usage` |
| `workspace` | `…/schema/workspace.sql.ts:5` | `slug`, `name`, `region` json, `allow_training`, `is_blocked`, `is_flagged_by_anthropic`, `is_flagged_by_openai`, `migrated_at` (`:15`) |

Workspace-scoped tables use composite PK `(workspace_id, id)` (`…/schema/workspace.sql.ts:21-27`). Latest migration `20260901161032_workspace_migrated_at` = `ALTER TABLE workspace ADD migrated_at timestamp(3)`.

**Satellite workers.** `Stat` worker: `POST {ids}` → 30 one-minute intervals of `model_tps_rate_limit` (`packages/console/function/src/stat.ts:6-43`). `LogProcessor`: Workers tail consumer extracting `_metric:` log lines for Zen POSTs (`log-processor.ts:4-55`), currently only `console.log`s.

**Ships to users?** No binary coupling. CLI coupling = (a) Zen gateway HTTP API, used through the `opencode` provider; (b) hosting of `config.json`/`tui.json`; (c) `changelog.json` for desktop/web app; (d) desktop download redirects.

**Rust recommendation: do not port (keep TS, freeze).** Rationale: hosted, being migrated to an out-of-repo New Console, heavy UI (SolidStart + 16k lines i18n + 12.8k CSS), deep coupling with Stripe/OpenAuth/PlanetScale serverless driver. If a Rust Zen gateway is ever wanted: `workers-rs` or `axum` service, port only `routes/zen/**` + `console/core` billing/usage/rate-limit tables; needs MySQL access (PlanetScale HTTP API has no first-party Rust driver — assumption: use `sqlx` MySQL over TCP from a non-Workers host) → **XL** (gateway alone **L**). Effort to keep: **S** (only replace the schema-generation build step, §I.3).

### A.2 `packages/function` (`api.opencode.ai`)

- Hono app `packages/function/src/api.ts:117-…`:
  - `GET /` hello; share legacy: `POST /share_create` (`:119`, returns `{secret, url: https://<WEB_DOMAIN>/s/<last 8 chars>}` `:126-129`), `POST /share_delete` (`:131`), `POST /share_delete_admin` (`:141`, `ADMIN_SECRET`), `POST /share_sync` (`:151`, keys must start `session/info/<id>`, `session/message/<id>/`, `session/part/<id>/` `:48-53`), `GET /share_poll` WebSocket (`:165-175`), `GET /share_data` (`:176-204`).
  - `POST /feishu` → Discord channel message `https://discord.com/api/v10/channels/<id>/messages` (`:205-239`).
  - GitHub App: `POST /exchange_github_app_token` (`:262`, verifies GitHub Actions OIDC JWT, issuer `https://token.actions.githubusercontent.com`, audience `opencode-github-action`, JWKS `…/.well-known/jwks` `:263-277`), `POST /exchange_github_app_token_with_pat` (`:312`), `GET /get_github_app_installation` (`:362`).
- Storage: Durable Object `SyncServer` storage + R2 `Bucket` keys `share/<key>.json` (`:56`).
- Coupling: **live** for `opencode github run` (GitHub Action `github/action.yml:61,70`; handler `github.handler.ts:324,677,977,984`). Share endpoints only serve old CLIs.
- **Recommendation:** keep TS worker (**S**). Optional port: `workers-rs` (DO support exists in workers-rs — assumption, verify crate version) with `jsonwebtoken`+JWKS and `octocrab` app auth → **M**. The Rust CLI must reproduce request/response payloads of the three GitHub endpoints exactly (integration agent).

### A.3 `packages/web` (docs site)

- Astro 5.7.13 + Starlight 0.34.3 + `toolbeam-docs-theme` (`packages/web/package.json`), `site` from `config.mjs:4`, `base: "/docs"`, SSR on Cloudflare (`astro.config.mjs:14-19`), 18 locales `root(en)…zh-tw` (`astro.config.mjs:35-127`), sidebar `astro.config.mjs:174+`, component overrides `Head/Header/Footer/LanguageSelect/SiteTitle` (`:300-304`).
- Content: 36 English MDX pages `src/content/docs/*.mdx`; 614 MDX total (17 translated locale dirs); i18n strings `src/content/i18n/*.json` (18 files). MDX imports actually used: `@astrojs/starlight/components` (`Tabs`, `TabItem`, `Steps`), `../../../config.mjs`, and code samples importing `@opencode-ai/plugin` / `@opencode-ai/sdk` (inside code fences).
- Dynamic routes: `src/pages/[...slug].md.ts:19-34` serves raw MDX body as `text/plain` (LLM-friendly `.md` URLs); `src/pages/s/[id].astro` legacy share viewer using `wss://<VITE_API_URL>/share_poll?id=` (`src/components/Share.tsx:111`).
- Build hook `configSchema()` runs `../opencode/script/schema.ts ./dist/config.json ./dist/tui.json` on `astro:build:done` (`astro.config.mjs:315-325`) — hard dependency on the TS CLI config schema.
- Served publicly as `https://opencode.ai/docs/*` via console proxy (§A.1).
- **Recommendation: keep as content.** Option K (keep Astro/Starlight, replace only the schema hook with the Rust schema emitter) → **S**. Option Z (migrate to Zola/mdBook): needs MDX→Markdown conversion of `<Tabs>/<TabItem>/<Steps>` into shortcodes, per-locale sidebars with translated labels, search, raw `.md` endpoint → **L**. Drop `/s/[id]` + `Share.tsx` (sharing discontinued). Note docs drift: `packages/web/src/content/docs/share.mdx:20,68-73` and `packages/web/src/content/docs/enterprise.mdx:34-53` still describe sharing.

### A.4 Repo build & release (current state)

#### A.4.1 Version/channel resolution — `packages/script/src/index.ts`
- Enforces Bun `^<packageManager>` (`bun@1.4.4`, `package.json` `"packageManager"`) (`:5-18`).
- Env inputs: `OPENCODE_CHANNEL`, `OPENCODE_BUMP`, `OPENCODE_VERSION`, `OPENCODE_RELEASE` (`:20-25`).
- Channel: explicit env → `"latest"` if bump or non-`0.0.0-` version → else current git branch (`:26-31`). `preview = channel !== "latest"` (`:32`).
- Version: explicit → preview `0.0.0-<channel>-<YYYYMMDDHHMM>` (`:36`) → else `registry.npmjs.org/opencode-ai/latest` + bump major/minor/patch (`:37-47`).
- `team` = `.github/TEAM_MEMBERS` + bots `actions-user`, `opencode`, `opencode-agent[bot]` (`:50-58`).

#### A.4.2 CLI build — `packages/opencode/script/build.ts`
- 12 targets (`:53-114`); name = `opencode-<os|windows>-<arch>[-baseline][-musl]` (`:147-156`); Bun target = name with `opencode`→`bun` (`:178`).
- `Bun.build` compile options: minify, splitting, `execArgv: ["--user-agent=opencode/<version>", "--use-system-ca", "--"]` (`:173-182`); embedded files: tree-sitter worker, optional `opencode-web-ui.gen.ts` mapping every file of `packages/app/dist` (`:26-48,183-192`).
- Compile-time defines: `OPENCODE_VERSION`, `OPENCODE_CHANNEL`, `OPENCODE_MODELS_DEV` (models.dev JSON, `script/generate.ts:10-13`), `OPENCODE_LIBC` (`glibc`/`musl`), `FFF_LIBC`, `OTUI_TREE_SITTER_WORKER_PATH`, `OPENCODE_WORKER_PATH` (`:193-202`).
- macOS ad-hoc re-sign (`:207-209`); smoke test `--version` on host target (`:212-222`).
- Platform npm `package.json`: `{name, version, preferUnplugged: true, os:[os], cpu:[arch], libc?:[musl]}` (`:225-238`).
- Release archives: linux → `<name>.tar.gz`, others → `<name>.zip`, created **inside `bin/`** (so archive root holds only `opencode`/`opencode.exe`) (`:242-249`); `gh release upload v<version> … --clobber` (`:250`).
- Flags: `--single`, `--baseline`, `--skip-install`, `--sourcemaps`, `--skip-embed-web-ui` (`:19-24`).
- Node build for desktop: `packages/opencode/script/build-node.ts:15-30` → `dist/node` (target node, externals `jsonc-parser`, `@lydell/node-pty`, empty web UI).

#### A.4.3 GitHub release assets (CLI) — exact names
`opencode-linux-arm64.tar.gz`, `opencode-linux-x64.tar.gz`, `opencode-linux-x64-baseline.tar.gz`, `opencode-linux-arm64-musl.tar.gz`, `opencode-linux-x64-musl.tar.gz`, `opencode-linux-x64-baseline-musl.tar.gz`, `opencode-darwin-arm64.zip`, `opencode-darwin-x64.zip`, `opencode-darwin-x64-baseline.zip`, `opencode-windows-arm64.zip`, `opencode-windows-x64.zip`, `opencode-windows-x64-baseline.zip` (derived from `build.ts:59-113,147-156,244-248`; darwin/windows zips re-uploaded after signing `publish.yml:166-184,267-286`). Tag `v<version>` (`script/version.ts:20`).

#### A.4.4 `publish.yml` pipeline (`.github/workflows/publish.yml`)
- Trigger: push to `v1` (preview, channel `dev`) or dispatch with `bump`/`version` (release) (`:4-21,32`); only `anomalyco/opencode` on `refs/heads/v1` (`:38`); release env approval (`:40`).
- `version` job: `./script/version.ts` → for non-preview: changelog (`script/changelog.ts`), draft release `gh release create v<v> -d` (`script/version.ts:9-24`); outputs `version, release, tag, repo`.
- `build-cli` (`:67-107`): `packages/opencode/script/build.ts` **and** `packages/cli/script/build.ts`; artifacts `opencode-cli` (darwin+linux), `opencode-cli-windows`, `opencode-preview-cli` (`packages/cli/dist/cli-*`).
- `sign-cli-macos` (`:109-200`, `macos-26`): import p12 into `build.keychain`; `codesign --force --timestamp --options runtime --entitlements packages/opencode/script/entitlements.plist` for `opencode-darwin-{arm64,x64,x64-baseline}` (`:149-161`). Entitlements: `allow-jit`, `allow-unsigned-executable-memory`, `disable-executable-page-protection`, `allow-dyld-environment-variables`, `disable-library-validation`. **No notarization of the CLI** (no `notarytool`/`stapler` anywhere; electron only `electron-builder.config.ts:82`).
- `sign-cli-windows` (`:202-294`): Azure Artifact Signing action for the 3 `opencode.exe` (`:231-249`), `Get-AuthenticodeSignature` verify (`:251-265`), repack with `Compress-Archive` (`:267-273`).
- `build-electron` (`:296-492`): matrix `x86_64-apple-darwin`, `aarch64-apple-darwin`, `aarch64-pc-windows-msvc`, `x86_64-pc-windows-msvc`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` (`:311-332`); `bun ./scripts/prepare.ts` with `RUST_TARGET` (`:396-402`); `bun run build`; `npx electron-builder <flag> --publish never` (`:419-438`); macOS `.app.tar.gz` → `opencode-desktop-mac-{x64,arm64}.app.tar.gz` (`:440-459`); artifacts `opencode-desktop-<target>`, `latest-yml-<target>`.
- `publish` (`:494-615`): downloads signed artifacts, uploads desktop assets `*.{exe,blockmap,dmg,zip,AppImage,deb,rpm}` + `*.app.tar.gz` (`:591-602`), runs `./script/publish.ts` with `AUR_KEY`, `HOMEBREW_TAP_DEPLOY_KEY`, `LATEST_YML_DIR`, `TAURI_SIGNING_PRIVATE_KEY[_PASSWORD]` (`:604-615`).

#### A.4.5 `script/publish.ts` (root) order
1. Rewrite every `package.json` `"version"` (excluding node_modules/dist), `bun install`, build SDK (`:13-29`).
2. `packages/opencode/script/publish.ts` → npm + Docker + AUR + Homebrew (`:39`).
3. `packages/sdk/js/script/publish.ts`, `packages/plugin/script/publish.ts`, `packages/ui/script/publish.ts` (`:42-48`) — npm dist-tag = channel.
4. If release: `packages/desktop/scripts/finalize-latest-json.ts` (only when `TAURI_SIGNING_PRIVATE_KEY` set) and `finalize-latest-yml.ts` (`:50-55`).
5. Non-preview release: commit `release: v<v>`, force-push tag, then sync versions back to `v1` (`:57-68`); `gh release edit v<v> --draft=false --latest` ("OpenCode 1 clients resolve updates through GitHub's Latest release") (`:70-73`).
- `packages/cli/script/publish.ts` (preview CLI `@opencode-ai/cli` + `@opencode-ai/cli-*`, binary `lildax`) is **not invoked** by any workflow (grep). The preview artifact is built and uploaded only.

#### A.4.6 npm packaging — `packages/opencode/script/publish.ts`
- Each platform dir published if not already (`npm view` check), `bun pm pack` + `npm publish *.tgz --access public --tag <channel>` (`:10-24,75-78`).
- Wrapper `opencode-ai`: `bin: {opencode: "./bin/opencode.exe"}` (placeholder shell script printing postinstall-not-run error `:38-52`), `scripts.postinstall: "node ./postinstall.mjs"`, `os: [darwin, linux, win32]`, `cpu: [arm64, x64]`, `optionalDependencies` = all 12 platform packages at the same version (`:54-73`).
- `postinstall.mjs` selection: `opencode-<darwin|linux|windows>-<arch>`; AVX2 probe (`/proc/cpuinfo`, `sysctl hw.optional.avx2_0`, PowerShell `IsProcessorFeaturePresent(40)`) (`:31-77`); musl probe (`/etc/alpine-release`, `ldd --version`) (`:79-94`); ordered fallbacks (`:96-117`); copy/hardlink `bin/<opencode|opencode.exe>` → `bin/opencode.exe`, verify `--version` exit 0, else `npm install --ignore-scripts --no-save --prefix <tmp> <pkg>@<ver>` (`:119-182`).

#### A.4.7 Other distribution channels
| Channel | Produced by | Details |
|---|---|---|
| Docker `ghcr.io/anomalyco/opencode:<version>` and `:<channel>` | `packages/opencode/script/publish.ts:81-88` (non-preview only) | `packages/opencode/Dockerfile:1-18`: `alpine`, `apk add libgcc libstdc++ ripgrep`, amd64 ← `dist/opencode-linux-x64-baseline-musl/bin/opencode`, arm64 ← `dist/opencode-linux-arm64-musl/bin/opencode`, `ENTRYPOINT ["opencode"]` |
| AUR `opencode-bin` | `publish.ts:98-144` | sources `opencode-linux-{arm64,x64}.tar.gz` + sha256, `depends=('ripgrep')`, `install -Dm755 ./opencode /usr/bin/opencode` |
| Homebrew tap `anomalyco/homebrew-tap` file `opencode.rb` | `publish.ts:147-223` (SSH deploy key) | darwin zips + linux tar.gz + sha256, `depends_on "ripgrep"`, `bin.install "opencode"` |
| Homebrew core `opencode` | external | is OpenCode 2 (`packages/opencode/src/installation/index.ts:297`) |
| Scoop `opencode`, Chocolatey `opencode`, `mise`, `nixpkgs#opencode`, cask `opencode-desktop`, scoop `extras/opencode-desktop` | external (`README.md:50-61,79-82`) | manifests not in repo → `UNKNOWN` |
| Nix flake | `flake.nix:1-72`, `nix/opencode.nix`, `nix/node_modules.nix`, `nix/desktop.nix`, `nix/hashes.json` | systems `aarch64-linux`, `x86_64-linux`, `aarch64-darwin` (`flake.nix:11-15`); outputs `packages.<sys>.{default,opencode,opencode-desktop,node_modules_updater}`, `overlays.default` (`flake.nix:32-69`); build runs `bun ./script/build.ts --single --skip-install` + `script/schema.ts schema.json` (`nix/opencode.nix:52-57`), installs `$out/bin/opencode` and `$out/share/opencode/schema.json`, wraps with ripgrep on PATH (`nix/opencode.nix:62-72`); hash bot `bot-nix-hashes.yml`, `nix-eval.yml` |
| VS Code extension | `publish-vscode.yml` on tag `vscode-v*.*.*` (`:3-7`) | `sdks/vscode/script/publish`: `vsce package` → `vsce publish`, `npx ovsx publish` (`VSCE_PAT`, `OPENVSX_TOKEN`) |
| GitHub Action | `publish-github-action.yml`, `release-github-action.yml` | `github/script/release`, `github/script/publish` move tag `latest` to newest `github-v*` |

#### A.4.8 Install script — root `install` (v1)
- `APP=opencode` (`install:3`); flags `-v/--version`, `-b/--binary`, `--no-modify-path`, env `VERSION` (`:18-29`).
- Refuses major ≥ 2 → points to `https://opencode.ai/v2/install`, `opencode upgrade --major`, `https://opencode.ai/v2/docs` (`:70-78`).
- `INSTALL_DIR=$HOME/.opencode/bin` hard-coded (`:80`) — README's `OPENCODE_INSTALL_DIR`/`XDG_BIN_DIR`/`$HOME/bin` priority (`README.md:85-97`) is **not implemented** in this script.
- Supported combos `linux-x64|linux-arm64|darwin-x64|darwin-arm64|windows-x64` (`:114-122`) — no `windows-arm64`; Rosetta → arm64 (`:107-112`); `.zip` except linux `.tar.gz`; musl and AVX2 detection identical to postinstall; `filename="$APP-$target$archive_ext"` (`:180`).
- Latest resolution: `registry.npmjs.org/opencode-ai/latest`, HEAD-check the release asset, else newest `v1.x.y` from `api.github.com/repos/anomalyco/opencode/releases?per_page=100` (`:199-216`).
- Skips if `opencode --version` equals target (`:245-258`) → **`--version` must print the bare version**.
- Extract and `mv "$tmp_dir/opencode" "$INSTALL_DIR"` (`:360-368`); PATH edits for fish/zsh/bash/ash/sh, `$GITHUB_PATH` support (`:438-467`).
- Tests: `script/install.test.ts:74-136` (v2 refusal, missing release).

#### A.4.9 Self-upgrade contract inside the binary — `packages/opencode/src/installation/index.ts`
- `Method = curl|npm|yarn|pnpm|bun|brew|scoop|choco|unknown` (`:18`).
- Detection: exe path contains `.opencode/bin` or `.local/bin` → `curl` (`:260-261`); else probe `npm list -g --depth=0`, `yarn global list`, `pnpm list -g --depth=0`, `bun pm ls -g`, `brew list --formula opencode`, `scoop list opencode`, `choco list --limit-output opencode` for `opencode-ai` / `opencode` (`:265-291`).
- `latest`: npm `<registry>/opencode-ai/<NPM_TAG>` where `NPM_TAG = channel` (or `latest` for local) (`:57,222-230`); choco only if community feed has it (`:233-241,302`); brew core → returns current (`:298`).
- `isUpdate`: same major and greater (`:49-53`).
- `upgrade` (`:306-376`): curl → fetch `https://opencode.ai/install`, pipe to `bash` (or `sh`) stdin with `VERSION=<target>` (`:160-186`); `npm|pnpm|bun install -g opencode-ai@<v>`, `yarn global add` (`:316-327`); brew tap `anomalyco/tap/opencode` with `brew tap`, `git pull --ff-only`, version check, `brew upgrade` (`:328-357`); `choco upgrade opencode --version=<v> -y` (`:358-360`); `scoop install opencode@<v>` (`:361-363`); then runs `<execPath> --version`.
- `upgradeMajor`: npm/pnpm/bun/yarn remove `opencode-ai` → install `@opencode/cli` (restore on failure) (`:78-98`); curl → `https://opencode.ai/v2/install` (`:188-197`); brew → `anomalyco/tap/opencode-v2` (`:200-207`).
- Channel → DB filename: `opencode.db` for channels `latest|beta|prod` (or `OPENCODE_DISABLE_CHANNEL_DB`), else `opencode-<channel>.db` (`packages/core/src/database/database.ts:48-54`); channel/version consts from build defines (`packages/core/src/installation/version.ts:6-8`).
- `--version`: yargs `.version("version", …, InstallationVersion)` (`packages/opencode/src/index.ts:51`).

#### A.4.10 Desktop (Electron) release
- Config `packages/desktop/electron-builder.config.ts`: `artifactName: "opencode-desktop-${os}-${arch}.${ext}"` (`:45`); app ids `ai.opencode.desktop.dev|.beta|ai.opencode.desktop` (`:38-42`); mac `dmg`+`zip`, hardened runtime, notarize (`:75-84`); win `nsis` with custom signer `script/sign-windows.ps1` (`:92-105`, Azure TrustedSigning module 0.5.8, timestamp `http://timestamp.acs.microsoft.com` `script/sign-windows.ps1:28,57`); linux `AppImage`, `deb`, `rpm` (`:106-118`); URL scheme `opencode` (`:88-91`); publish feed `github anomalyco/opencode` channel `latest` (prod, `:152`), `anomalyco/opencode-beta` (beta, `:141`).
- Updater: `electron-updater`, channel `latest`, `allowPrerelease=false`, `allowDowngrade=true`, `autoDownload=false` (`packages/desktop/src/main/updater.ts:15-20`).
- Feed files merged and uploaded by `packages/desktop/scripts/finalize-latest-yml.ts`: `latest.yml` (win arm64+x64), `latest-linux.yml`, `latest-linux-arm64.yml`, `latest-mac.yml` (arm64+x64) (`:81-111`), serialized `version/files[url,sha512,size,blockMapSize]/releaseDate` (`:61-71`).
- Legacy Tauri feed `latest.json`: platforms `windows-{x86_64,aarch64}-nsis`, `darwin-{x86_64,aarch64}-app`, `linux-{x86_64,aarch64}-{deb,rpm,appimage}` + aliases (`finalize-latest-json.ts:173-190`), each signed with `bunx @tauri-apps/cli signer sign` (`:129-130`), document `{version, notes:"", pub_date, platforms}` (`:201-204`).
- Server for the desktop app: `prebuild` runs `bun script/build-node.ts` in `packages/opencode` (`packages/desktop/scripts/prebuild.ts:10`), consumed as `../opencode/dist/node` (`packages/desktop/electron.vite.config.ts:6`). Dev channel additionally downloads `@opencode-ai/cli-<target>@0.0.0-next-16350` and copies binary `opencode2[.exe]` to `resources/opencode-cli` (`packages/desktop/scripts/utils.ts:6,16-53,72-92`) — that binary name does not match `packages/cli` output `lildax` (`packages/cli/script/build.ts:12,81`) → producer `UNKNOWN`.
- Public download mapping (`packages/console/app/src/routes/download/[channel]/[platform].ts:4-20`): `darwin-aarch64-dmg→opencode-desktop-mac-arm64.dmg`, `darwin-x64-dmg→opencode-desktop-mac-x64.dmg`, `windows-x64-nsis→opencode-desktop-win-x64.exe`, `linux-x64-deb→opencode-desktop-linux-amd64.deb`, `linux-x64-appimage→opencode-desktop-linux-x86_64.AppImage`, `linux-x64-rpm→opencode-desktop-linux-x86_64.rpm`.

#### A.4.11 Other CI workflows (25 files)
`test.yml` (unit on linux+windows via `bun turbo test`, generated-client check, `test:httpapi`; Playwright e2e for `packages/app`) (`.github/workflows/test.yml:24-151`); `typecheck.yml` (`bun typecheck`, `./script/generate.ts` drift check); `containers.yml` (push to `dev`, `packages/containers/script/build.ts --push`); `storybook.yml`; `stats.yml` (daily `bun script/stats.ts` → commits `STATS.md`); `docs-update.yml` (cron, runs the opencode GitHub Action with `agent: docs`, `model: opencode/gpt-5.2` `:44-52`); `nix-eval.yml`; `bot-nix-hashes.yml`/`check-bot-scope.yml`; repo-hygiene bots (`close-issues`, `close-prs`, `compliance-close`, `duplicate-issues`, `pr-management`, `pr-standards`, `triage`, `review`, `opencode.yml`, `unlock.yml`, `notify-discord.yml`) — several install the CLI via `curl -fsSL https://opencode.ai/install | bash` (`duplicate-issues.yml:41,175`, `pr-management.yml:40`, `review.yml:35`, `triage.yml:41`). Composite action `.github/actions/setup-bun`.

`script/generate.ts`: builds SDK and regenerates `packages/sdk/openapi.json` via `bun dev generate` (OpenAPI is emitted by the CLI itself).

### A.5 Smaller packages

Paths inside each row are relative to that row's package directory unless they start with `packages/`.

| Package | Facts | Rust recommendation | Effort |
|---|---|---|---|
| `packages/enterprise` | Hono API `basePath("/api")` (`src/routes/api/[...path].ts:14`): `GET /doc` (`:17`), `POST /share` (`:30`), `POST /share/:shareID/sync` (`:67`), `GET /share/:shareID/data` (`:95`), `DELETE /share/:shareID` (`:118`), `DELETE /support/actions/remove-share` (`:142`). Storage: S3-compatible via `aws4fetch`; R2 endpoint `https://<account>.r2.cloudflarestorage.com` (`src/core/storage.ts:77-83`; S3 adapter `:66-75`), env `OPENCODE_STORAGE_{ADAPTER,BUCKET,REGION,ACCESS_KEY_ID,SECRET_ACCESS_KEY,ACCOUNT_ID}`; keys `share/<id>`, `share_snapshot/<id>`, `share_compaction/<id>`, `share_event/<id>/…`, `share_data/<id>/{session,message/<id>,part/<msg>/<id>,session_diff,model}` (`src/core/share.ts:79-205`). UI `src/routes/share/[shareID].tsx` (417 lines) uses `@opencode-ai/session-ui`. | Legacy (sharing discontinued). Keep TS frozen. Port only if self-hosted share is a product requirement: axum + `object_store`/`aws-sdk-s3`, static viewer reusing JS `session-ui` | keep **S** / port **L** |
| `packages/stats` | Tables `model_stat` (`core/src/database/schema.ts:4`), `provider_stat` (`:33`), `geo_stat` (`:75`), `model_retention` (`:121`); source = Cloudflare R2 SQL `https://api.sql.cloudflarestorage.com/api/v1/accounts/<id>/r2-sql/query/<bucket>` (`core/src/r2-sql.ts:120`); hourly daemon, daily full pass (`server/src/stat-sync.ts:8-40`). README mentions a `function` package that does not exist (`packages/stats/README.md:3,9,16`). No CLI coupling. | Keep TS. Sync daemon is the only part that would benefit (memory: 2 GB to avoid OOM, `infra/stats.ts:108-110`): Rust `tokio` + `reqwest` + `sqlx` MySQL | keep **S** / daemon **M** / full **XL** |
| `packages/slack` | Bolt app; env `SLACK_BOT_TOKEN`, `SLACK_SIGNING_SECRET`, `SLACK_APP_TOKEN` (`src/index.ts:4-8`); spawns server with `createOpencode` (`:17`); calls `session.create`, `session.share` (`:78,96`, share now fails), `session.prompt` (`:105`), `event.subscribe` (`:24`). | Example; depends on JS SDK. Keep TS or delete. If ported: `slack-morphism` + Rust SDK client | keep **S** / port **M** |
| `packages/identity` | 6 brand marks; zero references | Copy into `opencode-rs/assets/brand/` | **S** |
| `packages/containers` | Images `base`, `bun-node`, `rust`, `tauri-linux`, `publish` → `ghcr.io/anomalyco/build/<name>:24.04`, multi-arch amd64+arm64 (`script/build.ts:10-37`); `rust` image already installs rustup minimal stable (`rust/Dockerfile`) | Keep; extend `rust` image with musl targets / `cargo-zigbuild` (assumption) for release builds | **S** |
| `packages/storybook` | Stories from `ui`, `session-ui`, `app` (`.storybook/main.ts:25-28`) | Stays with the JS web UI; no Rust work | none |
| `packages/script` | §A.4.1 | Re-implement in `xtask` (`cargo xtask version`) with identical env semantics | **S** |
| `packages/effect-drizzle-sqlite`, `packages/effect-sqlite-node` | Generic Drizzle/Effect SQLite adapters; consumed by `packages/core/src/database/database.ts:3-4` (`#sqlite` → `sqlite.bun.ts`/`sqlite.node.ts`) | Replaced by the storage agent's choice (`rusqlite`/`sqlx`); migration SQL compatibility is a storage-spec concern | drop **S** |
| `packages/docs` | Mintlify starter (`README.md:1`, `docs.json` placeholder support link `mailto:hi@mintlify.com`) | Delete | **S** |
| `github/` Action | Installs via v1 install script, runs `opencode github run` with env `MODEL, AGENT, PROMPT, USE_GITHUB_TOKEN, MENTIONS, VARIANT, OIDC_BASE_URL` (`github/action.yml:58-78`) | Keep; works unchanged if Rust binary keeps `opencode github run` + env contract and install paths | **S** |
| `sdks/vscode` | Terminal integration commands `opencode.openNewTerminal`, `opencode.openTerminal`, `opencode.addFilepathToTerminal` (`sdks/vscode/src/extension.ts:9-24`) | Keep TS (extension must be JS); depends only on CLI name/flags | none |

### A.6 Test infrastructure — `packages/http-recorder`

Paths `src/…` in this section are under `packages/http-recorder/`.

**Cassette format v1** (`packages/http-recorder/src/schema.ts:20-87`):
```
{ "version": 1,
  "metadata"?: { <string>: any },              // e.g. name, recordedAt, tags, provider, protocol, route
  "interactions": [
    { "transport": "http",
      "request":  { "method": str, "url": str, "headers": {str:str}, "body": str },
      "response": { "status": num, "headers": {str:str}, "body": str, "bodyEncoding"?: "text"|"base64" } },
    { "transport": "websocket",
      "open": { "url": str, "headers": {str:str} },
      "events": [ { "direction": "client"|"server", "kind": "text", "body": str }
                | { "direction": "client"|"server", "kind": "binary", "body": str, "bodyEncoding": "base64" } ] } ] }
```
- Path: `<directory>/<name>.json`, default directory `<cwd>/test/fixtures/recordings` (`src/cassette.ts:7,41-45`).
- Mode: `CI` set (not `""/false/0`) → replay only; else replay if file exists, otherwise record (`src/recorder.ts:6-18`). No overwrite mode (README "Refresh A Cassette").
- Matching: **strictly sequential** — request *i* must match interaction *i* (`src/matching.ts:95-106`); default matcher compares `JSON.stringify({method,url,headers(sorted),body(canonical JSON if parseable else raw)})` (`src/matching.ts:11-37`). The incoming request is passed through the **same redactor** before comparison (`src/internal-effect.ts:107-110`).
- Header retention: request headers kept only if in allowlist `content-type`, `accept`, `openai-beta` (+ per-test `allowRequestHeaders`/`headers`); response headers `content-type` only (`src/redactor.ts:8-9,108-112`). Sensitive defaults: headers `authorization, cookie, proxy-authorization, set-cookie, x-api-key, x-amz-security-token, x-goog-api-key`; query `access_token, api-key, api_key, apikey, code, key, signature, sig, token, x-amz-credential, x-amz-security-token, x-amz-signature` (`src/redaction.ts:5-28`). Pre-write secret scan (patterns `src/redaction.ts:30-37`, env values `:40-49`).
- Responses are fully buffered (no streaming timing) (README "Current Limits").

**Fixture inventory** (product cassettes):
| Dir | Files | Notes |
|---|---:|---|
| `packages/llm/test/fixtures/recordings/` | 40 | providers: anthropic-messages(+cache), bedrock-converse, cloudflare-ai-gateway, cloudflare-workers-ai, gemini(+cache), openai-chat, openai-compatible-chat (deepseek, groq, openrouter, togetherai), openai-responses(+cache) |
| `packages/opencode/test/fixtures/recordings/session/` | 3 | `native-anthropic-tool-loop`, `native-openai-oauth-tool-loop`, `native-zen-tool-loop` |
| `packages/core/test/fixtures/recordings/session-runner/` | 1 | `openai-chat-streams-text` |
| `packages/http-recorder/test/fixtures/recordings/record-replay/` | 2 | recorder self-tests (`multi-step`, `retry`) |

Measured over the 44 product cassettes: 59 HTTP interactions, 0 WebSocket; response content types `text/event-stream` 52, `application/vnd.amazon.eventstream` 4 (base64 bodies in 3 files), `application/json` 1, none 2; hosts `api.openai.com` 18, `api.anthropic.com` 11, `openrouter.ai` 8, `generativelanguage.googleapis.com` 5, `api.groq.com` 4, `bedrock-runtime.us-east-1.amazonaws.com` 4, `gateway.ai.cloudflare.com` 2, `api.cloudflare.com` 2, `api.together.xyz` 2, `console.opencode.ai` 2, `api.deepseek.com` 1.
Per-test redactions that a Rust replayer must replicate: URL `/proxy/connections/<id>/v1` → `/proxy/connections/{connection}/v1`; body regexes for `wrk_…`, `safety_identifier`, `access/refresh/account_id` (`packages/opencode/test/session/llm-native-recorded.test.ts:234-238,271-282`).

**Reusability verdict:** **Yes, high value.** These cassettes are golden request/response pairs for every LLM wire protocol. Rust replay gives (1) deterministic SSE/eventstream parsing tests and (2) a **request-shape conformance test**: the Rust provider layer must emit byte-equivalent (after canonical JSON) bodies, identical `content-type`/`accept`/`openai-beta` header values, and identical URLs, in the same order. Caveats: TS-specific defaults (field presence, `null` vs omitted, numeric formatting `1` vs `1.0`, header value casing/charset) cause mismatches; use a custom matcher per suite when divergence is intentional. Recording new cassettes from Rust must keep format v1 so TS and Rust can share fixtures during the transition.

---

## Verification

Commands used for evidence (read-only, run in `opencode-ts`):
- Sizes: `find packages/<p> -type f \( -name '*.ts' -o -name '*.tsx' … \) -not -path '*/node_modules/*' | xargs cat | wc -l`.
- Tables: `grep -n -A1 'mysqlTable($' packages/console/core/src/schema/*.sql.ts`; stats `grep -n mysqlTable packages/stats/core/src/database/schema.ts`.
- Coupling: `grep -rnoE 'https?://[a-z0-9.-]*(opencode\.ai|opncd\.ai)…' packages/{opencode,core,app,desktop,tui}/src`.
- Cassette profile: Python `json.load` over `packages/{llm,opencode,core}/test/fixtures/recordings/**/*.json` (counts in §A.6).

Not executed (would confirm behavior, not just source): `bun ./packages/opencode/script/build.ts --single` (asset layout), `bun ./packages/opencode/script/schema.ts out.json out-tui.json` (schema baseline to diff Rust output against), `CI=true bun --cwd packages/llm test` (cassette replay baseline), `bash install --version 1.19.5` in a container.

Recommended conformance gates for the Rust release (all derivable from this repo):
1. `diff <(ts schema.ts) <(opencode-rs schema)` → identical `config.json`/`tui.json` (normalization rules `packages/opencode/script/schema.ts:11-62`: drop `null` from `anyOf`, collapse single `allOf`, integer `maximum = 9007199254740991`, `model`/`small_model` `$ref: https://models.dev/model-schema.json#/$defs/Model`, `allowComments`/`allowTrailingCommas = true`).
2. `bash install` (unchanged file) against a draft release built by Rust CI for each supported combo; check `~/.opencode/bin/opencode --version` prints `X.Y.Z` only.
3. `npm i -g opencode-ai@<preview>` on linux-gnu, alpine, macOS, Windows; `postinstall.mjs` must pick and run the Rust binary.
4. Docker image build from `packages/opencode/Dockerfile` with Rust musl binaries; `RUN opencode --version` (`Dockerfile:17`).
5. Rust cassette replay of all 44 product cassettes in `CI=true` mode.

---

## Implementation

### I.1 Per-package decision table

| Package / area | Decision | Rust target / tooling | Effort | Depends on |
|---|---|---|---|---|
| `console/*` | Keep TS, freeze (superseded by New Console) | — (optional gateway: `axum`/`workers-rs` + `sqlx`) | S (keep) / XL (port) | schema emitter I.3 |
| `function` (api.opencode.ai) | Keep TS | optional `workers-rs` | S / M | GitHub-command contract (integration spec) |
| `web` docs | Keep content + Astro; swap schema hook | optional Zola/mdBook migration | S / L | schema emitter I.3 |
| `docs` (Mintlify) | Delete | — | S | — |
| `enterprise` | Keep frozen (legacy share) | optional axum + object_store | S / L | — |
| `stats` | Keep TS | optional Rust sync daemon | S / M | — |
| `slack` | Keep or delete | optional `slack-morphism` | S / M | JS SDK |
| `identity` | Copy assets | `assets/brand/` | S | — |
| `containers` | Keep; extend `rust` image | `cargo-zigbuild`/musl targets (assumption) | S | — |
| `http-recorder` | **Port replayer (Rust test crate)**, keep format v1 | `reqwest-middleware` + `serde_json` | M | LLM/provider crates |
| `storybook` | Stays with JS UI | — | none | — |
| `script` | Port to `xtask` | `cargo xtask` | S | — |
| `effect-*sqlite*` | Drop | storage agent's SQLite crate | S | storage spec |
| Release pipeline | **Rewrite build-cli; keep sign/publish/feeds** | GitHub Actions + cargo | L | everything above in the binary |
| Nix | Rewrite derivation | `crane` or `rustPlatform.buildRustPackage` (assumption) | M | release pipeline, web UI build |
| `install`, `postinstall.mjs`, AUR/Homebrew/Docker templating | Reuse unchanged | — | S | asset-name parity |

### I.2 Rust release pipeline (compatible artefacts)

Target mapping (asset names must stay byte-identical):

| Asset / npm package | Rust triple | Notes |
|---|---|---|
| `opencode-linux-x64` / `-baseline` | `x86_64-unknown-linux-gnu` | assumption: build once with default `x86-64` baseline and publish under both names; optional `-C target-cpu=x86-64-v3` for non-baseline later. glibc floor `UNKNOWN` for current Bun binaries — pick one via `cargo zigbuild --target x86_64-unknown-linux-gnu.2.17` (assumption) |
| `opencode-linux-arm64` | `aarch64-unknown-linux-gnu` | — |
| `opencode-linux-x64-musl` / `-baseline-musl` | `x86_64-unknown-linux-musl` | static; Docker amd64 uses `-baseline-musl` |
| `opencode-linux-arm64-musl` | `aarch64-unknown-linux-musl` | Docker arm64 |
| `opencode-darwin-arm64` | `aarch64-apple-darwin` | sign + (recommend) notarize |
| `opencode-darwin-x64` / `-baseline` | `x86_64-apple-darwin` | same binary under both names |
| `opencode-windows-x64` / `-baseline` | `x86_64-pc-windows-msvc` | Azure signing unchanged |
| `opencode-windows-arm64` | `aarch64-pc-windows-msvc` | not installable via `install` (no combo, `install:114-122`) |

Build-time parity checklist for the Rust binary (`build.rs`/`xtask`):
1. `OPENCODE_VERSION`, `OPENCODE_CHANNEL` compiled in (`env!`), channel semantics per §A.4.1; DB filename rule §A.4.9.
2. models.dev snapshot embedded from `${OPENCODE_MODELS_URL:-https://models.dev}/api.json` or `MODELS_DEV_API_JSON` file (`packages/opencode/script/generate.ts:10-13`) via `include_str!` of a fetched file.
3. Web UI: still built by Vite from `packages/app` (JS toolchain stays in CI), embedded with `rust-embed`/`include_dir` (assumption), skippable flag equivalent to `--skip-embed-web-ui`; `.map` files excluded (`build.ts:33`).
4. HTTP UA `opencode/<version>` (`build.ts:180`); system CA store (`--use-system-ca`) → `rustls-platform-verifier` or `native-tls` (assumption).
5. `opencode --version` → bare `X.Y.Z` (install `:250-252`, postinstall `:158-165`, Dockerfile `:17`).
6. Install-method detection via `std::env::current_exe()` with the same substrings and probes (§A.4.9), same upgrade commands and `VERSION` env for curl installer.
7. JSON schema subcommand/xtask producing `config.json` + `tui.json` (I.3).
8. OpenAPI emission compatible with `packages/sdk/openapi.json` (served at `opencode.ai/openapi.json` from raw GitHub `dev` branch, `routes/openapi.json.ts:2-4`) — API agent owns content; release must still run `script/generate.ts`-equivalent and publish `@opencode-ai/sdk`.

Pipeline (keep job graph of `publish.yml`, swap only producers):
```
version (xtask version → GITHUB_OUTPUT version/release/tag/repo; draft release)
build-cli  matrix{8 distinct triples} → cargo build --release --locked --target T
           → stage dist/<asset-name>/bin/opencode[.exe] + dist/<asset-name>/package.json
             {name, version, preferUnplugged:true, os, cpu, libc?}   (copy for *-baseline names)
           → tar.gz (linux) / zip (darwin, windows) from inside bin/   (gh release upload --clobber)
sign-cli-macos   (unchanged; drop JIT entitlements – assumption: no JIT in Rust; add notarytool)
sign-cli-windows (unchanged file list)
build-electron   (unchanged unless desktop is ported; prebuild currently needs build-node.ts → must point at Rust sidecar)
publish          (unchanged: packages/opencode/script/publish.ts consumes dist/*/package.json)
```
`packages/opencode/script/publish.ts` can be reused verbatim (it only reads `dist/*/package.json`, packs, publishes, builds Docker, AUR, Homebrew). Move it to `opencode-rs/xtask/` or keep invoking it with Bun until rewritten.

Desktop continuity (if desktop is rewritten by the clients agent, e.g. Tauri): existing Electron installs poll `latest*.yml` on `anomalyco/opencode` (prod) / `anomalyco/opencode-beta` (beta); legacy Tauri installs poll `latest.json` signed with the Tauri updater key (`script/publish.ts:51-53`). Any new desktop must either keep publishing these exact files or ship one bridging Electron release. Feasibility of an Electron→Tauri bridge via electron-updater: `UNKNOWN` (assumption: possible by publishing the new NSIS/dmg-zip as the update payload).

Major-version decision (load-bearing): v1 `install` refuses ≥2.x (`install:70-78`), `isUpdate` only within a major (`installation/index.ts:49-53`), `opencode-ai` npm is "OpenCode 1 only" and 2.x ships as `@opencode/cli` (`installation/index.ts:55-56`), Homebrew core `opencode` is already 2.x (`:297`), release CI publishes only from branch `v1` (`publish.yml:38`). → If the Rust port ships as **1.x** it must use this pipeline; if **2.x+**, it must target the v2 channels (`@opencode/cli`, `https://opencode.ai/v2/install`, `anomalyco/tap/opencode-v2`) whose producers are **not in this repo**.

### I.3 Schema emitter (unblocks console, web, nix)
- Current consumers: `packages/console/app/package.json:10`, `packages/web/astro.config.mjs:321`, `nix/opencode.nix:57,66`.
- Provide `opencode-rs` command or `cargo xtask schema <config.json> [tui.json]` with identical argv contract to `packages/opencode/script/schema.ts:68-77`, generated by `schemars` (assumption) + post-processing replicating `normalize`/`restoreModelRefs` (`schema.ts:26-62`). Gate with diff test vs the TS output (Verification #1). Effort **M** (most of it is schema-shape parity, owned jointly with the config spec).

### I.4 Rust cassette replayer (test crate `opencode-test-cassette`, sketch)
```rust
#[derive(serde::Deserialize, serde::Serialize)]
pub struct Cassette { pub version: u8, #[serde(default)] pub metadata: Option<serde_json::Map<String, serde_json::Value>>, pub interactions: Vec<Interaction> }
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(tag = "transport", rename_all = "lowercase")]
pub enum Interaction { Http { request: Req, response: Res }, Websocket { open: Open, events: Vec<WsEvent> } }
#[derive(serde::Deserialize, serde::Serialize, Clone)]
pub struct Req { pub method: String, pub url: String, pub headers: std::collections::BTreeMap<String, String>, pub body: String }
#[derive(serde::Deserialize, serde::Serialize, Clone)]
pub struct Res { pub status: u16, pub headers: std::collections::BTreeMap<String, String>, pub body: String, #[serde(rename = "bodyEncoding", default)] pub body_encoding: Option<String> }
#[derive(serde::Deserialize, serde::Serialize)]
pub struct Open { pub url: String, pub headers: std::collections::BTreeMap<String, String> }
#[derive(serde::Deserialize, serde::Serialize)]
pub struct WsEvent { pub direction: String, pub kind: String, pub body: String, #[serde(rename = "bodyEncoding", default)] pub body_encoding: Option<String> }

pub fn canonical(v: serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(m) => serde_json::Value::Object(m.into_iter().map(|(k, v)| (k, canonical(v))).collect::<std::collections::BTreeMap<_, _>>().into_iter().collect()),
        serde_json::Value::Array(a) => serde_json::Value::Array(a.into_iter().map(canonical).collect()),
        other => other,
    }
}
pub fn same(a: &Req, b: &Req) -> bool {
    let body = |s: &str| serde_json::from_str::<serde_json::Value>(s).map(canonical).unwrap_or(serde_json::Value::String(s.to_owned()));
    a.method == b.method && a.url == b.url && a.headers == b.headers && body(&a.body) == body(&b.body)
}
```
Replay middleware: lowercase header names, keep only allowlisted request headers (`content-type`, `accept`, `openai-beta` + suite extras), apply suite URL/body redactions, compare with interaction[`index`] (atomic counter), return `status`/`content-type`/decoded body (base64 when `bodyEncoding == "base64"`) as a single-chunk stream. `serde_json` needs the `preserve_order` feature **off** for `BTreeMap` canonicalization semantics shown above (assumption about crate features in workspace). Recording: only after Rust parity is reached; write v1 with the same redaction defaults and secret scan.

### I.5 Order of work
1. `xtask version` (A.4.1) + compile-time constants + `--version` parity.
2. Schema emitter (I.3) — unblocks dropping TS config code without breaking console/web/nix builds.
3. Cassette replayer (I.4) — needed by LLM/provider crates early.
4. `build-cli` cargo matrix + staging layout; reuse `publish.ts`, `postinstall.mjs`, `install`, Dockerfile.
5. Signing jobs adjust (entitlements, notarization), nix derivation.
6. Desktop sidecar switch (`packages/desktop/scripts/prebuild.ts:10`, `electron.vite.config.ts:6`) once the Rust server exists (clients agent).
7. Optional: `function` / stats daemon / docs SSG migrations.

---

## Result

- **Port scope in this area is mostly build/release, not services.** Hosted packages (console, function, enterprise, stats, web) stay TypeScript on Cloudflare/AWS; the only hard dependency they have on CLI source is `packages/opencode/script/schema.ts` → replace with a Rust schema emitter.
- **Reusable as-is:** root `install`, `packages/opencode/script/postinstall.mjs`, `packages/opencode/script/publish.ts` (npm/Docker/AUR/Homebrew), `packages/opencode/Dockerfile`, `script/sign-windows.ps1`, desktop feed finalizers, all signing jobs — provided asset names and `dist/<name>/{bin,package.json}` layout are preserved.
- **Reusable test data:** 44 product cassettes (59 interactions, 11 upstream hosts, SSE + AWS eventstream) as Rust golden tests; format v1 fully specified above.

Top porting risks:
1. **Major-version/channel gating** (§I.2): wrong version line silently breaks `opencode upgrade`, installer, Homebrew, npm (`install:70-78`, `installation/index.ts:49-57,78-98`).
2. **Config JSON Schema drift** at `https://opencode.ai/config.json` (built from CLI code by console/web/nix) — editor validation for all users.
3. **Request-shape divergence** vs cassettes (header allowlist values, `null`/omitted fields, URL redaction) — fails replay or hides real API incompatibility.
4. **Desktop updater continuity** (electron-updater `latest*.yml`, legacy Tauri `latest.json`) if desktop tech changes; desktop server today is the Node build of the TS CLI.
5. **Out-of-repo services the binary depends on** (New Console device auth/`/api/config`, `console.opencode.ai/proxy`, `opencode.ai/install` hosting, `update/api`, models.dev) — protocols must be captured from live traffic, not from this repo.
6. Release pipeline still needs Bun/Node for `packages/app` (embedded web UI), SDK/plugin/ui npm publishes, VS Code extension, GitHub Action — a "pure Rust" CI is not achievable while those stay JS.

---

## Required Additional Data

| # | UNKNOWN | Why it matters | How to determine |
|---|---|---|---|
| 1 | Version line of the Rust port (1.x on `v1` vs 2.x via `@opencode/cli`) and who produces OpenCode 2 artefacts (`@opencode/cli`, `opencode.ai/v2/install`, `anomalyco/tap/opencode-v2`, `@opencode-ai/cli-*@0.0.0-next-16350` with binary `opencode2`) | Decides which pipeline/installer the Rust binary uses | Ask maintainers; `npm view @opencode/cli versions dist-tags`; `npm view @opencode-ai/cli-linux-arm64@0.0.0-next-16350 dist.tarball` and inspect `bin/` |
| 2 | Hosting of `https://opencode.ai/install` and `/v2/install` | Upgrade path for curl installs | `curl -sIL https://opencode.ai/install`; `curl -s https://opencode.ai/install \| diff - install` |
| 3 | `https://opencode.ai/update/api/{latest,beta}/desktop/opencode` service and schema | Desktop download page redirects | `curl -s https://opencode.ai/update/api/latest/desktop/opencode \| jq .` |
| 4 | New Console API (`/console/auth/device/*`, `/console/api/{config,orgs,user}`, `/inference`, `console.opencode.ai/proxy/connections/*`) | CLI login/provider config | Capture with the TS CLI (`opencode auth login` / account login) through a proxy (mitmproxy) and record cassettes |
| 5 | Zen/Go base URLs and provider entries in models.dev | `opencode` provider routing | `curl -s https://models.dev/api.json \| jq '.opencode.api, ."opencode-go".api'` |
| 6 | glibc minimum of current Bun-built linux-gnu binaries | Choose Rust glibc floor without regressing users | Download `opencode-linux-x64.tar.gz` from the v1.19.5 release; `objdump -T opencode \| grep -o 'GLIBC_[0-9.]*' \| sort -uV \| tail -1` |
| 7 | Deployment mechanism for `dev`/`production` SST stages (console, api, web, enterprise, stats) | Whether any CI must remain for hosted packages | Check other branches (`git log origin/dev -- .github/workflows`), SST console/Autodeploy settings |
| 8 | Scoop/Chocolatey/mise/nixpkgs/Homebrew-cask manifest maintainers | Their URLs hard-code asset names | Inspect `ScoopInstaller/Main` `bucket/opencode.json`, Chocolatey `opencode` nuspec, `nixpkgs` `pkgs/by-name/op/opencode`, Homebrew cask `opencode-desktop` |
| 9 | Whether workers-rs in the pinned version supports Durable Object WebSocket hibernation used by `SyncServer` | Only if `function` is ported | Check `workers-rs` docs/changelog for `DurableObject` + `acceptWebSocket` |
| 10 | Exact OpenAPI/SDK regeneration contract after the server is in Rust | `@opencode-ai/sdk` publish in `script/publish.ts:42` | Owned by API spec; diff `packages/sdk/openapi.json` against Rust emitter output |
