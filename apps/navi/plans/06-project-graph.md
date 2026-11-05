# Section 6 — Project model and the completion graph

Scope: `src/main.rs` (settings, SQLite schema, events ring), `src/sessions.rs`, `src/chief.rs` (`project_for`), `ui/station.js`, `ui/station.css`, `ui/index.html`.

**Definitions this section fixes.** A project is a row, not a settings object: `projects(id, name UNIQUE, path UNIQUE, git_root, origin, path_state, archived, created)`. Aspects group work: `aspects(id, project_id, key, title, kind, weight, created)`. Tasks carry state: `tasks(id, project_id, aspect_id, title, state[todo|in_progress|done|blocked|dropped], weight, created, updated)`. Plans and checkpoints record intent and proof. `signals` is the append-only evidence ledger: `(project_id, task_id, aspect_id, kind, source, value, confidence, evidence, time)`, with `evidence` non-empty by CHECK. Every number is **derived** from signals by one pure function; nothing stores a percentage.

**Completion formula.** Task score from the newest decisive signal: user verdict `1.0/0.0`; verified pass/commit `1.0`; verified fail `0.0`; uncorroborated assistant claim `0.5 × confidence` (halved after 7 days); touched-only `0.5 × confidence`; nothing `0.0` and flagged *unknown*. Aspect percentage = weighted mean over its tasks (dropped excluded); empty aspects leave the denominator. Project percentage = weighted mean over aspects. `verified` = share of weight backed by non-assistant signals. `confidence` = coverage × freshness (10-day half-life). With no aspects, `percent_null=true` and the UI renders `—`, never 0 or 100. The assistant may write only `assistant_claim` (value capped, decays); the user writes authoritative state; git and harness adapters write verified signals. Every number returns the signal ids behind it.

> **Superseded: estimates.** Harnesses, the detector and the user now state completion directly as `estimate` signals for the whole project or one part, through the harness link (`navid mcp`, `navid progress`). A part takes its newest estimate unless newer verified task evidence exists, and is otherwise the weighted mean of its tasks; the headline is the newer of the whole-project estimate and the weighted part mean. `assistant_claim` remains only as a legacy kind. The current rules are in `README.md`, section "Project progress".

---

## BUG-083 · Projects have no identity and are never validated
- **Severity** High · **Type** data model
- **Evidence** `src/main.rs:119` is the entire model (`{name,path}`, path compiled from `CARGO_MANIFEST_DIR`); `:296-303` validates only "is an array, ≤100"; `src/chief.rs:1031-1062` resolves by lowercased name with a silent `projects.first()` fallback; `:206` treats any non-empty array as valid; `ui/station.js:263-268` reads projects only into a datalist.
- **Root cause** Projects are an anonymous array, so nothing can key sessions, signals, or the graph, and an unknown name silently means "project #1".
- **Fix** Create `projects` with `id/name/path/git_root/origin/path_state/archived/created`; migrate `settings.projects` and backfill `sessions.project_id` in one `user_version=1` transaction; replace `project_for` with `resolve_project(reference)` (exact id → case-insensitive name → the single project when exactly one exists; **no** `first()` fallback); validate on add/rename (`is_dir`, canonicalize, unique name/path, printable chars, ≤200 rows); `create` refuses unregistered cwds unless `allow_unregistered`; replace the compile-time default with `$HOME` + first existing candidate; serve projects from the table in `status`; add a Projects editor in Settings.
- **End state** Settings shows a Projects list with path state; "launch codex in atlantis" is refused instead of launching in project #1.
- **Why** Every later feature needs a stable key; mis-resolution is a silent wrong-directory launch.
- **Depends** none (foundation) · **Verify** `project_add` on a missing path fails and leaves the list unchanged; duplicate name/path rejected; `create` with an unregistered cwd fails while `allow_unregistered:true` succeeds and sets `project_id`; unit tests for zero/one/two matches.
- **Effort** M · **Risk** medium (settings validation, chief resolution, two dialogs).

## BUG-084 · No tasks, aspects, plans, checkpoints, or signals
- **Severity** High · **Type** schema
- **Evidence** the DB has exactly `settings` and `sessions` (`src/main.rs:105`); grep for `percent|progress|aspect|checkpoint|task` across `src/` and `station.js` returns nothing.
- **Root cause** Nothing models what "complete" means, so completion cannot be computed, shown, or argued about.
- **Fix** Add the tables and indexes above in migration v1 with `PRAGMA foreign_keys=ON`; implement `graph_score(project_id)` returning `{percent, percent_null, verified, confidence, aspects[], risks[], as_of, why}`; add RPCs `graph_get`, `aspect_add/set`, `task_add/set`, `plan_add/set`, `checkpoint_add` (writes its evidence signal atomically), `progress_claim` (assistant-only, capped); seed nothing (a new project shows `—`).
- **End state** Selecting a project shows aspect bars, task counts by state, verified vs reported, and an evidence drawer.
- **Why** This is the data plane for the requested panel; without it a percentage would be fabricated.
- **Depends** BUG-083 · **Verify** Two aspects, two tasks, one done → `percent 25.0`, `verified 25.0`, `confidence 0.5`; adding an empty aspect does not change the denominator; unit tests for decay and the claim cap.
- **Effort** M · **Risk** low.

## BUG-085 · History cannot feed a graph
- **Severity** High · **Type** persistence
- **Evidence** `src/main.rs:30` (`VecDeque<Value>`), `:169` (`next_event: 0`), `:213-220` (trim at 100), `:460-464` (`since` filter over RAM); the UI's only history is `seq=Math.max(...)`.
- **Root cause** History is a debug ring with process-local ids and no attribution.
- **Fix** Add an `events` table with `seq INTEGER PRIMARY KEY AUTOINCREMENT` (monotone across restarts), `epoch`, `project_id`, `session_id`; keep the RAM ring as a hot cache; `status` reads the table with a 500-row cap and optional project filter; add `history{project_id?,session_id?,before?,limit}` with keyset paging; attach owners at every emission site; prune >90 days or >200k rows on start.
- **End state** Restarting the daemon and the window shows real recent activity per project.
- **Why** A completion graph is a claim about time; it is only honest if events survive a restart.
- **Depends** BUG-083, BUG-084 · **Verify** Emit an event, restart, assert it returns with a larger seq; paging returns no gaps or duplicates.
- **Effort** S-M · **Risk** low.

## BUG-086 · No repository awareness (git, diffs, tests, TODO)
- **Severity** Medium · **Type** missing integration
- **Evidence** the only `git` string in `src/`/`workers/` is inside the word "digit" (`opencode_gateway.py:148`); `create_session` stops at `canonicalize` + `is_dir`.
- **Root cause** A project is a directory string; nothing knows whether work was committed, tested, or merely edited.
- **Fix** `probe_project(id)`: `git rev-parse --show-toplevel`, `--verify HEAD`, `status --porcelain=v1 -z`, `log -1 --format=%H%x00%ct`, 3 s timeouts, non-zero ⇒ no signal (never an error); write `git_root`, refresh `path_state`; emit `git_commit`/`git_dirty`/`git_clean` **only on change**; triggers: startup (staggered), create, close, and a 60 s timer for the active project; attribute commits by `navi:task=<id>` trailer or path hints, otherwise project-level freshness only; never infer "tests pass" from directories; TODO scanning may only *propose* an aspect.
- **End state** Each project shows branch, HEAD, clean/dirty, and commits today; the evidence drawer cites the commit message.
- **Why** Commits are the cheapest signal a model cannot fake.
- **Depends** BUG-083, BUG-084 · **Verify** Temp repo with one commit → one `git_commit`; a second probe adds nothing; a non-repo yields `git_root:null` and zero signals; dirty→clean yields exactly two transitions.
- **Effort** M · **Risk** low (read-only).

## BUG-087 · Harness output is invisible, so delegated progress cannot be measured
- **Severity** High · **Type** missing integration
- **Evidence** `window.py:158-165` base64s pty bytes to xterm and discards them; `src/sessions.rs:120-133` has no id capture; the gateway understands text only.
- **Root cause** Harness output exists only as pixels.
- **Fix** `harness_tail(session_id, lines=200)` via `capture-pane -p -J -S -N`; store `session_tails(session_id, project_id, hash, text, time)` and skip unchanged hashes; per-profile **versioned** parse rules producing candidate signals with the matching line as evidence; prefer structured transcript files where available; sample 5 s after each prompt, then every 10 s for 90 s, only for the active/attached session; emit nothing when the output is ambiguous; `exit != 0` marks a task `blocked` only with harness evidence.
- **End state** "12 passed" creates a verified `test_pass` signal with the line as evidence; a failing run blocks the task.
- **Why** This is the only path by which a delegated harness's real work reaches the graph.
- **Depends** BUG-075, BUG-083, BUG-084 · **Verify** `terminal` profile prints `PASSED 12` → one `test_pass`; `FAILED 3` → `test_fail` + blocked; random text → zero signals; unit tests on the parse table.
- **Effort** M-L · **Risk** medium (formats drift; "no signal when unsure").

## BUG-088 · The assistant has no project context
- **Severity** High · **Type** product behaviour
- **Evidence** `src/main.rs:21` prompt has no project; `src/chief.rs:576-586` spawns the gateway with no `current_dir`; `opencode_gateway.py:69-71` one global session; `:180,216` create with title only; `:345` prompt text only.
- **Root cause** There is exactly one project-blind conversation; the directory is never sent and the child inherits `$HOME`.
- **Fix** `assistant_sessions(project_id PK, session_id, model, agent, updated)`; create with `{title:"Navi · <name>", directory:<path>}`; set `current_dir` on the child; prepend a host-generated context block (≤600 bytes): project, path, branch@sha, aspect percentages (verified flagged), open task count, last activity; add two lines to the agent prompt ("work inside the project", "never state a completion percentage") and a `progress_claim` tool; make `oc_new` project-scoped; cache the block per (project, HEAD, graph_version) to protect the prompt cache.
- **End state** "What's left?" is answered from real aspects and tasks; edits land in the project.
- **Why** Product goal 1 and 4 both require it.
- **Depends** BUG-004, BUG-084, BUG-086 · **Verify** Fake gateway asserts directory and the context block; graph values in the block match `graph_get`; switching projects switches sessions.
- **Effort** M · **Risk** medium (block freshness vs cache).

## BUG-089 · There is no screen space for a graph panel
- **Severity** High · **Type** layout
- **Evidence** `ui/station.css:4-6` chat, `:71-72` sessions, `:78-79` terminal, `:52` toast, full-window canvas (`surface.css:14`); measured 119.6 px terminal/chat overlap; at 640 px height the only unobstructed band is ~19 px.
- **Root cause** Five surfaces claim the same viewport with independent percentage anchors; no layout reserves a region.
- **Fix** L0 grid (BUG-020) with a `--graph` column/row: ≥1280 px a 320–380 px right rail; 900–1279 px a collapsible drawer; <900 px a tab in the composer area. Panel contents: headline percentage + verified + confidence, aspect bars, risks, `as_of`, and an evidence drawer; an empty state that says "No aspects defined" and offers a setup flow. Never render 0%/100% for unknown.
- **End state** The graph lives beside the work without covering the stage, chat, or terminal.
- **Why** The panel is a product goal; today it would repeat the overlap problem.
- **Depends** BUG-020, BUG-084 · **Verify** V-PNG at three widths with the panel open: zero intersections, readable bars, `—` when empty.
- **Effort** M · **Risk** medium.

## BUG-090 · No project switcher and no project-scoped new session
- **Severity** Medium · **Type** UX
- **Evidence** `ui/station.js:263-268` projects appear only as a datalist in the new-session dialog; no switcher; `settings.projects[0]` is the default cwd; the chief's resolver falls back to the first project.
- **Root cause** "Current project" is implicit and singular.
- **Fix** Add a project switcher in the strip (name + path state) that sets `active_project_id` in state (not settings); scope the new-session dialog, assistant session, graph, and voice context to it; show the active project in the status line; keyboard shortcut `Ctrl+1..9`.
- **End state** One click changes what "this project" means everywhere, visibly.
- **Why** Multi-project work is the normal case; implicit defaults are how wrong-directory edits happen.
- **Depends** BUG-083, BUG-088 · **Verify** Switch projects: new-session default cwd changes, the graph changes, the assistant session changes, and the status line updates.
- **Effort** M · **Risk** low.

## BUG-091 · Nothing durable records conversations per project
- **Severity** High · **Type** data
- **Evidence** `ui/station.js:25` caps the DOM at 200 rows and persists nothing; `src/main.rs:105` has no messages table; a WebProcess restart recreates the view (`window.py:62-67`); the events ring is 100 entries.
- **Root cause** The transcript is a DOM artifact of a volatile ring.
- **Fix** `messages(id, project_id, session_id, role, parts_json, time, turn)`; write user and assistant messages when the turn opens/closes (idempotent by turn id); hydrate the last 200 on boot; `history` pages older turns; virtualise the DOM; include messages in the graph's evidence drawer. Share the schema and migration with BUG-047/085.
- **End state** Restarting keeps the conversation and lets the graph cite what was said.
- **Why** Without durable turns, neither the chat nor the graph can be trusted across a restart.
- **Depends** BUG-083, BUG-085 · **Verify** Submit, restart daemon and window, assert rows return in order with parts; no duplicates on re-delivery.
- **Effort** M · **Risk** medium.

## BUG-092 · Changing projects/profiles silently cancels the in-flight turn (and flushes browser actions)
- **Severity** High · **Type** bug
- **Evidence** `src/main.rs:320-326` bumps `generation` and clears `browser_queue` on any change to `model|profiles|projects|favorites`; `src/chief.rs:437-441` and `:158` kill the running child when `generation` changes; the settings dialog rebuilds `profiles` on every save (`ui/station.js:327-347,359-373`).
- **Root cause** One counter serves both "user cancelled" and "configuration changed", and the UI rewrites profiles even when only an unrelated toggle changed.
- **Fix** Split `cancel_generation` (Stop only) from `config_version`; configuration changes never abort a turn — they apply to the next one; the chief checks only `cancel_generation`; make settings save diff-based (send only changed sections, or have the UI preserve ids/arrays verbatim per BUG-077/078); keep `browser_queue` clearing only for changes that actually invalidate it (profiles/projects, not the local model).
- **End state** Toggling "speak" or adding a project during a long answer does not kill it.
- **Why** Silent cancellation looks like a crash and loses work.
- **Depends** BUG-078, BUG-079 · **Verify** Start a slow fake turn, toggle a setting, assert the turn completes and only the next turn uses the new config; Stop still aborts.
- **Effort** S-M · **Risk** medium (touches every generation check).

---

### Migration notes (one `user_version=1` transaction)
1. Create `projects`, `aspects`, `tasks`, `plans`, `signals`, `checkpoints`, `events`, `messages`, `assistant_sessions` with foreign keys.
2. Seed projects from the legacy settings array (dedupe by canonical path, keep the first name, `origin='settings_migration'`, mark missing paths).
3. `ALTER TABLE sessions ADD COLUMN project_id TEXT REFERENCES projects(id)` and backfill by canonical cwd; unresolvable paths keep NULL plus a project row with `path_state='missing'`.
4. **Required in the same commit:** `INSERT INTO sessions VALUES(?1..?5)` (`src/sessions.rs:154`) must become an explicit column list or it fails after the new column.
5. Remove `projects` from the settings validation and from the generation gate; `status.projects` is now table-derived so the UI keeps working.
6. Rollback on any error, emit `error`, serve degraded with the legacy blob, and hide the workbench until `migration_state='done'`. Add `project_migrate {dry_run:true}` for tests.

### Honesty rules (must hold in every implementation)
- No stored percentage; recompute from signals.
- Evidence is mandatory and non-empty.
- Estimates stay unverified and fade with a 10-day half-life; newer verified task evidence overrules a part's estimate.
- Unknown is `—`, never 0 or 100.
- Verified and confidence are always displayed beside the percentage.
- User verdict is terminal until the user changes it.
- Attribution comes from the session's `project_id`, never from words in the utterance.
- Every number exposes its signal ids; if it cannot be explained, it is a bug.
