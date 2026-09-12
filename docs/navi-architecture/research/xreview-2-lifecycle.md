# xreview-2 — NAVI activation & lifecycle

Adjudicating (1) Agent 8 — always-present daemon, one in-process `AgentSession`, fanned to all surfaces; (2) Agent 7 — no daemon, the browser native-messaging host is the bridge, unref'd idle-exit; (3) Agent 1 — no scheduler, eager startup.

## Verdict

**Hybrid: on-demand connect-or-spawn session supervisor + browser-managed activation for the browser hop + a Pi child spawned per turn, with an opt-in "warm" mode that is position (1) behind a user switch.** No always-on daemon by default.

Rule: *the first component needing a session spawns-or-connects a supervisor; it exits when the last attachment detaches plus a grace; the Pi child exits sooner; warm mode suspends both rules under consent.*

```
terminal `navi` ──(is the supervisor)──┤
browser ext ─stdio via native host─────┤ browser-spawned, killed with its port
   no supervisor? spawn detached +     │
   unref'd (10 ms poll / 10 s)─────────┤
tray / voice ─ UDS | named pipe ───────┘
   SUPERVISOR (sole writer) → pi child (`--mode rpc --session <id>`), per turn
```

Topology A: the terminal is the supervisor. B: the native host spawns a detached, unref'd one.

**Budgets** (targets, unmeasured; idle with no browser/terminal = 0 processes):

| Component | Summon latency | Idle RAM |
|---|---|---|
| Surface paint | ≤100 ms p50 | ~0 |
| Supervisor spawn | ≤400 p50 / 700 ms p95 | <40 MB |
| Pi child cold → `get_state` | ≤1.5 / 2.5 s p50-p95 | 40–80 MB, **not resident** |
| Warm mode | ≤400 ms p50 to first token | +60–100 MB, consented |

**Sleep/resume, browser restart.** Suspend is exit: unref'd timers never hold a process and fire on wake, so the decision is re-evaluated correctly. Windows has no SIGSTOP/SIGCONT (`interactive-mode.ts:4089-4092`). Browser restart kills the host; a surviving supervisor is orphaned and idle-exits before `connectNative` revives it.

**Continuity.** The supervisor is the sole writer: pi's JSONL has no locking, so a second writer corrupts (01-core §4). Others resync as viewers via `get_entries({since})`; one holds prompt authority, reset to unclaimed after a restart.

**Crash recovery.** Pi writes the session file only from the first assistant message, so a crashed child loses at most the in-flight turn; respawn with `pi --session <id|path>`. Supervisor crash → attachments re-enter connect-or-spawn. Host crash → the browser restarts it; nothing is lost.

**Autostart.** Linux: none; opt-in `systemd --user` socket activation. Windows: none — the native-messaging `allowed_origins` manifest *is* the activation vehicle; named pipes plus a lockfile/PID check replace UDS stale-socket probing (pi's UDS is POSIX-only); teardown is `taskkill /F /T`.

## Evidence (re-verified)

- `rpc-mode.ts` ends `// Keep process alive forever` → `return new Promise(() => {})`; exits only on stdin `end` or SIGTERM (no SIGHUP on Windows).
- Eager: `cli.ts` is a 6-line static shim; ~60 static imports in `main.ts:8-71`, no `import()`; `main.ts:930-980` only picks which `run*` runs.
- Pi retires rather than resides: connect-or-spawn (`coordinator.ts:187-199`); empty shutdown 250 ms, 30 s startup grace, unref'd (`483-506`); `ServerLifetime` retires after 1 s (`server.ts:244-320`); demand-only workers, detached unref'd children (`process.ts:41-70`).
- `CACHE_TTL_MS = 5 min` (`cache-stats.ts:8`); `npm run profile:rpc` emits `METRIC startup_time_ms`.

## Why the losing option remains attractive

**(1) always-on** is not merely faster: one process owning one session makes fan-out trivially correct and removes the connect-or-spawn race. Its economic claim is weak, though — *process residency ≠ cache residency*: a respawned child re-sends a byte-identical prefix and still hits the prompt cache inside the 5-minute TTL. It survives as warm mode because a 2 s wake-up loses to a chat window. **(2) browser-host-only** is the minimum-process design; the browser already supplies activation, teardown and perception, so it is the default whenever a browser is present — demoted only because it cannot serve terminal/tray/voice or shared state.

## Conditions/triggers to switch

- → always-on: RPC cold-start p95 >1.2 s **and** median inter-turn gap <5 min **and** resident supervisor+child <100 MB **and** user opt-in.
- → pure browser host: no non-browser surface ever ships.
- Revert warm mode: idle energy over budget, resident RSS >80 MB, Windows pipe contention.

## Must be measured first

1. RPC cold start p50/p95 + phase breakdown (`npm run profile:rpc -- --runs 10 --warmup 2`; `PI_TIMING=1`) — the latency budget rests on it.
2. Whether a respawned child re-sends a byte-identical prefix and still hits the cache — decides warm-mode economics.
3. Node spawn, connect-or-spawn and child idle RSS, Windows vs Linux.
4. Native-host spawn cost and MV3 worker-eviction (external).

## Confidence

**High** on "no always-on daemon by default" (eager ~60-module graph, no idle GC, the 250 ms–5 s retirement precedent, the stated idle requirement). **Medium** on the hybrid's shape — it hinges on unmeasured spawn/RPC latency and how many surfaces ship. **Low** on idle constants: the supervisor's 120 s fits, but the Pi child should exit sooner (~30–120 s), since respawn inside the cache TTL is cheap.
