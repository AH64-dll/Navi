# Section 5 — Harness sessions, resume, and adapters

Scope: `src/sessions.rs`, `src/main.rs` (create/prompt/select), `window.py` (pty attach), `ui/station.js` (sessions UI), `scripts/install.py`, `config/`.

**Adapter contract (new `src/adapters.rs`, no new crates).** `discover(path_env)`, `launch(cwd, name, resume, env) -> LaunchSpec{argv, env, cwd, preseed_id}`, `capture(home, ctx) -> Option<native_id>`, `status(home, row)`, `output(home, row, since)`, `interrupt_keys()`, `capabilities()`. Universal fallback that exists before any adapter work: `tmux pipe-pane -o -t <t> 'cat >> $DATA/sessions/<id>.log'`, `capture-pane -p -S -400`, and `list-panes -a -F`. Everything harness-specific is an enhancement over that.

**Verified harness capabilities (local machine).** Claude Code: `--session-id <uuid>` preseed (documented), `--resume <id|name|path>`, `--continue`, `-n <name>`, `--fork-session`; registry at `~/.claude/sessions/<pid>.json` (sessionId/cwd/pid/status/name); transcripts under `~/.claude/projects/<slug>/*.jsonl`. Codex: `codex resume <id|name>`, `--last`, `-C <cwd>`; no documented preseed flag; id must be scraped from `~/.codex/sessions/**/rollout-*.jsonl` line 1 (`payload.session_id`, `payload.cwd`); `~/.codex/session_index.jsonl` lists id/thread_name/updated_at. Antigravity: `agy --conversation <id>`, `-c/--continue`; conversation id approximated from `~/.gemini/antigravity-cli/cache/last_conversations.json[cwd]` or a new `conversations/<uuid>.db`; IDE and CLI have separate stores (`~/.gemini/antigravity` vs `~/.gemini/antigravity-cli`). Unknowns are marked as unknown in the plans and must be probed, not assumed.

---

## BUG-069 · No harness session identity, so nothing can be resumed
- **Severity** Critical · **Type** missing
- **Evidence** `src/sessions.rs:124` (`format!("navi-{}", id())` is the only id); `:152-163` inserts five columns; `src/main.rs:105` schema; grep for `--resume|resume|session-id|rollout` in `src/` returns only intent vocabulary.
- **Root cause** tmux's name was mistaken for the harness's conversation identity.
- **Fix** (1) Generate a native id before launch where preseed is documented (Claude `--session-id <uuid>`). (2) Add `adapters.rs` with `capture()`: preseed → hook/registry → store diff (snapshot before launch, poll ≤10 s for a new file whose cwd matches). (3) Persist `harness`, `native_id`, `native_name`, `launch_kind`, `gen` (BUG-079). (4) Add RPC `session_resume {id|harness+native_id|name, cwd?, prompt?}` that builds a `Resume` spec and launches a new tmux session reusing the native id. (5) Extend `create` with `resume:"new"|"last"|"<id>"`; `"last"` maps to `--continue` / `codex resume --last` / `agy -c`. (6) Add a `resume` action kind to the chief decision beside `launch`, and include it in `routes_to_conversation` so "continue X" executes. (7) UI: chip menu "Continue in this harness".
- **End state** "Continue the auth refactor in Claude" opens a new chip with the full prior conversation; the title shows `claude · <short id>`.
- **Why** Without identity, product goal 3 is unreachable.
- **Depends** BUG-070, BUG-079 · **Verify** Isolated fixture harness (a shell stub that echoes `--session-id` and writes a fake registry entry under a temp `HOME`): assert `create` stores the preseeded UUID; `session_resume` reuses it; `tmux display-message -p '#{pane_start_command}'` contains `--resume <uuid>`.
- **Effort** 3–4 d · **Risk** Claude is exact; Codex/Antigravity capture is best-effort.

## BUG-070 · A profile cannot carry launch options, and pane env is not usable
- **Severity** High · **Type** missing + latent incorrectness
- **Evidence** `src/main.rs:115-119` profiles are `{id,name,executable,args}`; `:307-318` validation accepts nothing else; `src/sessions.rs:132-134` appends only args; `:135` `env_remove` mutates the **tmux client**, not the pane; tmux panes inherit the server env, started by systemd with no `Environment=` (`scripts/install.py:56`).
- **Root cause** Two gaps with one symptom: no schema, and env cannot be set through the client.
- **Fix** Profile v2 `{id,name,harness,executable,args:[],env:{},cwd:"inherit"|"project",resume_default:"ask"|"new"|"last",size:{cols,rows},interrupt_keys:[]}`; launch through `/usr/bin/env -i HOME=… PATH=<computed> TERM=tmux-256color COLORTERM=truecolor XDG_RUNTIME_DIR=… <profile env> <exe> <args>`; `new-session -x -y` from `size`; validate env keys (`^[A-Z_][A-Z0-9_]*$`), reject `TMUX*`, never store or log secret-looking values. **Base env must be composed from the user manager, not the systemd unit**: merge `systemctl --user show-environment` (graphical session vars: `DISPLAY`, `WAYLAND_DISPLAY`, `DBUS_SESSION_BUS_ADDRESS`, `SSH_AUTH_SOCK`, language) with a 2 s timeout, then `tmux show-environment -g`, then profile env (highest precedence). Never bake the env at install time; refresh the snapshot on daemon start and on `settings` save.
- **End state** "Claude (opus, no telemetry)" launches with exactly that env, argv, and geometry; `ps`/`tmux show-environment` confirm it.
- **Why** Per-project, per-harness configuration is the practical meaning of context.
- **Depends** none (foundation for 069/071/077) · **Verify** Stub harness prints `env` and `"$@"` to stdout; capture the pane and assert exact env/argv and `window_width x window_height`.
- **Effort** 2 d · **Risk** dropping a var a harness needs — snapshot the base env and test the allowlist.

## BUG-071 · Harnesses are hand-typed strings; nothing is discovered
- **Severity** High · **Type** missing
- **Evidence** `src/main.rs:115-119` four hardcoded profiles; `ui/station.js:310-320,327-341` free-text form; `config/` has only `personas.json`; no `which`/`--version` probe anywhere in `src/`.
- **Root cause** Configuration is the only source of truth and it is never reconciled with the machine.
- **Fix** `discover_all(path_env)` over `$HOME/.local/bin`, `/usr/local/bin`, `~/.npm-global/bin`, `/usr/bin`, `/bin`, plus `$SHELL -lc 'command -v claude codex agy pi'`; run `--version` (3 s, null stdio); cross-check harness homes; RPC `harnesses` returns `{found,path,version,in_navi_path,capabilities,profile_exists}`; UI picker prefills a profile; emit a `harness` event when a discovered harness has no profile; cache 60 s.
- **End state** Installing Antigravity CLI makes it appear in "Add harness" with its version, one click to add.
- **Why** Manual configuration gates every other harness feature.
- **Depends** BUG-070, BUG-072 · **Verify** Fixture executables in a temp `HOME` and PATH; assert found/version/`in_navi_path` classification and that a path outside navi's PATH is flagged.
- **Effort** 1.5 d · **Risk** low.

## BUG-072 · Executables are resolved with `is_file()`, against a PATH frozen at install
- **Severity** High · **Type** correctness
- **Evidence** `src/sessions.rs:115` (`.find(|p| p.is_file())`); `:107-109` canonicalize accepts directories; `scripts/install.py:57` bakes `Environment=PATH=…` once at install; default profiles use bare names.
- **Root cause** A file is not an executable, and the daemon's search path is whatever shell ran the installer.
- **Fix** Require `is_file() && mode & 0o111` for the effective uid; distinguish `NotFound|NotExecutable|IsDirectory`; resolve through a computed launch PATH (`NAVI_SYSTEM_PATH` + `NAVI_USER_PATH_DEFAULT` + `$SHELL -lc 'printf %s "$PATH"'` with a 5 s timeout) cached 60 s; replace the frozen `Environment=PATH` with those two vars; startup self-check per profile emits a named error; include the searched path in the error text.
- **End state** A harness installed after setup is found; a non-executable file yields "found but not executable", never a raw tmux error.
- **Why** "Installed but navi says it isn't" is the first failure of the whole feature.
- **Depends** BUG-071 · **Verify** Four fixtures (executable, `0644` file, directory, absent) → four distinct errors; (b)/(c) never reach `new-session`.
- **Effort** 1 d · **Risk** low.

## BUG-073 · Dead sessions are permanent: no kill, restart, removal, or exit detection
- **Severity** Medium · **Type** lifecycle
- **Evidence** `src/sessions.rs:18-45` running = name appears in `list-sessions`; `:47-57` refuses anything not running, so `select` fails on a dead row; `ui/station.js:236-252` renders dead chips with the same click handler; `ui/station.js:210` bails; grep `kill-session|remain-on-exit` → zero.
- **Root cause** The DB row is immortal history and tmux is the only liveness oracle; no deliberate transition or removal exists.
- **Fix** Per-session `remain-on-exit on`; on death capture `capture-pane -p -S -400` into `<data>/sessions/<id>.log` and record `exited`/`exit_status`; one `list-panes -a -F` call replaces two subprocesses; RPCs `session_kill`, `session_remove` (soft delete), `session_restart` (new id, `replaced_by`, resume when a native id exists); reconcile at boot and each poll; chip shows "exited 2m ago", `×`, `↻`, and `■` for live.
- **End state** A closed harness leaves a readable log; restart resumes; remove hides it from the strip but keeps the transcript.
- **Why** The strip must model reality or users stop trusting it.
- **Depends** BUG-069, BUG-075, BUG-079 · **Verify** `terminal` profile: send `exit`; assert `state=exited`, `exit_status` present, log non-empty; restart yields a new id with `replaced_by`; remove hides it.
- **Effort** 2 d · **Risk** low.

## BUG-074 · Sessions started outside navi are invisible
- **Severity** Medium-High · **Type** observability
- **Evidence** `src/sessions.rs:18-45` reads only the DB plus the private tmux socket; no reader for `~/.claude/sessions/*.json`, `~/.codex/session_index.jsonl`, or Antigravity's `conversation_summaries.db` (all verified present).
- **Root cause** "Sessions" is defined as rows navi wrote.
- **Fix** `session_discover` (and `status?external=1`) returning `origin:"navi"|"external"` with `{harness,native_id,name,cwd,updated,state}`; bounded readers (50 rows, 200 ms, 5 s cache) per harness; actions: "Continue in navi" (resume into a managed pane), "Reveal transcript"; deduplicate by `(harness,native_id)` with the navi row winning; render a dimmer "also running elsewhere" group with no attach affordance.
- **End state** Work started in your own terminal can be continued inside navi.
- **Why** Turns navi into the place where all harness work is visible.
- **Depends** BUG-069, BUG-079 · **Verify** Seed a temp `HOME` with fixtures for all three harnesses; assert exact rows, cwd, state, and no duplicate for a navi-owned session.
- **Effort** 2.5 d · **Risk** medium (undocumented formats; tolerate parse failures).

## BUG-075 · No output read path outside the WebKit pane
- **Severity** Critical · **Type** observability
- **Evidence** `window.py:158-169` forwards pty bytes to JS and discards them; the pty exists only while attached; `ui/station.js:209-218` attaches only on open; grep `capture-pane|pipe-pane` → zero; `launched` is the only session event.
- **Root cause** Output was designed as a display feature, not as daemon state.
- **Fix** (1) `pipe-pane -o 'cat >> <data>/sessions/<id>.log'` at create. (2) `capture-pane` on exit/on demand, appended with a `\0SCREENSHOT` marker. (3) RPC `session_output {id,since,limit}` tailing the log, ANSI-stripped. (4) Derive `state` from `list-panes` (`pane_dead`, `pane_current_command`, `session_activity`) plus adapter rules for `needs-input`. (5) `status` includes a bounded `active_session_tail` (12 lines, 1 KiB) so "what is Claude asking?" is answerable; emit transitions at ≤1/s. (6) Rotate at 8 MiB × 3.
- **End state** The assistant and the graph can see what a harness said; the pane shows the same content.
- **Why** This is the substrate for status, approvals, summaries, and any completion metric.
- **Depends** BUG-073, BUG-079 · **Verify** `terminal` profile: send `printf 'READY\n'`; `session_output` returns it; kill the pane and assert the screenshot marker; log is `0600`.
- **Effort** 3 d · **Risk** medium (ANSI stripping, growth).

## BUG-076 · Prompt dispatch is a blind keystroke guess
- **Severity** High · **Type** reliability/data integrity
- **Evidence** `src/sessions.rs:60-62` (2000-byte cap only here); `:63` prepends a space to the prompt itself; `:64-79` literal send-keys then a separate Enter; `:167-192` a second unvalidated copy with `sleep(1500)`; `ui/station.js:228-233` single-line input.
- **Root cause** The tty is treated as a line queue: no readiness, no paste protocol, no single implementation.
- **Fix** One `inject(target,text,opts)` used by prompt/create/resume; transport via `set-buffer` + `paste-buffer -p -d` (bracketed paste), then Enter after 120 ms; readiness predicate from `capture-pane` (adapter prompt glyphs) polled at 100 ms up to 8 s, replacing the sleep; 32 KiB cap; normalise CRLF; reject control chars except `\n\t`; prefer structured channels (`codex queue`, `agy -p --input-format stream-json`) when `caps.structured_prompt`; log the exact text and outcome in `session_events`.
- **End state** A 1.4 KB multi-paragraph prompt with a leading `-` arrives once, intact, in any harness, including one still booting.
- **Why** Removes the highest-frequency silent failure and is a prerequisite for voice-driven prompting.
- **Depends** BUG-070, BUG-075 · **Verify** `terminal` profile: 5000-byte payload → byte count matches; 3 newlines → one command; leading `-x` → literal; stub with a 2 s boot banner → waits and succeeds.
- **Effort** 2 d · **Risk** low-medium.

## BUG-077 · Arguments containing spaces cannot be expressed and are destroyed on save
- **Severity** Medium · **Type** data loss
- **Evidence** `ui/station.js:315-317` one text input; `:333-338` `args.split(/\s+/)`; `:372` runs on every settings save; backend already accepts arrays.
- **Root cause** A JSON array is round-tripped through a whitespace-joined string.
- **Fix** Repeatable arg editor (one input per arg, add/remove, quote-aware paste parsed once); save arrays verbatim; `profiles_rev:2` migration; per-profile `args_mode:"argv"|"shell"`; backend rejects NUL and >4096-byte args; log argv at launch.
- **End state** `["--add-dir","/home/me/My Projects/demo"]` survives unrelated saves.
- **Why** Real project paths contain spaces (verified in local harness state).
- **Depends** BUG-070 · **Verify** GUI workflow saves twice with an unrelated toggle; assert the args array is byte-identical, including a space and an empty string.
- **Effort** 1 d · **Risk** low.

## BUG-078 · Renaming a profile rewrites its id and orphans sessions
- **Severity** High · **Type** data integrity
- **Evidence** `ui/station.js:334-338` derives `id` from the display name at save time; `:372` runs on every save; `src/main.rs:307-318` never checks id presence/uniqueness; `src/sessions.rs:88-91` matches by stored id; `:151-157` stores that id.
- **Root cause** Ids are a function of a mutable field.
- **Fix** Generate `id` once (`<slug>-<4hex>`), carry it in a `data-profile-id`, never recompute; validate uniqueness and format; reject disappearing ids unless `allow_profile_removal:true`, then mark affected sessions `orphan`; rename only `name`; migrate existing orphans by executable basename with a "re-link" action.
- **End state** Renaming "Claude Code" keeps every existing chip working and resumable.
- **Why** Losing the profile link destroys the ability to resume.
- **Depends** BUG-069, BUG-079 · **Verify** Rename via the dialog; assert profile id unchanged, session resolves, and a second unrelated save changes nothing; unit test duplicate-id rejection.
- **Effort** 1 d · **Risk** low.

## BUG-079 · A session records five fields and nothing about what it ran
- **Severity** High · **Type** schema/observability
- **Evidence** `src/main.rs:105` schema; `src/sessions.rs:152-163` writes exactly those; `ui/station.js:239-245` renders name/profile/cwd/running only.
- **Root cause** The schema was sized for a launcher, not an orchestrator.
- **Fix** Versioned migration (`PRAGMA user_version`, additive `ADD COLUMN` checked via `table_info`): `harness`, `native_id`, `native_name`, `argv` JSON, `env_keys` JSON (**keys only**), `launch_kind`, `gen`, `state`, `pid`, `last_active`, `exited`, `exit_status`, `origin`, `orphan`, `closed_at`, `replaced_by`, `harness_version`. Add `session_events(id INTEGER PK, session, time, kind, payload)` with kinds `created|launch_argv|native_captured|prompt|ready|busy|idle|needs_input|attach|detach|exited|removed`. Write the row before starting tmux (`state='starting'`) and update after. Store the first/last prompt (≤2 KiB) for intent matching. Never store env values; redact secret-looking argv.
- **End state** `sqlite3 … "select harness,native_id,launch_kind,state,exit_status,argv from sessions"` explains exactly what navi did.
- **Why** Every other plan depends on this record.
- **Depends** none (land first) · **Verify** Fresh data dir: schema version and columns present; create → expected `session_events` sequence; old-schema DB migrates with rows preserved.
- **Effort** 1.5 d · **Risk** low (note: `INSERT INTO sessions VALUES` must gain a column list).

## BUG-080 · `create` reports `running:true` without observing it
- **Severity** Medium · **Type** correctness
- **Evidence** `src/sessions.rs:165-166` hardcodes `"running":true`; `:136-142` checks only that `new-session` returned 0; `ui/station.js:281-283` opens the terminal immediately.
- **Root cause** Intent is returned as observation; tmux accepting a command proves only that a pane was forked.
- **Fix** Return `state:"starting"` and compute `running` from a post-launch `has-session`/`list-panes` check; settle for up to 1.2 s; if dead, return a structured error with the captured last screen and exit status, and mark the row `exited`; add `warnings[]`; the UI renders attach failures inside the pane, not as a global toast.
- **End state** A misconfigured harness yields "Codex exited immediately: <its own words>" with the screen, not a phantom success.
- **Why** Turns the most confusing failure of the feature into a self-explaining one.
- **Depends** BUG-073, BUG-079 · **Verify** Stub executable that exits 3 → `ok:false` or `running:false` with a non-empty screen; no running row on the next status.
- **Effort** 1 d · **Risk** low.

## BUG-081 · Every launch silently rewrites the voice target
- **Severity** Medium · **Type** state/consent
- **Evidence** `src/sessions.rs:164` calls `set_active` on every create; `src/main.rs:371-377` persists `active_session` and emits `session`; the consumer is `workers/voice.py:86-88` (ASR keyword ordering); `grep active_session src/chief.rs` is empty.
- **Root cause** "Which session am I talking to" and "which did I just open" share one field, written unconditionally.
- **Fix** Split `active_session` (explicit voice target) from `last_launched`; create sets only the latter; add `session_target {id}` as the only writer of `active_session`; the chief reports "launched X; voice target stays Y"; add a target badge; keep the voice sort on the genuine target.
- **End state** Opening a harness does not change what your voice addresses until you say so.
- **Why** A silent context switch in the one surface with no visual confirmation.
- **Depends** BUG-079 · **Verify** Create → `active_session` unchanged, `last_launched` set; `session_target` changes it and emits exactly one event; `test_voice_policy` still orders the explicit target first.
- **Effort** 0.5 d · **Risk** low.

## BUG-082 · Stop cannot reach a harness
- **Severity** Medium-High · **Type** missing control
- **Evidence** `src/main.rs:529-537` cancels generation, clears the browser queue, and interrupts OpenCode only; no `session_interrupt` op; `grep send-keys src/` shows only the two prompt injectors; the terminal pane has Send and Close, no Stop.
- **Root cause** `cancel` predates harness prompting and was never extended.
- **Fix** `session_interrupt {id,hard?}` using adapter `interrupt_keys` (Escape for a turn, Ctrl-C for hard), with structured control channels where documented; `cancel` becomes state-aware (interrupt the busy harness *and* the gateway); terminal head and busy chips gain a Stop control; emit `stopped {scope:"harness",id}`; `hard` offers `kill-session` after 3 s.
- **End state** "Stop Claude" returns it to its prompt; the chip leaves `busy`; Navi confirms audibly.
- **Why** Long autonomous turns without a brake are unacceptable.
- **Depends** BUG-075, BUG-079 · **Verify** `terminal` profile running `sleep 30`: prompt → `state=busy`; interrupt → `pane_current_command` back to `bash` in ≤1 s; `hard` → `exited`.
- **Effort** 1.5 d · **Risk** medium (key semantics differ per TUI; adapter-owned).

---

### Corrections applied during review
- Antigravity's CLI binary is `agy`; the IDE and CLI have separate stores. Conversation-id capture for the CLI is an approximation and is marked as such.
- Codex has no documented preseed flag; its id must be scraped from rollout JSONL line 1.
- Claude's `--session-id` preseed makes its path exact and is the recommended first adapter.
- `env_remove` on the tmux client cannot affect a pane; the fix must go through `env -i` inside the pane command.
- `active_session` is consumed only by the voice keyword sort, not by the chief (corrected from the original bug statement).
- Dead-session detection needs `remain-on-exit` to be observable at all.
