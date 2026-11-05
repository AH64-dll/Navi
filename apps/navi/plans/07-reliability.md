# Section 7 — Reliability, performance, and engineering hygiene

Scope: `src/main.rs` (poll/status/events/workers), `ui/station.js` (poll/render/errors), `window.py` (RPC, close, devtools), `workers/ipc.py`, `ui/scene.js` (failure paths).

**Verified constraints.** All UI verification runs in `scripts/isolated.py` (`NAVI_ISOLATED=1`, private data/runtime dirs, `LIBGL_ALWAYS_SOFTWARE=1`, env scrubbed of `DISPLAY`/`PULSE_SERVER`/`TMUX`) under `xvfb-run`, as `scripts/test_gui_isolated.py` already does. Any new test hook must be namespaced and rejected outside isolated mode. The UI keeps `connect-src 'none'`; every new channel reuses `window.webkit.messageHandlers.navi`. Workers stay stdlib-only.

---

## BUG-093 · A lost RPC reply hangs its promise forever and freezes the status feed
- **Severity** High · **Type** reliability (hang)
- **Evidence** `ui/station.js:10-13` parks promises with no timeout; `:383` is the only resolver; `window.py:92-98` catches only `(OSError, RuntimeError, ValueError)`, so a malformed **request** raising `KeyError` escapes a discarded `pool.submit` future; `:117-118` reports via `navi.notice` without the ticket; `ui/station.js:144-147` re-arms `poll` only after `await`, so one lost reply stops all updates.
- **Root cause** The bridge has no delivery contract: no client expiry, no unconditional reply, no observable evaluate failure.
- **Fix** (1) `rpc(op,params,timeout)` with per-op budgets (`status` 15 s, settings/create 20 s, command 200 s). (2) One 5 s sweeper rejecting expired entries; cap `pending` at 64. (3) In `window.py`, wrap the worker body in `try/except BaseException`, serialise inside its own try, always call `navi.reply(ticket,…)`; the outer handler replies with the ticket when present. (4) Give `evaluate()` a `evaluate_javascript_finish` callback that logs failures. (5) Harden `navi.reply` against unknown/duplicate tickets. (6) Make `poll` immortal: `try/finally` schedule, single in-flight guard, exponential backoff (0.7→15 s), `offline` phase after 3 failures, one toast on transition.
- **End state** A lost or malformed reply surfaces as a timed-out request; the app keeps polling and recovers.
- **Why** One lost message currently kills all live updates silently.
- **Depends** none (land first) · **Verify** Isolated test hook `NAVI_TEST_DROP_REPLY=1` (isolated-only) drops one reply → the promise rejects with `timeout`, polling continues, `inspect().pending === 0`; malformed request → one error reply, no hang.
- **Effort** M · **Risk** low.

## BUG-094 · Polling re-sends the world, forks tmux, and rebuilds the session strip
- **Severity** Medium · **Type** performance/UI correctness
- **Evidence** `ui/station.js:144-147` polls every 700 ms; `src/main.rs:461-466` returns the entire settings blob, sessions, events, browser, desktop, and voice; `src/sessions.rs:27-46` forks `tmux` per call; the voice worker adds ~2.5 status calls/s, each also forking; `ui/station.js:233` (`bar.innerHTML=''`) rebuilds every chip on every update, and again per `session` event.
- **Root cause** A full-state poll is used as the event transport, and rendering is a full rebuild with no reconciliation.
- **Fix** (1) Split status into `status` (small: phase, counters, cursors) and `state` (sections requested explicitly: `?sections=settings,sessions`), with a `version` per section so unchanged sections are omitted. (2) Cache `tmux list-sessions` for 1 s and share it between pollers. (3) Replace poll with the host-driven push from BUG-095 (40 ms while a turn is open, 700 ms idle). (4) Replace `renderSessions` with `syncSessions` keyed by id over a `Map`, creating the "+ session" button once and updating nodes in place; skip when a signature is unchanged; route the `session` event through the same reconciler. (5) Add a `MutationObserver` assertion to the GUI workflow.
- **End state** Idle polls cost almost nothing; chip identity, focus, and hover survive; clicks stop flaking.
- **Why** Constant churn wastes CPU, forks processes, and makes the UI feel unstable.
- **Depends** BUG-093, BUG-095 · **Verify** Five idle polls → zero `#sessions` mutations and `isSameNode` holds; `ps` shows no tmux fork on a cached poll; a 60 s capture shows CPU below a fixed budget.
- **Effort** M · **Risk** medium (payload contract changes with the client).

## BUG-095 · No developer surfaces: no devtools, console capture, or diagnostics
- **Severity** Medium · **Type** engineering hygiene
- **Evidence** `window.py:64-67` enables neither developer extras nor console forwarding; `:74-77` discards `evaluate_javascript` errors and no-ops after close; `:187-196` is the only diagnostics path (`NAVI_QA_OUTPUT`); grep finds no `enable_developer_extras`, inspector, or console wiring.
- **Root cause** The host was built for headless evidence capture, not interactive debugging.
- **Fix** `NAVI_DEVTOOLS=1` → `set_enable_developer_extras(True)` plus F12/`Ctrl+Shift+I`; `set_enable_write_console_messages_to_stdout(True)`; forward `window.onerror`, `unhandledrejection`, and rejected replies as `{type:'log',level,message,stack}` into a bounded deque (200) exposed in QA JSON; one stderr line per RPC under `NAVI_VERBOSE=1` (op, ms, ok, bytes — never params); a Diagnostics block in Settings (renderer, DPR, viewport, daemon pid/version, pending count, event cursor + boot_id, worker status) with Reload UI and Copy diagnostics; handle `load-failed` with a visible error pane; document the flags.
- **End state** A developer can inspect a live window and read why it failed without reading source.
- **Why** Every bug in this plan set was found by reading code because the app reports nothing.
- **Depends** BUG-093 (error contract) · **Verify** Run the isolated GUI with and without `NAVI_DEVTOOLS`; assert the setting; call `window.__missingFn()` and assert stderr and `inspect().errors.length === 1`; the diagnostics block renders in the QA snapshot with no network.
- **Effort** M · **Risk** low (env-gated).

## BUG-096 · A post-startup stage failure blanks the app or destroys the window
- **Severity** High · **Type** robustness
- **Evidence** `ui/scene.js:15-17` guards only the renderer constructor; `:221` `composer.render()` is unguarded; `:226-241` re-arms RAF before `draw()`, so a throw repeats 30×/s; `ui/station.js:390` is the only code that clears `booting`, behind the scene's optional callback; `ui/surface.css:24` keeps the chat at `opacity:0`; `window.py:67` destroys the window on `web-process-terminated`.
- **Root cause** Degradation is designed for one failure (no WebGL at construction) and no other.
- **Fix** (1) `station.js` owns boot: arm a 1200 ms `forceBooted()` plus `window.onerror`/`unhandledrejection`; idempotent with the scene callback. (2) Wrap `draw()` in try/catch with a failure counter; ≥3 → `degrade()`: cancel RAF, set `renderer='fallback'`, apply the gradient, swap in the stub API, dispatch `navi:degraded`. (3) On `webglcontextrestored`, re-run `resize()`; if context is still lost after 5 s, degrade. (4) `window.py`: first WebProcess termination within 60 s → reload; second → reload URI; third → destroy with a logged reason. (5) Mark a failed bundle load with `onerror`. (6) Skip glitch/draw work when degraded.
- **End state** Every stage failure leaves a usable chat and shell, with `renderer` reported honestly; a GPU crash reloads the UI instead of losing the window.
- **Why** A GPU hiccup currently reads as "Navi crashed and lost my work".
- **Depends** BUG-095 (diagnosis) for full value; the failsafe is independent · **Verify** `NAVI_FORCE_WEBGL_FAIL=1` (isolated) → `renderer='fallback'`, chat opacity 1, `booted` set; rename the bundle in a temp app copy → same; dispatch `webglcontextlost` → no unhandled exception; kill WebKitWebProcess → one reload with sessions intact.
- **Effort** M · **Risk** medium (do not touch a disposed renderer).

## BUG-097 · Window sizing ignores DPI and non-primary monitors, and remembers nothing
- **Severity** Medium · **Type** correctness/UX
- **Evidence** `window.py:35` unconditional 1440×920; `:38-47` primary monitor only, hardcoded 48 px gutter, floors `max(960,…)`/`max(640,…)` that can exceed the work area; `ui/scene.js:25` pixel ratio read once; `:176-186` never revisits it; settings have no geometry key.
- **Root cause** Sizing is computed once, from one monitor, in logical pixels, with floors that defeat the clamp.
- **Fix** Choose the monitor at the pointer (then refine after `show_all()`); clamp with `min(area.width, max(min(area.width,960), min(1440, area.width-48)))` (same for height); read `get_scale_factor()` and default to the full work area at scale ≥2; persist `{width,height,maximized,monitor}` via a dedicated `window_state` op (never `save_settings`); save debounced on `configure-event`; react to monitor add/remove; in the scene, re-evaluate `devicePixelRatio` inside `resize()` with a configurable cap (default 2) and update the composer.
- **End state** The window fits the monitor it opens on at any scale; the stage is sharp; geometry survives restarts.
- **Why** HiDPI laptops and multi-monitor setups are the common case.
- **Depends** BUG-096 · **Verify** `xvfb-run -screen 0 1024x600x24` and `2560x1440x24` with `GDK_SCALE=2`: size ≤ work area and ≥ minimums; `inspect()` reports DPR and `renderer.getPixelRatio() === min(dpr, cap)`; restart restores geometry.
- **Effort** M · **Risk** low-medium (keep persistence out of `save_settings`).

## BUG-098 · The live stream is lossy: a full turn overflows the 100-event ring
- **Severity** High · **Type** data loss + stuck state
- **Evidence** `src/chief.rs:617-625` emits one event per SSE chunk; `src/main.rs:211-221` caps at 100 and drops the oldest; `:460-462` returns only `seq > since`; `ui/station.js:40-46` appends deltas, so gaps concatenate silently; `:126-136` clears busy only on the terminal `reply`.
- **Root cause** Unbounded producer rate, a 100-entry sampled transport, and no coalescing, acknowledgement, or backpressure.
- **Fix** (1) Coalesce at the producer: accumulate per turn and flush every ~120 ms (or 240 chars) with `{turn, text:<cumulative>, done:false}`. (2) Keep authoritative `turn_state {turn,text,done}` in `State`, always included in `status`. (3) Two-tier retention: the diagnostic window plus a `terminal_events` deque (cap 16) that is never dropped. (4) Client replaces rather than appends; a shorter text for the same turn triggers a resync; busy clears from `turn_state.done`. (5) Drain immediately when `has_more`.
- **End state** Rendered text is identical whether the client samples every 100 ms or every 5 s; busy can never stick.
- **Why** Streaming correctness is the headline feature; this is the only path that leaves the UI showing an unstoppable live turn.
- **Depends** BUG-094, BUG-099 · **Verify** Fixture emitting 500 deltas in 300 ms: 700 ms polling yields exact concatenated text and a terminal reply; 5 s polling yields the same final row, not `.streaming`, with `busy === false`.
- **Effort** 1.5 d · **Risk** medium (payload and client must land together).

## BUG-099 · Event sequence numbers reset with the daemon, silently freezing the feed
- **Severity** High · **Type** data loss/silent failure
- **Evidence** `src/main.rs:169` (`next_event: 0` per process); `:211-217` assigns from RAM; `:460-462` filters `seq > since`; `ui/station.js:126` keeps `seq` monotonic and never reloads on daemon restart (`window.py:86-98` reconnects per call).
- **Root cause** The client treats `seq` as durable, but the counter restarts at 1 while the page keeps a higher cursor; every event is filtered out until the counter catches up, and the ring can evict them first.
- **Fix** (1) Initialise `next_event` from `now()*1024` and keep `max(prev+1)`. (2) Add `boot_id` (reuse `id()`) to `State` and to `status`; the client resets `seq=0` and requests a full snapshot on change. (3) Return `events_from` and `has_more`; the client detects `since < events_from-1` and resyncs with one "reconnected" notice. (4) Treat `seq` as opaque across boots.
- **End state** A daemon restart costs one resync; event-driven UI can never be filter-dead.
- **Why** Restarts are normal (tests do it, systemd does it); today they silently disable chat events.
- **Depends** BUG-098 · **Verify** Isolated: record `seq`/`boot_id`, stop/start the daemon, call `status` with the stale cursor → the new `ready` event returns and `boot_id` changed; the GUI keeps producing state transitions.
- **Effort** S · **Risk** low.

## BUG-100 · Worker lifecycle is guessed from a bare PID and never reaped
- **Severity** Medium · **Type** resource leak/false liveness
- **Evidence** `src/main.rs:420-428` (`kill(pid,0)` ⇒ running, no identity check); `:444-447` writes the pid file after spawn, so a write failure orphans a live child; `:41-51` a `Vec<Child>` with no `Drop`; `:544-557` `desktop_stop` only works for children in this process; no shutdown path at all.
- **Root cause** Supervision is inferred, not owned.
- **Fix** (1) Per-worker exclusive lock (`flock` on `runtime/<name>.lock`) plus a pid file removed in `finally`; a duplicate exits immediately. (2) Treat the `Child` handle as truth; validate the pid file against `/proc/<pid>/stat` start time. (3) On pid-write failure, kill and wait before returning. (4) Install SIGTERM/SIGINT handling (self-pipe + supervisor thread) that terminates workers, waits 1 s, kills stragglers, unlinks files; add `impl Drop for App`. (5) Report `workers:{voice:{pid,alive,since},desktop:{…}}` in `status` and the diagnostics panel. (6) On start, drop pid files whose `/proc/<pid>/cmdline` is not the expected worker.
- **End state** Exactly one instance per worker; stale pids cannot masquerade; navid exit releases the microphone; liveness is visible.
- **Why** A silent "voice stopped working until I delete a file" plus orphan processes holding the mic is a top support burden.
- **Depends** none · **Verify** Isolated `NAVI_TEST_WORKER_DIR` + `scripts/fake_worker.py`: a pid file containing the test's own pid does not fool `voice_start`; double start yields one process; SIGTERM leaves none; isolated mode still refuses real workers.
- **Effort** M · **Risk** medium (signal-handler discipline: atomic flag only).

---

### Appendix candidates (found during review, not part of the 100)
These are real and evidenced; they are listed for a follow-up plan rather than numbered, because they are small and independent:
1. **Terminal EOF is invisible and input is discarded** — `window.py:165-167` clears the read watch on EOF without notifying the page; `:171-185` swallows `OSError` and clears pending input. Fix: notify, close, hide the pane, clear `termSession`.
2. **The agent prompt file is rewritten on every daemon start** — `src/main.rs:188-200` calls `write_navi_agent` unconditionally, discarding hand edits. Fix: write only when the stored prompt changes, or back up the previous file.
3. **Harness prompt text is not control-character validated** — `src/main.rs:500-504` → `src/sessions.rs:58-77` checks length only; `\n`, `\x03`, `\x1b` in a transcript can submit lines or inject keys. Fix: reject control chars except `\n\t`, or encode via bracketed paste (BUG-076).

### Sequencing
1. BUG-093 (delivery contract) → BUG-095 (visibility).
2. BUG-094 → BUG-098 → BUG-099 as one event subsystem; do not split across changes.
3. BUG-096 → BUG-097 (survive failure before changing DPR), with geometry persistence on its own write path.
4. BUG-100 independent.
