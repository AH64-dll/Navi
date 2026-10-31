# NAVI — Refined Plan (post-research synthesis)

**Date:** 2026-10-10 · **Status:** supersedes the original product spec where they conflict.
**Inputs:** `voice-stack.md`, `harness.md`, `local-decision-and-gui-models.md`,
`cyber-toolkit.md`, `prime-agent-integration.md` (all in this dir).

This doc folds the user's 4 new decisions into one coherent plan.

---

## 0. Decisions now locked

| # | Decision | Change vs original plan |
|---|---|---|
| 1 | Harness = **Prime Agent** (not Pi) | Pi demoted to a *reference* (the existing `packages/navi` browser bridge is a Pi-repo fork and stays — its code is harness-agnostic). Prime Agent is the Rust daemon; NAVI talks to it via CLI + MCP, not by embedding (it has no SDK). |
| 2 | Replace the Jev-like models & Whisper | STT done in voice-stack.md. This doc adds: router = **FunctionGemma-270M**, GUI = **UI-Venus-2-9B** (primary) / **OpenCUA-7B** / **UI-Venus-1.5-2B** (fallbacks). Laya/Tev1/NanoJev/ShowUI dropped. |
| 3 | **No Kali VM** — cyber tools live in the harness | All tools are distro pkgs / static Go binaries / small Docker images behind a `navi-cyber` MCP server in a rootless container. |
| 4 | Plan refined | New architecture + roadmap below. |

**Machine correction:** this device is **Fedora 44**, not EndeavourOS. The design is
distro-agnostic (everything real runs in containers), so it's harmless — but fix any
Arch/KDE-specific assumptions (e.g. install pkgs via `dnf`, not `pacman`). KDE/Wayland still applies.

---

## 1. Refined architecture

```text
            USER — voice ("Hey NAVI") / text
                          │
            ┌─────────────▼─────────────┐
            │   VOICE DAEMON (always-on)│  CPU ~100MB, <2%
            │ sherpa KWS → Silero VAD → │
            │ QwenCleo-ASR / Nemotron   │
            └─────────────┬─────────────┘
                          │ `prime-agent send navi "<utterance>"`
                          ▼
        ┌─────────────────────────────────────┐
        │   PRIME AGENT — root agent "navi"   │  cloud supervisor
        │   (subscription LLM via /login,     │  (ChatGPT/Claude)
        │    IPython kernel = only tool)      │
        └───────┬─────────────────────┬───────┘
                │ fast path            │ heavy path
                ▼                      ▼
        ┌───────────────┐    ┌────────────────┐
        │ ROUTER skill   │    │ rlm.spawn local│
        │ FunctionGemma- │    │ -model subagent│
        │ 270M (llama.cpp│    │ (executor loop)│
        │  CPU, <100ms)  │    └───────┬────────┘
        └───────┬───────┘            │
   intent→JSON  │                    │ escalates to cloud on {escalate:true}
                ▼                    ▼
        ┌───────────────────────────────────────────┐
        │        MCP EXECUTOR LAYER (the tools)      │
        ├───────────┬───────────┬─────────┬─────────┤
        │navi-browser│navi-desktop│navi-cyber│navi-voice│
        │ (EXISTING  │ AT-SPI2 + │ container│ TTS reply│
        │ packages/  │ ydotool / │ toolset  │ +status  │
        │ navi bridge│ KDE portal│ (no Kali)│ (Piper/  │
        │ — BUILT)   │           │ allowlist│  SILMA)  │
        └───────────┴───────────┴─────────┴─────────┘
                          │
                ┌─────────▼─────────┐
                │  SANDBOX = the MCP│  rootless Podman / bwrap
                │  server boundary  │  container, per-invocation,
                │  (audited process)│  cap-drop=ALL, allowlist
                └───────────────────┘
```

**Honest engineering note:** for "open terminal and run X" no cloud LLM is needed — the
router decides `{"tool":"terminal","command":...}` locally. Rich/ambiguous requests go up
to the Prime Agent supervisor, which already holds full context.

---

## 2. Final component stack

| Layer | Pick | Notes |
|---|---|---|
| Harness/supervisor | **Prime Agent** | daemon; root agent `navi`; subscription auth, API-key fallback; **pin version** (beta). |
| Decision router (CPU) | **FunctionGemma-270M-IT** | fine-tune on NAVI action schema; <100ms CPU. Backup: **LFM2.5-1.2B**. |
| Offline mini-supervisor | **Qwen3-4B-Instruct-2507** (Q4) | BFCL-v3 61.9; ~2.6GB, on disk for cloud-down. |
| GUI grounding (GPU, on-demand) | **UI-Venus-2-9B** Q4/Q5 offload → fallback **OpenCUA-7B** (MIT) / **UI-Venus-1.5-2B** | OSWorld-V 70.8 / 27.9 / SSP 57.7. Verify Venus-2 license before ship. |
| STT | **QwenCleo-ASR-1.7B** → **Nemotron-3.5-ASR-0.6B** | voice-stack.md (locked). |
| TTS | **Piper** (en/short) + **SILMA v1** (ar) | voice-stack.md (locked). |
| KWS/VAD | **sherpa-onnx KWS** + **Silero VAD** | voice-stack.md (locked). |
| Browser | existing `packages/navi` bridge → `navi-browser` MCP | BUILT; wrap its socket. |
| Desktop exec | AT-SPI2 observe + ydotool/KDE portal → `navi-desktop` MCP | NAVI-built; reuse bridge consent model. |
| Cyber | `navi-cyber` MCP → rootless container toolset + `scope.yaml` allowlist | see cyber-toolkit.md; no Kali VM. |
| Generic sandbox | `navi-exec` MCP (bwrap) for untrusted generated commands | covers Prime Agent's no-sandbox gap. |
| Memory/context | Continual Harness (`rlm.harness`) + AGENTS.md | built-in. |

---

## 3. Revised roadmap (replaces old 6 phases)

- **Phase 0 — Audit (verify).** `packages/navi` bridge exists → run `npm run check:navi` + the
  manual Chrome gate from `navi-foundation/architecture.md`. Install & pin Prime Agent.
- **Phase 1 — Spine + voice loop.** Daemon up; named agent `navi`; `navi-voice` MCP (reply/status);
  voice daemon v0 (KWS→VAD→STT→`send navi`→`navi_voice.reply`→TTS). One spoken round-trip works.
- **Phase 2 — Executors.** `navi-browser` (wrap bridge), `navi-terminal`, `navi-desktop` v0
  (AT-SPI2 + ydotool, per-window grant).
- **Phase 3 — Local model tier.** llama.cpp router + `route()` skill; register local provider in
  `~/.prime/agent/models.json`; GUI model behind vLLM/llama.cpp on the 4050, on-demand load
  (VRAM can't hold ASR+TTS+GUI at once — router stays on CPU).
- **Phase 4 — Cyber, sandboxed.** `navi-cyber` MCP (container + allowlist + evidence dir) +
  `navi-exec` bwrap for untrusted commands. Target-scoped only.
- **Phase 5 — Autonomy + polish.** `/goal`, `schedule`/heartbeat, `--autonomous` gates, `/refine`,
  KDE shortcut/notification/systemd, perf budget (<200ms router, <1.5s cached loop).

Phases 2–4 are independent → parallel subagents.

---

## 4. Verified integration mechanics (Prime Agent)

- **Trigger:** `prime-agent send <agent-id|name> "<msg>"` delivers into a persistent daemon
  session (verified end-to-end, ~1s admission). Voice daemon keeps one named agent `navi`.
- **Replies:** no webhook — give `navi` a `navi_voice.reply(text)` MCP tool so it PUSHES to TTS
  (event-driven), or tail the session JSONL.
- **MCP:** `prime-agent mcp add <name> --url ...` (HTTP) or `-- <cmd>` (stdio); tools then appear
  as `mcp.call_tool` in the kernel. Every NAVI capability = an MCP server.
- **Local models:** add `{baseUrl, api:"openai-completions"}` provider to `~/.prime/agent/models.json`
  → `rlm.spawn(prompt, model="navi-local/<id>")` runs subagents on it. Verified by experiment.
- **Scheduling:** `prime-agent schedule add <agent> <cron> -- "<msg>"`.

---

## 5. Risks / open verification (do before committing)

1. **UI-Venus-2-9B license** — card lacks explicit field (1.5 family is Apache-2.0). Verify.
2. **FunctionGemma Gemma ToU** — not Apache; check acceptable-use.
3. **9B-on-6GB latency** — estimate only; measure real tok/s on the 4050 before committing
   (fallback to OpenCUA-7B / Venus-1.5-2B if too slow).
4. **VRAM contention** — ASR+TTS+GUI can't co-load on 6GB; GUI must load on-demand (adds 2-5s).
5. **Subscription ToS** for agentic use — keep API-key path (unchanged from harness.md).
6. **Daemon wire protocol** undocumented — integrate via CLI only.
7. **mcp-kali-server `/api/command`** is unauthenticated RCE — borrow pattern, not code; audit
   pentest-mcp before trusting it.
8. Serper was down during research → several benchmark numbers are vendor-reported, not cross-checked.
