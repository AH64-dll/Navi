# NAVI Research: Prime Agent Integration + Refined Architecture

Research date: 2026-10-10. All Prime Agent claims below were **verified on the live local install**
(`prime-agent` v0.10.x Rust build at `~/.local/share/prime-agent/prime-agent`, agent dir `~/.prime/agent`),
not just from docs. Items verified by experiment are marked **[verified]**; unverifiable or risky items are flagged.

---

## 1. Prime Agent as the NAVI harness — verified integration surface

### 1.1 What the local build actually is

- Rust binary `~/.local/share/prime-agent/prime-agent` (launched via `~/.local/bin/prime-agent` shell wrapper).
- **[verified]** `prime-agent --help` exposes: TUI (default), `-p/--print` headless mode, `--mode text|json|rpc|acp|daemon`,
  `--daemon-socket`, `--goal`, `--autonomous*` limits, `--skill`, `--model`, `--provider`.
- Daemon: `/tmp/prime-agent-rust-<uid>/daemon.sock` **[verified live]** — plus one `worker-*.sock` per session.
  The daemon owns sessions; `prime-agent` CLI is a thin client to it.
- Config/state dir `~/.prime/agent/` **[verified]**: `settings.json`, `models.json`, `auth.json`,
  `mcp-service-catalog.v2.json`, `sessions/*.jsonl`, `session-artifacts/`, `daemon-workers/`, `session-leases/`.
- Kernel: a persistent CPython REPL (`python -m rlm.repl`, protocol v3 over stdin/stdout NDJSON —
  source ships in `~/.local/share/prime-agent/prime-agent-runtime/src/rlm/`) whose venv lives at
  `~/.prime/agent/kernel-venv/` (has the `mcp` Python SDK installed). The model's only tool is `ipython`;
  everything else (bash, edit, mcp.*, rlm.*, agent_message, websearch) is a Python skill.

### 1.2 How an external voice daemon triggers NAVI — the answer

There are **four verified trigger surfaces**, ranked by fit:

| Mechanism | Verified | Latency | Persistence | Use for |
|---|---|---|---|---|
| `prime-agent send <agent-id> "<msg>"` | **[verified]** — sent "Reply with exactly: NAVI-TRIGGER-OK" to idle session `509a37bc0f8c`; assistant replied `NAVI-TRIGGER-OK` in the session log within ~5 s | ~1 s admission, reply as fast as the model | Message lands in the daemon-owned session; survives detachment | **Primary**: voice daemon keeps one long-lived named agent (`navi`) and `send`s every voice turn to it. Agent name/ID is stable; `prime-agent rename` exists. |
| `prime-agent -p "<msg>"` (print mode) | **[verified]** — returned `PRINT-MODE-OK` in <60 s | one-shot; cold kernel start each call | Session recorded in `~/.prime/agent/sessions/` | One-shot voice commands where context doesn't matter; simplest fallback, no session management needed. |
| `prime-agent schedule add <agent> <cron|once> -- <msg>` | help verified; not exercised | scheduled | persisted cron-jobs | Timed/reminder voice commands ("remind me at 6pm") — voice daemon forwards directly. |
| Daemon socket RPC directly | socket exists; wire protocol = NDJSON DaemonCommand enum (extracted from binary: `send_message`, `steer`, `follow_up`, `prompt`, `create`, `agent_messages_status`, ~80 commands) | lowest | n/a | **Not recommended**: no public protocol doc; CLI covers everything. Treat as internal. |

**Recommended voice→agent path:** a long-lived root agent named `navi` (created once with
`prime-agent --goal "<persistent objective>"` or via `rlm.create_session` from a supervisor),
then the voice daemon runs `prime-agent send navi "<transcribed utterance>"` per turn and reads the
session JSONL tail (`sessionFile` from `prime-agent list --json`) for the reply to speak back.
`send` also accepts `--steer`/`--follow-up` delivery modes (option placement is fussy — put flags
*before* the positional args and `--` before message text starting with dashes; plain
`prime-agent send <id> "<msg>"` worked reliably).

Replies: there is no documented reply-webhook. The voice daemon should either (a) poll the session
JSONL (each line is a `{"type":"message", "message":{"role":...}}` record — trivially parseable),
or (b) have the `navi` agent call out itself (TTS via bash/skill) — option (b) is simpler and
event-driven, see §1.5.

### 1.3 MCP servers — verified configuration

- `prime-agent mcp add <name> --url <url> [--bearer-token-env-var|--oauth]` for HTTP;
  `prime-agent mcp add <name> [--cwd] [--env X=Y] -- <command> [args...]` for stdio **[verified from CLI help]**.
- `mcp list` showed `No user-configured MCP servers` — entries persist under the `mcpServers` key
  (referenced in `settings.json` schema and in `rlm/mcp.py`: "not declared in user settings").
- In-kernel access: `mcp.list_tools(server)`, `mcp.call_tool(server, tool, args)`,
  `mcp.list_connections()`, `mcp.search_plugins()`, `mcp.search_tools()` — all real functions
  in `rlm/mcp.py` **[verified signatures]**. stdio servers spawn via `mcp.client.stdio.stdio_client`
  inside the kernel venv; HTTP servers use streamable-HTTP with Bearer from `auth.json`
  (`mcp:<server>` entries) or env-var tokens. OAuth login is host-side (`/mcp login` in TUI).
- There is also a **service catalog**: `mcp-service-catalog.v2.json` fetched from
  `raw.githubusercontent.com/PrimeIntellect-ai/prime-agent-catalog` — a curated plugin directory
  (`mcp.search_plugins/list_plugins` query it).

**NAVI implication:** every NAVI capability should be an MCP server. `navi-browser` (wraps the
existing navi extension CLI socket), `navi-desktop` (Wayland input/AT-SPI), `navi-cyber` (sandboxed
security tools, §3), `navi-voice` (TTS reply + listener status). `prime-agent mcp add` once per
server; tools then appear as Python callables in every session — no per-session setup.

### 1.4 Persistent always-on skills/tools

Three complementary layers, all verified present:

1. **Python skills** — importable packages under `~/.local/share/prime-agent/skills/<name>/`
   (SKILL.md + `src/<pkg>/`). `--skill <path>` per run, or `prime-agent package install <src>`
   for "capability packages" (skills + prompts + themes; `--local` installs into the project).
   Built-ins: `edit`, `bash`, `compact`, `refine`, `goal`, `mcp`, `websearch`, `attach-image`,
   `agent-message`, `agent-observe`, `rlm-heartbeat`, `factory`, `prime-intellect`, `skill-creator`.
2. **Continual Harness state** (`rlm.harness.*`) — durable per-scope memories, prompt notes,
   skills and *subagent specs* the agent itself CRUDs. `/refine` applies small evidence-backed
   edits; `autoRefine` exists in settings. NAVI gets self-improving voice-command playbooks for free.
3. **AGENTS.md / project context files** in the repo + `--append-system-prompt`/`--system-prompt`
   for the base persona.

### 1.5 Local model as a tool/subagent — verified two ways

1. **As a model in the catalog** — **[verified by experiment]**: adding
   ```json
   {"providers": {"navi-local": {
       "baseUrl": "http://127.0.0.1:8080/v1", "api": "openai-completions",
       "apiKey": "dummy",
       "models": [{"id": "navi-router-3b", "contextWindow": 32768, "maxTokens": 8192,
                   "cost": {"input":0,"output":0,"cacheRead":0,"cacheWrite":0}}]}}}
   ```
   to `~/.prime/agent/models.json` makes `prime-agent model list` show it immediately — same
   `models.json` format as Pi (`docs/models.md` in the navi fork still documents it: `openai-completions`,
   `openai-responses`, `anthropic-messages`, `google-generative-ai` APIs; `compat` flags for
   `supportsDeveloperRole`/`supportsReasoningEffort` on Ollama/vLLM-class servers).
   Then `await rlm.spawn(prompt, name="router", model="navi-local/navi-router-3b")` runs a
   **subagent on the local model** — the supervisor↔executor tier split works natively.
   `rlm.find_models('qwen')` and `subagentDefaultModel` in settings also exist.
   *Caveat:* a small local model runs the **full Prime Agent system prompt + tools**; it's a real
   subagent, good for structured extraction/classification subtasks, not a raw completion call.
2. **As a plain service** — the router model behind `llama.cpp`/`vLLM` on localhost called from a
   NAVI Python skill (`httpx`) — cheaper per call, no agent overhead; this is what I'd use for the
   always-on CPU router (see §2).

### 1.6 Limits and workarounds

| Limit | Status | NAVI workaround |
|---|---|---|
| **No sandbox** | README warning verified | Run NAVI's *executors* (desktop input, cyber tools) inside their own sandboxed service (bwrap/Firejail/Docker for cyber; Wayland portals for desktop). Prime Agent itself stays the trusted orchestrator — only its *tool servers* are hardened. For untrusted code execution, expose a `navi-sandbox` MCP that runs commands inside a throwaway container; the agent calls the tool, never the host shell, for risky work. |
| **Beta, fast-moving** | v0.10.x beta; daily commits; docs lag code | Pin the installed build (`prime-agent update` is manual); treat `send`/`list`/models.json/mcp CLI as the stable contract — smoke-test after every update. |
| **No SDK / no importable lib** | verified — it's a program | NAVI's voice daemon is a separate process driving the CLI — fine, since the daemon socket is an implementation detail. |
| **No reply push channel to external processes** | verified | Poll session JSONL, or better: give the navi agent a `navi_voice.reply(text)` skill/MCP tool so the *agent itself* pushes to the TTS — fully event-driven, no polling. |
| **RLM depth / subagent caps** | `rlmMaxDepth` setting exists | `rlm.spawn` from depth-0 is fine; keep NAVI's session as the depth-0 root. |
| **Auth = subscription or key via `/login`** | verified providers: `devin` (local shim @127.0.0.1:8311), `openai-codex`, `opencode` families in models.json | Keep an API-key-configured provider as fallback — subscription ToS is a gray zone for automation (same caveat as harness.md). |

### 1.7 What NAVI must still build itself

- Voice pipeline daemon (Pipecat: sherpa-onnx KWS → Silero VAD → QwenCleo/Nemotron ASR → `prime-agent send` → Piper/SILMA TTS) — Prime Agent has zero audio.
- `navi-desktop` executor service: AT-SPI2 for observation + `ydotool`/KDE portals for input (Wayland).
- `navi-cyber` sandboxed tool service (§3).
- The local decision router model + its serving (llama.cpp on CPU or GPU-offload).
- Optional: a systemd user unit wiring voice daemon → prime-agent.

---

## 2. Refined end-to-end architecture

```text
                 ┌──────────────────────────── NAVI voice daemon (Python/Pipecat) ─────────────┐
 mic ──► KWS ───►│ sherpa-onnx KWS · Silero VAD · QwenCleo-ASR-1.7B / Nemotron-0.6B · Piper/   │───► speaker
 ("Hey NAVI")    │ SILMA TTS                                                                 │
                 └──────┬──────────────────────────────────────────────────┬─────────────────┘
                        │ transcript                                        │ reply text (agent calls
                        ▼                                                   │  navi_voice.reply — push, no poll)
                 prime-agent send navi "<utterance>"                prime-agent list --json / JSONL tail (optional UI)
                        │
                        ▼
┌─────────────────────────────────────────────────────────────────────────────────────────┐
│  PRIME AGENT DAEMON  (/tmp/prime-agent-rust-<uid>/daemon.sock)                          │
│  root agent "navi" — supervisor model: subscription LLM (devin/openai-codex provider)   │
│  persistent IPython kernel · rlm.spawn subagents · /goal · heartbeats · /refine          │
│                                                                                         │
│   fast path: NAVI python skill `route()` ──HTTP──► LOCAL ROUTER MODEL                    │
│              (llama.cpp CPU, ~1-3B, intent→tool JSON; <200 ms, no agent overhead)        │
│              simple commands short-circuit to a tool without a cloud call*               │
│                                                                                         │
│   heavy path: rlm.spawn(name="exec", model="navi-local/<executor-model>")                │
│              local model subagent for cheap structured subtasks                          │
└──────┬───────────────┬──────────────────┬──────────────────┬───────────────┬─────────────┘
       │ mcp.call_tool │                  │                  │               │
       ▼               ▼                  ▼                  ▼               ▼
  navi-browser    navi-desktop       navi-terminal      navi-cyber      navi-voice
  MCP server      MCP server         (bash/ssh tool     MCP server      MCP server
       │               │              or direct bash)        │               │
       ▼               ▼                                   ▼               ▼
  existing NAVI    Wayland: AT-SPI2                  sandbox manager:   Piper/SILMA
  browser bridge   observe + ydotool/                bubblewrap/        daemon
  (packages/navi/  portal input                      Docker container
  browser — BUILT)                                   with security
                                                     toolset, host-
                                                     bridge allowlist
```

\* The fast path is honest engineering: for "open terminal and run X" you don't need a cloud LLM.
The router model decides `{"tool": "terminal", "command": ...}` locally; ambiguous/rich requests
escalate to the Prime Agent supervisor which already has full context.

**Placement notes:**
- **Cyber tools** live in `navi-cyber` — a Docker container (or bwrap profile) with the security
  toolset installed, exposing a narrow MCP API (`scan`, `enum`, `exploit-check`, `wordlist-run`)
  with an allowlisted target policy file. The *container* is the sandbox; Prime Agent just calls
  MCP tools. No Kali VM — the tools are distro packages/containers (e.g. a security-tools image
  or Arch `blackarch` packages inside a rootless container). Details are the cyber subagent's job.
- **Host bridge**: the *controlled* surface between sandboxed tools and the host is the MCP
  server boundary itself — one audited process owning the container socket, never raw socket
  access from the model.
- **Browser**: unchanged — `navi-browser` MCP wraps the existing CLI socket at
  `packages/navi` (native messaging extension + private POSIX socket, already built/tested).
- **Voice reply**: event-driven via `navi-voice` MCP `reply(text)` tool — the agent calls it as
  part of its turn; also exposes `status()`, `mute()`, `listen_state()`.

---

## 3. Component stack (revised)

| Layer | Component | Choice | Where it runs |
|---|---|---|---|
| Harness/supervisor | **Prime Agent** (daemon, root agent "navi") | subscription LLM (e.g. ChatGPT/Codex or Anthropic via `/login`; keep API-key fallback) | host, daemon |
| Decision router | Local ~1-3B instruct model behind llama.cpp | see decision-models subagent's pick | CPU (always-on) |
| Executor model | Small VLM/GUI model for desktop grounding | see decision-models subagent's pick | GPU on-demand |
| STT | QwenCleo-ASR-1.7B (primary), Nemotron-3.5-ASR-0.6B (streaming fallback) | voice-stack.md (done) | GPU/CPU |
| TTS | Piper (en/short) + SILMA v1 (ar) | voice-stack.md (done) | CPU |
| KWS/VAD | sherpa-onnx KWS + Silero VAD | voice-stack.md (done) | CPU, ~100 MB |
| Browser | existing NAVI extension + CLI socket | built: packages/navi | Chrome + host |
| Desktop exec | AT-SPI2 observe + ydotool/KDE portal input | NAVI-built `navi-desktop` MCP | host (Wayland-safe) |
| Terminal | Prime Agent `bash` builtin for host; `navi-terminal` MCP for controlled env | built-in | host/sandbox |
| Cyber | `navi-cyber` MCP → containerized toolset | NAVI-built; no Kali VM | rootless container |
| Sandbox manager | bubblewrap/rootless-docker behind `navi-cyber` + `navi-exec` MCPs | NAVI-built | host |
| Memory/context | Continual Harness (`rlm.harness`) + project AGENTS.md | built-in | daemon |

---

## 4. Revised phased roadmap (replaces the old 6 phases)

**Phase 0 — Foundation audit (done → verify).** `packages/navi` browser bridge exists; run its
`npm run check:navi` + manual Chrome validation list from `docs/navi-foundation/architecture.md`.
Install/pin Prime Agent; record version.

**Phase 1 — Prime Agent spine + voice loop skeleton.**
- `prime-agent` daemon up; create named root agent `navi` with `--goal` persona.
- Build `navi-voice` MCP (reply/status). Build voice daemon v0: KWS→VAD→STT→`prime-agent send navi`→agent calls `navi_voice.reply`→TTS. Proves full-duplex round trip on one utterance type ("what time is it" tier).
- Deliverable: wake word in, spoken answer out, all local except the LLM.

**Phase 2 — Executors behind MCP.**
- `navi-browser` MCP wrapping the existing bridge socket (observe/focus/scroll per current grants).
- `navi-terminal` thin MCP (guarded shell in project cwd; the agent's builtin bash covers the rest).
- `navi-desktop` v0: AT-SPI2 tree dump + ydotool click/type behind a small action vocabulary —
  reuse the browser bridge's consent model (explicit per-window grant).
- Deliverable: "open Firefox and scroll to the comments" works end-to-end.

**Phase 3 — Local models tier.**
- llama.cpp router (CPU) + `route()` skill; add provider to `~/.prime/agent/models.json` for
  `rlm.spawn(model="navi-local/...")` subagents.
- Executor/grounding model (see sibling research pick) behind vLLM/llama.cpp on the RTX 4050 —
  on-demand load (llama.cpp `--no-warmup` + unload policy), since 6 GB VRAM cannot hold it plus
  ASR plus TTS simultaneously.
- Deliverable: offline path for simple commands; VRAM budget doc.

**Phase 4 — Cyber capability, sandboxed.**
- `navi-cyber` MCP: containerized toolset + allowlist policy + audit log. No Kali.
- `navi-exec` generic sandboxed-run MCP (bwrap) for untrusted commands Prime Agent generates.
- Deliverable: "run nmap on my lab VM" works; policy denies non-allowlisted targets.

**Phase 5 — Autonomy + polish.**
- `/goal` for multi-step tasks, `prime-agent schedule`/`rlm_heartbeat` for reminders and
  background monitoring, `--autonomous` gates for unattended work, `/refine` tuning of NAVI's
  harness entries (voice playbooks, executor specs).
- KDE integration (global shortcut, notification, optional plasma widget), installer/systemd units,
  perf budget validation (<200 ms router, <1.5 s full loop on cached paths).

Phases 2–4 are independent and can run as parallel subagents.

---

## 5. Honest unknowns / could-not-verify

1. **Daemon wire protocol** — the NDJSON command list is real (extracted), but undocumented;
   treat as private. All integration goes through the CLI.
2. **`send` flag syntax** — `--follow-up`/`--steer` exist in help but reject flag-after-`--json`
   ordering variants; plain `send <id> "<msg>"` worked. Test the exact flag combination you need.
3. **models.json hot-reload** — Pi docs say it reloads on `/model` open; in the daemon the running
   session may cache. Worst case: restart the navi session after editing. File format verified live.
4. **MCP config persistence format** — `mcpServers` key confirmed to exist in settings schema and
   `rlm/mcp.py`, but I did not add a live server in this session (none configured on this machine).
   `prime-agent mcp add` + `mcp.list_tools` in a fresh session is the verification step for Phase 1.
5. **GPU contention** — ASR + TTS + executor model on 6 GB VRAM is tight; the voice doc already
   budgets it, but the executor model almost certainly needs on-demand load/unload, which adds
   2-5 s cold-start. Mitigate by keeping the *router* on CPU.
6. **Subscription ToS** for automated/agentic use — unchanged from harness.md; keep API-key fallback.
