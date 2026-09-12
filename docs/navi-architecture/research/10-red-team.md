# NAVI Red Team Report (Agent 10 of 10)

**Role:** skeptical architect / red team. Goal is not to kill NAVI but to prevent a fragile system that looks elegant.
**Evidence base:** read-only clone at `C:\Users\lain\NAVI\vendor\pi`, HEAD `71dca871bc80b6bc97be37f0ca3189399d651fff` (2026-09-11), `@earendil-works/pi-coding-agent@0.85.1`. All repo claims below were verified in `src/`, not only in docs, unless tagged otherwise.
**Note on the clone:** it is a single-commit snapshot (`git rev-list --count HEAD` = 1). Upstream history, churn and PR velocity could **not** be measured from this clone; the CHANGELOG was used as a proxy. This is recorded as an uncertainty, not a finding.

Convention: `file path:line → symbol → implication`. Tags: `[verified]` (read in source), `[inferred]` (reasoned from source), `[external]` (from documentation/prose, not code), `[unverified]`.

---

## 1. Repository Findings

### 1.1 Pi is a coding harness, and every one of its context-control mechanisms is tuned for a file tree

- `packages/coding-agent/src/core/session-manager.ts:476-481` → `getDefaultSessionDirPath()` → session storage path is derived from the **cwd** (`sessions/--<cwd with separators replaced>--/<file>.jsonl`). `packages/coding-agent/src/core/sdk.ts:183` → `SessionManager.create(cwd, getDefaultSessionDir(cwd, agentDir))` → cwd is the primary key of the whole persistence model. `[verified]`
  **Implication:** for a browser-aware ambient harness, cwd is a meaningless axis (pages are not directories this process is "in"). Any NAVI design that stores "what the user has been reading" inside Pi sessions inherits a directory-keyed partition and will silently fragment memory per shell cwd.
- `packages/coding-agent/src/core/compaction/compaction.ts:132-136` → `DEFAULT_COMPACTION_SETTINGS = { enabled: true, reserveTokens: 16384, keepRecentTokens: 20000 }` and `compaction.ts:235-238` → `shouldCompact()` → threshold is `contextTokens > contextWindow - reserveTokens`. `[verified]`
  **Implication:** auto-compaction fires an **extra LLM call per threshold crossing**, invisibly, mid-run (`compaction.ts` is driven from `agent-session.ts:1947-2121`). In a long-lived ambient process this is a recurring, unbudgeted cost and a silent quality cliff (the summary replaces detail the harness never sees again).
- `packages/coding-agent/docs/compaction.md:271` (external prose) → tool results are truncated to **2000 characters** during summarization serialization. `[external]` Consistent with `packages/coding-agent/src/core/compaction/utils.ts` behaviour.
  **Implication:** if NAVI routes page content through tool results, the content that survives compaction is capped at 2 KB per result. Semantic page context and compaction are in direct conflict.
- `packages/coding-agent/src/core/tools/truncate.ts:11-12` → `DEFAULT_MAX_LINES = 2000`, `DEFAULT_MAX_BYTES = 50 * 1024` → truncation is a **bytes/lines** policy. `[verified]`
  **Implication:** these are file-shaped defaults. Page text, DOM dumps and accessibility trees are unbounded-line, low-byte-value payloads; reusing these constants will either over-truncate structure or under-truncate a single long line.

### 1.2 Extensions are in-process, unsandboxed, and not a boundary

- `packages/coding-agent/src/core/extensions/loader.ts:501-513` → `loadExtensionModule()` → `createJiti(...).import(extensionPath, { default: true })` → extension TypeScript/JavaScript is executed **in the host process** with full Node privileges. `[verified]`
- `packages/coding-agent/src/core/extensions/loader.ts:395-398` → `api.exec = execCommand` → arbitrary argv execution is a **first-class extension API**, not something an extension must smuggle in. `[verified]`
- `packages/coding-agent/src/core/extensions/runner.ts:269-315` → `class ExtensionRunner` holds live `SessionManager` and `ModelRegistry` references and hands them to every handler via `createContext()` (`runner.ts:723-801`). `[verified]` → there is no capability filtering between extension code and session internals.
- `packages/coding-agent/docs/security.md:31-35` → "**No Built-in Sandbox**" … "A partial in-process sandbox would be easy to misunderstand as a security boundary while still depending on the host shell, filesystem, package managers, credentials, and extension code." `[external, but authoritative: this is the vendor's stated position in `docs/security.md`, and it matches the code]`
- `README.md:41` → "Pi does not include a built-in permission system for restricting filesystem, process, network, or credential access." `[external]`
- `packages/coding-agent/docs/extensions.md:111` → "> **Security:** Extensions run with your full system permissions and can execute arbitrary code." `[external]`
- **Counter-evidence that the vendor knows this is a gap:** `packages/agent/docs/mobile-handoff/02-plugins/02-sandbox/src/membrane.ts:1` → `import ivm from "isolated-vm"` → there is a working proof-of-concept membrane (README claims 412 assertions) **in docs, not in the shipped package**. `[verified for existence]` Its own README (`.../02-sandbox/README.md:103-114`) notes the native ABI matrix is real and that `darwin-x64` prebuilds are absent. `[verified]`
  **Implication:** if NAVI ships a third-party extension ecosystem or an always-on agent, "sandboxed NAVI plugins" means adopting an unshipped isolated-vm membrane with a native-ABI distribution problem, or containerizing the whole process (see `docs/containerization.md`). There is no third option inside Pi today.

### 1.3 In-process extension mechanics that will bite an ambient harness specifically

- `packages/coding-agent/src/core/extensions/runner.ts:1034-1064` → `emitContext()` → `let currentMessages = structuredClone(messages)` executes **before** the handler-existence check. Call site `packages/coding-agent/src/core/sdk.ts:362-366` → `transformContext` is guarded only by `if (!runner) return messages`, **not** by `runner.hasHandlers("context")`. `[verified]`
  **Contrast:** `sdk.ts:343-349` → `onPayload` **is** guarded by `runner.hasHandlers("before_provider_request")`. `[verified]`
  **Implication:** **every** provider request deep-clones the entire conversation, even with zero `context` handlers installed. This is a per-request O(conversation) allocation on the hot path. For NAVI — which wants to observe a lot and reason briefly, i.e. many requests over a long-lived session — this is a real, measurable tax, and it is trivially fixable upstream. It is also the single cheapest NAVI-relevant patch.
- `packages/coding-agent/docs/extensions.md:677` → the `context` event fires "before each LLM call" `[external]`, matching `runner.ts:1034`. `[verified]`
  **Implication:** any NAVI extension that injects semantic page context via `context` pays full-conversation cost on every call and can be defeated by compaction. This is the most likely place for other agents to design a NAVI feature that quietly doubles token spend.
- `packages/coding-agent/src/core/agent-session.ts:1537-1547` → `_appendCustomMessage()` → pushes into `agent.state.messages` **and** calls `sessionManager.appendCustomMessageEntry(...)`. `[verified]` Doc: `docs/extensions.md:1424` → "Custom messages participate in LLM context." `[external]`
  **Contrast:** `docs/extensions.md:1479` → `pi.appendEntry()` → "Custom entries do NOT participate in LLM context." `[external]`
  **Implication:** the correct API split for NAVI is `appendEntry` for observations/memory and `sendMessage` only for things the model must see. Any NAVI design that uses `sendMessage` for page observations is a permanent context and session-file growth bug, and it is durable (it is written to disk too).
- `packages/coding-agent/src/core/session-manager.ts:1035,1054` → `appendFileSync(...)` on the entry path. `[verified]`
  **Implication:** durable writes are **synchronous on the event loop**. A design that records every ambient observation as a session entry blocks the loop per observation. Any high-frequency perception path must not write Pi session entries per event.
- `packages/coding-agent/src/core/extensions/loader.ts:724-756` → `discoverExtensionsInDir()` → the doc comment at `loader.ts:722` states "No recursion beyond one level. Complex packages must use package.json manifest." `[verified]`
  **Implication:** NAVI's extension suite must ship as a manifest-declared package, not a directory tree. A nested directory layout will silently not load.
- `packages/coding-agent/src/core/extensions/runner.ts:500-511` → `getAllRegisteredTools()` → "first registration per name wins". `[verified]` And `runner.ts:653-687` → `resolveRegisteredCommands()` → duplicate command names are disambiguated to `<name>:2`. `[verified]`
  **Implication:** silent shadowing between NAVI's tools and a user's existing extensions, resolved by load order (`loader.ts:625-642` iterates paths in discovery order). There is no conflict error — only a diagnostic for keybindings (`runner.ts:544-587`), not for tools.
- `packages/coding-agent/src/core/extensions/loader.ts:59-62` and `loader.ts:111-114` → both comment blocks state that extensions resolve `@earendil-works/pi-ai` to the **compat** entrypoint "so existing extensions using the old global API keep working at runtime **until compat is removed**". `[verified]`
  **Implication:** there is a scheduled breaking change for extension imports already announced in code comments. NAVI's extension surface will need a migration pass. Same signal from `loader.ts:67-73,129-141`, which retain `@mariozechner/pi-*` aliases — the runtime package rename already happened once.
- `packages/coding-agent/docs/extensions.md:1266-1307` → "Session replacement lifecycle and footguns": captured `pi`/`ctx` throw (guarded getters), but "previously extracted raw objects are still your responsibility. For example, if you capture `const sm = ctx.sessionManager` before replacement, `sm` is still the old `SessionManager` object." `[external]` Implemented at `loader.ts:209-216` (`invalidate()`), which invalidates only tracked event-bus subscriptions and guarded getters — **not** extracted objects. `[verified]`
  **Implication:** the most likely way NAVI breaks in production is a long-lived extension holding a session-bound object across a `/reload`, `newSession`, or model switch. It fails *silently* on raw objects and *loudly* on `ctx`. Design for re-resolution, not caching.

### 1.4 RPC mode: a good protocol, a deliberately degraded UI, and a framing trap

Verified gaps, all in `packages/coding-agent/src/modes/rpc/rpc-mode.ts` and matching `docs/rpc.md:1195-1205`:

- `rpc-mode.ts:228-231` → `async custom() { return undefined as never; }` → **no custom UI component** in RPC mode. `[verified]`
- `rpc-mode.ts:163-166` → `onTerminalInput()` returns a no-op. `[verified]`
- `rpc-mode.ts:248-252` → `getEditorText()` returns `""` permanently ("Synchronous method can't wait for RPC response"). `[verified]`
- `rpc-mode.ts:277-284` → `setEditorComponent()` / `getEditorComponent()` unsupported. `[verified]`
- `rpc-mode.ts:290-301` → `getAllThemes()` → `[]`; `getTheme()` → `undefined`; `setTheme()` → `{ success: false, error: ... }`. `[verified]`
- `rpc-mode.ts:179-193` → `setWorkingMessage` / `setWorkingVisible` / `setWorkingIndicator` / `setHiddenThinkingLabel` are silent no-ops (no event emitted). `[verified]`
- `rpc-mode.ts:195-208` → `setWidget()` supports **string arrays only**; component factories are dropped silently. `[verified]`
  **Implication:** any extension that builds its UX on `custom()` widgets will render nothing in RPC mode with no error. If NAVI drives Pi over RPC, all NAVI UI must live in the NAVI client, not in extensions.
- `rpc-mode.ts:55` → `takeOverStdout()`; implementation `packages/coding-agent/src/core/output-guard.ts:45-70` → `process.stdout.write` is replaced so writes are redirected to **stderr**. `[verified]`
  **Implication:** useful (extension `console.log` cannot corrupt the protocol) but it means stdout is no longer a debug channel — NAVI's bridge must not rely on child stdout for diagnostics, and must use stderr.
- `rpc-mode.ts:394-416` → `"prompt"` responds as soon as **preflight** succeeds; `docs/rpc.md:76` → "Failures after acceptance are reported through the normal event and message stream, not as a second `response` for the same request id." `[verified + external]`
  **Implication:** a NAVI bridge cannot use request/response for prompt outcome. It needs a state machine keyed on `agent_settled` (`docs/rpc.md:905-911`). Naive bridges will report success for prompts that subsequently fail.
- `packages/coding-agent/src/modes/rpc/jsonl.ts:14-20` → "This intentionally does not use Node readline. Readline splits on additional Unicode separators that are valid inside JSON strings" `[verified]`, repeated at `docs/rpc.md:30-37`. `[external]`
  **Implication:** **this is the highest-probability silent data-corruption bug in a NAVI browser bridge.** Page text is exactly the content that contains `U+2028`/`U+2029`. A bridge written with `readline` will split a JSON record mid-string and either crash or, worse, mis-associate events.
- `rpc-mode.ts:54-56` + `317-364` → one `AgentSession` per RPC process; `new_session`/`switch_session` rebind in-process (`rpc-mode.ts:437-444, 605-611`). `[verified]`
  **Implication:** one RPC child = one active conversation = one model at a time. "Multi-agent research" cannot be N parallel RPC calls into one child; it is N children (memory cost) or serialized turns (latency cost). This is a genuine architectural fork in the road and must be decided early.
- `rpc-mode.ts:142-145` → `confirm` dialog default resolves to **`false`** on timeout/abort. `[verified]`
  **Implication:** fail-closed. Good. An approval UI built on the sub-protocol is safe by default; an approval UI built on `select` is not (`select` defaults to `undefined`, `rpc-mode.ts:137-140` — must be treated as denied).
- `rpc-mode.ts:366-380` → SIGTERM/SIGHUP only; `SIGHUP` is excluded on `win32`. `[verified]`
  **Implication:** Windows RPC children get a smaller shutdown surface; the host must own process-tree cleanup.
- `rpc-mode.ts:728-745` → on `SIGTERM` the code **skips** `flushRawStdout()`. `[verified]`
  **Implication:** final events can be lost on SIGTERM. A bridge that kills the child on user action may miss the last `agent_settled`.

### 1.5 SDK: the session owns the app, so an embedding app reimplements the app

- `packages/coding-agent/src/core/sdk.ts:173-410` → `createAgentSession()` performs, in one function: model/auth restore (`204-227`), thinking-level restore + clamp (`229-254`), tool activation from settings (`256-263`), `Agent` construction including header transforms and retry/timeout policy (`306-372`), session restore and header entries (`374-386`), and `AgentSession` construction (`388-402`). `[verified]`
  **Implication:** the SDK is a **turnkey coding agent**, not an agent framework. NAVI either accepts its opinions (cwd discovery, compaction, retry, settings files) or replaces them one by one.
- `packages/coding-agent/examples/sdk/12-full-control.ts:36-49` → to take full control the embedder must implement a `ResourceLoader` with 13 members including `getSystemPrompt`, `getAppendSystemPrompt`, `extendResources`, `reload`. `[verified]`
  **Implication:** "full control" is explicitly a high-cost path. Budget it. The cheap path is to let `DefaultResourceLoader` discover resources and override only what matters.
- `packages/coding-agent/examples/sdk/13-session-runtime.ts:40-50` → after every session replacement, the app must re-run `session.bindExtensions({})` **and** re-subscribe. `[verified]`
  **Implication:** a documented, easy-to-miss rebinding requirement. A NAVI UI that forgets this after `newSession`/`switchSession` will go silent with no error.
- `packages/coding-agent/src/core/sdk.ts:388-402` → `AgentSession` is the unit of state; `agent-session.ts` is **3,552 lines**. `[verified]`
  **Correction to the task brief:** the brief cites "3179 lines"; as of `71dca871` the file is **3,552 lines**. Even so, the size finding stands and is worse than stated, not better.
- `packages/coding-agent/src/modes/interactive/interactive-mode.ts` = **6,620 lines** `[verified]`; `settings-manager.ts` = 1,417; `package-manager.ts` = 2,699; `session-manager.ts` = 1,746; `extensions/types.ts` = **1,797** (the extension contract alone is 1.8k lines). `[verified]`
  **Implication for forks:** the class you must understand to fork NAVI is an order of magnitude larger than the part you want to change.

### 1.6 The experimental remote harness is a second, parallel stack — dev-only, POSIX-only, and mid-rewrite

- `packages/coding-agent/src/core/experimental.ts:1-3` → the entire harness is gated on `process.env.PI_EXPERIMENTAL === "1"`. `[verified]`
- `packages/coding-agent/src/experimental/server.ts:58-67` → `ensurePrivateServerDirectory()` → `if (typeof process.getuid !== "function") throw new Error("Unix socket directory requires a POSIX user ID")`. `[verified]`
  **Implication:** the remote harness is **unusable on Windows** by construction. A Windows-first NAVI must not build on it.
- `packages/coding-agent/package.json:29-33` → `"files": ["dist", "!dist/client", "!dist/experimental", "!dist/cli/experimental", ...]` and `package.json:75-78` → `pi-client`, `pi-protocol`, `pi-server` are **devDependencies**. `[verified]`
  **Implication:** none of this ships. Depending on it means depending on repo-internal, unpublished code.
- `packages/coding-agent/docs/development.md:33` → "The local SDK and stdio RPC API are unchanged." `[external]`
  **Implication:** the vendor's own stability statement names **SDK** and **stdio RPC** as the stable interfaces. That is the strongest available signal about where NAVI should sit.
- `packages/agent/docs/work-packages/00-runtime1-removal.md:5` → "Status: Complete. No tag. Runtime2 is the sole public implementation. **Do not release while its execution path is incomplete.**" `[verified]`
- `packages/agent/docs/runtime-simplification.md:3-7` → "This is not the isolated scratch spike… Format 4 is still work in progress, so the durable type replacement required no migration or compatibility representation." `[verified]`
- `packages/agent/docs/mobile-handoff/README.md:22-23` → "**Three units ship working code. Four are specifications.** … Do not assume a doc describes something that exists." `[verified]`
- `packages/agent/docs/mobile-handoff/README.md:56-65` → three **live bugs on `origin/dev`** are listed (`drive/tools.ts:257`, `runtime/progress.ts:44`, and the memo/checkpoint two-transaction bug), and `README.md:78-80` states the typecheck baseline is "~788 pre-existing errors". `[verified — as an upstream self-report]`
  **Implication:** do not build NAVI on `AgentHarness`, the experimental remote runtime, or anything under `packages/agent/src/harness/*`. It is under active breaking redevelopment, has documented data-loss bugs on the dev branch, and the SDK path explicitly retains the older `agent-loop.ts` unchanged (`00-runtime1-removal.md:49`).

### 1.7 Build, CI and maintenance reality

- `.github/workflows/ci.yml:15` → `runs-on: ubuntu-latest`, one job, `npm ci --ignore-scripts` → `npm run build` → `npm run check` → `npm test`. `[verified]`
  **Implication:** **the default test suite only ever runs on Linux.** macOS and Windows appear only in `build-binaries.yml:131-140`'s `smoke-test-binaries` matrix (launch-the-binary smoke tests, not the suite). `packages/coding-agent/docs/windows.md` exists precisely because Windows is a special case (Git Bash discovery, `pwsh` tool, `shellPath`). `[verified]`
  **Implication for NAVI:** if NAVI is developed on Windows, you are developing on a platform Pi does not fully test. Cross-platform fixes are your cost, not upstream's.
- `packages/coding-agent/package.json:44` → `build:binary` builds TUI, telemetry, ai, agent, protocol, client, then `bun build --compile`. `[verified]` `packages/agent/docs/mobile-handoff/README.md:78-81` → "`packages/ai` cannot be built offline — model data is fetched at build time." `[verified]`
  **Implication:** reproducing Pi's build requires network and a Bun toolchain. Offline/air-gapped or hermetic builds of a Pi-based NAVI are not free.
- Test volume: **544 `*.test.ts` files / ~122,600 lines** across `packages/`. `packages/coding-agent` alone: 646 `.ts` files / ~126,700 lines of source+test. `[verified]`
  **Implication:** a fork inherits a 120k-line test suite that must be kept green through every upstream merge.
- Release cadence from `packages/coding-agent/CHANGELOG.md`: `0.81.1` (2026-07-21) → `0.85.1` (2026-09-05) — **10 releases in ~7 weeks**, with sections that include removals ("Removed the unavailable inherited Grok Build 0.1 model"), compatibility restorations ("Restored the `@earendil-works/pi-coding-agent/client` compatibility entry point"), and tool-contract changes. `[verified]` PR numbers in range `#8293`–`#9093` in the `0.85.0` section alone. `[verified]`
  **Implication:** this is a fast-moving upstream with external contributors. Any fork diverges in days, not quarters.
- LICENSE is MIT (`LICENSE:1-21`). `[verified]` Forking is legally unencumbered. The barrier to forking is entirely engineering, not legal.

### 1.8 Pi contains no browser capability at all

- Grep for `playwright|puppeteer|chrome\.debug|devtools|CDP|webdriver` across `packages/` matched only one test fixture (`test/fixtures/before-compaction.jsonl`). `[verified]`
  **Implication:** every navigation, DOM read, screenshot, click and form interaction NAVI wants is **net-new**, outside Pi. Pi can host the tools; it cannot provide the capability. This also means the security story for page control cannot be delegated to Pi — see §2.4.

---

## 2. Architectural Implications

### 2.1 There are only three honest ways to sit on Pi, and one of them is wrong

| Option | What it means | Verdict for NAVI |
|---|---|---|
| **In-process SDK** (`createAgentSession`) | NAVI *is* the Node app; Pi is a library; sessions, extension runner and model runtime live in NAVI's process | Best control, best latency, worst coupling. Recommended (see §3). |
| **RPC child** (`pi --mode rpc`) | NAVI is a client; Pi is a subprocess over LF-JSONL | Best isolation, worst UI fidelity, prompt-outcome ambiguity, one session per child. Use only for privileged/isolated work. |
| **Fork** | NAVI owns a divergent Pi tree | Justified only for a very small number of reasons (§2.7). Otherwise it is a permanent merge tax. |

`docs/development.md:33` (`[external]`) names SDK + stdio RPC as the stable interfaces. Stick to them.

### 2.2 The context budget is the real architecture, and Pi gives you no ambient-tuned allocator

Three independent mechanisms all consume the same budget, and none of them know about the others:

1. System prompt, rebuilt per prompt (`system-prompt.ts:28-167`). Default persona is a coding assistant plus pointers to Pi's own README/docs/examples and the instruction "Always read pi .md files completely" (`system-prompt.ts:137-144`). `[verified]` Cheap in tokens (~250–400) but pure waste on every ambient call, and it actively invites the model to read documentation files mid-observation.
2. `emitContext` deep-clone per request (`runner.ts:1036`) — CPU/memory, not tokens, but on the hot path unconditionally. `[verified]`
3. Compaction (`compaction.ts:132-136, 235-238`) — amortized tokens, one silent LLM call per crossing, plus a 2,000-char cap on tool results inside summaries (`docs/compaction.md:271`). `[external/verified]`

**Design consequence:** NAVI must define its **own** budget accounting in front of Pi: a per-minute token ceiling, a per-observation byte ceiling, and a hard rule that ambient perception does not enter LLM context unless a decision needs it. The right primitives already exist and are cheap: `pi.appendEntry()` for durable non-context data (`docs/extensions.md:1479`) and `customPrompt` for a NAVI persona (`system-prompt.ts:48-73`).

### 2.3 "Semantic page context" is a browser-extension problem, not an agent problem

Pi has no DOM, no renderer, no CSS, no accessibility tree, no viewport concept, and no navigation lifecycle. Chromium gives all of these for free, in the page's own process, with the reader-mode and accessibility-tree APIs already implemented and maintained by someone else.

If NAVI computes "semantic page context" in the agent:
- it must ship a headless browser or a shim, doubling memory,
- it re-reads and re-posts page content over the bridge (bandwidth, latency, and a second copy in the session),
- it loses the page's own notion of "main content vs chrome",
- and it puts untrusted page text one hop closer to the model with fewer structural boundaries.

**Correct split (recommended):**
- **In the browser extension:** DOM traversal, readability/main-content extraction, accessibility-tree snapshot, selection/tab/focus events, screenshot capture at the OS level, navigation lifecycle, scroll and visibility signals, and *all* page mutation (click/type/scroll). Also the first-pass redaction of form fields, credential inputs, and anything the user marked private.
- **In the bridge (local service):** transport, framing, deduplication, byte and rate budgets, permission enforcement, and the mapping from browser events to NAVI "observations". Prune here, before anything reaches Pi.
- **In Pi:** reasoning only. It receives *summaries and deltas* — text already reduced — plus a small set of tools that ask the bridge for more when it needs more.

### 2.4 NAVI's permission model must be enforced *outside* Pi, because Pi has none

`README.md:41` and `docs/security.md:31-35` are unambiguous: no built-in permission system, no sandbox, extensions run with full user permissions. `[external]`

Therefore:
- A `tool_call` extension hook (`docs/extensions.md:778-818` → returns `{ block: true, reason, terminate }`) is a **policy** gate, not a security gate. It can be bypassed by any other loaded extension, by a NAVI bug, or by the model calling a tool that was registered before the gate. `[external/verified]`
- The only structural place to enforce "OBSERVE / RESEARCH / CONTROL" for **page** actions is inside the bridge or the browser extension, where the action actually happens, and where the principal (which tab, which URL, which form field) is known.
- For filesystem/process/network, the only real boundary is OS-level: container, micro-VM, or the unshipped isolated-vm membrane. `docs/security.md:41-53` and `docs/containerization.md` say exactly this. `[external]`

**Recommendation:** split the levels. `OBSERVE` and `RESEARCH` are cheap and should be enforced in the bridge (no writes, no navigation, read-only). `CONTROL` must require an out-of-band confirmation in the browser UI itself — **not** in Pi's `ctx.ui.confirm()`, which in RPC mode arrives as a JSON message on a pipe that any compromised or careless bridge client can answer `true` to (`rpc-mode.ts:142-145`, `docs/rpc.md:1228-1243`). The confirmation must be rendered by the browser extension, in a trusted context, with the target origin shown.

### 2.5 Multi-agent research should not be built from Pi child processes

Evidence: one `AgentSession` per process (`rpc-mode.ts:54-56`, `317-364`); `AgentSession` is 3,552 lines and holds model, compaction, retry, bash, and extension state; session storage is synchronous JSONL (`session-manager.ts:1035`). `[verified]`

N RPC children means N × (Node + Pi + extension set + model runtime) resident, plus N session files. It also means N independent permission surfaces and N places to drift.

**Better:** in-process. Build one Node service, create N `AgentSession`s (or, better, do not create N sessions at all — use one session with N *tasks* and explicit tool-call scoping), and enforce a global concurrency + token ceiling in the service. If isolation is genuinely needed (untrusted code execution), use one container per *task class*, not one process per *investigation*.

### 2.6 Coupling: NAVI must not reach into Pi internals, and the type surface makes that tempting

`extensions/types.ts` is **1,797 lines** `[verified]`, and the runner hands extensions live `SessionManager` and `ModelRegistry` (`runner.ts:723-801`). `[verified]` It is very easy to write a NAVI feature that manipulates session entries directly.

**Rule:** NAVI should depend on exactly four Pi surfaces — `createAgentSession` (+ its option type), `AgentSession`'s public methods (`prompt`/`abort`/`dispose`/`subscribe`/`getContextUsage`), the extension `pi.on(...)` events NAVI actually uses, and `pi.appendEntry`. Everything else is volatile. `examples/sdk/12-full-control.ts:36-49` shows the price of going deeper.

### 2.7 Reasons NOT to fork Pi

1. **Velocity.** 10 releases in 7 weeks with removals and contract changes (`CHANGELOG.md`). `[verified]`
2. **Test mass.** ~122.6k lines of tests, 544 files, Linux-only CI (`ci.yml:15`). `[verified]` Every merge is a merge conflict resolution exercise in `agent-session.ts` (3,552 lines) and `interactive-mode.ts` (6,620 lines).
3. **You do not need a fork to rename.** `piConfig` in `package.json` (`package.json:6-8`) plus `config.ts:500-509` (`APP_NAME`, `APP_TITLE`, `CONFIG_DIR_NAME`, `ENV_AGENT_DIR`) already give you a rebrand, distinct config dir, and distinct env vars. `[verified]`
   **However:** `docs/development.md:48` claims you only change "`name`, `configDir`, and `bin`" `[external]`, but the shipped `package.json:6-8` has **no `name` field in `piConfig`** — only `configDir`. `config.ts:500-502` reads `pkg.piConfig?.name` and defaults to `"pi"`. A rebrand therefore requires adding the field, and the doc understates the work: `config.ts:501` also derives `PACKAGE_NAME`, and `package.json:9-11` `bin` plus `package.json:22-27` `exports` entrypoints must stay coherent. Treat the doc as a starting hint, not a spec.
4. **You cannot rebrand everything.** `packages/coding-agent/src/config.ts:515` → `DEFAULT_SHARE_VIEWER_URL = "https://pi.dev/session/"`; `config.ts:518-521` → `getShareViewerUrl()`; the system prompt links `pi.dev`-hosted docs (`system-prompt.ts:76-78`). `[verified]` A rebrand that leaves these is inconsistent; a rebrand that changes them is a fork.
5. **Extension/protocol drift.** `loader.ts:59-62` announces a pending compat removal. `[verified]` A fork freezes NAVI extensions on an API upstream intends to break.
6. **No browser capability to fork for.** The thing NAVI needs does not exist in Pi at all (§1.8), so a fork buys nothing on the critical path. `[verified]`

### 2.8 Where a fork *is* justified

Only two, and both are narrow:

- **Ship a single binary with NAVI extensions baked in and no configured tool paths.** If NAVI must be installable by non-developers as one artifact with no `pi install`, no `~/.pi` dependency and no discovery step, the *build* must be owned. This is a **build-level** fork (own the bundle, pin the version), not a source-level divergence. `package.json:44` shows the bundle pipeline is scriptable. `[verified/inferred]`
- **A genuinely upstream-level fix.** The `emitContext` unconditional `structuredClone` (`runner.ts:1036`) is the clearest example: the correct fix is one `hasHandlers` guard. That is a 5-line upstream PR, not a fork. **Submit it upstream first.** `[verified]`
- **Not justified:** custom overlay UI, custom permission semantics, custom session format, custom context management. Each of those is achievable via SDK options, extension events, or an out-of-process bridge.

---

## 3. Recommended Approach

**One sentence:** NAVI is a Node service that embeds `@earendil-works/pi-coding-agent` in-process for reasoning, a browser extension that owns all page perception and action, and a local bridge between them that owns framing, budgeting and permission enforcement. Pi is a dependency, not a platform, and never the source of truth for what the user is looking at.

### 3.1 Process shape

```
[ Browser extension (MV3) ]  --perception + action + trusted confirmations-->
        |  WebSocket / native messaging, localhost only, token-authenticated
        v
[ NAVI bridge/service (Node 22, single process) ]
        |  in-process SDK call
        v
[ Pi AgentSession (one, long-lived) ]  -- tools are thin RPC stubs into the bridge
```

Rationale, each pinned to evidence:
- Browser extension owns perception/action → §2.3, §1.8.
- Bridge owns framing/budget/permissions → §2.4; framing trap at `jsonl.ts:14-20`.
- Pi in-process → avoids RPC UI degradation (`rpc-mode.ts:228-252`) and prompt-outcome ambiguity (`rpc-mode.ts:394-416`), and avoids the one-session-per-child cost (`rpc-mode.ts:54-56`).
- Pi tools are bridge stubs → the model's `browser_*` tools are *policy-checked in the bridge*, so the `tool_call` extension hook becomes defence-in-depth rather than the only gate.

### 3.2 Concrete decisions

1. **One long-lived `AgentSession`**, created with an explicit `customPrompt` NAVI persona, an explicit `tools` allowlist, and `SessionManager.inMemory()` unless durable history is wanted. `sdk.ts:256-263` shows `tools`/`noTools`/`excludeTools` are first-class. `[verified]`
2. **Never put page content in the system prompt or in `sendMessage`.** Observations go to `pi.appendEntry()` (durable, out of context) or to the bridge's own store. Only a *decision* payload enters `session.prompt(...)`. `[verified, docs/extensions.md:1479]`
3. **Do not register a `context` handler.** If NAVI needs per-call pruning, do it in the bridge before calling `prompt`, and file the `hasHandlers("context")` guard upstream. `[verified, runner.ts:1036]`
4. **Re-resolve session-bound objects at every use.** No cached `ctx.sessionManager`, no captured `pi` after `reload`/`newSession`. `[verified, docs/extensions.md:1266-1307]`
5. **Own the frame reader.** LF-only splitter, `StringDecoder`, strip trailing `\r`. Never `readline`. `[verified, jsonl.ts:14-20]`
6. **Approvals render in the browser**, in a trusted extension context, showing target origin; the bridge treats absence of approval as denial. `[inferred from §2.4]`
7. **Budget in front of Pi, not inside it.** Enforce: max bytes per observation; max observations per minute; max tokens per hour; a hard stop that pauses perception (not the agent) when exceeded. This is the mechanism that prevents NAVI being slower and more expensive than a normal browser.
8. **Pin Pi exactly** (`0.85.1` + shrinkwrap; `package.json:85` already ships `npm-shrinkwrap.json`). `[verified]` Upgrade on a schedule with a checklist, not with `latest`.

### 3.3 Directives to the other nine agents' predictable recommendations

Stated bluntly, because these are the ones I expect and they are where the fragility will come from:

- **"Inject semantic page context into the agent via the `context` event."** No. `runner.ts:1036` clones the whole conversation per request, `docs/extensions.md:677` says it fires per LLM call, and compaction will eventually discard it anyway. Prune in the bridge, pass a decision payload to `prompt`. If you still want the hook, file the guard upstream first.
- **"Build a custom overlay UI as a Pi extension."** No. `ctx.ui.custom()` is a terminal component (`docs/extensions.md:2741-2758`) and returns `undefined` in RPC mode (`rpc-mode.ts:228-231`). Overlays are *terminal* overlays (`docs/extensions.md:2773-2802`), not browser overlays. Any browser UI belonging to NAVI must be drawn by the browser extension.
- **"Run Pi as an RPC child, one per agent, for multi-agent research."** Only if you accept N resident Node processes and a request/response model that lies about prompt outcomes (`rpc-mode.ts:394-416`). Prefer one process, N tasks.
- **"Fork Pi to add NAVI's permission levels."** No. There is no permission system to extend (`README.md:41`), and the levels belong in the bridge and the browser anyway (§2.4). A fork buys you a merge war, not a boundary.
- **"Use Pi's memory/session store as NAVI's memory."** Careful. Sessions are keyed by cwd (`session-manager.ts:476-481`), written synchronously (`session-manager.ts:1035`), and rewritten in place by compaction. Keep NAVI memory separate; use Pi entries only for narrative.
- **"Build on the experimental remote harness because it already does server/client."** No. `PI_EXPERIMENTAL=1` (`core/experimental.ts:1-3`), POSIX-only (`experimental/server.ts:59`), excluded from the published package (`package.json:31-33`), and its sibling runtime is mid-rewrite with documented live bugs (`packages/agent/docs/mobile-handoff/README.md:56-65`).
- **"Fork just to rebrand."** No. `piConfig` + `config.ts:500-509` covers it. But read `config.ts:500-509` rather than `docs/development.md:48` — the doc omits the `piConfig.name` field that actually exists in the code.
- **"Voice is a NAVI differentiator."** Voice is an input modality with no Pi dependency at all. It belongs at the edge, next to the browser extension, feeding text into the bridge. Do not let it become an architectural tier.

---

## 4. Alternatives

### A1 — Pure browser extension, no agent (the null hypothesis)
Do the 80% with a browser-side assistant: local readability extraction, local heuristic triggers, a small on-device model or a single API call per *user-initiated* action. `[inferred]`
**Test NAVI against this.** If NAVI cannot beat this on a concrete task, the harness is decoration. This alternative is cheap to prototype and should be the baseline every NAVI claim is measured against.

### A2 — Pi in-process, browser extension as a pure sensor (no action)
A deliberately crippled v1: OBSERVE only. The extension sends page summaries; Pi can reason and answer, but has no page-mutation tools. Removes the entire CONTROL permission problem, the approval UI, and most of the security surface. `[inferred]`
Highest ratio of value to risk. Recommended as the first shippable version.

### A3 — RPC child per *trust level*, one child total
Two RPC children: a sandboxed read-only one for browsing content and a local one for filesystem work. Buys a real boundary for untrusted page content at the cost of one extra process. Only worth it if NAVI ever executes content-derived code. `[inferred, consistent with docs/security.md:41-53]`

### A4 — Own the agent loop, borrow only `pi-ai`
If NAVI's needs and a coding agent's needs diverge enough (no cwd, no compaction, no session tree), the honest alternative is: keep `@earendil-works/pi-ai` for the provider abstraction, write a NAVI-native loop, and drop `AgentSession`. `[inferred]`
Trade-off: you lose compaction, retry, `pi-ai` compat aliases, the extension system, and the model catalog's auth flows (`sdk.ts:178-227`). Do not choose this casually — but note that it is *less* work than forking, and it has no merge debt.

### A5 — Containerize everything, one privileged Pi
Reuse `docs/containerization.md` patterns (`[external]`): route all tool execution into a micro-VM, keep credentials on the host. This is the only way to get a real boundary without the isolated-vm native-ABI problem. `[external]`

---

## 5. Risks

| # | Risk | Trigger | Severity | Mitigation / reason to drop |
|---|---|---|---|---|
| R1 | **Silent JSONL corruption in the bridge** if `readline` is used | Page text containing `U+2028`/`U+2029` crossing the RPC boundary | High | Own the splitter (`jsonl.ts:14-20`); add a fuzz test with those separators in string fields |
| R2 | **Approval bypass** | Any page-control action gated only by `ctx.ui.confirm()` over RPC | High | Enforce in bridge + browser-trusted confirmation; fail closed (`rpc-mode.ts:142-145`) |
| R3 | **Ambient context runaway** | Per-observation `sendMessage`/`context` hooks | High | `appendEntry` for observations; no `context` handler; bridge-side pruning; hourly token ceiling |
| R4 | **NAVI slower than a plain browser** | Synchronous session writes (`session-manager.ts:1035`) + unconditional full-context clone (`runner.ts:1036`) + compaction LLM calls | High | Never write Pi entries per observation; upstream the `hasHandlers` guard; measure p95 interaction latency from day one as an explicit budget |
| R5 | **Extension holds stale session object** | `/reload`, model switch, `newSession` | Medium-High | Re-resolve every session-bound object at use (`docs/extensions.md:1266-1307`) |
| R6 | **Tool shadowing** | NAVI tool name collides with a user extension | Medium | Namespace all NAVI tools (`navi_*`); detect collisions at startup (`runner.ts:501-511` is first-wins, silently) |
| R7 | **Upstream breaking change** | compat entrypoint removal (`loader.ts:59-62`), `pi-ai` API changes, npm churn | Medium | Pin exactly; keep NAVI's Pi surface to the four interfaces in §2.6; scheduled upgrades with a replay test harness |
| R8 | **Windows dev on an untested platform** | CI is ubuntu-only (`ci.yml:15`); RPC SIGTERM skips flush (`rpc-mode.ts:741`) | Medium | Run NAVI's own CI on Windows + macOS; never rely on last-events-before-exit |
| R9 | **Fork entropy** | Any source-level fork | Medium-High | Do not fork (§2.7). If you must, fork the *build*, not the source |
| R10 | **Multi-process resource blowup** | N RPC children for multi-agent research | Medium | One process, N tasks; explicit concurrency cap |
| R11 | **Third-party NAVI extension executes arbitrary code** | Any plugin ecosystem | Medium-High | It runs unsandboxed (`docs/extensions.md:111`). Require review, or containerize, or wait for the isolated-vm membrane (unshipped, native-ABI cost) |
| R12 | **Reading the wrong docs** | Docs describe unbuilt specs (`mobile-handoff/README.md:22-23`) | Medium | Only trust `src/`; the repo itself says "If a doc and the code disagree, the code wins" (`mobile-handoff/README.md:27`) |
| R13 | **Compaction destroys NAVI memory** | Long sessions, `keepRecentTokens: 20000` | Medium | Keep NAVI memory out of Pi sessions; treat Pi context as ephemeral working memory |
| R14 | **Untrusted page content is prompt injection** | Any page NAVI reads | High, unavoidable | `docs/security.md:37` states this is expected and cannot be reliably prevented. Structural mitigation: the model never holds CONTROL credentials; the bridge re-verifies every action against the permission level and the origin |

---

## 6. Feature Assessment

**CORE** = build the whole design around it. **LIKELY** = build it, after CORE. **EXPERIMENTAL** = time-boxed spike, expect to throw it away. **DEFER** = real but not yet. **REJECT** = do not build.

| Feature | Verdict | One-line rationale |
|---|---|---|
| Browser extension as perception + action, bridge as policy/transport | **CORE** | Pi has zero browser capability (§1.8) and no permission system (`README.md:41`); the boundary must live where the action happens |
| Local bridge owning framing, byte/rate budgets, permission levels | **CORE** | It is the only place that can enforce OBSERVE/RESEARCH/CONTROL before untrusted page data reaches the model (§2.4) |
| Embed Pi in-process via SDK (`createAgentSession`) | **CORE** | `docs/development.md:33` names SDK as stable; RPC degrades UI (`rpc-mode.ts:228-252`) and hides prompt failure (`rpc-mode.ts:394-416`) |
| `customPrompt` NAVI persona replacing the coding-agent prompt | **CORE** | `system-prompt.ts:127-144` otherwise injects coding persona + "read pi .md files completely" into every ambient call |
| Pi tools as thin stubs into the bridge (policy-checked there) | **CORE** | Makes `tool_call` an extra layer rather than the only gate (`docs/extensions.md:778-818`) |
| Explicit token/byte/rate budget in front of Pi | **CORE** | Prevents R3/R4; the failure mode otherwise is a harness that is slower and more expensive than the browser |
| Page-content extraction in the extension (readability/a11y tree) | **CORE** | Browser already implements it; recomputing in the agent means shipping a second browser (§2.3) |
| Memory store **separate** from Pi sessions (own schema + key) | **CORE** | Pi sessions are cwd-keyed (`session-manager.ts:476-481`), sync-written (`:1035`), and compaction-mutated |
| `pi.appendEntry()` for durable non-context observations | **LIKELY** | Documented as out of LLM context (`docs/extensions.md:1479`); the correct home for NAVI narrative |
| Permission levels enforced in bridge + browser-trusted confirmation | **LIKELY** | Only structurally sound placement (§2.4); needs the browser UI first |
| Browser-owned input (voice as an edge modality → text → bridge) | **LIKELY** | Zero Pi dependency; keep it out of the architecture (`§3.3`) |
| Semantically compressed "decision payload" passed to `prompt()` | **LIKELY** | The only context-injection path that survives compaction and respects the budget |
| Short-lived `AgentSession` per user-visible task, disposed after | **LIKELY** | Avoids unbounded context growth; `dispose()` exists (`agent-session.ts:877-894`) and resets listener state |
| Multi-agent research | **EXPERIMENTAL** | Only after §2.5's one-process/N-task shape is proven; N RPC children is a resource trap |
| Experimental remote harness (server/client/coordinator/relay) | **REJECT** | `PI_EXPERIMENTAL`-gated (`core/experimental.ts:1-3`), POSIX-only (`experimental/server.ts:59`), unpublished (`package.json:31-33`) |
| `AgentHarness` / `packages/agent/src/harness/*` | **REJECT** | Mid-rewrite ("Do not release while its execution path is incomplete", `00-runtime1-removal.md:5`); documented live data-loss bugs on dev |
| Custom **Pi** overlay UI (`ctx.ui.custom`) for NAVI | **REJECT** | Terminal-only (`docs/extensions.md:2741-2758`); `undefined` over RPC (`rpc-mode.ts:228-231`) |
| Storing observations via `pi.sendMessage()` | **REJECT** | Enters LLM context *and* disk (`agent-session.ts:1537-1547`, `docs/extensions.md:1424`); permanent cost |
| A `context`-event handler for per-call pruning | **REJECT** (as designed) | Fires per LLM call (`runner.ts:1034`) and forces a whole-conversation clone today (`runner.ts:1036`); prune in the bridge |
| Forking Pi for rebrand | **REJECT** | `piConfig`/`config.ts:500-509` already covers it — but read the code, not `docs/development.md:48` |
| Forking Pi for permission semantics | **REJECT** | No permission system to extend; the boundary belongs in the bridge |
| Forking Pi to bake NAVI extensions into one binary | **DEFER** | Legitimate only at the *build* level, for a non-developer install story; own the bundle + pin version |
| Third-party NAVI extension ecosystem | **DEFER** | Unsandboxed (`docs/extensions.md:111`); wait for a real membrane or containerize |
| NAVI on Windows | **DEFER** | Pi's suite is Linux-only (`ci.yml:15`); ship when NAVI's own CI covers three OSes |
| Screenshot / vision-based page understanding as the primary perception path | **DEFER** | Tokens per observation dominate (`compaction.ts:244`: an image is estimated at 4,800 chars); use DOM/a11y text first, images only on demand |

---

## 7. Kill list

Ideas to drop outright, with the reason:

1. **Fork Pi to implement NAVI.** 10 releases in 7 weeks, 122.6k lines of tests, Linux-only CI (`CHANGELOG.md`, `ci.yml:15`). Fork only if you need a *build*, never a *behavior*.
2. **Build on the experimental remote harness.** POSIX-only (`experimental/server.ts:59`), env-gated (`core/experimental.ts:1-3`), excluded from the package (`package.json:31-33`).
3. **Build on `AgentHarness`.** Vendor says do not release while its path is incomplete (`00-runtime1-removal.md:5`) and lists live bugs on dev (`mobile-handoff/README.md:56-65`).
4. **`ctx.ui.custom()` as NAVI's overlay.** Terminal component; `undefined` in RPC.
5. **`context`-event page injection.** Per-call, full-conversation clone today (`runner.ts:1036`); discarded by compaction.
6. **`pi.sendMessage()` for ambient observations.** Enters context and disk permanently.
7. **N RPC children for multi-agent research.** One `AgentSession` per process (`rpc-mode.ts:54-56`).
8. **A NAVI memory store that is a Pi session tree.** cwd-keyed, sync-written, compaction-mutated.
9. **Permission levels implemented only as a `tool_call` hook.** It is a policy gate, not a boundary; and the extension that installs it runs unsandboxed next to every other extension.
10. **Using `readline` for the RPC JSONL stream.** Explicitly non-compliant (`jsonl.ts:14-20`).
11. **Depending on `@earendil-works/pi-client` / `pi-protocol` / `pi-server`.** They are devDependencies (`package.json:75-78`).
12. **Voice as an architectural tier.** It is an edge modality; making it a subsystem adds a process and a failure mode for no reasoning benefit.
13. **A NAVI extension that caches `ctx.sessionManager` or `pi`.** Documented footgun; fails silently on extracted objects.
14. **Relying on `agent_end` for completion.** Use `agent_settled` (`docs/rpc.md:905-911`); `agent_end` may be followed by retry/compaction/continuation.
15. **Screenshot-first perception.** ~4,800 estimated tokens per image (`compaction.ts:244`) versus a few hundred for an a11y-text summary.

---

## 8. Simplification opportunities

Ordered by return on effort.

1. **File the `emitContext` guard upstream (5 lines).** `runner.ts:1036` clones the whole conversation per request before checking for handlers; `sdk.ts:343-349` shows the correct pattern (`hasHandlers`). This is the single highest-value contribution NAVI can make to Pi, and it removes an unconditional hot-path cost for every extension user. `[verified]`
2. **Delete the `context` hook and `sendMessage` from NAVI's design.** One rule — "observations are durable and out of context; only decisions enter `prompt()`" — removes the largest token consumer and the 2 KB compaction cap (`docs/compaction.md:271`) in one stroke.
3. **Make the bridge the only component that talks to Pi.** Extension ↔ bridge ↔ Pi, with Pi having no direct knowledge of the browser. One boundary, one place to enforce policy, one place to budget.
4. **Replace "semantic page context" with "a page summary the extension already knows how to produce."** Readability + a11y-tree extraction is a solved problem and it is free in the browser. NAVI's differentiator is *when and why* it looks, not *how it parses*.
5. **Collapse the permission model to two levels in v1 (OBSERVE / CONTROL) and add RESEARCH when it has teeth.** Three levels with fuzzy boundaries will produce three code paths and one bypass. Note the codebase makes this concrete: `confirm` fails closed, `select` does not (`rpc-mode.ts:137-145`). Only build approval flows on `confirm`.
6. **One `AgentSession` per user-visible task, not per page view, not per observation.** `dispose()` is implemented (`agent-session.ts:877-894`) and clears listeners, so short-lived sessions are cheap and there is a supported teardown path.
7. **Drop the overlay-UI workstream entirely and put the UI in the browser.** `interactive-mode.ts` (6,620 lines) is not a UI toolkit you want to extend, and RPC mode cannot render it anyway.
8. **Namespace every NAVI surface.** Tools as `navi_*`, commands as `navi:*`, entries as `navi.<kind>`, events on the bridged bus with a `navi.` prefix. Silent first-wins tool shadowing (`runner.ts:501-511`) then becomes detectable.
9. **Add a replay harness now, before the first upgrade.** The API-compat churn (`loader.ts:59-62`) is scheduled; a recorded session-event fixture set makes upgrades a test run instead of an incident.
10. **Do not build `ResourceLoader`.** Only do the 13-member implementation (`examples/sdk/12-full-control.ts:36-49`) if `DefaultResourceLoader` demonstrably does something harmful. The default is 3 lines.

---

## 9. Open Questions

Each with the evidence that would resolve it.

1. **How much of the NAVI value survives if the model is only ever asked a question, never given ambient context?** Resolve by building alternative A2 (OBSERVE-only) first and measuring whether proactive context is actually needed. `[highest-leverage unknown]`
2. **What is the real p95 latency of an in-process SDK `prompt()` round trip with a warm session?** Not measurable from source; must be measured. Watch `runner.ts:1036` and `session-manager.ts:1035` specifically.
3. **Does upstream have a roadmap item for the extension compat removal, and on what timeline?** `loader.ts:59-62` says "until compat is removed" but gives no date. Check `rfc.earendil.com` (referenced at `README.md:51`). `[unverified]`
4. **Will `AgentHarness` and `AgentSession` converge, or will SDK users be asked to migrate?** `docs/development.md:33` says the SDK is unchanged *today*; `00-runtime1-removal.md` shows a parallel rewrite is real. `[unverified — biggest strategic unknown]`
5. **Is the isolated-vm membrane planned for the coding-agent, and with what distribution story?** Exists only under `packages/agent/docs/mobile-handoff/02-plugins/02-sandbox/`. Its README flags the native ABI matrix and a missing `darwin-x64` prebuild. `[verified for existence; unverified for roadmap]`
6. **Upstream commit velocity and merge-conflict rate on `agent-session.ts`.** Not answerable from a single-commit clone. Requires a full clone or the GitHub API. `[unverified]`
7. **Can a browser MV3 extension reliably hold a long-lived localhost socket across service-worker suspension?** Contradictory reports exist in the ecosystem; must be tested per browser. If not, the bridge needs a pull model or native messaging. `[external/unverified]`
8. **Does the target browser's extension API permit the action set NAVI's CONTROL level promises** (cross-origin scripted input, file uploads, clipboard)? Depends on the browser and the store's policy. `[external/unverified]`
9. **What is the acceptable per-hour token ceiling for an "always-on" assistant product, and does the compaction path (`compaction.ts:132-136`) fit inside it?** A product question that determines whether compaction should be disabled entirely and context managed by NAVI instead.
10. **Which NAVI observations are worth persisting at all?** Without an answer, the memory store accumulates and then silently feeds stale context — the same failure shape as compaction, but designed by NAVI rather than inherited.
