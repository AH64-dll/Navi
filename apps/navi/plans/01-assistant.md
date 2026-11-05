# Section 1 — Assistant / OpenCode bridge

Scope: `workers/opencode_gateway.py`, `src/chief.rs` (gateway spawn, converse, slash, decide), `src/main.rs` (RPC), `ui/station.js`, `~/.config/opencode/agents/navi.md`.

**Verified upstream schema (live probe, this machine).** The gateway currently handles only `session.step.started`, `session.step.ended`, `session.execution.idle`, `session.execution.failed`, `session.text.delta`. Observed events also include `session.text.started/ended`, `session.reasoning.started/delta/ended`, `session.tool.input.started/ended`, `session.tool.called`, `session.tool.progress`, `session.tool.success`, `session.step.streamed`, `session.usage.updated`, `session.execution.succeeded`, `shell.created`, `shell.exited`. `session.step.started` carries the actual `agent` and `model`; `session.step.ended` carries `tokens` (incl. `reasoning`) and `cost`; `session.tool.called` has **no** `name` field — the name comes from `session.tool.input.started`. `session.tool.called.executed` was `false` even though the shell ran, so it is **not** a permission or execution signal. `session.permission.*` was not observed and must be probed before implementing approvals.

---

## BUG-001 · Tool calls, reasoning and step boundaries are discarded
- **Severity** P0 · **Type** missing
- **Evidence** `workers/opencode_gateway.py:278-281` collects only `session.text.delta`; `:301-305` keeps only `part.type=="text"`; `src/chief.rs:619-623` emits text-only `reply_delta`; `ui/station.js:40-46` renders plain text.
- **Root cause** The SSE reader is a text pump; every structured part is filtered before it can become state.
- **Fix** (1) In `_read_events`, add handlers for `session.reasoning.*` → `{"type":"reasoning","text":…}`; `session.tool.input.started` (capture `id`,`name`) and `.ended` (`id`,`text`); `session.tool.called` (`id`,`input`); `session.tool.progress` (`id`); `session.tool.success` (`id`,`content`,`metadata.exit`); and a defensive `session.tool.failure` (name unverified). (2) Correlate by tool-call `id`, buffer `input.started`/`input.ended` until `called`, and merge `progress`/`success` into one record. (3) In `stream_gateway` map these lines to event kinds `tool_use` and `reasoning` with `details:{id,name,status,input,output}`. (4) In `station.js` render a collapsible tool chip per id (name, status, input, truncated output) and a collapsed reasoning block on the live reply. (5) Cap stored output at 8 KiB and redact values matching `(?i)(token|secret|key|password)`. (6) Prefer `session.text.ended` (it carries the full text) as the turn terminator when it arrives, keeping message-store polling as the fallback — this removes the 250 ms store poll from the normal path.
- **End state** While the assistant works you see `shell · git status · running → completed (exit 0)` and a reasoning disclosure, then the answer; no silent waits.
- **Why** Tools and reasoning are the visible substance of a coding agent; without them the assistant is a chat box.
- **Depends** none · **Verify** Extend `scripts/test_opencode_gateway.py` FakeOpenCode to emit `tool.input.started/ended`, `tool.called`, `tool.success` and `reasoning.delta`; assert `ask --stream` emits `{"type":"tool"}` and `{"type":"reasoning"}` lines and that one tool id produces exactly one final record.
- **Effort** M · **Risk** `tool.failure` name unverified; tolerate its absence.

## BUG-002 · The "thinking" state is fabricated at dispatch time
- **Severity** P2 · **Type** bug
- **Evidence** `src/chief.rs:767` emits `thinking` before any model work; `:735-738` does the same for slash; `ui/station.js:131` maps that event to the status label.
- **Root cause** A UI state was invented from the dispatch path instead of from model output; real reasoning is dropped upstream (BUG-001). *(Merged into BUG-001: its fix is step 4 there — route real reasoning to the label and rename the dispatch event to `working`.)*
- **Fix** Emit `kind:"working"` from dispatch; reserve `thinking` for actual `session.reasoning.delta`; label the strip "Working" until the first reasoning delta.
- **End state** "Thinking" means the model is reasoning, not that a request was sent.
- **Why** Honest state is the difference between feedback and decoration.
- **Depends** BUG-001 · **Verify** Status during a non-conversational command shows `working`; during a reasoning-capable turn shows `thinking` only after a `reasoning.delta`.
- **Effort** S · **Risk** none.

## BUG-003 · The system prompt forbids coding-agent behaviour
- **Severity** P0 · **Type** design
- **Evidence** `src/main.rs:22` (`DEFAULT_NAVI_PROMPT`: "one to three short sentences… no markdown, no code blocks… replies may be spoken aloud"); `~/.config/opencode/agents/navi.md:4` (same text deployed); `src/main.rs:367-378` rewrites the file from settings.
- **Root cause** The only prompt is written for a voice assistant, so the model is instructed to suppress exactly what a coding agent must produce.
- **Fix** Rewrite the prompt: "Start with one plain spoken sentence, then use markdown and fenced code freely. Work inside the current project directory. Use tools for real work. Never state a completion percentage." Keep a `spoken_summary` convention (first paragraph) for TTS. Extend the `LEGACY_NAVI_PROMPT` migration so saved prompts are upgraded, and stop rewriting the agent file on every daemon start (see Appendix A2).
- **End state** Answers contain readable code and diffs; voice still speaks only the lead sentence.
- **Why** No model choice fixes an instruction that forbids the product's purpose.
- **Depends** none · **Verify** Isolated `settings` save persists the new prompt; `~/.config/opencode/agents/navi.md` contains it after restart; TTS reads only the first paragraph.
- **Effort** S · **Risk** users who edited the prompt keep their text unless the migration targets the exact legacy string.

## BUG-004 · The agent runs in `$HOME`, not in a project
- **Severity** P0 · **Type** bug
- **Evidence** `workers/opencode_gateway.py:180` creates a session with `{"title":…}` only; `:345` sends `{"text":text}` only; `chief.rs:577-586` spawns the gateway without `current_dir`; systemd unit has no `WorkingDirectory` (`scripts/install.py:57`). Live probe: `GET /api/location` → `/home/amrhares`; `shell.created.info.cwd` → `/home/amrhares`.
- **Root cause** No directory ever crosses the UI → host → gateway → OpenCode boundary, so tools resolve against the daemon's cwd.
- **Fix** (1) Resolve the active project (BUG-083) and pass it as `--directory` to the gateway. (2) `ensure_session` posts `{"title":"Navi · <name>","directory":<path>}`; verify the field against the live server once (it is plausible but unverified). (3) Set `.current_dir(path)` on the gateway child. (4) If the server ignores `directory`, prepend `[working directory: <path>]` to the prompt as a fallback and record a warning event. (5) Store the directory per session (BUG-092).
- **End state** "fix the tests" edits files under the registered project; `shell.created.cwd` equals that path.
- **Why** Per-project work and the completion graph both require a correct cwd; today the agent can edit anything in your home.
- **Depends** BUG-042 (project model) · **Verify** FakeOpenCode records POST bodies; assert `directory` present and `current_dir` set; live probe re-run once.
- **Effort** M · **Risk** directory field unverified — fallback keeps behaviour correct either way.

## BUG-005 · No file, image or context attachments
- **Severity** P1 · **Type** missing
- **Evidence** `workers/opencode_gateway.py:345` sends text only; `ui/index.html:31` is a single text input; `src/main.rs:508-510` accepts only `text`.
- **Root cause** The prompt channel was designed as a string, so no structured content can exist.
- **Fix** Extend `command`/`prompt` RPC with `attachments:[{path|data,mime,name}]`; gateway builds a `parts:[{type:"file"|"image",…}]` array (schema unverified — probe once) with text retained; UI adds attach button and drag-drop; host resolves paths and enforces 4 files × 2 MiB; images travel base64 over the existing message bridge (CSP unchanged).
- **End state** Drop a screenshot or log file; the answer references its contents.
- **Why** Debugging is image- and log-driven; text-only input forces manual copying.
- **Depends** BUG-004 · **Verify** FakeOpenCode asserts a `parts` array; a temp-file attachment round-trips in isolated mode.
- **Effort** M · **Risk** parts schema unverified; text-only fallback preserves today's behaviour.

## BUG-006 · No approval path for gated tools
- **Severity** P0 · **Type** missing
- **Evidence** no `permission|approve|allow` matches in gateway, host or UI; the `command`/`prompt` dispatch has no approval op; `session.tool.called.executed` was `false` while the shell ran, so it cannot serve as a proxy.
- **Root cause** The SSE whitelist excludes any permission family and no response channel exists.
- **Fix** (1) Probe the permission event name and reply endpoint with a gated tool before coding. (2) Handle that event → `{"type":"permission","id","tool","input"}`. (3) Emit `kind:"approval"` with a pending-approval table in the daemon. (4) Add RPC `permission_reply {id,decision}` → gateway `permission` mode posting the decision. (5) UI renders Allow/Deny on the pending row; Stop cancels. (6) Never infer approval from `executed`.
- **End state** Gated tools ask, and the turn continues in seconds instead of dying at 180 s.
- **Why** Unattended approval loops are unsafe; unattended *silence* is worse.
- **Depends** BUG-001 · **Verify** FakeOpenCode emits a permission event → assert an `approval` event and exactly one POST for Allow and one for Deny; live probe once for the real names.
- **Effort** L · **Risk** schema entirely unverified until probed.

## BUG-007 · Stop does not stop the server side
- **Severity** P1 · **Type** bug
- **Evidence** `src/main.rs:533` discards the interrupt result (`let _ =`); `src/chief.rs:607-610` kills the child on timeout without interrupting; `workers/opencode_gateway.py:229` does not check the interrupt response.
- **Root cause** Cancellation is local-only on the paths that matter, and its outcome is unobservable.
- **Fix** Interrupt before killing on both generation-mismatch and deadline paths; propagate `{interrupted:bool}` to the reply; retry once after 1 s; UI warns "turn may still be running" when false. Record the outcome in the session event log.
- **End state** Stop always ends the server turn or tells you it could not.
- **Why** A half-stopped turn poisons the only session (BUG-008).
- **Depends** BUG-008 · **Verify** FakeOpenCode counts `/interrupt` POSTs; cancel during a hung ask → one POST and `interrupted:true`.
- **Effort** S · **Risk** interrupting an idle session is a harmless no-op.

## BUG-008 · A stopped or timed-out turn leaves the session wedged
- **Severity** P0 · **Type** bug
- **Evidence** `workers/opencode_gateway.py:326-334` never resets server state; `:268` clears busy only on `step.ended`/`execution.idle`, but the observed terminal event is `execution.succeeded`; `src/chief.rs:607-610` kills only the child.
- **Root cause** Terminal-event handling is incomplete and there is no idle-wait/repair path.
- **Fix** Clear busy on all terminal events (`execution.succeeded|failed|idle|cancelled|interrupted`, defensively on unknown `execution.*`); add `wait_idle(timeout)` that interrupts then waits for a terminal event before prompting; on timeout, `new_session` for the current project; never treat an incomplete message as a reply.
- **End state** The next message works after any Stop or timeout; aborted turns are never returned as answers.
- **Why** One timeout currently poisons the only session; this is the reliability floor for the assistant.
- **Depends** BUG-007 · **Verify** FakeOpenCode emits only `execution.succeeded` (no `step.ended`) → busy clears and ask returns; hold busy → second ask interrupts then prompts.
- **Effort** M · **Risk** unknown terminal names handled defensively.

## BUG-009 · No token or cost telemetry
- **Severity** P2 · **Type** missing
- **Evidence** `session.step.ended` carries `tokens{input,output,reasoning,cache}` and `cost`; `session.usage.updated` carries cumulative values; the gateway drops both; `ui/station.js:126` shows model + voice only.
- **Root cause** Usage arrives on the wire and is discarded; no display contract exists.
- **Fix** Capture `step.ended` and `usage.updated` in the gateway; add `usage`/`session_usage`/`finish` to the done line and reply event; render per-reply `12.4k tok · $0.002` and cumulative cost in the status strip.
- **End state** Every reply shows its cost and a truncated vs finished turn is distinguishable.
- **Why** Cost transparency is table stakes for a paid coding agent.
- **Depends** BUG-001 · **Verify** FakeOpenCode emits both events; assert values in `done` JSON and in the `reply` details.
- **Effort** S · **Risk** none — fields verified live.

## BUG-010 · The displayed model is the requested one, not the answering one
- **Severity** P2 · **Type** bug
- **Evidence** `ui/station.js:126` reads `settings.assistant.model`; `session.step.started` carries the actual `agent` and `model{id,providerID,variant}` but the gateway keeps only `_busy`.
- **Root cause** The one authoritative per-turn identity is thrown away at parse time.
- **Fix** Capture `agent`/`model` on `step.started`; carry `actual_model`/`actual_agent` through done and reply events; display them, and render `actual (requested: cfg)` when they differ.
- **End state** You see the model that answered; silent fallbacks become visible.
- **Why** Trust and debuggability.
- **Depends** BUG-009 (same capture site) · **Verify** FakeOpenCode reports a different model id → the UI text reflects it.
- **Effort** S · **Risk** none — verified live.

## BUG-011 · Model and agent are re-applied to a live session on every turn
- **Severity** P1 · **Type** bug
- **Evidence** `workers/opencode_gateway.py:184-186` POSTs `/model` and `/agent` before every ask/slash; `:219-221` duplicates it in `new_session`.
- **Root cause** Configuration is applied per turn rather than per session change, so concurrent gateway processes race and a settings edit mutates an in-flight thread.
- **Fix** Extract `_configure()`; persist `{id,model,agent,directory}` in the session file (migrate the legacy plain-id file); call only when the tuple changes or the session is new; prefer per-prompt model fields only if the live API accepts them.
- **End state** One configuration per session; edits apply to the next turn, not the current one.
- **Why** Predictable per-session behaviour and a prerequisite for per-project sessions.
- **Depends** BUG-004 · **Verify** FakeOpenCode counts: two asks → one model/agent POST; changed model → exactly one re-POST.
- **Effort** S · **Risk** low.

## BUG-012 · One global assistant session for every project
- **Severity** P1 · **Type** design
- **Evidence** `workers/opencode_gateway.py:69-71` stores a single id in `~/.local/share/navi/opencode-session`; `:191-203` reuses it unconditionally.
- **Root cause** Session identity is global rather than keyed by project.
- **Fix** Store `{directory:{id,model,agent}}`; `ensure_session(directory=)` creates/reuses per project with title `Navi · <project>`; `oc_new` resets only the current project; expose the mapping in `status` for the graph panel.
- **End state** Each project keeps its own conversation; switching projects does not leak context.
- **Why** Per-project context is a core product goal and contains the blast radius of BUG-008.
- **Depends** BUG-004 · **Verify** FakeOpenCode: ask in A then B → two session POSTs; repeat A → none.
- **Effort** M · **Risk** stale ids already validate via GET.

## BUG-013 · No markdown, code, or diff rendering
- **Severity** P1 · **Type** missing
- **Evidence** `ui/station.js:23,44,51` assign `textContent`; CSP `script-src 'self'`; no `pre/code` styles in `station.css`.
- **Root cause** The renderer treats every reply as one plain string.
- **Fix** Add a dependency-free renderer that builds DOM nodes (never `innerHTML`): fenced/inline code, bold, links (validated to http/https), lists; code blocks get a copy button; reasoning and tool parts render collapsed. Style `pre/code` and add a diff viewer for `tool` outputs that contain patches. External links must be opened through a host message (`open_external`) because `window.py:75-81` blocks every non-local navigation — render the URL as copyable text if the host refuses. Sanitise link targets and never load remote images (CSP `connect-src 'none'`).
- **End state** Code is readable and copyable; lists and emphasis render; no HTML injection.
- **Why** Code is the primary output of a coding agent.
- **Depends** BUG-003 · **Verify** An isolated reply containing a fenced block produces `.msg pre code`; a reply containing `<img onerror>` produces text, not an element.
- **Effort** M · **Risk** XSS — mitigated by node construction only.

## BUG-014 · Failures are rendered as confident answers
- **Severity** P1 · **Type** bug
- **Evidence** `src/chief.rs:727-729` converts any gateway error into reply text with `source:"unavailable"`; `:783-786` emits it as a normal `reply`; `ui/station.js:133-135` ignores `source`.
- **Root cause** The error path and the success path share one payload shape.
- **Fix** Return `{ok:false,error}` from `converse`/`run_slash` on gateway failure; emit `kind:"error"`; render a persistent error row with retry; keep `source` for telemetry. Reserve reply text for the model.
- **End state** An outage reads as "Assistant unavailable: <reason>", never as an answer.
- **Why** Silent failure destroys trust in every other output.
- **Depends** none · **Verify** Point `NAVI_OPENCODE_URL` at a closed port → `command` returns an error and no `reply` event; the UI shows one error row.
- **Effort** S · **Risk** voice needs a spoken fallback string.

## BUG-015 · Streaming is quantised to the 700 ms poll
- **Severity** P1 · **Type** design
- **Evidence** `ui/station.js:146` polls every 700 ms; `src/chief.rs:620-623` emits one event per SSE chunk; `src/main.rs:464` serves events only through `status`.
- **Root cause** The transport is a sampled ring, not a push channel.
- **Fix** Add a host-driven push: a GLib timeout (40 ms while a turn is open, 700 ms idle) calls the existing `status{since}` over the socket and evaluates `navi.events(payload)`; coalesce deltas at the producer (BUG-098); paint once per animation frame. CSP stays `connect-src 'none'`.
- **End state** Tokens appear in tens of milliseconds; no bursty text.
- **Why** Streaming is how a user knows the agent is alive and on track.
- **Depends** BUG-098 · **Verify** Fake gateway with timed deltas → inter-render gap < 250 ms in the GUI harness.
- **Effort** M · **Risk** one held socket per UI; bounded by the turn.

## BUG-016 · A second request is refused instead of queued
- **Severity** P1 · **Type** design
- **Evidence** `src/main.rs:522-525` rejects with "Chief is busy; stop the current command first"; no queue exists.
- **Root cause** The command lock is exposed as a user-facing error instead of scheduling.
- **Fix** Add a bounded queue (4) drained on completion, cleared by cancel; return `{queued:true,position}`; UI marks queued rows and promotes them when they run; slash commands join the same queue (BUG-018).
- **End state** A follow-up message waits its turn and runs automatically.
- **Why** Refusals break conversational flow and force manual retries.
- **Depends** none · **Verify** Two concurrent `command` RPCs → the second returns `{queued:true}` and its reply arrives without a resend.
- **Effort** M · **Risk** queue must honour cancel/generation.

## BUG-017 · The slash catalog is fetched once and cached forever
- **Severity** P2 · **Type** bug
- **Evidence** `ui/station.js:60-67` caches `slashCommands` including failures; `:103`; the catalog calls also skip `ensure_up` (`workers/opencode_gateway.py:365-380`), so a late-starting server is never consulted.
- **Root cause** No TTL, failure caching, and no autospawn on catalog reads.
- **Fix** Cache `{list,fetchedAt}` with a 60 s TTL; never cache failure; refetch on `oc_new`; call `ensure_up()` in `commands`/`models`/`agents` (respecting `NAVI_OPENCODE_AUTOSPAWN=0`); invalidate the list when the gateway session changes.
- **End state** Suggestions recover within a minute and work before the first message.
- **Why** Discoverability of commands is part of the agent surface.
- **Depends** none · **Verify** Fake catalog fails then succeeds → the list recovers without reload; with autospawn disabled, catalog returns a fast, clear error.
- **Effort** S · **Risk** none.

## BUG-018 · Slash commands bypass the command lock
- **Severity** P1 · **Type** bug
- **Evidence** `src/main.rs:512-514` returns `run_slash` before `command_lock` is acquired at `:522-525`.
- **Root cause** The slash fast path predates the lock and was never serialised.
- **Fix** Acquire the lock (or enqueue, after BUG-016) before dispatching slash; keep `decide` lock-free as the read-only path; keep persona switching pre-lock.
- **End state** Two turns can never interleave on the shared session.
- **Why** Interleaved prompts corrupt the conversation and the delta stream.
- **Depends** BUG-016 · **Verify** Hung fake ask + `/init` → the second is queued/refused, and FakeOpenCode sees strictly ordered prompts.
- **Effort** S · **Risk** none.

## BUG-019 · `/help` and `/commands` are faked locally
- **Severity** P2 · **Type** bug
- **Evidence** `workers/opencode_gateway.py:429-435` synthesises a 40-line catalog instead of posting the command; a unit test enshrines the fake.
- **Root cause** An old workaround for a missing catalog endpoint became permanent behaviour.
- **Fix** Delete the interception; send `/help` and `/commands` through `/api/session/{id}/command`; keep `oc_commands` as a pure catalog read; update the test to assert the POST.
- **End state** Help reflects the server's real commands and options.
- **Why** Faked output hides capabilities and drifts from reality.
- **Depends** none · **Verify** FakeOpenCode asserts `POST …/command {"name":"help"}`; the response body is returned verbatim.
- **Effort** S · **Risk** none.

---

### Corrections applied during review
- Tool-call names come from `session.tool.input.started`, not `session.tool.called` (the latter has no `name`).
- `session.execution.succeeded` is the observed terminal event; `execution.idle` was not observed and is kept only defensively.
- Reasoning events are `session.reasoning.*`, not `session.reason.*`.
- Usage is already on the wire (`step.ended`, `usage.updated`); no extra store call is needed.
- `session.tool.called.executed` is not a permission signal (it was `false` while the shell ran).
- The session-create `directory` field remains unverified; BUG-004 carries a text fallback.
