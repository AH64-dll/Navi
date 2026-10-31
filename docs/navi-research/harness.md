# NAVI Research: Agent Harness / Orchestrator

Research date: 2026-10-10. Focus: Prime Agent vs Pi vs Codex CLI as NAVI's supervisor/harness, plus the wider harness landscape.

---

## 1. Prime Agent (PrimeIntellect-ai/prime-agent) — deep dive

**Sources:** [repo](https://github.com/PrimeIntellect-ai/prime-agent) · [launch blog, Aug 5 2026](https://www.primeintellect.ai/blog/prime-agent) · [arXiv:2608.23552](https://arxiv.org/abs/2608.23552) · [RLM blog](https://www.primeintellect.ai/blog/rlm)

### What it actually is
- A **standalone Rust binary** (TUI + daemon), NOT a Python/TS SDK. Started as a TypeScript fork of **Pi** ("Prime Agent is built on top of `pi`" — launch blog acknowledgements), then rewritten to Rust. Current tree is `crates/pa-agent, pa-ai, pa-cli, pa-core, pa-daemon, pa-models, pa-telemetry, pa-tui, pa-types` — the old `packages/coding-agent` TS tree is gone on `main` (GitHub search still surfaces stale docs links that 404).
- The model's **only tool is a persistent IPython kernel**. File ops, shell, MCP-ish tools, subagents, and context management are all Python function calls in the REPL ("programmatic tool calling"). This is the Recursive Language Model (RLM) design: context-as-variables, subagents-as-function-calls.
- **Continual Harness**: the agent can CRUD its own supplemental prompts, memories, skills, and subagent specs via `rlm.harness.*`; `/refine` applies small evidence-backed edits to that layer with rollback. Base system prompt stays immutable.

### Maturity signals (verified from repo/commits, Oct 10 2026)
| Signal | Value |
|---|---|
| Stars | ~21.7k (2.4k forks) |
| Language | Rust (was TypeScript Pi-fork) |
| License | MIT |
| Latest release | v0.10.1-beta.2 — **still beta**, fast cadence (dozens of 0.9.9-beta.N builds) |
| Last commit | Oct 9 2026 — daily active development (authors: snimu, kevinjosethomas) |
| Age | Public launch Aug 5 2026 (~2 months) |
| Backing | Prime Intellect (serious funded AI-lab, verifiers/prime-RL ecosystem) |

### Capabilities relevant to NAVI
| Need | Status |
|---|---|
| Daemon / background sessions | **Yes, first-class** — background daemon owns sessions over a local socket; `prime-agent attach`, `agents`, `status`, `doctor`, `shutdown`; sessions survive terminal detach |
| Voice-listener trigger | Feasible via daemon socket / `prime-agent` CLI / ACP; no native voice integration |
| Subagents / multi-agent | **Best in class here** — `rlm.spawn/rlm()` persistent subagents, agent-to-agent messaging (restricted to "nuclear family": parent/sibling/child), Agents View UI |
| Subscription auth | **Yes** — README: `/login` supports "subscription or API-key provider"; docs (pre-rewrite, inherited from Pi) list ChatGPT Plus/Pro (Codex), Claude Pro/Max, GitHub Copilot OAuth logins |
| MCP servers | **Yes** — repo `.changes/` includes `acp-mcp-replace-cleanup`, `acp-daemon-initialize-validation`, `acp-session-flags` (ACP = Agent Client Protocol, Zed-style; MCP client support inherited from Pi lineage). Verified presence on current tree via changelog entries; detailed docs page 404s post-rewrite |
| Sandbox | **No.** Explicit README warning: worker/kernel processes are lifecycle isolation, "not a security sandbox"; untrusted code needs an external sandbox |
| Local model as component | Custom OpenAI-compatible endpoints (inherited Pi `models.json` pattern) — should work for a local executor model, but not documented post-rewrite |
| Computer use | None native; would be a Python skill/MCP server |
| Benchmarks | Vendor claims: Opus 5 → 95.5% ARC-AGI-3 (beats Claude Code's harness); strong OOLONG/long-context results vs Pi, Codex, Claude Code |

### Prime Agent vs Pi — what each gives
Prime Agent adds: RLM/IPython single-tool architecture, persistent named subagents, A2A messaging, `/refine` self-improvement, daemon + heartbeats + schedules + autonomous mode, `/goal` persistent objectives.
Prime Agent loses vs Pi: Pi's mature **TypeScript SDK + JSONL RPC mode** for embedding, Pi's extension system, Pi's documented MCP config (`mcp.json` with exposure controls), Pi's llama.cpp router integration, Pi's enormous provider catalog. Prime Agent is a *program you run*; Pi is a *program you can also embed* (OpenClaw embeds Pi's SDK directly).

---

## 2. Pi (earendil-works/pi) — deep dive

**Sources:** [repo](https://github.com/earendil-works/pi) · [docs](https://pi.dev/docs/latest) · [providers.md](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/providers.md) · [mcp.md](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/mcp.md) · [rpc.md](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/rpc.md) · [models.md](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/models.md) · [containerization.md](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/containerization.md)

- TypeScript monorepo, ~113.8k stars, MIT, Node 22.19+. Packages: `pi-ai` (unified multi-provider LLM API), `pi-agent-core` (agent runtime), `pi-coding-agent` (CLI), `pi-tui`, `pi-durable`, `chord`, `pi-telemetry`.
- Deliberately minimal: "ships with powerful defaults but **skips features like sub-agents and plan mode**" — you add them via extensions/packages.
- **Embedding surfaces** (critical for NAVI): interactive TUI, print/JSON modes, **RPC mode** (`pi --mode rpc` — JSONL stdin/stdout subprocess protocol, language-agnostic), and **TypeScript SDK** (`@earendil-works/pi-*` packages; OpenClaw embeds it in-process — see §4).
- **Subscription auth**: `/login` → OAuth device/browser flows for subscription providers (ChatGPT/Codex, Claude Pro/Max, GitHub Copilot, etc.) + API keys + `!command` key resolution + ambient cloud creds.
- **MCP**: first-class — stdio + streamable HTTP, OAuth for remote servers, per-tool exposure controls (`direct`/`codemode`/`deferred`/`hidden`), `mcp.json` user+project files. Exactly what NAVI needs for a Kali MCP server.
- **Local models**: direct **llama.cpp router** integration (`/llama`, GGUF on-demand loading) + OpenAI/Anthropic/Google-compatible endpoints via `models.json` (Ollama/vLLM/SGLang). **Classifier models** are a built-in primitive (TypeSafe Jev, Cloudflare Clef, OpenAI GPT-6 Luna via Decisions API) — close to NAVI's "local decision model" concept, and llama.cpp chat models can act as classifiers.
- **Sandbox**: none built-in, but documented patterns: Gondolin extension (built-in tools + `!` commands routed into a local Linux micro-VM while auth stays on host), plain Docker, OpenShell policy sandbox.
- Daemon: no built-in session daemon like Prime Agent, but RPC mode is a long-lived subprocess — a NAVI voice daemon can own it trivially.

---

## 3. Codex CLI (openai/codex)

**Sources:** [repo](https://github.com/openai/codex) · [docs](https://developers.openai.com/codex/cli) · [config ref](https://developers.openai.com/codex/local-config) · [Ollama integration](https://docs.ollama.com/integrations/codex.md)

- Rust CLI, ChatGPT subscription sign-in official ("Every ChatGPT plan includes Codex" — Plus works, no API billing).
- `codex exec` headless mode; **`codex app-server`** = JSON-RPC/websocket server surface for programmatic control (daemon-able).
- MCP servers via `config.toml` (`[mcp_servers.*]`), plus it can *be* an MCP server (`codex mcp`).
- Sandboxing is its strength: macOS Seatbelt / **Linux Landlock + seccomp**, read-only / workspace-write / danger modes — the only candidate with a real built-in sandbox.
- Local models: `--oss` flag / `model_providers` in config.toml → Ollama/LM Studio/any OpenAI-compatible endpoint. So a local executor model is possible, but the *whole* agent then runs on that provider per session (no built-in supervisor/executor split).
- Locks you to OpenAI models for the smart tier (Claude/Gemini supervisor impossible).

---

## 4. Harness landscape comparison

| Harness | Lang | License / maturity | Sub-auth (ChatGPT/Claude/Gemini acct) | Local model as sub-component | MCP client | Daemon/headless | Sandbox | NAVI fit |
|---|---|---|---|---|---|---|---|---|
| **Pi** | TS | MIT · ~114k★ · very active | ✅ OAuth subs + keys | ✅ llama.cpp router, models.json endpoints, classifier models | ✅ first-class | ✅ RPC/SKD embed, print/JSON | ⚠️ external (Gondolin micro-VM, Docker, OpenShell) | ★★★★★ proven: OpenClaw embeds it + has voice wake |
| **Prime Agent** | Rust | MIT · ~21.7k★ · **beta, 2 mo old** | ✅ `/login` subscription or key | ⚠️ custom endpoints (undocumented post-Rust-rewrite) | ✅ (changelog + docs evidence) | ✅✅ native daemon, heartbeats, schedules, autonomous mode | ❌ (README warns explicitly) | ★★★★ best autonomy loop; young; embeds poorly (no SDK) |
| **Codex CLI** | Rust | Apache-2.0 · huge, OpenAI-maintained | ✅ ChatGPT Plus native | ⚠️ `--oss`/custom providers — whole-agent swap, not tiered | ✅ | ✅ `exec`, `app-server` JSON-RPC | ✅✅ Landlock/seccomp built-in | ★★★ great if ChatGPT-only supervisor is acceptable |
| **Goose (Block→AAIF)** | Rust | Apache-2.0 · mature | ✅ via **ACP providers** — "use your subscriptions for codex, claude and gemini" (Mar 2026 blog); native ChatGPT Codex provider needs Plus/Pro | ✅ Ollama/OpenAI-compatible | ✅ "extensions" are MCP | ✅ `goose run` headless, recipes, scheduler | ⚠️ partial | ★★★ recipes/scheduling fit a voice daemon; heavier than Pi |
| **OpenHands** | Python | MIT · beta platform, ACP-compat | ❌ API keys | ✅ LiteLLM-based → local OK | ✅ | ✅ agent-server (REST, Docker workspaces) | ✅✅ Docker runtime isolation core design | ★★★ strongest sandbox story; heavyweight web platform |
| **Claude Agent SDK** | TS/Py | wraps Claude Code | ✅ Pro/Max OAuth (Jun 2026: subscription SDK use draws from separate monthly credit) | ❌ Claude only | ✅ | ✅ library | ⚠️ | ★★ Claude-locked |
| **OpenAI Agents SDK** | Py/TS | MIT | ❌ API key only | ⚠️ via LiteLLM/custom clients | ✅ | library | ❌ | ★★ thin SDK, you'd build the harness |
| **smolagents (HF)** | Python | Apache-2.0 · ~29.6k★ | ❌ API keys | ✅ LiteLLMModel/TransformersModel → easy local sub-model | ✅ MCP client+server | library | ⚠️ E2B/Docker executor options for generated code | ★★★ best library for NAVI's *local executor* agents, not the supervisor |
| **LangGraph** | Py/TS | MIT · mature | ❌ | ✅ any provider incl. local | ✅ (langchain-mcp-adapters) | library + LangGraph Server | ❌ | ★★ full control, you build everything |
| **CrewAI** | Python | MIT · ~28k★ | ❌ | ✅ LiteLLM | ✅ | library | ❌ | ★★ role-teams don't map to NAVI's arch |
| **AutoGen/AG2** | Python | MIT/CC-BY | ❌ | ✅ | ✅ | library | ⚠️ code-exec containers | ★ **AutoGen in maintenance mode** (Oct 2025); AG2 mid-v1.0 rewrite — avoid |
| **Microsoft Agent Framework** | Py/.NET | MIT · 1.0 GA | ❌ | ✅ | ✅ | library | ❌ | ★★ successor to AutoGen; early |
| **OpenClaw** | TS | MIT · very popular personal-agent gateway | ✅ API keys + some subs | ✅ | ✅ | ✅✅ **designed as always-on daemon** (WhatsApp/TG/Slack gateways) + native **voice wake** feature | ⚠️ | ★★★★ reference architecture — literally embeds Pi SDK; study it even if not adopted |
| Aider | Python | Apache-2.0 | ❌ | ✅ | limited | CLI | ❌ | ★ coding-tool not harness |
| Gemini CLI | TS | Apache-2.0 | ✅ Google acct OAuth | ❌ | ✅ | headless | ⚠️ sandbox opt | ★★ Gemini-only supervisor |

---

## 5. Specific evaluations (NAVI checklist)

**Subscription (non-API-billing) auth, officially:**
- Pi ✅, Prime Agent ✅, Codex CLI ✅ (ChatGPT), Goose ✅ (via ACP passthrough to Codex/Claude/Gemini CLIs), Claude Agent SDK ✅ (Claude), Gemini CLI ✅ (Google). All Python frameworks (smolagents/LangGraph/CrewAI/AG2/OpenAI Agents SDK) ❌ API keys only.

**Local model as subordinate executor (NAVI's Laya/Tev1/NanoJev/ShowUI tier):**
- Pi: best story — llama.cpp router + `models.json` compatible endpoints + built-in classifier-model primitive.
- smolagents: best *library* for building small-model executor agents (CodeAgent + LiteLLMModel→vLLM).
- Codex/Goose: whole-agent provider swap only — can't tier supervisor↔executor natively.
- Nobody offers a built-in "cloud supervisor → small local executor" hierarchy; NAVI builds this either as Pi extension/skills + local OpenAI-compatible endpoints, or as smolagents sub-processes exposed to Pi via MCP.

**MCP servers as tools (Kali MCP):** Pi ✅ (most mature config surface), Prime Agent ✅, Codex ✅, Goose ✅, OpenHands ✅, Claude Agent SDK ✅, smolagents ✅.

**Background daemon triggered by voice listener:**
- Prime Agent: native daemon (`prime-agent status/attach`, sessions persist across detach, `schedule`/heartbeats) — closest to "always-on" out of the box.
- OpenClaw: literally is this pattern (long-lived gateway + `voicewake` nodes) and runs on Pi.
- Pi: `pi --mode rpc --no-session` long-lived subprocess — NAVI's wake-word service spawns/owns it.
- Codex: `app-server` JSON-RPC.
- Goose: `goose run`/schedules.

**Computer-use / desktop control:** none of the harnesses ship it. NAVI needs a custom executor — e.g. ShowUI-2B served behind an OpenAI-compatible endpoint or wrapped as an MCP server with pyautogui/ydotool backends (Wayland note: ydotool works; xdotool doesn't).

---

## 6. Verdict

**Prime Agent vs Pi vs Codex CLI for NAVI:**

- **Pi is the recommended harness.** It is the only candidate combining: embeddable SDK + RPC (voice daemon owns the process), subscription OAuth for ChatGPT Plus *and* Claude (supervisor on existing subs), first-class MCP (Kali MCP), direct llama.cpp + custom-endpoint support (local executor models), and documented containerization patterns (Gondolin micro-VM). Its "no built-in subagents" minimalism is offset by the fact that **OpenClaw already demonstrates the exact architecture NAVI wants** — an always-on personal-agent gateway with voice wake embedding Pi's SDK. NAVI can study/reuse that pattern or even fork the approach.
- **Prime Agent is the best *autonomy engine*, not the best embeddable harness.** Its native daemon, persistent subagents, heartbeats/schedules, `/goal` + `/autonomous` + quality gates, and `/refine` self-improvement are genuinely ahead of Pi for long-horizon unattended work — and it inherits Pi's subscription logins. **But** it is ~2 months old, beta-versioned, mid-rewrite (TS→Rust), has no SDK/RPC embedding surface like Pi's, explicitly provides no sandbox, and its self-modifying harness state is a complexity/security surface NAVI doesn't need for voice-command dispatch. Good as a **delegated deep-work backend** behind Pi (Pi calls Prime Agent via ACP/CLI for hard tasks), not as the core.
- **Codex CLI is the pragmatic fallback** if the user's only spend is ChatGPT Plus: free-ish frontier supervisor, real Landlock sandbox, `app-server` daemon surface. Deal-breakers: OpenAI-only supervisor, no tiered local-executor concept, MCP config less flexible, and extending it means working around OpenAI's opinions rather than building NAVI's own.

**Recommended architecture:** Pi SDK/RPC as NAVI core → supervisor on ChatGPT/Claude subscription via `/login` OAuth → local models (Laya/Tev1/ShowUI) via llama.cpp router or `models.json` endpoints → MCP servers for Kali VM, browser, desktop executors → Gondolin/Docker sandbox for untrusted execution → optional Prime Agent invocation for long-horizon tasks. smolagents is the fallback library if NAVI's executor agents need bespoke Python loops Pi can't express.

**Key risks/blockers:**
1. Prime Agent's post-Rust-rewrite docs lag the code; MCP/provider specifics must be smoke-tested, not assumed from the Pi-lineage docs.
2. Subscription-auth for agent harnesses lives in a ToS gray zone and providers change terms (cf. Anthropic's June 2026 Agent-SDK-credit split); keep an API-key path configurable.
3. No harness gives desktop computer-use; the ShowUI-2B + input-injection executor is NAVI-built work regardless of choice.
4. Pi's RPC/SDK is Node/TS-centric — NAVI's voice stack (whisper.cpp/Piper, likely Python/C++) talks to it over JSONL fine, but in-process embedding requires Node.
