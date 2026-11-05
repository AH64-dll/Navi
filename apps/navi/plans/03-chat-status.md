# Section 3 — Chat surface, status and message UX

Scope: `ui/station.js`, `ui/station.css`, `ui/index.html`, `src/main.rs` (events/status), `src/chief.rs` (emission).

**Message model this section implements.** A message is an object, not a DOM append: `{id, turn, role, time, status, parts[], meta:{model,agent,persona,source,session,durationMs,usage?}}`. Parts are ordered and typed: `text`, `reasoning`, `tool`, `diff`, `error`. `time` comes from `event.time` (currently never read by the UI). `status` is `streaming|complete|failed|stopped`. Event mapping: `transcript` (new) opens a user message; `thinking` opens/attaches a navi message as a `reasoning` part; `reply_delta` appends to the tail `text`; `reply` finalises; `command` appends a `tool` part; `error` sets `failed`; `stopped` sets `stopped`; `launched` is a system tool message. The current `reply_delta` renderer is dead (BUG-048), so the model lands together with that fix.

---

## BUG-034 · The status chip is stuck at `starting`
- **Severity** High · **Type** UX/state
- **Evidence** `ui/index.html:35` (`<span id="state">starting</span>`); `ui/station.js:120-143` writes state only inside chat-event branches; `ui/station.js:144-147` never derives it; the boot event is `ready` (`src/main.rs:187`) and has no UI branch; the status payload already carries `generation`, `gui_open`, `voice.status`.
- **Root cause** Phase is a side effect of chat events rather than a projection of the payload.
- **Fix** Add a single `derivePhase(status)` returned by the host (`ready|working|attention|offline`), set it on every poll, and make chat events update only the turn sub-state. Handle `ready`/`chief`/`persona` in the event loop.
- **End state** A quiet, healthy app says "Ready"; offline says "Host not responding"; a turn says "Working".
- **Why** A permanent "starting" label makes a working app look broken.
- **Depends** BUG-095 (push transport) · **Verify** Isolated boot with no events: after one poll, `#state` is not `starting`; stop the daemon → `offline`.
- **Effort** S · **Risk** none.

## BUG-035 · The status line shows a raw model path and an unexplained "off"
- **Severity** Medium · **Type** UX/copy
- **Evidence** `ui/station.js:126` concatenates `settings.assistant.model` and `value.voice.status`; default model is `commandcode/deepseek/deepseek-v4.1-flash` (`src/main.rs:23`); voice emits `off|listening|waiting for window|transcribing`.
- **Root cause** Stored references and worker enums are rendered verbatim with no legend.
- **Fix** Show `model.name` (short) + a voice pill with an icon and plain words (`Mic off`, `Listening`, `Waiting for window`, `Transcribing`); put provider/ID in a tooltip; render the *actual* model from BUG-010.
- **End state** "DeepSeek V4.1 Flash · Mic off" instead of a path and a bare "off".
- **Why** The status line is the only always-visible explanation of what the app is doing.
- **Depends** BUG-010 · **Verify** Snapshot text contains no `/` model path and no bare `off`.
- **Effort** S · **Risk** none.

## BUG-036 · `attention` is sticky
- **Severity** Medium · **Type** state
- **Evidence** `ui/station.js:138` sets `attention` on any `error`; `src/main.rs:601-604` emits `error` for every failed RPC (settings, select, create, oc_models); only four branches clear it; no timer; the poll payload is not consulted.
- **Root cause** Request-scoped failures are treated as a terminal app state.
- **Fix** Separate `phase` (derived) from `lastError` (transient with timestamp); show a dismissible error row; clear `attention` when the next successful request or turn arrives; never set app phase from a side-channel RPC failure.
- **End state** One failed settings read shows a small row, not a permanent alarm badge.
- **Why** Permanent red trains users to ignore errors.
- **Depends** BUG-037 · **Verify** Fail `oc_models`, then succeed a `status`; assert the phase returns to `ready`.
- **Effort** S · **Risk** low.

## BUG-037 · A failed turn leaves no row in the transcript
- **Severity** High · **Type** UX/data
- **Evidence** `ui/station.js:161` catches, calls `finishLive('')`, toasts, and appends nothing; `src/chief.rs:733-757` lets slash errors propagate with no reply event; `ui/station.js:47-49` returns false when `liveRow` is null (always, see BUG-048); `src/chief.rs:727-730` turns outages into successful reply text.
- **Root cause** Turns have no identity that can carry failure, and errors travel on a side channel.
- **Fix** Give each turn an id; on failure create a message with an `error` part and `status:'failed'`, with Retry; route RPC errors to that row when a turn is open, otherwise to the toast. On Retry, resend the original text.
- **End state** The feed shows "Assistant unavailable — retry" in place, and the conversation preserves what happened.
- **Why** Silent failures make the app feel like it ignored you.
- **Depends** BUG-014, BUG-048 · **Verify** Point the gateway at a closed port; submit; assert one `.msg[data-status="failed"]` with a Retry control and no `reply` event.
- **Effort** M · **Risk** low.

## BUG-038 · The toast renders over the harness terminal
- **Severity** Medium · **Type** layout
- **Evidence** `ui/station.css:52-53` (`bottom:calc(3vh+36vh+14px)`, z40) vs terminal `top:12vh;height:min(62vh,620px)` z8; at 920 px the toast sits at ≈561 px inside the terminal.
- **Root cause** The toast is anchored to the chat's height and ignores its sibling.
- **Fix** L0 puts notifications in a dedicated region above the composer or as a transient inline banner inside the chat column; never derive position from another panel's percentage.
- **End state** Toasts never cover terminal content.
- **Why** An error message that hides the content it describes is worse than no message.
- **Depends** BUG-020 · **Verify** V-FLOW: trigger a toast with the terminal open; assert no intersection.
- **Effort** XS · **Risk** low.

## BUG-039 · A failed slash command toasts twice
- **Severity** Medium · **Type** UX/logic
- **Evidence** `src/main.rs:601-604` emits `error` **and** returns `ok:false`; `ui/station.js:161` toasts the rejection; `:138` toasts the event ≤700 ms later; `window.py:118` is a third `navi.notice` path; each toast lasts 6 s.
- **Root cause** One failure published on two channels with no correlation key.
- **Fix** Correlate by `turn`/`ticket`; the UI consumes one channel (the event when a turn is open, the rejection otherwise). `window.py` notices become `console`/error rows, not toasts, when they concern an in-flight request.
- **End state** One failure, one message.
- **Why** Duplicate errors look like two bugs.
- **Depends** BUG-037 · **Verify** Force a slash failure; assert exactly one visible error surface.
- **Effort** S · **Risk** low.

## BUG-040 · No empty state or onboarding
- **Severity** Medium · **Type** UX
- **Evidence** `ui/index.html:24-25` empty `#chat-log`; `ui/station.js:19-27` only appends; `evidence/isolated-gui.json` records `messages: 0`; entry points ("+ session", Settings → New session, mic) are undiscoverable; the mic can be disabled with no explanation.
- **Root cause** The surface assumes history exists.
- **Fix** Render a first-run block when the log is empty: "Type a request · Press `/` for commands · Say “Hey Navi”" with three buttons (New harness session, Settings, Enable mic). Show a mic-disabled explanation (isolated mode / permission missing) instead of a dead button.
- **End state** A new user knows the three ways to act within two seconds.
- **Why** An empty black panel with a text field is the definition of unfinished.
- **Depends** BUG-034 · **Verify** Fresh isolated profile: the empty-state block renders and disappears after the first message; no duplicate after reload with history (BUG-047).
- **Effort** S · **Risk** low.

## BUG-041 · No per-message actions or metadata
- **Severity** High · **Type** UX/data
- **Evidence** `ui/station.js:19-27` builds only `span.who + p`; `ui/station.css:8-15` has no action rules; `event.time` and `details.persona/source` are never read; `inspect()` exposes no hooks.
- **Root cause** Rows are transient text appends, not objects.
- **Fix** With the message model, add per-row actions on hover: copy, retry (last user turn), edit-and-resend, timestamp, model, duration, tokens/cost (when present, never a fake 0), and a source badge. Keyboard-reachable (`tabindex`, Enter).
- **End state** You can copy code, retry a bad answer, and see what it cost.
- **Why** These are the baseline verbs of any agent chat.
- **Depends** BUG-047, BUG-009 · **Verify** GUI harness: copy writes the text to the clipboard API (or shows a "copied" state); retry resends the same text; metadata renders only when provided.
- **Effort** M · **Risk** low.

## BUG-042 · Role and part styling is flat
- **Severity** Medium · **Type** visual
- **Evidence** `ui/station.css:8-15` — one block with colour/alignment modifiers; no rules for tool/diff/error/reasoning parts; the streaming caret is dead CSS (BUG-048).
- **Root cause** There is no visual grammar for message state or non-prose parts.
- **Fix** Define a small type scale and spacing system; distinct treatments for user (quiet surface, right), navi (plain), tool (monospace chip, collapsible), diff (green/red lines, path header), error (left rule, retry), reasoning (dimmed, collapsed). Use one accent colour, no gradient text.
- **End state** A conversation is scannable: you can tell a tool call from prose from an error at a glance.
- **Why** Visual hierarchy is what makes a chat feel professional.
- **Depends** BUG-041 · **Verify** Snapshot DOM fixtures for each part type; assert computed styles differ and contrast ≥4.5:1.
- **Effort** M · **Risk** low.

## BUG-043 · Session chips show profile ids, duplicates, and never prune
- **Severity** Medium · **Type** UX/data
- **Evidence** `ui/station.js:240-243` renders `session.name` plus `session.profile` (the profile **id**); `src/sessions.rs:16-42` returns every row, running or not; `:99-101` defaults the name to the profile name, so repeated launches collide (two "Fixture shell" in `evidence/gui-workflow.workflow.json`); `.dead` only lowers opacity.
- **Root cause** The strip renders the table verbatim with no live/dead partition, naming, disambiguation, or pruning.
- **Fix** Partition live/dead; show the profile *name* in the tooltip, not the id; disambiguate duplicates with a short index or cwd basename; show state (running/exited/needs-input) and last activity; group dead sessions under a collapsed "Recent" section; remove on explicit delete (BUG-075).
- **End state** A strip that reads like a session list, not a database dump.
- **Why** The strip is the primary navigation surface for harness work.
- **Depends** BUG-072, BUG-096 · **Verify** Fixture with two identically named sessions renders two distinguishable chips; dead ones are collapsed, not grey-forever.
- **Effort** M · **Risk** low.

## BUG-044 · The model picker is settings-only, unsearchable, and can blank the model
- **Severity** High · **Type** UX/data loss
- **Evidence** `ui/index.html:50-51` (`<select>` inside Settings); `ui/station.js:292-304` builds `${name} · ${providerID}` with no filter; `:300-303` clears the seeded option then sets `value=current` with no missing-value guard, so a catalog that lacks the saved model yields `""`; `:371` persists it; `src/chief.rs:698-701` accepts an empty string; the gateway refuses every prompt.
- **Root cause** A native select used as a catalog browser, and the save path trusts `selectedIndex` even when it is `-1`.
- **Fix** (1) Guard: if the current model is not in the catalog, keep the seeded option and never persist an empty value. (2) Replace the select with a searchable combobox (local filtering, provider grouping, current pinned). (3) Add an inline model switcher in the composer footer for the current session. (4) Validate `assistant.model` server-side against `^[^/\s]+/\S+$` and reject empty.
- **End state** Changing models is one keystroke away and cannot silently break every turn.
- **Why** Model choice is a core control, and the current path has a data-loss bug.
- **Depends** BUG-012 · **Verify** Fixture catalog missing the saved model → save → assert the stored model is unchanged; search "deepseek" narrows the list.
- **Effort** M · **Risk** low (the guard alone is S).

## BUG-045 · A repeated identical user message is dropped
- **Severity** High · **Type** UX/logic
- **Evidence** `ui/station.js:28-31` (`text===lastUser` → return), called from submit (`:154`) and every `thinking` event (`:130`); multi-clause commands emit one `thinking` per clause (`src/chief.rs:760-767`), so "a then b" renders extra user bubbles.
- **Root cause** Text-keyed de-duplication with one scalar, overloaded to suppress synthetic echoes and genuine repeats.
- **Fix** Tag echoes (`details:{text,echo:true,request_id}`) and skip only when the UI already rendered that request id; keep a small set of recent ids; render user rows once per origin (typed, voice, transcript event); make `thinking` a reasoning part, never a user row.
- **End state** Returning an identical request renders both; "do X then Y" renders one user bubble.
- **Why** Missing messages look like the app is broken.
- **Depends** BUG-037 · **Verify** Two identical submits → two rows; a two-clause command → one user row, two reasoning parts.
- **Effort** S · **Risk** low.

## BUG-046 · An identical reply is hidden for 15 seconds
- **Severity** High · **Type** UX/logic
- **Evidence** `ui/station.js:32-38` (`recent.get(text)` with `now−seen<15000`); `:158` and `:134` both render the same reply (RPC result and event); because `finishLive` always returns false (BUG-048), every reply goes through `naviSaid`, so a repeated answer can be the only copy and still vanish.
- **Root cause** Content-keyed de-duplication used as a workaround for double delivery.
- **Fix** Deduplicate by message id/seq, not text; render the event as authoritative and ignore a duplicate RPC payload with the same id; drop the time window entirely.
- **End state** Repeated answers always render; the same answer never double-renders.
- **Why** Suppressing legitimate output is indistinguishable from a hang.
- **Depends** BUG-048 · **Verify** Two `reply` events with identical text and different seq → two rows; same id → one.
- **Effort** S · **Risk** low.

## BUG-047 · The chat is capped at 200 rows and never persisted
- **Severity** High · **Type** data
- **Evidence** `ui/station.js:25` removes the first child beyond 200; `:382-387` counts DOM children; no storage or history RPC; the DB has only `settings`/`sessions`; a WebProcess restart recreates the view (`window.py:62-67`) and discards the transcript; `scroll()` (`:18`) is called unconditionally so scrolling up is impossible.
- **Root cause** The transcript is a DOM artifact of a volatile event ring.
- **Fix** Persist messages in SQLite (BUG-085) with `(id, project_id, session_id, role, parts_json, time)`; hydrate the last N on boot and page older history; keep the DOM virtualised (render window ±50 rows); remove the 200-row cap; only auto-scroll when the user is at the bottom (track `scrollHeight − scrollTop − clientHeight < 40`), and show a "jump to latest" pill.
- **End state** Restarting the app preserves the conversation; scrolling up during a stream is possible.
- **Why** Losing history is the fastest way to make an assistant feel disposable.
- **Depends** BUG-085 (events/messages tables) · **Verify** Submit, restart the daemon and the window, assert the rows return in order; scroll up during a 50-delta stream and assert `scrollTop` is unchanged.
- **Effort** M–L · **Risk** medium (schema + paging).

## BUG-048 · Streaming replies never render at all
- **Severity** High · **Type** bug
- **Evidence** `ui/station.js:19-20` `line()` returns `null` when text is empty; `:42` calls `line('navi','', 'streaming')`, so `liveRow` is always `null`; `liveText` accumulates into nothing; `liveRow?.querySelector('p')` is a no-op; every `reply_delta` (`:131`) is discarded; the `.msg.streaming p::after` caret is dead CSS; `finishLive` always returns false.
- **Root cause** The row factory refuses to create an empty row, so the streaming path is unreachable — and the bounded ring (BUG-098) could not deliver a full turn even if it were.
- **Fix** Split `row(role, extra)` (always creates the node) from `appendText(row, text)`; create the streaming row on the first delta; drive the caret from `data-status`; paint at most once per animation frame from an accumulator; replace (not append) the turn text from BUG-098's coalesced payload; finalise in place.
- **End state** The answer appears progressively in one row and is finalised in place, with no duplicate and no flash.
- **Why** Without this, the product cannot show the one thing a coding agent must show: work in progress.
- **Depends** BUG-098, BUG-095 · **Verify** GUI harness with `Fixture.delay=2`: assert a `.msg[data-status="streaming"]` exists, `textContent.length` grows between two samples 300 ms apart, and exactly one navi row remains after completion.
- **Effort** M · **Risk** medium (client and payload shape change together).

---

### Corrections applied during review
- #32 is permanent, not a slow load: the `ready` event has no UI branch and no poll-derived phase exists.
- #39 is exact: rows are `div.msg > span.who + p`; `event.time` is never read.
- #41: chips render the **profile id**, not a session id; duplicate names are real (`Fixture shell` ×2).
- #44: because streaming never renders, the final `reply` always goes through `naviSaid`, so a repeated reply can vanish even as the only copy.
- #37 has three producers: `main.rs` emits an `error` event while also returning `ok:false`, and `window.py` has a third notice path.
- Saving Settings can blank the model (folded into BUG-044) and the GUI workflow currently exercises exactly that path without asserting the result.
- Auto-scroll hijacks the reader (folded into BUG-047 step 6).
- Machine clauses are rendered as user messages (folded into BUG-045).
