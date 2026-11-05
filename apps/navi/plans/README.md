# Navi remediation plan — 100 bugs, 100 plans

Status: **planning complete, no code executed**. Every plan was produced against the current tree, then verified against the file it cites. Nine duplicate plans were merged into their parent (mapping below); the remaining set is exactly 100.

## How to use this

1. Read the section file for the area you are about to touch.
2. Pick the next plan in the current wave (below); respect `Depends`.
3. Implement, then run the verification named in the plan. The plan is done only when that verification passes in isolated mode.
4. Do not batch unrelated plans into one change unless a wave explicitly says so (098/099/094, and 020/021/024/025 are the only such groups).

Every plan has: evidence (`path:line`), root cause, fix steps, end state, why it helps, dependencies, verification, effort and risk.

## Sections

| File | Area | Plans |
|---|---|---|
| [01-assistant.md](01-assistant.md) | OpenCode bridge, prompt, sessions, approvals | 001–019 |
| [02-stage-layout.md](02-stage-layout.md) | 3D stage, layout system, panel geometry | 020–033 |
| [03-chat-status.md](03-chat-status.md) | Chat surface, message model, status | 034–048 |
| [04-voice.md](04-voice.md) | Capture, endpointing, STT, TTS | 049–068 |
| [05-harness.md](05-harness.md) | Harness discovery, launch, resume, output | 069–082 |
| [06-project-graph.md](06-project-graph.md) | Project model, signals, completion panel | 083–092 |
| [07-reliability.md](07-reliability.md) | RPC, polling, workers, failure paths | 093–100 |

## The 100 plans

Severity: P0 = blocks a product goal or loses data; P1 = major UX/correctness; P2 = polish/maintainability.

| # | Plan | Sev | Wave | Depends |
|---|---|---|---|---|
| 001 | Tool calls, reasoning and steps are discarded | P0 | 2 | — |
| 002 | The "thinking" state is fabricated | P2 | 2 | 001 |
| 003 | The system prompt forbids coding behaviour | P0 | 2 | — |
| 004 | The agent runs in `$HOME`, not a project | P0 | 2 | 083 |
| 005 | No file/image attachments | P2 | 8 | 004 |
| 006 | No approval path for gated tools | P0 | 8 | 001 |
| 007 | Stop does not stop the server side | P1 | 2 | 008 |
| 008 | A stopped turn leaves the session wedged | P0 | 2 | 007 |
| 009 | No token/cost telemetry | P2 | 2 | 001 |
| 010 | Shown model is requested, not actual | P2 | 2 | 009 |
| 011 | Model/agent re-applied every turn | P1 | 2 | 004 |
| 012 | One global assistant session | P1 | 2 | 004 |
| 013 | No markdown/code/diff rendering | P1 | 3 | 003 |
| 014 | Failures rendered as answers | P1 | 2 | — |
| 015 | Streaming quantised to 700 ms | P1 | 3 | 098 |
| 016 | Second request refused, not queued | P1 | 2 | — |
| 017 | Slash catalog cached forever | P2 | 2 | — |
| 018 | Slash bypasses the command lock | P1 | 2 | 016 |
| 019 | `/help` and `/commands` faked | P2 | 2 | — |
| 020 | Stage always dead-centre behind panels | P0 | 6 | — |
| 021 | Chat covers the emblem's lower third | P1 | 6 | 020 |
| 022 | Floor glow invisible | P2 | 6 | 020 |
| 023 | Halo sliced by the panel edge | P2 | 6 | 020 |
| 024 | Terminal overlaps chat by ~120 px | P1 | 6 | 020 |
| 025 | Small windows make layout worse | P1 | 6 | 020, 024 |
| 026 | Translucent panels blur the artwork | P2 | 6 | 020 |
| 027 | Scanlines painted over the UI | P2 | 6 | 020 |
| 028 | Terminal is a fixed card, no dock | P1 | 6 | 024 |
| 029 | Boot intro never plays | P2 | 6 | 030 |
| 030 | Reduced motion half honoured | P2 | 6 | — |
| 031 | One message, two glitches | P2 | 6 | 001 |
| 032 | Pointer parallax tracks the viewport | P2 | 6 | 020, 030 |
| 033 | `hidden` does nothing on panels | P1 | 0 | — |
| 034 | Status chip stuck at `starting` | P1 | 8 | 095 |
| 035 | Raw model path and unexplained "off" | P2 | 8 | 010 |
| 036 | `attention` is sticky | P2 | 8 | 037 |
| 037 | A failed turn leaves no row | P1 | 8 | 014, 048 |
| 038 | Toast renders over the terminal | P2 | 8 | 020 |
| 039 | Failed slash toasts twice | P2 | 8 | 037 |
| 040 | No empty state or onboarding | P2 | 8 | 034 |
| 041 | No per-message actions or metadata | P1 | 8 | 047, 009 |
| 042 | Role and part styling is flat | P2 | 8 | 041 |
| 043 | Session chips show ids and duplicates | P2 | 8 | 072, 096 |
| 044 | Model picker can blank the model | P1 | 8 | 012 |
| 045 | Repeated user message is dropped | P1 | 8 | 037 |
| 046 | Identical reply hidden for 15 s | P1 | 8 | 048 |
| 047 | Chat capped at 200 rows, no persistence | P1 | 3 | 091 |
| 048 | Streaming replies never render | P1 | 3 | 098, 095 |
| 049 | Noise and wake word uploaded to STT | P1 | 4 | — |
| 050 | Wake removal is a fragile regex | P2 | 4 | 049 |
| 051 | Fixed RMS decides silence | P1 | 4 | — |
| 052 | Short commands are slow to endpoint | P2 | 4 | 051 |
| 053 | 20 s cap truncates silently | P2 | 4 | 056 |
| 054 | Mic not read while STT runs | P0 | 4 | 055, 063 |
| 055 | Overflow discards the whole command | P1 | 4 | 054 |
| 056 | No live transcript or correction | P1 | 4 | 049, 051 |
| 057 | English hardcoded everywhere | P2 | 4 | — |
| 058 | Hotwords only on the local fallback | P2 | 4 | 056 |
| 059 | One failure disables a provider 5 min | P1 | 4 | 056 |
| 060 | No echo cancellation | P1 | 4 | — |
| 061 | Spoken "stop" does not stop playback | P1 | 4 | 060 |
| 062 | Speech during a turn is dropped | P2 | 4 | 054, 061 |
| 063 | Crashed voice worker never restarted | P1 | 4 | 100 |
| 064 | Capture loop blocks on status RPC | P1 | 4 | 054 |
| 065 | Voice cannot address a harness | P2 | 4 | 069, 075, 082 |
| 066 | STT docstring describes an old chain | P2 | 4 | — |
| 067 | `ignored_wake` eats "navigate…" | P1 | 4 | 049 |
| 068 | Hex caches one failure forever | P1 | 4 | — |
| 069 | No harness session identity/resume | P0 | 5 | 070, 079 |
| 070 | Profiles can't carry launch options/env | P1 | 5 | — |
| 071 | No harness discovery | P1 | 5 | 070, 072 |
| 072 | `is_file()` + frozen PATH | P1 | 5 | 071 |
| 073 | Dead sessions are permanent | P2 | 5 | 069, 075, 079 |
| 074 | External sessions invisible | P1 | 5 | 069, 079 |
| 075 | No output read path | P0 | 5 | 073, 079 |
| 076 | Prompt dispatch is a blind guess | P1 | 5 | 070, 075 |
| 077 | Args with spaces destroyed on save | P2 | 5 | 070 |
| 078 | Renaming a profile orphans sessions | P1 | 5 | 069, 079 |
| 079 | Session stores five fields only | P1 | 0 | — |
| 080 | `create` reports running without checking | P2 | 5 | 073, 079 |
| 081 | Launch silently rewrites voice target | P2 | 5 | 079 |
| 082 | Stop cannot reach a harness | P1 | 5 | 075, 079 |
| 083 | Projects have no identity/validation | P1 | 0 | — |
| 084 | No tasks/aspects/plans/signals | P1 | 1 | 083 |
| 085 | History cannot feed a graph | P1 | 1 | 083, 084 |
| 086 | No repository awareness | P2 | 1 | 083, 084 |
| 087 | Harness output invisible to the graph | P1 | 7 | 075, 083, 084 |
| 088 | Assistant has no project context | P1 | 2 | 004, 084, 086 |
| 089 | No screen space for a graph panel | P1 | 6 | 020, 084 |
| 090 | No project switcher | P2 | 6 | 083, 088 |
| 091 | No durable per-project conversations | P1 | 1 | 083, 085 |
| 092 | Settings save cancels the in-flight turn | P1 | 1 | 078, 079 |
| 093 | Lost RPC reply hangs forever | P1 | 0 | — |
| 094 | Poll re-sends everything; strip rebuilds | P2 | 8 | 093, 095 |
| 095 | No devtools/console/diagnostics | P2 | 8 | 093 |
| 096 | Stage failure blanks the app | P1 | 8 | 095 |
| 097 | Window sizing ignores DPI/monitors | P2 | 8 | 096 |
| 098 | Stream overflows the 100-event ring | P1 | 3 | 094, 099 |
| 099 | Seq resets with the daemon | P1 | 1 | 098 |
| 100 | Worker lifecycle guessed from a PID | P2 | 0 | — |

## Waves and gates

Each wave is a shippable increment. Do not start the next wave until the gate passes.

**Wave 0 — Truth and foundations.** 033, 079, 083, 093, 100.
Gate: existing tests green; migrations idempotent; a dropped reply times out instead of hanging; a duplicate worker cannot start; no phantom terminal card at boot.

**Wave 1 — Data plane.** 084, 085, 086, 091, 092, 099.
Gate: `graph_score` unit tests (weights, decay, claim cap, empty aspects); restart continuity for events and messages; setting a project during a turn does not cancel it; stale seq resyncs.

**Wave 2 — The assistant becomes an agent.** 001, 002, 003, 004, 007, 008, 009, 010, 011, 012, 014, 016, 017, 018, 019, 088.
Gate: FakeOpenCode schema tests for reasoning/tools/usage/terminal events; one live probe for `directory` and permission names; a turn survives Stop and timeout; queue works; project cwd is correct.

**Wave 3 — Streaming and rendering.** 013, 015, 047, 048, 098.
Gate: 500 deltas at 700 ms and 5 s polling produce identical text; exactly one navi row; code renders safely; history survives restart.

**Wave 4 — Voice accuracy.** 049–068 (020 plans).
Gate: synthetic fixtures for trim/endpoint/VAD/overflow; WER before/after on the project's own fixture set; no real mic or GPU; provider retries and confidence verified with FakeHTTP.

**Wave 5 — Harness orchestration.** 069–082 (all but 079, which is Wave 0).
Gate: stub-harness fixtures for discovery, env/argv, resume, inject, output, exit; Claude path exact; Codex/Antigravity marked best-effort with unknown fields.

**Wave 6 — Layout and the graph surface.** 020–032, 089, 090.
Gate: V-PNG and V-FLOW at 1440×920, 1280×720, 960×640 with zero rect intersections; the graph shows `—` when empty and never a fabricated 0/100.

**Wave 7 — Graph data from harnesses.** 087.
Gate: `PASSED 12` produces one verified signal with evidence; ambiguous text produces none.

**Wave 8 — Finish and polish.** 005, 006, 034–046, 094–097.
Gate: devtools gated; diagnostics block; empty state; per-message actions; one error surface per failure; stage degrades instead of blanking; window fits the monitor.

## Merges (content preserved, not lost)

| Merged | Into | Reason |
|---|---|---|
| S1-02 | 001/002 | Same SSE plumbing; the dispatch label is a step of the activity fix |
| S1-20 | 099 | Identical bug (seq resets on restart) |
| S1-22 | 017 | Same catalog path (no autospawn on reads) |
| S1-23 | 046 | Identical dedupe bug |
| S1-24 | 045 | Same echo-correlation root |
| S2-33 | 020 | Same commit as the frame contract (opacity ownership) |
| S2-34 | 020 | Same commit as the frame contract (generalised framing) |
| S6-87 | 092 | Same generation counter |
| S7-89 | 094 | Same polling/render path |

## Additional defects found during review (not in the 100)

These are real and evidenced; they belong in a follow-up plan rather than being numbered:

1. **Terminal EOF is invisible and input is silently discarded** — `window.py:165-167`, `:171-185`. Fix: notify the page, close and hide the pane, clear `termSession`.
2. **The agent prompt file is rewritten on every daemon start** — `src/main.rs:188-200`. Hand edits to `~/.config/opencode/agents/navi.md` are discarded. Fix: write only when the stored prompt changes; keep a backup.
3. **Harness prompt text is not control-character validated** — `src/main.rs:500-504`, `src/sessions.rs:58-77`. `\n`, `\x03`, `\x1b` can submit lines or inject keys. Fix: reject control chars except `\n\t`, or use bracketed paste (076).

## Verification harness (no mic, no GPU, no real harness required)

- `python3 scripts/test_gui_isolated.py` → real WebKit under Xvfb (`LIBGL_ALWAYS_SOFTWARE=1`), `evidence/isolated-gui.png` + `.json`.
- `python3 scripts/test_gui_isolated.py --workflow` → drives settings, sessions, terminal, Stop.
- `scripts/test_station.py`, `test_opencode_gateway.py`, `test_cloud_stt.py`, `test_voice_stream.py`, `test_voice_policy.py`, `test_voice_headless.py`, `test_hex_asr.py`, `test_persona.py`.
- New hooks must be namespaced (`NAVI_TEST_*`, `NAVI_FORCE_*`) and rejected unless `NAVI_ISOLATED=1`.
- Any scene change requires `npm run bundle`; `index.html` loads `scene.bundle.js`, not `scene.js`.

## Non-goals

- No code changes are part of this deliverable.
- No claims about runtime behaviour that were not observed. Where a field or event name is unverified (session-create `directory`, permission events, tool-failure name, attachment `parts`), the plan says so and carries a fallback or a probe step.
