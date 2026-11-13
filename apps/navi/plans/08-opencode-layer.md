# Section 8 — Navi as a layer over OpenCode and other harnesses

Written 2026-11-13 against OpenCode 2.2.5 (latest 2.2.7). Every API fact below was read from the
live server's `/openapi.json` or probed on it; nothing is guessed. Plans 001–100 in this folder are
older and assume guessed endpoints; where they conflict with this file, this file wins.

## The problem

The user's words: Navi "feels unfinished", "just a link to OpenCode", and "I can't make a new
session or check settings from OpenCode". It should be a layer over OpenCode that keeps working
when OpenCode updates, and it should drive other harnesses the way T3 Code does.

What the code does today:

- `workers/opencode_gateway.py` is started as a new Python process for every turn and every
  catalog read (`src/chief.rs:1016` `stream_gateway`, `src/chief.rs:1213` `gateway_op`). It holds
  one session per project directory (`$NAVI_DATA_DIR/opencode-session`). Nothing is listening
  between turns, so Navi never sees a session, model or setting change.
- The UI can do: send a prompt, `oc_new` (new session for the current project), `oc_models`,
  `oc_agents`, `oc_commands`, `oc_interrupt`, `permission_reply` (`src/main.rs:1567-1584`).
  Everything else OpenCode offers (169 operations) is invisible.
- Harnesses (Claude, Codex, Antigravity, Pi) run as tmux panes (`src/adapters.rs`,
  `src/sessions.rs`). Navi types into them and scrapes the screen. There is no structured stream.

## Already done (do not redo)

| Commit | What |
|---|---|
| `cd09f5607` | Voice: capture starts at the word the name was said in; accented spellings of "Navi"; words after calling over Navi's reply are kept; a captured but unsent utterance resumes; "Navi, stop" is never held; cloud STT timeout grows with audio length; `[BLANK_AUDIO]` is not a command. 190 speech tests pass. |
| `604ec2b8e` | Gateway speaks the real 2.x API: `location.directory` (sessions ran in `$HOME` before), `files` with `file://` URIs (attachments were silently dropped), `permission.asked` + `/permission/{id}/reply` with `once/always/reject`, question forms (`form.created`) emitted and answerable with the new `form` mode. |
| `604ec2b8e` | `workers/opencode_contract.py` + `config/opencode-contract.json`: the operations Navi uses, pinned from the server's spec. The fake server in `scripts/test_opencode_gateway.py` returns 400 for any body the spec does not accept, and every test's tearDown fails on a violation. |

## Rules for whoever executes this

1. **Never guess an OpenCode endpoint, field or event name.** The server drops unknown body fields
   and answers 200; a guess fails silently (that is how the three bugs above survived for months).
   To use a new operation: find it in the live spec (recipe below), add it to `OPERATIONS` in
   `workers/opencode_contract.py`, run `python workers/opencode_contract.py snapshot`, and only then
   write code. The tests will reject a wrong body.
2. Read the spec like this (password is in `~/.config/opencode/service.json`):
   ```bash
   python3 -c 'import json,base64,urllib.request,pathlib; p=json.loads((pathlib.Path.home()/".config/opencode/service.json").read_text())["password"]; r=urllib.request.Request("http://127.0.0.1:49374/openapi.json",headers={"Authorization":"Basic "+base64.b64encode(f"opencode:{p}".encode()).decode()}); open("/tmp/oc-spec.json","w").write(urllib.request.urlopen(r).read().decode())'
   ```
   Then look up `paths` by operationId and resolve `$ref`s under `components.schemas`.
3. Event payloads are not typed in the spec; only their names are. Before handling a new event,
   capture one real instance from `GET /api/event` and put that exact shape in a test.
4. Treat `/api/experimental/*` as optional: check the operation exists (contract `problems()`), and
   hide the feature when it does not.
5. Tests: `NAVI_ISOLATED=1 ~/.local/share/navi/venv/bin/python scripts/<suite>.py`. Daemon suites
   need `NAVI_TEST_BINARY` pointing at a built `navid` (`/tmp/navi-cargo-target/debug/navid` after
   `cargo build`, or `~/.local/share/navi/bin/navid` when Rust is unchanged). UI chat changes need
   `scripts/test_gui_isolated.py --chat`, not `--workflow`.
6. Probing the live server creates real sessions. Delete every probe session
   (`DELETE /api/session/{id}`) before finishing.
7. Rust: `cargo build` and `cargo test` from `apps/navi`. Python: `py_compile` plus the suites.

---

## Phase 0 — Land the finished work (user, 5 minutes)

The session that wrote this was sandboxed to its worktree and could not move `main` or the live
install. Run:

```bash
git -C ~/Documents/navi merge --ff-only worktree-navi-voice-layer
git -C ~/.local/share/navi/app checkout --detach main
systemctl --user restart navi-station   # closes the window; reopen with ~/.local/bin/navi-station
```

No Rust changed, so `navid` does not need reinstalling. The fast-forward is safe with the
uncommitted edits in `~/Documents/navi`: none of them touch the files these commits change.

**Voice check, by ear:** say "Navi, what time is it in Tokyo and remind me to call my brother", one
breath, then pause. The whole sentence must arrive. While Navi is answering, say "Navi, stop": it
must stop within about 2 s. While Navi is answering, say "Navi, open the browser": the new command
must run. Report any failure with `journalctl --user -u navi-station --since -5min | grep -i voice`.

---

## Phase 1 — Sessions: list, switch, create, rename, archive, fork, delete

**Goal:** a session sidebar for the current project that shows exactly what `opencode` shows, and
lets the user do everything the OpenCode TUI can do with sessions.

**Verified API** (all under Basic auth, user `opencode`):

| Action | Operation | Request |
|---|---|---|
| List | `session.list` `GET /api/session` | query `limit`, `order`, `search`, `parentID`, `archived`, `directory`, `cursor` |
| Running now | `session.active` `GET /api/session/active` | `{data: {"ses_…": {type:"running"}}}` |
| Create | `session.create` `POST /api/session` | `{title?, agent?, model?:{providerID,id,variant?}, location?:{directory}, parentID?}` |
| Get | `session.get` | returns `Session.Info`: `id, title, location.directory, agent, model, cost, tokens, outcome, time.{created,updated,busy,idle}` |
| Archive / unarchive, metadata | `session.update` `PATCH /api/session/{id}` | `{archived?, metadata?, permissions?}`; **there is no title field** |
| Fork | `session.fork` `POST /api/session/{id}/fork` | `{before?: "msg_…"}` |
| Delete | `session.remove` `DELETE /api/session/{id}` | none |
| Messages | `session.message.list` | query `type`, `order`, `limit` |
| Compact | `session.compact` | `{delivery?: "steer"|"queue"}` |
| Undo | `session.revert.stage` / `.commit` / `.clear` | `{messageID, files?}` |

Rename: the `session.renamed` event exists but `session.update` has no title. Before building
rename, find how the server renames (search the spec for `title`; try `metadata.title` on a probe
session and read it back with `session.get`). If there is no way, ship without rename and say so.

**Steps**

1. Gateway: add `sessions`, `session_get`, `session_fork`, `session_archive`, `session_delete`,
   `session_compact` modes to `workers/opencode_gateway.py` `main()`. Add each operation to
   `OPERATIONS`, re-snapshot, add one test per mode in `scripts/test_opencode_gateway.py` (extend
   `FakeOpenCode` routes; the contract check is automatic).
2. Gateway: add `--session <id>` to `ask`/`slash` so a turn can target a chosen session instead of
   the stored per-directory one. When given, store it as the directory's current session.
3. Rust: add RPCs `oc_sessions {directory?, search?, archived?}`, `oc_session_select {id}`,
   `oc_session_fork {id, before?}`, `oc_session_archive {id, archived}`, `oc_session_delete {id}`
   next to `oc_new` in `src/main.rs:1567`. Each calls `gateway_op`. `oc_session_select` updates
   `assistant_sessions` (`src/store.rs:97`) for the project.
4. UI (`ui/station.js`): a session list in the assistant panel: title, relative time, cost,
   a running dot from `session.active`. Click selects and loads messages; a menu gives fork,
   archive, delete (delete asks first). Reuse the existing chat message renderer.
5. Tests: gateway unit tests; one daemon test in `scripts/test_assistant.py` for select + ask
   targets the selected session; a `--chat` GUI assertion that the list renders and selecting
   switches the visible conversation.

**Done when** a session created in a terminal `opencode` in the same project shows up in Navi
after a refresh, and continuing it from Navi appends to the same conversation in the terminal.
**Effort** 2–3 d. **Risk** low.

---

## Phase 2 — Settings that come from OpenCode, not from Navi

**Goal:** Navi's settings page shows OpenCode's real configuration and lists, so new agents,
commands, models, skills and MCP servers appear without a Navi release.

**Verified API**

| Panel | Read | Write |
|---|---|---|
| Models | `model.list` `GET /api/model`, `model.default` | per session: `session.switchModel` `{model:{providerID,id,variant?}}` |
| Agents | `agent.list`, `agent.get` | per session: `session.switchAgent` `{agent}` |
| Providers / accounts | `provider.list`, `credential.list`, `integration.list` | `credential.activate`, `integration.oauth.connect` / `.status` / `.complete`, `integration.connect.key` |
| Commands, skills | `command.list`, `skill.list` | none |
| MCP servers | `mcp.list` | experimental: `experimental.mcp.add` `PUT /api/experimental/mcp/{server}` `{config:{type:"local",command:[…],environment?}}`, `.remove`, `.connect`, `.disconnect` |
| Saved approvals | `permission.saved.list` | `permission.saved.update`, `permission.saved.remove` |
| Config | `config.get` `GET /api/config` | `experimental.config.update` accepts only `{shell}` today |
| Plugins | `plugin.list` | `plugin.update` |
| Usage | `experimental.usage.limits`, `experimental.session.stats` | none |

Change events to refresh on: `model.updated`, `agent.updated`, `command.updated`, `skill.updated`,
`config.updated`, `provider.updated`, `credential.updated`, `integration.updated`,
`mcp.status.changed`, `plugin.updated`.

**Steps**

1. Gateway: one generic read mode, `catalog <name>`, where `<name>` is one of a fixed allowlist
   mapping to the read operations above. Do not add one mode per list.
2. Rust: `oc_catalog {name}` RPC. Cache each result for 30 s; Phase 4 replaces the timer with
   events.
3. UI: an "OpenCode" settings tab with sub-sections in the order of the table. Read-only first.
   Then add the writes, one per PR: model and agent per session, MCP add/remove, saved approval
   removal, provider connect (OAuth opens the returned URL in the default browser).
4. Drop Navi's own duplicate model/agent lists where they exist; the OpenCode lists are the source.

**Done when** a new agent file in `~/.config/opencode/agents/` shows up in Navi's agent picker
without restarting Navi, and an MCP server added in Navi appears in
`opencode mcp list`. **Effort** 3 d. **Risk** medium: experimental writes can change shape, so
gate each one on `problems()` being empty for its operation.

---

## Phase 3 — Approvals and questions in the chat

**Goal:** when OpenCode asks for approval or asks a question, the turn waits and the user answers
in Navi by click or by voice.

**State today** (after `604ec2b8e`): the gateway emits
`{"type":"permission","id","session","tool","input":{"resources","metadata"},"message"}` and
`{"type":"form","id","session","title","fields","metadata"}` / `{"type":"form_settled","id","outcome"}`.
`chief.rs:4145` maps `permission` to the existing approval card (`ui/station.js:1381`). `form` lines
are dropped by `map_gateway_line` (unknown type). The Navi agent file written at `src/main.rs:911`
has `permission: question: deny`, which was a workaround for unanswerable questions.

**Verified API:** `session.permission.reply` body `{decision:"once"|"always"|"reject", message?}`;
`session.form.reply` body `{answer:{<field key>: string|number|boolean|string[]}}`;
`session.form.cancel` `DELETE`. `Form.Field` kinds: `string` (may have `options`, `format`,
`placeholder`), `number`, `integer`, `boolean`, `multiselect` (`options`, `minItems`, `maxItems`),
`external` (`url`, open it). Fields may carry `when: [{key, op:"eq"|"neq", value}]` (show only
when another field matches) and `required`.

**Steps**

1. Rust: add `GatewayLine::Form` and `GatewayLine::FormSettled` in `chief.rs` beside
   `GatewayLine::Permission`; forward them as chat events with the session id.
2. Rust: `permission_reply` accepts `always` and an optional `session`; pass `--session` through.
   Add `form_reply {id, session, answer}` and `form_cancel` RPCs.
3. UI: an "Always allow" button on the approval card. A form card that renders each field kind,
   honours `when` and `required`, and submits once.
4. Gateway: while a form or permission is pending, the turn is waiting for the user, not silent:
   `_tool_running()` (`workers/opencode_gateway.py`) must count pending forms and permissions as
   work so the 180 s silence limit does not interrupt the turn. Add a test.
5. Remove `question: deny` from the agent file at `src/main.rs:911`.
6. Voice: when a card is open, "yes / allow", "always", "no / deny" answer the newest permission;
   a single-choice form accepts its option label. Route through the existing `voice_confirm` path.
7. Tests: a gateway test with a pending form outlasting the silence limit; a daemon test that a
   `form` line becomes a chat event and `form_reply` posts the answer; a `--chat` GUI test that
   the card renders and submits.

**Done when** asking Navi "rename the files in ~/tmp/x, ask me before each one" produces approval
cards that work by click and by voice, and the turn finishes after the last answer.
**Effort** 2 d. **Risk** low.

---

## Phase 4 — One live connection instead of a process per turn

**Problem:** a new process per turn means a new SSE connection per turn. Between turns nobody
listens, so session lists, settings and approvals raised by background work go stale, and each
catalog read pays a Python start-up plus a TCP handshake (measure it before and after).

**Solution:** `workers/opencode_bridge.py`, a long-lived worker that `navid` starts once, like the
voice worker.

1. It imports `Gateway` from `opencode_gateway.py` (keep all request code there) and holds one
   `GET /api/event` stream for the whole server (not filtered to one session).
2. It speaks JSON lines over a Unix socket at `$NAVI_RUNTIME_DIR/opencode.sock`: requests
   `{"id", "op", "args"}`, replies `{"id", "ok", "result"|"error"}`, and unsolicited
   `{"event": <type>, "data": …}` lines for every server event.
3. Each time the event stream (re)connects it tells `navid` to refetch everything, since the
   stream does not replay missed events. (There is no `server.connected` event in 2.2.5; the
   reconnect itself is the signal.)
4. `navid` (`src/chief.rs` `gateway_op` and `stream_gateway`) sends requests to the socket and
   falls back to spawning `opencode_gateway.py` when the socket is absent, so a crashed bridge
   degrades instead of breaking chat. Restart the bridge with backoff.
5. `navid` turns `session.*`, `*.updated`, `permission.asked`, `form.created` into UI events, which
   replaces the 30 s cache from Phase 2 and lets the session list update live.
6. Concurrency: one turn per session at a time, several sessions at once. The per-turn state in
   `Gateway` (`_busy`, `_tools`, `_records`, …) must become per-session objects. Do this as a
   separate step with its own tests before wiring `navid`.

**Done when** starting a turn from a terminal `opencode` in the same project makes Navi's session
list show it running within 1 s, and killing the bridge mid-idle leaves chat working through the
fallback. **Effort** 4–5 d. **Risk** medium. This is the hardest phase; do it after 1–3 so their
tests guard the refactor.

---

## Phase 5 — Stay in step with OpenCode updates

**Goal:** an OpenCode update either just works or tells the user exactly what broke.

1. On bridge start (Phase 4; until then, on `navid` start) call `server.info` for the version and
   run `opencode_contract.problems()` against the live spec. Store the result in status.
2. Subscribe to `installation.updated` and `installation.update-available`; re-run the check after
   an update.
3. UI: when problems exist, a one-line banner, "OpenCode 2.3.0 changed: session.fork moved.
   Some features are off.", and disable only the features whose operations are listed.
4. `scripts/check_opencode.sh`: runs `opencode_contract.py check`, then the gateway, assistant and
   streaming suites. Run it after every OpenCode upgrade; when it passes, `snapshot` and commit.
5. Read each OpenCode release's "Breaking changes" section; 2.2.7 has an SDK entry. The 1.x docs
   at `opencode.ai/docs/server` are wrong for 2.x: do not use them.

**Done when** editing a path in `config/opencode-contract.json` to something the server lacks
makes `check` exit 1, shows the banner, and disables only that feature. **Effort** 1 d.

---

## Phase 6 — Drive other harnesses the way T3 Code does

**Where things stand:** tmux panes plus screen scraping (`src/adapters.rs`, plans 069–082). That
cannot show tool calls, approvals or a reliable "done", and typing into a pane is a guess.

**What T3 Code does** (MIT, `github.com/pingdotgg/t3code`; Paseo `github.com/getpaseo/paseo`,
Apache-2.0, is the same shape): one server owns every harness process. Each harness has an adapter
that turns its native protocol into one normalized event stream. Clients read capability flags,
never the harness name. Native protocols for the big four (Claude Agent SDK, `codex app-server`,
OpenCode, `pi --mode rpc`); ACP for the rest. The adapter method list to copy:
`packages/provider-core/src/server/ProviderAdapter.ts` (`startTurn`, `interruptTurn`,
`respondToRuntimeRequest`, `forkThread`, `rollbackThread`, plus optional `compactThread`,
`applyModelSelection`, …).

**ACP (Agent Client Protocol)** is JSON-RPC 2.0 over the agent's stdin/stdout, one message per
line. Spec: `agentclientprotocol.com`. Stable v1 methods: client to agent `initialize`,
`session/new`, `session/load`, `session/prompt`, `session/cancel`, `session/set_mode`; agent to
client `session/update` (text and thought chunks, tool calls, plans), `session/request_permission`,
`fs/read_text_file`, `fs/write_text_file`, `terminal/*`. Launch commands (official registry):

| Harness | Command |
|---|---|
| Claude Code | `npx @agentclientprotocol/claude-agent-acp` |
| Codex | `npx @agentclientprotocol/codex-acp` |
| OpenCode | `opencode acp` (starts a private server, not the shared one) |
| Pi | `npx pi-acp` (third-party, MVP quality) |
| Gemini CLI | `npx @google/gemini-cli --acp` |
| Cursor | `cursor-agent acp` |
| Copilot | `npx @github/copilot --acp` |
| Goose | `goose acp` |

**Recommendation:** start with one ACP client for every harness except OpenCode (which keeps its
HTTP API), then replace Claude and Codex with native adapters only when a needed feature is
missing in ACP (fork, rollback, steering). This is T3's own advice in
`docs/internals/adding-a-provider.md`.

**Steps**

1. `workers/acp_client.py`: spawn the command, `initialize` (protocol v1), `session/new {cwd,
   mcpServers: []}`, `session/prompt`, `session/cancel`. Map `session/update` notifications to
   the same records the gateway emits (`delta`, `reasoning`, `tool_status`, `tool`, `permission`,
   `done`) so `chief.rs` and the chat UI need no new paths. Answer `session/request_permission`
   through the Phase 3 approval card. Implement `fs/read_text_file` and `fs/write_text_file`
   restricted to the session's cwd. Do not advertise the `terminal` capability at first.
2. Record a real transcript from `npx @agentclientprotocol/claude-agent-acp` for one small task
   and use it as the fixture for a fake agent in `scripts/test_acp_client.py`. No paid calls in
   tests.
3. Adapter registry in `src/adapters.rs`: each harness gets `transport: "tmux" | "acp" |
   "opencode"` and capability flags (`fork`, `resume`, `cancel`, `approvals`, `models`). Profiles
   pick a transport; tmux stays as the fallback and for interactive use.
4. Chat UI: a harness picker per conversation. Same message renderer; capability flags hide what
   a harness cannot do.
5. Hand-off between harnesses: when switching mid-conversation, start the new harness with a
   short summary of the old conversation (T3's "context handoff"); never pretend it is the same
   thread.
6. Voice: "ask Codex to …" routes through the same path; `chief.rs` already parses harness
   names for the tmux launcher.

**Done when** "Navi, ask Claude to add a test for the parser in zen-relay" runs Claude through ACP,
the chat shows its tool calls and approval requests, and "stop" cancels it.
**Effort** 6–8 d. **Risk** medium: adapters lag their harness; pin versions in the profile.

---

## Phase 7 — Browser, email and accounts

Facts: the default browser is **Firefox** (`org.mozilla.firefox.desktop`); Chrome exists only as a
Flatpak. Most "use my logged-in browser" tools are Chromium-only.

**Recommendation:** register these as MCP servers in OpenCode (Phase 2 MCP writes, or
`opencode mcp add`), so every harness and Navi's own assistant can use them.

1. **Browser (Firefox):** Mozilla's `@mozilla/firefox-devtools-mcp` on a dedicated profile.
   Mozilla warns against the everyday profile (it sets `navigator.webdriver`, which trips bot
   checks).
   ```bash
   firefox -CreateProfile navi-agent
   firefox -P navi-agent --no-remote --marionette --remote-debugging-port &   # sign in to mail and social sites once here
   opencode mcp add firefox --global -- npx -y @mozilla/firefox-devtools-mcp@0.10.7 --connect-existing --marionette-port 2828
   ```
   Navi setting: "Agent browser profile", with a button that starts that Firefox profile. Pin the
   MCP version; do not use `@latest`.
2. **Browser (Chrome, alternative):** `@playwright/mcp --extension` with the Playwright extension
   attaches to the real Chrome profile. Test against the Flatpak Chrome before offering it.
3. **Email:** an API, not webmail clicking. Gmail: `uvx workspace-mcp --tools gmail calendar drive`
   (`taylorwilsdon/google_workspace_mcp`, MIT) with the user's own Google Cloud OAuth client.
   Other providers: `nikolausm/imap-mcp-server`. Navi already has `workers/mail.py`; decide with
   the user whether MCP replaces it, and keep one path only.
4. **Safety:** the persona prompt (`src/main.rs:38`) already says page and mail content is
   information, never instructions, and to ask before sending, posting, buying or deleting. Keep
   approvals on for the browser and mail tools (`permission.saved` must not hold an "always" for
   send or post); add that as a check in the settings page.
5. **Assistants worth linking, not embedding:** OpenClaw and Hermes Agent speak ACP and can be
   added as Phase 6 harnesses. Both are Chromium-only for real-profile browsing, so neither
   replaces step 1.

**Done when** "Navi, check my unread email and summarize it" uses the mail MCP, and "open my X
notifications" opens them in the agent Firefox profile, with an approval card before any post.
**Effort** 2–3 d. **Risk** OAuth setup needs the user; sites may still flag automation.

---

## Order and sizing

| Phase | Depends | Effort | Who |
|---|---|---|---|
| 0 Land work | none | 5 min | user |
| 1 Sessions | 0 | 2–3 d | executor |
| 2 Settings | 0 | 3 d | executor |
| 3 Approvals and questions | 0 | 2 d | executor |
| 4 Live bridge | 1–3 | 4–5 d | strongest available agent |
| 5 Update-proofing | 4 (or start now on `navid` start) | 1 d | executor |
| 6 Harnesses over ACP | 3 | 6–8 d | executor; step 1 by a strong agent |
| 7 Browser and email | 2 | 2–3 d | executor plus user for sign-ins |

Phases 1, 2 and 3 are independent and can run in parallel worktrees. Note that `ui/station.js` had
uncommitted edits by another session in `~/Documents/navi` when this was written; rebase onto them
before touching the UI.
