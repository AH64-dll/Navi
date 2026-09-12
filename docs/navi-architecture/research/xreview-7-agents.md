# X-Review 7 — Agent topology and autonomy controls

**Adjudicator:** independent verification pass over `09-capabilities.md` (Agent 9) and `10-red-team.md` (Agent 10).
**Repo:** `vendor/pi` @ `71dca871`; claims re-read in source, line numbers re-checked.

## Verdict

**Ship (a): one agent, tools + skills.** Not (c), not (d), not (b) yet.

- **Agent 9 sustained:** the governor is mandatory and buildable; the subagent example proves spawning works; Pi has no cost/step/time cap.
- **Agent 9 overturned:** P7's middle term. "Subagents" and "budget governor" are not one primitive — a governor in the parent cannot govern a child, and multi-source research needs no second agent for concurrency.
- **Agent 10 sustained:** N `--mode json` children is a resource trap; auto-compaction is an unbudgeted LLM call. **Narrowed:** compaction is *switchable* — `shouldCompact` returns false when `settings.enabled` is false (`compaction/compaction.ts:235-238`), reachable via RPC `set_auto_compaction` (`rpc-mode.ts:540-543`). A budget line item, not a fixed tax.
- **Both understated:** in-context tool parallelism already exists, and roles are modes, not agents.

## Evidence

1. **One agent already fans out.** `agent-loop.ts:417-423` → `executeToolCallsParallel` unless `config.toolExecution === "sequential"` or a tool sets `executionMode: "sequential"` (`extensions/types.ts:472-479`). N fetches in one turn need no new machinery; process isolation buys **context isolation only**, and `--no-session` makes the child's context unreadable anyway.
2. **Roles are modes.** `examples/extensions/plan-mode/index.ts:22-23,120-141,170-183` is a role swap: `setActiveTools` allowlist, injected persona, `appendEntry` state, `{ block: true }` on disallowed calls. A "skeptic" is this with a different allowlist.
3. **The kill switch is real; `terminate` is not.** `ctx.abort()` (`extensions/types.ts:338`) is the hard stop. Block-with-`terminate` ends the run only when **every** call in the batch is blocked-and-terminating (`agent-loop.ts:589-591, 643-652`).
4. **Governor failure is fail-closed.** A throwing `tool_call` handler propagates out of `runner.ts:982-1003` (no try/catch, unlike `emitUserBash`) and is rethrown as `"Extension failed, blocking execution"` (`agent-session.ts:496-501`), so the tool is **never executed** (`agent-loop.ts:668-674`).
5. **`getContextUsage()` has a blind spot.** `agent-session.ts:3442-3444` returns `{ tokens: null, percent: null }` with no post-compaction assistant; `trigger-compact.ts` treats null as "skip". A governor copying that shape stops enforcing after every compaction.
6. **Cost is computable and pre-categorized.** `core/usage-totals.ts` groups assistant usage by model and `branch_summary`/`compaction` usage into a `"Tools/summaries"` bucket; tool results carry nested `Usage` (`extensions/types.ts:963, 1148`).
7. **Agent 10's hot-path finding verified.** `sdk.ts:362-366` guards `transformContext` on `if (!runner)` only; `runner.ts:1034-1036` deep-clones the whole message array **before** the handler check. Any installed extension pays an O(conversation) clone per LLM call even with zero `context` handlers, so P2 (bubble + prune) is not free; cheap v1 passes page state as a payload at the `prompt()` call site.
8. **Cross-process governance is impossible from the parent.** `subagent/index.ts:296-302` passes only `--mode json -p --no-session` (+`--model`, `--thinking`, `--tools`, `--append-system-prompt`). No cap crosses the boundary; the only lever is the abort signal (SIGTERM, SIGKILL at 5s), and its tally sums only assistant `message_end`, so a child's compaction cost is uncounted.

**Governor contract before any autonomous multi-step run:** cost cap from entry-level `Usage` at `turn_end`; token cap from `getContextUsage()` with an explicit null branch; step cap = `turn_end` counter; wall-clock = NAVI's own timer (nothing in Pi); kill switch = `ctx.abort()`; confirmations = `ctx.ui.confirm` (`rpc-mode.ts:142-145` fails **closed**; `select` does not); progress = `setStatus`/`setWidget` (string[] only over RPC, `rpc-mode.ts:195-208`), `appendEntry` + renderer, `agent_settled`; auto-compaction budgeted or disabled; no subprocess child while no cap crosses the boundary. All CORE, none new Pi features.

**Cheapest evidence that multi-source research needs >1 agent:** one session, one agent, ≥10 real sources, ledger in tool `details`. Measure whether auto-compaction fires, context tokens at source N, and answer quality vs. isolated-context runs. If compaction never fires and quality ties, isolation buys nothing. One `evalHarnessTable` cell at `repetitions: 5` (`packages/evals/README.md:152-184`) — a day's work, cheaper than the governor.

## Why the losing option remains attractive

The subagent example is genuinely good: `MAX_CONCURRENCY = 4`, `PER_TASK_OUTPUT_CAP = 50KB`, streaming `onUpdate`, project-agent trust confirmation, markdown-defined roles. It looks finished and costs the parent **no context** for a large source. But its costs are invisible by design (`--no-session`), its cap is unknowable from inside, and a page that fits a child could equally be a tool returning an excerpt. (b) stays attractive because `AgentLanes` is the designed answer (`packages/agent/docs/plugins.md:108`) — just unpublished and mid-rewrite.

## Conditions/triggers to switch

- **→ (c), one gated context-isolation tool:** a single source exceeds ~15% of the context window, its answer fits in <2 KB, **and** the eval shows a quality lift. Cap concurrency at 2, restrict `--tools`, treat the child as untrusted.
- **→ (b):** in-process lanes ship non-experimental, or the governor must stop work *mid-fetch* rather than at a turn boundary.
- **→ (d):** never in v1. Revisit only if two roles need **different models** concurrently.

## Confidence

**High** on (a) as v1 and on the governor contract (each item read in source). **High** that `--no-session` subagents cannot be governed from the parent. **Medium-high** that one agent suffices — an empirical claim the eval settles; parallel tool calls plus a `details`-based ledger remove the concurrency motive, leaving only context isolation, a threshold question rather than a topology question. **Low** on roadmap-contingent claims (lanes, protocol stability) — `packages/agent/docs/mobile-handoff/README.md:22-23` says specs are not code.
