# NAVI — Integration Modes: Pi extensions vs SDK vs RPC vs hybrid vs fork (Agent 2 of 10)

**Scope:** design research only. No implementation, no writes under `vendor/`.
**Evidence base:** read-only clone at `C:\Users\lain\NAVI\vendor\pi` (`github.com/earendil-works/pi`, branch `main`, commit `71dca871`, 2026-09-11; `@earendil-works/pi-coding-agent v0.85.1`).
**Citation convention:** `path:line → symbol → implication`. Tags: `[verified]` (read in source, code paths included), `[inferred]` (reasoned from source), `[external]` (general platform knowledge).
**Doc-vs-code rule applied:** where docs and source disagree, source wins and the disagreement is named.

**Strategies under comparison**
- **A — Pi extensions:** NAVI ships TypeScript extension(s); Pi's own process/lifecycle is the host.
- **B — SDK embedded:** NAVI imports `@earendil-works/pi-coding-agent` and owns the process.
- **C — RPC process:** NAVI spawns `pi --mode rpc` and speaks JSONL over stdio.
- **D — hybrid:** a process split where the SDK is the *embedded* surface in a NAVI-owned agent process and the UI/bridge is a separate process (NAVI's own IPC, not Pi's RPC).
- **E — NAVI fork:** NAVI maintains a rebranded/patched Pi.

---

## Repository Findings

### 1. The public API surface is exactly three subpaths, and it is CI-enforced

**The exports map is the contract.** `packages/coding-agent/package.json:14-28 → exports` declares only `.`, `./rpc-entry` (bundle, `:19-21`), `./client` (`source` condition only, `:22-24`), and `./experimental/plugin` (`source` only, `:25-27`). `[verified]` The `files` whitelist at `:29-39` explicitly excludes `dist/client`, `dist/experimental`, and `dist/cli/experimental`. `[verified]` Implication: **the experimental harness is not installable from npm at all** — it exists only in a checkout under the `source` condition.

**Upstream states this in its own changelog.** `CHANGELOG.md:37` (0.85.1) — *"Fixed SDK import failures caused by unintentionally publishing internal experimental code and dependencies in 0.85.0. The experimental `client` and `experimental/plugin` subpaths and server/client commands are now source-only through `pi-test.sh`; **the supported local SDK and stdio RPC API are unchanged**"*. `[verified]` Implication: **SDK + stdio RPC are the two declared supported integration surfaces; the experimental remote harness is explicitly not one.** `CHANGELOG.md:66` (0.85.0) also shows the `./client` entry point being *restored* after being dropped — the subpath was already broken once in a minor release. `[verified]`

**The root barrel is 425 lines and exports a lot.** `src/index.ts:1-425` re-exports the SDK factories (`:204-231`), the extension type + runtime surface (`:53-172`), session/settings/skills/tool factories, the run modes including `RpcClient`/`runRpcMode` (`:350-366`), and TUI components/themes (`:368-417`). `[verified]` Implication: importing the root barrel from a NAVI UI is cheap to write and expensive to load — the repo treats this as a measured cost (`scripts/check-entry-graphs.mjs:1-12` states a 1-file import through a barrel costs ~37 MB of evaluated module graph), though its budgets are declared **only** for `packages/ai` and `packages/agent` (`:33-43`), not for `coding-agent`. `[verified]`

**One documented API is not in the export map.** `docs/sdk.md:167` instructs SDK hosts to call `runtime.session.bindExtensions(...)`. The method is public (`src/core/agent-session.ts:2468 → bindExtensions(bindings: ExtensionBindings)`), and `ExtensionBindings` is exported from its own module (`:232-239 → interface ExtensionBindings`), but **`ExtensionBindings` does not appear in `src/index.ts`** (verified absent from the export list; `src/index.ts:16-25` exports `AgentSessionConfig` but not `ExtensionBindings`). `[verified]` Implication: a NAVI host can call the method (the class is exported, TS is structural) but cannot name the parameter type from the package root; expect to hand-roll that interface. `[inferred]`

### 2. Extension model: in-process, same-thread, fully privileged, and deliberately not isolated

**Loading is in-process TypeScript via jiti.** `src/core/extensions/loader.ts:501-513 → loadExtensionModule` → `createJiti(...).import(extensionPath, { default: true })`. `[verified]` There is **no worker thread, no child process, no `vm` sandbox** anywhere in `src/core/extensions/` (grep for `isolation|worker_thread|child_process|vm2` returns nothing). `[verified]` `docs/extensions.md:111` — *"Extensions run with your full system permissions and can execute arbitrary code."* and `docs/security.md:31-37` — *"Pi does not include a built-in sandbox… Extensions are TypeScript modules that run with the same permissions."* `[verified]`

**What the extension API actually hands out is a curated subset.** `src/core/extensions/types.ts:309-349 → ExtensionContext` gives `ui`, `mode`, `hasUI`, `cwd`, `sessionManager: ReadonlySessionManager` (`:319`, explicitly read-only), `modelRegistry` (read-only facade), `model`, `scopedModels`, `thinkingLevel`, `isIdle()`, `isProjectTrusted()`, `signal`, `abort()`, `hasPendingMessages()`, `shutdown()`, `getContextUsage()`, `compact()`, `getSystemPrompt()`. `[verified]` `:355-379 → ExtensionCommandContext` adds `waitForIdle()`, `newSession()`, `fork()`, `navigateTree()`, `switchSession()`, `reload()` — *"only available in commands because they can deadlock if called from event handlers"* (`:351-354`, `docs/extensions.md:1117`). `[verified]`

**Event dispatch is serial and per-handler try/caught — except `tool_call`.** `src/core/extensions/runner.ts:851-883 → emit()` iterates extensions × handlers, `await handler(event, ctx)` inside `try/catch`, converting throws into `emitError` (`:869-878`). `[verified]` But `:982-1003 → emitToolCall()` has **no try/catch**: a throwing `tool_call` handler propagates, which `docs/extensions.md:2935` documents as intentional fail-safe (*"`tool_call` errors block the tool (fail-safe)"*). `[verified]` Implication: **a buggy gate extension fails closed (tool blocked) rather than open — good for NAVI permissions — but a slow/hung handler stalls the whole agent loop, because everything is awaited in-process.** `[verified]`→`[inferred]`

**Rendering/UI authority in extensions is TUI-scoped.** `pi.registerTool({renderCall, renderResult})` and `ctx.ui.custom()` return `pi-tui` `Component`s (`docs/extensions.md:2246-2323`, `:2938-2947` mode table). `registerEntryRenderer`/`registerMessageRenderer` are TUI render hooks. `[verified]` Over RPC these degrade: `custom()` returns `undefined`, `setWidget` accepts string arrays only, `getTheme()` returns `undefined`, `setTheme()` returns `{success:false}` (`src/modes/rpc/rpc-mode.ts:195-311`; summary table in `docs/rpc.md:1195-1205`). `[verified]`

**Extension capability that matters for NAVI and is *not* in the doc list.** `ctx.modelRegistry.stream()/streamSimple()` (through configured providers with resolved auth) was added recently (`CHANGELOG.md:7`), and `before_provider_headers` / `before_provider_request` / `after_provider_response` give extensions raw request/response visibility (`docs/extensions.md:687-736`). `[verified]` A NAVI extension can therefore inspect and rewrite provider payloads — a capability that is *not* reachable over RPC at all.

### 3. SDK: the widest authority, in-process, and the one upstream calls "supported"

**Factories.** `src/core/sdk.ts:39-88 → CreateAgentSessionOptions` (cwd, agentDir, modelRuntime, model, thinkingLevel, scopedModels, noTools, tools/excludeTools, `customTools`, `resourceLoader`, `sessionManager`, `settingsManager`, `sessionStartEvent`) and `:91-98 → CreateAgentSessionResult { session, extensionsResult, modelFallbackMessage }`. `[verified]`

**Runtime = session *replacement*.** `src/core/agent-session-runtime.ts:74-107 → class AgentSessionRuntime` owns `session`, `services`, `diagnostics`; `:196 switchSession`, `:226 newSession`, `:262 fork`, `:361 importFromJsonl`, `:406 dispose`. `[verified]` `importFromJsonl` and `setRebindSession` have **no RPC equivalent** (see §4).

**Full session authority.** `src/core/agent-session.ts` exposes `prompt/steer/followUp/sendUserMessage` (`:1175,1425,1437,1571`), `clearQueue` (`:1608`), `setModel`/`cycleModel` (`:1679,1721`), `setThinkingLevel` (`:1814`), `setScopedModels` (`:1028`), `compact` (`:1967`), `bindExtensions` (`:2468`), `reload` (`:2841`), `executeBash`/`recordBashResult` (`:3006,3044`), `navigateTree` (`:3136`), `exportToHtml`/`exportToJsonl` (`:3463,3488`), `getSessionStats`/`getContextUsage` (`:3359,3411`), plus getters `modelRuntime` (`:408`), `resourceLoader` (`:1633`), `promptTemplates` (`:1033`), `extensionRunner` (`:3549`), `state` (`:901`). `[verified]`

**A NAVI host can supply its own UI context.** `:2546-2554 → _applyExtensionBindings` → `runner.setUIContext(this._extensionUIContext, this._extensionMode)`, and `runner.ts:492-494 → hasUI()` is literally `this.uiContext !== noOpUIContext`. `[verified]` The pattern to copy is Pi's own: `src/modes/interactive/interactive-mode.ts:1859-1862` builds `createExtensionUIContext()` and calls `session.bindExtensions({ uiContext, mode: "tui" })`. `[verified]` Implication: **NAVI can make every extension dialog (approvals, questions) render in NAVI's UI, in-process and typed, by binding its own `ExtensionUIContext`.** `[inferred]`

**`mode` is a closed union.** `src/core/extensions/types.ts:307 → type ExtensionMode = "tui" | "rpc" | "json" | "print"`. `[verified]` A NAVI host must claim one of these; `"rpc"` is the honest choice for a non-terminal UI, and it is what makes `hasUI` true *and* dialogs functional (`docs/rpc.md:1205`). Extensions that gate on `ctx.mode === "tui"` will correctly skip terminal-only features. `[inferred]`

**Prompt semantics differ from RPC.** `docs/sdk.md:196-201` — `prompt()` resolves only after the full accepted run finishes; `preflightResult` fires before that. `[verified]` RPC instead answers on *acceptance* (`src/modes/rpc/rpc-mode.ts:394-416`; `docs/rpc.md:76`). `[verified]` For a UI that must not await a multi-minute turn, RPC's acceptance-response shape is the better primitive, though the SDK can achieve the same by not awaiting.

### 4. RPC: an excellent *event* channel with a deliberately narrow *control* channel and no version negotiation

**Transport.** `src/rpc-entry.ts:1-13 → main(["--mode","rpc", ...])`; `src/modes/rpc/rpc-mode.ts:54-56 → runRpcMode(runtimeHost)`. Framing is strict LF-only JSONL (`src/modes/rpc/jsonl.ts:10-12,21-41`), and the source explicitly says `readline` is **not** protocol-compliant because it splits on U+2028/U+2029 (`jsonl.ts:16-19`; `docs/rpc.md:30-37`). `[verified]` **Code/doc disagreement:** the shipped example client violates this — `examples/rpc-extension-ui.ts:521` uses `readline.createInterface({input: agent.stdout})` even though `test/rpc-jsonl.test.ts:14-30` pins the U+2028/U+2029 behavior. `[verified]` Do not copy the example's reader.

**Commands (the full control surface).** `src/modes/rpc/rpc-types.ts:20-74 → RpcCommand` is the complete list: prompt/steer/follow_up/abort/clear_queue/new_session, get_state, set_model/cycle_model/get_available_models, thinking level ×3, steering/follow-up modes, compact/set_auto_compaction, set_auto_retry/abort_retry, bash/abort_bash, get_session_stats, export_html, switch_session, fork, clone, get_fork_messages, get_entries, get_tree, get_last_assistant_text, set_session_name, get_messages, get_commands. `[verified]`

**What RPC therefore cannot do** (verified by absence from that union):
- **No individual built-in tool invocation** — there is no `read`/`edit`/`write`/`grep` command. The parent can only `prompt` the LLM, run raw shell via `bash`, or dispatch an extension/skill/prompt command. `[verified]`
- **No tool introspection** — `RpcSessionState` (`:96-109`) has model, thinkingLevel, streaming/compacting, modes, session file/id/name, autoCompaction, message counts. **No active tool list, no system prompt, no scoped models, no context-usage field**, though the same session object can produce them (`src/modes/rpc/rpc-mode.ts:450-466`). `[verified]`
- **No custom-message injection** — no equivalent of `pi.sendMessage()` / `pi.appendEntry()`; the only injected-content path is `prompt` with an extension command (`rpc-mode.ts:394-416`; the `bash` command *does* record a `BashExecutionMessage`, `docs/rpc.md:521-537`). `[verified]`
- **No import** — `AgentSessionRuntime.importFromJsonl` (`agent-session-runtime.ts:361`) is unreachable over RPC. `[verified]`
- **No settings/system-prompt/model-scope mutation**, no `executeBash` with custom `BashOperations` (extension `user_bash` can, `rpc-mode.ts:563-584`), no theme/UI-component access, no extension *loading* control. `[verified]`

**Event fidelity is better than the docs claim.** `src/modes/json-event.ts:18 → JsonAgentSessionEvent = Exclude<AgentSessionEvent, {type:"message_update"}> | JsonMessageUpdateEvent`, and `:46-61 → toJsonEvent` returns every non-`message_update` event **unchanged**. `AgentSessionEvent` (`src/core/agent-session.ts:144-185`) includes `entry_appended` and `thinking_level_changed`, which the RPC docs' event table (`docs/rpc.md:861-884`) **does not list**. `[verified]` Implication: RPC streams more state than documented (a NAVI UI can observe every persisted entry and thinking-level change), but those frames are undocumented and therefore unpinned. `[verified]`

**The one documented loss:** `message_update` strips the cumulative `partial` snapshot by design (`json-event.ts:40-45`), so clients must assemble partials from `message_start` + `contentIndex` deltas (`docs/rpc.md:991-996`). `[verified]`

**UI prompts and approvals are a real, bidirectional sub-protocol.** `src/modes/rpc/rpc-mode.ts:136-311 → createExtensionUIContext()` emits `extension_ui_request` and blocks on `extension_ui_response` keyed by a UUID; `rpc-types.ts:246-291` defines both directions; responses are matched in the input loop at `rpc-mode.ts:768-782`. `[verified]` Dialog methods auto-resolve on timeout **agent-side** (`rpc-mode.ts:91-131`; `docs/rpc.md:1193`), so the client need not track timers. `[verified]` The canonical NAVI-relevant pattern is `examples/extensions/rpc-demo.ts:46-66` — a `tool_call` handler regex-matches dangerous shell commands and returns `{block:true}` unless the user selects "Allow" via `ctx.ui.select`, which the client renders (`examples/rpc-extension-ui.ts:386-467`). `[verified]` **Note: there is no first-class "approval"/"permission" concept in the RPC protocol** — approvals are an *extension* convention layered on UI dialogs. `[verified]`

**Lifecycle: the child never exits on its own.** `rpc-mode.ts:819-820` — `// Keep process alive forever` → `return new Promise(() => {})`; shutdown comes only from stdin end (`:804-807`) or SIGTERM/SIGHUP (`:366-380`, SIGHUP registered only when `platform !== "win32"`). `[verified]` Signal handling also kills tracked detached children (`:373-375`). `[verified]`

**No handshake.** `src/modes/rpc/rpc-client.ts:94-98` spawns `node dist/cli.js --mode rpc` with pipes; `:132-139` then **sleeps a hard 100 ms** and checks liveness. `[verified]` There is no `hello`, no ready frame, and **no protocol version field anywhere in `rpc-types.ts`**. `[verified]` Crash handling is at least pinned: `:107-125` rejects pending requests on `exit`/`error`/stdin error, and `test/rpc-client-process-exit.test.ts:24-37` asserts `getCommands()` rejects with `Agent process exited (code=43 signal=null)`. `[verified]`

**Boundary tests exist and are LLM-free.** `test/rpc-jsonl.test.ts` (framing), `test/rpc-prompt-response-semantics.test.ts` (mocks the model, asserts the *acceptance* semantics of `prompt`), `test/rpc-client-process-exit.test.ts`, `test/rpc-client-clear-queue.test.ts`, `test/rpc-client-clone.test.ts`. `test/rpc.test.ts` requires real API keys (`promptAndWait("Reply with just the word 'hello'")`). `[verified]`

**Trust interaction is a deployment trap.** `docs/security.md:29` — *"Non-interactive modes (`-p`, `--mode json`, and `--mode rpc`) do not show a trust prompt. Without an applicable saved trust decision, `defaultProjectTrust: "ask"` and `"never"` ignore such resources, while `"always"` trusts them. Use `--approve`/`-a` or `--no-approve`/`-na`."* `[verified]` Project-local extensions/skills are skipped unless trust is pre-resolved (`src/core/resource-loader.ts:380-402`). `[verified]` Implication: a NAVI-spawned RPC child will silently not load project-local resources unless NAVI passes `--approve` or has written `~/.pi/agent/trust.json`. `[inferred]`

### 5. The experimental client/server harness: a working prototype of NAVI's topology, and explicitly a moving target

**It is the closest in-repo analogue to NAVI.** `src/experimental/server.ts:520-707 → startServer()` implements a replaceable server behind a stable coordinator socket, with per-session workers, plugin packages, and idle retirement; `docs/development.md:22-33` calls it *"development-only"*, gated by `PI_EXPERIMENTAL=1` (`src/core/experimental.ts:1-3`; `src/experimental/commands.ts:94`). `[verified]`

**Transport is strictly POSIX and strictly versioned.**
- `packages/protocol/src/protocol.ts:5 → PROTOCOL_VERSION = 8`; `packages/protocol/src/codec.ts:139-141 → isSupportedProtocolVersion` requires **exact** equality; `packages/server/src/server.ts:262-269 → finishHandshake` fails with `code:"version"` on mismatch. `[verified]`
- Framing is 4-byte big-endian length prefix + **CBOR** payload, default cap 16 MiB (`packages/protocol/src/framing.ts:1-6,28-39`; `codec.ts:39-63`). Not text, not JSON. `[verified]`
- `packages/client/src/unix.ts:38` and `:99` **throw `"Unix transport is not supported on Windows"`**; `packages/coding-agent/src/experimental/server.ts:58-67 → ensurePrivateServerDirectory` throws `"Unix socket directory requires a POSIX user ID"` when `process.getuid` is absent. `[verified]` Windows-only guards are skipped at `packages/server/src/transports/unix/listener.ts:365-372`. `[verified]`
- Package-level: `packages/coding-agent/src/client/index.ts:1` is a bare `export * from "@earendil-works/pi-client"`, and `pi-client`/`pi-protocol`/`pi-server` are **devDependencies** of coding-agent, not runtime deps (`packages/coding-agent/package.json:75-78`). `[verified]`

**A third-party frontend has no stable attach point.** The experimental presentation imports Pi's *internal* modules directly — `src/experimental/client-tui.ts:23-31` imports `../../core/keybindings.ts`, `../../core/resource-loader.ts`, `../../modes/interactive/chat-viewport.ts`, `.../components/custom-editor.ts`, `.../theme/theme.ts`, `.../tui-renderer.ts`. `[verified]` Those paths are not exported subpaths (`package.json:14-28`) and are excluded from the published tarball (`:29-39`). `[verified]` Implication: **"a 3rd-party UI can attach" is true only inside a checkout, at the current commit, against internal APIs.** `[verified]`

**Its own service inventory is unfinished by design.** `src/experimental/services/README.md:5-17` lists per-scope slices and, for three of them, continuation points like *"add authenticated per-client projection when identity lands"* and *"add authenticated workspace authorization"*. `[verified]` The parallel prototype `src/experimental/mini/README.md:8-11,98-102` states its purpose is to *"find out what an RPC-shaped presentation actually needs"* and that it has *"No slash-command system, extensions, skills, hooks, themes beyond the shared one, session picker, or tree navigation"*, and admits N² event fan-out and a login path with no abort. `[verified]`

**Answer to Q3:** the experimental client/server is a *design donor*, not a substrate. It is dev-only, unpublished, POSIX-only, hash-exact-versioned, and layered on internal APIs — and upstream's own changelog (`CHANGELOG.md:37`) frames its accidental publication as a bug. `[verified]`

### 6. Fork cost surface

**License is permissive.** `vendor/pi/LICENSE:1-3` — MIT, © 2025 Mario Zechner. Forking is legally unencumbered. `[verified]`

**Rebranding is a supported, narrow operation.** `docs/development.md:35-48` — *"Change `name`, `configDir`, and `bin` field for your fork. Affects CLI banner, config paths, and environment variable names."* The mechanism is `piConfig` in `package.json` (`packages/coding-agent/package.json:6-8`), read at `src/config.ts:486-505`: `APP_NAME` (`:502`), `APP_TITLE` (`:503`), `CONFIG_DIR_NAME` (`:504`), and derived env var names `ENV_AGENT_DIR`/`ENV_SESSION_DIR` (`:508-509`, e.g. `PI_CODING_AGENT_DIR`). `[verified]`

**What a fork actually inherits.** An 11-package workspace (`package.json:5-13`), a build chain that orders 10 packages by hand (`package.json:16`), per-workspace version syncing (`:41-44`, `scripts/sync-versions.js`), a shrinkwrap + install-lock generator for the shipped package (`packages/coding-agent/package.json:48-49`), generated model catalogs (`package.json:30-33`), native prebuild sets for 3 OSes (root `build:native:*`, `:18-20`), and a bundle step with 3 entries + 9 lazy provider files (`scripts/build-coding-agent-bundle.mjs:164-172`, `:184-200`). `[verified]` Release cadence is high: `CHANGELOG.md` shows 0.77.0 (2026-05-28) → 0.85.1 (2026-09-05), i.e. **9 minor lines plus patches in ~3.3 months** (`CHANGELOG.md:3,23,41,89,136,217,276,307,503,548,575,616,634,948,1211,1279,1308`). `[verified]`

### 7. Compatibility-churn evidence (what breaks on each upgrade path)

All cited from `CHANGELOG.md`, `[verified]`:

| Surface | Evidence | Implication |
|---|---|---|
| Extension API | `:21` 0.85-unreleased *"Fixed extension tools without parameter schemas to be rejected during registration"*; `:13` *"Extensions can re-register tool definitions with `constrainedSampling: false`"*; `loader.ts:57-74` maintains `@mariozechner/*` legacy aliases and routes `@earendil-works/pi-ai` → `compat` *"until compat is removed"* | Extension API is the best-shimmed surface, but it *does* tighten (schema requirement) and it carries an explicit deprecation debt |
| SDK API | `:37` 0.85.1 declares the SDK supported; `:66` 0.85.0 *"Restored the `@earendil-works/pi-coding-agent/client` compatibility entry point"* | SDK is supported but the barrel/public surface has silently shipped broken subpaths within a minor release |
| RPC protocol | `:87` 0.85.0 *"Fixed RPC `abort` reporting success without cancelling an in-progress manual compaction"*; `:18` unreleased *"Fixed direct RPC `steer` and `follow_up` commands bypassing extension `input` handlers"* | RPC has **no version field** (`rpc-types.ts`) yet its *semantics* change between patch releases; a NAVI RPC client must pin the Pi version |
| Experimental protocol | `protocol.ts:5` `PROTOCOL_VERSION = 8`; `codec.ts:139-141` exact match; `server.ts:262-269` rejects | Version skew is detected loudly — but every upgrade is a hard break, so both sides must ship together |

---

## Architectural Implications

1. **The five strategies are not one axis; they are two orthogonal ones — *where the agent session lives* and *who renders the UI*.**
   *Where*: in-process (A/B) vs out-of-process (C/D). *Who renders*: Pi's TUI (A), NAVI's own UI (B/C/D), a fork of Pi (E). Most of the confusion in the brief dissolves once these are separated: the "UI freedom" question is about the renderer, the "isolation/latency" question is about the process boundary. `[inferred]`

2. **Extensions do not give NAVI a UI surface.** Every extension rendering hook is `pi-tui`-typed (`docs/extensions.md:2246-2323`) and degrades under RPC (`rpc-mode.ts:195-311`). An extension can *request* UI and *style* rows; it cannot *be* the UI. `[verified]`→`[inferred]`

3. **SDK embedding is a strict superset of extension authority, but it is not a strict superset of extension *slots*.** The SDK can load inline extension factories (`docs/sdk.md:622-635`; `loader.ts:597-606 → loadExtensionFromFactory`) and supply the UI context (`agent-session.ts:2468`), so B *contains* A. What B does not inherit for free is Pi's *host wiring*: interactive mode's editor, session selector, theme controller, keybindings, footer, tree navigation — all `modes/interactive/*`, reachable only as internal modules or the `InteractiveMode` class (`src/index.ts:351`). `[verified]`→`[inferred]`

4. **RPC's exposure is asymmetric: rich outbound events, thin inbound control.** Events are the full `AgentSessionEvent` stream (`json-event.ts:18,46-61`); commands are 30-odd verbs with no tool-level control, no tool introspection, no import, no settings mutation, no custom-message injection (`rpc-types.ts:20-74`). For NAVI this maps well onto "perceive" (events: excellent) and adequately onto "act" (prompt + bash + extension commands + approvals via dialogs) — but it *cannot* implement "NAVI drives a specific tool with specific arguments" without going through the LLM or an in-child extension. `[verified]`→`[inferred]`

5. **Approvals are an extension convention, not a Pi feature.** There is no permission model in the RPC protocol (`rpc-types.ts:20-74`), no sandbox in Pi (`docs/security.md:31-37`), and project trust is only an input-loading guard (`:7`, `:37`). A NAVI permission system is therefore **NAVI's own code**, enforced wherever `tool_call` can be intercepted — which is only inside the Pi process, as an extension. `[verified]`→`[inferred]`

6. **The extension `tool_call` gate is fail-closed, which is the right default for a browser-adjacent agent.** `runner.ts:982-1003` propagates handler throws; `docs/extensions.md:2935` documents blocking. The corollary is unpleasant: **a NAVI gate extension is on the critical path of every tool call and shares a thread with the agent loop.** `[verified]`

7. **The experimental harness answers a different question than NAVI is asking.** It solves *multi-client attach to a durable session over a local socket, with facet-based UI plugins*. NAVI needs *browser-derived context in, approvals out, one desktop UI*. The overlap is the daemon/worker/idle pattern (already characterized as reusable in the sibling report `research/07-performance-crossplatform.md:116-127`), not the code. `[verified]`→`[inferred]`

8. **Both supported surfaces are in-process or stdio.** SDK = same process. RPC = stdin/stdout pipes. Neither requires a listening socket, so **both are Windows-safe by construction**, unlike the experimental stack (`client/src/unix.ts:38,99`). This is the single strongest reason NAVI's Pi integration should be SDK and/or stdio-RPC. `[verified]`→`[inferred]`

9. **Debugging cost tracks the number of processes and the number of undocumented frames.** A: one process, `/debug` log + `PI_TIMING=1`. B: one process, but the crash takes the UI with it. C: two processes, stderr passthrough (`rpc-client.ts:102-105,185-187`), plus undocumented `entry_appended`/`thinking_level_changed` frames. D: three, plus NAVI's own protocol on top. `[verified]`→`[inferred]`

---

## Recommended Approach

**Decision: D (hybrid), with C as the *default* transport for the agent and B as the *embedded* surface in NAVI's own agent-side process. A is used as the in-process policy/telemetry plugin layer. E is rejected.**

Concretely — NAVI has three components with three different answers, which is the direct answer to the brief's final question.

### R0. Component → method mapping

| NAVI component | Method | Why (evidence) |
|---|---|---|
| Browser extension (`navi-ext`) | **neither A/B/C** — native messaging / localhost WS to `navi-bridge` | Pi has no browser surface; Chromium/Firefox cannot open a UDS or Pi's stdio. `[inferred]`, consistent with `vendor/pi` exposing no browser entry point |
| `navi-bridge` (local daemon, idle-exits) | **NAVI-owned process**, speaking RPC to a Pi child | RPC's child never self-exits (`rpc-mode.ts:819-820`) so *someone* must own its lifetime; stdio is Windows-safe; the bridge also owns the permission policy |
| Pi agent runtime | **C — `pi --mode rpc` child over stdio JSONL**, pinned version, NAVI-owned lifetime | Declared-supported surface (`CHANGELOG.md:37`), stdio-only (Windows-safe), full event fidelity (`json-event.ts:18`), backpressure already implemented (`rpc-mode.ts:361-363,789`) |
| NAVI permission gate + browser-context injection + telemetry | **A — one TypeScript extension loaded into that child** (shipped by NAVI, loaded via `-e`) | `tool_call` is the only place to block a tool (`runner.ts:982-1003`), and it lives in the Pi process. `user_bash` gives command interception (`runner.ts:1005`), `before_provider_headers`/`before_provider_request` give request visibility (`docs/extensions.md:687-736`) |
| NAVI UI (desktop shell / browser surface) | **NAVI's own process**, RPC client; later optionally B if UI and agent must share a session object | UI freedom is total over RPC; the cost is re-implementing transcript assembly from `message_start`+deltas (`docs/rpc.md:991-996`) or folding the stream |
| Ambient/session memory (lightweight) | **Extension state via `pi.appendEntry()` + reconstruction on `session_start`** (`docs/extensions.md:1477-1493`) and/or NAVI's own store | `appendEntry` data is durable, tree-correct, and does **not** enter LLM context; it is the only persistence primitive an extension has. It is *not* reachable over RPC, which is exactly why the memory layer must live in the extension. `[verified]` |
| Long-running research/automation | second `pi --mode rpc` child with its own session, or SDK-in-sidecar | Isolation for the risky path; keep it off the interactive child |

### R1. Why C (RPC) and not B (SDK-in-UI-process) for the agent slot

- **Crash isolation is a real, tested property of C.** `test/rpc-client-process-exit.test.ts:24-37` pins that a dead child rejects in-flight requests rather than hanging a UI. B has no equivalent property and no isolation. `[verified]`
- **The RPC child is a disposable process; the UI is not.** A NAVI UI that embeds the SDK dies with the agent. `[inferred]`
- **RPC's inbound surface is thin *on purpose*.** It gives NAVI exactly the control a UI should have (prompt/steer/abort/compact/session ops/bash/approvals) and *withholds* the authority to reach into agent state — a good default for a product whose whole premise is "act under explicit permissions". `[verified]`→`[inferred]`
- **The `mode` union forbids a NAVI-native mode anyway** (`types.ts:307`), so a UI process cannot make extension semantics cleaner by embedding. `[verified]`

### R2. Why B (SDK) still belongs in the plan — as the NAVI-side process's own library

Use the SDK in NAVI's *agent-side* process (or a future NAVI sidecar) for the things RPC cannot do, all verified to exist only on the SDK:
- `AgentSessionRuntime.importFromJsonl` (`agent-session-runtime.ts:361`) — import a session from disk.
- Tool introspection and active-tool control: `agent-session.ts:939-966 → getActiveToolNames/getAllTools/setActiveToolsByName`; over RPC there is no equivalent (`rpc-types.ts:20-74`).
- Scoped models and settings mutation: `:1028 setScopedModels`, `SettingsManager.*` (`src/index.ts:258-269`).
- `session.executeBash()` with custom `operations` (`:3006`) for routing shell into a sandbox/VM — the Gondolin/ssh precedent (`docs/extensions.md:3010-3015`).
- Supplying NAVI's `ExtensionUIContext` in-process and binding `mode: "rpc"` (`agent-session.ts:2468`; `runner.ts:492-494`).

If NAVI instead chooses to keep exactly one agent process, the SDK version of the same design is: SDK host + NAVI's own `ExtensionUIContext` + inline NAVI extension factories. That is a legitimate variant of D; it trades crash isolation for ~1 IPC hop. `[inferred]`

### R3. The permission model must live in the child (extension), not the UI

Sequence, all pieces verified:
1. NAVI spawns `pi --mode rpc --no-session --no-extension -e /path/navi-gate.ts` (mirrors `examples/rpc-extension-ui.ts:259-263`). Pass `--approve` or pre-seed trust if project-local resources are wanted (`docs/security.md:29`).
2. `navi-gate.ts` registers `pi.on("tool_call", ...)`, inspects `event.input` (mutable, `docs/extensions.md:786-793`), decides policy locally for cheap cases, and for policy-relevant cases calls `ctx.ui.select/confirm` (`rpc-demo.ts:46-66`).
3. The UI receives `extension_ui_request` and answers with `extension_ui_response` (`rpc-mode.ts:136-311`, `:768-782`). Timeouts are honoured agent-side (`:115-120`).
4. `navi-gate.ts` must **fail closed** on its own errors and must never throw out of `tool_call` in a way that blocks the entire batch unintentionally (`runner.ts:998-1002` returns on `block`; throw = block for that call). If NAVI wants "deny the whole turn", it returns `{block:true, terminate:true}` and *every* finalized result in the batch must be terminating (`docs/extensions.md:792-793`). `[verified]`
5. Because `/reload` is a **builtin** command (`src/core/slash-commands.ts:41`) and builtins are excluded from RPC `get_commands` (`rpc-mode.ts:682-713`; `docs/rpc.md:853`), NAVI cannot reload its gate over RPC. It must ship a **second extension command** that calls `ctx.reload()` and invoke it via `prompt` with `expandPromptTemplates` — exactly the documented pattern at `docs/extensions.md:1337-1362`. `[verified]`

### R4. Context injection: extension hooks, not RPC prompts

NAVI's ambient page context should enter the model via the extension layer, not by prefixing user prompts:
- per-turn system-prompt chaining: `before_agent_start` returning `{systemPrompt}` (`docs/extensions.md:530-565`);
- non-destructive message shaping per LLM call: `context` (`:675-685`);
- volatile-ambient data belongs in a **suffixed** message after the cache breakpoint — the sibling report's cache analysis (`research/07-performance-crossplatform.md:208-212`) plus `CHANGELOG.md:7`'s new `ctx.modelRegistry.stream()` show the hooks exist.
This is unavailable over RPC, which is a second, independent reason the gate/injection extension must exist. `[verified]`→`[inferred]`

### R5. Version and packaging discipline

- **Pin `@earendil-works/pi-coding-agent` exactly** (no `^`) and pin the NAVI gate extension to that version. Reason: RPC semantics changed inside patch releases (`CHANGELOG.md:18,87`) and there is no in-band RPC version to detect skew. `[verified]`
- **Probe readiness with a real command, not a sleep** — replace `rpc-client.ts:132-139`'s 100 ms sleep with a `get_state` round-trip (the repo's own profiler already does exactly this: `scripts/profile-coding-agent-node.mjs` sends `{"type":"get_state"}`, per `research/07-performance-crossplatform.md:91`). `[verified]`
- **Use a strict LF-only JSONL reader** — `modes/rpc/jsonl.ts` semantics, *not* `readline` (`jsonl.ts:16-19`; the shipped example is wrong at `examples/rpc-extension-ui.ts:521`). `[verified]`
- **Ship adapters, not a fork**, if the experimental transport is ever wanted: the version/framing layer (`packages/protocol`) is small and self-contained (`protocol.ts:5-110`, `framing.ts`, `codec.ts`), but it is CBOR+POSIX-only, i.e. the wrong substrate for a browser-adjacent harness. `[verified]`→`[inferred]`

---

## Alternatives

**A1 — "Extensions only" (A): ship NAVI as one Pi extension and let the user run Pi.**
*For:* lowest maintenance; best-shimmed API (`loader.ts:57-74`); no process/lifecycle ownership; zero packaging. Gates (`tool_call`), commands, tools, memory (`appendEntry`), and context injection all work.
*Against:* no UI surface worth the name — `custom()` is TUI-only, RPC degrades it to string widgets (`rpc-mode.ts:195-311`), `renderCall/renderResult` are `pi-tui` components; NAVI's "custom UI (desktop and/or browser)" is simply not expressible (`docs/extensions.md:2938-2947`). Also: extensions run with full user permissions and no sandbox (`docs/security.md:33`).
**Verdict: adopt as the *policy/plugin layer* (R0), reject as the product's integration strategy.**

**A2 — "SDK only" (B): one NAVI process embeds the SDK, no Pi subprocess.**
*For:* widest authority (`agent-session.ts` passim), in-process latency, typed dialogs via `bindExtensions` + custom `ExtensionUIContext` (`:2468`, `interactive-mode.ts:1859-1862`), `importFromJsonl`, tool introspection, custom `ResourceLoader` (`examples/sdk/12-full-control.ts:36-49`).
*Against:* a crash, OOM, or a hung extension handler takes the UI with it (nothing in `src/core/extensions/**` isolates); NAVI must hand-maintain subscription/rebinding across every session replacement (`docs/sdk.md:163-180`; `examples/sdk/13-session-runtime.ts:38-50`); `prompt()` resolves only on completion (`docs/sdk.md:196-201`); and NAVI inherits Pi's eager module graph in its own always-on footprint (`research/07-performance-crossplatform.md:17-21`). `[verified]`→`[inferred]`
**Verdict: use for the agent-side process's own capabilities (R2); reject as the sole integration strategy.**

**A3 — "RPC only" (C): a pure RPC client with no in-child NAVI code.**
*For:* maximum isolation, total UI freedom, Windows-safe, small protocol.
*Against:* it cannot block a tool, cannot inject context, cannot read the active tool set, cannot `appendEntry`, cannot `importFromJsonl` (`rpc-types.ts:20-74`; `:96-109`). NAVI's whole permission story would degrade to post-hoc observing `tool_execution_start` and racing an `abort` — which is strictly worse than a fail-closed gate. `[verified]`
**Verdict: adopt as the *transport*, reject as the *whole* design.**

**A4 — "Experimental client/server as the NAVI frontend substrate" (the brief's Q3 candidate).**
*For:* it already implements multi-client attach, worker-per-session, event fan-out, plugin facets, and idle retirement (`src/experimental/server.ts:520-707`; `services/README.md`, `mini/README.md`).
*Against:* unpublished and source-condition-only (`package.json:22-27,29-39`); upstream calls it a bug that it was ever published (`CHANGELOG.md:37`); POSIX-only with hard throws (`client/src/unix.ts:38,99`; `experimental/server.ts:59`); binary CBOR with exact-version handshake (`protocol.ts:5`; `codec.ts:139-141`); the reference frontend imports Pi's internal modules (`client-tui.ts:23-31`); its own README lists unimplemented authorization and a deliberate feature absence list (`mini/README.md:98-102`). `[verified]`
**Verdict: reject as substrate; mine it for design (service slices, idle retirement, worker lifecycle).**

**A5 — "Fork NAVI-Pi" (E).**
*For:* MIT (`LICENSE:3`); rebranding is a 3-field `piConfig` change (`docs/development.md:35-48`; `config.ts:486-505`); lets NAVI change anything, including `ExtensionMode`, the RPC command set, or the transport.
*Against:* inherits an 11-package workspace, hand-ordered build, version sync, shrinkwrap/install-lock generation, model-catalog generation, 3-OS native prebuilds, and a 3-entry bundle with 9 lazy provider files (`package.json:5-20,30-33,41-44`; `packages/coding-agent/package.json:40-49`; `build-coding-agent-bundle.mjs:164-172,184-200`) against a ~9-minor-in-3-months upstream cadence (`CHANGELOG.md:1279`→`:23`). And **no NAVI candidate capability was found that requires it** (see Q6 below). `[verified]`
**Verdict: reject for now; revisit only if a specific upstream-only change becomes blocking.**

**A6 — "Hybrid with NAVI's own sidecar protocol instead of Pi's RPC" (D-variant).**
*For:* SDK authority + process isolation + typed IPC NAVI controls; no dependence on Pi's thin RPC command set.
*Against:* NAVI owns and versions an IPC protocol and a message-schema problem (`ExtensionUIContext` is a rich interface — `types.ts:133+` — and serializing it is real work), plus cross-process rebinding on every session replacement (`docs/sdk.md:163-180`).
**Verdict: keep as the escape hatch if RPC's thin control channel bites; do not build it first.**

**A7 — "Use `--mode json` as the transport."**
*For:* simplest possible surface (`docs/json.md:1-7`), one-shot event stream.
*Against:* single-shot, no inbound commands (`main.ts:112-121` shows it routes to print-mode output), so no steering/abort/approvals. `[verified]`
**Verdict: reject as transport; note it as a useful *fixture generator* for a NAVI transcript parser.**

---

## Risks

| # | Risk | Evidence | Mitigation |
|---|---|---|---|
| I-1 | **No RPC protocol version → silent semantic skew.** A NAVI client cannot detect it is talking to the wrong Pi. | `rpc-types.ts:20-74` (no version field); semantics changed in patches (`CHANGELOG.md:18,87`) | Pin the exact Pi version; `get_state` probe on start and after every restart; fail loudly on unexpected frames |
| I-2 | **`ExtensionBindings` is not exported from the package root**, yet `docs/sdk.md:167` tells SDK hosts to call it. | `agent-session.ts:232-239`, `:2468`; absent from `src/index.ts:1-425` | If NAVI embeds the SDK, declare the shape locally; expect upstream churn on this type |
| I-3 | **NAVI's gate extension is on the agent's critical path and shares its thread.** A hang or a bad regex stalls or blocks everything. | `runner.ts:851-883` (awaited, serial), `:982-1003` (throw ⇒ block) | Keep the gate O(1) and non-blocking; do all policy I/O off-thread-ish (async with timeouts via `ctx.ui.*` `signal`/`timeout`, `rpc-mode.ts:91-131`); treat a gate crash as "deny + notify" |
| I-4 | **Undocumented event frames drive NAVI state.** `entry_appended` and `thinking_level_changed` are streamed by `toJsonEvent` but absent from the RPC event table. | `json-event.ts:18,46-61`; `agent-session.ts:158,160`; `docs/rpc.md:861-884` | Treat them as best-effort: reconstruct durable state from `get_entries`/`get_tree` (`rpc-types.ts:65-66`) rather than from frames; assert on frame shape in tests |
| I-5 | **Trust silently disables project-local resources in the spawned child.** | `docs/security.md:29`; `resource-loader.ts:380-402` | Pass `--approve`/`-na` explicitly per NAVI policy; never rely on ambient `trust.json` |
| I-6 | **Two-lifetime coordination bugs** (bridge believes the child is alive; child is wedged). | `rpc-client.ts:132-139` = a 100 ms sleep, no handshake; `rpc-mode.ts:819-820` = intentional immortality | Readiness probe + periodic `get_state` heartbeat + a watchdog that kills and respawns on N missed beats |
| I-7 | **The shipped RPC example client is protocol-noncompliant.** Copying `examples/rpc-extension-ui.ts` propagates a `readline`-based reader that mis-splits on U+2028/U+2029. | `rpc-extension-ui.ts:521` vs `jsonl.ts:16-19`; `test/rpc-jsonl.test.ts:14-30` | Implement the LF-only reader; add the U+2028/U+2029 case to NAVI's own test suite |
| I-8 | **Extension UI in RPC is lossy by design.** No component factories, no theme access, string-only widgets. | `rpc-mode.ts:195-311`; `docs/rpc.md:1195-1205` | Design NAVI's approval/question surfaces against `select/confirm/input/editor/notify/setStatus/setWidget(string[])` only; never depend on `custom()` |
| I-9 | **No sandbox anywhere in Pi; extensions run as the user.** | `docs/security.md:31-37`; `docs/extensions.md:111` | NAVI's permission gate is the only boundary; for genuinely untrusted work, use the container/micro-VM path the repo documents (`docs/security.md:39-53`, `docs/extensions.md:3010-3015`) |
| I-10 | **The experimental stack is a moving target with hard-version breaks.** | `protocol.ts:5`; `codec.ts:139-141`; `server.ts:262-269`; `CHANGELOG.md:37` | Do not depend on it; if reused, vendor it at a pinned commit and own the upgrade |
| I-11 | **Windows parity is unverified upstream and the socket layer is Unix-only.** | `client/src/unix.ts:38,99`; `listener.ts:365-372`; `research/07-performance-crossplatform.md:84-87` (CI is ubuntu-only) | Stay on stdio (SDK/RPC) for portability; if NAVI adds its own IPC, ship and test a named-pipe transport from day one |
| I-12 | **Fork cost is dominated by tooling, not code.** 11 packages, hand-ordered build, version sync, shrinkwrap, native prebuilds. | `package.json:5-20,30-33,41-44` | Don't fork; if forced, fork *one* package (`coding-agent`) and consume the rest from npm |
| I-13 | **Session-replacement footguns are unavoidable if NAVI embeds the SDK.** Subscriptions and extension bindings must be re-established after every `newSession/switchSession/fork`. | `docs/sdk.md:163-180`; `examples/sdk/13-session-runtime.ts:38-50`; `src/core/extensions/loader.ts:209-216 → invalidate("This extension ctx is stale…")` | Centralize a single `bindSession()` helper; never hold a captured `pi`/`ctx` across replacement (`docs/extensions.md:1266-1307`) |
| I-14 | **Process count vs latency tension.** More isolation ⇒ more processes ⇒ more cold starts; Pi's RPC child has no fast-start path documented as trimmed for RPC. | `build-coding-agent-bundle.mjs:164-172` builds a dedicated `rpc-entry`, but `main.ts`'s graph is eager (`research/07-performance-crossplatform.md:17-21`) | Measure `npm run profile:rpc` before committing to the split; cache a warm child if latency dominates |

---

## Comparison table

Scale: **VL** = very low, **L** = low, **M** = medium, **H** = high, **VH** = very high. "Difficulty" rows are inverted-good (lower is better for maintainers). Ratings are for NAVI's stated use case (ambient browser-aware harness, custom UI, explicit permissions), not for a generic coding agent.

| Criterion | A — Extensions | B — SDK embedded | C — RPC child | D — Hybrid (recommended) | E — Fork |
|---|---|---|---|---|---|
| **Coupling to Pi** | H — depends on extension API + host lifecycle, nothing else | VH — imports the package; version-locked to types (`agent-session.ts` passim) | L — JSONL only; no shared code, no types required (`rpc-types.ts`) | M — RPC for control + extension API inside the child + SDK types on the NAVI side | VH — owns the entire codebase |
| **Latency (interactive turn)** | VL — in-process, no serialization | VL — in-process | L — one stdio hop + JSON parse; cold start is the real cost, no readiness handshake (`rpc-client.ts:132-139`) | VL–L — UI↔agent hop + existing browser hop; model dominates `[inferred]` | VL–L — whatever the fork keeps |
| **Memory overhead** | 1 Pi process | 1 process (+ NAVI UI runtime) | 2 processes; child is a full Node + Pi graph (`build-coding-agent-bundle.mjs:164-172`) | 3 processes (ext host 1 + bridge + UI) | as-chosen + repo tooling |
| **Access to internal state** | M — `ReadonlySessionManager`, model facade, scoped models; **no** `AgentSession`, no `agent.state` (`types.ts:309-349`) | VH — `session.agent.state` mutation, `runtime.importFromJsonl`, settings/model runtime, resource loader (`agent-session.ts:901`; `agent-session-runtime.ts:361`) | L — `get_state`/`get_messages`/`get_entries`/`get_tree`/stats; **no** tools list, no system prompt, no settings, no import (`rpc-types.ts:20-109`) | VH for the agent process, L for the UI process | unbounded |
| **Extensibility (add capabilities)** | H — tools, commands, shortcuts, flags, providers, compaction, renderers, 30+ events | VH — everything A can do *plus* inline factories, custom `ResourceLoader`, custom UI context, tool-set control (`sdk.md:622-635`) | L–M — capabilities must be baked into the child as extension files; the protocol itself is closed (`rpc-types.ts:20-74`) | VH — RPC for control, extension API for in-child authority, SDK for the rest | VH |
| **UI freedom** | VL — `pi-tui` components; `custom()` is TUI-only and RPC-degraded (`rpc-mode.ts:228-231,195-208`) | VH — NAVI owns the window/process; must implement `ExtensionUIContext` itself (`types.ts:133`) | VH — any renderer in any language; the protocol is text (`jsonl.ts`) | VH | VH |
| **Crash isolation** | VL — same thread; handler errors caught per-event but `tool_call` throws propagate (`runner.ts:851-883,982-1003`) | VL — a crash takes the UI with it | H — separate process; pending requests rejected on exit and *pinned by test* (`test/rpc-client-process-exit.test.ts:24-37`) | H (agent isolated from UI, gate still in-process with the agent) | VL unless the fork adds it |
| **Update compatibility** | H — best-shimmed surface (`loader.ts:57-74`), but tightens (schema requirement, `CHANGELOG.md:21`) | M–H — declared supported (`CHANGELOG.md:37`) but public subpaths have broken within a minor (`:66`) | M — no version field; semantics changed in patches (`:18,87`) | H if pinned exactly + probed; M if the experimental stack is involved (hard version breaks, `codec.ts:139-141`) | VL — NAVI owns all upstream churn |
| **Debugging difficulty** (lower better) | VL — one process, `/debug`, `PI_TIMING=1`, `profile:rpc` | L — one process, but UI-vs-agent crashes are indistinguishable | M — two processes, stderr passthrough, undocumented frames (`jsonl.ts`; `json-event.ts:18`) | H — three processes plus NAVI's own protocol | VH — plus merge burden |
| **Windows behavior** | OK — no platform-specific dependency in the extension path | OK — Node/Electron/Tauri sidecar; no sockets needed | Best — stdio pipes only; SIGHUP not registered on win32 (`rpc-mode.ts:368-370`) | OK — as long as NAVI's own IPC is not UDS (Pi's is POSIX-only: `client/src/unix.ts:38,99`) | as-chosen |
| **Linux behavior** | OK | OK | OK | OK — may optionally use UDS | as-chosen |
| **Maintenance burden** (lower better) | VL — ship `.ts` files, no build | M — track SDK types + own lifecycle/rebinding (`docs/sdk.md:163-180`) | L — small frozen-ish protocol; pin + probe | M–H — three components, one gate extension, one client | VH — 11 packages, ordered build, version sync, native prebuilds, ~9 minors/3 months (`package.json:16,41-44`; `CHANGELOG.md:1279`→`:23`) |

**Reading the table.** No single column dominates. A wins maintenance and loses UI. B wins authority and loses isolation. C wins isolation and portability and loses authority. E loses on every axis except "can change upstream behaviour", which no required capability needs. D is the only column that is not disqualifying on any axis NAVI actually requires — at the cost of being the most moving parts.

---

## Feature Assessment

| Feature / decision | Verdict | Rationale (one line) |
|---|---|---|
| NAVI permission gate as an in-child Pi extension on `tool_call` | **CORE** | The only place a tool can be blocked, and the only fail-closed hook (`runner.ts:982-1003`; `docs/extensions.md:2935`). |
| Agent runtime as `pi --mode rpc` over stdio JSONL | **CORE** | Declared-supported surface (`CHANGELOG.md:37`), Windows-safe, full event fidelity, backpressure already implemented (`json-event.ts:18`; `rpc-mode.ts:361-363`). |
| Approvals via `extension_ui_request` / `extension_ui_response` | **CORE** | The only bidirectionally-tested UI path over RPC, with agent-side timeouts (`rpc-mode.ts:136-311`; `rpc-demo.ts:46-66`). |
| LF-only JSONL reader in NAVI's client | **CORE** | `readline` is explicitly non-compliant (`jsonl.ts:16-19`) and the shipped example gets this wrong (`rpc-extension-ui.ts:521`). |
| Readiness probe (`get_state`) instead of a sleep | **CORE** | The repo's own client just sleeps 100 ms (`rpc-client.ts:132-139`); NAVI must not inherit that race. |
| Exact version pinning of `@earendil-works/pi-coding-agent` | **CORE** | No in-band RPC version; semantics changed within patches (`CHANGELOG.md:18,87`). |
| Ambient context injected via `before_agent_start` / `context`, not user-prompt prefixing | **CORE** | Per-turn system-prompt chaining and non-destructive message shaping exist only as extension hooks (`docs/extensions.md:530-565,675-685`). |
| Lightweight memory via `pi.appendEntry()` + reconstruct on `session_start` | **CORE** | Durable, tree-correct, excluded from LLM context, and **unreachable over RPC** (`docs/extensions.md:1477-1493`). |
| A NAVI extension command wrapping `ctx.reload()` for hot-swapping the gate | **CORE** | `/reload` is builtin and therefore invisible to RPC `get_commands` (`slash-commands.ts:41`; `rpc-mode.ts:682-713`). |
| SDK embedded in NAVI's own agent-side process | **LIKELY** | Needed for `importFromJsonl`, tool introspection, scoped models, custom `BashOperations` (`agent-session-runtime.ts:361`; `agent-session.ts:939-966,3006`). |
| NAVI-side `ExtensionUIContext` implementation (if SDK is embedded) | **LIKELY** | Makes extension dialogs render in NAVI's UI in-process and typed (`agent-session.ts:2468`; `runner.ts:492-494`). |
| Second, isolated RPC child for long-running research/automation | **LIKELY** | Isolation for the unbounded path; keeps the interactive child clean. |
| Named-pipe/IPC transport for NAVI's own bridge→UI hop | **LIKELY** | Pi's socket stack is POSIX-only, so parity is NAVI's problem, not Pi's (`client/src/unix.ts:38,99`). |
| Wrapping risky shell via an extension `user_bash` `BashOperations` (sandbox/VM/SSH) | **LIKELY** | Documented and precedent-rich (`docs/extensions.md:2119-2174,3010-3015`), but needs a concrete sandbox target before it is core. |
| Reusing the experimental service-slice design (idle retirement, worker-per-session) as a *pattern* | **LIKELY** | Validated in-repo (`src/experimental/server.ts:244-321`), but only the idea is portable, not the code. |
| Tool-level control over RPC (invoke `read`/`write` with arguments) | **EXPERIMENTAL** | Not in the protocol (`rpc-types.ts:20-74`); achievable only by adding an extension tool and prompting the LLM to call it. |
| `importFromJsonl` over RPC | **EXPERIMENTAL** | Exists on the runtime only (`agent-session-runtime.ts:361`); would need an extension command wrapper. |
| Adopting `packages/protocol` CBOR framing for NAVI's own IPC | **EXPERIMENTAL** | Small and typed, but exact-versioned and POSIX-coupled (`codec.ts:139-141`; `unix.ts:38`). |
| Observing `entry_appended` / `thinking_level_changed` frames for UI state | **EXPERIMENTAL** | They are emitted (`json-event.ts:18`) but undocumented and therefore unpinned (`docs/rpc.md:861-884`). |
| `--mode json` as a NAVI transport | **DEFER** | One-shot, no inbound commands (`docs/json.md:1-7`); useful only as a transcript-fixture generator. |
| NAVI-specific `ExtensionMode` value (e.g. `"navi"`) | **DEFER** | Requires either an upstream change or a fork (`types.ts:307`); `"rpc"` already yields `hasUI: true` with working dialogs. |
| Upstreaming a Windows transport / cross-platform `pi-server` | **DEFER** | Valuable long-term, but the experimental stack is unpublished and version-fragile (`CHANGELOG.md:37`). |
| NAVI as *only* a Pi extension (no own UI process) | **REJECT** | Extension rendering is `pi-tui`-typed and RPC-degraded; NAVI's custom UI is not expressible (`docs/extensions.md:2938-2947`). |
| NAVI as a pure RPC observer with no in-child extension | **REJECT** | Loses all pre-execution blocking and context injection (`rpc-types.ts:20-74`). |
| Embedding the experimental client/server as NAVI's frontend substrate | **REJECT** | Unpublished, dev-only, POSIX-only, exact-version, internal-API-coupled (`package.json:22-27`; `unix.ts:38,99`; `client-tui.ts:23-31`). |
| Depending on Pi's TUI components for a non-terminal NAVI UI | **REJECT** | They are terminal renderers (`packages/tui`); a desktop/web surface cannot use them. |
| Relying on ambient `~/.pi/agent/trust.json` for a NAVI-spawned child | **REJECT** | Non-interactive modes do not prompt, so project-local resources are silently skipped (`docs/security.md:29`). |
| Forking Pi (E) for any currently identified NAVI capability | **REJECT** | MIT makes it legal, but every capability found so far is reachable via A/B/C/D, and the fork inherits an 11-package toolchain against a ~9-minor/3-month cadence (`package.json:5-20,41-44`). |

---

## Open Questions

1. **Does NAVI's UI need to *drive individual tools*, or only approve/observe LLM-chosen tools?** The answer decides whether the SDK sidecar (A6) is needed at all, or whether RPC-plus-extension is sufficient (`rpc-types.ts:20-74`). `[inferred]`
2. **Can the NAVI gate extension stay O(1) and non-blocking?** It sits on the awaited critical path of every tool call (`runner.ts:982-1003`). No in-repo evidence says what a realistic worst-case handler latency is. `[unmeasured]`
3. **What is Pi's real RPC cold-start and `get_state` latency on Windows and Linux?** NAVI's whole process model rests on this. The tooling exists (`npm run profile:rpc`, `scripts/profile-coding-agent-node.mjs`); it has not been run. **Highest-value first measurement.** `[unmeasured]`
4. **Is `ExtensionBindings` going to be exported, and is `bindExtensions` a supported entry point?** `docs/sdk.md:167` says use it; `src/index.ts` does not type it. `[verified]` gap, `[inferred]` risk.
5. **Will upstream add an RPC protocol version or a readiness frame?** Absent today; if added, NAVI's I-1 and I-6 risks largely disappear (`rpc-types.ts`; `rpc-client.ts:132-139`). `[inferred]`
6. **Should NAVI write `trust.json` / pass `--approve` by default, and for which directories?** This is a policy decision with a security consequence, not an engineering one (`docs/security.md:29`). `[inferred]`
7. **Is the `entry_appended` frame safe to depend on?** It carries exactly the durable state a NAVI UI wants, but it is undocumented (`agent-session.ts:158`; `docs/rpc.md:861-884`). `[inferred]`
8. **At what point does the thin RPC control channel force A6 (NAVI's own agent-side protocol)?** Needs a concrete NAVI capability list, not a guess. `[inferred]`
9. **Does NAVI need session persistence at all, or is `--no-session` plus NAVI's own store the better default?** `appendEntry`-based memory implies persistence; `--no-session` in the examples implies not (`examples/rpc-extension-ui.ts:261`). `[inferred]`
10. **What is the fork trigger checklist?** NAVI should write one now (e.g. "upstream refuses a Windows transport AND NAVI must run multi-client attach"), so E is a decision, not a drift. `[inferred]`

---

## Direct answers to the six questions

1. **Extensions cannot do, that SDK embedding can:** name/own the `AgentSession` (mutate `agent.state`, replace tools/messages); call `AgentSessionRuntime.importFromJsonl`; control the active tool set (`getAllTools`/`setActiveToolsByName`); own `SettingsManager`/`ModelRuntime` mutation and `scopedModels`; supply the `ResourceLoader`; host their own event loop and UI toolkit; choose process lifetime. Conversely, **SDK embedding does not automatically get** extension *slots* that Pi's host wires up (tool renderers, entry/message renderers, shortcuts, flags, `project_trust`, `ui_prompt_*`, extension commands) nor the TUI's editor/footer/theme/keybinding machinery — those are host-side (`modes/interactive/*`) and must be supplied by the embedder. `[verified]`

2. **RPC exposes / hides:** exposes the full `AgentSessionEvent` stream including two undocumented frames, all persisted entries and the tree, session stats, model + thinking control, compaction/retry control, raw shell, session replacement (new/switch/fork/clone), and a bidirectional dialog protocol for approvals; hides tool introspection, system prompt, scoped models, settings, `importFromJsonl`, custom-message/`appendEntry` injection, per-tool invocation, and all TUI/theme/component access. `[verified]`

3. **The experimental client/server is a moving target, not a substrate.** Unpublished and source-condition-only (`package.json:22-27,29-39`), described by upstream as accidentally published (`CHANGELOG.md:37`), POSIX-only with hard throws, exact-version CBOR handshake, and its reference frontend imports non-exported internal modules. Mine the pattern; don't build on it. `[verified]`

4. **What breaks on each upgrade path:** extension API — best shimmed (`loader.ts:57-74`) but tightens over time (`CHANGELOG.md:21`); SDK — declared supported but its public subpaths have already broken inside a minor (`:66`) and one documented type is unexported; RPC — no version field at all, yet semantics (abort, steer/follow_up input handling) changed within patches (`:18,87`); experimental protocol — hard `PROTOCOL_VERSION = 8` exact match, detected loudly but breaking on every bump. `[verified]`

5. **Per-strategy Windows/Linux deployment:** A — Pi's own process, user-installed CLI; no NAVI process, no transport, NAVI ships `.ts`+`package.json` extensions; works on both but has no UI. B — one NAVI app process (Electron/Tauri + Node sidecar) embedding the SDK; no sockets; identical on both OSes; packaging concerns are NAVI's runtime, not Pi's. C — NAVI app + one `pi --mode rpc` child over stdio pipes; identical on both OSes; Windows caveat: no SIGHUP (`rpc-mode.ts:368-370`), so termination is SIGTERM/`process.kill`. D — adds NAVI's own bridge process and IPC; on Linux a UDS is fine, on Windows it must be a named pipe because Pi's own stack throws there (`client/src/unix.ts:38,99`). E — anything you want, at the cost of maintaining the whole toolchain. `[verified]`+`[inferred]`

6. **No evidence that a fork is necessary.** Every NAVI capability examined is reachable: gate/approvals (`runner.ts:982-1003` + `rpc-mode.ts:136-311`), ambient context injection (`docs/extensions.md:530-565,675-685`), lightweight memory (`appendEntry`, `:1477-1493`), custom UI (any process, RPC), custom tools and providers (`docs/extensions.md:1371-1420,1744-1884`), remote/sandboxed execution (`:2119-2174`), custom compaction and tree navigation (`:452-514`), `importFromJsonl` and tool introspection (SDK only). The only fork-grade wants are cosmetic (a `"navi"` `ExtensionMode`) or upstream-roadmap (cross-platform `pi-server`), and neither is required by a NAVI component identified here. `[verified]`
