# NAVI Agent 9/10 — Research, Actions, Voice and Higher-Level Capabilities

**Scope:** design research only, no implementation.
**Repo studied (read-only):** `C:\Users\lain\NAVI\vendor\pi` — clone of `github.com/earendil-works/pi` @ `71dca871` (2026-09-11), `@earendil-works/pi-coding-agent` v0.85.1.
**Evidence convention:** `file:line → symbol → implication`. Tags: `[verified]` = read in source (not just docs); `[inferred]` = reasoned from source/docs without a direct implementation; `[external]` = outside the repo.

All paths below are relative to `vendor/pi/` unless stated otherwise. Line numbers are for commit `71dca871`.

---

## Repository Findings

### F1. The built-in tool set is exactly eight tools, five of them read-only

`packages/coding-agent/src/core/tools/index.ts:95` → `export type ToolName = "read" | "bash" | "powershell" | "edit" | "write" | "grep" | "find" | "ls"` → the surface a NAVI tool set must extend is small and explicitly enumerated; `packages/coding-agent/src/core/tools/index.ts:96` → `allToolNames` → the set is a hard-coded `Set`, so adding a NAVI tool is additive, not invasive. `[verified]`

`packages/coding-agent/src/core/tools/index.ts:164` → `createCodingToolDefinitions` (read/bash/edit/write) and `packages/coding-agent/src/core/tools/index.ts:173` → `createReadOnlyToolDefinitions` (read/grep/find/ls) → Pi already ships a *read-only profile* as a first-class constructor; a "research mode" is a supported shape, not a hack. `[verified]`

Every built-in tool exposes a **pluggable operations interface** — `BashOperations` (`src/core/tools/bash.ts:58`; options at `:193`), `ReadOperations` (`src/core/tools/read.ts:35`; options at `:50`), plus `WriteOperations`, `EditOperations`, `GrepOperations`, `FindOperations`, `LsOperations` → execution backends can be swapped (remote/browser/virtual FS) without touching the model-facing schema. `[verified]`

`README.md:41` (coding-agent) → "Pi does not include a built-in permission system for restricting filesystem, process, network, or credential access" → **capability gating is the application's job**, which is exactly the seam NAVI needs for browser actions. `[verified]`

### F2. Images are already a first-class content type — perception needs no core change

`packages/ai/src/types.ts:351` → `TextContent`; `packages/ai/src/types.ts:367` → `ImageContent { type:"image"; data; mimeType }` → the provider content union is **text / thinking / image only**. `[verified]`

`src/core/tools/read.ts` → `detectSupportedImageMimeTypeFromFile` + `processImage` → `read` already ingests jpg/png/gif/webp/bmp and sends them as attachments (auto-resized to 2000x2000 by default). `[verified]`

`src/utils/tool-result-images.ts:22` → `normalizeToolResultImages` → and its docstring at `:15` explicitly names "*extensions, MCP bridges, screenshot tools*" as producers of image tool results → **a screenshot tool is an anticipated extension shape**, and oversized images are normalized once on entry into history. `[verified]`

`README.md:627` (coding-agent) → `pi -p @screenshot.png "What's in this image?"` → the "what am I looking at?" capability already works today as a file attachment. `[verified]`

`docs/settings.md:225` → `images.autoResize` (default `true`) → perception cost/context pressure has an existing control knob. `[verified]`

### F3. On-demand capability loading is real, in two layers (skills + dynamic tools)

**Layer 1 — skills (prompt-level progressive disclosure).** `src/core/skills.ts:74` → `interface Skill`; `src/core/skills.ts:355` → `formatSkillsForPrompt` emits an `<available_skills>` XML block with only name/description/location; the model then uses `read` to load the full `SKILL.md`. `docs/skills.md:96-101` describes this as "progressive disclosure: only descriptions are always in context". `[verified]`

`src/core/skills.ts:80` and `:341` → `disableModelInvocation` from frontmatter `disable-model-invocation` → a capability can be *installed but hidden* from the model and only user-invoked via `/skill:name`. `[verified]`

`docs/skills.md:149` → documents an `allowed-tools` frontmatter field ("experimental"), but `grep allowed-tools` across `packages/` matches **only that doc line** → **skills cannot actually gate tools today**; tool gating must come from extensions. `[verified]`

`docs/skills.md:1` (Repository skill list) → points at `github.com/badlogic/pi-skills` for "*Web search, browser automation, Google APIs, transcription*" → in the Pi ecosystem, **browser automation and transcription are skill-shaped, i.e. CLI scripts + a README**, not core services. `[verified]` (doc claim) `[external]` (the skills themselves)

**Layer 2 — dynamic tools.** `docs/extensions.md:2371-2385` → "Dynamic Tool Loading" lifecycle: register everything, keep a loader tool active, call `pi.setActiveTools([...current, ...matches])` during the loader's execution, and Pi applies the additive change before the next request. `docs/extensions.md:2387-2400` → native deferred loading (`defer_loading` / `tool_reference` / `tool_search`) for Anthropic 4.5+, OpenAI `gpt-5.4+`, Fireworks; `:2402-2408` → safe fallback for every other model, with the caveat that activating a tool that has `promptSnippet`/`promptGuidelines` rebuilds the system prompt and can invalidate the prompt cache. `[verified]`

`src/core/extensions/types.ts:1400` → `getActiveTools()`, `:1406` → `setActiveTools()`, `:1645` → `ToolInfo` → the runtime registry is inspectable and mutable mid-session; `examples/extensions/dynamic-tools.ts` shows registration during `session_start` and at runtime. `[verified]`

**Verdict on "does on-demand loading replace 'features'?"** Partially, and this is the single most important architectural finding:
- It replaces **feature packaging**: capability = skill (instructions + scripts) and/or lazily-activated tool, not a new core module. `[inferred from F3]`
- It does **not** replace **behavior**: anything that must act *without* the model choosing to (ambient page tracking, gating an action, injecting context, surfacing progress) requires an extension subscribing to lifecycle events. `[inferred]`
- It does **not** replace **state**: there is no durable capability registry beyond the session. `[inferred]`

### F4. Prompt templates are the user-invoked mirror of skills

`src/core/prompt-templates.ts:11` → `interface PromptTemplate`; `:70` → `substituteArgs` (`$1`, `$@`, `${1:-default}`, `${@:N:L}`); `:269` → `expandPromptTemplate` → a `/research <topic>` entry point is trivial. `docs/prompt-templates.md:1` → templates are `.md` files with `description` and `argument-hint` frontmatter, discoverable from `~/.pi/agent/prompts/`, `.pi/prompts/`, packages. `[verified]`

`docs/extensions.md:915-920` → processing order for typed input: extension commands → `input` event → skill commands → prompt templates → agent. → there is a **single, ordered interception pipeline**, which is where NAVI's "Point→Ask" and voice-submitted text must land. `[verified]`

### F5. The extension API is a complete ambient-agent substrate

`src/core/extensions/types.ts` (whole file) is the contract. The pieces that matter for NAVI:

| Capability | Evidence |
|---|---|
| Block or rewrite any tool call before execution | `types.ts:1125-1134` `ToolCallEventResult { block, reason, terminate }`; `event.input` is mutable (`docs/extensions.md:786-793`) |
| Rewrite any tool result | `types.ts:1144-1149` `ToolResultEventResult { content, details, isError, usage }` |
| Inject a persistent message + rewrite the system prompt per turn | `types.ts:1156-1160` `BeforeAgentStartEventResult`; `docs/extensions.md:530-565` |
| Prune/rewrite the message list before every LLM call | `types.ts:688-691` `ContextEvent { messages }` |
| Intercept/transform/handle raw user input before expansion | `types.ts:867-883` `InputEvent`, `InputEventResult { continue \| transform \| handled }` |
| Ask the user (blocking) with optional timeout | `types.ts:133-144` `select/confirm/input/notify`; `:98-103` `signal`/`timeout` |
| Two-tier UI: transcript vs durable non-context entries | `types.ts:1219-1223` `EntryRenderer`; `types.ts:1381` `appendEntry`; `docs/extensions.md:1424` "Custom entries do NOT participate in LLM context" |
| Progress surfaces | `types.ts:150` `setStatus`, `:172` `setWidget`, `:144` `notify`, tool `onUpdate` (`types.ts:485-488`) |
| Inter-extension bus | `src/core/event-bus.ts:12` `createEventBus()` (interface at `:3`); `types.ts:1505` `pi.events` |
| Session control (new/fork/switch/tree/reload) | `types.ts:355-389` `ExtensionCommandContext` |
| Tool execution mode per tool | `types.ts:472-479` `executionMode: "sequential" \| "parallel"` |
| Nested LLM calls with usage accounting | `docs/extensions.md:2021` tool returns `usage`; `packages/evals/src/pi-harness.ts:42` `transformSystemPrompt` shows the same hook used programmatically |

→ NAVI can be built as **one extension package** with no core changes. `[inferred]`

### F6. Reference implementations already exist for four of NAVI's hardest patterns

1. **Multi-agent** — `examples/extensions/subagent/index.ts:300` → subagents are spawned as `["--mode","json","-p","--no-session"]` child processes; `:33-36` → `MAX_PARALLEL_TASKS = 8`, `MAX_CONCURRENCY = 4`, `PER_TASK_OUTPUT_CAP = 50KB`; `examples/extensions/subagent/agents.ts` → agent definitions are markdown files with `name/description/tools/model` frontmatter, discovered from `~/.pi/agent/agents` and `.pi/agents`. Modes: single / parallel / chain with `{previous}`. Verified limits from `examples/extensions/subagent/README.md`: "Output truncated to last 10 items in collapsed view", "Parallel model-visible output is capped at 50 KB per task", "Agents discovered fresh on each invocation", "Parallel mode limited to 8 tasks, 4 concurrent". → **feasible, and honestly bounded** — but `--no-session` (`index.ts:300`) means **subagent transcripts are not persisted**; only the final text returns. `[verified]`
2. **Capability gating / read-only mode** — `examples/extensions/plan-mode/index.ts:22` → `PLAN_MODE_TOOLS = ["read","bash","grep","find","ls","questionnaire"]`; `:23` → `NORMAL_MODE_TOOLS = ["read","bash","edit","write"]`; `:170` → `return { block: true, reason: ... }` for non-allowlisted bash; `:181` → `context` handler filters stale `[PLAN MODE ACTIVE]` messages so a mode does not poison later turns. → **tool allowlisting + blocking + stale-context pruning is a working, copyable pattern**, and it is exactly NAVI's action-gating requirement. `[verified]`
3. **State that survives branching** — `examples/extensions/todo.ts` → `reconstructState(ctx)` replays `toolResult.details` from `ctx.sessionManager.getBranch()` on `session_start`/`session_tree`; `examples/extensions/tools.ts` → same pattern via `pi.appendEntry("tools-config", …)` + `custom` entries. → **NAVI's memory/ledger should be branch-local first**: a branch is a fork of the investigation, and state follows it for free. `[verified]`
4. **Machine-readable final output** — `examples/extensions/structured-output.ts` → a terminating tool (`terminate: true`) that ends the turn without a follow-up LLM call; `docs/extensions.md:2025` → early termination only applies when *every* finalized result in the batch is terminating. → the right primitive for "give me the research result as data". `[verified]`

### F7. A planning loop exists, but only as a demonstration

`examples/extensions/plan-mode/index.ts` → `/plan` toggles tool-write suppression, injects a `[PLAN MODE ACTIVE]` message via `before_agent_start` (`:190+`), extracts a numbered `Plan:` list from assistant text at `agent_end`, presents a `ctx.ui.select` menu ("Execute the plan (track progress)" / "Stay in plan mode" / "Refine the plan"), tracks `[DONE:n]` markers at `turn_end`, and renders progress via `setStatus` + `setWidget` (`:59-88`). State persists through `pi.appendEntry("plan-mode", …)` and is restored on resume with an explicit re-scan boundary. `[verified]`

→ NAVI's research plan/progress should reuse this shape verbatim (plan object → explicit human decision point → step markers → widget). Note that both planning and progress are **conventions over the transcript**, not APIs: `usage.md:309` → "It intentionally does not include built-in MCP, sub-agents, permission popups, plan mode, to-dos, or background bash." `[verified]`

### F8. Asking the user is a solved problem with a real protocol

`examples/extensions/question.ts:50` → `executionMode: "sequential"` and `:53` → `ctx.mode !== "tui"` guard → a dialog tool must run sequentially and degrade gracefully headless. `examples/extensions/questionnaire.ts` adds multi-question tabs, per-question answers, and a submit review screen. `examples/extensions/qna.ts` uses the "prompt generator" pattern (model extracts questions → `ctx.ui.setEditorText`).

`docs/rpc.md:1184-1205` → the **Extension UI Protocol**: dialogs (`select/confirm/input/editor`) emit `extension_ui_request` on stdout and block until the client replies with a matching `extension_ui_response`; `notify/setStatus/setWidget/setTitle/set_editor_text` are fire-and-forget; `custom()` returns `undefined`, `setFooter/setHeader/...` are no-ops; `ctx.hasUI` is `true` in RPC mode. `docs/rpc.md:1211-1223` → requests may carry `timeout`, and the agent auto-resolves with a default. `[verified]`

→ **This is the single most important finding for voice and for "Point→Ask":** any surface (terminal, desktop app, phone, voice) can be a Pi client and answer agent questions without core changes.

### F9. Presentation events are split into "for the model" and "for the human"

- `pi.appendEntry(customType, data)` → durable, **not** in LLM context, renderable via `registerEntryRenderer` (`types.ts:1381`, `:1219`).
- `pi.sendMessage({ customType, content, display })` → **is** in LLM context, renderable via `registerMessageRenderer` (`docs/extensions.md:1424`).
- Tool `renderCall`/`renderResult` + `renderShell: "self"` (`types.ts:466-500`) → rich per-tool presentation.
- `onUpdate` inside `execute()` → live streaming progress; `bash.ts` implements throttled snapshots (`src/core/tools/bash.ts`, `BASH_UPDATE_THROTTLE_MS`).
- `ui_prompt_start` / `ui_prompt_end` (`types.ts:745-761`) → explicit "waiting for user" vs "running" signal, "so host/status integrations can report 'waiting for user' instead of just 'running'" (`docs/extensions.md:585`).

→ NAVI's "user-visible research progress" is a **composition** of these, not a new subsystem. `[inferred]`

### F10. Session memory exists; session *search* does not

`packages/agent/src/search/index.ts:1-22` → `SessionSearchService`, `SessionQuery`, `SessionSearchHit`, `EntrySearchHit` — an interface with `searchSessions/searchEntries/sync/notify/remove/close` and **no implementation anywhere in the repo** (`grep SessionSearchService` matches only this file plus tests/docs). `[verified]`

`packages/agent/docs/harness.md:149` → "**S3 — search (§2.8):** design only; the current `src/search/index.ts` skeleton conflicts with it and has no implementation." `packages/agent/docs/post-wp05-roadmap.md:102-110` → S3 requires "standalone catch-up and a separate SQLite FTS5 projection", and warns "Post-filtering after ranked `limit` is unsound"; `:240` lists S3 as item 10 of 11 in the dependency order, i.e. **deliberately late**. `[verified]`

What *does* exist:
- Sessions are JSONL files organized by cwd (`README.md:239-252`); `SessionManager.list/listAll` (`docs/sdk.md:799-800`) enumerates them; `get_entries` with a `since` cursor is a durable client cursor (`docs/rpc.md:717-745`).
- Durable values/lists exist as a storage primitive: `packages/agent/docs/harness.md:212-243` → `value<T>(namespace,key)` / `list<T>(namespace,key)` with a built-in inventory (`pi.session.name`, `pi.entry.label`, `pi.lane.config`, …) → **an application-defined memory namespace is a supported, first-class storage concept** in the durable harness. `[verified]`

→ For v1, "remember this" should be **file-based + branch-local**, with the durable-harness values path explicitly reserved for Phase 2. `[inferred]`

### F11. The durable multi-agent / multi-surface runtime is real but explicitly experimental

- `packages/agent/docs/harness.md:25` → a session has an immutable entry tree, mutable values/lists, **Branches and AgentLanes** ("a session may start with zero of either"), and a usage ledger; `:27` → the harness drives lanes through `accept/drive/requestAbort/inspectExecution`. → *in-process* multi-agent with shared history is a designed primitive (contrast with the subprocess subagent). `[verified]`
- `packages/agent/docs/plugins.md:13-43` → facet kernel / application hosts (server, session worker, presentation) / services; `:108` → "Exactly one process owns a Session's authority at a time"; `:110` → out of scope: "arbitrary undeclared object remoting", "remote hook or tool execution", "a universal remote `AgentHarness`". `[verified]`
- `packages/agent/docs/plugins.md:72-85` → the **question extension** is the canonical example: a model tool call creates an invocation-keyed dialog service; "every connected TUI/web facet observes the service instance"; "the first accepted answer settles it for everyone"; "With no presentation connected, the question remains pending. A TUI or web facet that connects later obtains the same pending question." → **deferred, multi-surface, answer-later interaction is a designed primitive** — precisely what a phone/voice answer to a desktop question requires. `[verified]`
- `packages/protocol/README.md:...` → "The protocol is experimental and has no compatibility guarantees."; `packages/server/README.md:...` → "peer authentication … is not implemented by the experimental Unix transport"; `packages/client/README.md` → "It never reconnects or replays requests automatically."
- `packages/agent/docs/post-wp05-roadmap.md:212-225` → "Optional or deferred product capabilities" includes "standalone S3 search", "authenticated workspace/client authorization for the experimental local server", "generic remote Harness/object capabilities".

→ The correct long-term NAVI topology (server + session workers + per-surface facets) already has a design and partial code, but is **not a v1 foundation**. `[inferred]`

### F12. Cost and risk controls are observable, not enforceable

`grep -i "maxCost|budget|spendLimit|costLimit|maxSteps|maxTurns"` across `src/` matches only retry/compaction/thinking token budgets (`src/core/settings-manager.ts:67` `ThinkingBudgetsSettings`, `docs/settings.md:118-121` compaction, `:175-181` retry) → **there is no spend cap, step cap, or wall-clock cap anywhere in Pi**. `[verified]`

What exists to build one on:
- `ctx.getContextUsage()` → `{ tokens, contextWindow, percent }` (`types.ts:290-296`, `:344`).
- Tool results can report nested LLM `usage` which is persisted in session totals (`docs/extensions.md:2021`, `types.ts:1144-1149`).
- `get_session_stats` over RPC reports tokens, cost and context usage (`docs/rpc.md:554-595`).
- Blocking (`types.ts:1125-1134`) and `ctx.abort()` (`types.ts:338`) are available from inside `tool_call`.
- `examples/extensions/trigger-compact.ts` → `COMPACT_THRESHOLD_TOKENS = 100_000` with a `previousTokens` crossing check at `turn_end` → the exact shape of a threshold governor. `[verified]`

`README.md:509` → "**No background bash.** Use tmux." → long-running work cannot be delegated to a background shell inside Pi; the agent loop or an external supervisor must drive it. `[verified]`

### F13. Checkpoint/notify/summarize/handoff are cheap, complete patterns

- `examples/extensions/git-checkpoint.ts` → `turn_start` runs `git stash create` and indexes the ref by the current leaf entry id; `session_before_fork` offers to restore it. → **"checkpoint before every turn, restore at a branch point"** — NAVI's analogue is page-state/URL checkpoints. `[verified]`
- `examples/extensions/auto-commit-on-exit.ts` → `session_shutdown` reads the last assistant text, generates a commit message, `git add -A && git commit`. `[verified]`
- `examples/extensions/notify.ts` → desktop notification on `agent_settled` — and the comment explains why not `agent_end`: "*Pi may still retry, compact, or continue with queued follow-ups. Notify only after the full run settles.*" `[verified]`
- `examples/extensions/handoff.ts` → nested `ctx.modelRegistry.complete(ctx.model, …, { cacheRetention: "none", sessionId: uuidv7() })` to generate a context-transfer prompt, `ctx.ui.editor` to let the user edit it, then `ctx.newSession({ parentSession, withSession })` to start the focused thread. → **"this investigation is done; continue it as a new focused session"** is a working pattern. `[verified]`
- `examples/extensions/custom-compaction.ts` (`session_before_compact` → summary written by a *different, cheaper model*, returning `{ compaction: { summary, firstKeptEntryId, tokensBefore, usage } }`) and `examples/extensions/summarize.ts` (`ctx.modelRegistry.complete` + custom UI card) → summarization is a pluggable, model-swappable step; a research run can compact on a cheap model while preserving a source list. `[verified]`
- `examples/extensions/ssh.ts` → overrides `read/write/edit/bash` by re-registering tools with the same names and swapping `Operations` to SSH; also rewrites the system prompt's cwd line and intercepts `user_bash`. → **the remote-execution seam is generic**; the same trick would route tools into a browser or a container. `[verified]`

### F14. Evaluation of capabilities is a first-class, comparative workflow

`packages/evals/README.md:3-5` → evals are "behavioral, model-backed checks … adapt a real `AgentSession` to `vitest-evals`, run it in isolated temporary project and agent directories, and attach native Pi session artifacts. Use them to measure end-to-end behavior and compare prompts, tools, skills, models, or other harness configurations." `[verified]`

`packages/evals/src/pi-harness.ts:39-42` → the harness knobs are `noTools`, `tools`, `customTools`, `transformSystemPrompt`; `:291` → `createPiCodingAgentHarness`. `packages/evals/README.md:152-184` → `evalHarnessTable` runs baseline vs candidate with `repetitions`, judges, and reports pass-rate lift in percentage points plus token/latency/cost deltas; `:42-44` → "Use one repetition while developing an eval and five when reporting lift." `[verified]`

→ NAVI can (and should) **prove** that "verify a claim" or "collect conflicting sources" actually helps, with the same tooling used to evaluate skills and prompts. This is the strongest answer to "is this capability worth building?" in the repo. `[inferred]`

### F15. No voice, no audio, no browser — by design

`grep -i "voice|speech|audio|transcri|tts|stt|whisper"` across all `.md` in `packages/` returns exactly one hit: `docs/termux.md:86` → `termux-tts-speak "hello"` in a list of Android external tools. `[verified]`

`packages/ai/src/types.ts:351/367` (content union) and `:782-783` (`/** Price per audio unit. */ audio?: number|string`) → **audio exists only as a cost-rate field; there is no audio content type**. `[verified]`

`README.md:499-511` → "**No MCP.** Build CLI tools with READMEs (see Skills), or build an extension that adds MCP support."; "**No sub-agents.**"; "**No permission popups. Run in a container, or build your own confirmation flow.**"; "**No plan mode.**"; "**No built-in to-dos.**" → the project's stated philosophy is that these are *composition problems, not core features*. `[verified]`

→ Any voice or browser capability is `[external]` to Pi: it must live in a client, a skill script, or an extension, and reach Pi through prompts, tool calls, or RPC. `[inferred, strongly supported]`

### F16. The security boundary is the OS, not Pi

`docs/security.md` → "Pi is a local coding agent. It runs with the permissions of the user account that starts it"; "**No Built-in Sandbox**"; "Project trust is only an input-loading guard … It does not make untrusted code, untrusted prompts, or untrusted model output safe. **Prompt injection from repository files, comments, documentation, context files, or build output is expected local-agent risk and cannot be reliably prevented by pi.**" `[verified]`

`README.md:299-308` (coding-agent) → project trust gates project-local settings/resources/extensions, saved per canonical directory in `~/.pi/agent/trust.json`; non-interactive modes never prompt and fall back to `defaultProjectTrust`. `[verified]`

→ Browsed web content is *untrusted input by definition* and will be concatenated into model context. NAVI inherits the whole prompt-injection problem and has a **strictly worse** input surface than a code repository (attacker-controlled text arrives continuously, including in the framing of the user's own request). `[inferred]`

---

## Architectural Implications

### A1. NAVI's model-facing surface should be ~5 tools, not ~15 features

Every listed capability ("what am I looking at", "explain selection", "verify", "compare pages", "research") is a *composition* over: current page state (context), the model's own reach (tools), and the answer format (structured output). Building one tool per capability would (a) fragment the prompt, (b) make dynamic activation useless, (c) duplicate the same perception/source plumbing. Pi's own evidence: the minimal set is 4 tools by default (`README.md:91`), and the extension ecosystem solves depth with *skills*, not tools. `[inferred from F1/F3]`

### A2. Ambient awareness is an extension concern; perception is a tool concern

Nothing in Pi observes the world on its own. `tool_call`/`context`/`before_agent_start`/`input` are the only places NAVI can act without the model asking (`types.ts:1125/688/1156/867`). Therefore:
- **Pull** (model asks "what am I looking at?") → a tool returning text + `ImageContent`.
- **Push** (NAVI knows what you're looking at) → an extension listening to `input`/`before_agent_start` and injecting a *prunable* context message (plan-mode's `:181` filter is the precedent).

### A3. Two memory tiers, and only one of them is available in v1

- **Branch-local, session-scoped** (available now, cheap, branch-correct): `toolResult.details` replay + `pi.appendEntry` custom entries + `ctx.sessionManager.getBranch()` — proven by `todo.ts` and `tools.ts`.
- **Cross-session semantic** (not available): needs S3 search, which is design-only (`packages/agent/docs/harness.md:149`). The only v1-grade substitute is a **plain file/directory under the project or agent dir that skills and tools can read** (Pi's own philosophy: "Write plans to files", `README.md:505`), plus `grep` over `~/.pi/agent/sessions/*.jsonl`. `[inferred]`

### A4. Capability gating is a three-layer stack, and NAVI must supply all three

1. **What tools exist / are active**: `--tools`, `--exclude-tools`, `defaultTools` (`README.md:581-588`, `docs/settings.md:262-280`), `pi.setActiveTools` (`types.ts:1406`).
2. **What a specific call may do**: `tool_call` → `{ block: true, reason }`, plus `ctx.ui.confirm` (`permission-gate.ts`, `protected-paths.ts`, `plan-mode/index.ts:170`).
3. **What the human can see/undo**: `invoke`/`session_before_fork`/`session_tree` + checkpointing (`git-checkpoint.ts`).

Pi provides the seams; it provides no policy. `[verified F1/F6]`

### A5. The correct client architecture for voice is "RPC client", not "Pi extension"

`docs/rpc.md:3` → RPC mode enables headless operation via a JSON protocol over stdin/stdout "useful for embedding the agent in other applications". `:43-78` → `prompt` accepts text *and images* and returns once accepted/queued; `:124-158` → `abort` and `clear_queue`; `:1184-1191` → dialogs and fire-and-forget display. `docs/sdk.md:19-34` → in-process alternative with `session.subscribe` for token deltas.

A voice front-end therefore needs: mic capture → ASR → `prompt`; Pi event stream (`message_update` `text_delta`) → TTS; `extension_ui_request` (`confirm`/`select`) → spoken question → ASR answer → `extension_ui_response`; `ui_prompt_start/end` → "waiting for you" vs "thinking" acoustic state.

**What must be local:** microphone capture, VAD/barge-in, wake/activation gesture, TTS playback, and — critically — **ASR**, because the utterance and the page context it refers to are both sensitive; streaming audio to a cloud ASR is a browsing-history disclosure. **What may be cloud:** the LLM turn itself (already cloud), optional higher-quality TTS where the *text* is not sensitive, and optional cloud ASR only behind an explicit per-user opt-in. `[inferred from F15 + A5]`

**Latency reality:** Pi is turn-based with multi-second-to-minute tool runs. Voice must therefore be designed as *command + progress announcements + final speech*, not conversational ping-pong; `ui_prompt_start/end` exists precisely so the client can say "waiting for you" instead of "running".

### A6. "Verify a claim" and "autonomous research" are the same primitive with different budgets

Both are: a *plan object* (persisted) + N bounded source-gathering steps + a *skeptic* step + a *structured terminal artifact*. The difference is who reads the artifact and how many steps are allowed. Building them as one "investigation" primitive with parameters (budget, agents, stop condition, output schema) avoids two divergent implementations. `[inferred from F6/F7/F12]`

### A7. Progress must be a ledger, not a spinner

The repo offers a spinner (`setWorkingMessage`/`setWorkingIndicator`) and a structured surface (`setWidget` string lines, `setStatus`, custom entries + entry renderers). Plan-mode already renders a step list in a widget (`plan-mode/index.ts:59-88`). For a long research run, the honest presentation is: **plan step list (done/current/pending), sources collected with counts, running cost/context, and a "stop" affordance.** `[inferred]`

---

## Recommended Approach

### R1. Build NAVI as a single extension package over unmodified Pi (not a fork)

Rationale: every primitive NAVI needs is exported (`README.md` SDK exports list, `docs/sdk.md:1173-1224`); extension packages are installable via npm/git (`README.md:408-457`); the core philosophy explicitly delegates this class of work to extensions (`README.md:497-511`). Concretely: one package with `extensions/` (ambient behavior, gating, governor), `skills/` (browser automation, source-specific fetchers, transcription), `prompts/` (`/research`, `/verify`, `/point-ask`), and `agents/` (research/skeptic agent definitions in the `subagent` markdown format).

### R2. The minimal primitive set (7 primitives, all expressible today)

| # | Primitive | Shape | What it unlocks | Evidence |
|---|---|---|---|---|
| **P1** | **Perception** — one `see` tool | Extension tool returning `TextContent` (accessible DOM/text summary) + optional `ImageContent` (region or full screenshot) + region metadata in `details` | what-am-I-looking-at, explain-selection, Point→Ask, compare pages, browser-action verification | `ai/src/types.ts:367`, `utils/tool-result-images.ts:22`, `read.ts` image path, `settings.md:225` |
| **P2** | **Context bubble** — ambient page state as a prunable injection | Extension: `input` transform and/or `before_agent_start` message `customType:"navi-context"`; `context` handler drops it when stale; `appendEntry` stores the latest snapshot for the UI | "what am I looking at" without asking, Point→Ask disambiguation, compare-with-previous | `types.ts:867/1156/688/1381`, `plan-mode/index.ts:181` |
| **P3** | **Research fetch** — `fetch_page` + `search` tools with a **source ledger** | 2 tools; every result carries `details: { url, title, retrievedAt, excerpt, queryId }`; ledger accumulated in tool-result `details` (branch-correct) and mirrored to a custom entry for display | research from current page, collect supporting/conflicting sources, research graphs, citations | `todo.ts` state pattern, `types.ts:1144-1149`, `README.md:195-197` (`/export`, `/share`) |
| **P4** | **Memory** — two tiers | Tier 1 (v1): branch-local ledger + `pi.appendEntry("navi-memory", …)` + a plain `.pi/navi/memory/*.jsonl` file read via `read`/`grep`. Tier 2 (Phase 2): harness `value`/`list` namespaces | "NAVI, remember this", cross-session recall, per-site preferences | `todo.ts`, `tools.ts`, `packages/agent/docs/harness.md:212-243`, `search/index.ts:1-22` |
| **P5** | **Action with capability gating** — one `act` tool with risk classes | Extension tool + `tool_call` policy: read-only actions auto; state-changing actions require `ctx.ui.confirm` (or a spoken confirm); a "research/read-only mode" toggles `pi.setActiveTools` and blocks non-allowlisted calls | controlled browser actions, verify-by-acting, safe default posture | `plan-mode/index.ts:22/170`, `permission-gate.ts`, `protected-paths.ts`, `types.ts:1125-1134` |
| **P6** | **Presentation events** — ledger widget + status + terminal artifact | `setStatus`/`setWidget` for the step ledger; custom entries + `registerEntryRenderer` for source cards; `structured_output`-style terminating tool for the final artifact; `normalizeToolResultImages` for screenshots | user-visible progress, research graphs (data), shareable dossier | `types.ts:150/172/1219`, `structured-output.ts`, `utils/tool-result-images.ts:22` |
| **P7** | **Delegation + budget governor** | A `subagent`-style tool (2–4 roles: gatherer / skeptic / summarizer) + a governor extension that blocks tool calls and aborts when tokens/steps/time exceed policy | autonomous multi-step research, specialist agents, verification | `subagent/index.ts:300/33-36`, `trigger-compact.ts`, `types.ts:344/1125/338`, `rpc.md:554-595` |

That is the whole v1. Fifteen candidate capabilities map onto it; **no capability needs a primitive that does not exist in the repo today**. `[inferred from the citations above]`

### R3. Capability → primitives mapping

| Candidate capability | Primitives | Fits Pi as | v1 verdict | Why |
|---|---|---|---|---|
| What am I looking at? | P1 (+P2) | Extension tool + skill | **CORE** | Base affordance; images already first-class (`read.ts`, `@screenshot.png`) |
| Explain selected element | P2 + P6 | Prompt template + context bubble (no new tool) | **CORE** | Selection is just a small context payload; no new code path |
| Point→Ask (screen point → question) | P1 (region crop) + P2 + P6 | Extension tool + client gesture | **LIKELY** | Needs a client-side click/region capture; agent side is trivial |
| Compare current page with previous context | P2 + P4 + P6 | Extension (snapshot history) + prompt template | **LIKELY** | Requires deliberately storing snapshots; naive caching bloats context |
| Verify a claim | P7 + P3 + P6 | Prompt template + subagent + structured output | **EXPERIMENTAL** | Verdicts are model judgments; needs eval discipline (`packages/evals`) before promises |
| Research a topic from current page | P2 + P3 + P7 | Skill + prompt template + tools | **CORE** | The flagship; bounded version is buildable now |
| Collect supporting *and conflicting* sources | P3 + P6 + P7 | Extension (skeptic role) + ledger + structured output | **CORE** | Cheapest high-value differentiator; adversarial prompt + per-source provenance |
| Research graphs | P6 (+P4) | Presentation-only (data from ledger) | **EXPERIMENTAL** | Data is trivial; the graph UI is a product decision and RPC/TUI-only for now |
| Controlled browser actions | P5 + P1 | Extension tool + gating + skill scripts | **EXPERIMENTAL** | Start read-only; write actions need gating + undo story |
| Voice interaction | Client of RPC/SDK (A5) | **Separate NAVI client process**, not an extension | **EXPERIMENTAL** | Pi has no audio type; must be local ASR/TTS + RPC; latency profile is command-shaped |
| "NAVI, remember this" | P4 (tier 1) | Extension + file memory | **CORE** | `appendEntry` + file = today's capability; no search needed |
| Autonomous multi-step research | P7 + P3 + P4 + P6 | Extension (governor) + skill + subagent | **EXPERIMENTAL** | Buildable but only with explicit budget + stop + human checkpoint |
| Multiple specialist agents | P7 | Subprocess subagents via markdown agent defs | **LIKELY** (2–3 roles) / **DEFER** (swarm) | `MAX_CONCURRENCY = 4`, 50KB cap, **`--no-session`**: no shared memory, no provenance |
| User-visible research progress | P6 | Extension status/widget/entries | **CORE** | Direct composition of existing surfaces |
| **NEW:** Source ledger with provenance & freshness | P3 + P6 | Extension tool `details` + custom entry | **CORE** | Everything else depends on it; also the audit trail for trust |
| **NEW:** Investigation handoff ("continue this as a focused thread") | P4 + P6 | Prompt-driven `newSession` (copy `handoff.ts`) | **LIKELY** | Working pattern; keeps sessions small and reviewable |
| **NEW:** Shareable dossier export | P6 | `/export` HTML, `/share` gist (`README.md:195-197`) | **LIKELY** | Free; makes research output portable |
| **NEW:** Long-run completion notification | P6 | `agent_settled` + desktop notify (`notify.ts`) | **LIKELY** | Trivial; essential for 10-minute runs |
| **NEW:** Budget/step governor ("spend guard") | P7 | Extension blocking `tool_call` on threshold | **CORE** | Pi has **no** spend cap (`F12`); without this, autonomous runs are unsafe |
| **NEW:** Session recall search | P4 tier 2 | Skill (`grep` over session JSONL) now; S3 service later | **DEFER** | S3 is design-only (`harness.md:149`) |
| **NEW:** Multi-surface deferred questions (answer on your phone later) | P6 + facets | Facet/service (`plugins.md:72-85`) | **DEFER** | Designed and demonstrated, but experimental protocol, no auth |
| **NEW:** Prompt-injection quarantine for page content | P2 + P5 | Extension: provenance-tag injected content + block `act` when a turn's inputs are untrusted-only | **CORE** | `docs/security.md` says injection "cannot be reliably prevented"; NAVI must at least not let it *act* |

### R4. Voice — concrete recommended design (v1 scope)

- **Form factor:** push-to-talk / hotkey activation. No always-on mic. (Privacy; also avoids the "wrong page context" problem.)
- **Transport:** NAVI voice client ⇄ `pi --mode rpc` (`docs/rpc.md:3`). In-process `createAgentSession` (`docs/sdk.md:19-34`) is an acceptable alternative if the client is Node.
- **Path in:** ASR (local) → `{"type":"prompt","message":…}` (`rpc.md:43-49`); page context rides along as a `transform`ed/`before_agent_start`-injected bubble rather than being re-spoken.
- **Path out:** subscribe to `message_update`/`text_delta` events (`sdk.md:269-330`) → sentence-chunked TTS. Speak *only* the final assistant message plus milestone announcements, never raw tool output.
- **Interruption:** `abort` + `clear_queue` (`rpc.md:124-158`); barge-in handled locally by stopping TTS and sending a steer.
- **Confirmation by voice:** make NAVI's dangerous actions go through `ctx.ui.confirm`/`select` (`types.ts:138-139`) so they surface as `extension_ui_request` (`rpc.md:1228-1243`) with a `timeout` (`rpc.md:1193`) — the auto-resolve default must be the *safe* answer.
- **Local vs cloud:** local — mic, VAD, barge-in, TTS playback, **and ASR** (utterance + page context are sensitive). Cloud-acceptable — the LLM turn (already), optional premium TTS on non-sensitive text, optional cloud ASR behind explicit opt-in. There is no audio content type in `pi-ai` (`ai/src/types.ts:351-371`), so *no* audio ever reaches the model as audio: voice is always text at the Pi boundary. `[inferred from F15 + A5]`
- **Degradation:** in `print`/`json` mode `ctx.hasUI` is false (`docs/extensions.md:974`), so confirmations must have a non-UI path (block by default, as `permission-gate.ts` does when `!ctx.hasUI`).

### R5. Autonomous multi-step research — cost/risk controls and progress

**Safeguards (all enforceable through existing seams):**

1. **Plan-first, human-approved.** Generate a plan object, show it (`ctx.ui.select`, as plan-mode does), and only then start. Persist the plan via `appendEntry`.
2. **Budget governor.** Track `ctx.getContextUsage()` (`types.ts:344`) per `turn_end` (the `trigger-compact.ts` shape), plus accumulated nested-model `usage` from tool results (`docs/extensions.md:2021`). On breach: start refusing *new* gathering calls via `{ block: true, reason }` (`types.ts:1125-1134`) and then `ctx.abort()` (`types.ts:338`). Report the breach as a tool result so the model can write up what it has.
3. **Step/agent caps.** Mirror the subagent limits deliberately (`MAX_PARALLEL_TASKS = 8`, `MAX_CONCURRENCY = 4`, `PER_TASK_OUTPUT_CAP = 50KB`, `subagent/index.ts:33-36`) rather than discovering them.
4. **Per-domain fetch policy:** max fetches per domain, robots/`noindex` respect, no credentials, no auth'd browsing in v1.
5. **Action firewall:** while a turn's context is dominated by untrusted page content, `act` is blocked (the plan-mode `tool_call` block pattern, `plan-mode/index.ts:170`). This is the only cheap mitigation for `docs/security.md`'s stated injection risk.
6. **Cheap-model separation:** compacting and summarizing on a cheaper model is a supported pattern (`custom-compaction.ts`); use it so long runs don't pay premium rates to re-read their own history.
7. **Termination:** end with a terminating structured tool (`structured-output.ts`) so the run has a machine-checkable artifact and no extra follow-up turn.

**Progress surfacing:** a step ledger in a widget (`setWidget`, `types.ts:172`) with done/current/pending, source counts (supporting/conflicting), elapsed time and accumulated cost in `setStatus` (`types.ts:150`), source cards as custom entries with entry renderers (`types.ts:1219`), and a desktop notification on `agent_settled` (`notify.ts`) so the user can walk away. In RPC mode the same data survives as `extension_ui_request` fire-and-forget messages (`rpc.md:1293-1324`).

### R6. Capabilities the repo suggests that NAVI's list does not include

1. **Source ledger with provenance, retrievedAt, and excerpts** — prerequisite for verify/compare/graph and the only way to make "conflicting sources" trustworthy. (P3)
2. **Investigation handoff to a fresh session** — copy `handoff.ts`; keeps threads small and reviewable instead of one unbounded session.
3. **Shareable dossier export** — `/export` HTML and `/share` gist already exist (`README.md:195-197`); a research result should be a file, not a transcript.
4. **Budget/step governor (spend guard)** — Pi has no cost cap (`F12`); any autonomous capability without this is irresponsible.
5. **Prompt-injection quarantine** — provenance-tag all injected page content and refuse to let untrusted-only context trigger state-changing actions.
6. **Session recall** — v1 as a skill that greps session JSONL; later as an S3-style search service.
7. **Checkpoint/rollback for actions** — the git-checkpoint analogue (page state + URL + scroll) so an action run can be undone.
8. **Capability discovery for the user** — a `/navi` command listing installed NAVI skills/agents/subcommands, modelled on the `/tools` extension (`examples/extensions/tools.ts`) so capabilities are inspectable rather than hidden.
9. **Result-to-eval pipeline** — turn real research runs into eval cases using `packages/evals` harness tables + judges, so capability changes are measured rather than asserted.
10. **Multi-surface answer capture** (Phase 3) — the deferred-dialog facet design (`plugins.md:72-85`) is the only clean way to answer a desktop NAVI question from a phone or by voice later.

---

## Alternatives

**Alt A — Extension package over stock Pi (RECOMMENDED).** Cheapest path, uses only exported APIs, installable, survives Pi upgrades, benefits from Pi's own evals. Cost: bound to extension lifecycle semantics (event ordering, `withSession` staleness rules at `docs/extensions.md:1266-1305`), and TUI-only features (`custom()`) degrade in RPC/print (`docs/extensions.md:970-974`).

**Alt B — Skills-only NAVI (no extension).** Very cheap; ships as markdown + scripts; no arbitrary code execution, no version pinning problem. But: no ambient page tracking, no action gating, no progress widget, no memory beyond files, and `allowed-tools` is documented-but-unimplemented (`F3`) so skills cannot restrict anything. Rejected as the primary architecture; retained as the delivery vehicle for individual capabilities (browser automation, transcription, site-specific fetchers).

**Alt C — Fork Pi / add NAVI tools to `src/core/tools`.** Rejected. The tool set is enumerated (`tools/index.ts:95`), the project delegates this class of work to extensions (`README.md:497-511`), and this research explicitly must not modify the vendor clone. A fork also forfeits compatibility with `pi update`, packages, and the eval harness.

**Alt D — Separate NAVI service on the durable harness (server + session workers + facets).** This is the *right* Phase 3 architecture: it is the only design that gives in-process multi-agent lanes (`harness.md:25-27`), durable application state (`harness.md:212-243`), session search (S3), multi-surface deferred dialogs (`plugins.md:72-85`), and multi-device attachment (`server/README.md`). It is *wrong* for v1: "experimental and has no compatibility guarantees" (`protocol/README.md`), "peer authentication … is not implemented" (`server/README.md`), search unimplemented (`harness.md:149`), and 10 of 11 roadmap items precede S3 (`post-wp05-roadmap.md:240`). Decision: design NAVI's v1 extension so its ledger/memory shapes can migrate into harness values/lists later.

**Alt E — NAVI as a "presentation host" from day one (own UI, Pi as engine).** Attractive for voice and Point→Ask (clients own the mic and the screen). Recommended as the *client* architecture (RPC/SDK) while keeping the *agent* as stock Pi. Do not build the client as an extension.

**Alt F — Use MCP as the integration bus.** Rejected as the primary bus: Pi has no MCP support (`README.md:499`), and the project's stated reason is exactly NAVI's use case ("build CLI tools with READMEs, see Skills"). A NAVI MCP *client* extension is acceptable if NAVI must consume a third party's tools, but NAVI's own capabilities should be Pi tools/skills.

**Alt G — One tool per capability.** Rejected (see A1). Fifteen tools in the prompt is the opposite of Pi's minimalism and negates dynamic activation.

---

## Risks

| # | Risk | Severity | Evidence / mitigation |
|---|---|---|---|
| R1 | **Prompt injection from browsed content**, escalating to actions or memory poisoning | **Critical** | `docs/security.md`: injection "cannot be reliably prevented by pi". Mitigation: provenance tags on injected content, `act` blocked while untrusted-only context dominates a turn, no credentials in the browser profile, memory writes require explicit user confirmation |
| R2 | **Unbounded cost** — no spend/step/time cap exists anywhere in Pi | **High** | `F12` (`grep` found only retry/compaction/thinking budgets). Mitigation: P7 governor, hard caps, cheap-model compaction, terminal artifact to stop the loop |
| R3 | **No sandbox, no permission system** — browser automation runs with the user's full permissions and real logged-in cookies if the wrong profile is used | **High** | `README.md:41`, `docs/security.md`; `docs/containerization.md` offers only whole-process isolation. Mitigation: dedicated throwaway browser profile, read-only actions in v1, container for autonomous runs |
| R4 | **Subagent work is not persisted** — `--no-session` means specialist output exists only as returned text, truncated | **Medium** | `subagent/index.ts:300`; README limits: 10-item collapsed view, 50KB per parallel task. Mitigation: subagents must return *structured* findings that the parent writes to its ledger; never rely on subagent transcripts |
| R5 | **Context bloat from perception** — screenshots are large and every tool-result image is re-sent on every subsequent request | **Medium** | `utils/tool-result-images.ts:12-19` states exactly this; `docs/settings.md:225` `images.autoResize`. Mitigation: region crops over full screenshots, images dropped from history after the turn that used them (`context` event pruning), text-first default |
| R6 | **Memory leaks sensitive content** by default | **Medium** | memory tier 1 = files + session JSONL; sessions auto-save under `~/.pi/agent/sessions/` (`README.md:243`). Mitigation: per-site namespaces, explicit "remember" gesture, deletion command, never store page bodies verbatim |
| R7 | **Voice latency & privacy** | **Medium** | no audio type (`F15`); ASR/TTS must be local for privacy, which costs quality; Pi turn latency is seconds-to-minutes. Mitigation: command-shaped voice (not chat), milestone speech, safe-default timeouts on spoken confirms |
| R8 | **Verdict/verification overconfidence** — "verify a claim" produces a plausible judgment, not a proof | **Medium** | No judge/ground-truth machinery in-product; `packages/evals` shows model-backed judges are the accepted method (`evals/README.md:160-184`) with the caveat "trustworthy judges" (`:198`). Mitigation: never present a verdict without the artifact (sources, quotes); measure with evals; label confidence |
| R9 | **Relying on experimental APIs** (protocol v8, server, harness S3/lanes) reworks | **Medium** | `protocol/README.md` "no compatibility guarantees"; `harness.md:143-158` lists stubs; `post-wp05-roadmap.md:240` orders S3 late. Mitigation: keep v1 in the extension layer with a migration-shaped internal API |
| R10 | **Extension trust model** — NAVI is arbitrary code, and so is every third-party skill it loads | **Medium** | `docs/skills.md:32` ("Review skill content before use"), `README.md:412`, `docs/extensions.md:111`. Mitigation: pin versions, vendor skills, keep NAVI's permissions story explicit to the user |
| R11 | **Windows/desktop target friction** | **Low–Medium** | This workspace is Windows; Pi supports it (`docs/windows.md`, powershell tool at `tools/powershell.ts`) but shell/browser automation scripts and permission prompts differ. Mitigation: prefer Node-based automation over shell scripts; avoid bash-only skills |
| R12 | **"No background bash"** limits long-running collection | **Low–Medium** | `README.md:509`. Mitigation: drive long work through the agent loop with the governor, or supervise externally (tmux/external process) and feed results back |

---

## Feature Assessment

**CORE** — v1 requires it; the repo supports it directly.
**LIKELY** — high value, small incremental cost; schedule after CORE.
**EXPERIMENTAL** — build behind a flag, measure with evals, expect redesign.
**DEFER** — right idea, wrong time (missing dependency in Pi).
**REJECT** — do not build as specified.

| Feature | Verdict | One-line rationale |
|---|---|---|
| See current page ("what am I looking at?") | **CORE** | Images are a first-class content type and `read`/tool results already carry them (`ai/src/types.ts:367`, `utils/tool-result-images.ts:22`) |
| Explain selected element | **CORE** | Selection is a small context payload over an existing prompt; no new primitive |
| Ambient context bubble (page state → prompt) | **CORE** | `input`/`before_agent_start`/`context` are the designed seams, with plan-mode's prune filter as precedent |
| Source ledger with provenance | **CORE** | Every research/verify capability depends on it; ledger in tool `details` is branch-correct for free |
| "NAVI, remember this" (branch + file memory) | **CORE** | `appendEntry`/tool-result-details replay is the established pattern (`todo.ts`, `tools.ts`) |
| Research a topic from the current page (bounded) | **CORE** | Compose P2+P3+P7; a fixture of the design |
| Collect supporting **and conflicting** sources | **CORE** | Cheapest real differentiator; requires an adversarial role and provenance |
| User-visible research progress (ledger widget + status) | **CORE** | Composition of `setStatus`/`setWidget`/entry renderers; plan-mode is the template |
| Budget / step / time governor | **CORE** | Pi has no spend cap at all (`F12`); autonomy without it is unsafe |
| Prompt-injection quarantine for page content | **CORE** | `docs/security.md` concedes injection is unpreventable; NAVI must at least prevent *action* |
| Point→Ask (screen point → question) | **LIKELY** | Agent side is trivial; needs a client gesture + region crop |
| Compare current page with previous context | **LIKELY** | Requires deliberate snapshot history; naive caching bloats context |
| Desktop notification on long-run completion | **LIKELY** | One `agent_settled` handler (`notify.ts`) |
| Investigation handoff to a fresh session | **LIKELY** | Working pattern (`handoff.ts`); keeps threads small |
| Shareable research dossier (`/export`, `/share`) | **LIKELY** | Already exists (`README.md:195-197`); makes output portable |
| 2–3 specialist agents (gatherer / skeptic / summarizer) | **LIKELY** | `subagent` markdown agent defs; keep to ≤3 and require structured findings |
| Capability discovery UI (`/navi`) | **LIKELY** | Modelled on `/tools` (`examples/extensions/tools.ts`); capabilities should be inspectable |
| Voice interaction | **EXPERIMENTAL** | No audio type in `pi-ai`; must be a local ASR/TTS RPC client; latency is command-shaped |
| Verify a claim | **EXPERIMENTAL** | Verdict is a model judgment; ship only with artifacts + eval measurement |
| Autonomous multi-step research | **EXPERIMENTAL** | Buildable, but needs plan-first, budget, stop condition, and human checkpoint |
| Controlled browser actions (write/state-changing) | **EXPERIMENTAL** | Start read-only; writes need gating, dedicated profile, and a rollback story |
| Research graphs (visual UI) | **EXPERIMENTAL** | Data is trivial; the graph surface is a product bet, TUI/RPC-limited today |
| Always-on ambient listening (no activation gesture) | **REJECT** | Mic always-on plus attacker-controlled page content is an unacceptable privacy/injection combination |
| Feature-per-capability modules (15 tools / 15 flags) | **REJECT** | Violates Pi's minimalism and defeats dynamic activation (`A1`) |
| Unbounded "automatic source collection" without a visible ledger/budget | **REJECT** | Cost and trust blow-up; the ledger is the product |
| MCP as NAVI's integration bus | **REJECT** | Pi has no MCP by design (`README.md:499`); use tools/skills |
| Own agent runtime / research DB service in v1 | **REJECT** | Duplicates the session/compaction/event model; durable harness is not stable enough |
| Cross-session semantic memory (search/embeddings) | **DEFER** | `SessionSearchService` is an unimplemented skeleton; S3 is late in the roadmap (`harness.md:149`) |
| Multi-surface deferred questions (answer later from phone) | **DEFER** | Design + partial code exist (`plugins.md:72-85`) but the protocol is unauthenticated and version-unstable |
| In-process agent lanes for concurrent specialists with shared memory | **DEFER** | Designed in the durable harness (`harness.md:25-27`) but experimental; subprocess subagents cannot share state |
| Sandboxed/containerized autonomous browsing by default | **DEFER** | `docs/containerization.md` patterns exist, but this is an ops deliverable, not a NAVI feature |

---

## Open Questions

1. **What exactly is "the current page"?** URL + title + visible text + DOM summary + screenshot + selection + scroll position is four different payload sizes with different fidelity. Which is the default, and what is the escalation rule to a screenshot? (Cost/context impact: `utils/tool-result-images.ts:12-19`.)
2. **ASR/TTS: which engine, and is local-only acceptable?** If local-only, what is the quality floor? If cloud, what is the consent ceremony, and does the page context count as part of the audio disclosure?
3. **Where does memory live, and is it user-inspectable/deletable?** Per-site namespace vs global; project-scoped vs user-scoped; exports. (Basis: `harness.md:212-243` namespaces, `pi.appendEntry` entries, or plain files.)
4. **What is the default budget?** Tokens, cost, wall-clock, and step count — and what happens at breach: hard stop, write-up-what-we-have, or ask? (`F12`: no precedent in Pi.)
5. **Do we ship verification verdicts at all in v1,** or only evidence bundles with explicit "no verdict" framing? (Depends on whether evals can show lift: `evals/README.md:152-184`.)
6. **Which browser integration?** Extension API vs CDP vs accessibility tree — it determines whether NAVI sees pixels or semantics, and how much of P1/P5 is engineering vs plumbing.
7. **Is the action surface read-only in v1?** If not, what is the rollback story and which profile/credentials do actions use?
8. **Who owns the browser profile?** A dedicated throwaway profile is safe but unhelpful for logged-in research; the user's real profile is helpful and dangerous (`docs/security.md`).
9. **How do we keep the model's context clean when injected page context is stale?** The plan-mode filter (`plan-mode/index.ts:181`) is a string-match hack; a provenance-typed context entry would be the principled version.
10. **Do we need multi-device (voice on phone + browser on desktop) in v1, or is single-device enough?** This decision determines DEFER-vs-EXPERIMENTAL for the facet design (`plugins.md:72-85`).
11. **What is the eval ground truth for "verification quality"?** Without ground-truth claims, "verify" improves fluency, not correctness.
12. **Where is the extension/skill boundary, really?** If skills cannot gate tools (`allowed-tools` unimplemented, `F3`), does NAVI ship a thin extension + fat skills, and accept that skills run with full user permissions (`docs/skills.md:32`)?

---

*Prepared from read-only inspection of the vendor clone at `C:\Users\lain\NAVI\vendor\pi` (commit `71dca871`). No files inside the repo were modified.*
