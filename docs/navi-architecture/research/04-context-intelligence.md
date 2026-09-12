# 04 — Context Intelligence & Token Efficiency for NAVI

Investigation area 4 of 10. Design research only — no implementation. Evidence base: read-only clone at
`C:\Users\lain\NAVI\vendor\pi` (earendil-works/pi, `main`, commit `71dca871`, 2026-09-11, `@earendil-works/pi-coding-agent` v0.85.1).
All repo citations are `path:line → symbol → implication`, tagged `[verified]` (read in `src`), `[inferred]`, or `[external]`.

**Thesis.** NAVI's core risk is "a big page becomes a big prompt". Pi already owns the *harness* half of that problem
per-LLM-call context transformation, system-prompt assembly, tool-output truncation, token accounting, prompt-cache
placement, and compaction. NAVI must own only the *perception* half — raw browser state → page understanding →
semantic representation → relevance scoring — and then hand a small, append-only, budgeted artifact to Pi's existing
insertion points.

---

## 1. Repository Findings

### 1.1 What Pi injects into the system prompt, and when

- `packages/coding-agent/src/core/system-prompt.ts:28 → buildSystemPrompt() →` the system prompt is assembled from
  static inputs: tool list (`:83-84`, only tools that supply a one-line `promptSnippet` appear), guidelines
  (`:87-125`), Pi-docs pointers (`:137-144`), `<project_context>` files (`:151-158`), skills (`:161-163`), and cwd
  (`:165`). `[verified]` Implication: the system prompt is a *session-scoped* artifact with no per-page channel other
  than the per-turn `before_agent_start` hook. Page context must not be routed here.
- `core/resource-loader.ts:119 → loadProjectContextFiles() →` walks cwd → filesystem root collecting
  `AGENTS.override.md | AGENTS.md | AGENTS.MD | CLAUDE.md | CLAUDE.MD` (`:72`), always prepending the global
  agent-dir file (`:129-133`), with worktree-shadow dedup (`:101-117`) and path dedup (`:144-147`). `[verified]`
  Implication: Pi's existing "eager context" precedent is *unbounded* — nothing on this path calls `truncateHead`, so
  there is no size cap on injected project context. NAVI must not copy this pattern for pages.
- `core/resource-loader.ts:515-524 → this.agentsFiles = resolvedAgentsFiles.agentsFiles →` context files are loaded on
  every `reload()` and are `agentsFilesOverride`-customizable. `[verified]`
- `core/resource-loader.ts:1023 → discoverSystemPromptFile() →` `.pi/SYSTEM.md` (project, only when project-trusted
  `:1025`) else `~/.pi/agent/SYSTEM.md`; `:1037 → discoverAppendSystemPromptFile() →` the same for
  `APPEND_SYSTEM.md`, surfaced via `getAppendSystemPrompt()`. `[verified]` Implication: stable textual policy belongs
  in these files, not in code and not per page.

### 1.2 The extension surfaces that matter for context injection

- `core/extensions/types.ts:688-691 → ContextEvent { type:"context"; messages: AgentMessage[] } →` "Fired before each
  LLM call. Can modify messages." Return type `ContextEventResult { messages?: AgentMessage[] }` at
  `:1119-1121`; registered at `:1275`. `[verified]`
- `core/extensions/runner.ts:1034-1064 → emitContext() →` deep-copies with `structuredClone(messages)` (`:1036`), runs
  every extension's `context` handlers in load order, chains each handler's returned `messages` into the next handler
  (`:1047-1049`), and swallows handler errors into diagnostics (`:1050-1058`). `[verified]` Implication: the `context`
  hook is a **per-LLM-call, chainable, non-destructive** injection point — the correct home for a page snapshot or
  page delta.
- `core/sdk.ts:362-366 → transformContext: async (messages) => runner.emitContext(messages) →` wires the hook into the
  agent; `packages/agent/src/agent-loop.ts:288-290 → if (config.transformContext) messages = await
  config.transformContext(messages, signal) →` runs inside `streamAssistantResponse`, i.e. **once per provider call**,
  including each tool-loop iteration. `[verified]` Implication: mid-turn injection works without a new user turn
  (each tool batch gets a fresh transform pass).
- `core/extensions/types.ts:694-697 → BeforeProviderRequestEvent { type:"before_provider_request"; payload: unknown } →`
  "Can replace the payload" (`:693`); result type is plain `unknown` (`:1123`); `runner.ts:1066-1098 → emitBeforeProviderRequest()`; wired at
  `core/sdk.ts:343-349 → onPayload →`. `docs/extensions.md:705-720` states handlers run in load order, returning
  `undefined` keeps the payload, and "this is mainly useful for debugging provider serialization and cache behavior."
  `[verified]` Implication: last-resort hook; bypasses Pi's accounting (`docs/extensions.md:1105`) — debug only.
- `core/extensions/types.ts:290-296 → ContextUsage { tokens: number|null; contextWindow: number; percent: number|null } →`
  `:344 → ctx.getContextUsage(): ContextUsage | undefined →`, `:346 → ctx.compact(options?: CompactOptions) →`,
  `:298-302 → CompactOptions { customInstructions?, onComplete?, onError? } →`. `[verified]`
- `core/agent-session.ts:3411-3455 → getContextUsage() →` returns `tokens: null` after a compaction until a
  post-compaction assistant usage exists (`:3418-3444`), otherwise `estimateContextTokens(this.messages)` (`:3447`) and
  `percent` (`:3448`). `core/agent-session.ts:2662 → getContextUsage: () => this.getContextUsage() →` is the exposed
  action. `[verified]` Implication: usage is derived from **session state**, not from the transformed message view.
- `core/extensions/types.ts:594-604 → SessionBeforeCompactEvent { preparation, branchEntries, customInstructions?, reason:
  "manual"|"threshold"|"overflow", willRetry, signal } →` can cancel or supply a custom summary; `:607-615 →
  SessionCompactEvent →`; `:618-630 → SessionCompactFailedEvent (errorMessage, aborted, willRetry, fromExtension) →`.
  `[verified]`
- `core/extensions/types.ts:717-727 → BeforeAgentStartEvent { prompt, images?, systemPrompt, systemPromptOptions } →`
  per-turn system-prompt mutation point; pattern demonstrated in
  `examples/extensions/claude-rules.ts:64-85` (append an **index** of loadable files, not the files themselves) and
  `docs/extensions.md:547-563`. `[verified]` Implication: this is exactly NAVI's model for "here are the sections
  available; fetch on demand" — applied to page sections instead of rule files.
- `core/extensions/types.ts:451-500 → ToolDefinition →` `:458-459 promptSnippet` opts the tool into the `Available
  tools` list, `promptGuidelines` into `Guidelines`, `:482-488 execute(...) → Promise<AgentToolResult>`.
  `docs/extensions.md:1375-1381,1921-1931` cover registration, dynamic tool loading via `pi.setActiveTools()`
  (`:1377`, `:2373`), and the warning that `promptGuidelines` bullets must name their tool. `[verified]`
- `core/extensions/types.ts:1365-1368 → sendMessage(message, options?: { triggerTurn?; deliverAs?:
  "steer"|"followUp"|"nextTurn" }) →` plus `ReplacedSessionContext.sendMessage` (`:397-400`); semantics in
  `docs/extensions.md:1422-1443` (`steer` = delivered after the current assistant turn's tool calls, before the next
  LLM call; `triggerTurn` wakes an idle agent). `[verified]` Implication: how a page change can interrupt mid-run.

### 1.3 Tool-output limits and truncation strategy (Pi's template for page content)

- `core/tools/truncate.ts:11-12 → DEFAULT_MAX_LINES = 2000; DEFAULT_MAX_BYTES = 50 * 1024 →` two independent limits,
  first hit wins. `:78 → truncateHead()` keeps the head and never returns partial lines; if the first line alone exceeds
  the byte limit it returns empty content with `firstLineExceedsLimit` (`:104-119`). `:168 → truncateTail()` keeps the
  tail and *does* allow a partial trailing line (`:199-218`). `:268 → truncateLine()` (grep matches, 500 chars,
  `:13 GREP_MAX_LINE_LENGTH`). `[verified]`
- `core/tools/read.ts:73 →` the tool description declares the limits to the model; `:150 → truncateHead(selectedContent)
  →`; `:152-155` when the first line is too large the result becomes an actionable instruction
  (`Use bash: sed -n 'Np' path | head -c 51200`); `:162-166` appends `[Showing lines X-Y of N. Use offset=… to
  continue.]`; `:156,167` return `details: { truncation }`. `[verified]` Implication: Pi's contract for oversized content
  is **truncate + explicit continuation affordance + structured details out of context**. A page snapshot should follow
  the same contract verbatim.
- `docs/extensions.md:2176-2209 →` custom tools "MUST truncate their output" (50 KB / 2000 lines, whichever first),
  full output to a temp file, and `truncateHead`/`truncateTail`/`truncateLine`/`DEFAULT_MAX_BYTES`/`DEFAULT_MAX_LINES`
  are the sanctioned exports. `[verified]`

### 1.4 Compaction: what it preserves, what it drops, what it costs

- `core/compaction/compaction.ts:132-136 → DEFAULT_COMPACTION_SETTINGS { enabled: true, reserveTokens: 16384,
  keepRecentTokens: 20000 } →`; `:235-238 → shouldCompact(contextTokens, contextWindow, settings) →` trigg when
  `contextTokens > contextWindow - reserveTokens`. `[verified]`
- `:266-306 → estimateTokens(message) →` is the chars/4 heuristic (image blocks counted as
  `ESTIMATED_IMAGE_CHARS = 4800`, `:244`), explicitly conservative. `[verified]` Implication: NAVI's own byte budget
  arithmetic should use the same heuristic so numbers are comparable; vision is expensive by this ruler.
- `:403-461 → findCutPoint() →` walks backwards accumulating estimates until `keepRecentTokens`, cutting only at
  user/assistant/bashExecution/custom/branchSummary/compactionSummary messages and never at tool results
  (`:308-321`); `:750-829 → prepareCompaction() →` produces `CompactionPreparation` (`:732-748`:
  `messagesToSummarize`, `turnPrefixMessages`, `isSplitTurn`, `tokensBefore`, `previousSummary`, `fileOps`, `settings`);
  `:858-964 → compact() →` merges a history summary and a split-turn prefix summary (`:887-947`) and appends
  `<read-files>`/`<modified-files>` lists (`:950-951`). `:545-553 → getSummarizationFailure() →` refuses to persist a
  summary that hit the token cap. `[verified]`
- Summarization cost shape: `buildSummarizationContext()` (`:642-653`) sends a summarization system prompt + one user
  message; the conversation is flattened to text by `serializeConversation()` with **tool results truncated to 2000
  characters** (`core/compaction/utils.ts:88-99,144`); `:589-593 → completeSummarization() →` forces
  `cacheRetention: "none"` and a fresh `sessionId` — i.e. **summaries deliberately do not write prompt cache**;
  output budget is `min(0.8 * reserveTokens, model.maxTokens)` (`:672-675`). `[verified]`
- `docs/compaction.md:37` documents *where* the threshold is checked (after tools, before the next assistant response;
  before a new user prompt; after a run ends), `:111-119` the cut-point rules, `:271` the 2000-char tool-result cap,
  `:277-347` the `session_before_compact` extension contract, `:398-442` settings and per-model `modelOverrides`
  (with the note that `reserveTokens` also caps summarization output). `[verified]`
- `core/agent-session.ts:538-555 → _compactBeforeNextAssistantResponse() →` checks
  `shouldCompact(estimateContextTokens(context.messages).tokens, model.contextWindow, settings)` and then rebuilds the
  context from `this.agent.state.messages.slice()` (`:553`). `[verified]` **This is the single most important repo
  finding for NAVI:** context invented inside `transformContext`/the `context` hook is not part of
  `context.messages` at this checkpoint, so Pi's auto-compaction threshold and `getContextUsage()`
  (`agent-session.ts:3447`) both **under-count NAVI-injected page context**.
- `core/agent-session.ts:2161-2185 →` overflow detection and recovery compare against `this.model?.contextWindow`;
  `:2254-2256` is the threshold-triggered `_runAutoCompaction("threshold", false)`. `[verified]`

### 1.5 Token accounting and cache instrumentation

- `core/usage-totals.ts:22-28 → addUsageToTotals() →` accumulates `input/output/cacheRead/cacheWrite/cost`;
  `:37-70 → getUsageCostBreakdown(entries) →` buckets by `provider/model`, and puts tool-result usage plus
  `branch_summary`/`compaction` usage into a single **`"Tools/summaries"`** bucket. `[verified]` Implication: NAVI's
  auxiliary model calls (page understanding) should carry `usage` so they land in a visible bucket rather than
  disappearing.
- `core/cache-stats.ts:8 → CACHE_TTL_MS = 5 * 60 * 1000 →` (Anthropic default TTL); `:11 → NOISE_FLOOR_TOKENS = 1024 →`;
  `:56-90 → detectMiss() →` computes `missedTokens = min(prevPromptTokens, promptTokens) - cacheRead` and prices the
  miss from the paid-vs-read rate spread; `:104-132 → scan() →` **resets the previous-request baseline at every
  `compaction` / `branch_summary` entry** (`:113-119`) so post-compaction turns are not counted as misses, but model
  switches *are* counted (`:116`). `[verified]` Implication: NAVI gets a free, principled cache-miss meter — reuse it
  instead of inventing one.
- `packages/ai/src/types.ts:383-404 → Usage { input, output, cacheRead, cacheWrite, cacheWrite1h?, reasoning?,
  totalTokens, cost{...} } →`; `:859-860 → Model.contextWindow; Model.maxTokens →`; `:204 → StreamOptions.cacheRetention
  ("short" default) →`; `:206 → sessionId →`. `[verified]` Implication: budgets must be derived from the *model object*
  (per-model windows vary; `packages/coding-agent/src/cli/list-models.ts:68` renders `m.contextWindow`), not hard-coded.
- `packages/ai/src/api/anthropic-messages.ts:1027 → buildParams() →` resolves `cache_control`;
  `:1081-1090` attaches it to the system block; `:1374-1381` attaches it to the **last user message**. `[verified]`
  Implication: Pi's own caching posture is "cache the static prefix + the rolling tail". Appending a new trailing
  message preserves the prefix read; rewriting an older message or the system prompt invalidates from that point on.
- Exports NAVI can import instead of rebuilding: `src/index.ts:28-50` (`calculateContextTokens`, `compact`,
  `DEFAULT_COMPACTION_SETTINGS`, `estimateTokens`, `findCutPoint`, `findTurnStartIndex`, `generateSummary`,
  `generateSummaryWithUsage`, `getLastAssistantUsage`, `prepareBranchEntries`, `serializeConversation`,
  `shouldCompact`), `src/index.ts:283-339` (`truncateHead`, `truncateLine`, `truncateTail`,
  `DEFAULT_MAX_BYTES`, `DEFAULT_MAX_LINES`, `formatSize`, tool factories, `withFileMutationQueue`),
  `src/index.ts:70` (`ContextUsage` type). `[verified]`
- Auxiliary model calls from an extension: `core/model-registry.ts:115-117 → streamSimple(model, context, options) →`
  (and `:107-112 stream`, `:119-124 complete`), documented at `docs/extensions.md:1021` as the supported way to get
  provider-neutral options with resolved auth. `[verified]` Implication: the T4 "page understanding pass" needs no new
  provider plumbing; it is one `streamSimple` call with `cacheRetention: "none"` and its own usage record.

### 1.6 External evidence (perception layer and caching)

- `[external]` Full accessibility-tree snapshots are **not small**: measured on 2026-02-26 with Playwright MCP
  v0.0.68 returning untruncated trees — Wikipedia (MCP article) ≈ **16,044 tokens** (64,176 chars), GitHub repo page
  ≈ **19,409 tokens** (77,637 chars), Hacker News front page ≈ **14,547 tokens** (58,189 chars). A compacter format
  (refs only on interactive elements, no `[cursor=pointer]`, W3C accessible-name order, one-line table rows) cut the
  same pages by 51–79%. (dev.to, kuroko1t, "How Accessibility Tree Formatting Affects Token Cost in Browser MCPs".)
- `[external]` The same source enumerates the levers that actually move the number: which nodes get refs, which
  attributes are emitted, how tables collapse, whether text content is included at all (`interactiveOnly`), and
  landmark-region focus (`focusRegion`). Playwright MCP performs full-tree only (no region/interactive modes).
- `[external]` Delta snapshots work: after a full snapshot, emissions are restricted to changed subtrees with
  `[unchanged]` references to earlier refs, reported as ~94% data reduction (5 KB → ~300 B for 100+ elements) on
  multi-step tasks (sudonull.com ARIA-snapshot analysis; treat the 94% as directional, not a benchmark).
- `[external]` Per-step cost in a real browser-agent loop (dashboard-style task, 7 steps): snapshot inputs of
  8k/6k/22k/16k/20k/14k/2k tokens ≈ 88k input tokens before retries; the recommendation is to budget **cost per
  completed task** and cap observations (`max_snapshots_per_task`, `max_snapshot_tokens`, `summarize_after_steps`).
  (baeseokjae.github.io, "Browser MCP Snapshot Token Cost 2026".)
- `[external]` Perception ladder used by production browser agents: search → fetch/extract → accessibility snapshot →
  screenshot; use the expensive rung only when interaction or visual state is the actual problem.
- `[external]` Main-content extraction is a mature problem with measurable quality differences across page types:
  Mozilla Readability and trafilatura are the standard baselines; the Web Content Extraction Benchmark (WCXB) has
  2,008 annotated pages across **7 structurally distinct page types**, which is direct support for page-type adapters
  rather than one universal extractor.
- `[external]` Anthropic prompt caching mechanics that constrain NAVI's injection design: cache prefix order is
  `tools → system → messages`; up to **4** explicit breakpoints; writes happen only at breakpoints and reads look back
  at most **20 blocks**; cache hits require 100% identical prefix up to the breakpoint; minimum cacheable prefix is
  model-dependent (512 tokens for Opus 5 / Fable 5; 1,024 for Opus 4.8 / Sonnet 4.5–5; 2,048 for Opus 4.7 / Haiku 3.5;
  4,096 for Opus 4.5–4.6 and Haiku 4.5) and silently no-ops below it; TTL is 5 minutes by default and is refreshed on
  use; adding/removing **images anywhere** invalidates message caches; a mid-conversation `{"role":"system"}` message
  preserves the cached prefix on some models (not Sonnet 5).

---

## 2. Architectural Implications

**I1 — Route page context through the per-call transform, never the system prompt.** `[verified]`
`runner.ts:1034-1064` already gives NAVI a per-LLM-call, chainable, error-isolated message transform. Editing the
system prompt per page (`before_agent_start`) rebuilds the static prefix and invalidates the tools+system cache level
(`[external]` invalidation table), and Pi warns about this pattern for tool activation (`docs/extensions.md:2404-2408`).

**I2 — Pi's threshold logic cannot see NAVI's injections, so NAVI must keep its own ledger.** `[verified]`
`agent-session.ts:545` and `:3447` both measure `context.messages` / session entries, not the transformed view.
Consequences: (a) a 4k-token page block injected via the `context` hook is invisible to `shouldCompact`; (b) if NAVI
ever overshoots, the failure mode is a provider context-overflow error ("overflow" compaction reason,
`types.ts:600`) rather than a graceful pre-emptive compaction. NAVI must therefore (i) enforce its own ceiling before
injecting, and (ii) call `ctx.compact()` at its own threshold when its ledger plus `getContextUsage().tokens`
approaches `contextWindow - reserveTokens`.

**I3 — Append-only is the cache strategy.** `[verified]` + `[external]` Pi marks cache breakpoints on the system block
and the last user message (`anthropic-messages.ts:1081-1090,1374-1381`); Anthropic's prefix hash is cumulative, writes
occur only at breakpoints, and the backward lookback is 20 blocks. Therefore: append each new page view as a **new
trailing message**, never mutate previous page blocks; supersede them by version ("page_context v3") and let
compaction retire the older ones. Rewriting an earlier block costs a full re-write of everything after it.

**I4 — Pi's truncation contract is the page-content contract.** `[verified]` `truncateHead` + explicit
`Use offset=… to continue.` + `details` out of context (`read.ts:150-172`) is precisely what an oversized page
snapshot needs. Reuse the exported helpers; do not write a new truncator.

**I5 — Compaction is not a page cache.** `[verified]` Compaction summarizes *messages to text* with tool results capped
at 2000 chars (`compaction/utils.ts:89,144`), uses `cacheRetention: "none"` (`compaction.ts:589-593`), and costs one
(or two, for split turns) LLM calls at `min(0.8*reserve, maxTokens)` output. Dropping a page snapshot into the message
stream therefore makes it *transient* — after compaction the model keeps at most a 2000-char echo of it inside a prose
summary. Any page fact that must survive must either be re-derivable through a tool call or be re-asserted cheaply
after a `session_before_compact` event.

**I6 — The cost model must include the harness, not just the page.** `[verified]` + `[external]` Pi's default
compaction reserve is 16,384 tokens; the system prompt carries tools + guidelines + docs pointers; each custom tool's
schema is billed input; and the external browser-agent cost analysis shows snapshot tokens are only one row of the
stack (tool schemas, history, output, retries). NAVI's budget must be stated as a fraction of `model.contextWindow`
with an absolute floor, and validated with Pi's own cache-miss meter (`cache-stats.ts:56-90`) and
`getUsageCostBreakdown`'s `"Tools/summaries"` bucket.

**I7 — A11y tree is the right spine, but it is not a budget solution.** `[external]` Untruncated a11y trees measured at
14.5k–19.4k tokens. The a11y tree solves *semantics* (roles/names/states, actionable targets, resilient to CSS churn);
budgeting is solved by *selection* (viewport/focus/region scoping, interactive-only refs, table collapsing, omitting
text where possible) and by *deferral* (on-demand retrieval). NAVI needs both, in that order.

**I8 — Page understanding belongs in a separate request, not in the main context.** `[verified]` `streamSimple`
(`model-registry.ts:115`) + `usage` accounting + a summarization-style single-shot prompt (the `buildSummarizationContext`
pattern, `compaction.ts:642-653`) reproduce Pi's own "cheap model call over a big blob returns small text" trick. This
is the escape hatch for genuinely large pages (long docs, repo pages, transcripts) without touching the main prompt.

**I9 — Stable refs are the interface between turns.** `[external]` `[unchanged]`-style deltas only work if element
identifiers survive between turns. NAVI must version refs and invalidate them on navigation, and must not let a stale
ref silently resolve to a different element (a silent wrong-target failure is worse than an error).

---

## 3. Recommended Approach

### 3.1 Pipeline (text diagram)

```
 STAGE 0 — RAW BROWSER STATE                       (NAVI host side; never reaches the model)
 ┌──────────────────────────────────────────────────────────────────────────────────────────┐
 │ document (DOM) · computed layout boxes · accessibility tree · focus/selection/caret      │
 │ scroll offsets + viewport rect · active dialog/menu/tooltip · form field values+validity │
 │ tab set + history · MutationObserver event stream · user activity (last input, dwell)     │
 │ canonical URL · MIME/type hints · media elements · network status                        │
 └───────────────────────────────────┬──────────────────────────────────────────────────────┘
                                     │  strip: script/style/noscript/<template>, aria-hidden,
                                     │  display:none, zero-size, off-DOM, tracking iframes,
                                     │  ad/cookie/support widgets
                                     ▼
 STAGE 1 — PAGE UNDERSTANDING (cheap, deterministic; per page load, then per mutation batch)
 ┌──────────────────────────────────────────────────────────────────────────────────────────┐
 │ classify(pageType)  ∈ {docs, article, code-repo, video, app-ui, search, canvas/visual}    │
 │ mainContent = Readability/trafilatura-class extraction (article/docs)                     │
 │ outline     = heading tree with section ids, node counts, byte sizes                      │
 │ interact    = interactive element index (role, accessible name, state, ref)              │
 │ landmarks   = main/nav/header/footer/dialog/form/table regions with extents              │
 │ anchors     = canonical URL · title · docVersion hash · language · media inventory        │
 └───────────────────────────────────┬──────────────────────────────────────────────────────┘
                                     ▼
 STAGE 2 — SEMANTIC REPRESENTATION (SR; the ONLY thing that may be serialized to tokens)
 ┌──────────────────────────────────────────────────────────────────────────────────────────┐
 │ tiered, addressable nodes, stable refs (e.g. p1/s3/e12), page-type-specific shaping:      │
 │   docs    → outline + headings + code blocks (lang, first lines) + "signature" spans      │
 │   article → headings + lead paragraph per section (+ reading order)                       │
 │   repo    → repo identity + README digest + top-level tree + current file ref + PR/issue  │
 │   video   → title/channel/duration + description head + chapter list (transcript deferred)│
 │   app-ui  → landmarks + form fields(labels,values,validation) + table heads + first rows  │
 │             + active dialog/toast/error text + current tab                                │
 │   search  → query + result count + first K results (title/url/truncated snippet) + facets │
 │   canvas  → a11y + text fallback; screenshot ONLY on explicit request                     │
 │ token cost of each node is precomputed with estimateTokens() (chars/4, Pi-comparable)     │
 └───────────────────────────────────┬──────────────────────────────────────────────────────┘
                                     ▼
 STAGE 3 — RELEVANCE SCORING (per question, and continuously for focus signals)
   score(node) = w_q·relevance(node, question)         (lexical + heading/path match first)
               + w_v·viewportOverlap(node)              (boolean-ish, strong)
               + w_f·focusProximity(node)               (focused/selected/last-interacted)
               + w_p·pageTypePrior(node)                (adapter-declared importance)
               + w_r·mutationRecency(node)              (just changed ⇒ likely what user means)
               − hardFilter(boilerplate | nav | footer | cookie | ad | hidden)
   Diversity term: cap nodes per section so one huge section cannot eat the budget.
                                     ▼
 STAGE 4 — CONTEXT ASSEMBLY under a hard budget (see §3.2), deterministic packer:
   T0 identity+outline  ⊕  T1 viewport/focus slice  ⊕  T2 top-k query-relevant sections
   ⊕  an explicit index of what is NOT included ("+N sections, ids s6..s19") — the model can
      ask for them, exactly like claude-rules.ts indexes rule files instead of inlining them.
                                     ▼
 STAGE 5 — DELIVERY INTO PI (single trailing artifact; append-only)
 ┌──────────────────────────────────────────────────────────────────────────────────────────┐
 │ pi.on("context")  →  append ONE trailing message:  <page_context v=3 kind=app-ui …>       │
 │                       … same-page revisit ⇒ DELTA ONLY: changed refs + [unchanged] refs   │
 │ mid-turn page change while agent is running → pi.sendMessage({deliverAs:"steer"})          │
 │ idle wake on "user asked about the screen"   → sendMessage({triggerTurn:true})            │
 │ large/off-screen content                      → NOT injected; exposed via the `page` tool │
 │ overflow                                      → ctx.getContextUsage() + own ledger        │
 │                                                 → ctx.compact() at NAVI's own threshold   │
 │ post-compaction continuity                    → session_before_compact customInstructions │
 └───────────────────────────────────┬──────────────────────────────────────────────────────┘
                                     ▼
                         PI (unchanged): provider serialization + cache breakpoints,
                         tool-call loop, auto-compaction, usage/cost accounting.
```

### 3.2 Budget table

Units: tokens, using Pi's chars/4 heuristic (`compaction.ts:266-306`) so numbers are comparable with
`getContextUsage()`. "Ratio" is of `model.contextWindow` (`ai/src/types.ts:859`). Absolute floors matter because a
1M-token window and a 128k window need different defaults; ceilings protect the small-window case.

| Tier | When it fires | Default budget | Hard cap | Mechanism in Pi | Cache effect |
|---|---|---|---|---|---|
| **T0** page identity + outline | every page settle (new URL or first paint) | 120–250 | 400 | `context` hook append | new tail only; prefix read intact |
| **T1** viewport + focus slice | page settle; focus/selection change; explicit "look at this" | 300–900 | 1,200 | `context` hook append | new tail only |
| **T2** query-relevant sections | each new user question | 800–2,500 | 3,000 | `context` hook append | new tail only |
| **T2Δ** same-page delta | after T0/T1, on mutation batch | 40–300 | 800 | `context` hook append (diff vs. vN) | new tail only |
| **T3** on-demand read | model calls the `page` tool | 1,500/2,500 typical; 8,000 max per call | 3 calls/turn, 12,000 cumulative | custom tool result (`truncateHead` guard) | tool result appended; prefix read intact |
| **T4** page-understanding pass | candidate SR > 2,500, or page type ∈ {docs, code-repo, video}, or 2nd repeat request for the same section | output 300–800 | 1,000 | `streamSimple` with `cacheRetention:"none"` | no main-context cost; attributable via `usage` |
| **T5** screenshot | canvas/map/visual-diff or explicit user request | 800–1,600 (image ≈ `ESTIMATED_IMAGE_CHARS` 4800 chars / 4) | 2 images/turn | custom tool result (image block) | **invalidates message cache** — never on a hot path |
| **NAVI total injected, per turn** | — | ≤ 1,500 (target), ≤ 4,000 (allowed) | 8,000 or 4% of `contextWindow`, whichever smaller | — | one cache write per turn in the normal case |
| **NAVI total, per session, before NAVI-triggered compaction** | — | 40,000 | `contextWindow − reserveTokens(16,384) − 2,048` | NAVI ledger + `ctx.compact()` | — |
| **Static prefix floor** (must be ≥ caching minimum) | — | ≥ 1,024 tokens cached prefix | model minimum: 512 / 1,024 / 2,048 / 4,096 by model `[external]` | Pi's own system+tools | below minimum, caching silently no-ops |
| **Tool schema budget (NAVI's own tools)** | — | ≤ 1,200 for the whole NAVI tool surface | 2,000 | `registerTool` descriptions | tool changes invalidate the entire cache — keep the set stable |

Rationale for the headline numbers: Pi's default `keepRecentTokens` is 20,000 and `reserveTokens` is 16,384
(`compaction.ts:132-136`). If NAVI contributed 4,000 tokens per turn, three pages of browsing would consume 12,000 —
more than half the "recent work" Pi is designed to retain, and enough to force early compaction. A 1,500-token
average keeps NAVI a *minority* consumer of context. `[inferred]` from `[verified]` defaults.

### 3.3 Thresholds and escalation path

1. **T2 candidate > 2,500 tokens** → do not inline. Emit T0 + T1 + section index and a one-line instruction
   (`Use page(action="read_section", id="s4")`). This mirrors `claude-rules.ts:64-85` and README-style "index, then
   load on demand".
2. **Same section requested twice in a turn** → switch to T4: one `streamSimple` pass over the full extracted section
   with a page-type-shaped output contract; cache the result as `page_summary@v` and emit ≤800 tokens instead of a
   third raw read.
3. **`ctx.getContextUsage().percent > 60`** → degrade: T0 only for new pages, T3 results capped at 2,000, T5 disabled.
   Recompute after every assistant message (`turn_end`), as `examples/extensions/trigger-compact.ts:27-41` does.
4. **Projected injection would exceed `contextWindow − reserveTokens − 2,048`** → return a truncated T3 result with the
   Pi-style continuation notice instead of the full content, and (optionally) call `ctx.compact()` **before** the next
   request rather than letting the provider return an overflow error (`types.ts:600 reason:"overflow"`).
5. **Mutation storm** (a live ticker, video progress, ad rotation) → coalesce for 400–800 ms of quiescence; if more than
   N deltas accumulate in one assistant turn, emit a single "page changed: <count> nodes" line and defer detail to the
   next turn.
6. **Navigation** (URL/document change) → new page version; the previous `page_context` is superseded, not edited; refs
   are invalidated; if the tool has a `page` handle to the old document, the tool returns a hard error rather than a
   stale element read.

### 3.4 How deltas avoid breaking prompt caching

- **Append-only.** Each page artifact is a new trailing message. Because the cache prefix hash is cumulative and Pi
  marks only the system block + last user message (`anthropic-messages.ts:1081-1090,1374-1381`), appending keeps the
  prior prefix a read; only the new tail is written. `[external]` + `[verified]`
- **Never touch tools, system prompt, or earlier messages during a page change.** Tool-definition changes invalidate
  the entire cache; system changes invalidate system+messages; image add/remove invalidates messages. `[external]`
- **One cache write per turn.** In the normal case NAVI emits exactly one trailing artifact per LLM call, keeping the
  turn's writes to that artifact plus the assistant/tool blocks.
- **Reuse the 20-block lookback.** If NAVI ever emits several artifacts within a single tool-loop turn, keep them
  contiguous at the tail so the next request's lookback finds the previous write. `[external]`
- **Do not rewrite a page block to update it.** Emit a delta that references prior refs (`[unchanged]` style) and bumps
  the version, so the old block stays byte-identical.
- **Respect the minimum cacheable prefix.** If NAVI's own static prefix (tool schemas + guidance) is small, caching may
  silently no-op for some models; if hit-rate matters, keep NAVI's tool surface large enough to be worth caching, or
  accept uncached tails. `[external]`
- **Instrument with Pi.** `computeCacheWaste`/`detectCacheMiss` (`cache-stats.ts:138,158`) already ignore legitimate
  post-compaction resets and apply a 1,024-token noise floor; NAVI should read those numbers rather than build its own.
- **Screenshots are a cache hazard.** Image presence changes invalidate message caches `[external]`; T5 must be
  explicit-request-only and never interleaved casually with cached prefixes.

### 3.5 Which Pi primitive each NAVI mechanism uses (cooperate, don't duplicate)

| NAVI mechanism | Pi primitive | Repo reference | Why not our own version |
|---|---|---|---|
| T0/T1/T2/delta injection | `pi.on("context")` append trailing message | `types.ts:688-691`, `runner.ts:1034-1064`, `sdk.ts:362-366`, `agent-loop.ts:288-290` | per-call, chainable, error-isolated, already wired to the provider path |
| Mid-turn page change / idle wake | `pi.sendMessage(msg, {deliverAs:"steer"\|"followUp", triggerTurn})` | `types.ts:1365-1368`, `docs/extensions.md:1422-1443` | queueing semantics (steer vs followUp vs nextTurn) are already defined |
| Off-screen retrieval, screenshots | `registerTool` + `promptSnippet` + `promptGuidelines`; results truncated with `truncateHead`/`truncateLine`, `details` for the UI | `types.ts:451-500`, `docs/extensions.md:2176-2209`, `src/index.ts:283-339` | Pi's tool-result contract, truncation utilities and rendering are reusable |
| Static page-type guidance at load time | `before_agent_start` systemPrompt append **or** tool `promptGuidelines` | `types.ts:717-727`, `claude-rules.ts:64-85`, `docs/extensions.md:1379-1381` | stable prefix ⇒ cache-safe |
| Heavy browser-action tools | `pi.setActiveTools()` dynamic loading | `docs/extensions.md:1373-1377,2371-2408` | but note the prefix-invalidation caveat before adopting |
| Overflow handling | Pi auto-compaction (`shouldCompact`) + `ctx.compact()` | `compaction.ts:235-238`, `types.ts:346`, `trigger-compact.ts:27-41` | Pi owns cut points, split turns, summary format, filing of read/modified files |
| Post-compaction page continuity | `session_before_compact` custom instructions / custom compaction result | `types.ts:594-604`, `docs/compaction.md:277-347` | no fork of the summarizer needed |
| Page-understanding model call (T4) | `ctx.modelRegistry.streamSimple()` with `cacheRetention:"none"` + `usage` | `model-registry.ts:115-117`, `compaction.ts:642-653,589-593` | auth resolution + provider neutrality already solved |
| Token accounting / cost reporting | `ctx.getContextUsage()`, `Usage` on our own messages, `getUsageCostBreakdown` | `types.ts:290-296,344`, `ai/types.ts:383-404`, `usage-totals.ts:37-70` | NAVI's spend shows up in Pi's own buckets, including `"Tools/summaries"` |
| Cache-miss telemetry | `computeCacheWaste` / `detectCacheMiss` | `cache-stats.ts:8,11,56-90,138,158` | 5-minute TTL awareness and the 1,024-token noise floor are already encoded |
| Estimated token sizing of SR nodes | `estimateTokens()` (chars/4) | `compaction.ts:266-306`, `src/index.ts:38` | identical ruler to Pi's own numbers |
| Multi-page comparison | subagent-style isolated context (separate process, capped return) | `examples/extensions/subagent/README.md:5-12,116` | 50 KB/task cap and isolated windows already demonstrated |

---

## 4. Alternatives

**A1 — Eager full-page accessibility snapshot per turn (Playwright-MCP behaviour).** `[external]` Measured
14.5k–19.4k tokens for HN/GitHub/Wikipedia with no truncation, and 6k–22k per step in a 7-step dashboard task.
*Kept only as the T3/T4 artefact* (a full snapshot is a legitimate *result* of an explicit tool call), never as the
default per-turn artifact.

**A2 — Screenshot-first perception (computer-use style).** `[external]` Screenshot loops re-observe pixels every step
and are the most expensive rung; they also invalidate message caches. Inverse blind spots from a11y (canvas, layout,
icon-only controls) make them a *complement*, not a replacement. **Fallback only.**

**A3 — Fetch + extract (HTML→markdown, no browser).** `[external]` Search→fetch→extract is the cheapest rung and is
right for public docs, articles, pricing, API references. It cannot see focus/selection, app state, or live mutations,
so NAVI keeps it as the **page-type adapter for docs/article** rather than the universal path.

**A4 — DOM-diff-only protocol (never send a cold snapshot).** Cheapest steady state, but there is no anchor: the first
observation of a page must be a full (budgeted) snapshot, and refs must exist before a diff can reference them.
**EXPERIMENTAL**: run deltas *after* T0/T1, not instead of them.

**A5 — Always run a page-understanding model pass (T4-as-default).** Robust to page size, but adds a second provider
call per page and latency on the critical path, and can lose literal strings (an error message the user is asking
about). **LIKELY for heavy page types only** (docs/repo/video/very long articles), with literal extraction always
retained as a tool path.

**A6 — Embedding-index the page and retrieve chunks.** Better recall on long documents, but needs an embedder, an index
lifecycle, and a reranker; heading-path + lexical matching over a *heading-structured* SR usually suffices because web
pages are already sectioned. **EXPERIMENTAL**, revisit when pages routinely exceed ~10k tokens of extractable text.

**A7 — Sub-agent per page (isolated context) for multi-tab/multi-page work.** `[verified]`
`examples/extensions/subagent/README.md:5-12,116` shows isolated windows, per-agent usage/context reporting, and a
50 KB/capped return. Good fit for "compare these three tabs", poor fit for the single-page ambient case (process cost,
latency, and the parent still needs a compact artifact). **LIKELY for explicit comparison workflows.**

**A8 — Put page context in the system prompt via `before_agent_start`.** Rebuilds the static prefix each turn and
invalidates the tools+system cache level; also invisible to `getContextUsage()`. **REJECT.**

**A9 — Rewrite the serialized payload in `before_provider_request`.** Powerful (can even edit provider-level system
instructions) but bypasses Pi's accounting and is documented as a debugging hook (`docs/extensions.md:709,720`).
**DEFER** to cache diagnostics/inspection.

**A10 — Custom compaction implementation (replacing Pi's summary call).** Pi's summarizer already handles cut points,
split turns, previous-summary chaining, file-operation fingerprinting, and `cacheRetention:"none"`. Replacing it buys
nothing and risks quality regressions. **REJECT**; use `session_before_compact` custom instructions instead.

**A11 — Persist page content into a NAVI-side knowledge store for cross-session memory.** Attractive long-term, but
it is a privacy surface (user's authenticated pages), a storage/gc problem, and an unbounded cost path. **DEFER.**

---

## 5. Risks

| # | Risk | Evidence | Mitigation |
|---|---|---|---|
| R1 | Page context injected via the `context` hook is **invisible** to Pi's compaction threshold and `getContextUsage()`, so overflow arrives as a provider error rather than a graceful compaction | `[verified]` `agent-session.ts:538-555` (uses `context.messages` / `agent.state.messages`), `:3411-3455` (usage from session entries) | NAVI keeps its own ledger; enforce ceilings pre-injection; call `ctx.compact()` at NAVI's own threshold; prefer materializing large reads as real tool results |
| R2 | Cache invalidation from rewriting earlier page blocks or per-page system/tool changes → 10–30× input cost on the affected prefix | `[external]` Anthropic invalidation table; `[verified]` `anthropic-messages.ts:1081-1090,1374-1381` | append-only; version bump instead of edit; freeze tool set and system prompt per session |
| R3 | Silent no-op caching when the static prefix is below the model's minimum cacheable length | `[external]` 512–4,096 tokens depending on model | measure `cacheRead`/`cacheWrite` on real traffic; do not assume caching |
| R4 | Prompt injection from untrusted page text into a tool-enabled context | `[inferred]` there is no sanitizer on the extension `context` path (`runner.ts:1036` only clones); Pi's trust model is about repos, not page content (`resource-loader.ts` `resolveProjectTrust`) | delimiter-tag all page text (`<page_context>`…), never let page text select tools/commands, treat page text as data in the system prompt, and require confirmation for side-effecting tools |
| R5 | Stale element refs after DOM churn resolve to the wrong element | `[inferred]` from refs being NAVI-side identifiers | version refs per document version; invalidate on navigation/mutation batch; tool errors on unknown refs |
| R6 | Mutation storms (tickers, ads, progress bars) produce a delta flood | `[inferred]` | debounce 400–800 ms, coalesce per turn, hard cap deltas per turn |
| R7 | Screenshots invalidate message caches and cost ≈1,200 tokens each | `[external]` images invalidate message caches; `[verified]` `compaction.ts:244` counts an image as 4,800 chars ≈ 1,200 tokens | explicit-request-only; never interleave with cache-sensitive prefixes |
| R8 | Compaction summarizes the page referent away ("what about that button?") | `[verified]` `compaction/utils.ts:89,144` (2,000-char tool results inside a prose summary), `compaction.ts:589-593` (no cache retention, fresh session id) | `session_before_compact` custom instructions to preserve URL/type/current refs; keep facts re-derivable via the `page` tool; attach structured `details` |
| R9 | Privacy: page content is user data (authenticated dashboards, personal docs) | `[inferred]` | keep page text session-scoped; do not write it to `.pi/` config files or share it across sessions; respect the same isolation Pi applies to session files |
| R10 | Budgets hard-coded for a 200k window misbehave on 128k or 1M windows | `[verified]` per-model `contextWindow`/`maxTokens` (`ai/types.ts:859-860`), rendered per model in `cli/list-models.ts:68` | express budgets as ratios of `model.contextWindow` with absolute floors/ceilings; per-model overrides like `compaction.modelOverrides` (`docs/compaction.md:420-442`) |
| R11 | T4 summarization loses literal strings the user asks about (error codes, IDs) | `[inferred]` | keep a deterministic literal extractor reachable by tool (regex/aria-name/table cell), never only the summary |
| R12 | Over-instrumenting NAVI's own context leads to a second, competing meter that disagrees with Pi | `[verified]` `getContextUsage()` semantics (null after compaction) | treat Pi's numbers as canonical for display; NAVI's ledger is only a pre-injection gate |

---

## 6. Feature Assessment

**CORE**

- Accessibility-tree–based semantic serialization (roles, accessible names, states) as the primary representation — `[external]` standard perception layer, `[external]` format choices worth 51–79% of tokens.
- Interactive-only refs, omitted non-actionable attributes, collapsed one-line table rows — `[external]` the measured big levers.
- Tiered ladder T0/T1/T2 with a hard per-turn budget (target ≤1.5k, cap 4k) — the only defence that scales to 19k-token pages.
- Off-screen retrieval as an explicit tool with `offset`/`limit` continuation and truncation `details` — `[verified]` Pi's own `read.ts:150-172` contract.
- Append-only injection through `pi.on("context")` — `[verified]` `runner.ts:1034-1064`; `[external]` prefix-cache semantics.
- Same-page deltas that reference prior refs instead of re-serializing — `[external]` ~94% data reduction claim; `[inferred]` necessary for cost control.
- Stable element refs + a dedup cache keyed by (canonical URL, doc version, viewport bucket, focus target) — `[inferred]` prerequisite for deltas.
- Page-type classification with at least four adapters (docs, article, app-UI, search) — `[external]` WCXB's 7 page types show one extractor does not fit all.
- Readability/trafilatura-class main-content extraction for article/docs — `[external]` mature baselines with published benchmarks.
- Stripping script/style/aria-hidden/hidden/zero-size/off-DOM before anything is serialized — `[external]` these are the verbose, valueless nodes.
- NAVI-side token ledger + `ctx.getContextUsage()` cross-check + NAVI-threshold `ctx.compact()` — `[verified]` I2 accounting gap.
- All NAVI static guidance shipped at load time (tool `promptSnippet`/`promptGuidelines`, `before_agent_start`) so the cached prefix never churns — `[verified]` + `[external]`.

**LIKELY**

- Focus/selection-weighted scoring (focused element + its form context outranks viewport peers, decaying over seconds) — highest-signal cheap heuristic for "what is the user asking about".
- Viewport-first extraction as the T1 default (viewport ∩ a11y, with immediate landmark ancestors) — matches the "only the visible thing is the question" premise of NAVI.
- MutationObserver-driven delta batches with debounce — `[external]` deltas are the steady-state economy.
- Separate cheap-model page-understanding pass (T4) for docs/repo/video/very long articles — `[verified]` `streamSimple` + Pi's summarization pattern make it cheap to build.
- `session_before_compact` hook to preserve page anchor (URL/type/current refs) and attach custom `details` — `[verified]` `types.ts:594-604`, `docs/compaction.md:281-311`.
- Query-relevant top-k section selection within the T2 budget — needed as soon as an SR exceeds one screen of tokens.
- Sub-agent isolation for explicit multi-page/multi-tab comparisons — `[verified]` subagent README.
- Per-page-type budget splits (e.g. app-UI allows more interactive nodes, article allows more prose) — `[inferred]` from the differing failure modes.

**EXPERIMENTAL**

- Embedding/reranker-based in-page chunk retrieval — only pays off past ~10k tokens of extractable text; heading paths cover most pages first.
- Screenshot fallback for canvas/map/visual-diff pages — right answer for the blind spot, but a cache and cost hazard.
- DOM-diff-only steady state with no periodic full-anchor refresh — cheap but drifts; needs a staleness re-anchor policy.
- Video transcript/chapter retrieval — high value for "what did they say about X", but transcripts are the single largest blob on the web.
- Vision-based page-type detection / visual-difference salience — useful when semantics are absent; unproven for ambient use.
- Tuning cache-breakpoint strategy from `computeCacheWaste` deltas — Pi gives the meter, but the optimum is empirical per model.

**DEFER**

- Cross-session page memory / personal browsing knowledge base — privacy + storage + unbounded cost; revisit after per-session budgets are proven.
- `before_provider_request` payload rewriting — documented as a debugging hook (`docs/extensions.md:720`) and invisible to accounting.
- Dynamic tool activation for browser-action tools (`setActiveTools`) — `docs/extensions.md:2404-2408` warns the prompt-prefix change can invalidate the cache; wait until page context works.
- A user-visible page-context inspector (what NAVI sent, and why) — high trust value, not on the correctness path.
- NAVI-specific compaction fork — rejected as a *feature*, so deferred permanently; use the hook.

**REJECT**

- Eager full-page a11y snapshot every turn — measured 14.5k–19.4k tokens/snapshot on ordinary public pages.
- Injecting page context into the system prompt per page/per turn — invalidates the cached prefix, invisible to accounting.
- Re-implementing truncation, token estimation, usage totals, cache-miss detection, or the compaction pipeline — all already exported (`src/index.ts:28-50,283-339`).
- Screenshot-first perception as the default path — most expensive rung, cache-hostile, and blind in the *other* direction.
- Editing previously emitted page blocks to "update" them — cumulative prefix hashing makes this the most expensive possible move.
- Using compaction as a page-content cache — summaries keep ≤2,000 chars per tool result and do not write prompt cache.

---

## 7. Open Questions

1. **Where does page understanding run?** In the browser extension/host (best latency, no permission friction, direct
   DOM/a11y access) or in a NAVI companion process with a debug-protocol connection? The pipeline is identical, but the
   representation API and the trust boundary differ.
2. **What is the real distribution of page sizes NAVI must survive?** We have three external data points
   (14.5k–19.4k tokens for HN/GitHub/Wikipedia). We need our own measurement across the page types NAVI will actually
   see before freezing T0/T1/T2 defaults.
3. **Do users ask about what is visible, or about the page?** The whole viewport-first design assumes the former. If
   ambient questions are frequently page-level ("summarize this"), T4 becomes the primary path rather than an escalation.
4. **Is the a11y tree sufficient for the app-UI pages that matter most?** Grids, virtualized lists and canvas widgets
   degrade a11y badly; we do not yet know NAVI's page mix.
5. **How should T3 refs be namespaced so they cannot collide across turns** while remaining short enough to be cheap
   (`page` / `section` / `element`), and what is the cost of the versioning metadata versus its safety benefit?
6. **Should large tool results be materialized as real messages or kept ephemeral?** Materializing makes them visible to
   Pi's compaction/accounting (fixing R1) but makes them permanent context liabilities. Which wins?
7. **What is the right NAVI-owned compaction trigger?** A fixed percentage (60%?), a share of `reserveTokens`, or an
   absolute NAVI-injected-token count? Pi uses `contextWindow − reserveTokens`; NAVI's ledger needs a rule that composes
   with it rather than fighting it.
8. **How much cache does NAVI actually save or lose?** Answerable with Pi's `cache-stats.ts` semantics on a real
   transcript set: how often does a delta-every-turn pattern keep the prefix hot versus emitting one artifact per user
   question?
9. **What is the minimum useful static prefix for caching** given a small NAVI tool surface, and is it worth padding
   (consistently) to reach a model's cache minimum?
10. **Which page-type adapters are worth building first**, and can we validate adapter quality with a small labelled set
    (the WCXB methodology suggests 7 structurally distinct types, we probably start with 4)?
11. **How do we prevent page text from steering behaviour** (R4) without a sanitizer Pi does not provide — delimiter
    discipline, tool-side argument validation, and a "page text is data" system-prompt line, or something stronger?
12. **How does "compare these tabs" compose with the single-page pipeline** — one T4 pass per page plus a merge step, or
    a subagent per page with capped returns?

---

## Appendix — External sources

- Playwright documentation, *Snapshot testing / aria snapshots* (YAML a11y snapshot format: `- role "name" [attr=value]`,
  `/url` properties, partial matching) — https://playwright.dev/docs/aria-snapshots
- kuroko1t, *How Accessibility Tree Formatting Affects Token Cost in Browser MCPs* (dev.to; measured 2026-02-26:
  Playwright MCP 14,547–19,409 tokens per full tree; 51–79% reduction from format choices) —
  https://dev.to/kuroko1t/how-accessibility-tree-formatting-affects-token-cost-in-browser-mcps-n2a
- Sudonull editorial, *ARIA snapshots Playwright for AI agents* (YAML structure, ref-based actions, `[unchanged]` deltas
  reported at ~94% reduction, ARIA blind spots) — https://sudonull.com/aria-snapshots-playwright-for-ai-agents
- baeseokjae, *Browser MCP Snapshot Token Cost 2026* (per-step snapshot token table ≈88k input tokens over 7 steps,
  search→fetch→snapshot→screenshot ladder, budget guardrails, cost-per-completed-task) —
  https://baeseokjae.github.io/posts/browser-mcp-snapshot-token-cost-2026/
- Web Content Extraction Benchmark (WCXB): 2,008 annotated pages across 7 structurally distinct page types —
  https://webcontentextraction.org/
- Readability vs trafilatura vs jsdom, 2026 benchmark (50 real pages, extraction fidelity vs runtime) —
  https://bulkmd.app/blog/readability-vs-trafilatura-extractors
- Anthropic, *Prompt caching* (prefix order tools→system→messages, ≤4 breakpoints, 20-block lookback, minimum cacheable
  prefix by model, 5-minute TTL refresh, image invalidation, usage fields) —
  https://platform.claude.com/docs/en/build-with-claude/prompt-caching

*Report scope: findings, architecture, and budgets only. No files inside `vendor/pi` were modified; this document is the
only file created.*
