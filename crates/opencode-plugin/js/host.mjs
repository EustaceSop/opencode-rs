// opencode plugin sidecar host. Runs under Bun; speaks newline-delimited JSON-RPC 2.0 on stdio.
// Loader behaviour mirrors opencode v1.19.5 `opencode/src/plugin/{index,loader,shared}.ts` and `tool/registry.ts`.
import path from "node:path"
import fs from "node:fs"
import { fileURLToPath, pathToFileURL } from "node:url"

const stdout = process.stdout
for (const key of ["log", "info", "debug"]) console[key] = (...args) => console.error(...args)

const DEPRECATED_PLUGIN_PACKAGES = ["opencode-openai-codex-auth", "opencode-copilot-auth"]
const INDEX_FILES = ["index.ts", "index.tsx", "index.js", "index.mjs", "index.cjs"]
const TRIGGERS = [
  "chat.message",
  "chat.params",
  "chat.headers",
  "permission.ask",
  "command.execute.before",
  "tool.execute.before",
  "shell.env",
  "tool.execute.after",
  "experimental.chat.messages.transform",
  "experimental.chat.system.transform",
  "experimental.provider.small_model",
  "experimental.session.compacting",
  "experimental.compaction.autocontinue",
  "experimental.text.complete",
  "tool.definition",
]

const state = {
  opts: {},
  hooks: [],
  tools: new Map(),
  workspaces: new Map(),
  functions: new Map(),
  callbacks: new Map(),
  streams: new Map(),
  running: new Map(),
  pending: new Map(),
  seq: 1,
}

class RpcError extends Error {
  constructor(code, message, data) {
    super(message)
    this.code = code
    this.data = data
  }
}

function send(msg) {
  let text
  try {
    text = JSON.stringify(msg)
  } catch (error) {
    if (msg.id === undefined || msg.method) throw error
    text = JSON.stringify({
      jsonrpc: "2.0",
      id: msg.id,
      error: { code: -32603, message: `result is not JSON-serializable: ${errorMessage(error)}` },
    })
  }
  stdout.write(text + "\n")
}

function notify(method, params) {
  send({ jsonrpc: "2.0", method, params })
}

function request(method, params) {
  const id = `s${state.seq++}`
  return new Promise((resolve, reject) => {
    state.pending.set(id, { resolve, reject })
    send({ jsonrpc: "2.0", id, method, params })
  })
}

function errorMessage(error) {
  if (error instanceof Error) return error.message || error.name
  if (typeof error === "string") return error
  try {
    return JSON.stringify(error) ?? String(error)
  } catch {
    return String(error)
  }
}

function isRecord(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value)
}

function exists(file) {
  return fs.existsSync(file)
}

function isDirectory(file) {
  try {
    return fs.statSync(file).isDirectory()
  } catch {
    return false
  }
}

function importTarget(value) {
  if (value.startsWith("file://")) return value
  if (path.isAbsolute(value) && !isDirectory(value)) return pathToFileURL(value).href
  return value
}

// ---- plugin resolution (shared.ts) ----

function isAbsolutePath(raw) {
  return path.isAbsolute(raw) || /^[A-Za-z]:[\\/]/.test(raw)
}

function isPathPluginSpec(spec) {
  return spec.startsWith("file://") || spec.startsWith(".") || isAbsolutePath(spec)
}

function resolveDirectoryIndex(dir) {
  for (const name of INDEX_FILES) {
    const file = path.join(dir, name)
    if (exists(file)) return file
  }
}

function resolvePathPluginTarget(spec) {
  const raw = spec.startsWith("file://") ? fileURLToPath(spec) : spec
  const file = isAbsolutePath(raw) ? raw : path.resolve(raw)
  if (!isDirectory(file)) {
    if (spec.startsWith("file://")) return spec
    return pathToFileURL(file).href
  }
  if (exists(path.join(file, "package.json"))) return pathToFileURL(file).href
  const index = resolveDirectoryIndex(file)
  if (index) return pathToFileURL(index).href
  throw new Error(`Plugin directory ${file} is missing package.json or index file`)
}

function readPluginPackage(target) {
  const file = target.startsWith("file://") ? fileURLToPath(target) : target
  const dir = isDirectory(file) ? file : path.dirname(file)
  const pkg = path.join(dir, "package.json")
  const json = JSON.parse(fs.readFileSync(pkg, "utf8"))
  return { dir, pkg, json }
}

function tryReadPluginPackage(target) {
  try {
    return readPluginPackage(target)
  } catch {
    return undefined
  }
}

function extractExportValue(value) {
  if (typeof value === "string") return value
  if (!isRecord(value)) return undefined
  for (const key of ["import", "default"]) {
    if (typeof value[key] === "string") return value[key]
  }
  return undefined
}

function resolvePackagePath(spec, raw, pkg) {
  const resolved = raw.startsWith("file://") ? fileURLToPath(raw) : path.isAbsolute(raw) ? raw : path.resolve(pkg.dir, raw)
  const root = path.resolve(pkg.dir)
  const next = path.resolve(resolved)
  const rel = path.relative(root, next)
  if (rel.startsWith("..") || path.isAbsolute(rel)) {
    throw new Error(`Plugin ${spec} resolved server entry outside plugin directory`)
  }
  return pathToFileURL(next).href
}

function resolvePackageEntrypoint(spec, pkg) {
  const exports = pkg.json.exports
  if (isRecord(exports)) {
    const raw = extractExportValue(exports["./server"])
    if (raw) return resolvePackagePath(spec, raw, pkg)
  }
  const main = typeof pkg.json.main === "string" ? pkg.json.main.trim() : ""
  if (!main) return
  return resolvePackagePath(spec, main, pkg)
}

function resolvePluginEntrypoint(spec, source, target, pkg) {
  if (!pkg) return target
  const entry = resolvePackageEntrypoint(spec, pkg)
  if (entry) return entry
  const file = target.startsWith("file://") ? fileURLToPath(target) : path.isAbsolute(target) ? target : undefined
  const dir = file && isDirectory(file) ? file : undefined
  if (dir && isRecord(pkg.json.exports)) {
    if (source === "file") {
      const index = resolveDirectoryIndex(dir)
      if (index) return pathToFileURL(index).href
    }
    return
  }
  return target
}

function checkPluginCompatibility(version, pkg) {
  if (!version || !pkg) return
  const match = /^(\d+)\.\d+\.\d+/.exec(version)
  if (!match || Number(match[1]) === 0) return
  const engines = pkg.json.engines
  if (!isRecord(engines)) return
  const range = engines.opencode
  if (typeof range !== "string") return
  if (!Bun.semver.satisfies(version, range)) {
    throw new Error(`Plugin requires opencode ${range} but running ${version}`)
  }
}

async function resolveAndImport(item, errors) {
  const spec = item.spec
  if (DEPRECATED_PLUGIN_PACKAGES.some((pkg) => spec.includes(pkg))) return
  const source = isPathPluginSpec(spec) ? "file" : "npm"
  const fail = (stage, error) => {
    const cause = error instanceof Error ? (error.cause ?? error) : error
    errors.push({ stage, spec, message: errorMessage(stage === "load" ? error : cause) })
  }

  let target = ""
  try {
    if (source === "file") target = resolvePathPluginTarget(spec)
    else {
      if (!item.npm) throw new Error(`Plugin ${spec} has no install directory`)
      target = path.join(item.npm.dir, "node_modules", item.npm.name)
      if (!exists(target)) throw new Error(`Plugin ${spec} is not installed (expected ${target})`)
    }
  } catch (error) {
    return fail("install", error)
  }

  let pkg
  let entry
  try {
    pkg = source === "npm" ? readPluginPackage(target) : tryReadPluginPackage(target)
    entry = resolvePluginEntrypoint(spec, source, target, pkg)
  } catch (error) {
    return fail("entry", error)
  }
  if (!entry) {
    errors.push({ stage: "missing", spec, message: `Plugin ${spec} does not expose a server entrypoint` })
    return
  }

  if (source === "npm") {
    try {
      checkPluginCompatibility(state.opts.opencodeVersion, pkg)
    } catch (error) {
      return fail("compatibility", error)
    }
  }

  let mod
  try {
    mod = await import(importTarget(entry))
  } catch (error) {
    return fail("load", error)
  }
  if (!mod) return fail("load", new Error(`Plugin ${spec} module is empty`))
  return { spec, options: item.options, source, target, entry, pkg, mod }
}

// ---- module shape (index.ts applyPlugin) ----

function readV1Plugin(mod, spec) {
  const value = mod.default
  if (!isRecord(value)) return
  if (!("id" in value) && !("server" in value) && !("tui" in value)) return
  const server = "server" in value ? value.server : undefined
  const tui = "tui" in value ? value.tui : undefined
  if (server !== undefined && typeof server !== "function") throw new TypeError(`Plugin ${spec} has invalid server export`)
  if (tui !== undefined && typeof tui !== "function") throw new TypeError(`Plugin ${spec} has invalid tui export`)
  if (server !== undefined && tui !== undefined) {
    throw new TypeError(`Plugin ${spec} must default export either server() or tui(), not both`)
  }
  if (server === undefined) throw new TypeError(`Plugin ${spec} must default export an object with server()`)
  return value
}

function readPluginId(id, spec) {
  if (id === undefined) return
  if (typeof id !== "string") throw new TypeError(`Plugin ${spec} has invalid id type ${typeof id}`)
  const value = id.trim()
  if (!value) throw new TypeError(`Plugin ${spec} has an empty id`)
  return value
}

function resolvePluginId(load, id) {
  if (load.source === "file") {
    if (id) return id
    throw new TypeError(`Path plugin ${load.spec} must export id`)
  }
  if (id) return id
  const hit = load.pkg ?? readPluginPackage(load.target)
  if (typeof hit.json.name !== "string" || !hit.json.name.trim()) {
    throw new TypeError(`Plugin package ${hit.pkg} is missing name`)
  }
  return hit.json.name.trim()
}

function getServerPlugin(value) {
  if (typeof value === "function") return value
  if (!value || typeof value !== "object" || !("server" in value)) return
  if (typeof value.server !== "function") return
  return value.server
}

function getLegacyPlugins(mod) {
  const seen = new Set()
  const result = []
  for (const entry of Object.values(mod)) {
    if (seen.has(entry)) continue
    seen.add(entry)
    const plugin = getServerPlugin(entry)
    if (!plugin) throw new TypeError("Plugin export is not a function")
    result.push(plugin)
  }
  return result
}

async function applyPlugin(load, input) {
  const dir = entryDirectory(load.entry)
  const plugin = readV1Plugin(load.mod, load.spec)
  if (plugin) {
    const id = resolvePluginId(load, readPluginId(plugin.id, load.spec))
    state.hooks.push({ spec: load.spec, id, dir, hooks: (await plugin.server(input, load.options)) ?? {} })
    return
  }
  for (const server of getLegacyPlugins(load.mod)) {
    state.hooks.push({ spec: load.spec, dir, hooks: (await server(input, load.options)) ?? {} })
  }
}

function entryDirectory(entry) {
  const file = entry.startsWith("file://") ? fileURLToPath(entry) : entry
  return isDirectory(file) ? file : path.dirname(file)
}

// ---- SDK client ----

function missingClient() {
  return new Proxy(
    {},
    {
      get(_target, key) {
        if (typeof key !== "string" || key === "then" || key === "toJSON") return undefined
        throw new Error(
          `@opencode-ai/sdk could not be resolved by the plugin host, so \`client.${key}\` is unavailable; install it or pass sdkPath`,
        )
      },
    },
  )
}

async function makeClient(p) {
  const candidates = []
  if (p.sdkPath) candidates.push(p.sdkPath)
  for (const dir of p.resolveDirs ?? []) {
    try {
      candidates.push(Bun.resolveSync("@opencode-ai/sdk", dir))
    } catch {}
  }
  for (const candidate of candidates) {
    try {
      const mod = await import(importTarget(candidate))
      if (typeof mod.createOpencodeClient !== "function") continue
      const client = mod.createOpencodeClient({
        baseUrl: p.serverUrl ?? "http://localhost:4096",
        directory: p.directory,
        headers: p.headers ?? undefined,
      })
      return { client, sdk: candidate }
    } catch (error) {
      console.error(`plugin host: failed to load SDK from ${candidate}: ${errorMessage(error)}`)
    }
  }
  return { client: missingClient(), sdk: null }
}

// ---- tools (registry.ts fromPlugin) ----

const zodCache = new Map()

async function importZod(target) {
  if (zodCache.has(target)) return zodCache.get(target)
  let hit
  try {
    const mod = await import(importTarget(target))
    for (const z of [mod.z, mod.default, mod]) {
      if (z && typeof z.toJSONSchema === "function" && typeof z.object === "function") {
        hit = z
        break
      }
    }
  } catch {}
  zodCache.set(target, hit)
  return hit
}

async function loadZod(dir) {
  const dirs = [dir, ...(state.opts.resolveDirs ?? [])].filter(Boolean)
  for (const base of dirs) {
    for (const name of ["zod", "zod/v4"]) {
      let resolved
      try {
        resolved = Bun.resolveSync(name, base)
      } catch {
        continue
      }
      const z = await importZod(resolved)
      if (z) return z
    }
  }
  if (state.opts.zodPath) return importZod(state.opts.zodPath)
}

function isZodType(value) {
  return typeof value === "object" && value !== null && "_zod" in value
}

function isPluginTool(value) {
  return typeof value === "object" && value !== null && "args" in value && "description" in value && "execute" in value
}

function isJsonSchemaDefinition(value) {
  return typeof value === "boolean" || (typeof value === "object" && value !== null && !Array.isArray(value))
}

function legacyJsonSchema(entries) {
  const properties = Object.fromEntries(entries.filter((entry) => isJsonSchemaDefinition(entry[1])))
  return { type: "object", properties, required: Object.keys(properties) }
}

function zodMetadataRegistry(z, schema) {
  const registry = z.registry()
  const seen = new WeakSet()
  const collect = (value) => {
    if (typeof value !== "object" || value === null) return
    if (seen.has(value)) return
    seen.add(value)
    if (isZodType(value)) {
      const metadata = typeof value.meta === "function" ? value.meta() : undefined
      const description = typeof value.description === "string" ? value.description : undefined
      const merged = {
        ...(metadata && typeof metadata === "object" ? metadata : {}),
        ...(description ? { description } : {}),
      }
      if (Object.keys(merged).length) registry.add(value, merged)
      collect(value._zod.def)
      return
    }
    for (const item of Object.values(value)) collect(item)
  }
  collect(schema)
  return registry
}

function normalizeZodJsonSchema(value) {
  if (Array.isArray(value)) return value.map((item) => normalizeZodJsonSchema(item))
  if (typeof value !== "object" || value === null) return value
  return Object.fromEntries(
    Object.entries(value)
      .filter(
        (entry) => !((entry[0] === "exclusiveMaximum" || entry[0] === "exclusiveMinimum") && typeof entry[1] === "boolean"),
      )
      .map(([key, item]) => [key, normalizeZodJsonSchema(item)]),
  )
}

function zodJsonSchema(z, schema) {
  const result = normalizeZodJsonSchema(z.toJSONSchema(schema, { io: "input", metadata: zodMetadataRegistry(z, schema) }))
  if (!isRecord(result)) throw new Error("plugin tool Zod schema produced a non-object JSON Schema")
  const { $defs, ...rest } = result
  return $defs && isRecord($defs) ? { ...rest, definitions: $defs } : rest
}

// Used only when no `zod` module with toJSONSchema can be resolved; walks Zod v4 `_zod.def`.
function fallbackZodSchema(value) {
  const def = value?._zod?.def ?? {}
  const describe = (schema) => {
    const description = typeof value.description === "string" ? value.description : undefined
    return description ? { ...schema, description } : schema
  }
  switch (def.type) {
    case "string":
      return describe({ type: "string" })
    case "number":
    case "int":
      return describe({ type: "number" })
    case "boolean":
      return describe({ type: "boolean" })
    case "null":
      return describe({ type: "null" })
    case "array":
      return describe({ type: "array", items: fallbackZodSchema(def.element) })
    case "enum":
      return describe({ enum: Object.values(def.entries ?? {}) })
    case "literal":
      return describe({ enum: def.values ?? [] })
    case "union":
      return describe({ anyOf: (def.options ?? []).map(fallbackZodSchema) })
    case "record":
      return describe({ type: "object", additionalProperties: fallbackZodSchema(def.valueType) })
    case "object":
      return describe(fallbackObjectSchema(def.shape ?? {}))
    case "nullable":
      return describe({ anyOf: [fallbackZodSchema(def.innerType), { type: "null" }] })
    case "optional":
    case "default":
    case "prefault":
    case "readonly":
    case "nonoptional":
    case "catch":
      return describe(fallbackZodSchema(def.innerType))
    default:
      return describe({})
  }
}

function fallbackObjectSchema(shape) {
  const properties = {}
  const required = []
  for (const [key, value] of Object.entries(shape)) {
    properties[key] = fallbackZodSchema(value)
    const type = value?._zod?.def?.type
    if (type !== "optional" && type !== "default" && type !== "prefault") required.push(key)
  }
  return { type: "object", properties, required }
}

async function registerTool(id, def, dir) {
  const args = def.args ?? {}
  const entries = Object.entries(args)
  const allZod = entries.every((entry) => isZodType(entry[1]))
  let parameters
  let validate
  let converter = "legacy"
  if (allZod) {
    const z = await loadZod(dir)
    if (z) {
      const schema = z.object(args)
      parameters = zodJsonSchema(z, schema)
      validate = (value) => schema.safeParse(value)
      converter = "zod"
    } else {
      parameters = fallbackObjectSchema(args)
      converter = "fallback"
    }
  } else {
    parameters = legacyJsonSchema(entries)
  }
  state.tools.delete(id)
  state.tools.set(id, { id, def, description: def.description, parameters, validate, converter })
}

function toolList() {
  return Array.from(state.tools.values(), (tool) => ({
    id: tool.id,
    description: tool.description,
    parameters: tool.parameters,
    converter: tool.converter,
  }))
}

async function toolExecute(p, requestID, signal) {
  const tool = state.tools.get(p.id)
  if (!tool) throw new RpcError(-32601, `Unknown plugin tool ${p.id}`)
  const args = p.args ?? {}
  if (tool.validate) {
    const parsed = tool.validate(args)
    if (!parsed.success) {
      throw new RpcError(-32602, errorMessage(parsed.error), { kind: "invalid_arguments", tool: p.id })
    }
  }
  const base = p.context ?? {}
  const ctx = {
    ...base,
    abort: signal,
    // The original passes an un-run Effect here, so calls are no-ops (spec 05 §4.7).
    metadata() {},
    ask(input) {
      return request("tool.ask", {
        requestID,
        callID: base.callID,
        sessionID: base.sessionID,
        permission: input?.permission,
        patterns: input?.patterns ?? [],
        always: input?.always ?? [],
        metadata: input?.metadata ?? {},
      }).then(() => undefined)
    },
    directory: state.opts.directory,
    worktree: state.opts.worktree,
  }
  const result = await tool.def.execute(args, ctx)
  if (typeof result === "string") return { title: "", output: result, metadata: {} }
  return {
    title: result?.title ?? "",
    output: result?.output,
    metadata: result?.metadata ?? {},
    ...(result?.attachments ? { attachments: result.attachments } : {}),
  }
}

// ---- init ----

function describePlugins() {
  return state.hooks.map((item, index) => ({
    index,
    spec: item.spec,
    id: item.id ?? null,
    hooks: Object.keys(item.hooks).filter((key) => typeof item.hooks[key] === "function"),
    tools: Object.keys(item.hooks.tool ?? {}),
    auth: item.hooks.auth ? item.hooks.auth.provider : null,
    provider: item.hooks.provider ? item.hooks.provider.id : null,
  }))
}

async function init(p) {
  state.opts = p
  const errors = []
  const { client, sdk } = await makeClient(p)
  const serverUrl = new URL(p.serverUrl ?? "http://localhost:4096")
  const input = {
    client,
    project: p.project,
    worktree: p.worktree,
    directory: p.directory,
    experimental_workspace: {
      register(type, adapter) {
        state.workspaces.set(type, adapter)
      },
    },
    get serverUrl() {
      return serverUrl
    },
    $: typeof Bun === "undefined" ? undefined : Bun.$,
  }

  const loaded = await Promise.all((p.plugins ?? []).map((item) => resolveAndImport(item, errors)))
  for (const load of loaded) {
    if (!load) continue
    try {
      await applyPlugin(load, input)
    } catch (error) {
      errors.push({ stage: "apply", spec: load.spec, message: errorMessage(error) })
    }
  }

  for (const file of p.toolFiles ?? []) {
    const namespace = path.basename(file, path.extname(file))
    try {
      const mod = await import(pathToFileURL(file).href)
      for (const [id, def] of Object.entries(mod)) {
        if (!isPluginTool(def)) continue
        await registerTool(id === "default" ? namespace : `${namespace}_${id}`, def, path.dirname(file))
      }
    } catch (error) {
      errors.push({ stage: "tool", spec: file, message: errorMessage(error) })
    }
  }
  for (const item of state.hooks) {
    for (const [id, def] of Object.entries(item.hooks.tool ?? {})) {
      try {
        await registerTool(id, def, item.dir)
      } catch (error) {
        errors.push({ stage: "tool", spec: item.spec, message: `${id}: ${errorMessage(error)}` })
      }
    }
  }

  const hooks = new Set()
  for (const item of state.hooks) {
    for (const key of Object.keys(item.hooks)) if (typeof item.hooks[key] === "function") hooks.add(key)
  }
  return {
    bun: Bun.version,
    sdk,
    plugins: describePlugins(),
    hooks: Array.from(hooks),
    tools: toolList(),
    workspaces: Array.from(state.workspaces.keys()),
    errors,
  }
}

// ---- hooks ----

async function trigger(p) {
  const output = p.output
  if (!p.name) return { output }
  if (!TRIGGERS.includes(p.name)) throw new RpcError(-32602, `Unknown hook ${p.name}`)
  for (const item of state.hooks) {
    const fn = item.hooks[p.name]
    if (!fn) continue
    await fn(p.input, output)
  }
  return { output }
}

async function config(p) {
  const cfg = p.config
  for (const item of state.hooks) {
    try {
      await item.hooks.config?.(cfg)
    } catch (error) {
      console.error(`plugin config hook failed (${item.spec}): ${errorMessage(error)}`)
    }
  }
  return { config: cfg }
}

function event(p) {
  for (const item of state.hooks) {
    try {
      const result = item.hooks.event?.({ event: p.event })
      if (result && typeof result.catch === "function") {
        result.catch((error) => console.error(`plugin event hook failed (${item.spec}): ${errorMessage(error)}`))
      }
    } catch (error) {
      console.error(`plugin event hook failed (${item.spec}): ${errorMessage(error)}`)
    }
  }
}

// ---- auth / provider ----

function authHook(provider) {
  let hit
  for (const item of state.hooks) if (item.hooks.auth?.provider === provider) hit = item.hooks.auth
  if (!hit) throw new RpcError(-32602, `No plugin provides auth for ${provider}`)
  return hit
}

function serializePrompt(prompt) {
  const { validate, condition, ...rest } = prompt
  return { ...rest, validate: typeof validate === "function", condition: typeof condition === "function" }
}

function authList() {
  return state.hooks
    .map((item, index) => ({ item, index }))
    .filter(({ item }) => item.hooks.auth)
    .map(({ item, index }) => ({
      plugin: index,
      provider: item.hooks.auth.provider,
      loader: typeof item.hooks.auth.loader === "function",
      methods: (item.hooks.auth.methods ?? []).map((method) => ({
        type: method.type,
        label: method.label,
        authorize: typeof method.authorize === "function",
        ...(method.prompts ? { prompts: method.prompts.map(serializePrompt) } : {}),
      })),
    }))
}

function handle(map, prefix, value) {
  const id = `${prefix}${state.seq++}`
  map.set(id, value)
  return id
}

async function authAuthorize(p) {
  const method = authHook(p.provider).methods?.[p.method]
  if (!method) throw new RpcError(-32602, `Auth method ${p.method} not found for ${p.provider}`)
  if (typeof method.authorize !== "function") return null
  if (method.prompts && p.inputs) {
    for (const prompt of method.prompts) {
      if (prompt.type === "text" && prompt.validate && p.inputs[prompt.key] !== undefined) {
        const message = prompt.validate(p.inputs[prompt.key])
        if (message) throw new RpcError(-32602, message, { kind: "validation_failed", field: prompt.key })
      }
    }
  }
  const result = await method.authorize(p.inputs)
  if (!isRecord(result) || typeof result.callback !== "function") return result ?? null
  const { callback, ...rest } = result
  return { ...rest, handle: handle(state.callbacks, "cb", callback) }
}

async function authCallback(p) {
  const callback = state.callbacks.get(p.handle)
  if (!callback) throw new RpcError(-32602, `Unknown auth callback handle ${p.handle}`)
  state.callbacks.delete(p.handle)
  return (await (p.code === undefined || p.code === null ? callback() : callback(p.code))) ?? null
}

async function authLoader(p) {
  const hook = authHook(p.provider)
  if (typeof hook.loader !== "function") return { options: {}, functions: {} }
  const getAuth = () => request("auth.get", { provider: p.provider }).catch(() => p.auth)
  const loaded = (await hook.loader(getAuth, p.providerInfo)) ?? {}
  const options = {}
  const functions = {}
  for (const [key, value] of Object.entries(loaded)) {
    if (typeof value === "function") functions[key] = handle(state.functions, "fn", value)
    else options[key] = value
  }
  return { options, functions }
}

function providerList() {
  return state.hooks
    .filter((item) => item.hooks.provider)
    .map((item) => ({ id: item.hooks.provider.id, models: typeof item.hooks.provider.models === "function" }))
}

async function providerModels(p) {
  let hit
  for (const item of state.hooks) if (item.hooks.provider?.id === p.id) hit = item.hooks.provider
  if (!hit) throw new RpcError(-32602, `No plugin provides provider ${p.id}`)
  if (typeof hit.models !== "function") return null
  return await hit.models(p.provider, { auth: p.auth ?? undefined })
}

// ---- fetch proxy ----

async function fetchProxy(p, _requestID, signal) {
  const fn = state.functions.get(p.handle)
  if (typeof fn !== "function") throw new RpcError(-32602, `Unknown function handle ${p.handle}`)
  const controller = new AbortController()
  signal.addEventListener("abort", () => controller.abort())
  const init = { method: p.method ?? "GET", headers: p.headers ?? [], signal: controller.signal }
  if (p.body !== undefined && p.body !== null) init.body = Buffer.from(p.body, "base64")
  const response = await fn(p.url, init)
  state.streams.set(p.stream, controller)
  setTimeout(async () => {
    try {
      if (response.body) {
        for await (const chunk of response.body) {
          if (!state.streams.has(p.stream)) return
          notify("fetch.chunk", { stream: p.stream, data: Buffer.from(chunk).toString("base64") })
        }
      }
      notify("fetch.end", { stream: p.stream })
    } catch (error) {
      notify("fetch.end", { stream: p.stream, error: errorMessage(error) })
    } finally {
      state.streams.delete(p.stream)
    }
  }, 0)
  return {
    status: response.status,
    statusText: response.statusText ?? "",
    headers: Array.from(response.headers?.entries?.() ?? []),
  }
}

function fetchCancel(p) {
  const controller = state.streams.get(p.stream)
  state.streams.delete(p.stream)
  controller?.abort()
}

// ---- shutdown ----

async function shutdown() {
  for (const item of state.hooks) {
    try {
      await item.hooks.dispose?.()
    } catch (error) {
      console.error(`plugin dispose hook failed (${item.spec}): ${errorMessage(error)}`)
    }
  }
  return null
}

// ---- dispatch ----

const methods = {
  init,
  "hook.trigger": trigger,
  config,
  "tool.list": toolList,
  "tool.execute": toolExecute,
  "auth.list": authList,
  "auth.authorize": authAuthorize,
  "auth.callback": authCallback,
  "auth.loader": authLoader,
  "provider.list": providerList,
  "provider.models": providerModels,
  "fetch.proxy": fetchProxy,
  shutdown,
}

const notifications = {
  event,
  "fetch.cancel": fetchCancel,
  "$/cancel"(p) {
    state.running.get(p.id)?.abort()
  },
}

async function handleRequest(msg) {
  const fn = methods[msg.method]
  if (!fn) {
    send({ jsonrpc: "2.0", id: msg.id, error: { code: -32601, message: `Method not found: ${msg.method}` } })
    return
  }
  const controller = new AbortController()
  state.running.set(msg.id, controller)
  try {
    const result = await fn(msg.params ?? {}, msg.id, controller.signal)
    send({ jsonrpc: "2.0", id: msg.id, result: result === undefined ? null : result })
  } catch (error) {
    send({
      jsonrpc: "2.0",
      id: msg.id,
      error: {
        code: error instanceof RpcError ? error.code : -32000,
        message: errorMessage(error),
        data: error instanceof RpcError ? error.data : { stack: error instanceof Error ? error.stack : undefined },
      },
    })
  } finally {
    state.running.delete(msg.id)
  }
}

function handleLine(line) {
  if (!line.trim()) return
  let msg
  try {
    msg = JSON.parse(line)
  } catch {
    console.error(`plugin host: dropped malformed message`)
    return
  }
  if (typeof msg.method === "string") {
    if (msg.id !== undefined && msg.id !== null) void handleRequest(msg)
    else {
      try {
        notifications[msg.method]?.(msg.params ?? {})
      } catch (error) {
        console.error(`plugin host: notification ${msg.method} failed: ${errorMessage(error)}`)
      }
    }
    return
  }
  const waiter = state.pending.get(msg.id)
  if (!waiter) return
  state.pending.delete(msg.id)
  if (msg.error) waiter.reject(Object.assign(new Error(msg.error.message ?? "request failed"), { code: msg.error.code }))
  else waiter.resolve(msg.result)
}

process.on("unhandledRejection", (error) => {
  console.error(`plugin host: unhandled rejection: ${error instanceof Error ? (error.stack ?? error.message) : errorMessage(error)}`)
})

let buffer = ""
process.stdin.setEncoding("utf8")
process.stdin.on("data", (chunk) => {
  buffer += chunk
  for (;;) {
    const index = buffer.indexOf("\n")
    if (index < 0) break
    const line = buffer.slice(0, index)
    buffer = buffer.slice(index + 1)
    handleLine(line)
  }
})
process.stdin.on("end", () => process.exit(0))
