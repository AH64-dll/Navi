# 01 — Pi Core Architecture Map (Agent 1 of 10, NAVI design research)

Scope: runtime core of `C:\Users\lain\NAVI\vendor\pi` (read-only clone, branch `main`, commit `71dca871`, `@earendil-works/pi-coding-agent@0.85.1`).
Method: read `src`, not only `docs`; where docs and code disagree, code wins and is called out. Every claim carries `path:line → symbol → implication` and a `[verified]` / `[inferred]` tag.
Assumptions: no network use; no writes inside `vendor/pi`; line numbers are true file lines (grep/`read_file` numbering).

Snapshot facts and discrepancies checked first:
- `packages/coding-agent/src/core/agent-session.ts` is **3,551 lines** (3,179 non-blank; the brief's "3179 lines" is the non-blank count) `[verified]`.
- `docs/sdk.md` documents `SessionManager.getPath()`; that method does not exist. The real method is `getBranch(fromId?)` — `packages/coding-agent/src/core/session-manager.ts:1274` → `getBranch` → walk leaf→root `[verified]` (code wins).
- `docs/compaction.md` prints `CompactionEntry.timestamp: number`; entries use an ISO string (`session-manager.ts:46-51` → `SessionEntryBase.timestamp: string`), while a message's own `timestamp` is a number `[verified]` (code wins).
- `packages/coding-agent` never imports the harness runtime (`packages/agent/src/harness/*`); the harness is used by `server`, `client`, `session-backends/sqlite-node`, `evals`, and agent's own tests `[verified]` via repo-wide grep for `AgentHarness` / `pi-agent-core/harness`.

---

## Repository Findings

### 1. What AgentSession actually is [verified]

Construction and injected services:
- `agent-session.ts:306-406` → `class AgentSession` / `constructor(config)` → it is a **facade around one `Agent` plus cwd-bound services**, not a process or loop owner.
- `agent-session.ts:200-230` → `AgentSessionConfig` → required injections: `agent`, `sessionManager`, `settingsManager`, `cwd`, `resourceLoader`, `modelRuntime`; optional `customTools`, tool allow/deny lists, `baseToolsOverride`, `extensionRunnerRef`.
- `agent-session.ts:398-405` → `this.agent.subscribe(this._handleAgentEvent)` then `_installAgentToolHooks()`, `_installAgentNextTurnRefresh()`, `_buildRuntime()` → **AgentSession cannot exist before the Agent**, and it is the only place that persists, compacts and bridges extensions.
- `agent-session.ts:2787-2839` → `_buildRuntime()` → builds built-in tool definitions per `cwd` (default active: `read, bash, edit, write`, line 2831-2833), instantiates `ExtensionRunner` from `resourceLoader.getExtensions()` (2818-2824), wires `bindCore` (2828) and extension bindings (2829), then refreshes the tool registry.
- `agent-session.ts:1633-1635`, `2694-2785` → `resourceLoader` getter / `_refreshToolRegistry()` → the tool registry is a merge of built-in definitions and extension/SDK tools, filtered by allow/deny lists, with `promptSnippet`/`promptGuidelines` feeding the system prompt.

Lifecycle states (all derived, no state machine):
- `agent-session.ts:916-923` → `isStreaming` (= `_isAgentRunActive`), `isIdle` (= `!_isAgentRunActive && !isCompacting`) → "idle" is a composite of run + compaction.
- `agent-session.ts:984-990` → `isCompacting` → true while any of manual/auto/branch-summary abort controllers is set.
- `agent-session.ts:1101-1114` → `_runAgentPrompt()` → sets `_isAgentRunActive = true`, awaits `agent.prompt(messages)`, then loops `_handlePostAgentRun()` → `agent.continue()` (retry / compaction-continue / queued messages); `finally` clears the per-turn system-prompt override, flushes pending bash/custom messages, and emits `agent_settled`.
- `agent-session.ts:625-633` → `_emitAgentSettled()` → `agent_settled` is the "no further automatic continuation" signal; `waitForIdle()` (`1648-1653`) resolves there.
- `agent-session.ts:877-894` → `dispose()` → aborts retry/compaction/branch-summary/bash/agent, invalidates the extension runner, unsubscribes, `cleanupSessionResources(sessionId)` → disposal is explicit; there is no idle GC.

Loop responsibility split (the important architectural seam):
- `packages/agent/src/agent.ts:173-260` → `class Agent` → owns transcript (`_state.messages`), one `activeRun` with its own `AbortController`, steering/follow-up queues, and event fan-out. `prompt()` throws if a run is active (`335-345`), so **one run at a time per Agent**.
- `packages/agent/src/agent-loop.ts:96-155` → `runAgentLoop` → emits `agent_start`/`turn_start`, then `runLoop` (`156-277`) → outer loop for follow-ups, inner loop for tool calls + steering; emits `turn_end`, then `agent_end`.
- `packages/agent/src/agent-loop.ts:279-296` → `streamAssistantResponse` → **the only place `AgentMessage[]` becomes provider `Message[]`** (`config.convertToLlm`), after optional `config.transformContext`. Provider-specific detail (auth, headers, payload) lives one level down behind `streamFn`.
- `agent-session.ts` adds what the loop deliberately lacks: prompt preflight (extension-command dispatch, skill/template expansion, queueing while streaming, model+auth validation) `1175-1326`; persistence on `message_end` `669-686`; auto-compaction/retry orchestration `1116-1144`, `2154-2454`; extension event bridging `765-846`; bash `3006-3105`; model/thinking management `1679-1914`; tree navigation `3136-3332`.
- `agent-session.ts:1320-1325` → `prompt()` awaits `_runAgentPrompt(messages)` → **`prompt()` resolves only after the accepted run (including retries/compaction) finishes**; acceptance is reported separately via `PromptOptions.preflightResult` (`251-252`, called at `1187/1206/1230/1324`).

### 2. Event flow, sync vs async, and what a listener may mutate [verified unless noted]

Three independent channels exist; they must not be conflated:
1. **Agent listeners** — `agent.ts:544-590` → `processEvents()` → after reducing state, `for (const listener of this.listeners) await listener(event, signal)` → listeners are **awaited in subscription order** and the run is not idle until they settle (`agent.ts:243` doc; `finishRun()` at `528-537`). A slow listener **delays the loop**; each listener receives the run's `AbortSignal`.
2. **AgentSession listeners** — `agent-session.ts:586-590` → `_emit()` → plain synchronous `for (...) l(event)` with `void` return → fire-and-forget: cannot block, cannot mutate loop behavior, exceptions are the listener's problem.
3. **Extension event bus** — `event-bus.ts` → `createEventBus()` → Node `EventEmitter` wrapper; handlers are wrapped in an async closure that try/catches and `console.error`s (`safeHandler`) → publication is async-but-unawaited and errors are swallowed; used for cross-extension/out-of-process signalling (`pi.events`), not for control flow.

Ordering inside a run: `agent-session.ts:639-719` → `_handleAgentEvent` → (a) queue bookkeeping for user messages, (b) `await this._emitExtensionEvent(event)` (b), (c) synchronous `_emit` to session listeners, (d) persistence on `message_end`, (e) `_flushPendingCustomMessages()` on `turn_end`. Extension handlers therefore always observe state **before** session listeners (which matches `docs/extensions.md:782`: tool_call waits for prior Agent events to drain).

What a hook/listener can actually change (each wiring verified in `sdk.ts`):
- Ephemeral LLM context: `context` event → `runner.ts:1034-1064` `emitContext` deep-clones with `structuredClone` and returns replacement messages → wired as Agent `transformContext` (`sdk.ts:362-366`), called once per request (`agent-loop.ts:288-296`) → **non-persistent, per-request context injection**, the cleanest ambient channel.
- Provider payload: `before_provider_request` → Agent `onPayload` (`sdk.ts:343-349`; `runner.ts:1066-1098`) → payload replacement; `before_provider_headers` mutates the header object in place (`sdk.ts:334-341`; `runner.ts:1100-1129`); `after_provider_response` is notification only (`sdk.ts:350-360`).
- Tool arguments: `tool_call` → Agent `beforeToolCall` (`agent-session.ts:483-502`; `agent-loop.ts:626-660`) → `event.input` is mutable in place, return `{block,reason,terminate?}` to stop the call; docs note no re-validation after mutation (`docs/extensions.md:786-793`).
- Tool results: `tool_result` → Agent `afterToolCall` (`agent-session.ts:504-535`; `agent-loop.ts:731-760`) → partial patch of `{content, details, isError, usage}`, chained across extensions (`runner.ts:927-980`), followed by image normalization (`agent-session.ts:521-523`).
- Finalized messages: `message_end` → `emitMessageEnd` may return a replacement of the same role (`runner.ts:885-925`); AgentSession applies it by **mutating the message object in place** so agent state, later events and the eventual `appendMessage` all stay consistent (`agent-session.ts:805-818`, `748-762`).
- System prompt per turn: `before_agent_start` returns `{message?, systemPrompt?}` (`runner.ts:1131-1180`); AgentSession appends returned messages before the user message and stores the override, which the next-turn refresh re-applies (`agent-session.ts:1285-1314`, `557-579`) and clears after the run (`1109`).
- Input: `input` event can `transform` or fully `handled` a prompt before skill/template expansion (`agent-session.ts:1146-1164`, `1199-1216`; `docs/extensions.md:915-957`).
- Trigger a turn from code: `sendCustomMessage(..., {triggerTurn:true})` → `_runAgentPrompt(appMessage)` (`agent-session.ts:1524-1525`); `deliverAs:"nextTurn"` parks it in `_pendingNextTurnMessages` (`1516-1517`) and injects it alongside the next user message (`1280-1283`).
- UI prompt notifications (`ui_prompt_start/end`) are emitted via `queueMicrotask` and explicitly not awaited (`runner.ts:482-486`).

### 3. Message/history representation and branching [verified]

- Message union: `messages.ts` → `declare module "@earendil-works/pi-agent-core" { interface CustomAgentMessages { bashExecution; custom; branchSummary; compactionSummary } }` → pi extends the base `user|assistant|toolResult` union by declaration merging, i.e. **core stays provider-agnostic and coding-agent-specific roles are injected at compile time**.
- `messages.ts` → `convertToLlm()` → maps `bashExecution` → user text (`Ran \`cmd\``, skippable when `excludeFromContext`), `custom` → user, `branchSummary`/`compactionSummary` → user with `<summary>`-wrapped prefixes; `user/assistant/toolResult` pass through. `sdk.ts:305-341` wraps it to strip images when `blockImages` is on → **one transformation point for everything the model sees**.
- Session entries: `session-manager.ts:32-153` → header (`session`, v3, id, timestamp, cwd, optional `parentSession`) + `message`, `thinking_level_change`, `model_change`, `compaction`, `branch_summary`, `custom` (state only, **not** in LLM context), `custom_message` (in LLM context), `label`, `session_info`.
- History assembly: `session-manager.ts:334-470` → `buildSessionPath` (leaf→root via `parentId`) + `getSessionContextSettings` (last model/thinking on the path) + `buildContextEntries` (compaction-aware: latest compaction entry, then entries from `firstKeptEntryId` up to it, then everything after) + `sessionEntryToContextMessages` → `buildSessionContext()`. AgentSession then **replaces `agent.state.messages` wholesale** with that result after compaction (`agent-session.ts:2056`, `2382`) and after tree navigation (`3313-3314`).
- Branch semantics: leaf pointer only — `appendX` makes a child of the leaf (`1058-1063`), `branch(id)` / `resetLeaf()` / `branchWithSummary()` move or reset the leaf without touching existing entries (`1374-1420`). `createBranchedSession(leafId)` writes a new file containing just that path, re-chaining `parentId`s and remapping label ids and `firstKeptEntryId` (`1427-1544`).
- Cherry-picking context across branches happens in exactly two ways: (a) the compaction window above, and (b) a `branch_summary` entry attached at the navigation point, which is a real entry on the new path and therefore reaches the LLM as a user-role summary.
- UI-level navigation rules: `agent-session.ts:3264-3310` → selecting a user/custom message sets the leaf to its **parent** and returns the text for re-editing; any other entry becomes the leaf; summary (when requested) is created **at the target position**, not on the abandoned branch.
- Fork/clone live on the runtime, not the session: `agent-session-runtime.ts:270-345` → `fork(entryId,{position})` → `createBranchedSession`; clone == `fork(entryId,{position:"at"})`.

### 4. Session persistence [verified]

- Format/location: append-only JSONL (one JSON object per line, header first), `~/.pi/agent/sessions/--<cwd with `/`,`\`,`:` replaced by `-`>--/<ISO timestamp with `:`/`.` replaced by `-`>_<uuidv7>.jsonl` — `session-manager.ts:476-489`, `947-952`; `config.ts:504` (`CONFIG_DIR_NAME = ".pi"`), `config.ts:528` (`getAgentDir`), `config.ts:572` (`getSessionsDir`). `ENV_SESSION_DIR`/`<APP>_CODING_AGENT_SESSION_DIR` overrides the dir (`config.ts:509`; `main.ts:671-675`).
- Versions/migrations: `session-manager.ts:30` → `CURRENT_SESSION_VERSION = 3`; v1→v2 adds `id`/`parentId` and converts `firstKeptEntryIndex`→`firstKeptEntryId` (`231-257`), v2→v3 renames `hookMessage`→`custom` (`259-275`); migration **rewrites the file in place** (`961-963`, `993-1003`).
- What is written when: `appendMessage` on `message_end` for user/assistant/toolResult (`agent-session.ts:669-686`); `custom` → `appendCustomMessageEntry` (`673-678`); `bashExecution` on record/flush (`3066`, `3101`); `model_change`/`thinking_level_change` on mutation (`1687`, `1829`); compaction at `2053`/`2379`; branch summary at `3286`; labels/name on demand; extension state via `pi.appendEntry` (`2616-2622`).
- Write policy: `session-manager.ts:1029-1056` → `_persist()` refuses to create the file until the session contains an assistant message; the first write creates the whole file with `openSync(..., "wx")`, later entries are `appendFileSync`. Implication: **an idle session with no assistant reply leaves no file on disk**, and a mid-session crash can only lose the tail.
- Locking: **none for session files.** `proper-lockfile` appears only for `auth.json`/settings/models store/trust/package-manager and the experimental server/worker (`package-manager-cli.ts:160,178`; `trust-manager.ts:146`; `experimental/server.ts:113,201`; `experimental/session-worker.ts:533`). Two processes pointing at one session file can interleave appends with no protection `[verified absence]`.
- cwd handling: header `cwd` is authoritative; `session-cwd.ts:54` → `assertSessionCwdExists()`; `main.ts:677-689` → interactive offers a fallback cwd, non-interactive exits; a session resolved from another project is **forked** rather than opened (`main.ts:394-402`). All cwd-bound services (settings, model runtime, resource loader) are recreated per effective cwd by the runtime factory (`agent-session-runtime.ts:167-183`, `createRuntime` closure in `main.ts:713-839`).

### 5. Compaction: triggers, algorithm, hooks [verified]

- Trigger rule: `compaction.ts:235-238` → `shouldCompact()` = `contextTokens > contextWindow - reserveTokens`; defaults `enabled:true, reserveTokens:16384, keepRecentTokens:20000` (`compaction.ts:132-136`), per-model overrides resolved in `settings-manager.ts:891`.
- Three automatic entry points, all funnelling into `_runAutoCompaction`: (a) inside a run, between turns, via `prepareNextTurnWithContext` → `_compactBeforeNextAssistantResponse` (`agent-session.ts:538-555`, installed at `557-579`); (b) after a run ends → `_checkCompaction` distinguishes **overflow/length recovery** from **threshold** (`2154-2258`); (c) before a new prompt, using the last assistant message (`1260-1263`). Manual `/compact` → `compact()` (`1967-2116`).
- Overflow recovery: one compact-and-retry per run — the failed assistant message is removed from agent state (`2218-2223`), guarded by `_overflowRecoveryAttempted` (`332`, `2194-2214`).
- Algorithm: `compaction.ts:403-...` `findCutPoint` / `750-...` `prepareCompaction` → walk backwards accumulating `estimateTokens` until `keepRecentTokens`, valid cut points exclude tool results, split turns produce a history summary + turn-prefix summary; `tokensBefore` is recalculated from the rebuilt context; summaries use the structured Goal/Progress/Critical-Context format and cumulatively track read/modified files (docs/compaction.md; code: `estimateTokens:266`, `findTurnStartIndex:369`).
- Extension hooks: `session_before_compact` (cancel or supply the whole compaction result) `agent-session.ts:1996-2015` (manual) and `2296-2328` (auto); `session_compact` on success `2064-2072` / `2390-2398`; `session_compact_failed` `600-604` + `2105-2111` / `2441-2447`. Branch-summary equivalents: `session_before_tree` / `session_tree` (`3197-3223`, `3317-3323`).
- Retry policy for summarization requests is the same settings budget as turn retries, surfaced as `summarization_retry_*` events (`2888-2911`).
- **Nothing NAVI-specific must be invented here.** Do not re-implement: threshold math, cut-point rules, summary schema, `firstKeptEntryId` semantics, overflow recovery, or summarization retries. If NAVI needs different summarization, use `session_before_compact` and return `{compaction}`; if NAVI needs its own memory, store it outside pi (see §4 locking) and project it in.

### 6. ResourceLoader: kinds, discovery order, prompt injection [verified]

- Interface: `resource-loader.ts:43-57` → `getExtensions/getSkills/getPrompts/getThemes/getAgentsFiles/getSystemPrompt/getSystemPromptSource/getAppendSystemPrompt/extendResources/reload` — five resource kinds plus two prompt sources.
- Kinds and file patterns: `package-manager.ts:202-211` → extensions `.ts|.js`, skills `.md` (or `SKILL.md` inside a directory, `mapSkillPath`), prompts `.md`, themes `.json`; context files `AGENTS.md|AGENTS.override.md|CLAUDE.md` walking up from cwd plus the agent dir (`resource-loader.ts:loadProjectContextFiles`, `loadContextFileFromDir`); `SYSTEM.md` / `APPEND_SYSTEM.md` (`discoverSystemPromptFile`, `discoverAppendSystemPromptFile`, project version gated on trust).
- Discovery order / precedence: `package-manager.ts:176-192` documents and implements a rank: 0 project+settings entry, 1 project+auto-discovered, 2 user+settings entry, 3 user+auto-discovered, 4 package resource — sorted, then **first wins** on name collisions; `.gitignore`/`.ignore`/`.fdignore` are honoured (`213`, `255-273`). Auto-discovery dirs: `<agentDir>/{extensions,skills,prompts,themes}` and `<cwd>/.pi/{...}` plus `~/.agents/skills` and trusted ancestor `.agents/skills` (`2352-2401`). Extension-contributed paths are merged post-`session_start` via `extendResources` (`agent-session.ts:2493-2516`).
- Eager vs lazy: `agent-session-services.ts:135-176` → `createAgentSessionServices()` loads settings, creates `ModelRuntime`, then `await resourceLoader.reload(...)` **before any session exists**; extension factories run (and are awaited) at load time; extension-registered providers are flushed into the model runtime (`156-176`). So **everything is discovered eagerly at process start**; only skill *content* and tool *results* are fetched on demand.
- System prompt vs on demand: `system-prompt.ts:buildSystemPrompt()` injects tool list (only tools that declare a `promptSnippet`), guideline bullets (including per-tool `promptGuidelines`), `<project_context>` with `<project_instructions path=...>`, the custom prompt (SYSTEM.md / `--system-prompt`), the append prompt, `formatSkillsForPrompt` (skill name/description/location only), and the cwd. Skill bodies are read when `/skill:name` is expanded (`agent-session.ts:1362-1386` reads the file, strips frontmatter, wraps in `<skill ...>`), or when the model reads the file with a tool. The prompt is rebuilt on tool-set change (`1061-1095`) and re-assembled every turn from `_baseSystemPrompt` + per-turn override (`557-579`).

### 7. Startup / shutdown / idle [verified]

- Start: `main.ts:562-...` → auth/package/config subcommands, `parseArgs`, `runMigrations`, settings + diagnostics, session selection (`createSessionManager`, `main.ts:353-444`), then one runtime factory that (re)creates services → session → `createAgentSessionRuntime` (`713-846`).
- Eager at process start: settings load/merge + diagnostics; project-trust resolution when the project has trust-requiring resources (`main.ts:724-761`); `ModelRuntime.create` (+ optional catalog refresh: **off the network path by default** — `refresh({allowNetwork:false})` at `agent-session-services.ts:~178`, and `main.ts:921-928` fires a background refresh only in RPC mode); full `resourceLoader.reload()` (extensions/skills/prompts/themes/context files); theme init; model scope resolution.
- Mode dispatch: `main.ts:930-979` → `runRpcMode` | `InteractiveMode.run()` | `runPrintMode`.
- Idle behaviour: there is **no background loop in the core**. `Agent` is idle when `activeRun` is undefined; `AgentSession.isIdle` adds compaction. Print mode does one prompt sequence, disposes the runtime and returns an exit code (`print-mode.ts:121-168`). RPC mode **keeps the process alive forever while stdin is open** (`rpc-mode.ts:819-820` `return new Promise(() => {})`).
- Explicit shutdown paths: RPC — extension `ctx.shutdown()` sets a flag checked after `agent_settled` (`rpc-mode.ts:345-346`, `357-359`, `747-750`); stdin `end` (`804-807`); `SIGTERM` (and `SIGHUP` off Windows) → `shutdown(143|129)` which awaits `runtimeHost.dispose()`, pauses stdin, flushes stdout, `process.exit` (`366-380`, `728-745`). Print/interactive install the same signal handlers (`print-mode.ts:50-68`; `interactive-mode.ts:3957-4044`). Interactive exits through its own TUI exit/signal handlers.
- Session shutdown semantics: `agent-session-runtime.ts:167-183` → `teardownCurrent()` → `session.abort()` → `emitSessionShutdownEvent({reason})` → host teardown callback → `session.dispose()`; called for `new`, `resume`, `fork`, `quit` (`406-...`). `runner.ts:193-202` → `emitSessionShutdownEvent()` only emits when handlers exist. **Every session replacement invalidates the old extension context and reloads extensions** (`agent-session.ts:2841-2866` reload; `888-890` invalidation message).

### 8. Stable integration boundaries vs internals [verified unless noted]

Stable / public (exported from `packages/coding-agent/src/index.ts` and documented):
- Session factories and runtime: `createAgentSession`, `createAgentSessionServices`, `createAgentSessionFromServices`, `createAgentSessionRuntime`, `AgentSessionRuntime` (`index.ts:204-231`); session replacement APIs (`newSession`/`switchSession`/`fork`/`importFromJsonl`) live on the runtime by design (`docs/sdk.md`).
- `AgentSession` surface: `prompt`, `steer`, `followUp`, `sendUserMessage`, `sendCustomMessage`, `subscribe`, `waitForIdle`, `abort`, `compact`, `navigateTree`, model/thinking setters, `getSessionStats`, `exportToHtml/Jsonl` (`index.ts:15-25`; `agent-session.ts:1175`, `1425`, `1437`, `1503`, `1571`, `853`, `1648`, `1640`, `1967`, `3136`).
- Extension API: event types, `defineTool`, `ExtensionRunner`, tool-result type guards, `createEventBus`, resource-loader overrides and `extensionFactories` (`index.ts:51-172`, `201-202`).
- Cross-process: RPC mode types + `RpcClient` + `runRpcMode` (`index.ts:350-366`; `docs/rpc.md`), with a documented extension-UI sub-protocol for dialogs.
- Tooling: tool factories, `BashOperations` (`createLocalBashOperations`) for sandbox/remote execution used by `AgentSession.executeBash({operations})` (`index.ts:283-339`; `agent-session.ts:3006-3038`).
- Compaction helpers if you truly need them: `compact`, `generateSummary[WithUsage]`, `serializeConversation`, `shouldCompact`, `findCutPoint`, `DEFAULT_COMPACTION_SETTINGS` (`index.ts:28-50`).
- UI components/themes for hosts that embed the TUI (`index.ts:367-417`).

Internals — reachable but not a boundary (do not depend on): `_handleAgentEvent`, `_emit`, `_installAgentToolHooks`, `_pendingNextTurnMessages`/`_pendingCustomMessages`, `_baseSystemPrompt*`, `_replaceMessageInPlace`, `_checkCompaction`/`_runAutoCompaction`, `_refreshToolRegistry`, `SessionManager._persist`/`_rewriteFile`, and the 6k-line `modes/interactive/interactive-mode.ts` internals.

Can NAVI stay an external layer? **Yes.** Concretely supported without touching core:
- In-process SDK host: construct the session, supply `customTools`, pass a `DefaultResourceLoader` with `extensionFactories` for policy, share an `eventBus` with your own code, and use `AgentSessionRuntime` for session switching.
- Out-of-process supervisor: `pi --mode rpc` + JSONL (`prompt`/`steer`/`follow_up`/`abort`/`get_state`/`get_entries since`/`get_tree`), with approvals through `extension_ui_request`/`extension_ui_response`.
- The one thing core does **not** provide for an ambient agent is an event source and a policy engine: there is no watcher/scheduler facility, and docs explicitly instruct extensions not to start background resources from their factory (`docs/extensions.md:220-224`). That loop must be NAVI's, outside pi.

### 9. Adjacent substrates (relevant to an ambient design)

- `packages/agent/src/harness/*` → `AgentHarness` = a durable, multi-lane runtime: lanes, operations (`run`/`compaction`/`navigation`), `accept/drive/resume/abort/watch`, hooks (`before_run`, `transform_context`, `before_request`, `before_payload`, `after_response`, `before_tool`, `after_tool`, `before_compaction`, `before_navigation`), event stream including `config_update`/`usage`/`lane_created`, plus a `Session`/`Entry`/`EntryProjector` storage abstraction with a conformance test suite and a SQLite backend package (`harness/agent-harness.ts`, `harness/types.ts`, `session-backends/sqlite-node/src/sqlite/*`). It also models **deferred provider responses** (`deferred` handle, `stopReason:"deferred"`), which is architecturally closer to an always-on assistant than anything in the CLI package. `[verified code]` — but it is not part of coding-agent's runtime, not in the documented SDK surface, and is excluded from the shipped distribution. Treat as watch-and-maybe-later `[inferred]`.
- Experimental remote harness: `PI_EXPERIMENTAL=1 ./pi-test.sh server|client` (`docs/development.md`); `pi-client`/`pi-protocol`/`pi-server` are devDependencies of coding-agent and the `./client` subpath resolves only under the `source` condition, i.e. **excluded from npm packages and binaries** (`packages/coding-agent/package.json`). `protocol.ts:5` → `PROTOCOL_VERSION = 8` with `serverId/sessionId/attachmentId` fencing — a real transport, but pre-1.0 and moving.

---

## Architectural Implications for NAVI

1. **Pi is a request/response harness with a durable transcript; it has no perception loop, no scheduler and no policy engine.** NAVI's ambient behaviour therefore cannot live "in pi core" — and does not need to. `[determined by §1, §7, §8]`
2. **Two injection channels already exist and have different semantics.** `context` = ephemeral, non-persisted, per-request (good for volatile page state); `custom_message` = persisted, auditable, appears as a user-role message (good for anything the user must be able to review later). Choosing wrongly is the cheapest way to poison a session. `[§2, §3]`
3. **Ambient turns are possible without core changes** (`sendCustomMessage({triggerTurn:true})` + `ctx.isIdle()`, and `deliverAs:"nextTurn"` for deferral), but "no hidden automation" must be enforced by NAVI: pi records what happened, it does not decide what may happen. `[§2]`
4. **Untrusted web content has no structural boundary in pi.** The available levers are the `tool_call` block/modify hook, the tool allow/deny lists at session construction, project trust, and prompt-level framing via `before_agent_start`/`context`. Anything stronger (taint tracking, per-source capability limits) is NAVI's to build.
5. **The session transcript is a single-writer, unlocked, append-only JSONL tree.** Any multi-process NAVI design must not share a session file. Long-term memory belongs in NAVI's own store, projected per turn. `[§4]`
6. **Compaction will eventually eat ambient context.** Page text injected as messages becomes part of compaction summaries; `context`-injected text simply vanishes at the boundary because it was never persisted. NAVI needs an explicit policy for both. `[§5]`
7. **Extension state is session-scoped and dies on session replacement** (extensions are reloaded and old contexts are invalidated). NAVI's durable state must live outside extensions. `[§7]`
8. **Cost/attention control is NAVI's problem.** Pi meters usage but has no budget guard; ambient turns must be coalesced, debounced and budgeted by NAVI. `[inferred from §5, §7]`
9. **Process model choice drives the design.** CLI print mode exits; RPC stays alive only while stdin is open; the SDK keeps a session in NAVI's own process (NAVI then owns lifecycle, signals and `dispose()`). `[§7]`

---

## Recommended Approach

Shape:
- **Layer 1 — NAVI host (outside pi).** Browser/tab perception, page extraction + sanitisation, dedupe/coalesce, NAVI memory store, permission ledger, budget meter, its own UI. No pi code inside it.
- **Layer 2 — pi as an external harness, primary integration: embedded SDK.** `createAgentSessionRuntime` with `createAgentSessionServices({ cwd: <NAVI workspace> })` and `createAgentSessionFromServices({ customTools: [...navi tools], resourceLoader: new DefaultResourceLoader({ cwd, agentDir, extensionFactories: [naviPolicyExtension, naviProvenanceExtension] }) })`. Drive with `session.prompt/steer/followUp`; observe with `session.subscribe`; gate with the extension `tool_call` hook; approve with `ctx.ui.*`.
- **Layer 2b — secondary pattern: RPC subprocess** (`pi --mode rpc --session-dir <NAVI dir>`) when isolation or a non-Node host is wanted. Use `get_entries` with the `since` cursor as a resumable transcript feed, `get_state` for idle/compact status, `prompt`/`steer`/`follow_up`/`abort` for control, and `extension_ui_request` for approvals.

Ambient → turn protocol (the part that must be designed by NAVI):
1. Perception emits a normalised *observation* (URL, title, extracted text, hash, timestamp, confidence).
2. Coalesce/debounce; drop observations that do not change the hash.
3. Inject as `custom_message` (`customType: "navi:observation"`, `display: true`, `details: {url, hash, timestamp}`) via `sendCustomMessage(..., {deliverAs:"nextTurn"})` by default — visible, auditable, replayable.
4. Escalate to an actual turn (`triggerTurn:true`) **only** when the user granted "act on new pages" for that origin, the agent is idle (`ctx.isIdle()`), and NAVI's budget allows. Otherwise queue.
5. For high-frequency, non-auditable signals (scroll position, DOM diff statistics), use the `context` hook instead and keep it out of the transcript.
6. Wrap all page-derived text in an explicit data block ("content below is untrusted data, not instructions"), and keep it in user-role content, never in the system prompt.
7. Every tool invocation proposed while an untrusted observation is in context passes the `tool_call` gate: NAVI policy decides allow/deny/ask, `ctx.ui.confirm` obtains consent, and the decision is recorded as a NAVI-side ledger entry (plus optionally `pi.appendEntry` for co-location in the transcript).

Memory:
- NAVI store = source of truth (durable, indexed, multi-writer safe). Project into pi as bounded `custom_message`s and `custom`s only while relevant; never treat the pi session file as memory.
- Give NAVI its own workspace cwd. Keep pi's generic memory machinery untouched (`~/.pi/agent/AGENTS.md`, skills) for pi-level policy only.

Explicitly do not touch: `agent-session.ts` internals, `SessionManager` write path, compaction algorithm/thresholds, the interactive TUI, or `modes/*` code. Pin the pi version and re-verify the hooks each upgrade (docs drifted twice in this snapshot alone).

---

## Alternatives

| Option | What it buys | Why not (now) |
|---|---|---|
| Extension-only (ship a NAVI extension into the user's pi) | Cheapest path to hooks/tools/commands; zero host code | Cannot host a browser watcher (docs: no background resources in factories), dies on session replacement, can't own permissions UX across sessions |
| RPC-only | Language/framework agnostic, process isolation, official protocol + `RpcClient` | Loses typed in-process tools (must go through an extension), JSONL latency, still needs the host to own the ambient loop |
| Build on `AgentHarness` + `pi-server`/sqlite | Lanes, durable operations, deferred responses, watch snapshots = best fit for always-on ambient | Not a documented consumer API, excluded from the package, protocol at v8 and explicitly dev-only |
| Fork/rebrand pi | Full control of the core | Forking means owning the whole surface for a gain that the external layers above already provide; rebranding is supported without a fork (`piConfig` in `docs/development.md`) |
| Use only `pi-ai` and write NAVI's own loop | Maximum control | Discards session/compaction/extension/trust work that pi already does well — the very thing this research is trying to reuse |

---

## Risks

- **Session file contention:** no locking on JSONL (`session-manager.ts:1029-1056`); two writers = interleaved/lost entries. Mitigation: one process per session file; NAVI owns longer-lived memory.
- **Prompt-cache and cost damage:** injecting volatile page text early (system prompt or context prefix) invalidates provider cache prefixes; prefer appending as the last user-role message and keep the system prompt stable. `[inferred from §2/§3 mechanics]`
- **Compaction poisoning/summary drift:** ambient page content becomes part of summaries and then of all future context; also compaction may drop the ambient message mid-run.
- **Prompt-injection blast radius:** pi does not structurally separate untrusted data from instructions; the only barriers are the hooks NAVI writes.
- **Extension lifecycle traps:** session replacement invalidates captured extension context (`agent-session.ts:888-890`); code that holds a `ctx` across `newSession/fork/switchSession/reload` throws.
- **API-surface drift:** the event set is large and versioned loosely; two doc/code mismatches found in this snapshot. Anything NAVI depends on must be re-verified per upgrade (see Open Questions about versioning).
- **SDK embedding lifecycle:** no idle GC; `dispose()` is mandatory; the signal handlers exist only in the run modes, so an embedding host must install its own and call `runtime.dispose()`.
- **Cold-start latency:** resource discovery + extension loading + model catalog are all startup work; an ambient assistant either keeps a session warm or pays this per interaction.
- **Experimental-path churn:** `PI_EXPERIMENTAL` server/client and the harness protocol are moving targets.
- **Windows/PowerShell:** path and shell differences are handled by pi (`createPowerShellTool`, `config.ts` self-update paths) but NAVI's own tooling must not assume POSIX.

---

## Feature Assessment

| NAVI feature | Class | Rationale |
|---|---|---|
| Perceive the active page (browser extraction, DOM/text capture) | **REJECT** (of pi) / NAVI-owned | No perception, browser or page primitive exists anywhere in pi core, and none is planned in the SDK surface. |
| Inject current page as ephemeral per-request context | **CORE** | `context` event is purpose-built: `structuredClone` + per-request replacement, never persisted (`runner.ts:1034`, `sdk.ts:362`, `agent-loop.ts:288`). |
| Inject page/observation as a persisted, auditable entry | **CORE** | `custom_message` entries + `sendCustomMessage` with `display`/`details`/`deliverAs` (`agent-session.ts:1503-1535`, `session-manager.ts:135`). |
| Autonomous ambient turn initiation | **LIKELY** | Supported via `sendCustomMessage({triggerTurn:true})` + `ctx.isIdle()`; no scheduler exists, so NAVI must own the trigger and the visible justification. |
| Permission gate on actions | **CORE** | `tool_call` hook can block and/or rewrite arguments before execution (`agent-session.ts:483-502`, `agent-loop.ts:626`). |
| Approval UX (allow once/origin/session) | **CORE** | `ctx.ui.confirm/select` in TUI and RPC; `ctx.mode`/`ctx.hasUI` tell you what is available (`docs/extensions.md:968-975`; `rpc-mode.ts:136-200`). |
| Sanitising/redacting tool results before they reach the model | **CORE** | `tool_result` patch of `{content,details,isError,usage}` plus built-in image normalisation (`agent-session.ts:504-535`). |
| Hardening provider payload for untrusted data | **LIKELY** | `before_provider_request`/`before_provider_headers` exist, but rewriting the payload is sharp-edged and is not reflected by `ctx.getSystemPrompt()`. |
| Lightweight session-scoped memory / state across reloads | **LIKELY** | `pi.appendEntry` (custom entries) survives reload and is documented for this; `custom_message` participates in context (`session-manager.ts:94-141`). |
| Long-term NAVI memory | **DEFER to NAVI store** | Pi has no memory/indexing subsystem, and its transcript is unlocked/single-writer — building memory on it would import that constraint. |
| Always-on durable process with resumable state | **EXPERIMENTAL** | RPC stays alive only while stdin is open (`rpc-mode.ts:819`); durable/lane semantics exist only in the non-public harness + server + sqlite backend. |
| Multiple concurrent conversations (per tab/persona) | **EXPERIMENTAL** | `AgentSession` is one transcript; lanes exist only in `AgentHarness`. Multiple `AgentSession`s in one process appear feasible but are unverified for shared-runtime semantics. |
| Background watchers/timers inside pi | **REJECT** | No facility; docs explicitly forbid starting background resources in extension factories (`docs/extensions.md:220-224`). |
| Built-in web fetch/browse tool | **DEFER** | Nothing built in; NAVI should register its own tool and own the untrusted-content policy (pi's `bash` exists but is not a browser). |
| Vector/semantic memory | **REJECT** (of pi) | No embeddings or memory subsystem in core; no extension hook would make it first-class. |
| Multi-user / multi-tenant isolation | **REJECT** | Single-user CLI model with process-global credentials in `~/.pi/agent/auth.json`; no sandbox or tenancy concept. |
| Audit trail of everything the assistant saw and did | **CORE** | Session entries + the full event stream are a complete, replayable log (`session-manager.ts`, `AgentSessionEvent` union `agent-session.ts:144-185`). |
| Enforcing "no hidden automation" | **LIKELY** | Pi records; it does not police. NAVI enforces by making every ambient turn visible (`display:true`), requiring explicit grants, and refusing to run tools that were not consented to. |
| Contract/compaction customisation | **CORE** | `session_before_compact` can cancel or supply the summary; NAVI should extend, never re-implement. |

---

## Open Questions

1. Is `AgentSessionRuntime` intended for long-lived embedded use (session churn, extension reload, resource cleanup), or should hosts expect to create a fresh runtime per user interaction? No documented lifetime contract found.
2. Is `packages/agent/src/harness` (AgentHarness + lanes + sqlite session backend + `pi-server`/`pi-client`) planned to become the supported embedding surface? If yes, NAVI's design should target lanes instead of one session, and this decision is worth deferring until it stabilises. *(confidence: medium that it is the intended successor; low on timing)*
3. What is the sanctioned way to keep ephemeral ambient context out of compaction? Today `context`-injected content is invisible to compaction but also unrecorded; is a "transient, never-summarised" marker planned?
4. Is there any versioning guarantee for the extension event set / SDK exports (semver surface for `ExtensionEvent`, `AgentSessionEvent`)? `PROTOCOL_VERSION` exists for the experimental transport but nothing equivalent was found for in-process APIs.
5. How is a host UI expected to render NAVI-injected entries outside the TUI? `messageRenderers`/`entryRenderers` are only consulted by the TUI; embedded hosts get raw entries.
6. Confirm that several `AgentSession` instances can share one `ModelRuntime`/`SettingsManager` safely (per-session auth overrides, concurrent catalog refresh) — needed if NAVI ever runs concurrent lanes without the harness.
7. Which stability policy applies to `~/.pi/agent/sessions` JSONL v3 entries produced by NAVI-adjacent tooling (migration guarantees, forward-compat expectations)?
8. Whether NAVI should ship as a pi *package* (`pi install` npm/git, settings-listed) or as an SDK-level integration — the former gives distribution and `pi update`, the latter gives control.
