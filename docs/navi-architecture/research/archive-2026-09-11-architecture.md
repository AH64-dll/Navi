# NAVI — Architecture Research & Plan

> Historical snapshot, superseded on 2026-09-13 by [the current architecture](../NAVI-ARCHITECTURE.md). This document contains known incorrect or unresolved assumptions; preserve it as research history, not implementation instructions.

**Status:** Research and planning only. No NAVI code has been written and no Pi source was modified.
**Evidence base:** read-only clone of `github.com/earendil-works/pi` at `C:\Users\lain\NAVI\vendor\pi`, branch `main`, commit `71dca871bc80b6bc97be37f0ca3189399d651fff` (2026-09-11), `@earendil-works/pi-coding-agent v0.85.1`.
**Method:** 10 independent research sub-agents (one per mandated investigation area), each writing a report to `NAVI/research/`; then 7 independent adjudication memos on the genuinely contested decisions. Every load-bearing claim was verified against `src/`, not docs. Tags used throughout: `[verified]` (read in source), `[inferred]` (reasoned from source), `[external]` (outside the repo, unverified here). Citation form: `path:line → symbol → implication`.
**Supporting material:** `NAVI/research/01-core-architecture.md` … `10-red-team.md` and `xreview-1-substrate.md` … `xreview-7-agents.md`. This document is the synthesis; the reports hold the full evidence.

---

## Executive Summary

**What NAVI should probably become.** NAVI is an ambient, browser-first intelligence that helps a person understand, verify and investigate what they are looking at — not a chat window with a browser plugin. Its architecture should be: a **browser extension** that owns perception, page actions and consent; a **local NAVI core** that owns lifecycle, budgets, permissions, memory and presentation; and **Pi embedded as the reasoning engine** that owns the agent loop, sessions, tools, providers and compaction. Pi is a dependency, not a platform. NAVI does not need to modify Pi, fork Pi, or build on Pi's unfinished experimental runtime.

Twelve conclusions from the investigation, in order of importance:

1. **The browser, not the agent, understands web pages.** Pi has zero browser capability — `packages/coding-agent/src/core/tools/index.ts:95` enumerates exactly eight tools, and a grep for `playwright|puppeteer|CDP|webdriver` across `packages/` matches only a test fixture. Chromium/Firefox already implement readability, DOM and accessibility extraction inside the page's own process. Computing "semantic page context" in the agent would mean shipping a second browser, doubling latency and memory, and putting untrusted page text one hop closer to the model with fewer structural filters. `[verified]`
2. **Pi's two declared-stable integration surfaces are the SDK and stdio RPC.** `packages/coding-agent/docs/development.md:33` says so explicitly, and `CHANGELOG.md:37` calls the accidental publication of the experimental remote harness a bug. Everything under `src/experimental/**`, `packages/server`, `packages/client`, `packages/protocol`, and `packages/agent/src/harness/**` is dev-only, POSIX-only, unpublished, and under active breaking redevelopment. `[verified]`
3. **NAVI should embed the SDK in-process as its primary runtime**, with `pi --mode rpc` kept as a *second, isolated* runtime behind one `AgentPort` abstraction. Rationale: SDK hosts can supply their own `ExtensionUIContext` (`agent-session.ts:2468 bindExtensions`, `runner.ts:492-494 hasUI()`), can inject context and block tools before execution, and avoid RPC's missing version field, missing per-tool control, missing `appendEntry`, missing `importFromJsonl`, and its `prompt`-acceptance semantics. This was the single most contested decision — see **Key Disagreements** and the **confidence levels** in the final section. `[verified]`
4. **Nothing in Pi must change.** Every NAVI v1 capability maps onto existing, exported API. Eight small upstream improvements are worth filing (three of them genuinely valuable to Pi itself), but none blocks NAVI. `[verified]`
5. **No permanent daemon by default.** Pi's RPC child is explicitly immortal (`rpc-mode.ts:819-820`, "Keep process alive forever"), so lifecycle must be owned by NAVI. Pi's own experimental coordinator shows the intended pattern: connect-or-spawn with a 10-second deadline, and idle self-shutdown on an unref'd timer 250 ms after the last client (`experimental/coordinator.ts:187-199,483-506`). Browser-spawned activation plus a short idle window beats an always-resident process: process residency buys spawn latency only, not cache residency, because a respawned session can still hit the 5-minute provider prompt cache (`cache-stats.ts:8`). `[verified]`
6. **Browser perception must be snapshot-plus-delta, budgeted before the model sees anything.** The default slice is page identity plus a viewport/focus semantic projection (a DOM-derived accessibility projection, not the raw DOM and not the browser's full AXTree); full-page artifacts are on-demand only. A full accessibility tree is measured at ~14.5k–19.4k tokens on common pages — three to ten times the entire session's useful context budget. `[external]` corpus figures + `[verified]` budget mechanisms.
7. **The page is data, never instructions.** Pi's own security documentation concedes prompt injection "cannot be reliably prevented" (`docs/security.md:37`), and Pi has no permission system or sandbox at all (`README.md:41`, `docs/security.md:31-35`). NAVI must enforce capability at the bridge and the browser — where the action actually happens — not only as an in-agent hook. The proposed model replaces the provisional OBSERVE/RESEARCH/CONTROL levels with four axes: **Scope × Verbs × Mode × Taint**, with the three levels kept as UX presets. `[verified]`
8. **Memory belongs to NAVI, not to Pi sessions.** Pi sessions are cwd-keyed (`session-manager.ts:476-481`), append-only with no edits or deletes (`:1315-1317`), written synchronously per entry (`:1035`), and mutated by compaction — the wrong store and the wrong primary key for page-domain memory. v1 memory is explicit-first: pins and highlights, structured filters, SQLite FTS5 when available. Vectors and semantic recall are a later tier, gated on measured retrieval quality. `[verified]`
9. **One agent, not a swarm.** Pi already parallelizes tool calls within a turn; process isolation (the `subagent` example spawns `--mode json -p --no-session` children) buys context isolation only and cannot be cost-capped across the boundary. Specialist behavior is better expressed as *modes* (the plan-mode pattern: tool allowlist + persona + `{block:true}`) than as separate agents. `[verified]`
10. **The feature list should get smaller, not longer.** Seven primitives (perception, context assembly, agent runtime port, memory, capability engine, presence, budget governor) unlock all 15 candidate capabilities; roughly a third of the original feature list moves to LATER or gets replaced by a better mechanism. V1 should be OBSERVE-only. `[inferred from verified primitives]`
11. **Budgets are architecture.** The realistic failure mode is not a crash — it is NAVI being slower and more expensive than a normal browser through unconditional full-context clones on every provider call (`runner.ts:1034-1036`), synchronous per-entry session writes, hidden compaction LLM calls, and unbounded event streams. Every component gets a measurable budget, and the first experiments measure them. `[verified]`
12. **The first prototype must be able to lose.** The cheapest way to falsify this whole direction is Agent 10's null hypothesis: a plain browser extension with local extraction and one API call per user-initiated action. If NAVI cannot beat that on a concrete task, the harness is decoration. The first prototype therefore includes that comparison as an explicit kill criterion. `[inferred]`

**What this means for the original concept.** NAVI survives, but reshaped: it is not "Pi with browser tools bolted on"; it is an ambient browser harness that uses Pi for reasoning. The Serial Experiments Lain aesthetic is a presentation concern that the architecture deliberately keeps at the edge — none of the core decisions depend on it, and one of the strongest recommendations (browser panel + badge first, desktop shell later) exists precisely because the ambient-interaction model must be proven before visual ambition is funded.

---

## What Pi Already Gives Us

These are the capabilities NAVI should reuse rather than rebuild. All are public surface unless noted.

| Capability | Evidence | Why NAVI cares |
|---|---|---|
| **Four operating modes**: interactive TUI, print/JSON, RPC (stdin/stdout JSONL), SDK embedding | `README.md:19`; `docs/index.md:62-67` | NAVI needs only SDK + RPC; the other two are irrelevant but prove the boundary is designed for reuse |
| **`createAgentSession()`** with cwd, agentDir, model, thinking level, `tools`/`excludeTools`/`noTools`, `customTools`, `resourceLoader`, `sessionManager`, `settingsManager`, `scopedModels`, `modelRuntime`, `sessionStartEvent` | `core/sdk.ts:39-88,173-410` | One call constructs the whole agent; the options are exactly the seams NAVI needs (tool allowlist, custom tools, in-memory sessions) |
| **`AgentSessionRuntime`** — `newSession`, `switchSession`, `fork`, `importFromJsonl`, `dispose` | `core/agent-session-runtime.ts:74-406` | Session replacement lifecycle; `importFromJsonl` and `dispose` have no RPC equivalent |
| **`AgentSession` public API** — prompt/steer/followUp, abort, compact, model + thinking control, `getContextUsage`, `getSessionStats`, active-tool control, `bindExtensions`, `reload`, `executeBash` with custom `BashOperations`, tree navigation, export | `core/agent-session.ts` (3,552 lines) | The full control surface; NAVI should depend only on the small stable subset |
| **Tree-structured sessions** — JSONL v3, `id`/`parentId`, compaction entries, branch summaries, labels, `custom` (out of context) vs `custom_message` (in context), in-memory mode, `forkFrom` | `docs/session-format.md`; `core/session-manager.ts:32-153,334-470` | The transcript/replay model; `custom` vs `custom_message` is exactly the "observation vs decision" durability split NAVI needs |
| **Compaction and branch summarization** with `reserveTokens`/`keepRecentTokens` defaults (16384/20000), extension hooks `session_before_compact`/`session_compact`/`session_before_tree`, disableable | `core/compaction/compaction.ts:132-238`; `docs/compaction.md` | NAVI must not duplicate it; it must budget for it (or switch it off) and keep its memory elsewhere |
| **Extension API** (`ExtensionAPI`, ~1,797-line contract): 30+ lifecycle events, `registerTool`, `registerCommand`, `registerShortcut`, `registerFlag`, `sendMessage`, `sendUserMessage`, `appendEntry`, session naming, message/entry renderers, markdown transformers, provider registration, event bus, tool-set control, model/thinking control, `exec`, UI widgets/status/footer/header/working indicator, dialogs with timeout+signal, custom components and editors | `core/extensions/types.ts:1252-1512`; `docs/extensions.md` | Policy gate, tools, commands, durable state and (in SDK mode) UI all live here |
| **Event semantics that make gating and injection possible**: awaited in-order dispatch; `tool_call` fails closed on throw; `before_agent_start` per-turn system prompt; `context` per-provider-call message shaping; `input` transform/handle; `ui_prompt_start/end`; `agent_settled` | `core/extensions/runner.ts:851-1003,1034-1064`; `core/agent-session.ts:1199,1285-1314`; `docs/extensions.md:273-960` | This is the entire NAVI policy and context surface; it also contains the two traps (unconditional `context` clone; `sendMessage` persists) |
| **Eight built-in tools with pluggable execution backends**, read-only profile constructor, images as first-class content, 2000-line/50KB truncation contract with continuation offsets and out-of-context `details` | `core/tools/index.ts:95,118-173`; `core/tools/truncate.ts:11-12`; `ai/src/types.ts:351-371` | NAVI's tools are additive and can be stubs into the bridge; screenshots are already provider content |
| **Skills (progressive disclosure) and prompt templates** — `<available_skills>` block with bodies loaded on demand; `disable-model-invocation`; prompt templates with positional args | `core/skills.ts:74,341,355`; `core/prompt-templates.ts:11,70,269`; `docs/skills.md` | Capability packaging without new core code; note `allowed-tools` is documented but unimplemented (`docs/skills.md:149` only) |
| **Dynamic tool loading** — `setActiveTools` at runtime plus native provider-side deferred loading (Anthropic 4.5+, OpenAI 5.4+), with a universal fallback | `docs/extensions.md:2371-2408`; `core/extensions/types.ts:1400-1406` | Lets NAVI keep the prompt small; caveat: activating a tool with a `promptSnippet` rebuilds the system prompt and can invalidate the cache |
| **Provider abstraction** — model catalog, subscription OAuth and API keys, custom providers, streaming, retry/timeout policy, thinking levels, HTTP proxy, usage and cost accounting | `packages/ai/src/**`; `core/model-runtime.ts`; `docs/providers.md`, `docs/models.md` | NAVI inherits multi-provider flexibility for free; a NAVI-specific model policy is configuration, not code |
| **RPC mode** — LF-only JSONL, full event stream (including two undocumented frames), session/tree/entry queries, model/thinking/compaction control, raw bash, session replacement, and a bidirectional extension-UI dialog sub-protocol with agent-side timeouts | `modes/rpc/rpc-mode.ts:136-311,394-416,819-820`; `docs/rpc.md` | The isolated lane and the fallback transport; its gaps are enumerated in Architecture C |
| **Example extensions that de-risk nearly every NAVI pattern**: `subagent`, `plan-mode`, `permission-gate`, `protected-paths`, `ssh` (remote operations via tool re-registration), `custom-compaction`, `structured-output`, `todo`, `handoff`, `notify`, `dynamic-tools`, `rpc-extension-ui` | `examples/extensions/**` | Each is a working proof of a pattern NAVI would otherwise invent |
| **Cache and cost accounting** — `usage-totals.ts`, `cache-stats.ts` (5-min TTL, 1,024-token noise floor, dollar waste computation), `timings.ts` (`PI_TIMING=1`), startup profiler (`profile:tui`/`profile:rpc`) | `core/cache-stats.ts:8,11,138-163`; `core/timings.ts`; `scripts/profile-coding-agent-node.mjs` | Prompt-cache stability and startup cost are already measurable in Pi's own terms |
| **Evals harness** — behavioral, model-backed comparisons with baseline/candidate tables, repetitions and judges | `packages/evals/README.md:152-184`; `src/pi-harness.ts:39-42` | The repo's own answer to "is this capability worth building?" |

**What Pi deliberately does not give us** (and NAVI must therefore own): any browser capability; any permission system or sandbox; any scheduler or background execution; any memory beyond sessions; any UI toolkit that is not terminal-shaped; any cost/step/time cap; any process lifecycle for an ambient service; any cross-session search (the `SessionSearchService` skeleton at `packages/agent/src/search/index.ts:20-27` has no implementation). `[verified]`

---

## Repository Architecture Map

### Monorepo packages (11)

| Package | Role | Usable by NAVI? |
|---|---|---|
| `packages/coding-agent` | The agent: CLI, modes, core runtime, extensions, tools, sessions, docs, examples | **Yes** — the only package NAVI integrates with |
| `packages/agent` | Provider-agnostic agent loop + message types + experimental harness | Loop: yes (transitively). `harness/**`: **no** (mid-rewrite, documented live bugs) |
| `packages/ai` | Provider/model abstraction, catalogs, OAuth, streaming, usage | Transitively, and directly for auxiliary model calls |
| `packages/tui` | Terminal UI components + optional native clipboard/modifier helpers | No (terminal-only) |
| `packages/protocol`, `packages/server`, `packages/client` | Experimental remote harness: CBOR framing, exact-version handshake (v8), Unix-socket transport | **No** — dev-only, POSIX-only, unpublished (`package.json:29-39,75-78`; `client/src/unix.ts:38,99` throws on Windows) |
| `packages/session-backends/sqlite-node` | SQLite persistence backend with WAL and integrity triggers; no FTS, no search service | Reference only (pragma patterns) |
| `packages/telemetry` | No-op by default | No |
| `packages/evals` | Eval harness | Yes — for measuring NAVI capabilities |
| `packages/chord` | Experimental multi-facet UI/plugin design ("agent worker, terminal UI, remote WebUI") | Design reference only |

### `packages/coding-agent/src` map

- **`core/`** (48 files) — the runtime: `agent-session.ts` (3,552 lines; facade over `Agent` + services + persistence + compaction + extensions + bash), `agent-session-runtime.ts` (session replacement, import), `agent-session-services.ts` (settings/model/loader/extension wiring), `sdk.ts` (public SDK factory), `session-manager.ts` (1,746 lines; JSONL tree), `resource-loader.ts` (skills/prompts/themes/AGENTS.md discovery), `system-prompt.ts`, `compaction/`, `tools/` (8 built-ins + renderers + truncation), `extensions/` (`types.ts` 1,797 lines, `loader.ts` jiti in-process, `runner.ts` 1,160 lines dispatch), `model-runtime.ts`/`model-registry.ts`/`models-store.ts`, `settings-manager.ts`, `auth-storage.ts`/`runtime-credentials.ts`, `cache-stats.ts`, `timings.ts`, `output-guard.ts`, `event-bus.ts`, `project-trust.ts`/`trust-manager.ts`, `skills.ts`, `prompt-templates.ts`, `slash-commands.ts`.
- **`modes/`** — `interactive/` (6,620-line `interactive-mode.ts` + TUI renderer + components), `rpc/` (`rpc-mode.ts` 712 lines, `rpc-client.ts`, `rpc-types.ts`, `jsonl.ts`), `print-mode.ts`, `json-event.ts`.
- **`cli/`** — argument parsing, auth commands, session picker, setup, startup UI.
- **`experimental/`** — client/server/coordinator/session-worker/plugin/relay stack (gated by `PI_EXPERIMENTAL=1`, excluded from the package).
- **`index.ts`** — 425-line public barrel. **`cli.ts`/`main.ts`** — eager entry (see below).
- **`examples/`** — 70+ test extensions, 13 SDK examples, plugin examples. **`docs/`** — 31 documents. **`test/`** — 191 top-level test files (repo-wide: 544 test files, ~122.6k lines).

### Runtime facts that shape NAVI

| Fact | Evidence | Consequence |
|---|---|---|
| Entry is fully eager: `main.ts:8-71` statically imports ~60 modules for every mode, including TUI/theme/export code | `[verified]` | A NAVI process must have its own thin entry; never import the CLI barrel casually (the repo's own `check-entry-graphs.mjs` quotes ~37 MB of evaluated graph through a barrel) |
| Only three lazy islands: photon/WASM, highlight.js, jiti extension loading | `[verified]` | RPC/SDK cold start is dominated by the eager graph — the number to measure first |
| Session files: `~/.pi/agent/sessions/--<cwd>--/<ts>_<uuidv7>.jsonl`, v3 tree, auto-migrated on load; file created only after the first assistant message; no file locking | `[verified]` | NAVI must own its own storage and treat Pi sessions as one transcript per task, not as the memory system |
| Every appended entry is a synchronous `appendFileSync` that also advances the leaf | `session-manager.ts:1029-1063` | Never write a Pi entry per browser observation |
| Auto-compaction fires a hidden LLM call per threshold crossing; summaries cap tool results at 2,000 chars | `[verified]` + `docs/compaction.md:271` | Budget or disable compaction for ambient sessions; never route critical page data through tool results you expect to survive |
| RPC child never self-exits; TUI renders only on request (16 ms coalescing); the only idle polling is git `watchFile` (1 s) and reftable (250 ms) | `[verified]` | Lifecycle is NAVI's job; idle CPU in Pi itself is otherwise ≈0 |
| Native helpers (clipboard, modifier keys) are prebuilt, optional, lazy, and absent under pure Wayland | `[verified]` | Feature-detect and degrade; do not build NAVI on clipboard |
| CI runs the full suite on `ubuntu-latest` only; Windows gets `--help`/`--version` smoke tests | `.github/workflows/ci.yml:14`; `build-binaries.yml:196-200` | Windows correctness is NAVI's cost; ship our own cross-platform CI |
| Trust: project trust gates *resource loading* only, is resolved by the CLI, and is defaulted **true** when `SettingsManager.create()` is called without options (which is what the SDK does) | `docs/security.md:7`; `main.ts:722-741`; `settings-manager.ts:374` | SDK hosts fail open on project trust; NAVI must pass `projectTrusted: false` explicitly (see Pi Changes) |

### Integration boundaries (what NAVI may depend on)

**Stable:** the SDK entry (`createAgentSession` + option/result types), `AgentSession` public methods, the extension event/tool API, `pi.appendEntry`, and (as a second runtime) stdio RPC.
**Avoid:** anything `_`-prefixed, `modes/interactive/**` internals, `src/experimental/**`, `packages/agent/src/harness/**`, the experimental protocol/server/client packages, and any doc that describes unbuilt behavior (the repo itself says "if a doc and the code disagree, the code wins" in `packages/agent/docs/mobile-handoff/README.md:27`).
**Doc drift already found:** `docs/sdk.md` documents a `SessionManager.getPath()` that does not exist; `docs/compaction.md` shows `timestamp` as a number where entries use ISO strings; the shipped RPC example uses `readline`, which the protocol explicitly forbids; `docs/development.md`'s fork checklist omits the `piConfig.name` field the code reads. `[verified]`

---

## 10-Agent Findings (synthesis)

**Agent 1 — Core architecture.** `AgentSession` is a facade over one `Agent` plus injected `SessionManager`, `SettingsManager`, `ResourceLoader`, `ModelRuntime`; it subscribes to the agent in its constructor, so it cannot precede it. Lifecycle is derived, not a state machine (`isIdle` = no run + no compaction); `agent_settled` is the only correct "ready" edge. The agent loop owns turns/tools/`convertToLlm`; `AgentSession` adds preflight, persistence, compaction, extension bridging, bash and tree navigation. Extension listeners are awaited in order and can delay the loop; `session.subscribe` is synchronous fire-and-forget. Everything loads eagerly in `createAgentSessionServices`; only skill bodies are read on demand. NAVI can stay fully external: SDK factories, the session API, the extension API and `RpcClient` are all exported. *Verdict:* integrate as an external layer; keep the watcher, scheduler and policy engine in NAVI; do not re-implement compaction.

**Agent 2 — Integration modes.** The public surface is CI-enforced to three subpaths and upstream states SDK + stdio RPC are the supported integration surfaces; the experimental harness is source-only and was accidentally published once. Extensions are in-process jiti modules with full privileges and no isolation. `ExtensionContext` is a curated read-only subset; `bindExtensions` is public but its `ExtensionBindings` type is not exported from the package root. RPC's inbound command set is deliberately thin: no per-tool invocation, no tool introspection, no `appendEntry`, no `importFromJsonl`, no settings/scoped-model mutation, no protocol version, no readiness handshake. The `tool_call` gate is the only pre-execution block and it fails closed. *Verdict:* hybrid — RPC for the agent process, one in-child extension for policy/injection, SDK for what RPC cannot reach, NAVI-owned UI; reject fork and the experimental stack.

**Agent 3 — Browser perception.** Pi has no fetch/browser/MCP support by design, so all perception is net-new. The `context` event is the right ambient carrier only if you accept its cost (see Agent 10); `registerTool` is the depth carrier (text + images already supported). The `ssh` extension is a near-perfect template for a bridge (flag → spawn → operations → tool re-registration → config at `session_start`). Pi already ships transport conventions in `experimental/mini` (newline JSON, versioned handshake) and a strict LF-only JSONL contract. Three-layer trust split (content script → background → bridge) with redaction at capture; `activeTab`+`scripting` by default, `debugger`/`<all_urls>` optional and user-gesture-gated; snapshot-authoritative plus advisory deltas; budgets before the model. *Verdict:* browser-side extraction, snapshot+delta, no continuous DOM, no debugger in v1.

**Agent 4 — Context intelligence.** The `context` hook fires once per provider call (including every tool-loop iteration) and is deep-cloned; it is invisible to Pi's compaction threshold and usage meter, so NAVI must keep its own ledger. Pi's truncation contract (2000 lines/50 KB + continuation) and accounting primitives are reusable exports. A tiered ladder (identity+outline 120–250 tokens; viewport/focus 300–900; query-relevant sections 800–2,500; deltas 40–300) with a hard cap of ~4k tokens per turn is defensible against Pi's 20k `keepRecentTokens`. Off-screen content must be tool-only, append-only injection preserves cache prefixes. *Verdict:* tiered, budgeted, tool-escalated context; never system-prompt page text.

**Agent 5 — Memory.** Pi's session store is append-only, immutable per entry, cwd-keyed, synchronously written, and rewritten in place by compaction; there is no retention, no reclamation, no working search, and `/share` would leak whatever is stored. A real SQLite backend exists but exposes no search. The normative search design in `packages/agent/docs/harness.md:568-576` (standalone store, pull-based cursors, "the index is a rebuildable projection with zero authority") is the right template. Capture must be explicit-first; the bridge must be the writer; the Pi extension can only read. *Verdict:* separate NAVI SQLite store; v1 = metadata + explicit pins + FTS; vectors later; retention shipped with capture.

**Agent 6 — Security.** Pi has exactly one trust concept (input-loading) and no per-tool approval unless an extension supplies one; the hooks that gate all fail closed. Pi opens no network listener; every network surface is one NAVI introduces. There is no redaction anywhere: sessions store verbatim output, `/share` uploads them, and bash output ≥50 KB spills to a temp file handed to the model. Extensions can read credentials and register providers that become the transport. Bridge transport should prefer native messaging; if a socket, bind 127.0.0.1 with a per-launch token, origin checks, no CORS. Approvals must render in a trusted surface (a page can draw a pixel-perfect fake); timeouts deny. *Verdict:* four-axis capability model enforced outside Pi's message array; audit log separate from sessions; reject a NAVI credential vault.

**Agent 7 — Performance and platforms.** Startup is eager for every mode; the only idle polling in Pi is git watching; RPC is designed to live forever; the experimental coordinator already implements connect-or-spawn + unref'd idle shutdown. Windows-specific defensive code is extensive (cross-spawn shims, a re-arming stdio grace for the inherited-handle hang, `taskkill /F /T`, no SIGHUP, no suspend), and none of it is regression-protected because CI is Ubuntu-only. Measurable constants to reuse: 16 ms render coalescing, 100 ms bash flush, 500 ms debounce, 5-minute cache TTL, 300 s HTTP keep-alive, 4-hour catalog refresh. *Verdict:* event-driven, no daemon by default, stdio everywhere, bounded queues, cache-stable context, explicit budgets and per-OS CI.

**Agent 8 — UX/interface.** Pi's UI is terminal-shaped from the floor up: components return ANSI strings; overlays, widgets and custom components are TUI constructs; RPC degrades `custom()` to `undefined`, widgets to string arrays, and themes to nothing. But the event/state layer is transport-neutral: `ui_prompt_start/end`, `agent_settled`, `queue_update` and the full session/tree stream flow over RPC, so a NAVI-drawn surface loses nothing that matters. `Terminal` is an injectable interface with an in-tree non-TTY implementation, so rendering Pi's TUI into another sink is possible but requires a hand-built viewport. *Verdict:* NAVI draws its own surfaces from the event stream; idle state is minimal presence, not a transcript; overlays/panels/ ambient annotations over chat.

**Agent 9 — Capabilities.** Every candidate capability is a composition of ~5–7 primitives, and no capability needs a primitive that does not already exist. Skills + dynamic tools replace feature packaging but not ambient behavior, gating or durable state — those need an extension. Four hard patterns already have working examples (subagent, plan-mode, state reconstruction from tool details, structured termination). Pi has no cost/step/time cap anywhere, no audio content type, and no background bash. *Verdict:* 7 primitives; v1 CORE = see/explain, ambient bubble, source ledger, bounded research, conflicting sources, progress ledger, governor, injection quarantine; voice is an edge client of RPC, not an extension.

**Agent 10 — Red team.** The unconditional `structuredClone` in `emitContext` is paid on every provider request even with zero `context` handlers — the cheapest upstream fix in the repo (5 lines). `sendMessage` writes to context *and* disk; `appendEntry` does neither. Pi sessions are cwd-keyed, synchronously written, and compaction-mutated — never memory. RPC cannot render custom UI and its `prompt` response means "accepted", not "succeeded". The experimental harness and `AgentHarness` are mid-rewrite with documented live bugs and must not be foundations. Forking buys a merge war, not a capability: the browser capability NAVI needs does not exist in Pi to fork. The null hypothesis (extension + one API call) is the comparison NAVI must beat. *Verdict:* SDK in-process, browser-side semantics, bridge-owned policy and budgets, no fork, no daemon, no overlay workstream.

## Key Disagreements

Seven adjudications were run as independent cross-reviews (`NAVI/research/xreview-*.md`). The table records what was contested, what evidence decided it, and what remains genuinely open. Consensus was not forced.

| # | Disagreement | Positions | Adjudication and deciding evidence | Residual uncertainty |
|---|---|---|---|---|
| 1 | **Integration substrate: SDK in-process vs RPC child vs hybrid** | A1/A8/A10: SDK in-process. A2: RPC-first hybrid. A6: RPC for isolation. XR1: SDK primary, RPC as second runtime behind one port. XR2/XR6 (lifecycle/surface memos) assumed an RPC child | XR1 verified that an SDK host **can** render extension UI (`uiContext !== noOpUIContext` flips `hasUI`, `runner.ts:492-494`) and that RPC cannot (`rpc-mode.ts:228-231`), that RPC has no protocol version while its semantics changed inside patch releases (`CHANGELOG.md:18,87`), and that the SDK fails **open** on project trust (`settings-manager.ts:374` defaults `projectTrusted` true; the CLI computes it at `main.ts:722-741`). Winner: SDK primary; RPC retained as an isolated second runtime | **The unresolved one.** XR1's own confidence is *medium*; XR2/XR6's lifecycle and surface memos are more natural over an RPC child. Decision is made swappable by the `AgentPort` abstraction (see Recommended Architecture). Triggers to flip to RPC-first: measured SDK host issues (leaks, blocking extension thread), need for per-turn process isolation, or a multi-language UI process |
| 2 | **Activation: always-on daemon vs lazy/event-driven** | A8: one resident daemon owning one AgentSession. A7: no daemon; browser-managed activation; idle exit. XR2: hybrid, warm mode opt-in | XR2 verified Pi's own precedent: connect-or-spawn with 10 s deadline and 250 ms empty-shutdown on an **unref'd** timer (`experimental/coordinator.ts:187-199,483-506`); the RPC child is immortal by design (`rpc-mode.ts:819-820`); startup is eager (~60 static imports, `main.ts:8-71`). Winner: lazy by default, no resident process; warm mode is an opt-in | Low on the verdict; the exact idle constants (120 s supervisor, shorter for a child) are targets, not measurements |
| 3 | **Bridge transport and where processing happens** | A3: socket-first, bridge in-process in Pi. A6: native messaging preferred. A7: native messaging host **is** the bridge. A10: semantics must be computed browser-side. XR3: native messaging primary, WS fallback, standalone bridge | XR3 corrected a factual premise: **no browser extension can originate a UDS or Windows named pipe**; the only egress paths are native messaging (browser-spawned host over stdio) or WebSocket to loopback. Winner: native messaging primary, loopback WS fallback with token + Origin + no CORS; bridge is a standalone process that *may embed* Pi; content script does DOM extraction + first-pass redaction, background is the privileged layer, bridge does framing/budgets/dedup/provenance, Pi reasons only | Medium on native-messaging-primary (enterprise policy friction, per-browser install path unverified); Medium on exact MV3 worker-lifetime behaviour |
| 4 | **What represents "the page": a11y tree vs DOM vs browser-side semantics** | A4: accessibility-tree spine + viewport-first tiers. A3: DOM snapshot + deltas, pruned a11y ≤32 KB, debugger experimental. A10: browser-side semantics, not agent-side. XR4: DOM-derived accessibility projection, browser-side | XR4 reconciled these: a11y *semantics* is the spine, the DOM is the substrate (geometry, scroll, repair), the real CDP AXTree is a Chrome-only, infobar-costed ground truth → EXPERIMENTAL (absent in Firefox). Full trees stay on-demand (14.5k–19.4k tokens). Winner: DOM-derived a11y projection computed browser-side; viewport/focus is the default *slice*, page identity is always page-scope; page-scope questions escalate to an artifact, not a bigger slice | Medium on the token figures (three external samples, no NAVI corpus); Low on a11y sufficiency for canvas/grid app-UI pages |
| 5 | **Memory: dedicated store + vectors now vs explicit pins only vs Pi sessions** | A5: full SQLite store with vectors/RRF. A9: branch+file memory only in v1. A10: never Pi sessions. XR5: separate store, explicit-first, FTS if present, **no vectors in v1** | XR5 verified Pi sessions are immutable, cwd-keyed, synchronously written and compaction-mutated; a real SQLite backend exists but exports no search; Pi's own normative search design says the index is a rebuildable projection with zero authority. Winner: NAVI-owned SQLite, one writer, explicit-first capture, structured filters + FTS5-if-available with fallback; vectors gated on a measured paraphrase-miss precision floor | Store placement re-validated when the capture path is settled (bridge vs extension); Medium on MV3 capture-loss mechanics; Low on TTLs |
| 6 | **First surface: terminal-first vs browser-first vs desktop-first** | A8: Phase 1 terminal-only, browser/desktop later. Original concept + A6/A10: the browser is where the user is. XR6: browser extension panel + badge first | XR6 verified that the extension is mandatory under every option (Pi has no browser capability), that RPC's UI cliff degrades *extension-drawn* UI but not NAVI-drawn UI, and that the tray-only option proves neither summon nor answer (`ServerLifetime` retires in ~1 s; `notify` emits no `ui_prompt_*`). Winner: browser side panel + badge as a thin view over a session it does not own; desktop shell later; terminal as an optional developer surface | Medium — contingent on the browser-centred-user premise; if false, desktop-first inverts the surface order but not the architecture |
| 7 | **One agent vs specialist agents** | A9: 7 primitives incl. subagents + governor. A10: N RPC children is a resource trap. XR7: one agent; roles are modes; subprocess subagents only as a flag-gated context-isolation tool | XR7 verified Pi already parallelizes tool calls inside one turn (`agent-loop.ts:417-423`), that the subagent example's children get **no** cost cap from the parent and their own compaction is uncounted (`subagent/index.ts:296-302`), that plan-mode is a complete role-swap pattern, and that `terminate:true` is not a reliable batch stop. Winner: one agent + tools + skills; specialist behavior as modes; subagents only behind evals + caps | Low on the verdict; the "cheapest evidence" experiment (one eval cell, ~a day) is specified in *Experiments Before Commitment* |

Two smaller disagreements were resolved without a memo: **generic parser vs site-specific adapters** (XR4: generic extraction + page-*type* output shaping; per-site adapters are the maintenance trap) and **desktop UI vs browser UI** (folded into #6: browser first, desktop shell later, one session owner, publish snapshots).

---

## NAVI Product Principles

1. **The browser is the world; NAVI is a guest in it.** NAVI does not own the user's browsing, does not require a special browser, and does not automate anything the user did not ask for. Normal, user-driven browsing with a read-oriented assistant.
2. **OBSERVE first; capability is earned.** V1 is read-only. Navigation and actions are separate, later, per-origin, per-verb permissions.
3. **Pages are data, never instructions.** Every token of page content is tainted, fenced and labelled; no page-derived content may trigger an irreversible action.
4. **The browser understands pages; Pi reasons about decisions.** Extraction, redaction and semantics live in the extension/bridge. Pi receives reduced summaries and asks for more via tools.
5. **Pi stays Pi.** Do not duplicate compaction, session trees, the tool loop, providers or retries. NAVI extends through the SDK, the extension API and (when isolated) stdio RPC — never by forking behaviour.
6. **Idle means absent.** No component runs when the user is not browsing, unless explicitly in an opt-in warm mode.
7. **Budget before beauty.** Tokens, CPU, RAM, latency and dollars are architectural inputs with measured targets, enforced outside the model.
8. **Memory is explicit-first, separable and forgettable.** Remembering is a user gesture; the store is inspectable, deletable and never the transcript.
9. **Presence, not chat.** The idle state is a quiet indicator and a summon affordance, not an empty text box.
10. **Prove value against the null hypothesis.** Every capability must beat the dumb-extension baseline on a measured task, using Pi's own eval harness.
11. **Upstream-compatible by default.** No source fork. Contribute small fixes upstream; own only the build if and when a non-developer installer requires it.
12. **The aesthetic is an edge concern.** The Lain-inspired surface (dark, cyan, signal-processing) is a presentation layer over the same event stream; it must never drive architecture decisions.

---

## Candidate Feature Matrix

Ratings are for the recommended architecture. Cost/risk columns: **L/M/H**. "Leverage" = how much other capability it unlocks.

| Feature (candidate or discovered) | Verdict | Value | Leverage | Runtime/token cost | Security/privacy risk | Notes |
|---|---|---|---|---|---|---|
| Page identity + viewport/focus semantic context (P1/P2) | **FOUNDATIONAL** | H | H | L–M (perception is local; ~0 incremental tokens when unchanged via hash gate) | M (untrusted text enters context; mitigated by fencing + budgets) | The spine. Everything else is a view over it |
| "What am I looking at?" | **FOUNDATIONAL** | H | H | L (one decision payload) | M (injection) | The first prototype; must work without copy/paste |
| Ambient context bubble (proactive page state) | **FOUNDATIONAL** | H | H | M (must be hash-gated, appended after cache breakpoint, never persisted per observation) | M–H (attack surface grows with ambient ingestion) | The mechanism, not a feature: budgets + taint + provenance are part of it |
| Explicit memory: "remember this" / pin / recall / forget (P4) | **FOUNDATIONAL** | H | H | L (metadata + FTS; no vectors in v1) | M (a semantic browsing history is sensitive; explicit-first + retention + delete) | Bridge is the writer; Pi extension is read-only; `/share` must exclude it |
| Capability engine: scope × verbs × mode × taint + consent UI (P5) | **FOUNDATIONAL** | H | H | L | H if skipped (this *is* the mitigation) | Enforcement where the action happens: bridge/browser, not only in Pi |
| Budget/step/time governor (P6) | **FOUNDATIONAL** | H | H | L (local accounting; prevents H cost elsewhere) | L | Pi has no caps at all; nothing autonomous ships before this |
| Prompt-injection quarantine / taint propagation | **FOUNDATIONAL** | H | H | L (labeling) | H if skipped | Tainted-only context cannot invoke irreversible verbs |
| Cache-stable context discipline + cache-waste metric | **FOUNDATIONAL** | M | H | L (prevents H re-billing) | L | Byte-stable prefix; volatile data appended; reuse `cache-stats.ts` |
| Source ledger with provenance (url, title, retrievedAt, excerpt, query id) | **FOUNDATIONAL** | H | H | L (details, not context) | M (stores untrusted text + URLs) | Prerequisite for every research/verify/compare capability |
| Local heuristics gate in front of every ambient LLM call | **HIGH VALUE** | H | H | L (prevents unbounded spend) | L | Default ≥ 5 min between ambient calls, 1 in flight |
| Explain selected element | **HIGH VALUE** | H | M | L | M (selection may contain secrets; redact) | Selection is just a small payload; no new primitive |
| Point→Ask (screen point → question) | **HIGH VALUE** | M–H | M | L–M (region crop if sent as image) | M | Needs a client gesture; text-first, image only on demand |
| Compare current page with previous context | **HIGH VALUE** | M | M | M (snapshot history must be deliberate) | M | Requires the snapshot store; naive caching bloats context |
| Bounded research from the current page (RESEARCH) | **HIGH VALUE** | H | H | M–H (fetches + turns; governor caps) | H (fetching attacker-influenced URLs) | Domain policy, fetch caps, no credentials, robots respect |
| Collect supporting **and conflicting** sources | **HIGH VALUE** | H | H | M | M | Cheapest real differentiator; adversarial role as a mode |
| User-visible research progress (ledger + status) | **HIGH VALUE** | H | M | L | L | Composition of status/widget/entry events; plan-mode shape |
| Desktop notification on long-run completion | **HIGH VALUE** | M | L | L | L | One `agent_settled` handler |
| Investigation handoff ("continue as a focused thread") | **HIGH VALUE** | M | M | L | L | Copy the `handoff` pattern; keeps sessions small |
| Checkpoint/undo for actions (page state + URL + scroll) | **HIGH VALUE** | H | M | M | M | Mandatory before CONTROL ships, not before |
| Shareable dossier export | **OPTIONAL** | M | L | L | M (redaction before export) | `/export` HTML exists; make it a file, not a transcript |
| Capability discovery UI (`/navi`) | **OPTIONAL** | M | L | L | L | Modelled on the `/tools` extension; makes permissions inspectable |
| Browser overlay annotations (beyond panel/badge) | **OPTIONAL** | M | M | M (rendering + DOM anchoring) | M (page can spoof visuals; never render consent in-page) | Aesthetics + interaction bet; after the panel proves the model |
| Research graphs / tab graph | **LATER** | M | L | M (UI work) | L | Data is trivial (ledger + session tree); the graph is a product bet |
| Voice (push-to-talk; local ASR/TTS; text at the Pi boundary) | **LATER** | M | L | M (edge process) | M–H (utterance + page context are sensitive; local ASR required) | An edge client of RPC/SDK, not an extension, not a tier |
| Semantic recall ("the repo I looked at earlier") | **LATER** | H | M | M (index + embeddings) | M (privacy) | Gated on measured retrieval precision; FTS first, vectors second |
| Autonomous multi-step research | **LATER** | H | L | H (unbounded by nature) | H | Only after governor + plan-first + evals; human checkpoint mandatory |
| Controlled browser actions (CONTROL) | **LATER** | H | M | M | H | Stage 6; per-origin allowlists; trusted consent; undo story first |
| Multi-surface deferred questions (answer later from phone) | **LATER** | M | M | M | M–H (auth missing upstream) | Pi's facet design exists but is experimental and unauthenticated |
| Screenshot/vision page understanding | **LATER** (on-demand only) | M | L | H (~4,800 tokens/image vs a few hundred for text) | M | Region crops on request; never the primary path |
| Voice as an architectural tier | **REJECT / REPLACE** | — | — | — | — | Replaced by "edge client feeding text" |
| Multiple specialist agents (N agents/swarm) | **REJECT / REPLACE** | — | — | H (N processes or serialized turns) | M | Replaced by modes + one session; subagents only as a capped, flag-gated context-isolation tool |
| Per-capability tools/modules (one tool per feature) | **REJECT / REPLACE** | — | — | M (prompt pressure) | M | Replaced by ~6 tools over 7 primitives + skills |
| Site-specific page adapters | **REJECT / REPLACE** | — | — | H (per-site maintenance) | M | Replaced by generic extraction + page-type output shaping |
| Page content in the system prompt | **REJECT** | — | — | H (cache invalidation + permanent context) | H | — |
| Full DOM streaming to the model | **REJECT** | — | — | H | H | Snapshot + delta, budgeted, browser-side |
| `context`-hook per-call page injection / `sendMessage` for observations | **REJECT** | — | — | H (whole-conversation clone; durable context growth) | M | Decision payloads at the `prompt()` site; `appendEntry` for observations |
| Pi session tree as NAVI memory | **REJECT** | — | — | M | M | cwd-keyed, immutable, compaction-mutated |
| MCP as NAVI's integration bus | **REJECT** | — | — | M | M | Pi deliberately has no MCP; use tools/skills |
| Forking Pi (source-level) for any NAVI capability | **REJECT** | — | — | H (maintenance) | L | MIT, but 10 releases in ~7 weeks, ~122.6k lines of tests, Ubuntu-only CI |
| Building on the experimental remote harness / `AgentHarness` | **REJECT** | — | — | — | — | Dev-only, POSIX-only, unpublished, mid-rewrite with documented live bugs |
| Always-on microphone / wake word | **REJECT** | — | — | M | H | Ambient mic + attacker-controlled pages is unacceptable |
| Always-resident daemon (default) | **REJECT** (warm mode is opt-in) | — | — | M | L | Residency buys spawn latency only; cache expires at 5 min anyway |
| Auto-update of components in v1 | **REJECT** (defer) | — | — | — | M | Design the update story after the first release |

---

## Minimal NAVI Primitives

The investigation's most useful negative result: NAVI does not need 15 features, it needs **seven primitives** — and each maps to a distinct owner and a testable unit. Everything above is a composition of these.

| # | Primitive | Owner / where it runs | What it is (concretely) | Unlocks |
|---|---|---|---|---|
| **P1** | **Perception** | Browser (content script → background) with bridge-side budgets | Page identity (URL, title, doc version, type), DOM-derived accessibility projection, viewport/focus slice, selection, tab/navigation events; snapshot-authoritative + hash-gated deltas; first-pass redaction at capture | Every "what am I looking at"-class capability; ambient awareness; action verification |
| **P2** | **Context Assembly** | NAVI core (bridge) | Semantic projection → relevance selection → taint label → token/byte budget → cache-stable decision payload at the `prompt()` site; plus one on-demand page tool (`outline / read / find / section / region`) with Pi-style continuation | "What am I looking at", explain/compare/verify, research inputs, Point→Ask |
| **P3** | **Agent Runtime Port** | NAVI core | One interface with two adapters: `SdkAgentPort` (in-process `createAgentSession`, NAVI `ExtensionUIContext`, inline extension factories) and `RpcAgentPort` (`pi --mode rpc` child) for isolated lanes; owns session lifecycle, rebinding, tool stubs, model policy | All reasoning; isolation for risky work; a migration path between substrates |
| **P4** | **Memory** | NAVI core (store) + Pi extension (reader) | NAVI-owned SQLite: visits, pages, pins, topics, source ledger, session slices; explicit-first capture; structured + FTS retrieval; retention and forget; never stored in Pi sessions | "Remember this", recall, per-site context, research continuity, audit |
| **P5** | **Capability Engine** | NAVI core + browser (consent UI) | Scope (origin/eTLD+1) × Verbs (read/navigate/act.submit/act.irreversible/auth.use/clipboard/file/...) × Mode (deny/confirm-each/confirm-once+TTL/allow) × Taint (page-derived content may not trigger irreversible verbs); fail-closed; audit log | Safe RESEARCH and CONTROL; user trust; regulatory hygiene |
| **P6** | **Budget Governor** | NAVI core | Token/cost/step/wall-clock caps derived from Pi's usage + `getContextUsage()` (with the documented post-compaction `tokens: null` branch), cache-waste metric, local heuristic gate, kill switch (`ctx.abort()`), ambient call rate cap | Any autonomous or ambient behavior that spends money |
| **P7** | **Presence** | Surfaces (extension panel/badge, later desktop shell, optional terminal) fed by the core's event hub | Idle state, summon, answer presentation, progress ledger, notifications, graph views; publish whole snapshots, one author per session, thin views | The product itself; multi-surface without duplicated state |

**Why these boundaries.** Each primitive has one owner and one process boundary, so each is separately testable and replaceable; none duplicates a Pi capability (P3 is explicitly an adapter, not a second agent loop); and the failure of any one is diagnosable without a rewrite. The `AgentPort` split is what keeps the contested substrate decision reversible.

**What is deliberately *not* a primitive:** a UI toolkit, a plugin ecosystem, a voice tier, a graph engine, a second agent framework, a credential vault. Each of those either belongs at the edge or is a composition of the seven.

---

## Architecture A — Pi-Native / Minimal

**Thesis:** NAVI is a Pi extension package (plus skills and prompt templates) and a small browser bridge; Pi is the host; the user runs Pi.

**Components**
- Browser extension (content script + background) → native-messaging host → a socket or stdio link → a NAVI extension loaded inside the user's Pi process (Pi has no listener, but extensions do: they run in-process with full Node privileges, `loader.ts:501-513`).
- The NAVI extension registers tools (`navi_page`, `navi_memory`, …), a `tool_call` gate, commands, and durable state via `appendEntry`.
- Memory: `appendEntry` + files under the project/agent dir. No dedicated store.
- UI: Pi's TUI (the user's terminal) — no NAVI-owned surface.

**Data flow:** page → content script → background → native host → socket → Pi extension → context/tool → model → TUI.

**Process boundaries:** browser extension (browser-managed) + native host (browser-spawned) + the user's Pi process. No NAVI-owned long-lived process.

**Pi integration points:** extension API entirely; no SDK, no RPC.

**Strengths:** lowest infrastructure; zero lifecycle code; best update compatibility (extensions are the best-shimmed surface); reuses Pi's packages, skills, evals, and the user's existing auth/models; fastest possible spike (Agent 3's `ssh.ts`-style template).

**Weaknesses:** no product UI (extension rendering is `pi-tui`-typed; RPC degrades it, `rpc-mode.ts:195-311`); ambient behavior only exists while the user has a Pi session open; memory is weak (appendEntry is per-session and cwd-adjacent); policy runs inside the same unsandboxed process as every other extension (silent first-wins tool shadowing, `runner.ts:501-511`); no budget governor host; no multi-surface; conflicts with the user's coding sessions; `ctx` staleness footguns across `/reload` and session replacement.

**Update resilience:** highest — but the extension API has a scheduled compat removal (`loader.ts:59-62`) and tightened tool-schema validation, so even A is not maintenance-free.

**Failure modes:** socket lifecycle races; extension tool/command collisions; stale captured session objects; no lifecycle owner when the browser is closed.

**Verdict:** excellent **Stage 0/1 spike** and a permanent "power user mode"; not the product. Adopt it as the *policy layer inside the Pi process* in Architecture B.

---

## Architecture B — Embedded NAVI (Recommended)

**Thesis:** NAVI is its own application. A local NAVI core (Node 22) embeds Pi through the SDK as its reasoning engine; the browser extension owns perception, actions and consent; the core owns lifecycle, budgets, permissions, memory and presentation. Pi is a dependency, not a platform.

```
┌──────────────────────────── Browser (Chromium first; Gecko second) ─────────────────────────────┐
│  content script (untrusted, least privilege)   background SW (privileged)   trusted UI pages     │
│   DOM/a11y projection, redaction              coalesce, hash, queue,        panel, badge, consent │
│                                               store buffer, policy hints    (never in-page)      │
└──────────────┬───────────────────────────────────────────────────────────────────────────────────┘
               │ native messaging (stdio JSONL, versioned)  ·  fallback: WS 127.0.0.1 + token + Origin
┌──────────────▼───────────────────────────────────────────────────────────────────────────────────┐
│  NAVI CORE (single process, idle-exits ~120 s; spawned lazily by the browser host)                │
│  ┌────────────┐ ┌──────────────┐ ┌───────────────┐ ┌───────────┐ ┌──────────┐ ┌────────────────┐ │
│  │ Perception │ │ Context      │ │ Capability    │ │ Memory    │ │ Budget   │ │ Presence /     │ │
│  │ pipeline   │ │ Assembly     │ │ Engine + audit│ │ SQLite    │ │ Governor │ │ event hub      │ │
│  └─────┬──────┘ └──────┬───────┘ └──────┬────────┘ └─────┬─────┘ └────┬─────┘ └───────┬────────┘ │
│        │               │                │                │            │               │          │
│  ┌─────▼───────────────▼────────────────▼────────────────▼────────────▼───────────────▼────────┐ │
│  │ AgentPort (one interface)                                                                    │ │
│  │   ├── SdkAgentPort  → Pi AgentSession in-process (primary)                                   │ │
│  │   └── RpcAgentPort  → `pi --mode rpc` child (isolated research/actions lane)                 │ │
│  └──────────────────────────────────────────────────────────────────────────────────────────────┘ │
└──────────────┬───────────────────────────────────────────────────────────────────────────────────┘
               │ Pi SDK / stdio RPC            (provider traffic goes to the model API directly)
        ┌──────▼──────┐
        │  Pi (stock) │  AgentSession · sessions · tools · providers · compaction · extension hooks
        └─────────────┘
```

**Key decisions (each with its evidence)**

1. **One `AgentSession` in the core, created via `createAgentSession()`** with: `cwd` = a NAVI-owned directory; `agentDir` under NAVI's config (`~/.navi/pi-agent` / `%LOCALAPPDATA%\navi\pi-agent`); `tools` = an explicit allowlist; `customTools` = NAVI's bridge stubs; `sessionManager` = NAVI-owned session dir (or `inMemory` for ephemeral asks); `settingsManager` = created with **`{ projectTrusted: false }`** (the SDK default is `true` — `settings-manager.ts:374`); `resourceLoader` = a NAVI `DefaultResourceLoader` with extension factories (no project discovery); `modelRuntime` = shared, pinned models. `[verified]`
2. **A NAVI extension loads inline** (`extensionFactories`) into that session for the things only the extension API reaches: the `tool_call` gate (defence in depth), `appendEntry` state, tools/commands, and `ui_prompt_*` signalling. Its dialogs route through a NAVI `ExtensionUIContext` (mode `"rpc"`) into the browser panel. `[verified]`
3. **Page context enters as a decision payload at the `prompt()` call site — not the `context` hook, not `sendMessage`.** Rationale: the `context` hook costs a whole-conversation `structuredClone` per provider call today (`runner.ts:1034-1036`) and is invisible to Pi's compaction threshold and usage meter; `sendMessage` persists into context *and* disk (`agent-session.ts:1537-1547`). Observations go to `appendEntry` (durable, out of context) or NAVI's store. `[verified]`
4. **Perception runs browser-side, budgeted bridge-side.** Content script produces a redacted DOM-derived accessibility projection; background coalesces (navigation 0 ms; DOM/network 250 ms; scroll/selection 500 ms drop-intermediate), hash-gates, and sends snapshots + advisories; bridge prunes to tier budgets and assigns taint. No full DOM, no full AXTree, no screenshots unless asked. `[inferred from verified mechanisms]`
5. **Memory lives in NAVI's SQLite store** (WAL, `busy_timeout`, one writer). The Pi extension can read NAVI memory but never writes it. Pins/promotions are explicit; ambient capture is metadata-only by default. `[verified constraints]`
6. **Capability enforcement lives in the core and the browser**, with the in-process `tool_call` hook as an additional fail-closed layer. Consent renders only in trusted extension pages. `[verified: Pi has no permission system; RPC `confirm` fails closed, `select` does not]`
7. **Lifecycle:** the browser's native-messaging host spawns the core on first message and holds a control attachment; the core idle-exits ~120 s after the last event on an **unref'd** timer; optional warm mode keeps it for a user-set window; suspend/resume is exit + resume from store; crash recovery = respawn + session continuity (Pi's JSONL + NAVI store), with the supervisor as sole writer. `[inferred from Pi's coordinator precedent, verified]`
8. **Tools exposed to the model** (all thin stubs into the core, all budgeted and permission-checked there): `navi_page` (outline/read/find/section/region), `navi_remember`, `navi_recall`, `navi_fetch` (stage 5), `navi_act` (stage 6). Namespaced `navi_*` to make shadowing detectable. `[verified: first-wins silent collision, `runner.ts:501-511`]`
9. **Compaction policy:** leave Pi's compaction enabled only for long research sessions, budget it via `session_before_compact` (or disable with the setting), and keep authoritative NAVI state outside the transcript. `[verified: disableable; usage attributable]`
10. **The RPC lane** is a second `AgentPort` implementation for isolated/untrusted or long-running work: same NAVI extension file loaded with `-e`, own session file, own process, tree-killed by the core. `[verified: RPC is a declared-stable surface; crash isolation is test-pinned]`

**Strengths:** owns its lifecycle, UI, budgets, memory and policy; uses only declared-stable Pi surfaces; no fork; browser-side semantics avoid a second browser; the `AgentPort` split makes the contested substrate decision reversible; multi-surface comes free once the event hub exists.

**Weaknesses:** the most moving parts of the three architectures (browser extension + core + optional child); the core is a single process that must not block (extension handlers share its event loop); the SDK path requires careful rebinding on session replacement; native-messaging installation is a per-browser story.

**Implementation complexity:** medium-high — but most of it is browser-side plumbing and policy code, not agent code; the Pi integration surface is deliberately four interfaces.

**Maintenance cost:** moderate. Pin the Pi version exactly, run a replay-style upgrade checklist, own cross-platform CI, keep the extension file small.

**Pi-update resilience:** high if the "four interfaces" rule is kept (`createAgentSession` + options; a small `AgentSession` method subset; the extension events actually used; `appendEntry`); low if NAVI starts reading session internals.

**Likely failure modes:** perception payload blow-up (mitigated by budgets + hash gating); consent spoofing (mitigated by trusted-only UI); stale session-bound objects in the extension after reload/session replacement (mitigated by re-resolve-at-use discipline); core event-loop stalls from blocking extension handlers (mitigated by O(1) gate + async I/O with timeouts); native messaging blocked by policy (mitigated by WS fallback).

---

## Architecture C — Isolated / Hybrid (Browser extension + bridge + Pi RPC child)

**Thesis:** maximum separation. The browser extension talks to a bridge; the bridge talks to `pi --mode rpc`; the NAVI UI is its own process; every hop is IPC.

**Components:** extension → (native messaging / WS) → bridge daemon → `pi --mode rpc` child with a NAVI extension loaded via `-e` → model. Optional per-lane children.

**Data flow:** identical to B, but context injection and policy happen through the in-child extension, and the UI consumes the RPC event stream.

**Strengths:** the strongest crash isolation (pinned by `test/rpc-client-process-exit.test.ts`); fail-closed trust default in the child; any language/runtime for the UI; two processes can be updated or restarted independently; per-turn child possible without embedding a large Node app.

**Weaknesses (all verified):** RPC cannot render custom UI (`rpc-mode.ts:228-231`) and degrades widgets/themes; no per-tool invocation, no tool introspection, no `appendEntry`, no custom-message injection, no `importFromJsonl`, no settings/scoped-model mutation (`rpc-types.ts:20-74`); `prompt` responds on acceptance, so outcome tracking needs an `agent_settled` state machine (`rpc-mode.ts:394-416`); no protocol version and no readiness handshake (`rpc-client.ts:132-139` sleeps 100 ms); SIGTERM skips the final stdout flush; one session per child; two undocumented frames drive state; and *policy still has to live in an in-child extension*, so C does not actually move policy out of the Pi process — only the UI and lifecycle out.

**Update resilience:** medium — RPC is declared stable but unversioned, and its semantics changed within patch releases.

**Verdict:** not the primary architecture, but **the right shape for the isolated lane** (Stage 5+ autonomous research, Stage 6 actions) and the fallback if the SDK-host risks materialize. The `AgentPort` abstraction exists precisely so this swap is a configuration, not a rewrite.

---

## Architecture Comparison and Ranking

| Criterion | A — Pi-native minimal | B — Embedded NAVI | C — Isolated hybrid |
|---|---|---|---|
| Ambient lifecycle fit | Poor (requires a user Pi session) | **Best** (lazy spawn, idle exit, own core) | Good (child per need, but daemon-ish bridge) |
| UI freedom | None (TUI only) | **Full** (own surfaces; extension dialogs routed) | Full (own process; but no agent-side UI) |
| Access to Pi internals | Extension subset | **Widest usable** (SDK + extension) | Narrow (RPC contract) |
| Context injection | Extension hooks (paying the `context` clone) | **Decision payload at `prompt()` + `appendEntry`** | In-child extension only |
| Per-tool control from NAVI | Via extension | **Via extension + stubs** | Via in-child extension only |
| Crash isolation | None (same process as Pi) | Medium (core is one process; RPC lane for isolation) | **Best** |
| Coupling to Pi | Low (extension API) | Medium (SDK types) | Low (JSONL) |
| Latency | Lowest | **Low** (in-process) | Higher (two hops + child start) |
| Memory overhead | Lowest | Medium (core + child) | Highest (bridge + child + UI) |
| Windows/Linux | Good (no sockets needed) | **Good** (stdio; named pipes only if we add them) | Good on stdio; bad if it borrows the POSIX-only socket stack |
| Update resilience | Best | High (with the four-interface rule) | Medium (unversioned RPC) |
| Debuggability | Best (one process) | Medium (2–3) | Hardest (3+ and undocumented frames) |
| Maintenance burden | Lowest | Medium | Highest |
| Product completeness | Low | **High** | Medium (policy still in-child) |

**Ranking: B > C > A.** B is the only option that is not disqualifying on any axis the product requires (ambient lifecycle, UI freedom, context control, policy placement, cross-platform), and its main cost — more parts — is managed by the `AgentPort` seam and the four-interface discipline. C is the correct *second* runtime and the fallback. A is the correct *spike* and a power-user mode.

---

## Recommended Architecture

**Take Architecture B, with Architecture C's RPC child as the isolated lane, and Architecture A's extension as the in-process policy layer.** Concretely:

1. **Browser extension is the front door** (XR6). Chromium MV3 first, Firefox/Zen second. Three trust tiers inside it: content script (least privilege), background service worker (privileged), trusted extension pages for panel/badge/consent (never render consent in the page).
2. **`navi-core` is the only component that talks to Pi.** It is spawned lazily by the native-messaging host, idle-exits on an unref'd ~120 s timer, and is the sole writer of both NAVI state and Pi session files.
3. **Pi is embedded via the SDK** (`createAgentSession`) with a NAVI persona (`customPrompt`/system prompt), an explicit tool allowlist, NAVI-owned `SessionManager`/`SettingsManager`/`ResourceLoader`, `projectTrusted: false`, and inline NAVI extension factories. A second `RpcAgentPort` implementation runs the same extension via `-e` for isolated lanes.
4. **Context is assembled in the core**: perception → semantic projection → relevance slice → taint → budget → decision payload appended at the cache-safe tail of the `prompt()`. Observations that must persist go to `appendEntry` or NAVI's store — never `sendMessage`, never the system prompt, never the `context` hook.
5. **Memory is NAVI-owned SQLite**; explicit-first; FTS5-if-present; retention and forget shipped in the same milestone as capture; no vectors until a measured precision trigger.
6. **Policy is four-axis and enforced at the ownership boundary** (bridge for transport-level, browser for consent, extension for defence-in-depth); fail closed everywhere; audit log separate from sessions and redacted.
7. **One agent, roles as modes.** Skills/prompt templates carry capabilities; the governor caps spend; subagents appear only as a flag-gated context-isolation tool after an eval shows lift.
8. **Presentation: browser panel + badge first**, desktop shell (global hotkey, tray, notifications, voice later) as a second consumer of the same event stream; terminal as an optional developer surface. One session owner, one author, snapshots not deltas, `get_entries{since}`-style resync.
9. **Version discipline:** pin `@earendil-works/pi-coding-agent` exactly; add a `get_state` readiness probe (never a sleep); own the LF-only JSONL reader; run an upgrade replay checklist per Pi release.

**Why this beats the alternatives.** It is the only architecture in which NAVI owns what it must own (lifecycle, budgets, memory, policy, presentation) while Pi keeps what Pi is good at (the loop, sessions, tools, providers, compaction). It uses only the two surfaces upstream declares stable, requires no fork, and preserves the option — via `AgentPort` — to move the agent runtime into an RPC child or back without rewiring the product.

**What would change this recommendation.** (a) Measured SDK-host instability, event-loop stalls, or leak growth under a long-lived browser workload → promote `RpcAgentPort` to primary (Architecture C), keeping the same core. (b) Upstream ships the `emitContext` guard, exports `ExtensionBindings`, adds RPC versioning and a readiness frame, and publishes a supported cross-platform server → re-evaluate C as primary, because its isolation becomes cheap. (c) A non-developer install requirement forces a single binary → own the *build* (baked-in extensions, pinned Pi), still not a source fork.

## Pi Changes Required

The answer to "how invasive is this?" is: **not at all**. NAVI v1 is achievable on unmodified Pi at a pinned version. The list below separates what NAVI writes, what it configures, what upstream could improve, and what would require a fork (nothing today).

### None (no core changes)

Every mechanism NAVI needs is public: `createAgentSession` and its options, the `AgentSession` public methods, the extension event/tool/UI API, `appendEntry`, stdio RPC, and the provider abstraction. No patch, no vendored patch set, no internal import.

### Extensions (NAVI-owned code, loaded from NAVI)

One small extension file (plus optional companions), loaded inline via `extensionFactories` in the SDK host and via `-e <path>` in the RPC lane:

| Piece | Purpose | API used |
|---|---|---|
| `tool_call` gate | Policy enforcement/defence-in-depth for every tool call; fails closed | `pi.on("tool_call")` returning `{block, reason}` |
| Tools | `navi_page`, `navi_remember`, `navi_recall` (then `navi_fetch`, `navi_act` at later stages) — thin stubs into the core | `pi.registerTool` |
| Commands | `navi:*` user commands (status, permissions, forget, resume) | `pi.registerCommand` |
| Durable state | Observations, consent decisions, ledger snapshots — out of LLM context | `pi.appendEntry` + `registerEntryRenderer` (TUI only) |
| Compaction hook (optional) | Preserve the active page anchor and source ledger through compaction; or disable auto-compaction for ambient sessions | `pi.on("session_before_compact")`, auto-compaction setting |
| Prompt signalling | Route NAVI questions/consents into the browser surface | `ctx.ui.*` through NAVI's `ExtensionUIContext`; `ui_prompt_start/end` |

Rules: do not register a `context` handler; do not use `sendMessage` for observations; do not hold session-bound objects (re-resolve at use); namespace everything `navi_*`/`navi:*`.

### SDK-level integration (configuration, not modification)

| Setting | Value / rationale |
|---|---|
| `cwd` / `agentDir` | NAVI-owned directories (`~/.navi/...`, `%LOCALAPPDATA%\navi\...`) so NAVI never mixes with a user's coding sessions |
| `projectTrusted` | **Explicitly `false`** — the SDK default is `true` (`settings-manager.ts:374`), which would load project settings/packages unprompted |
| `sessionManager` | NAVI-owned session directory, one session per user-visible task; `inMemory` acceptable for ephemeral asks |
| `resourceLoader` | A NAVI `DefaultResourceLoader` with extension factories only; no project discovery, no packages |
| `tools` | Explicit allowlist; never the default four |
| `customTools` | The NAVI tool set |
| `modelRuntime` / models | Pinned, shared; auxiliary model for summaries/embeddings decisions made explicitly |
| Session replacement | Central `bindSession()` helper: re-bind extensions and re-subscribe after `newSession`/`switchSession`/`fork` (documented footgun, `docs/sdk.md:163-180`, `examples/sdk/13-session-runtime.ts:38-50`) |
| Migrations | SDK path does not call `runMigrations` (`main.ts:649`, not exported); NAVI creates fresh sessions and treats migration as a startup check to be added when upstream exports it |

### Potential upstream Pi changes (optional, ordered by value)

1. **Guard the `context` clone** — `runner.ts:1034-1036` deep-clones the whole message array before checking whether any handler exists; `sdk.ts:343-349` shows the correct `hasHandlers` pattern. This is a five-line fix that benefits every extension user and removes an unconditional hot-path cost. **File it regardless of NAVI.**
2. **Export `ExtensionBindings`** — `docs/sdk.md:167` tells SDK hosts to call `bindExtensions`, but the type is not exported from `src/index.ts` (structural typing saves callers for now).
3. **Resolve project trust in the SDK path** — or default it to false; today the SDK fails open while the CLI and RPC fail closed.
4. **Export/call `runMigrations`** in `createAgentSession`.
5. **Honour `ENV_SESSION_DIR`** in `getDefaultSessionDirPath` (`session-manager.ts:476-481`) as `main.ts:671` does.
6. **RPC: add a protocol version field and a readiness frame** — removes the two riskiest RPC workarounds (silent skew, 100 ms sleep in the shipped client).
7. **RPC: flush stdout on SIGTERM** (`rpc-mode.ts:728-745` currently skips the flush).
8. **Document `entry_appended` / `thinking_level_changed`** frames, which are streamed (`json-event.ts:18,46-61`) but absent from the docs table.

None of these blocks NAVI; 1–5 are cheap, upstream-friendly, and each removes a real footgun for other integrators too.

### Fork-only changes

**None required.** A *build-level* fork (own the bundle, bake in NAVI extensions, pin Pi) becomes justified only if NAVI must ship as a single installer-friendly artifact with no `pi install` step for non-developers. Even then the recommendation is to own the build, not to diverge the source. If a source-level fork is ever proposed, the checklist should be: (a) the missing abstraction is upstream-incompatible, (b) upstream declined or cannot take it, (c) the fix cannot be done in NAVI's extension/SDK layer, (d) the maintenance budget for merging a ~122.6k-line test suite and 10-releases-in-7-weeks cadence is explicitly accepted. None of today's requirements passes (a).

---

## Cross-Platform Strategy (Windows and Linux)

**Shared invariants.** stdio everywhere on the hot paths (no listening sockets by default); LF-only JSONL with a fuzz test for U+2028/U+2029 in string fields; unref'd idle timers; explicit process-tree kill; exit-and-resume as the universal suspend path; per-OS CI in NAVI (Pi's own suite is Ubuntu-only).

| Concern | Windows | Linux |
|---|---|---|
| Extension → bridge | Native messaging host registered under `HKCU\Software\<vendor>\<browser>\NativeMessagingHosts\<name>` with a JSON manifest; WS fallback identical to Linux | Native messaging manifest at the browser-specific path (`~/.config/<browser>/NativeMessagingHosts/`, etc.); WS fallback |
| NAVI-owned IPC (core ↔ other surfaces, later) | Named pipe `\\.\pipe\navi-<sid>`; **do not** reuse Pi's UDS stack (throws on Windows, `client/src/unix.ts:38,99`) | Unix domain socket under `$XDG_RUNTIME_DIR`, mode 0600 |
| Process management | `taskkill /F /T /PID` from the trusted System32 path (Pi's pattern); no SIGHUP (Pi skips it on win32), no SIGSTOP/SIGCONT; re-arming stdio idle-grace pattern for inherited-handle hangs | `process.kill(-pid, SIGKILL)` process groups; SIGTERM first with a 5 s grace |
| Suspend/resume | Exit + resume from store (no OS suspend plumbing) | Same; SIGSTOP optimisable later, never required |
| Autostart | None by default; later opt-in `HKCU\...\Run` or a scheduled task | None by default; later opt-in `systemd --user` unit (socket activation possible) |
| Storage | `%LOCALAPPDATA%\navi` (state), `%APPDATA%\navi` (config) | `$XDG_DATA_HOME/navi`, `$XDG_CONFIG_HOME/navi`, sockets under `$XDG_RUNTIME_DIR` |
| Watchers | `fs.watch` recursive works | inotify with explicit bounded directory sets; no recursive `fs.watch`; no 1 s polling loops (Pi's git `watchFile` is an anti-pattern to copy) |
| Clipboard | Native helper available (prebuilt, optional) | X11 only via Pi's helper; **no native clipboard under pure Wayland** — feature-detect and degrade |
| Shell/tooling | NAVI's tools are bridge stubs, not shell; Pi's bash tool needs Git Bash discovery if ever used | `/bin/bash`, standard |
| CI | `windows-latest`: unit tests + native-messaging/pipes integration + kill-tree + idle-exit tests | `ubuntu-latest`: the same suite plus UDS tests |
| Measurement | `typeperf "\Process(navi-core)\% Processor Time"`; `(Get-Process -Id <pid>).WorkingSet64`; `powercfg /energy` | `pidstat -p <pid> 1 60`; `ps -o rss= -p <pid>`; `powertop` wakeups |

**Browser matrix.** Chromium (Chrome/Edge/Brave) first: MV3, `sidePanel`, `scripting`, `activeTab` defaults, `chrome.debugger` optional and experimental (persistent infobar, detaches when DevTools opens, absent in Firefox). Firefox/Zen second: WebExtensions with `sidebar_action`, same native-messaging model, different a11y/debugger capabilities; Zen installs AMO-signed extensions only `[external]`. One internal signal schema, per-browser adapters.

---

## Performance Strategy

### Activation lifecycle

```
idle (0 NAVI processes)
  └─ browser sends first native message ─▶ native host spawns navi-core (cold start budget ≤ 250 ms p50)
        └─ navi-core lazily: opens store (SQLite WAL), starts AgentPort (Pi SDK init ≤ 1.5 s p50 to first prompt ready)
              └─ user asks ─▶ perception payload ─▶ prompt ─▶ stream ─▶ panel
        └─ 120 s without events (unref'd timer) ─▶ flush store, dispose session, exit
  └─ optional "warm" mode (user-set, default off): keep core alive ≤ 30 min while the browser runs
  └─ RPC lane: spawned per isolated task, killed on completion or idle, never resident
```

No component is resident at idle. Process residency is not used as a cache strategy: the provider prompt cache expires in 5 minutes (`cache-stats.ts:8`), and a respawned session with a byte-identical prefix can hit the cache anyway.

### Budgets (targets; every number must be measured before it is trusted)

| Component | Metric | Target | How measured |
|---|---|---|---|
| Browser extension | Idle CPU / memory | < 0.1 % of a core; < 8 MB per content script; worker parked | `chrome://extensions` worker state; Chrome Task Manager |
| Extension → bridge | Event rate at rest / burst | ≤ 4 msg/s; ≤ 50/s coalesced | counter exported over the bridge |
| `navi-core` | Cold start → ready | p50 ≤ 250 ms, p95 ≤ 400 ms | `performance.now()` span, logged as a metric |
| `navi-core` | Idle RSS / CPU | < 40 MB; < 0.3 % of a core; 0 resident processes at idle | `ps`/`Get-Process`; `pidstat`/`typeperf` |
| `navi-core` | Event queue bound | ≤ 1,000 items with drop counter and `resync_required` | unit test + status frame |
| Pi runtime (SDK init or RPC child) | Ready → first prompt | **p50 ≤ 1.5 s, p95 ≤ 2.5 s** — measure first with `npm run profile:rpc -- --runs 10 --warmup 2` (`METRIC startup_time_ms`) | Pi's own profiler; `PI_TIMING=1` for phase breakdown |
| Pi runtime | RPC round trip | p50 ≤ 15 ms, p95 ≤ 40 ms | harness over `get_state` |
| Context | Injected page tokens per turn | ≤ 1.5k steady state; hard cap 4k | NAVI ledger + `usage.input` deltas |
| Context | Cache hit ratio | ≥ 90 % of steady-state prompt tokens from cache | reuse `computeCacheWaste` semantics |
| Memory | Index lag / storage | p95 ≤ 5 s; < 200 MB after 30 days typical use; explicit content only | timestamps + `du`/`Get-ChildItem` |
| Whole system | Idle CPU / RSS | < 0.5 % of a core; < 120 MB (bridge only) | per-process sums |
| Whole system | User action → first token | p50 ≤ 2.5 s cold, ≤ 1.0 s warm | instrument the panel |

### Token and context strategy

- **Tier ladder:** T0 identity + addressable outline (120–250 tokens, always available); T1 viewport/focus projection (300–900); T2 query-relevant sections (800–2,500, on demand); T2Δ same-page delta (40–300, hash-gated to zero when unchanged). Hard cap 4k/turn.
- **Off-screen content is tool-only** (`navi_page` with Pi-style offset/limit continuation), never inlined speculatively.
- **Append-only, cache-safe:** the system prompt and tool schemas stay byte-stable; volatile page data is appended at the tail of the prompt, after the provider cache breakpoint; identical content is serialised identically (no timestamps in cached regions).
- **NAVI owns the ledger** for injected context, because Pi's `getContextUsage()`/`shouldCompact` derive from session entries and cannot see payloads that never became entries.
- **Compaction policy:** budgeted via `session_before_compact`, or disabled per session; never relied on to preserve page facts (summaries cap tool results at 2,000 chars).
- **Ambient spend gate:** local heuristics first; at most one ambient LLM call per 5 minutes (configurable) and one in flight; ambient calls are the first thing dropped under pressure.
- **No warm-streaming:** if the user is idle > 5 minutes, there is nothing to keep warm.

### IPC and events

Batching windows: navigation 0 ms; DOM/network 250 ms; scroll/selection 500 ms drop-intermediate; clip/OCR on explicit intent only. Every hop has bounded queues, size caps, sequence numbers, and a `resync_required` signal. Framing is LF-only JSONL (never `readline`). The core's own entry graph stays thin (no TUI/theme/export imports), enforced by an entry-graph budget check modelled on Pi's `check-entry-graphs.mjs`.

---

## Security & Privacy Model

### Trust boundaries

```
UNTRUSTED ────────────────────────────────────────────────────────────────────────────────────────────
  Web page  ──▶  content script (isolated world, DOM read/write, no secrets by construction)
                     │ structured messages only, redacted at capture
TRUSTED-ISH ─────────┼───────────────────────────────────────────────────────────────────────────────
                     ▼  background service worker (privileged: tabs, scripting, storage, bridge socket)
                        consolidated view; policy hints; no page content beyond the projection
LOCAL ────────────────┼───────────────────────────────────────────────────────────────────────────────
                        native host ─▶ navi-core (policy engine, budgets, memory writer, audit)
                                           │ exact pinned SDK; extension loaded from NAVI only
                                           ▼
                                        Pi session ──▶ LLM provider  (page-derived tokens are tainted data)
LOCAL (files) ── NAVI store (encrypted-at-rest option), audit log (redacted, hash-chained), Pi sessions
OS ── user account boundary (no sandbox; containerisation optional for risky lanes)
```

### Capability model (replaces OBSERVE/RESEARCH/CONTROL as the policy unit)

| Axis | Values |
|---|---|
| **Scope** | none / this origin / eTLD+1 / explicit per-origin grant with TTL |
| **Verbs** | ~16 enumerated: `observe.read`, `observe.selection`, `observe.screenshot`, `research.fetch`, `research.search`, `navi.navigate`, `act.click`, `act.type`, `act.submit`, `act.irreversible`, `auth.use-credential`, `sys.clipboard.read/write`, `sys.file.read/write`, `sys.shell` |
| **Mode** | deny / confirm-each / confirm-once + TTL / allow (allow only for observe/research verbs) |
| **Taint** | content derived from `page:*` may never trigger `act.submit`, `act.irreversible`, `auth.use-credential`, or `sys.*` without a fresh, literal, user-typed confirmation naming the target |

Presets: **OBSERVE** (observe + read, research.fetch per prompt), **RESEARCH** (adds search/fetch with domain policy), **CONTROL** (adds act verbs, all confirm-each or confirm-once with TTL, irreversible verbs never retained). Enforcement points: browser (what may be captured / rendered / acted on), bridge (what may cross to Pi; what may leave it), extension `tool_call` hook (defence in depth, fail closed, loaded last so it sees mutated inputs).

### Untrusted content rules

- Page content enters prompts fenced and labelled (`<page_context trust="untrusted_data" source="...">`); it is never concatenated into the system prompt and never persisted as a durable instruction.
- **Structural "never capture" list** enforced at the content script, not by regex later: password/OTP/credit-card fields, form values of type password, auth tokens in DOM, cookies/headers/storage, clipboard (unless the user explicitly asks), cross-origin iframe contents, browser-internal pages, incognito, and pages the user marked private.
- Sensitive-page auto-downgrade (banking, payment, auth, webmail, health): no ambient capture beyond identity; explicit ask still permitted.
- Prompt injection is treated as an accepted risk that cannot be eliminated; the mitigations are volume reduction (budgets), provenance, taint gating, no credentials in the model path, trusted-only consent UI, and a hard default-deny on irreversible verbs.
- Confirmations render only in trusted extension pages (a web page can draw a pixel-perfect fake); timeouts deny; only `confirm`-style prompts (which fail closed) are used for approvals — never `select`.
- Audit: redacted, hash-chained JSONL under NAVI's config dir, separate from Pi sessions; logs *that* something happened and *which* verb/scope, never the page content. Not claimed to be tamper-proof.
- Bridge: native messaging preferred (no listening port); WS fallback bound to 127.0.0.1 with a per-launch 256-bit token, `Origin` check, no CORS, fail closed.
- Plugins: no third-party Pi packages/extensions loaded into the core session; NAVI's extension only. Skills are instructions and are user-reviewed before install (Pi's own guidance).

### Threat table (condensed)

| Asset | Adversary / vector | Mitigation |
|---|---|---|
| Credentials, tokens, private messages | Malicious page content exfiltrated through the model | Structural never-capture; taint; budgets; no credential use by the model |
| User's actions (click/submit/pay) | Prompt injection from page text | Taint gating; trusted-UI confirmation; per-origin scope; irreversible verbs never auto-allowed |
| Browser session | Port hijack / DNS rebinding on the fallback bridge | Native messaging primary; token + Origin; loopback bind; no CORS |
| Memory store | Covert capture, over-retention, leak via share | Explicit-first; metadata-only default; retention + forget; `/share` exclusion |
| Audit / logs | Content leakage into logs | Redaction; hash-chained metadata-only audit; separate from sessions |
| Host machine | Compromised dependency or extension | Exact version pin + shrinkwrap; NAVI-owned extension only; containerised lane for risky work |
| User trust | Silent automation, fake consent UI | No hidden automation; consent only in trusted surfaces; capability inspector UI |

---

## Development Stages

Each stage exists to test a specific architectural assumption. A stage is not "done" until its exit criteria are measured, and any stage may be cancelled by its own evidence.

### Stage 0 — Architecture validation (days, no product surface)

- **Objective:** measure the numbers the whole design rests on.
- **Assumptions tested:** Pi SDK/RPC cold start fits the latency budget; the deployed stack is idle at idle; the perception projection fits the token budget; cache-stable tail-append works in practice; native messaging is installable on the target browsers.
- **Components:** throwaway harnesses only (Pi profiler, a fake extension, a page corpus).
- **Deliverable:** a measurements note with numbers against the budget table.
- **Dependencies:** none.
- **Risks:** SDK init may be slower than assumed; native messaging may be policy-blocked.
- **Measurements:** `npm run profile:rpc -- --runs 10 --warmup 2`; SDK `createAgentSession`→first-prompt timing; `PI_TIMING=1` phase breakdown; a11y/DOM projection byte/token counts over ~20 representative pages; `computeCacheWaste` deltas with tail-appended context; idle process/RSS after 10 minutes.
- **Test strategy:** scripted, repeatable, recorded; no product code.
- **Exit criteria:** every target is either met, revised with justification, or escalated as a blocking finding. (This stage can invalidate the substrate choice — that is the point.)

### Stage 1 — Perception prototype (OBSERVE only, no LLM)

- **Objective:** produce the semantic projection from a real browser, under budget, with redaction.
- **Assumptions tested:** browser-side projection is sufficient for "what am I looking at"; snapshot+delta is cheap; capture redaction is structural.
- **Components:** P1 perception, the bridge transport, a debug viewer (no Pi yet, or a trivial one-shot prompt).
- **Deliverable:** extension + bridge that stream a redacted projection for the active tab and expose a `page` outline.
- **Dependencies:** Stage 0 (transport + budget).
- **Risks:** MV3 worker suspension; per-page-type extraction gaps; over/under-redaction.
- **Measurements:** extraction latency, bytes/tokens per page type, event rate, false-capture rate on a redaction test suite.
- **Test strategy:** unit tests for the extractor/redactor; fixture pages; a "no continuous DOM" assertion.
- **Exit criteria:** projection ≤ budget on the corpus; no sensitive-field capture in tests; zero background streaming when the page is unchanged.

### Stage 2 — Context intelligence

- **Objective:** turn the projection into budgeted, tainted, cache-stable decision payloads plus an on-demand page tool.
- **Assumptions tested:** the tier ladder answers real questions; deltas are stable; the model behaves correctly with fenced untrusted content.
- **Components:** P2 context assembly, P6 budget governor (first cut), Pi adapter (SDK), page tool.
- **Deliverable:** a CLI/browser test harness where typed prompts get page-grounded answers with measured token costs.
- **Dependencies:** Stage 1.
- **Risks:** relevance scoring quality; prompt-cache invalidation; injection compliance.
- **Measurements:** injected tokens/question, cache-read ratio, answer grounding on a question set, injection-suite compliance rate.
- **Test strategy:** fixture question set with expected facts; adversarial pages with embedded instructions; token-budget assertions.
- **Exit criteria:** ≤1.5k injected tokens typical, ≤4k worst case; cache ratio ≥90 %; injection suite shows zero policy-violating actions.

### Stage 3 — Ambient ask (first user-facing surface)

- **Objective:** the browser panel + badge; "What am I looking at?" and selection explain, with no copy/paste.
- **Assumptions tested:** the interaction model (summon, not chat); the user-facing latency budget; one session owner.
- **Components:** P7 presence (panel/badge), P3 agent port, P5 capability engine (OBSERVE preset), consent UI skeleton.
- **Deliverable:** installable extension + local core; ask about the current page and selection.
- **Dependencies:** Stages 1–2.
- **Risks:** latency; panel scope creep; permission UX confusion.
- **Measurements:** first-token latency cold/warm; answer usefulness in a small user test; idle footprint with the panel open.
- **Test strategy:** end-to-end scripted asks; latency instrumentation; idle measurement.
- **Exit criteria:** p50 first token ≤2.5 s cold/≤1.0 s warm; testers prefer it to copy-paste for the target questions; the null-hypothesis comparison is run (see First Prototype).

### Stage 4 — Memory v1

- **Objective:** explicit-first memory: remember/pin/recall/forget, recent context, source ledger.
- **Assumptions tested:** explicit pins plus FTS is sufficient for the target recall questions; retention is operable; capture loss is bounded.
- **Components:** P4 memory, `navi_remember`/`navi_recall`, panel memory views, retention/forget commands.
- **Deliverable:** memory store with inspect/delete UI and tool access.
- **Dependencies:** Stage 3 (a place to remember from), Stage 1 (capture path).
- **Risks:** wrong-entity merges; stale recall; storage growth; privacy perception.
- **Measurements:** retrieval precision@3 on a labelled 100-query set; storage/day; delete completeness; recall latency.
- **Test strategy:** labelled retrieval set; deletion tests; crash tests on the single-writer store.
- **Exit criteria:** precision@3 ≥0.8 for explicit pins and ≥0.6 for ambient metadata; `forget` verified complete; retention enforced.

### Stage 5 — Research (RESEARCH level)

- **Objective:** bounded, provenance-carrying research from the current page, supporting *and* conflicting sources, visible progress.
- **Assumptions tested:** the governor caps real runs; the source ledger drives a trustworthy dossier; single agent + modes suffices.
- **Components:** `navi_fetch` + search, P6 governor (full), progress ledger in P7, RESEARCH preset in P5, optional RPC lane.
- **Deliverable:** `/navi:research <question>` producing a dossier with citations and a cost report.
- **Dependencies:** Stages 2–4.
- **Risks:** cost blow-up; source quality; verdict overconfidence.
- **Measurements:** cost/tokens per investigation; sources found; conflicting-source inclusion rate; eval-harness lift vs baseline.
- **Test strategy:** Pi eval harness cells (baseline vs NAVI) with repetitions; budget-breach tests; domain-policy tests.
- **Exit criteria:** every run terminates within policy; measurable lift on the eval; dossiers include provenance and at least one counter-source when one exists.

### Stage 6 — Controlled actions (CONTROL)

- **Objective:** minimal, safe actions: scroll-to/point, click, type, submit — with the full capability model and undo.
- **Assumptions tested:** per-origin consent is usable; irreversible-action gating holds under injection; checkpoints work.
- **Components:** `navi_act`, consent UI (confirm-only), checkpoint/restore (URL/scroll/form state), audit.
- **Deliverable:** action support behind explicit grants, default-deny, with an audit trail.
- **Dependencies:** Stages 1–5 (perception, governor, memory, capability engine mature).
- **Risks:** irreversible mistakes; consent fatigue; bypasses.
- **Measurements:** approval-bypass red-team results; action success/undo rate; consent friction.
- **Test strategy:** adversarial injection suite, fail-closed tests, kill-switch tests, red-team review before any release.
- **Exit criteria:** zero known bypasses in red-team; every irreversible verb requires fresh confirmation; undo verified for the supported action set.

### Stage 7 — NAVI experience (desktop shell, voice, graphs, multi-surface)

- **Objective:** the ambient product surface: global hotkey, tray, notifications, voice input, graphs, multi-surface sessions.
- **Assumptions tested:** multi-surface adds value without duplicating state; voice-as-edge works; the aesthetic is worth its cost.
- **Components:** desktop shell, voice edge client, graph views over the ledger/session tree, author/viewer/notifier roles.
- **Deliverable:** a desktop presence plus the browser surface sharing one session.
- **Dependencies:** Stages 3–6; user evidence that earlier stages are worth it.
- **Risks:** scope; two UIs drifting; voice privacy; graph vanity.
- **Measurements:** feature usage; latency; idle footprint with the shell running.
- **Test strategy:** multi-surface resync tests; voice privacy review; idle tests.
- **Exit criteria:** surfaces share state without divergence; idle footprint within budget; usage justifies each surface.

---

## First Prototype

**Recommendation:** the candidate in the brief is right — *Pi + browser extension + minimal local bridge*, answering **"What am I looking at?"** — but with a sharper contract and a built-in falsification test.

**Scope (small but technically meaningful):**

- Chromium extension: content script projects page identity + viewport/focus semantic projection + selection; background coalesces and hash-gates; native-messaging host (WS fallback) to a minimal core.
- Core: one embedded Pi `AgentSession` (SDK, pinned), a NAVI extension with one `tool_call` gate and one `navi_page` tool, the tier-T0/T1 context assembly, and a naive budget ledger. Panel UI shows the answer and the injected-context debug view.
- Capability preset: OBSERVE only. No memory, no navigation, no actions, no voice.

**What it must prove:**

1. The answer is correctly grounded in the *current* page — verified on a fixture question set across page types (docs, article, repo, search results, app UI) — **without** copy/paste, **without** the user supplying the URL.
2. The page→prompt payload stays within budget (≤1.5k tokens typical) and is zero-cost when the page has not changed (hash gate).
3. No continuous DOM streaming (measured event rate at rest ≈0), and no browser control paths exist in the build.
4. Page-embedded instructions do not change NAVI's behavior in an adversarial test suite.
5. Cold and warm latency land inside the Stage-0 budgets.
6. **Falsification:** the same questions are also answered by the null hypothesis (extension + local extraction + one direct API call, no Pi). If NAVI cannot beat it on answer quality, follow-up questions, or cost, the harness is not yet justified — stop and fix that, don't build Stage 2.

**What it must not do:** stream the DOM; touch the clipboard; navigate or act; store browsing history; ask the user to install a service/tray; read pages the user didn't open; run when no browser is open.

**Deliverable:** a runnable prototype, a measurements report against the budget table, and a written verdict on the null-hypothesis comparison. Success does not commit us to Stage 2 without the numbers.

---

## Experiments Before Commitment

Cheap prototypes/benchmarks that could invalidate major assumptions, ordered by value:

| # | Experiment | Question it settles | Cost | What would invalidate |
|---|---|---|---|---|
| X1 | Pi SDK/RPC cold start + phase timings (`profile:rpc`, `PI_TIMING=1`) | Is the ≤1.5 s p50 ready budget real? | 1 day | >2.5 s p95 → rework lifecycle/warm mode or trim entries |
| X2 | Page projection corpus (20 pages, 6 types) | Do T0+T1 fit ≤1.5k tokens and answer typical questions? | 2 days | >4k typical → tool-first design, viewport-only default |
| X3 | Prompt-cache behaviour with tail-appended context | Does the cache survive deltas? | 1 day | Any front-of-prompt mutation → redesign assembly |
| X4 | MV3 lifetime + native messaging vs WS on Chrome/Edge/Firefox | Is the transport chain viable per browser? | 2–3 days | Cannot hold a session / policy-blocked → pull model or per-turn host |
| X5 | Injection suite (30 adversarial pages) | Do taint+fencing hold in practice? | 2 days | Any action despite taint → stop action plans |
| X6 | Memory retrieval precision (100 labelled queries, pins+FTS vs vectors) | Is v1 memory sufficient without embeddings? | 2 days | precision <0.6 → reconsider vectors, or narrow the promise |
| X7 | `emitContext` clone cost at 1k–100k-token conversations | How much does the unconditional clone cost, and does upstream take the fix? | 0.5 day | Large → keep no `context` handlers and prioritise the PR |
| X8 | Single-agent vs isolated-context subagent on a 10-source research task (one eval cell, 5 repetitions) | Does research need more than one agent at all? | 1–2 days | Isolated wins decisively → add capped subagents |
| X9 | Idle footprint of the full stack over 30 minutes | Does idle really mean ~zero? | 1 day | Any residual polling/keep-alive → fix before Stage 3 |
| X10 | Null-hypothesis comparison (extension + one API call) | Is the harness justified at all? | 1 day | NAVI loses → re-scope the product before more architecture |

---

## Open Questions

1. **Is the SDK host stable enough for a long-lived browser workload?** XR1 says SDK primary at *medium* confidence; the fix is measurement (X1, plus a soak test). Resolve before Stage 3.
2. **What is Pi's real RPC cold-start on Windows and Linux?** The entire lifecycle budget rests on it; the tooling exists and has never been run for this purpose.
3. **Will `AgentSession` and the experimental `AgentHarness` converge or diverge?** If the SDK is superseded, the `AgentPort` abstraction is the migration path; check `rfc.earendil.com` and the changelog before each upgrade.
4. **How much NAVI value survives if the model is *never* given ambient context?** (Agent 10's highest-leverage unknown.) The prototype's null-hypothesis comparison is designed to answer it.
5. **Native messaging vs WS per browser and per policy environment** — install paths, enterprise blocks, MV3 worker lifetime in the wild.
6. **What exactly is "the current page" by default** — text projection, selection, screenshot, or identity-only — and what is the escalation rule?
7. **What is the default ambient-LLM call rate** (cost/policy question, not engineering), and who sets it?
8. **Where does capture physically run** (content script vs SW vs bridge) — it decides the writer role and the memory schema freeze.
9. **ASR/TTS engine and the local-only requirement** — quality floor vs privacy trade-off for voice.
10. **Retrieval quality without embeddings** — and the precise trigger to add vectors (corpus size + measured precision floor).
11. **Whether `entry_appended`/`thinking_level_changed` frames are safe to depend on**, or NAVI must reconstruct state from `get_entries`/`get_tree`.
12. **Does the target browser permit the CONTROL action set** NAVI would promise (cross-origin input, uploads, clipboard)?
13. **An acceptable per-hour token ceiling**, and whether it means disabling auto-compaction entirely for ambient sessions.
14. **Which observations are worth persisting at all** — without an answer, memory accumulates stale context by design rather than by accident.
15. **The Windows autostart vehicle** (none, Run key, scheduled task) and the installer story in general.
16. **Upstream merge velocity on `agent-session.ts`** — not measurable from the single-commit clone; requires a full clone or the GitHub API.

---

## Rejected Ideas

Each was proposed in the brief or generated during research, examined, and dropped or replaced. Reasons are evidence-linked; this is the list to re-read before re-proposing any of them.

| Rejected | Why (and what replaced it) |
|---|---|
| Source-level fork of Pi | MIT and trivially rebranded (`piConfig`, `config.ts:500-509`), but the fork inherits an 11-package workspace, hand-ordered build, ~122.6k lines of tests, 10 releases in ~7 weeks, and Ubuntu-only CI — for a capability (browser) that does not exist in Pi to fork. Replaced by SDK + extension + optional RPC. Build-level fork deferred to the installer problem only |
| Building on the experimental remote harness / `pi-server` / `pi-client` / `AgentHarness` | Dev-only (`PI_EXPERIMENTAL=1`), POSIX-only (throws on Windows), excluded from the published package, exact-version protocol, and its sibling runtime is mid-rewrite with documented live bugs. Mined for *patterns* (idle retirement, worker lifecycle), not code |
| MCP as NAVI's integration bus | Pi deliberately has no MCP (`README.md:499`); NAVI's own capabilities are tools/skills; an MCP *client* extension is acceptable only for third-party tools |
| Custom Pi overlay/TUI UI as NAVI's surface | Terminal components only; `custom()` returns `undefined` over RPC. Replaced by NAVI-drawn browser/desktop surfaces fed by the event stream |
| `ctx.ui.confirm()` over RPC as the only action gate | A JSON message on a pipe can be answered `true` by any client between; consent must render in a trusted browser surface |
| `context`-hook per-call page injection | Whole-conversation `structuredClone` per provider call today, invisible to compaction accounting, discarded by compaction. Replaced by decision payloads at `prompt()` |
| `sendMessage()` for ambient observations | Enters LLM context *and* disk permanently. Replaced by `appendEntry` + NAVI's store |
| Pi session tree as NAVI memory | cwd-keyed, per-entry immutable, synchronously written, compaction-mutated, no retention, non-excludable from `/share`. Replaced by a NAVI-owned SQLite store |
| Per-observation session writes | Synchronous `appendFileSync` on the event loop; would make NAVI slower than the browser it augments |
| N specialist agents / N RPC children for research | One `AgentSession` per process; no cost cap crosses the process boundary; concurrency already exists inside one turn. Replaced by modes + one session, with subagents as a later, capped, flag-gated tool |
| One tool per capability | Prompt pressure and fragmentation. Replaced by seven primitives and ~6 namespaced tools |
| Site-specific page adapters | Maintenance trap. Replaced by generic extraction + page-*type* output shaping |
| Full DOM streaming / screenshot-first perception | Cost (a screenshot ≈4,800 tokens) and attack surface. Replaced by hash-gated semantic projection, images on demand |
| Page content in the system prompt | Cache invalidation and permanent untrusted context. Replaced by tail-appended, fenced decision payloads |
| Always-on microphone / wake word | Attacker-controlled pages plus a live mic is an unacceptable combination; push-to-talk only, later |
| Voice as an architectural tier | It has no Pi dependency; it is an edge client that emits text into the bridge |
| Always-resident daemon as the default | Residency buys spawn latency only and the provider cache expires in 5 minutes; Pi's own coordinator exits when empty. Warm mode is the opt-in version |
| `readline`-based JSONL reader | Splits on U+2028/U+2029 inside JSON strings — exactly the characters page text contains; the protocol explicitly forbids it |
| Relying on `agent_end` for completion | `agent_end` may be followed by retries/compaction/continuation; `agent_settled` is the correct edge |
| Auto-update in v1 | Pi's own update path needed a Windows exit-path workaround; design the update story after the first release |
| Semantic vectors in v1 memory | Deferred behind a measured precision trigger; FTS + pins first |
| Three privilege levels as the enforcement unit | Fuzzy boundaries produce bypasses. Replaced by Scope × Verbs × Mode × Taint with the three levels as UX presets |
| A NAVI credential vault / secret-injection broker | Rejected in the security research as an unjustifiable liability; the model never holds credentials |
| Screenshot/vision as the default perception path | Text projection is one to two orders of magnitude cheaper and less attack-surface-rich |
| "Semantic browsing history" captured implicitly by default | Privacy and cost; explicit-first capture with metadata-only ambient by default |

---

## Final Recommendation

**Build NAVI as Architecture B.** A browser-extension-fronted, locally-embedded agent harness: the extension owns perception, actions and consent; a lazily-spawned NAVI core owns lifecycle, budgets, memory, policy and presentation; Pi (unmodified, pinned) is embedded via the SDK as the reasoning engine, with stdio RPC as an isolated second lane. Start with OBSERVE-only "What am I looking at?", prove it beats the null hypothesis, and grow capabilities through the seven primitives — not through a feature list. Do not fork Pi. Do not build on the experimental harness. Do not put page content in Pi's system prompt, `sendMessage`, or the `context` hook. Do not make the user choose between NAVI and their browser's speed.

Confidence levels (with the trigger that would move each):

| Decision | Confidence | What would change it |
|---|---|---|
| Browser-side perception; DOM-derived a11y projection; viewport-first slice; budgets before the model | **High** | Corpus measurements showing generic projection cannot answer real questions (X2) |
| Page content is tainted data; capability model as Scope × Verbs × Mode × Taint; consent in trusted surfaces only | **High** | Nothing foreseen; only a browser platform change |
| Memory in a NAVI-owned store; explicit-first; no vectors in v1 | **High** on placement; **Medium** on no-vectors | Retrieval precision below the floor (X6) |
| No fork; SDK + extension + stdio RPC only | **High** | A required capability that provably cannot be expressed (none found) |
| Lazy activation, no always-on daemon | **High** | X1 showing cold start is unusable *and* X9 showing warm mode is cheap enough to justify |
| Browser panel + badge as the first surface | **Medium-high** | If user testing shows the browser panel is the wrong ambient surface (contingent on the browser-centred premise) |
| One agent, roles as modes, no specialist swarm | **Medium-high** | The X8 eval cell showing isolated-context subagents decisively win on research quality |
| **SDK in-process as the primary substrate** | **Medium — the weakest load-bearing call** | SDK host leaks/stalls in a soak test, event-loop contention from extension handlers, or a need for per-turn process isolation → promote `RpcAgentPort` (Architecture C) with no product rewrite |
| The prototype's usefulness as an architecture gate | **High** that it is the right first step | It fails the null-hypothesis comparison — in which case the harness is re-scoped before more is built |

**Immediate next actions (in order):** (1) run Stage 0/X1, X2, X4, X10 — the four experiments that can change the design; (2) file the five upstream fixes as issues/PRs, starting with the `context`-clone guard; (3) build the First Prototype; (4) decide Stage 1 funding only on its measurements. Nothing in this plan requires a decision about NAVI's final feature set, and the architecture is deliberately sized so that most of that feature set can arrive without new primitives.

---

## Appendix — Research Index

| File | Contents |
|---|---|
| `research/01-core-architecture.md` | AgentSession/runtime/lifecycle/events/sessions/compaction/resources/startup-boundary map |
| `research/02-integration-modes.md` | Extensions vs SDK vs RPC vs hybrid vs fork; full comparison table and risk register |
| `research/03-browser-perception.md` | Signal model, Chromium/Firefox/Zen constraints, bridge protocol, budgets, redaction |
| `research/04-context-intelligence.md` | Pi context machinery, tier ladder, tool escalation, cache discipline, budgets |
| `research/05-memory.md` | Pi persistence analysis, store/schema, capture policy, retrieval tiers, storage costs |
| `research/06-security-privacy.md` | Trust boundaries, four-axis permission model, never-send list, threat table |
| `research/07-performance-crossplatform.md` | Startup/idle analysis, process model, IPC per hop, budget table with measurement commands |
| `research/08-ux-interface.md` | Interaction model, surfaces, Pi UI capabilities and their limits, multi-surface rules |
| `research/09-capabilities.md` | Primitive derivation, capability→primitive map, voice design, governor contract |
| `research/10-red-team.md` | Objections, kill list, simplification opportunities, fork analysis, null hypothesis |
| `research/xreview-1…7-*.md` | Independent adjudications of the seven contested decisions in **Key Disagreements** |

*End of document.*

