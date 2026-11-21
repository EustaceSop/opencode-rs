# 03 — LLM / Provider Layer (opencode v1.19.5 → Rust)

Source root: `D:\Download\opecode\opencode-ts` (read-only). Path prefixes below are relative to `packages/`
(e.g. `opencode/src/provider/provider.ts:216` = `packages/opencode/src/provider/provider.ts` line 216).
`patches/…` is repo-root relative. `UNKNOWN` = not determinable from in-repo source; resolution listed in §10.
Confidence tags: **[src]** read directly from source; **[cache]** observed in operator's local models.dev cache
(`C:\Users\<user>\.cache\opencode\models.json`, 232 providers) — data, not code, may drift; **[assumption]** inference.

---

## 1. Architecture

### 1.1 Two runtimes, one event stream [src]

| Runtime | Package | Status in v1.19.5 | Selected when |
|---|---|---|---|
| **AI SDK** (Vercel `ai` + `@ai-sdk/*` npm packages, plus in-repo custom Copilot SDK) | `opencode/src/session/llm.ts:279` `streamText({...})` | **Default for every provider** | always, unless native gate passes |
| **Native** `@opencode-ai/llm` (`packages/llm`) — opencode's own Effect-based reimplementation of the wire protocols | `opencode/src/session/llm.ts:225-252` → `session/llm/native-runtime.ts` | Opt-in, experimental | `OPENCODE_EXPERIMENTAL_NATIVE_LLM` truthy (`opencode/src/effect/runtime-flags.ts:54`) **and** gate passes |

- `session/llm/AGENTS.md:88` claims the umbrella `OPENCODE_EXPERIMENTAL=true` also opts in; **code disagrees**: flag is
  `bool("OPENCODE_EXPERIMENTAL_NATIVE_LLM")`, not `enabledByExperimental(...)` (`runtime-flags.ts:54` vs `:11-14`). Code wins.
- Native gate (`session/llm/native-runtime.ts:50-72`): providerID ∈ {`openai`,`anthropic`, `opencode*`} **and** npm ∈
  {`@ai-sdk/openai`,`@ai-sdk/openai-compatible`,`@ai-sdk/anthropic`} **and** (auth not oauth, or provider `openai` with a
  plugin fetch override) **and** an API key (`provider.options.apiKey` string else `provider.key`). Otherwise falls back to AI SDK
  with a logged reason (`llm.ts:253-267`).
- Both paths emit `LLMEvent` (schema in `llm/src/schema/events.ts`); AI SDK `fullStream` parts are converted by
  `opencode/src/session/llm/ai-sdk.ts:77-289`. The session processor only sees `LLMEvent` (`session/processor.ts:280-548`).
- `packages/llm` is the in-repo, readable description of the wire protocols opencode intends to speak; the AI SDK npm
  packages themselves are **not in the repo** (no `node_modules`), only patches (`patches/@ai-sdk%2F*.patch`, §5.10).

**Porting consequence:** "identical to original" = identical to what `@ai-sdk/<pkg>@<version>` (+ patches) sends. Versions are
pinned in `opencode/package.json:58-77` (e.g. `@ai-sdk/anthropic` 3.0.111, `@ai-sdk/openai` 3.0.88,
`@ai-sdk/openai-compatible` 2.0.41, `@ai-sdk/amazon-bedrock` 4.0.166, `@ai-sdk/google` 3.0.73, `@ai-sdk/google-vertex`
4.0.181, `@ai-sdk/xai` 3.0.139, `@openrouter/ai-sdk-provider` 2.9.0, `gitlab-ai-provider` 6.23.1, `ai-gateway-provider` 3.2.0).
`ai` core = `6.0.168` (root `package.json:67`, workspace catalog).

### 1.2 Call chain (default path) [src]

```
SessionPrompt loop ──LLM.Service.stream(StreamInput)──▶ session/llm.ts:356
  ├─ Provider.getLanguage(model)        provider/provider.ts:1921  (npm factory → LanguageModelV3, cached per "provider/model")
  ├─ Config.get, Provider.getProvider, Auth.get(providerID)        llm.ts:94-102
  ├─ LLMRequestPrep.prepare(...)        session/llm/request.ts:55  (system, options, params, headers, tools)
  ├─ [native gate]                      llm.ts:225
  └─ streamText({ model: wrapLanguageModel(language, middleware: ProviderTransform.message on prompt), ...})  llm.ts:279-352
        fullStream ──▶ LLMAISDK.toLLMEvents ──▶ Stream<LLMEvent>                                             llm.ts:371-377
```

`StreamInput` boundary (`session/llm.ts:34-47`): `{ user, sessionID, parentSessionID?, model: Provider.Model, agent, permission?,
system: string[], messages: ModelMessage[] (AI-SDK shape), small?, tools: Record<string, Tool>, retries?, toolChoice? }`.
Callers: main loop (via `SessionProcessor.process`, `processor.ts:641-697`), title generation (`prompt.ts:225-236`, `small:true`,
`retries:2`, `tools:{}`).

### 1.3 npm id → SDK factory → wire protocol

Bundled factories (`opencode/src/provider/provider.ts:148-175`) [src]; protocol column from native reimplementation /
package identity [assumption where marked]:

| `model.api.npm` | Factory | Wire protocol | Count in cache [cache] |
|---|---|---|---|
| `@ai-sdk/openai` | `createOpenAI` | OpenAI **Responses** (`getModel`→`sdk.responses()` for `openai`, `meta`; `provider.ts:243-257`); other providers using this npm call `sdk.languageModel()` = Responses in AI SDK v3 [assumption, verify §10] | 6 prov + 146 model overrides |
| `@ai-sdk/openai-compatible` | `createOpenAICompatible` | OpenAI Chat Completions | 193 prov + 62 overrides (default when npm unset: `provider.ts:1329,1555`) |
| `@ai-sdk/anthropic` | `createAnthropic` | Anthropic Messages | 8 + 149 |
| `@ai-sdk/google` | `createGoogleGenerativeAI` | Gemini `generateContent`/`streamGenerateContent` | 1 + 17 |
| `@ai-sdk/google-vertex` | `createVertex` | Gemini on Vertex | 1 |
| `@ai-sdk/google-vertex/anthropic` | `createVertexAnthropic` | Anthropic Messages on Vertex (`rawPredict`/`streamRawPredict`) [assumption] | 1 + 17 |
| `@ai-sdk/amazon-bedrock` | `createAmazonBedrock` | Bedrock Converse / ConverseStream + SigV4 or bearer | 1 |
| `@ai-sdk/amazon-bedrock/mantle` | `createBedrockMantle` | OpenAI Responses (chat for `openai.gpt-oss-safeguard-*`, `provider.ts:203-207`) | 16 overrides |
| `@ai-sdk/azure` | `createAzure` | Responses / Chat (`selectAzureLanguageModel`, `provider.ts:195-201`) | 2 |
| `@ai-sdk/xai` | `createXai` | `sdk.responses()` (`provider.ts:258-265`) | 1 |
| `@ai-sdk/github-copilot` | **in-repo** `core/src/github-copilot/copilot-provider.ts` `createOpenaiCompatible` | Chat or Responses (§2.7.4) | via plugin |
| `@openrouter/ai-sdk-provider` | `createOpenRouter` | OpenAI-chat-like + `reasoning`/`usage` extensions | 1 + 40 |
| `@ai-sdk/mistral`,`groq`,`deepinfra`,`cerebras`,`cohere`,`togetherai`,`perplexity`,`vercel`,`alibaba`,`gateway` | `create*` | vendor-specific / chat-like | 1 each |
| `gitlab-ai-provider` | `createGitLab` | GitLab Duo (agenticChat / workflowChat over WS) — external | 1 |
| `venice-ai-sdk-provider` | `createVenice` | chat-like | 1 |
| `ai-gateway-provider` | (custom loader builds clients, `provider.ts:848-931`) | CF AI Gateway | 1 |
| anything else (e.g. `@jerome-benoit/sap-ai-provider-v2`, `watsonx-ai-provider`, `@aihubmix/…`, `merge-gateway-ai-sdk-provider`) | `Npm.add(npm)` at runtime, first export starting with `create` (`provider.ts:1867-1887`); `file://` npm values imported directly | arbitrary JS | — |

**Rust mapping:** implement a fixed `enum WireProtocol { OpenAIChat, OpenAIResponses, AnthropicMessages, Gemini, VertexGemini,
VertexAnthropic, BedrockConverse, CopilotChat, CopilotResponses, GitLabDuo, … }` keyed by npm string. Dynamic npm loading
cannot be ported; unknown npm → map to `OpenAIChat` if the package is a known openai-compatible wrapper, else error
`ProviderInitError` (`provider.ts:1208-1219`). Prioritise: openai-compatible, openai(responses), anthropic, google, copilot,
bedrock, vertex, openrouter, azure, xai.

### 1.4 v2 core files

`core/src/{catalog.ts,aisdk.ts,provider.ts,model.ts}` and `core/src/plugin/provider/*` belong to the v2 core/server stack; the v1
CLI request path uses `opencode/src/provider/provider.ts` and only imports `ModelsDev`, `ProviderV2.ID`, `ModelV2.ID` from core
(`provider.ts:13,29-30`). See §2.8 for what differs.

---

## 2. Provider registry (v1 runtime)

### 2.1 Data types [src]

`Provider.Model` (`provider.ts:1129-1143`): `id, providerID, api{id,url,npm}, name, family?, capabilities{temperature,
reasoning, attachment, toolcall, input{text,audio,image,video,pdf}, output{…}, interleaved: bool | {field}}, cost{input,
output, cache{read,write}, tiers?[{input,output,cache,tier{type:"context",size}}], experimentalOver200K?}, limit{context,
input?, output}, status ("alpha"|"beta"|"deprecated"|"active"), options: Record, headers: Record<string,string>,
release_date, variants?: Record<string, Record>}`.
`Provider.Info` (`provider.ts:1146-1154`): `id, name, source: "env"|"config"|"custom"|"api", env: string[], key?, options,
models`.

`fromModelsDevModel` (`provider.ts:1316-1371`): `api.url = model.provider?.api ?? provider.api ?? ""`; `api.npm =
cloudflareGatewayNpm(...) ?? model.provider?.npm ?? provider.npm ?? "@ai-sdk/openai-compatible"`; `toolcall` default
`true`, other caps default `false`; `interleaved` string → `{field}`; variants = `reasoningVariants(model) ??
variants(base)`. `experimental.modes` produce extra models `${id}-${mode}` with name `"<Name> <Mode>"`, merged cost, options
from `modeOptions` (snake→camel keys; for `@ai-sdk/openai` `reasoning.mode` → `reasoningMode`, `provider.ts:1400-1409`), and
mode headers (`provider.ts:1377-1388`).

### 2.2 State build pipeline (per instance/project) — order matters [src] (`provider.ts:1451-1783`)

1. `modelsDev = ModelsDev.get()`; `catalog = mapValues(modelsDev, fromModelsDevProvider)`; `database = toPublicInfo(catalog)` (deep JSON clone, drops functions) (`:1455-1457`).
2. `plugins = plugin.list()` — plugin `config()` hooks already ran, so `cfg.provider` may contain plugin-injected providers (`:1491-1492`).
3. `disabled = cfg.disabled_providers`, `enabled = cfg.enabled_providers` (allow-list if set) (`:1496-1503`).
4. Plugin `provider.models(provider, {auth})` hook **replaces** `database[id].models` (skips disabled / unknown ids) (`:1505-1530`).
5. Config providers (`cfg.provider[id]`) extend/override `database` (`:1533-1634`). Per model: `api.id = model.id ?? existing.api.id ?? key`; `npm = model.provider.npm ?? provider.npm ?? existing.npm ?? cloudflareGatewayNpm ?? modelsDev[id].npm ?? "@ai-sdk/openai-compatible"`; `url = model.provider.api ?? provider.api ?? existing.url ?? modelsDev[id].api ?? ""`; caps fall back to existing then defaults (input/output `text` default **true** here, unlike catalog); DeepSeek on openai-compatible without catalog entry gets `interleaved={field:"reasoning_content"}` (`:1596-1598`); variants: `reasoning_options` set → recompute; same npm → keep catalog variants; else recompute; then `mergeDeep(variants, model.variants)` and drop `disabled:true` (`:1620-1630`).
6. **Env**: for each database provider, first truthy `env[provider.env[i]]` → `source:"env"`, `key` set **only if `provider.env.length === 1`** (`:1637-1647`).
7. **auth.json api entries** → `source:"api"`, `key = auth.key` (`:1650-1660`).
8. **Plugin auth loaders**: for each plugin with `auth.provider` and a stored auth entry → `options = await auth.loader(getAuth, publicProviderInfo)`; merged into provider options (`:1663-1681`).
9. **Custom loaders** `custom(dep)` (§2.3): run for every database id not disabled; result is applied when `autoload || providers[id]` exists — registers `getModel`, `vars`, `discoverModels`, merges `options` (`:1683-1699`).
10. **Config re-apply**: every `cfg.provider[id]` becomes active with `source:"config"`, merging `env`, `name`, `options` (`:1702-1709`). ⇒ configured providers are always active even without a key.
11. GitLab model discovery (`:1711-1723`).
12. Per-provider filter (`:1725-1773`): drop if not allowed; delete `gpt-5-chat-latest` for `openai`/`github-copilot`/`openrouter` and `openai/gpt-5-chat` for openrouter; drop `alpha` unless `OPENCODE_ENABLE_EXPERIMENTAL_MODELS`; drop `deprecated`; config `blacklist`/`whitelist`; default variants; merge config variants; drop providers with 0 models.

`mergeProvider(id, patch)` (`:1478-1489`): if active → `mergeDeep(existing, patch)`; else `mergeDeep(database[id], patch)` (ignored if not in database). `mergeDeep` = remeda (arrays replaced, objects merged) **[assumption]**.

### 2.3 Custom loaders (`provider.ts:209-1055`) [src]

| Provider id | autoload | Options / headers injected | `getModel` | Notes |
|---|---|---|---|---|
| `anthropic` | false | `headers["anthropic-beta"]="interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14"` (`:216`) | default | patched SDK adds more betas (§5.10) |
| `opencode` | `models>0` | no key anywhere (env/auth/config `apiKey`) → `apiKey:"public"` and **delete all models with `cost.input !== 0`** (`:220-242`) | default | |
| `openai` | false | `headerTimeout: 300_000` (`:35,249`) | `sdk.responses(id)` | |
| `meta` | false | — | `sdk.responses(id)` | |
| `xai` | false | — | `sdk.responses(id)` | |
| `github-copilot` | false | — | `model.api.endpoint` "responses"/"chat" if present; else `/^gpt-(\d+)/` ≥5 and not `gpt-5-mini*` → responses, else chat; SDK without chat/responses → `languageModel` (`:266-280`) | auth via plugin (§4.3) |
| `azure` | false | `resourceName` = first non-blank of `options.resourceName`, api-auth `metadata.resourceName`, oauth `accountId`, `env.AZURE_RESOURCE_NAME`; missing & no baseURL → getModel throws `"AZURE_RESOURCE_NAME is missing, …"`; `vars → AZURE_RESOURCE_NAME` (`:281-321`) | `selectAzureLanguageModel(sdk,id,useCompletionUrls)`: chat if `useCompletionUrls`, else responses → messages → chat → languageModel (`:195-201`) | |
| `azure-cognitive-services` | false | `baseURL = https://${AZURE_COGNITIVE_SERVICES_RESOURCE_NAME}.cognitiveservices.azure.com/openai` + (`useDeploymentBasedUrls` ? "" : "/v1") (`:322-335`) | same selector | |
| `amazon-bedrock` | true if any of profile/`AWS_ACCESS_KEY_ID`/bearer/config apiKey/`AWS_WEB_IDENTITY_TOKEN_FILE`/container creds (`AWS_CONTAINER_CREDENTIALS_RELATIVE_URI`/`_FULL_URI`) | `region = config.options.region ?? AWS_REGION ?? "us-east-1"`; `profile = config.options.profile ?? AWS_PROFILE`; bearer = `process.env.AWS_BEARER_TOKEN_BEDROCK` else api-auth key (also written back to `process.env`); no bearer & no config apiKey → `credentialProvider = fromNodeProviderChain({profile?})`; `baseURL = options.endpoint ?? options.baseURL`; `vars → AWS_REGION` (`:336-408`) | mantle npm → §1.3; `arn:`/already-prefixed (`global. us. eu. jp. apac. au.`) untouched; else cross-region prefixing (below) | |
| `llmgateway` | false | `HTTP-Referer: https://opencode.ai/`, `X-Title: opencode`, `X-Source: opencode` (`:502-512`) | | |
| `openrouter` | false | `HTTP-Referer: https://opencode.ai/`, `X-Title: opencode` (`:513-522`) | | |
| `nvidia` | `source==="config"` | + `X-BILLING-INVOKE-ORIGIN: OpenCode` (`:523-533`) | | |
| `vercel` | false | lowercase `http-referer`, `x-title` (`:534-543`) | | |
| `zenmux`, `kilo` | false | `HTTP-Referer`, `X-Title` (`:639-648`, `:945-954`) | | |
| `cerebras` | false | `X-Cerebras-3rd-Party-Integration: opencode` (`:936-944`) | | |
| `google-vertex` | `Boolean(project)` | `project = options.project ?? GOOGLE_VERTEX_PROJECT ?? GOOGLE_CLOUD_PROJECT ?? GCP_PROJECT ?? GCLOUD_PROJECT`; `location = options.location ?? GOOGLE_VERTEX_LOCATION ?? GOOGLE_CLOUD_LOCATION ?? VERTEX_LOCATION ?? "us-central1"`; custom `fetch` adds `Authorization: Bearer <ADC token>` (scope `https://www.googleapis.com/auth/cloud-platform`); `vars → GOOGLE_VERTEX_PROJECT, GOOGLE_VERTEX_LOCATION, GOOGLE_VERTEX_ENDPOINT` (`:544-594`) | `languageModel(id.trim())` | `resolveSDK` **deletes this fetch unless npm is openai-compatible** (`:1805-1807`) |
| `google-vertex-anthropic` | `Boolean(project)` | project from `GOOGLE_CLOUD_PROJECT/GCP_PROJECT/GCLOUD_PROJECT`; location `GOOGLE_CLOUD_LOCATION ?? VERTEX_LOCATION ?? "global"`; baseURL only for `eu`/`us` (`:595-614`) | | |
| `sap-ai-core` | `AICORE_SERVICE_KEY` (env or api-auth, written back to env) | `{deploymentId: AICORE_DEPLOYMENT_ID, resourceGroup: AICORE_RESOURCE_GROUP}` (`:615-638`) | `sdk(modelID)` | |
| `gitlab` | `!!token` | `instanceUrl = GITLAB_INSTANCE_URL \|\| "https://gitlab.com"`; token = oauth access / api key / `GITLAB_TOKEN`; `aiGatewayHeaders = {User-Agent: "opencode/<ver> gitlab-ai-provider/<ver> (<platform> <release>; <arch>)", "anthropic-beta":"context-1m-2025-08-07", ...config}`; `featureFlags {duo_agent_platform_agentic_chat:true, duo_agent_platform:true, ...}` (`:649-684`) | `duo-workflow-*` → `sdk.workflowChat(...)` (+`selectedModelRef`); else `sdk.agenticChat(id,{aiGatewayHeaders,featureFlags})` (`:685-705`) | discovery header `PRIVATE-TOKEN` (api) or `Authorization: Bearer` (oauth) (`:713-714`) |
| `cloudflare-workers-ai` | `!!apiKey` (skip if `baseURL` set) | `apiKey = CLOUDFLARE_API_KEY \|\| auth.key`; `User-Agent: opencode/<ver> cloudflare-workers-ai (...)`; `vars → CLOUDFLARE_ACCOUNT_ID` (env or `auth.metadata.accountId`) (`:774-811`) | | |
| `cloudflare-ai-gateway` | true (needs account+gateway, else getModel throws; no token → **throws at load**) | token `CLOUDFLARE_API_TOKEN \|\| CF_AIG_TOKEN \|\| auth.key`; gateway id `CLOUDFLARE_GATEWAY_ID \|\| auth.metadata.gatewayId` (`:812-935`) | `openai/*` → ai-gateway OpenAI passthrough; `anthropic/*` → Anthropic passthrough with `.`→`-` in id; `workers-ai/*`,`@cf/*` → unified w/ token; else openai-compatible at `https://api.cloudflare.com/client/v4/accounts/${accountId}/ai/v1` + header `cf-aig-gateway-id` | binding `run()` POSTs to `https://gateway.ai.cloudflare.com/v1/${accountId}/${gateway}` with `cf-aig-authorization: Bearer <token>` + `parseAiGatewayOptions` headers (`:882-895`) |
| `snowflake-cortex` | `source==="config"` | `baseURL = https://${account}.snowflakecomputing.com/api/v2/cortex/v1`; token precedence env `SNOWFLAKE_CORTEX_TOKEN`/`SNOWFLAKE_CORTEX_PAT` → api key → oauth access → config `token`/`apiKey`; non-oauth: fetch rewrites `max_tokens`→`max_completion_tokens`, 400 containing "conversation complete" → synthetic `{choices:[{finish_reason:"stop",message:{content:"",role:"assistant"}}]}`, SSE text `"role":""` → `"role":"assistant"` (`:955-1054`) | | |

Bedrock cross-region prefix (`provider.ts:418-496`): region prefix = `region.split("-")[0]`.
`us` (not `us-gov*`): prefix `us.` if id contains any of `nova-micro,nova-lite,nova-pro,nova-premier,nova-2,claude,deepseek.r1`.
`eu`: if region ∈ {eu-west-1,eu-west-2,eu-west-3,eu-north-1,eu-central-1,eu-south-1,eu-south-2} and id contains
`claude,nova-lite,nova-micro,llama3,pixtral` → `eu.`. `ap`: `ap-southeast-2|4` + (`anthropic.claude-sonnet-4-5`|`anthropic.claude-haiku`) → `au.`;
`ap-northeast-1` + (`claude,nova-lite,nova-micro,nova-pro`) → `jp.`; other ap + same list → `apac.`.

### 2.4 `resolveSDK` — client options (`provider.ts:1788-1891`) [src]

1. `options = {...provider.options}`.
2. `google-vertex` + npm `@ai-sdk/google-vertex/anthropic` + no baseURL → `googleVertexAnthropicBaseURL(project, location)` = `https://aiplatform.${location}.rep.googleapis.com/v1/projects/${project}/locations/${location}/publishers/anthropic/models` (only `eu`/`us`) (`:129-134`).
3. `google-vertex` and npm not openai-compatible → `delete options.fetch`.
4. npm contains `@ai-sdk/openai-compatible` → `includeUsage = true` unless explicitly `false` (`:1809-1811`).
5. baseURL = non-empty `options.baseURL` else `model.api.url`; substitute `${KEY}` from `vars` loader, then `${ENV}` from env (unknown left literal) (`:1813-1834`). `googleVertexEndpoint(loc)`: `global`→`aiplatform.googleapis.com`; `eu|us`→`aiplatform.${loc}.rep.googleapis.com`; else `${loc}-aiplatform.googleapis.com` (`:136-140`).
6. `apiKey ??= provider.key`; `headers = {...options.headers, ...model.headers}` (`:1835-1840`).
7. SDK instance cache key `Hash.fast(JSON.stringify({providerID, npm, options}))` (`:1842-1850`).
8. `options.fetch = timeoutFetch(options)`; delete `chunkTimeout`,`headerTimeout`; `factory({ name: providerID, ...options })` (`:1852-1865`). `name` drives AI SDK providerOptions key for openai-compatible (§5.4).

`timeoutFetch` (`provider.ts:96-127`): `chunkTimeout` default 300 000 ms — idle timeout between body reads, **only if response
`content-type` includes `text/event-stream`**; timeout → `ResponseStreamError("SSE read timed out")` (`:37-83`). `headerTimeout`
default 300 000 ms (until `fetch` resolves) → `HeaderTimeoutError` msg `Provider response headers timed out after ${ms}ms`
(`error.ts:7-13`). `timeout` (total) via `AbortSignal.timeout`. `false` disables each. Config schema: `core/src/v1/config/provider.ts:106-145`
(`apiKey, baseURL, enterpriseUrl, setCacheKey, timeout, headerTimeout, chunkTimeout` + arbitrary keys).

`getLanguage` (`provider.ts:1921-1950`): cache key `${providerID}/${model.id}`; `modelLoaders[providerID](sdk, model.api.id,
{...provider.options, ...model.options}, model)` else `sdk.languageModel(model.api.id)`; `NoSuchModelError` → `ModelNotFoundError`.

### 2.5 Model selection helpers [src]

- `parseModel("p/m/x")` → `{providerID:"p", modelID:"m/x"}` (`provider.ts:2083-2089`).
- `sort()`: priority `["gpt-5","claude-sonnet-4","big-pickle","gemini-3-pro"]` index desc, then ids containing `latest` first, then id desc (`:2072-2081`).
- `defaultModel`: `cfg.model` → most recent valid entry of `${Global.Path.state}/model.json` `.recent[]` → first provider (restricted to configured ones if any) → `sort(models)[0]` (`:2033-2066`).
- `getSmallModel`: `cfg.small_model` → plugin `experimental.provider.small_model` → none for `azure`/`azure-cognitive-services` → family priority (`opencode*`: `["gpt-nano"]`; `github-copilot*`: `["gpt-mini","gemini-flash","gpt-nano","claude-haiku"]`; else `["gemini-flash","gpt-nano","claude-haiku"]`), models sorted by `release_date` desc then id desc; Bedrock prefers `global.` → region `us./eu.` → unprefixed (`:1964-2031`).
- Errors: `ProviderModelNotFoundError` (`"Model not found: p/m. Did you mean: …?"`, fuzzysort suggestions, `:1192-1206`), `ProviderInitError`, `ProviderNoProvidersError`, `ProviderNoModelsError`.

### 2.6 Catalog provider sample (non-openai-compatible + key compat ones) [cache]

| id | npm | api base | env |
|---|---|---|---|
| anthropic | @ai-sdk/anthropic | (SDK default) | ANTHROPIC_API_KEY |
| openai | @ai-sdk/openai | (SDK default) | OPENAI_API_KEY |
| google | @ai-sdk/google | (SDK default) | GOOGLE_API_KEY, GOOGLE_GENERATIVE_AI_API_KEY, GEMINI_API_KEY |
| google-vertex | @ai-sdk/google-vertex | — | GOOGLE_VERTEX_PROJECT, GOOGLE_VERTEX_LOCATION, GOOGLE_APPLICATION_CREDENTIALS |
| google-vertex-anthropic | @ai-sdk/google-vertex/anthropic | — | same |
| amazon-bedrock | @ai-sdk/amazon-bedrock | — | AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, AWS_REGION, AWS_BEARER_TOKEN_BEDROCK |
| azure / azure-cognitive-services | @ai-sdk/azure | — | AZURE_RESOURCE_NAME, AZURE_API_KEY / AZURE_COGNITIVE_SERVICES_* |
| github-copilot | @ai-sdk/openai-compatible (per-model: 17× @ai-sdk/openai, 11× @ai-sdk/anthropic @ `https://api.githubcopilot.com/v1`) | https://api.githubcopilot.com | GITHUB_TOKEN |
| opencode (Zen) | @ai-sdk/openai-compatible; per-model @ai-sdk/openai, @ai-sdk/anthropic (`…/inference/anthropic/v1`), @ai-sdk/google (`…/inference/google/v1beta`) | https://opencode.ai/inference/openai/v1 | OPENCODE_API_KEY |
| opencode-go | same pattern under `/inference/go/...` | https://opencode.ai/inference/go/openai/v1 | OPENCODE_API_KEY |
| openrouter | @openrouter/ai-sdk-provider | https://openrouter.ai/api/v1 | OPENROUTER_API_KEY |
| xai | @ai-sdk/xai | — | XAI_API_KEY |
| mistral / groq / cerebras / deepinfra / cohere / togetherai / perplexity | own SDKs | — | `<NAME>_API_KEY` (TOGETHER_API_KEY for togetherai) |
| vercel | @ai-sdk/gateway | — | AI_GATEWAY_API_KEY |
| gitlab | gitlab-ai-provider | — | GITLAB_TOKEN |
| cloudflare-ai-gateway | ai-gateway-provider | — | CLOUDFLARE_API_TOKEN, CLOUDFLARE_ACCOUNT_ID, CLOUDFLARE_GATEWAY_ID |
| sap-ai-core | @jerome-benoit/sap-ai-provider-v2 | — | AICORE_SERVICE_KEY |
| deepseek | openai-compatible | https://api.deepseek.com | DEEPSEEK_API_KEY |
| moonshotai | openai-compatible | https://api.moonshot.ai/v1 | MOONSHOT_API_KEY |
| zai | openai-compatible | https://api.z.ai/api/paas/v4 | ZHIPU_API_KEY |
| minimax (+cn, coding-plan) | @ai-sdk/anthropic | https://api.minimax.io/anthropic/v1 | MINIMAX_API_KEY |

Note env-key rule (§2.2 step 6): providers with >1 env var (google, bedrock, azure…) get `source:"env"` but **no `key`** —
the SDK reads its own env var. Rust must replicate each SDK's own env lookup (e.g. `@ai-sdk/google` reads
`GOOGLE_GENERATIVE_AI_API_KEY` **[assumption — verify in SDK]**; opencode itself does not map `GEMINI_API_KEY` → apiKey).

**Rust mapping (§2):** `struct ProviderRegistry { providers: IndexMap<ProviderId, ProviderInfo>, sdk_cache: HashMap<u64,
Arc<dyn LanguageClient>> }`; build exactly in the 12-step order above (plugin hooks become Rust trait objects); keep
`source` enum; keep `${VAR}` URL templating; `reqwest::Client` per (provider,npm,options-hash); implement
header/chunk/total timeouts with `tokio::time::timeout` around `send()` and around each `bytes_stream().next()`. AWS:
`aws-config` default chain + `aws-sigv4`; Vertex: `gcp_auth` / `google-cloud-auth` ADC with scope above.

### 2.7 Special cases (wire-relevant)

#### 2.7.1 Anthropic
- Beta header from loader (§2.3) + betas added by patched SDK (§5.10). Message-level cache markers (§5.6).
- Empty text / blank unsigned reasoning parts removed for npm `@ai-sdk/anthropic` (`transform.ts:168-195`); tool-call IDs scrubbed `[^a-zA-Z0-9_-]`→`_` for any `api.id` containing `claude` (`:224-251`).
- `toolStreaming:false` for `@ai-sdk/google-vertex/anthropic` and for non-claude models on `@ai-sdk/anthropic` (`transform.ts:1227-1232`).
- Thinking: see §5.4 variants; block binding §5.4.4.

#### 2.7.2 OpenAI (API key)
- Responses API always (`sdk.responses`). `store:false` default; reasoning/include defaults (§5.4). Tools `strict:false`
  (`session/llm/request.ts:151-157`). JSON schema sanitized (§5.7). itemId stripped when `store !== true` (`transform.ts:501-515`).
- OpenAI 404 counted retryable (`provider/error.ts:23-28`).

#### 2.7.3 Bedrock / Vertex — see §2.3 and §5.10; native SigV4 details in §7.

#### 2.7.4 GitHub Copilot — see §4.3.1

#### 2.7.5 OpenAI ChatGPT/Codex OAuth — see §4.3.2

#### 2.7.6 Wire protocols actually needed (endpoint summary)
| Protocol | Endpoint (AI SDK default base unless configured) | Stream framing | Detail |
|---|---|---|---|
| OpenAI Chat (openai-compatible, openrouter, copilot chat, CF REST, snowflake) | `POST {baseURL}/chat/completions` | SSE `data:` JSON, `[DONE]` | §7.4, §4.3.1 |
| OpenAI Responses (openai, azure, xai, mantle, copilot responses, codex) | `POST {baseURL}/responses` | SSE (event `type` in JSON); Codex optionally WebSocket | §7.5, §4.3.2 |
| Anthropic Messages (anthropic, minimax, Zen claude, copilot claude, vertex-anthropic) | `POST {baseURL}/messages` (`anthropic-version: 2023-06-01`) | SSE | §7.3, §5.10 |
| Gemini (google, Zen gemini) | `POST {base}/models/{id}:streamGenerateContent?alt=sse` | SSE | §7.6 |
| Vertex Gemini / Vertex Anthropic | `https://{googleVertexEndpoint(loc)}/…` (path built inside `@ai-sdk/google-vertex` — UNKNOWN) / §2.4 step 2 | SSE | §10 |
| Bedrock Converse | `POST https://bedrock-runtime.{region}.amazonaws.com/model/{id}/converse-stream` | AWS event-stream binary | §7.6 |
| GitLab Duo | inside `gitlab-ai-provider@6.23.1` — UNKNOWN | — | §10 |

### 2.8 v2 core (`core/src/...`) differences

- **Not on the request path** (high confidence): sessions call v1 `Provider.getLanguage` (`opencode/src/session/llm.ts:96`,
  `opencode/src/agent/agent.ts:375`); v2 `AISDK.Service`/`Catalog.Service` used only by `opencode debug v2`
  (`opencode/src/cli/cmd/debug/v2.ts:3,15`).
- Side effect: v2 provider plugins still boot (`core/src/plugin/internal.ts:118`, list `core/src/plugin/provider.ts:36-71`); the v2
  `opencode` plugin forks a remote-config fetch into its own integration store (`core/src/plugin/provider/opencode.ts:189-195`).
- Informational diffs: azure-cognitive-services baseURL without `/v1` (`core/src/plugin/provider/azure.ts:70`); cloudflare-ai-gateway
  unified-only (`cloudflare-ai-gateway.ts:21-36`); snowflake no OAuth, `includeUsage=true` (`snowflake-cortex.ts:79`); v2 Zen
  "public" fallback disables models with any cost tier `input>0` (`opencode.ts:176-186`) vs v1 deleting `cost.input !== 0`.
- **Rust:** port only v1 semantics; skip v2 catalog/AISDK unless `debug v2` parity is required.

### 2.9 OpenCode Console account (separate from auth.json)
- `opencode account login [url]`, default `https://opencode.ai/console` (`opencode/src/cli/cmd/account.ts:18,178,188`), client id
  `"opencode-cli"` (`opencode/src/account/account.ts:131`), device flow: POST `${server}/auth/device/code` `{client_id}`; POST
  `${server}/auth/device/token` `{grant_type:"urn:ietf:params:oauth:grant-type:device_code",device_code,client_id}`; refresh same
  endpoint `{grant_type:"refresh_token",refresh_token,client_id}`; refresh 5 min before expiry (`account.ts:132-136,214-221,384-421`).
  GET `${url}/api/user`, `/api/orgs`, `/api/config` (Bearer + `x-org-id`, 404 = none) (`:279-303,363-371`).
- Stored in SQLite (`core/src/account/sql.ts:6-39`: `account`, `account_state`). Active account's remote config merged as global;
  token exported as env `OPENCODE_CONSOLE_TOKEN` (`opencode/src/config/config.ts:492-519`), referenced from remote config as
  `{env:OPENCODE_CONSOLE_TOKEN}` — this is how Zen keys can arrive without auth.json.

---

## 3. models.dev catalog [src] (`core/src/models-dev.ts`)

| Item | Value |
|---|---|
| Source URL | `${OPENCODE_MODELS_URL \|\| "https://models.opencode.ai"}/api.json` (`:160,176`) — note build-time snapshot uses `OPENCODE_MODELS_URL \|\| "https://models.dev"` (`opencode/script/generate.ts:10-13`) |
| Request | GET, header `User-Agent: opencode/${InstallationChannel}/${InstallationVersion}/${OPENCODE_CLIENT}` (`:23,177`; `OPENCODE_CLIENT` default `"cli"`, `core/src/flag/flag.ts:75-76`); timeout 10 s (`:180`); retry transient errors+responses ×2, exponential 200 ms jittered (`:150-158`); non-2xx = error |
| Cache file | `${Global.Path.cache}/models.json` if default source else `models-${Hash.fast(source)}.json` (`:161-164`). `Global.Path.cache = path.join(xdgCache, "opencode")` (`core/src/global.ts:11-12`). Observed on Windows: `C:\Users\<u>\.cache\opencode\models.json` [cache] (xdg-basedir → `~/.cache` when `XDG_CACHE_HOME` unset [assumption, consistent with observation]) |
| Override file | `OPENCODE_MODELS_PATH` (read-only, never deleted) (`:184`) |
| Load order (`populate`, `:217-231`) | disk cache (any age; corrupt JSON → delete file) → build-time snapshot `OPENCODE_MODELS_DEV` → if `OPENCODE_DISABLE_MODELS_FETCH` → `{}` → fetch under cross-process flock `models-dev:<filepath>` and write |
| Write | temp `${filepath}.${pid}.${Date.now()}.tmp` then rename (`:204-206`) |
| Freshness | TTL 5 min by file mtime (`:165-173`) |
| Refresh | background fiber at startup: `refresh()` then every 60 min (`Schedule.spaced("60 minutes")`) unless disabled or argv contains `--get-yargs-completions` (`:255-258`); refresh re-checks freshness under lock, writes, invalidates in-memory cache, publishes `ModelsDev.Event.Refreshed` (`:237-253`). In-memory cache TTL = infinity until invalidated (`:233`). Only subscriber is the v2 core plugin (`core/src/plugin/models-dev.ts:178-181`: reloads integration + catalog); the v1 `Provider` `InstanceState` keeps the catalog it read at instance init |

Schema consumed (`core/src/models-dev.ts:15-130`): Provider `{api?, name, env[], id, npm?, models{}}`; Model `{id, name, family?,
release_date, attachment, reasoning, temperature, tool_call, reasoning_options?[{type:"effort",values:(string|null)[]} |
{type:"toggle"} | {type:"budget_tokens",min?,max?}], interleaved?: bool | "reasoning"|"reasoning_content"|"reasoning_text"|string
| {field}, cost?{input,output,cache_read?,cache_write?,tiers?[{…,tier:{type:"context",size}}],context_over_200k?{…}},
limit{context,input?,output}, modalities?{input[],output[]} (text|audio|image|video|pdf), experimental?{modes?{<mode>:{cost?,
provider?{body?,headers?}}}}, status?: alpha|beta|deprecated, provider?{npm?,api?}}`. Extra keys in real data (e.g.
`structured_output`, `knowledge`, `open_weights`, `canonical_model_id`, `description`) are ignored [cache]. Prices are USD per
1 M tokens (§6.5).

Config merge: §2.2 step 5 (config schema `core/src/v1/config/provider.ts:24-147`; config model adds `options`, `headers`,
`variants{<name>:{disabled?,…}}`, `experimental: bool`; provider adds `whitelist`, `blacklist`).

**Rust mapping:** `serde_json::Value`-tolerant structs (`#[serde(default)]`, unknown fields ignored); cache in
`dirs::home_dir()/.cache/opencode` honoring `XDG_CACHE_HOME`; embed a snapshot with `include_str!` for offline first run;
reuse the same cache filename so both binaries share it. Cross-process lock is **directory-based, not OS flock**:
`${Global.Path.state}/locks/${sha1_hex(key)}.lock/` created with `mkdir` (mode 0700), containing `meta.json` + `heartbeat`;
stale after 60 s, wait timeout 5 min, backoff 100 ms→2 s with ±30 % jitter (`core/src/util/flock.ts:19-30,103-107,150-154,321`;
`Hash.fast` = SHA-1 hex, `core/src/util/hash.ts:4-5`). Rust must use the identical scheme to interoperate with a running TS
opencode. Observed state dir on Windows: `~/.local/state/opencode/locks` [cache].

---

## 4. Auth

### 4.1 Storage (v1) [src] (`opencode/src/auth/index.ts`)

- File: `${Global.Path.data}/auth.json` (`:10`), `Global.Path.data = xdgData/opencode` (`core/src/global.ts:11`). Observed on
  this Windows host: `C:\Users\<u>\.local\share\opencode\auth.json` exists (contents not read) [cache] ⇒ xdg-basedir uses
  `~/.local/share` on Windows too. Written with mode `0o600` (`:79,88`). Same dir also holds `opencode.db` (SQLite).
- Env override: `OPENCODE_AUTH_CONTENT` (whole JSON) replaces the file for reads (`:59-63`).
- Shape: `{ "<providerID>": Info }`, `Info` discriminated on `type` (`:14-35`):
  - `{"type":"oauth","refresh":str,"access":str,"expires":int>=0,"accountId"?:str,"enterpriseUrl"?:str}`
  - `{"type":"api","key":str,"metadata"?:{str:str}}`
  - `{"type":"wellknown","key":str,"token":str}`
- Invalid entries silently dropped on read (`:66`). `set` normalises key by stripping trailing `/` and deletes `key` and `key/`
  variants (`:73-81`). `OAUTH_DUMMY_KEY = "opencode-oauth-dummy-key"` (`:8`).
- Core v2 `Credential` service stores in SQLite table (`core/src/credential.ts`, `core/src/credential/sql.ts`) — not used by
  the v1 LLM path [assumption, see §2.8].

**Rust mapping:** `serde(tag="type")` enum; atomic write + `0o600` on Unix (Windows: default ACL). Preserve unknown
provider keys verbatim when rewriting.

### 4.2 API-key precedence per request [src]
`provider.options.apiKey` (config or loader) → `provider.key` (env single-var or auth.json `api`) (`provider.ts:1835`);
plugin loaders may override via returned options (step 8). Native gate uses the same (`native-runtime.ts:64`).

### 4.3 Plugin auth flows

Contract (`plugin/src/index.ts:88-217`): `AuthHook{provider, loader?(getAuth, providerInfo) → options, methods[]}`; method
`{type:"api"|"oauth", label, prompts?[text{key,message,placeholder?,validate?,when?{key,op:eq|neq,value}} | select{options}],
authorize?}`. OAuth `authorize(inputs)` → `{url, instructions, method:"auto", callback()} | {…, method:"code", callback(code)}`;
success `{provider?, refresh, access, expires, accountId?, enterpriseUrl?}` → stored `oauth`, or `{provider?, key, metadata?}` →
stored `api`. `ProviderHook{id, models?(provider,{auth})}`. Internal plugins, in registration order
(`opencode/src/plugin/index.ts:67-85`): Codex, Copilot, Modal, GitLab (npm `opencode-gitlab-auth`), Poe (npm `opencode-poe-auth`),
CloudflareWorkers, CloudflareAIGateway, Azure, DigitalOcean, SnowflakeCortex, xAI, Cerebras. Last plugin wins per provider
(`providers.ts:431` `findLast`). `getAuth` re-reads auth.json on each call (`provider.ts:1674`) — refresh-in-fetch relies on it.

Login entry points: CLI `opencode providers|auth login [url] [-p provider] [-m method]` (`opencode/src/cli/cmd/providers.ts:239-319`)
— url form fetches `${url}/.well-known/opencode` → `{auth:{command[],env}}`, runs command, stores `{type:"wellknown",key:env,token}`
(`:327-348`); plugin flow `handlePluginAuth` (`:39-210`) stores under `result.provider ?? provider`; generic fallback stores
`{type:"api",key}` (`:480-485`). Server routes `GET /provider/auth`, `POST /provider/:id/oauth/authorize|callback`
(`opencode/src/provider/auth.ts:131-220`) store under the path id (ignores `result.provider`).

#### 4.3.1 GitHub Copilot [src] (`opencode/src/plugin/github-copilot/copilot.ts` = CP, `models.ts` = CM, SDK `core/src/github-copilot/**`)

**Auth (device flow).** Method `{type:"oauth", label:"Login with GitHub Copilot"}` with select `deploymentType` ∈
{`github.com`,`enterprise`} and text `enterpriseUrl` when enterprise (`https://` prefixed if no scheme) (CP:184-220).
| Item | Value |
|---|---|
| Client ID | `Ov23li8tweQw6odWQebz` (CP:9) |
| Domain | `github.com` or `normalizeDomain(enterpriseUrl)` (strip `^https?://`, trailing `/`) (CP:15-24) |
| Device code | POST `https://${domain}/login/device/code`, JSON `{client_id, scope:"read:user"}`, headers `Accept: application/json`, `Content-Type: application/json`, `User-Agent: opencode/<v>` (CP:234-245) |
| Instructions | url = `verification_uri`, `"Enter code: ${user_code}"`, method `auto` (CP:258-261) |
| Poll | POST `https://${domain}/login/oauth/access_token` JSON `{client_id, device_code, grant_type:"urn:ietf:params:oauth:grant-type:device_code"}`; `authorization_pending` → sleep `interval*1000+3000`; `slow_down` → `(data.interval>0 ? data.interval : interval+5)*1000 + 3000` (**not persisted** for later polls); other error → failed; no overall timeout (CP:263-331) |
| Stored | `{type:"oauth", refresh: access_token, access: access_token, expires: 0, enterpriseUrl?: domain}` (CP:286-306) |
| Token exchange / refresh | **none** — GitHub OAuth token used directly as Bearer; `expires:0` never checked |
| API base | `https://api.githubcopilot.com`, enterprise `https://copilot-api.${domain}` (CP:26-28) |
| API version | `X-GitHub-Api-Version: 2026-06-01` (CP:10,363) |

**Loader** (oauth only; else `{}`): `{apiKey:"", fetch}` (CP:96-102) — empty key ⇒ SDK adds no Authorization (`core/src/github-copilot/copilot-provider.ts:62`).
Fetch (re-reads auth each call) classifies the JSON body (CP:107-158):
- Chat (`body.messages` && url contains `completions`): vision = any `image_url` part; agent = last.role ≠ user or last is synthetic-attachment message.
- Responses (`body.input`): vision = any `input_image`; agent same rule.
- Anthropic (`body.messages`, other URL): vision = `image` part or `tool_result` containing `image`; agent = `!(last.role=="user" && has non-tool_result part) || imgMsg(last)`.
- `imgMsg` = user message whose string content or text/input_text part equals `"Attached media from tool result:"` (CP:31-43).
Headers (object spread order; CP:160-173): `{"x-initiator": agent?"agent":"user", ...init.headers, "User-Agent": "opencode/<v>",
Authorization: "Bearer <refresh>", "Openai-Intent": "conversation-edits"}` + `Copilot-Vision-Request: "true"` if vision; then delete
`x-api-key` and lowercase `authorization`. `Copilot-Integration-Id` / `Editor-Version` are **not** sent (absent from source).

**Hooks:** `chat.headers` (providerID contains `github-copilot`, CP:360-413): `X-GitHub-Api-Version: 2026-06-01`, `X-Interaction-Id:
<sessionID>`, title agent → `X-Interaction-Type: agent-session-name-generation`; npm anthropic → `anthropic-beta:
interleaved-thinking-2025-05-14`; `x-initiator: agent` if message has a `compaction` part or synthetic text with
`metadata.compaction_continue===true`, or session has `parentID`. `chat.params` (CP:340-354): api.id contains `gpt` →
`maxOutputTokens = undefined`; npm anthropic → `options.toolStreaming=false`. `small_model`: first of
`["gpt-5.4-nano","gpt-4.1","gpt-4o","gpt-4o-mini"]` present (CP:11,355-359).

**Model discovery** (`provider.models`, oauth; CP:62-92, CM): GET `${base}/models` with provider headers + `Authorization: Bearer
<refresh>`, `User-Agent`, `X-GitHub-Api-Version`, 5 s timeout; envelope `{data:[]}`; undecodable items dropped (CM:221-237). Keep item if
`policy.state !== "disabled"` and `capabilities.limits.max_output_tokens`, `max_prompt_tokens`, `capabilities.supports.tool_calls`
defined (CM:207-214). Build (CM:82-205): endpoint = `"/v1/messages"`∈`supported_endpoints` → `messages` (npm `@ai-sdk/anthropic`,
url `${base}/v1`), else `/responses` → `responses`, else `/chat/completions` → `chat` (npm `@ai-sdk/github-copilot`, field
`api.endpoint`); reasoning if `adaptive_thinking`/`reasoning_effort[]`/`max_thinking_budget`/`min_thinking_budget`; image =
`supports.vision` or `image/*` media type; pdf = vision && `application/pdf`; limit `{context: max_context_window_tokens ??
max_prompt_tokens, input: max_prompt_tokens, output: max_output_tokens}`; cost per 1M = `price × (batch_size>0 ? 10_000/batch_size :
0)`, cache.write 0; release_date = `version` minus `${id}-`; variants: efforts (non-messages) `{reasoningEffort, reasoningSummary:"auto",
include:["reasoning.encrypted_content"]}`; efforts+adaptive `{thinking:{type:"adaptive",display:"summarized"}, effort}`; budget →
`max:{thinking:{type:"enabled",budgetTokens:max-1}}`, `high:{…floor(max/2)}`; else fallback §5.4.5. Only `model_picker_enabled`
models returned; on error fallback to catalog with `api.url=base`, `api.npm="@ai-sdk/github-copilot"` (CP:45-54,86-91).
Non-oauth also rewrites npm to `@ai-sdk/github-copilot`. Endpoint selection §2.3.

**Copilot SDK wire (chat)** (`core/src/github-copilot/chat/*`): body `model, user, max_tokens, temperature, top_p,
frequency_penalty, presence_penalty, response_format({type:"json_object"} only), stop, seed, …raw passthrough of
providerOptions["github-copilot"], reasoning_effort, verbosity, messages, tools, tool_choice, thinking_budget, stream:true`; **no
`stream_options`** (`includeUsage` not wired, `openai-compatible-chat-language-model.ts:139-187,308-314`). Options parsed from
`copilot` then `github-copilot` namespaces (`:79-116`) ⇒ variant `reasoningSummary`/`include` are dropped on chat [inference].
Messages (`convert-to-openai-compatible-chat-messages.ts`): system `{role,content,...copilotMeta}`; user single text → string, else
`[text | image_url{url}]` (`image/*` data mapped to `image/jpeg` data URL; other media throws); assistant `{content|null,
tool_calls[{id,type:"function",function:{name,arguments}}], reasoning_text, reasoning_opaque}`; tool `{role:"tool",tool_call_id,
content:string}`; `copilot_cache_control:{type:"ephemeral"}` spread from metadata (§5.6). Stream deltas: `content`, `reasoning_text`
(id `reasoning-0`), `reasoning_opaque` (latest kept, attached as `copilot.reasoningOpaque`), `tool_calls[{index,id?,function}]`
(new index needs id+name; tool-call emitted as soon as accumulated args parse as JSON; later deltas ignored), `finish_reason`,
`usage`; finish map stop/length/content_filter/function_call|tool_calls→tool-calls/else other (`:387-708`,
`map-openai-compatible-finish-reason.ts:6-18`).

**Copilot SDK wire (responses)** (`responses/*`): body `model, input, temperature, top_p, max_output_tokens, text{format?,verbosity?},
max_tool_calls, metadata, parallel_tool_calls, previous_response_id, store, user, instructions, service_tier, include,
prompt_cache_key, safety_identifier, top_logprobs, reasoning{effort?,summary?}, tools, tool_choice, stream:true`
(`openai-responses-language-model.ts:253-309,382-389,786-789`); system role `developer` for `o*`/`gpt-5*`/`codex-*`/`computer-use*`
(non-chat), removed for `o1-mini`/`o1-preview`, temperature/top_p dropped for reasoning models (`:1673-1726`); function tools
`strict:false`. Input items as §7.5 but metadata namespace `copilot.{itemId, reasoningEncryptedContent}`; reasoning without
`itemId` skipped (`convert-to-openai-responses-input.ts:185-245`) ⇒ with `store:false` host strips itemId (§5.5) so encrypted
reasoning is likely **not replayed** [inference, medium]. Stream: tracks reasoning by `output_index` (Copilot rotates item ids),
text under first item id (`:836-853,950-980`); events `response.output_text.delta, .completed, .incomplete, .created,
.output_item.added/.done, .function_call_arguments.delta, .reasoning_summary_part.added, .reasoning_summary_text.delta,
.output_text.annotation.added, …, error` (`:1555-1570`).

**Billing:** `copilot_usage.total_nano_aiu` read from raw chunks (top-level or `response.copilot_usage`) by host (§6.1, §6.5);
which event carries it: UNKNOWN.

#### 4.3.2 OpenAI ChatGPT / Codex OAuth [src] (`opencode/src/plugin/openai/codex.ts` = CX, `ws.ts` = WS, `ws-pool.ts` = WP)

| Item | Value |
|---|---|
| Client ID | `app_EMoamEEZ73f0CkXaXp7hrann` (CX:10) |
| Issuer | `https://auth.openai.com` (CX:11) |
| API endpoint | `https://chatgpt.com/backend-api/codex/responses` (CX:12) |
| PKCE | verifier 43 chars (`chars[b % 66]`, `A-Za-z0-9-._~`), S256 base64url no pad; state = base64url(32 random bytes) (CX:23-36,446) |
| Authorize | `https://auth.openai.com/oauth/authorize?response_type=code&client_id&redirect_uri&scope=openid profile email offline_access&code_challenge&code_challenge_method=S256&id_token_add_organizations=true&codex_cli_simplified_flow=true&state&originator=opencode` (CX:88-102) |
| Redirect | `http://localhost:1455/auth/callback`; server `listen(1455)` all interfaces; `/cancel`; 5 min timeout; state mismatch 400 `"Invalid state - potential CSRF attack"` (CX:164-271) |
| Token | POST `https://auth.openai.com/oauth/token`, form-urlencoded: exchange `grant_type=authorization_code,code,redirect_uri,client_id,code_verifier`; refresh `grant_type=refresh_token,refresh_token,client_id` (CX:117-149) |
| Headless | POST `https://auth.openai.com/api/accounts/deviceauth/usercode` JSON `{client_id}` → `{device_auth_id,user_code,interval}`; show `https://auth.openai.com/codex/device`; poll POST `…/api/accounts/deviceauth/token` JSON `{device_auth_id,user_code}` (403/404 → sleep interval+3 s) → `{authorization_code, code_verifier}` exchanged with redirect `https://auth.openai.com/deviceauth/callback` (CX:470-552) |
| Methods | oauth "ChatGPT Pro/Plus (browser)", oauth "ChatGPT Pro/Plus (headless)", api "Manually enter API Key" (CX:440-556) |
| Stored | `{type:"oauth", refresh, access, expires: now+(expires_in??3600)*1000, accountId?}`; accountId from id_token then access_token JWT claims: `chatgpt_account_id`\|\|`["https://api.openai.com/auth"].chatgpt_account_id`\|\|`organizations[0].id` (CX:49-78) |
| Residency | `["https://api.openai.com/auth"].chatgpt_compute_residency ?? chatgpt_compute_residency`, `"no_constraint"` ⇒ unset (CX:80-86) |

Loader/fetch (CX:330-437): `apiKey: OAUTH_DUMMY_KEY`; delete `authorization`/`Authorization`; refresh when `!access || expires <
now` (no skew), single-flight, persisted via `client.auth.set` (id `openai`); headers `authorization: Bearer <access>`,
`ChatGPT-Account-Id: <accountId>`; URL whose path contains `/v1/responses` or `/chat/completions` → replaced by
`CODEX_API_ENDPOINT` (query dropped) + `x-openai-internal-codex-residency` if residency. **No body rewrite** — host shaping:
`instructions` = system text, no system messages in `input` (§5.1), `store:false`, `promptCacheKey`, gpt-5 reasoning defaults,
`textVerbosity:"low"`, tools `strict:false`, itemId stripping (§5.4–5.5). `chat.headers` for providerID `openai` (any auth type):
`originator: opencode`, `User-Agent: opencode/<v> (<platform> <release>; <arch>)`, `session-id: <sessionID>`, title agent + WS →
`x-opencode-title: true` (CX:559-568). `chat.params`: `maxOutputTokens = undefined` (CX:569-573, any auth).
`provider.models` (oauth): drop `options.reasoningMode==="pro"`, keep `{gpt-5.5, gpt-5.3-codex-spark, gpt-5.4, gpt-5.4-mini,
gpt-6-sol, gpt-6-luna}`, drop `gpt-5.5-pro`, `gpt-5.6`, else keep `^gpt-(\d+)(?:\.(\d+))?` with major>5 or 5.>4; zero cost; `gpt-5.5`/
`gpt-5.6` limit `{context:400000,input:272000,output:128000}` (CX:15-16,288-327). Error `usage_not_included` → upgrade message (§6.3).

WebSocket transport: enabled if `OPENCODE_EXPERIMENTAL_WEBSOCKETS` or install channel ∈ {local, dev, beta}
(`opencode/src/plugin/index.ts:62-64`; channel from build `OPENCODE_CHANNEL`, default `local`, `core/src/installation/version.ts:7`).
Used for oauth `/responses` and also for stored `api` auth (CX:339,434). Routed to HTTP when not POST `/responses`, `stream` falsy,
title header, no session key (`x-session-affinity ?? session-id`), or pool entry busy/fallback (WP:43-81). Handshake URL
`url.replace(/^http/,"ws")`, request headers lowercased minus `x-opencode-title`,`content-length`, plus `openai-beta:
responses_websockets=2026-02-06` (WS:11,72-137). Send one text frame `{"type":"response.create", ...body}` minus `stream`,
`background`; each server frame re-emitted as SSE `data:` lines; `response.completed|response.done` → `[DONE]`;
`response.failed|incomplete|error` terminal; non-2xx `status` in `error` → `APICallError` (WS:222-317). Pool: connect 15 s, idle 5 min,
max age 55 min, 5 stream retries then sticky HTTP fallback; close 1009 → fallback; `websocket_connection_limit_reached` counted
(WP:26-201).

#### 4.3.3 Other plugins [src]

| Plugin / provider | Methods | Endpoints / constants | Stored | Loader / fetch rewrite |
|---|---|---|---|---|
| **xAI** `xai` (`opencode/src/plugin/xai.ts`) | oauth "SuperGrok Subscription" (device flow, `auto`); api "Manually enter API Key" (`:307-334`) | client `b1a00492-073a-47ea-816f-4c329264a828` (`:6`); token `https://auth.x.ai/oauth2/token` (`:7`); device `https://auth.x.ai/oauth2/device/code` (`:15`); grant `urn:ietf:params:oauth:grant-type:device_code`; scope `openid profile email offline_access grok-cli:access api:access` (`:17`); form-urlencoded, `Accept: application/json`, `User-Agent: opencode/<v>` (`:46-52`); device body `client_id,scope,referrer=opencode` (`:112-116`); poll interval default 5 s (min 1 s), `slow_down` +5 s, pending sleeps interval+3 s, terminal `access_denied\|authorization_denied\|expired_token` (`:22-26,148-197`) | `oauth{refresh,access,expires=now+(expires_in??3600)s}` (`:319-324`) | oauth only: `{apiKey: OAUTH_DUMMY_KEY, fetch}`; refresh (form `grant_type=refresh_token,refresh_token,client_id`, keep old refresh if not rotated, single-flight) when no expiry, within 120 s, or JWT `exp` within skew (`:30,81-85,237-269`); sets `authorization: Bearer`, `User-Agent` (`:290-291`); no 401 retry |
| **Azure** `azure` (`azure.ts`) | api "API key"; oauth "Microsoft Entra ID (Azure CLI)" (hidden if no `az` on PATH) (`:74-107`); prompt `resourceName` when `AZURE_RESOURCE_NAME` unset (`:45-53`) | scopes `https://cognitiveservices.azure.com/.default`, `https://ai.azure.com/.default` (`:8-9`); token via `az account get-access-token --scope <s> --output json` (`accessToken`, `expires_on` epoch s or `expiresOn` date), cached per scope, 60 s buffer (`:10-43,111-114`) | api `{key,metadata:{resourceName}}`; oauth `{access:OAUTH_DUMMY_KEY, refresh:OAUTH_DUMMY_KEY, expires:now+365d, accountId:resourceName}` (`:93-99`) | oauth: fetch deletes `api-key`,`x-api-key`, sets `authorization: Bearer <az token>`; Foundry scope if host ends `.services.ai.azure.com` and path not `/models` (`:63-69,116-122`) |
| **Cloudflare** (`cloudflare.ts`) | workers-ai api "API key" (+`accountId` prompt); ai-gateway api "Gateway API token" (+`accountId`,`gatewayId`) | — | `api{key, metadata:{accountId?,gatewayId?}}` | none (v1 custom loaders, §2.3) |
| **DigitalOcean** `digitalocean` (`digitalocean.ts`) | oauth "Login with DigitalOcean" (`auto`); api "Paste Model Access Key" | client `b1a6c5158156caac821fd1b30253ca8acb52454a48fa744420e41889cb589f82`; authorize `https://cloud.digitalocean.com/v1/oauth/authorize` **implicit grant** (`response_type=token`), scope `genai:read inference:query`, redirect `http://localhost:1456/auth/callback`, token relay POST `/auth/token` from bootstrap page (`core/src/oauth/page.ts:61-134`), 5 min timeout (`:8-17,47-152`) | **as `api`**: `{key:access_token, metadata:{oauth_access, oauth_expires, oauth_scopes, routers(JSON), routers_fetched_at}}` (`:295-308`), no refresh | `models` hook: GET `https://api.digitalocean.com/v2/gen-ai/models/routers` (`model_routers`), adds `router:<name>` models at `https://inference.do-ai.run/v1`, npm openai-compatible (`:170-269`) |
| **Snowflake Cortex** `snowflake-cortex` (`snowflake-cortex.ts`) | oauth "Login with Snowflake (External Browser)"; api "Paste PAT or bearer token manually"; prompts `account` (required), `role` | client `LOCAL_APPLICATION`; loopback `127.0.0.1` random port, path `/`; PKCE S256 (verifier 64 chars); scope `refresh_token` [+ `session:role:<role>` or `session:role-encoded:<urlenc>`]; authorize `https://${account}.snowflakecomputing.com/oauth/authorize`; token POST `…/oauth/token-request` with `Authorization: Basic base64("LOCAL_APPLICATION:LOCAL_APPLICATION")` (`:8-147`) | oauth `{refresh,access,expires=now+(expires_in??600)s,accountId:account}`; api `{key,metadata:{account}}` | oauth: `{apiKey:OAUTH_DUMMY_KEY, fetch}`; refresh within 120 s, retry once on 401; body/response rewrites as in §2.3 (`:320-452`) |
| **Cerebras** (`cerebras.ts`) | none | `chat.params`: npm `@ai-sdk/cerebras` + `options.max_completion_tokens` set → `maxOutputTokens = undefined` (`:5-8`) | — | — |
| **Modal** `modal` (`modal/modal.ts`, `models.ts`) | none (generic api key) | key = api key or env `MODAL_PROXY_TOKEN`; GET `${baseURL}/models` Bearer, 3 s timeout; maps `data[]` incl. `pricing` ×1e6, `reasoning_options` → `reasoningEffort` variants (`models.ts:9-127`) | — | — |
| **GitLab** `gitlab` | npm `opencode-gitlab-auth@2.1.0` — **UNKNOWN** internals | v1 loader §2.3 | UNKNOWN | — |
| **Poe** | npm `opencode-poe-auth@0.0.1` — **UNKNOWN** | — | UNKNOWN | — |

**Rust mapping (§4):** `trait AuthPlugin { fn provider(&self) -> &str; fn methods(&self) -> Vec<AuthMethod>; async fn
loader(&self, get_auth: AuthGetter, info: &ProviderInfo) -> ProviderOptions; }` where `ProviderOptions` may carry a
`RequestInterceptor` (Rust replacement for injected `fetch`: mutate headers/body before send, map response after). OAuth:
`oauth2` crate (device + PKCE), loopback server `axum`/`tiny_http`; single-flight refresh with `tokio::sync::Mutex`; re-read
auth.json on every request to stay coherent with a concurrently running TS instance. Spawn `az` via `tokio::process`.

---

## 5. Request shaping (AI SDK path)

### 5.1 System prompt [src]
- Base prompt (`session/system.ts:28-51`), first match on `model.api.id`: contains `muse` → `meta.txt` with `{{MODEL_NAME}}` =
  "Muse Glimmer"/"Muse Spark"; `gpt-4`|`o1`|`o3` → `beast.txt`; `gpt` → `gpt-6`→`gpt-astra.txt`, `codex`→`codex.txt`, else
  `gpt.txt`; `gemini-` → `gemini.txt`; `claude` → `anthropic.txt`; lowercase `trinity` → `trinity.txt`; lowercase `kimi` or
  providerID ∈ {kimi-for-coding, moonshotai, moonshotai-cn} → `kimi.txt`; else `default.txt` (files in `opencode/src/session/prompt/`).
- Assembly (`session/llm/request.ts:57-77`): `system[0] = [agent.prompt || providerPrompt, ...input.system, user.system]
  .filter(Boolean).join("\n")`; plugin `experimental.chat.system.transform` may push more; if >2 entries and `system[0]`
  unchanged → collapse to `[header, rest.join("\n")]`. ⇒ at most 2 system messages normally.
- `input.system` (from session loop) includes the environment block (`session/system.ts:69-105`: "You are powered by the model
  named ${api.id}. The exact model ID is ${providerID}/${api.id}" + `<env>` block) — session-loop spec owns ordering.
- Messages = system strings as `{role:"system"}` messages + history, **except** OpenAI OAuth (system goes to
  `providerOptions.instructions = system.join("\n")`, `request.ts:98`) and GitLab workflow models (`request.ts:100-111`).

### 5.2 Headers sent on every LLM request [src] (`request.ts:186-205`, merge order low→high)
1. `x-opencode-session-id: <sessionID>`; `x-opencode-parent-session-id` if parent.
2. providerID starts with `opencode`: `x-opencode-project` (project id, if any), `x-opencode-session`, `x-opencode-request:
   <user message id>`, `x-opencode-client: <OPENCODE_CLIENT|"cli">`, `User-Agent: opencode/<version>`.
   Else: `x-session-affinity: <sessionID>`, `X-Session-Id: <sessionID>`, `User-Agent: opencode/<version>`.
3. `x-parent-session-id` if parent. 4. `model.headers`. 5. plugin `chat.headers` output.
Provider-level headers (`options.headers`, §2.3) are set on the SDK client and merged by the SDK (call headers win
[assumption — AI SDK `combineHeaders` order]).

### 5.3 Sampling params [src] (`request.ts:113-131`, `transform.ts:520-574,1481-1483`)
- `temperature` only if `capabilities.temperature`: `agent.temperature ?? temperature(model)`. `temperature(model)` on lowercase
  `api.id`: `north-mini-code`→1.0; `claude`→none; `gemini` → 1.0 iff matches sampling-default regexes
  (`/gemini-2[.-]5(?:[.-]|$)/i`, `/gemini-3-(?:flash|pro)(?:[.-]|$)/`, `/gemini-3[.-]1(?:[.-]|$)/`,
  `/gemini-3[.-]5-flash(?!-lite)(?:[.-]|$)/`); `glm-4.6`,`glm-4.7`,`minimax-m2` → 1.0; `kimi-k2` → 1.0 if contains
  `thinking|k2.|k2p|k2-5` else 0.6.
- `topP = agent.topP ?? topP(model)`: gemini(sampling regex)→0.95; `minimax-m2|kimi-k2.5|kimi-k2p5|kimi-k2-5`→0.95;
  `deepseek-v4-flash-0731|deepseek-v4-flash:0731`, or `deepseek-v4-flash` on provider `deepseek`/`opencode*` → 0.95.
- `topK`: `minimax-m2` → 40 if contains `m2.|m25|m21` else 20; gemini(sampling regex) → 64.
- `maxOutputTokens = min(limit.output, outputTokenMax) || outputTokenMax`, `OUTPUT_TOKEN_MAX = 32_000`, overridable by
  `OPENCODE_EXPERIMENTAL_OUTPUT_TOKEN_MAX` (positive int) (`runtime-flags.ts:52`).
- All of the above then pass through plugin hook `chat.params` (may mutate).
- AI SDK call: `maxRetries = input.retries ?? 0` (main loop 0, title 2), `toolChoice` passthrough, `abortSignal`.

### 5.4 Provider options (reasoning, caching keys, store…) [src]

**5.4.1 Merge order** (`request.ts:79-97`): `base = small ? smallOptions(model) : options({model, sessionID, providerOptions:
provider.options})` → `mergeDeep(base, model.options)` → `mergeDeep(…, agent.options)` → `mergeDeep(…, variant)` where
`variant = model.variants[user.model.variant]` (not for `small`). Azure + `useCompletionUrls` → delete `reasoningSummary`,
`include`. Result is wrapped by `providerOptions(model, opts)` (§5.4.3) before `streamText`.

**5.4.2 `options()` defaults** (`transform.ts:1220-1388`), in order:
| Condition | Set |
|---|---|
| npm `@ai-sdk/google-vertex/anthropic`, or `@ai-sdk/anthropic` && api.id lacks `claude` | `toolStreaming:false` |
| providerID `openai` or npm ∈ {openai, github-copilot, bedrock/mantle, xai} ; or npm azure | `store:false` |
| npm `@openrouter/ai-sdk-provider` / `@llmgateway/ai-sdk-provider` | `usage:{include:true}`; non-legacy gemini → `reasoning:{effort:"high"}` |
| providerID `baseten`, or `opencode` with api.id ∈ {kimi-k2-thinking, glm-4.6} | `chat_template_args:{enable_thinking:true}` |
| providerID contains `zai`/`zhipuai` + openai-compatible | `thinking:{type:"enabled",clear_thinking:false}` |
| providerID `meta` + npm openai | `reasoningSummary:"auto"`, `include:["reasoning.encrypted_content"]` |
| npm google / google-vertex + reasoning | `thinkingConfig:{includeThoughts:true}` + `thinkingLevel:"high"` unless legacy gemini (`/gemini-(?:(?:flash\|pro)-)?[12](?:[.-]\|$)/i`) |
| api.id contains `minimax-m3` + npm anthropic | `thinking:{type:"adaptive"}` |
| npm anthropic/vertex-anthropic + Kimi family + reasoning | `thinking:{type:"adaptive",display:"summarized"}`, `effort:"high"` |
| providerID `alibaba-cn` + reasoning + openai-compatible + not `kimi-k2-thinking` | `enable_thinking:true` |
| `provider.options.setCacheKey !== false`: npm deepinfra/cerebras → `prompt_cache_key=sessionID`; npm openai/azure/xai/mistral/venice or `setCacheKey===true` → `promptCacheKey=sessionID` | |
| npm `@ai-sdk/gateway` | `gateway:{caching:"auto"}` |
| npm azure + `provider.options.useCompletionUrls` | `reasoningEffort:"medium"` unless gpt ≥ 5.5 (regex `/gpt-(\d+)\.(\d+)/`) → **return early** |
| api.id contains `gpt-5`, not `gpt-5-chat`, not `gpt-5-pro` | `reasoningEffort:"medium"`; + `reasoningSummary:"auto"` for openai/azure/copilot/mantle; + `include` encrypted for openai/mantle |
| api.id contains `gpt-5.`, not `codex`, not `-chat`, npm openai/mantle | `textVerbosity:"low"` |
| gpt-5 family + providerID `opencode*` + setCacheKey≠false | `promptCacheKey`, `include` encrypted, `reasoningSummary:"auto"` |

Kimi family (`transform.ts:29-39`): providerID or api.id contains `kimi`/`moonshot`, or url host ∈ {api.kimi.com, api.moonshot.ai,
api.moonshot.cn, api.moonshotai.cn}.

`smallOptions` (`transform.ts:1390-1413`): first variant (or `{}`); providerID openai or npm openai/copilot/xai → `mergeDeep({store:false},
small)`; openrouter/llmgateway with no variant + google id → `{reasoning:{enabled:false}}`; venice → variant or
`{veniceParameters:{disableThinking:true}}`.

**5.4.3 Namespacing** `providerOptions(model, opts)` (`transform.ts:1421-1479`):
- npm openai/azure/mantle and (`capabilities.reasoning` or `reasoningEffort`/`reasoningSummary` set) → add `forceReasoning:true`;
  otherwise apply `anthropicBlockBinding` (§5.4.4).
- npm `@ai-sdk/gateway`: keep `gateway` key; all other keys under slug = `api.id` prefix before `/` (`amazon`→`bedrock`).
- key = `sdkKey(npm)` (`transform.ts:42-98`: copilot→`copilot`, azure→`azure`, openai & mantle→`openai`, bedrock→`bedrock`,
  anthropic & vertex-anthropic→`anthropic`, vertex→`vertex`, google→`google`, alibaba, cerebras, cohere, deepinfra, groq, mistral,
  perplexity, togetherai, vercel, xai, venice→`venice`, gateway→`gateway`, openrouter→`openrouter`,
  merge-gateway→`mergeGateway`, ai-gateway-provider→`openaiCompatible`) else (npm openai-compatible/openai/anthropic ?
  `providerID.split(".")[0]` : providerID). Azure → `{openai: o, azure: o}`.
- For openai-compatible the SDK reads options under the provider **name** (= providerID) — Rust must look up the same key.

**5.4.4 Anthropic "block binding"** (`transform.ts:690-739`): for `api.id` Claude ≥ 5.1 (regex
`/claude-(?:([a-z]+)-)?(\d+)(?:[.-](\d{1,2}))?(?:-([a-z]+))?(?:[.@-]|$)/i`, excluding `mythos` 5.1): anthropic npm → `thinking =
{...(thinking ?? {type:"adaptive"}), blockBinding:{prefixMismatchBehavior:"drop_block"}}` if type adaptive|enabled; bedrock →
same under `reasoningConfig`. User opt-out `thinking.blockBinding === false` (or `reasoningConfig.blockBinding`) is consumed and
removed. Patched SDK emits the corresponding beta (§5.10). Dropped blocks reported in
`providerMetadata.anthropic.inputTransformations` (logged, `processor.ts:438-451`).

**5.4.5 Variants** (`transform.ts:790-1218`, `reasoningVariants` `:1717-1923`). Variant name → options object (pre-namespacing).
`reasoningVariants(model.reasoning_options)` wins if catalog/config provides `reasoning_options` (`provider.ts:1365,1620-1625`):
`effort{values}` → per value (null→`"none"`) `reasoningEffort(model, e)`; `budget_tokens{min,max}` → `high`=`min(max(min,
floor((maxB+1)/2)), maxB)` and `max`=`maxB` where `maxB = min(max ?? 31999, limit.output-1, 31999)`; `toggle` → alibaba
`enableThinking`, cohere `thinking.type`. Fallback heuristic `variants(model)` (empty unless `capabilities.reasoning`):

| Match (first wins) | Variants |
|---|---|
| api.id `minimax-m3` + npm anthropic/compat | nvidia/lilac: `none`/`thinking` via `chat_template_kwargs.thinking_mode`; else `thinking.type` disabled/adaptive |
| GLM-5.2 (`glm-5.2\|glm-5-2\|glm-5p2`) | openrouter `reasoning.effort` high/xhigh; compat `reasoningEffort` high/max; anthropic `effort` high/max |
| Kimi family + anthropic/vertex-anthropic | low…max: `{thinking:{type:"adaptive",display:"summarized"}, effort}` |
| id contains deepseek-chat/-reasoner/-r1/-v3, minimax, glm (≠5.2), kimi, k2p, qwen, big-pickle | `{}` |
| `grok-3-mini` | low/high (`reasoning.effort` on openrouter else `reasoningEffort`) |
| npm openrouter | efforts (openai-ish ids: `openaiCompatibleReasoningEfforts`, else low/medium/high) → `reasoning:{effort}` |
| npm ai-gateway-provider | `reasoningEffort` (openai/* → `openaiReasoningEfforts`) |
| npm @ai-sdk/gateway | anthropic: adaptive efforts or budget 16000/31999; google: thinkingConfig; else `reasoningEffort` |
| npm github-copilot | gemini `{}`; claude low/medium/high `reasoningEffort`; else low/medium/high(+xhigh for `5.1-codex-max`, `5.2`, `5.3`, or gpt-5 released ≥ 2025-12-04) with `reasoningSummary:"auto"`, `include` encrypted |
| npm cerebras/togetherai/xai/deepinfra/venice/openai-compatible | `north-mini-code`: none/high; else low/medium/high (+`max` for `deepseek-v4`) as `reasoningEffort` |
| npm azure | `o1-mini` → `{}`; else `openaiReasoningEfforts` + summary/include |
| npm openai / mantle | `meta`: all OPENAI_EFFORTS; else `openaiReasoningEfforts(api.id, release_date)` + `reasoningSummary:"auto"`, `include` |
| npm anthropic / vertex-anthropic | adaptive models: efforts `{thinking:{type:"adaptive", display:"summarized" if ≥4.7}, effort}` (Copilot: drop max/xhigh; `opus-4.7` → only medium); Opus 4.5: low/medium/high `{thinking:{type:"enabled",budgetTokens:min(16000,floor(out/2-1))}, effort}`; else `high` budget `min(16000,floor(out/2-1))`, `max` budget `min(31999,out-1)` |
| npm amazon-bedrock | adaptive: `reasoningConfig{type:"adaptive",maxReasoningEffort,display?}`; anthropic: budget 16000/31999; Nova: `reasoningConfig{type:"enabled",maxReasoningEffort}` low/medium/high |
| npm google / vertex | gemini-2.5: high budget 16000, max 32768 (2.5 pro) / 24576; else `thinkingLevel` per `googleThinkingLevelEfforts` (gemma: minimal/high; legacy: low/high; flash-image: minimal/high; pro-image: high; flash: minimal/low/medium/high; else low/medium/high) |
| npm mistral | only ids containing `mistral-small-2603\|mistral-small-latest\|mistral-medium-3.5\|mistral-medium-2604` → `high:{reasoningEffort:"high"}` |
| npm groq | none/low/medium/high `reasoningEffort` |
| cohere, perplexity | `{}` |
| `@jerome-benoit/sap-ai-provider-v2` | wrapped in `{modelParams:…}` (anthropic adaptive → `output_config.effort`) |

Adaptive Claude detection (`transform.ts:657-688`): `claude-` id with version ≥ 4.7 (regex `/claude-(?:[a-z]+-)?(\d+)(?:[.-](\d{1,2}))?(?:[.@-]|$)/i`, no
version match ⇒ treated as modern) → efforts low/medium/high/xhigh/max, `display:"summarized"`; 4.6 opus/sonnet ids → low/medium/high/max.
OpenAI effort sets (`transform.ts:576-647`): deep-research → [medium]; gpt-5*-chat → [medium] (unversioned → []); gpt-5-pro →
[high]; gpt-5.N-pro → [medium,high,xhigh]; codex: v≥3 [none,low,medium,high,xhigh], codex-max or v≥2 [low,medium,high,xhigh], else
[low,medium,high]; gpt-5.1 → [none,low,medium,high]; gpt-5.N≥2 → +xhigh; otherwise [low,medium,high] + `minimal` (gpt-5 family)
+ `none` if release ≥ 2025-11-13 + `xhigh` if release ≥ 2025-12-04.

### 5.5 Message transforms (`ProviderTransform.message`, applied inside AI-SDK middleware to the *final* SDK prompt) [src]

Applied in `transformParams` for `stream` calls (`session/llm.ts:329-339`), order (`transform.ts:465-518`):
1. `unsupportedParts` (`:409-445`, user messages only): image data-URL with empty base64 → text `"ERROR: Image file is empty or corrupted. Please provide a valid image."`; file/image whose modality (`image/*`, `audio/*`, `video/*`, `application/pdf`) is not in `capabilities.input` → text `` `ERROR: Cannot read ${"\"filename\"" | modality} (this model does not support ${modality} input). Inform the user.` ``.
2. `normalizeMessages` (`:101-356`): (a) replace lone UTF-16 surrogates with U+FFFD in all text/reasoning/tool-result text; (b) npm anthropic: drop `""` string messages, empty text parts, reasoning parts that are blank *and* lack `anthropic.signature`/`redactedData`, then drop empty messages; (c) npm bedrock: same but reasoning kept only with `signature`/`redactedContent`/`redactedData` under `providerOptions[providerID] ?? bedrock`; (d) `api.id` contains `claude`: toolCallId `replace(/[^a-zA-Z0-9_-]/g,"_")`; (e) providerID `mistral` or id contains mistral/devstral/codestral/pixtral/mixtral: toolCallId → alnum only, first 9 chars, `padEnd(9,"0")`; insert `{role:"assistant",content:[{type:"text",text:"Done."}]}` between a `tool` message and a following `user` message; **returns here** (skips f,g); (f) id contains `deepseek`: every assistant message gets a trailing `{type:"reasoning",text:""}` if none; (g) `capabilities.interleaved.field` set and npm ≠ openrouter: assistant reasoning parts removed, concatenated text put in `providerOptions.openaiCompatible[field]` (always set, even `""`).
3. Prompt caching (§5.6) when anthropic-ish.
4. Re-key `providerOptions[providerID]` → `providerOptions[sdkKey(npm)]` on messages and parts.
5. If `options.store !== true` and npm ∈ {openai, azure, mantle, github-copilot}: delete `providerOptions[key].itemId` everywhere.

Tool-call repair (`llm.ts:295-311`): unknown tool whose lowercase name exists → rename; otherwise rename to `invalid` with
input `JSON.stringify({tool, error})`. `activeTools` excludes `invalid` (`:316`).

### 5.6 Prompt caching markers [src] (`transform.ts:358-407,465-484`)
- Applied iff (providerID ∈ {anthropic, google-vertex-anthropic} or api.id/id contains `anthropic`/`claude` or npm ∈
  {anthropic, alibaba}) and npm ≠ `@ai-sdk/gateway` and **not** (`options.cacheControl` set and npm anthropic/vertex-anthropic).
- Targets: first 2 `system` messages + last 2 non-system messages (deduped).
- Placement: message-level `providerOptions` when providerID == `anthropic` or contains `bedrock` or npm bedrock; else on the
  **last content part** (skip tool-approval parts) if content is a non-empty array, otherwise message-level.
- Payload (all namespaces at once): `anthropic.cacheControl={type:"ephemeral"}`, `openrouter.cacheControl`, `bedrock.cachePoint={type:"default"}`,
  `openaiCompatible.cache_control={type:"ephemeral"}`, `copilot.copilot_cache_control={type:"ephemeral"}`, `alibaba.cacheControl`.
- Wire effect: Anthropic `cache_control:{type:"ephemeral"}` on the block; Bedrock `cachePoint` block; openai-compatible copies
  `cache_control` onto the message/part JSON [assumption: AI SDK passes namespace fields through] — see §7 for native encoding.
- OpenAI-family caching = `promptCacheKey`/`prompt_cache_key` (§5.4.2), no markers.

### 5.7 Tool schema transforms [src] (`transform.ts:1491-1715`, applied per tool in `session/tools.ts:98,143-309,396`)
1. npm openai/azure → `sanitizeOpenAISchema` (Codex parity): boolean schema → `{type:"string"}`; keep only `$ref, description,
   enum (const→enum[const]), properties, required (strings), items, additionalProperties, anyOf/oneOf/allOf, $defs/definitions`;
   `type` filtered to JSON types; no type + ($ref|composition) → return as-is; infer type (`properties/required/additionalProperties`
   → object; `items/prefixItems` → array; enum/format → string; numeric keywords → number; else `{}`); object w/o properties →
   `properties:{}`; array w/o items → `items:{type:"string"}`.
2. providerID `moonshotai` or api.id contains `kimi`: `$ref` nodes → `{$ref}` only; tuple `items[]` → `items[0] ?? {}`.
3. providerID `google` or api.id contains `gemini`: enum values → strings (integer/number type → string); `type:[…]` → `anyOf`
   of non-null types + `nullable:true` (all-null → `"null"`); `required` filtered to existing properties; arrays get `items`
   (schema-empty items → `type:"string"`); non-object types lose `properties/required`.
- Tools: `strict:false` for openai/azure/mantle (`request.ts:151-157`); Copilot with no tools but tool history → add `_noop`
  tool (`description "Do not call this tool. It exists only for API compatibility and must never be invoked."`, schema
  `{type:"object",properties:{reason:{type:"string",description:"Unused"}}}`) (`request.ts:158-174`); tools sorted by name
  (`localeCompare`) (`request.ts:183`); tools disabled by permission rules or `user.tools[k]===false` removed (`:209-215`).

### 5.8 Image / file parts [src] (`session/message-v2.ts:131-428`)
- User file parts (except `text/plain`, `application/x-directory`) → `{type:"file", url, mediaType, filename}`; `stripMedia` →
  text `[Attached ${mime}: ${filename ?? "file"}]`.
- Tool results: `{text, attachments[{mime,url(data:)}]}` → `{type:"content", value:[{type:"text"}, {type:"media", mediaType,
  data:<base64 after comma>}]}`. Media allowed inside tool results only for npm anthropic, openai, mantle, vertex-anthropic,
  bedrock (images, and ids containing `anthropic.`/`nova`/`llama4`/`llama-4`), xai (images), google (`gemini-3`, not `gemini-2`);
  otherwise media is moved into a synthetic following user message: text `"Attached media from tool result:"` + file parts
  (`message-v2.ts:46,393-411`). xAI drops images not png/jpeg/webp (`:166-169`).
- Assistant replay: when the target model differs from the message's model, provider metadata is stripped and reasoning becomes
  plain text (`:255,376-383`); empty text next to signed Anthropic reasoning → `" "` (`:283-290`); pending/running tool calls →
  `output-error` `"[Tool execution was interrupted]"` (`:364-373`); compacted outputs → `"[Old tool result content cleared]"`.

### 5.9 OpenAI OAuth / workflow special paths — see §4.3.2 and §2.3 (gitlab).

### 5.10 Patched AI SDK behaviour (`patches/@ai-sdk%2F*.patch`, applied via root `package.json` `patchedDependencies`)

Line numbers = patch-file lines (dist/index.js hunk; .mjs/internal/src repeat it).
| Package | Wire change |
|---|---|
| `@ai-sdk/anthropic@3.0.111` | option `thinking.blockBinding{prefixMismatchBehavior:"error"\\|"drop_block"}` (adaptive & enabled) → body `thinking.block_binding.prefix_mismatch_behavior` (`:89-124`) + beta `thinking-binding-controls-2026-08-01` (`:132-134`); context shows existing beta `server-side-fallback-2026-06-01` when `fallbacks` set (`:129-130`); parses `input_transformations[]{type,path?,reason?}` from response / SSE `message_start` → metadata `inputTransformations` (`:61-81,142-168`) |
| `@ai-sdk/openai@3.0.88` | `serviceTier` adds `"ultrafast"`; client no longer strips `service_tier` for flex/priority (chat `:53-77`, responses `:85-109`) |
| `@ai-sdk/openai-compatible@2.0.41` | SSE error chunk → stream `error` part carries whole `error` object (`:9-10`) — affects `parseStreamError` (§6.3) |
| `@ai-sdk/google@3.0.73` | `model` turns whose parts were all filtered out are removed from `contents` (`:9-11,65-69`) |
| `@ai-sdk/groq@3.0.31` | `reasoningEffort` free-form string (`:47-48`); body field UNKNOWN |
| `@ai-sdk/mistral@3.0.51` | `reasoningEffort` string; `promptCacheKey` → body `prompt_cache_key` (`:75-85`); assistant `content` becomes array `[{type:"text"}\\|{type:"thinking",thinking[],closed?,signature?}]` when reasoning carries `providerOptions.mistral.thinking.type=="thinking"` (`:37-70`); thinking parts → reasoning with `providerMetadata.mistral.thinking`, stream merges thinking chunks (`:93-176`) |
| `@ai-sdk/xai@3.0.139` | `reasoningEffort` string; Responses PDF → `{type:"input_file", file_id}` if data starts `file-` else `{type:"input_file", filename, file_data:"data:application/pdf;base64,…"}` (`:102-109`); `promptCacheKey` → `prompt_cache_key` (`:126-139`) |
| `@ai-sdk/amazon-bedrock@4.0.166` | `reasoningConfig.maxReasoningEffort` adds `"none"`; `reasoningConfig.blockBinding` → `additionalModelRequestFields.thinking.block_binding.prefix_mismatch_behavior` + beta `thinking-binding-controls-2026-08-01` merged into anthropic betas (destination field UNKNOWN) (`:59-105`) |

**Rust mapping (§5):** one `RequestShaper` with pure functions mirroring `transform.ts` 1:1 (`temperature/top_p/top_k`,
`options`, `variants`, `provider_options`, `message`, `schema`) operating on an internal `ModelMessage` enum identical to AI SDK
`LanguageModelV3Prompt` (system/user/assistant/tool; parts text/file/reasoning/tool-call/tool-result with per-part
`provider_options: Map<String, Value>`). Then per-protocol encoders consume that representation. Port the regexes with the
`regex` crate (lookbehind in `sanitizeSurrogates` → operate on UTF-16 via `String::from_utf16_lossy`, which already yields
U+FFFD for lone surrogates; Rust `String` cannot hold lone surrogates, so do it at JSON-decode time). Golden-test against
`packages/llm/test/fixtures/recordings/*.json`.

---

## 6. Streaming, retry, errors, accounting

### 6.1 LLMEvent stream consumed by the session loop [src] (`session/llm/ai-sdk.ts:77-289`)

| AI SDK `fullStream` part | LLMEvent | Notes |
|---|---|---|
| `start` | — | |
| `start-step` | `step-start{index}` | index counter per stream |
| `finish-step` | `step-finish{index, reason, usage, providerMetadata}` | `rawFinishReason === "network_error"` → fail `ResponseStreamError("Provider finish_reason: network_error")`; Copilot `totalNanoAiu` injected into `providerMetadata.copilot` |
| `finish` | `finish{reason, usage(totalUsage), providerMetadata}` | adapter state reset |
| `text-start/-delta/-end` | `text-start/-delta/-end{id,text?,providerMetadata}` | missing ids → `text-<n>` |
| `reasoning-start/-delta/-end` | `reasoning-*` | ids `reasoning-<n>` |
| `tool-input-start/-delta/-end` | `tool-input-*{id,name,text?}` | name remembered by id |
| `tool-call` | `tool-call{id,name,input,providerExecuted?,providerMetadata}` | |
| `tool-result` / `tool-error` | `tool-result{id,name,result}` / `tool-error{id,name,message,error}` | AI SDK executes tools itself (tools have `execute`) |
| `error` | stream failure | |
| `raw` | — | Copilot: `copilot_usage.total_nano_aiu` (top-level or under `response`) captured (`:33-43,277-281`); raw chunks only requested for Copilot (`llm.ts:294`) |
| `abort`,`source`,`file`,`tool-output-denied`,`tool-approval-request` | — | |

`FinishReason` = values accepted by `Schema.is(FinishReason)` else `"unknown"` (`ai-sdk.ts:22-24`; literal set in
`llm/src/schema/events.ts`, see §7). Processor: `content-filter` finish → `ContentFilterError` (`prompt.ts:1301-1308`); finish not in
{`tool-calls`,`unknown`} ends the turn (`prompt.ts:1295`). Usage mapping (`ai-sdk.ts:45-65`): `inputTokens, outputTokens,
totalTokens, reasoningTokens = outputTokenDetails.reasoningTokens ?? reasoningTokens, cacheReadInputTokens =
inputTokenDetails.cacheReadTokens ?? cachedInputTokens, cacheWriteInputTokens = inputTokenDetails.cacheWriteTokens`.

### 6.2 Retry policy (session level) [src] (`session/retry.ts`, used at `processor.ts:674-688`)
- SDK-level retries disabled for the main loop (`maxRetries:0`), so all retries are here. Max `RETRY_MAX_RETRIES = 5` (`attempt > 5` stops).
- Not retried: `ContextOverflowError`. `APIError` retried if `isRetryable` or `status >= 500` or message/responseBody matches
  `RETRYABLE_MESSAGE_PATTERNS` (`retry.ts:33-41`, copy regexes verbatim). Non-API errors: message contains `too_many_requests`,
  `exhausted`, `unavailable`, or matches patterns.
- Delay (`retry.ts:47-83`): headers present → `retry-after-ms` (float ms) → `retry-after` (seconds float, or HTTP-date) → else
  exponential; no headers → `min(exponential, 30_000)`. `exponential(n) = ceil(b + b*0.25*rand)`, `b = 2000 * 2^(n-1)`; cap
  2 147 483 647 ms.
- Special bodies: contains `FreeUsageLimitError` → message `"Free usage exceeded, subscribe to Go"` + action link
  `https://opencode.ai/go`; `GoUsageLimitError` → reset text from `retry-after` + link `https://opencode.ai/workspace/${workspace}/go`
  (`retry.ts:99-144`). `Overloaded` → `"Provider is overloaded"`.
- Status updates: `{type:"retry", attempt, message, action?, next}` (session-loop spec).

### 6.3 Error classification [src]
- `MessageV2.fromError` (`session/message-v2.ts:619-747`): `AbortError` → `AbortedError`; `LoadAPIKeyError` → `AuthError{providerID}`;
  `ECONNRESET` → retryable `APIError "Connection reset by server"`; `ZlibError` → retryable `"Response decompression failed"` (or
  Aborted if aborted); `HeaderTimeoutError`/`ResponseStreamError` → retryable `APIError{metadata.code=name}`; `APICallError` →
  `parseAPICallError`; other `Error` → `UnknownError`; non-Error objects → `parseStreamError`.
- `parseAPICallError` (`provider/error.ts:172-193`): context overflow if `isContextOverflow(message)` or HTTP 413 or body
  `error.code === "context_length_exceeded"`; else `APIError{message, statusCode, isRetryable (openai*: 404 → true),
  responseHeaders, responseBody, metadata.url}`. Message building (`:32-71`): empty → body or HTTP status text; JSON body →
  `"${msg}: ${body.message|body.error|body.error.message}"`; HTML body 401/403 → fixed gateway messages; else `"${msg}: ${body}"`.
- `parseStreamError` (`:102-154`) for in-stream `{type:"error", error:{code,message}}`: `context_length_exceeded` → overflow;
  `insufficient_quota`, `usage_not_included`, `invalid_prompt` → non-retryable; `server_is_overloaded`/`server_error`/other →
  retryable.
- `isContextOverflow` (`llm/src/provider-error.ts:4-38`): 27 regexes (copy verbatim) OR `/^4(00|13)\s*(status code)?\s*\(no body\)/i`,
  unless message matches exclusions `/^(throttling error|service unavailable):/i`, `/rate limit/i`, `/too many requests/i`.

### 6.4 Token accounting [src] (`session/session.ts:338-405`)
```
input   = max(0, inputTokens - cacheRead - cacheWrite)          // AI SDK v6 inputTokens include cache
output  = max(0, outputTokens - reasoning)
cacheWrite = usage.cacheWriteInputTokens ?? meta.anthropic.cacheCreationInputTokens ?? meta.vertex.cacheCreationInputTokens
             ?? meta.bedrock.usage.cacheWriteInputTokens ?? meta.venice.usage.cacheCreationInputTokens ?? 0
```
Rust native protocols must emit `inputTokens` **including** cached tokens to keep this formula valid.

### 6.5 Cost [src]
`contextTokens = inputTokens` (raw). Price row = highest `cost.tiers[]` with `tier.type=="context" && contextTokens > tier.size`,
else `experimentalOver200K` if `contextTokens > 200_000`, else base `cost`. `cost = (input*in + output*out + cacheRead*cr +
cacheWrite*cw + reasoning*out) / 1e6` (Decimal arithmetic, non-finite → 0, clamp ≥ 0). If
`providerMetadata.copilot.totalNanoAiu` is a finite number ≥ 0 → `cost = totalNanoAiu / 100_000_000_000` instead. Summed into
`assistantMessage.cost` per `step-finish` (`processor.ts:452-469`). Use `rust_decimal`.

**Rust mapping (§6):** `enum LlmEvent` mirroring `llm/src/schema/events.ts`; `tokio_stream`; SSE via `eventsource-stream` over
`reqwest::Response::bytes_stream()`; retry loop in session crate using the exact delay function; error enum with
`is_retryable()` and `ContextOverflow` detection via `regex::RegexSet`.

---

## 7. Native protocol reference (`packages/llm`) — wire details for Rust encoders

Paths in this section are relative to `llm/src/` unless prefixed. This is opencode's **own** encoding; where it differs from
the AI SDK default path (noted ⚠), the AI SDK path is what v1.19.5 sends by default. Use §7 as the encoder skeleton and
diff against AI SDK traffic (§10) before claiming parity.

### 7.1 Native adapter (`opencode/src/session/llm/native-{runtime,request}.ts`)
- Reachable routes today: OpenAI Responses, OpenAI-compatible Chat, Anthropic Messages (gate §1.1). Azure/Google/Bedrock/OpenRouter
  branches in `native-request.ts:165-177` are unreachable.
- Facade per npm: openai → `OpenAI.configure(o).responses(api.id)`; anthropic → `Anthropic.configure(o).model(...)`;
  openai-compatible → `OpenAICompatible.configure({...o, provider: providerID, baseURL})` (baseURL required, `:148-151`).
  Route options `{apiKey, baseURL: input.baseURL ?? model.api.url, headers, limits:{context, output}}` (`:145-146,153-179`).
- All `role:"system"` messages → `LLMRequest.system` parts (`:106`). Message-level `providerOptions` stored in `Message.native`
  but **not read** by protocols (only `openai-chat.ts:260` reads `native.openaiCompatible`) ⇒ opencode cache markers are ignored
  on native path; only the auto cache policy (§7.7) applies.
- Part map: text→text; file→media (data must be string|Uint8Array, default mediaType `application/octet-stream`, `:57-61`);
  reasoning; tool-call; tool-result (`text`→text, `error-text`→error, **all else→json**, `:69`); others throw.
- Tools appended after build (`native-runtime.ts:110-112`); local tool calls dispatched concurrently via `ToolRuntime.dispatch`,
  results emitted **after** provider stream ends (`:103-137,169-193`). Tool exception → `tool-error` + `tool-result{type:"error"}`.
- Headers `{...string provider.options.headers, ...input.headers}` (`:101,155-160`). OpenAI OAuth fetch injected as
  `FetchHttpClient.Fetch` (`:144`).

### 7.2 Shared pipeline (`route/client.ts`, `route/transport/http.ts`, `protocols/shared.ts`)
- Compile: resolve options (generation last-defined-wins: route → model → request; providerOptions/http deep-merged) →
  `applyCachePolicy` → `body.from` → schema validate → `transport.prepare` (`client.ts:167-180,344-359`).
- URL = `trimTrailingSlash(baseURL) + path` + endpoint query + `request.http.query` (`route/endpoint.ts:47`, `http.ts:24-29,90-93`).
- Headers: route headers → `request.http.headers` → auth set/remove → `content-type: application/json` (forced last)
  (`http.ts:100-103`, `shared.ts:322`). Always `POST`.
- `request.http.body` deep-merged but 37 protocol-owned keys rejected (`http.ts:31-68`).
- **SSE**: UTF-8 → SSE decode; `Retry` controls dropped; empty `data` or `[DONE]` dropped; **`event:` names ignored**, dispatch on
  JSON `type`; per-frame JSON schema decode failure is fatal `Invalid <provider>/<route> stream event` (`shared.ts:242-249`,
  `route/protocol.ts:82`, `client.ts:232-241`).
- Media (`shared.ts:158-206`): images png/jpeg/gif/webp; media adds video mp4/webm/quicktime, audio wav/mp3/aiff/aac/ogg/flac;
  ≤ 28 MiB encoded / 20 MiB decoded; data-URL mime must equal `mediaType`; canonical base64 required; http(s) URLs rejected.
- Tool input `""` → `{}`; invalid JSON fatal (`shared.ts:155-156`). Tool-result text: text→`String(v)`; error→JSON if object/array;
  json→JSON (`:213-223`).
- Mid-conversation system → user text `<system-update>\n…\n</system-update>` with `& < >` escaped (`shared.ts:111-121`); exception:
  Anthropic `claude-opus-4-8` native system role (`anthropic-messages.ts:356,363-373`).
- Lifecycle (`protocols/utils/lifecycle.ts:74-100`): lazy `step-start{index:0}`; `text-start`/`reasoning-start` once per id;
  finish closes open reasoning then text, emits `step-finish{index:0}` then `finish`.
- `generate` without terminal finish/provider-error fails `Provider stream ended without a terminal finish event` (`client.ts:387-390`).

### 7.3 Anthropic Messages (`protocols/anthropic-messages.ts`, `providers/anthropic.ts`)
| Item | Native value |
|---|---|
| URL | `https://api.anthropic.com/v1` + `/messages` (`:29-30`) |
| Headers | `anthropic-version: 2023-06-01` (`:852`); `x-api-key` (`providers/anthropic.ts:13-18`, env `ANTHROPIC_API_KEY`); ⚠ **no `anthropic-beta`** natively (AI SDK path sends loader betas §2.3 + patch betas §5.10) |
| Body | `model, system[], messages, tools, tool_choice, stream:true, max_tokens (= maxTokens ?? limits.output ?? 4096), temperature, top_p, top_k, stop_sequences, thinking` (`:156-169,510,539-552`) |
| system | text blocks with optional `cache_control` |
| user parts | `text`, `image{source:{type:"base64",media_type,data}}` (no document block natively ⚠) |
| assistant | reasoning→`thinking{thinking,signature}` (signature = `encrypted ?? providerMetadata.anthropic.signature`, `:449-455`); tool-call→`tool_use` / `server_tool_use` (providerExecuted); hosted results → `web_search_tool_result`/`code_execution_tool_result`/`web_fetch_tool_result` (`:293-298`) |
| tool results | `role:"user"` with `tool_result{tool_use_id, content: string \\| [text\\|image], is_error?}` (`:473-485`) |
| tool_choice | auto→`{type:"auto"}`, required→`{type:"any"}`, tool→`{type:"tool",name}`, none→omit **both** `tool_choice` and `tools` (`:268-274,516`) |
| thinking | only `providerOptions.anthropic.thinking.type==="enabled"` → `{type:"enabled",budget_tokens}` (`:493-504`); ⚠ adaptive/effort/display/blockBinding (AI SDK path) not supported natively |
| cache | `cache_control:{type:"ephemeral"}` (+`ttl:"1h"` if ttlSeconds ≥ 3600), max 4 markers, order tools → system → messages, extras dropped with warning (`:238-251,534-537`) |

Stream (`type` field): `message_start` usage; `content_block_start` tool_use/server_tool_use → tool start (block `input` ignored) +
`tool-input-start`; text/thinking initial text → delta (ids `text-{index}`/`reasoning-{index}`); `*_tool_result` →
`tool-result{providerExecuted}` (error if `content.type` ends `_tool_result_error`); `content_block_delta`: `text_delta`,
`thinking_delta`, `signature_delta` (→ `reasoning-end{anthropic:{signature}}`), `input_json_delta` (append; unknown → fatal);
`content_block_stop` → tool finish (`tool-input-end`+`tool-call`) or text/reasoning end; **`message_delta` emits finish**
(`:782-793`); `error` → `provider-error "{type}: {message}"` + overflow classification (`:797-812`); `message_stop`/`ping` ignored.
Finish: end_turn/stop_sequence/pause_turn→stop, max_tokens→length, tool_use→tool-calls, refusal→content-filter, else unknown
(`:558-564`). Usage: `inputTokens = input_tokens + cache_read_input_tokens + cache_creation_input_tokens` (`:573-617`).

### 7.4 OpenAI Chat / OpenAI-compatible Chat (`protocols/openai-chat.ts`, `openai-compatible-chat.ts`)
- URL `https://api.openai.com/v1/chat/completions`; compatible route id `openai-compatible-chat`, same path, no default base.
- Body: `model, messages, tools, tool_choice, stream:true, stream_options:{include_usage:true} (always), store, reasoning_effort,
  max_tokens (never max_completion_tokens), temperature, top_p, frequency_penalty, presence_penalty, seed, stop` (`:91-107,361`).
- Messages: one `system` (joined `"\n"`); user all-text → **joined with `""`** else `[text | image_url{url:dataURL}]` (`:226-227`);
  assistant `content` joined `"\n"` or `null`, `tool_calls[{id,type:"function",function:{name,arguments}}]`,
  `reasoning_content` = reasoning joined `""` else `native.openaiCompatible.reasoning_content` (`:253-261`); tool →
  `{role:"tool",tool_call_id,content}`; tool-result images deferred into a synthetic user message (`:264-331`).
- `tool_choice`: "auto"/"none"/"required"/`{type:"function",function:{name}}`; tools still sent with none.
- Options read from `providerOptions.openai.{store,reasoningEffort}` (efforts none/minimal/low/medium/high/xhigh;
  `utils/openai-options.ts:5-8`). ⚠ opencode writes compat options under `providerID.split(".")[0]` (§5.4.3) → mismatch on native.
- Tools `{type:"function",function:{name,description,parameters: ToolSchemaProjection.openAI(...)}}` (`:184`).
- Stream: `choices[0]` only; `delta.reasoning_content` → `reasoning-0`; `delta.content` ends reasoning, → `text-0`;
  `tool_calls[]` keyed by `index` (id/name only on first delta; missing → fatal); first `finish_reason` → `finishAll`; **tool-call
  events and `finish` emitted at EOF (`onHalt`)**; `stop` with tools → `tool-calls` (`:407-470`).
- Finish: stop→stop, length→length, content_filter→content-filter, function_call/tool_calls→tool-calls, else unknown.
  Usage: input=`prompt_tokens`, cacheRead=`prompt_tokens_details.cached_tokens`, reasoning=`completion_tokens_details.reasoning_tokens`,
  total=`total_tokens` (`:391-405`).

### 7.5 OpenAI Responses (`protocols/openai-responses.ts`)
- URL `https://api.openai.com/v1/responses`; route default `providerOptions.openai.store=false` (`:991`).
- Body: `model, input, instructions, tools, tool_choice, store, service_tier, prompt_cache_key, include, reasoning{effort,summary},
  text{verbosity}, max_output_tokens, temperature, top_p, stream:true` (`:126-155`). Options from `providerOptions.openai.{store,
  promptCacheKey, reasoningEffort, reasoningSummary, include, textVerbosity, instructions, serviceTier}`; `include` filtered to 8
  allowed values (`utils/openai-options.ts:12-21`); `service_tier` ∈ auto/default/flex/priority.
- Input items (`:346-454`): system → `{role:"system",content}`; user → `[input_text | input_image{image_url}]`; assistant text →
  `{role:"assistant",content:[output_text]}`; tool-call → `function_call{call_id,name,arguments}` (providerExecuted skipped);
  tool result → `function_call_output{call_id, output: string | [input_text|input_image]}`; reasoning needs
  `providerMetadata.openai.itemId`: `store!==false` → `item_reference{id}` (dedup); `store===false` →
  `{type:"reasoning",summary:[summary_text],encrypted_content}` merged per itemId, dropped if no `encrypted_content` (`:449-453`).
  ⚠ AI SDK path: opencode strips `itemId` when `store !== true` (§5.5 step 5) — reconcile.
- Tools `{type:"function",name,description,parameters,strict:false}` (`:259-266`); tool_choice auto/none/required/`{type:"function",name}`.
- Stream; terminal on `response.completed|incomplete|failed` (`:613,975`):

| event | effect |
|---|---|
| `response.output_text.delta` | text delta, id=`item_id` |
| `response.reasoning_text.delta` / `.reasoning_summary.delta` / `.reasoning_summary_text.delta` | reasoning delta id `"{item_id}:{summary_index}"` |
| `response.reasoning_summary_part.added/.done` | open/close sub-ids; `store===false` defers close (`:692-787`) |
| `response.output_item.added` reasoning | `reasoning-start "{id}:0"` meta `{openai:{itemId, reasoningEncryptedContent}}` |
| `response.output_item.added` function_call | tool start key `item.id`, call id `call_id ?? id` |
| `response.function_call_arguments.delta` | append |
| `response.output_item.done` function_call | finish; **`item.arguments` replaces accumulated deltas** |
| `response.output_item.done` hosted (`web_search_call, web_search_preview_call, file_search_call, code_interpreter_call, computer_use_call, image_generation_call, mcp_call, local_shell_call`) | tool-call + tool-result providerExecuted (`:543-561`) |
| `response.completed` / `.incomplete` | finish, meta `{openai:{responseId, serviceTier}}` |
| `response.failed` / `error` | provider-error `"{code}: {message}"`, overflow if `context_length_exceeded` or pattern (`:896-911`) |

  Finish (`:523-529`): no `incomplete_details.reason` → tool-calls if any function call else stop; `max_output_tokens`→length;
  `content_filter`→content-filter. Usage: `input_tokens_details.cached_tokens`, `output_tokens_details.reasoning_tokens`.
- WebSocket variant (`:996-1020`, `route/transport/websocket.ts`): URL https→wss; one text frame = body minus `stream` plus
  `type:"response.create"`; each inbound frame = one JSON event; close 1000/1005 normal; queue 128. Not wired in app platform
  (`core/src/effect/app-node-platform.ts:16`). (Codex plugin has its own WS, §4.3.2.)

### 7.6 Gemini, Bedrock (unreachable natively today, reference only)
- **Gemini** (`protocols/gemini.ts`): `https://generativelanguage.googleapis.com/v1beta/models/{model.id}:streamGenerateContent?alt=sse`
  (id not URL-encoded), header `x-goog-api-key` (env `GOOGLE_GENERATIVE_AI_API_KEY`), body `contents[{role:user|model,parts}],
  systemInstruction{parts:[{text}]}, tools[{functionDeclarations}], toolConfig{functionCallingConfig{mode:AUTO|NONE|ANY,
  allowedFunctionNames}}, generationConfig{maxOutputTokens,temperature,topP,topK,stopSequences,thinkingConfig}` (`:108-114`);
  thinkingConfig read from `providerOptions.gemini` (⚠ opencode writes `google`); reasoning parts `{text,thought:true,
  thoughtSignature}`; tool results `role:"user"` `functionResponse{name,response:{name,content}}` (+`inlineData`), matched by
  name (`:251-284`); stream: `functionCall` → direct `tool-call` (ids `tool_{n}`, no input events); finish STOP→stop/tool-calls,
  MAX_TOKENS→length, IMAGE_SAFETY/RECITATION/SAFETY/BLOCKLIST/PROHIBITED_CONTENT/SPII→content-filter, MALFORMED_FUNCTION_CALL→error
  (`:363-377`); usage output = `candidatesTokenCount + thoughtsTokenCount`, cacheRead = `cachedContentTokenCount`.
- **Bedrock Converse** (`protocols/bedrock-converse.ts`, `bedrock-event-stream.ts`, `utils/bedrock-*.ts`): POST
  `https://bedrock-runtime.{region}.amazonaws.com/model/{encodeURIComponent(modelId)}/converse-stream` (`:665-667`, region default
  `us-east-1`, `providers/amazon-bedrock.ts:20-28`); body `modelId` (also in body), `messages`, `system[text|cachePoint]`,
  `inferenceConfig{maxTokens,temperature,topP,stopSequences}`, `toolConfig{tools[toolSpec{name,description,inputSchema:{json}} |
  cachePoint], toolChoice{auto:{}}|{any:{}}|{tool:{name}}}`, `additionalModelRequestFields{top_k}` (`:122-141,424`); blocks
  `toolUse`, `toolResult{toolUseId,content[text|json|image],status}`, `reasoningContent{reasoningText{text,signature}}`,
  `image{format,source{bytes}}`, `document{format,name,source{bytes}}` (pdf,csv,doc,docx,xls,xlsx,html,txt,md;
  `utils/bedrock-media.ts:32-58`); cache = positional `{cachePoint:{type:"default"}}` after marked block, max 4
  (`utils/bedrock-cache.ts:8-35`). Framing: AWS event-stream (BE u32 total length, CRC via `@smithy/eventstream-codec`), skip
  frames whose `:message-type` ≠ `event`, payload wrapped as `{[":event-type"]: json}` minus key `p` (`bedrock-event-stream.ts:35-85`).
  Events: `contentBlockStart.start.toolUse`, `contentBlockDelta.delta.{text|reasoningContent|toolUse.input}`, `contentBlockStop`,
  `messageStop.stopReason`, `metadata.usage`, exceptions (`internalServerException`, `modelStreamErrorException`,
  `serviceUnavailableException`, `throttlingException` → retryable; `validationException` → overflow check). Finish end_turn/
  stop_sequence→stop, max_tokens→length, tool_use→tool-calls, content_filtered/guardrail_intervened→content-filter. SigV4 via
  `aws4fetch` 1.0.20 `AwsV4Signer{service:"bedrock"}`, `content-type` set before signing, exact body bytes signed
  (`utils/bedrock-auth.ts:28-38,56`); `apiKey` → `Authorization: Bearer` instead (`providers/amazon-bedrock.ts:29`).

### 7.7 Provider facades, executor, schemas
| Facade | Default base | Auth (option → env) |
|---|---|---|
| anthropic | `https://api.anthropic.com/v1` | `x-api-key` ← `ANTHROPIC_API_KEY` |
| openai | `https://api.openai.com/v1` | `authorization: Bearer` ← `OPENAI_API_KEY` (`openai.ts:24`) |
| azure | `https://{resourceName}.openai.azure.com/openai/v1` + query `api-version=v1` | `api-key` ← `AZURE_OPENAI_API_KEY` (authorization removed) (`azure.ts:26,65-72`) |
| amazon-bedrock | `https://bedrock-runtime.{region}.amazonaws.com` | SigV4 or Bearer |
| google | `https://generativelanguage.googleapis.com/v1beta` | `x-goog-api-key` ← `GOOGLE_GENERATIVE_AI_API_KEY` |
| github-copilot | required | Bearer; responses if `/^gpt-(\d+)/`≥5 and not `gpt-5-mini` (`github-copilot.ts:19-25`) |
| openrouter | `https://openrouter.ai/api/v1` | Bearer ← `OPENROUTER_API_KEY`; `usage:true`→`{usage:{include:true}}`, `promptCacheKey`→`prompt_cache_key` (`openrouter.ts:56-67`) |
| xai | `https://api.x.ai/v1` | Bearer ← `XAI_API_KEY` |
| cloudflare-ai-gateway | `https://gateway.ai.cloudflare.com/v1/{accountId}/{gatewayId\\|"default"}/compat` | `cf-aig-authorization: Bearer` ← `CLOUDFLARE_API_TOKEN`/`CF_AIG_TOKEN` (`cloudflare.ts:42-51`) |
| cloudflare-workers-ai | `https://api.cloudflare.com/client/v4/accounts/{accountId}/ai/v1` | Bearer ← `CLOUDFLARE_API_KEY`/`CLOUDFLARE_WORKERS_AI_TOKEN` |
| openai-compatible profiles (`openai-compatible-profile.ts:7-15`) | baseten `https://inference.baseten.co/v1`, cerebras `https://api.cerebras.ai/v1`, deepinfra `https://api.deepinfra.com/v1/openai`, deepseek `https://api.deepseek.com/v1`, fireworks `https://api.fireworks.ai/inference/v1`, groq `https://api.groq.com/openai/v1`, openrouter, togetherai `https://api.together.xyz/v1`, xai | Bearer |

- Native GPT-5 defaults (`providers/openai-options.ts:44-70`) mirror §5.4.2.
- Executor (`route/executor.ts`): `MAX_RETRIES=2`, base 500 ms, max 10 s; delay `min(retryAfterMs,10000)` or `rand[0.8,1.2]×500×2^n`;
  retries only HTTP-status phase, never mid-stream; retryable = RateLimit(429), ProviderInternal(≥500). Classification order:
  body `/content[-_\s]?policy|content_filter|safety/i` → ContentPolicy (any status) → 401 auth invalid → 403 → 429 quota
  (`/insufficient[-_\s]?quota|quota[-_\s]?exceeded/i`) or RateLimit → 400/404/409/413/422 InvalidRequest(+overflow) → ≥500 →
  Unknown (`:225-275`). Request-id headers: `x-request-id, request-id, x-amzn-requestid, x-amz-request-id, x-goog-request-id,
  cf-ray`. No HTTP timeouts in `packages/llm`.
- `FinishReason` = `stop | length | tool-calls | content-filter | error | unknown` (`schema/ids.ts:39`).
- `Usage` (`schema/events.ts:51-60`): `inputTokens` (incl. cache), `outputTokens` (incl. reasoning), `nonCachedInputTokens`,
  `cacheReadInputTokens`, `cacheWriteInputTokens`, `reasoningTokens`, `totalTokens`, `providerMetadata`.
- LLMEvent variants (`schema/events.ts:209-226`): `step-start{index}`, `text-start/-delta/-end`, `reasoning-start/-delta/-end`,
  `tool-input-start{id,name}`, `tool-input-delta{id,name,text}`, `tool-input-end`, `tool-call{id,name,input,providerExecuted?}`,
  `tool-result{id,name,result:ToolResultValue,output?}`, `tool-error{id,name,message,error?}`, `step-finish{index,reason,usage?}`,
  `finish{reason,usage?}`, `provider-error{message,classification?:"context-overflow",retryable?}` — all with optional
  `providerMetadata`. `ToolResultValue = {type:"json"|"text"|"error", value} | {type:"content", value: ({type:"text",text} |
  {type:"file",uri,mime,name?})[]}` (`schema/src/llm.ts:12-25`).
- Tool schema: `ToolSchemaProjection.openAI` (`utils/tool-schema.ts:48-64`) forces root `type:"object"`, merges root `anyOf`
  properties (earlier wins) + `additionalProperties:false`, strips `{type:"null"}` from `anyOf` recursively. Gemini projection keeps
  only `description, required, format, type, nullable, enum, properties, items, allOf, anyOf, oneOf, minLength`.
- Tool JSON streaming (`utils/tool-stream.ts`): concatenate raw text, parse once at finish (empty → `{}`), emit `tool-input-end` then
  `tool-call`.
- Cache policy (`cache-policy.ts:18-22,42,67-83,99-100`): only routes `anthropic-messages`, `bedrock-converse`; `auto` marks last
  tool, last system part, last text part of latest user message.

---

## 8. Porting risks (ranked)

1. **The real wire encoder is the AI SDK (+ patches), not `packages/llm`.** Default path = `@ai-sdk/*` npm code that is not in the
   repo. `packages/llm` diverges in places (no Anthropic betas, no adaptive thinking, Chat `max_tokens`/`stream_options` always,
   option-namespace mismatches `openai` vs providerID, `gemini` vs `google`). Capture golden HTTP traffic from the TS build per
   protocol before freezing Rust encoders (§10 item 1).
2. **Option namespacing.** `providerOptions` key = `sdkKey(npm)` or `providerID.split(".")[0]` (§5.4.3); openai-compatible SDK reads
   by provider *name*; Copilot chat reads `copilot` + `github-copilot`; Azure needs both `openai` and `azure`. Wrong key = silently
   dropped reasoning/caching options.
3. **Message-transform ordering and early return** (Mistral branch returns before DeepSeek/interleaved steps; caching before key
   remap; itemId stripping) — port `transform.ts` 1:1 with golden tests (port `opencode/test/provider/transform.test.ts`, `provider.test.ts`, `error.test.ts`, `header-timeout.test.ts`).
4. **Token accounting contract**: `inputTokens` must include cache read+write and `outputTokens` must include reasoning, else
   §6.4 subtraction yields wrong costs; Copilot nano-AIU overrides price.
5. **Plugin loaders inject `fetch`** (Copilot, Codex, xAI, Azure, Snowflake, Vertex); Rust needs a request-interceptor abstraction
   with case-insensitive header maps that can delete SDK-added auth headers (`x-api-key`, `authorization`, `api-key`).
6. **Shared on-disk state with a running TS opencode**: `auth.json` (0600, trailing-slash key normalisation), models cache
   filename, directory-lock scheme `locks/<sha1>.lock/` with heartbeat; refresh writes must not clobber concurrent writers.
7. **Dynamic npm providers** (`Npm.add`) are unportable; catalog entries with exotic npm (sap, watsonx, aihubmix, merge-gateway,
   qvac, salad) need explicit decisions.
8. **Timeout semantics**: chunk timeout only for `text/event-stream`; header timeout ends at headers; `false` disables; Bedrock
   event-stream has no idle timeout.
9. **Retry layering**: SDK retries off (`maxRetries:0`) in main loop; session retry handles all, with header-driven delays and
   5xx-always-retry; title generation uses SDK `maxRetries:2` (AI SDK internal backoff UNKNOWN).
10. **Codex/Copilot quirks**: raw GitHub token as Copilot Bearer (no exchange), non-persisted `slow_down`, Codex callback on all
    interfaces port 1455, sticky WS fallback, Codex hooks apply to API-key OpenAI too.
11. **Surrogate sanitisation**: Rust `String` cannot hold lone surrogates — sanitise at JSON decode / storage read time.
12. **External packages with UNKNOWN internals**: `gitlab-ai-provider@6.23.1`, `opencode-gitlab-auth@2.1.0`, `opencode-poe-auth@0.0.1`,
    `ai-gateway-provider@3.2.0`, `@ai-sdk/google-vertex` URL/auth, `@openrouter/ai-sdk-provider@2.9.0` body extensions.

---

## 9. Implementation sketch (Rust crates)

| Concern | Crate / approach |
|---|---|
| HTTP + streaming | `reqwest` (rustls, `stream` feature), `eventsource-stream` over `bytes_stream()`; per-chunk `tokio::time::timeout` = chunkTimeout |
| AWS | `aws-config` (profile/env/web-identity/container chain = `fromNodeProviderChain`), `aws-sigv4` (service `bedrock`), `aws-smithy-eventstream` for frame decode |
| GCP | `gcp_auth` / `google-cloud-auth` ADC, scope `https://www.googleapis.com/auth/cloud-platform` |
| OAuth | `oauth2` (PKCE S256, device grant), loopback via `axum`; JWT claim decode with `base64` + `serde_json` (no signature check, as TS) |
| JSON schema transforms | `serde_json::Value` recursive functions mirroring `transform.ts:1491-1715` |
| Regex | `regex` (+`fancy-regex` only for the surrogate/lookahead patterns: `GEMINI_3_5_FLASH` uses `(?!-lite)`) |
| Decimal cost | `rust_decimal` |
| Locks | own impl of the `mkdir`-based lock (§3) |
| Hashing | `sha1` (`Hash.fast`) for cache filenames, lock names, SDK cache key |

Crate layout suggestion: `oc-llm-core` (types: `ModelMessage`, `LlmEvent`, `Usage`, `FinishReason`, errors) → `oc-llm-protocols`
(`openai_chat`, `openai_responses`, `anthropic`, `gemini`, `bedrock`, `copilot_*`) → `oc-providers` (registry §2, transforms §5,
catalog §3) → `oc-auth` (auth.json, plugins §4). Session crate depends only on `LlmService::stream(StreamInput) ->
impl Stream<Item = Result<LlmEvent, LlmError>>`.

---

## 10. Required additional data (UNKNOWN list)

| # | Item | How to determine |
|---|---|---|
| 1 | Exact HTTP bodies/headers the AI SDK path sends per protocol (ground truth for parity) | Run TS opencode with a MITM proxy (`HTTPS_PROXY` + `NODE_EXTRA_CA_CERTS`, or `bun --inspect` fetch hook) against openai, anthropic, openai-compatible, google, copilot, bedrock; or `bun install` then read `node_modules/@ai-sdk/<pkg>/dist/index.mjs` at the pinned versions |
| 2 | `ai@6.0.168` internals: `streamText` retry/backoff for `maxRetries:2`, `combineHeaders` order, `withUserAgentSuffix` | `npm pack ai@6.0.168`, read `dist/index.mjs` and `@ai-sdk/provider-utils` |
| 3 | Whether non-`openai` providers on npm `@ai-sdk/openai` (`sdk.languageModel`) hit `/responses` or `/chat/completions` | `node_modules/@ai-sdk/openai/dist/index.mjs` → `createOpenAI().languageModel` |
| 4 | `@ai-sdk/google-vertex@4.0.181` URL template and auth (ADC inside SDK since opencode deletes its fetch) and env read by `@ai-sdk/google` (`GOOGLE_GENERATIVE_AI_API_KEY`?) | read SDK dist |
| 5 | GitLab: `gitlab-ai-provider@6.23.1` protocol, `opencode-gitlab-auth@2.1.0` OAuth (client id, scopes, endpoints), `opencode-poe-auth@0.0.1` | `npm pack <pkg>@<ver> && tar xzf *.tgz` and read `package/dist` |
| 6 | Copilot: which event carries `copilot_usage`; Anthropic-on-Copilot URL building (`${base}/v1` + `/messages`) | capture raw SSE with `includeRawChunks`; read `@ai-sdk/anthropic` URL code |
| 7 | Bedrock: where patched SDK sends merged anthropic betas; event-stream `:message-type` for exceptions; aws4fetch signed header set | `npm pack @ai-sdk/amazon-bedrock@4.0.166`; AWS docs / recorded throttling response; `aws4fetch@1.0.20` source |
| 8 | `@openrouter/ai-sdk-provider@2.9.0`, `ai-gateway-provider@3.2.0` request extensions (`reasoning`, `usage`, `cf-aig-*` header mapping in `parseAiGatewayOptions`) | `npm pack` and read |
| 9 | `remeda.mergeDeep` array semantics (replace vs concat) used throughout option merging | `node_modules/remeda/dist/mergeDeep.js` |
| 10 | Live models.dev values (catalog drift vs operator cache sample in §2.6) | `curl -s https://models.opencode.ai/api.json \\| jq 'map_values({api,env,npm})'` |
