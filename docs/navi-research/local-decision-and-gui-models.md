# NAVI Research — Local Decision Router + GUI Vision/Grounding Models

Date: 2026-10-10. Scope: replace the original plan's "Jev-like" tier (Laya 322M / Tev1 0.8B / NanoJev 600M) and ShowUI-2B with current models that fit NAVI hardware (RTX 4050 6GB VRAM, 24GB RAM, Ryzen 5 8645HS CPU). Voice stack already decided — out of scope.

Method note: Serper web-search was unavailable this session; facts verified via Hugging Face API + model cards and benchmark tables quoted in them (OSWorld/-Verified, ScreenSpot-Pro/-V2, OSWorld-G, UI-Vision, BFCL-v3, WebVoyager). Numbers are vendor-reported unless flagged.

---

## A. FAST LOCAL DECISION / ROUTER (CPU-resident, <2GB, sub-100ms)

Job: classify intent -> pick action from NAVI's structured action set -> emit reliable JSON -> decide escalate-to-cloud vs execute-locally. This is a trained classifier/router, NOT a reasoner (reasoning is the cloud supervisor's job).

### Ranked candidates

| # | Model | Params | License | Tool/FC evidence | Footprint | Notes |
|---|-------|--------|---------|------------------|-----------|-------|
| 1 | google/functiongemma-270m-it | 0.27B | Gemma Terms of Use | Purpose-built function-calling model; trained to emit structured calls in a "mobile actions" JSON schema, officially designed to be fine-tuned on YOUR function set | ~300-500MB, >100 tok/s CPU -> true sub-100ms | Right architecture for an always-on router: fine-tune once on NAVI's action schema (~100-500 examples) -> near-deterministic parser. https://huggingface.co/google/functiongemma-270m-it |
| 2 | LiquidAI/LFM2.5-1.2B-Instruct | 1.2B | LFM Open License v1.0 (permissive under ~$10M rev) | Native function-calling special tokens; trained for tool use + JSON; Japanese edge-AI pedigree | ~800MB Q4 -> fast CPU | Best general-purpose tiny instruct: good fallback for escalation phrasing / lightweight reformatting. https://huggingface.co/LiquidAI/LFM2.5-1.2B-Instruct |

| 3 | Qwen/Qwen3-0.6B | 0.6B | Apache-2.0 | Function-calling via chat template; huge ecosystem; hybrid thinking/non-thinking switch | ~400MB Q4 | Weakest FC of the three but smallest permissive option. https://huggingface.co/Qwen/Qwen3-0.6B |
| 4 | ibm-granite/granite-4.1-3b | 3B | Apache-2.0 | Card explicitly lists "enhanced tool calling" after post-training; 12 languages incl. Arabic | ~2GB Q4 | Best multilingual (Arabic!) small instruct — matches NAVI's Egyptian-Arabic surface. https://huggingface.co/ibm-granite/granite-4.1-3b |
| 5 | Qwen/Qwen3-4B-Instruct-2507 | 4.0B | Apache-2.0 | BFCL-v3 61.9 (vendor card) — strongest <8B FC score verified | ~2.6GB Q4 | If latency budget allows ~1-2s, doubles as a local "mini-supervisor" when the cloud is unreachable. |

Not recommended: Salesforce xLAM-2-8b (CC-BY-NC), watt-tool-8B (8B, 2024), Hammer-3b (low adoption), SmolLM3-3B (FC is not its strength), Llama-3.2-1B/3B (weak FC + Meta license).

### Router pick
**FunctionGemma-270M-IT, fine-tuned on NAVI's action schema**, as the always-on CPU router; **LFM2.5-1.2B-Instruct** as its backup / natural-language layer; keep **Qwen3-4B-Instruct-2507** (Q4, ~2.6GB) on disk as the offline fallback supervisor. All three together <4GB RAM.

---

## B. GUI VISION / UI-GROUNDING MODEL (GPU, on-demand)

Job: screenshot -> action + pixel coordinates. Loaded on demand into the 6GB card alongside (or in place of) the ASR/TTS stack.

### Verified benchmark table (all vendor-reported)

| Model | Params | License | ScreenSpot-Pro | OSWorld-G | OSWorld-V (steps) | Notes |
|-------|--------|---------|----------------|-----------|--------------------|-------|
| inclusionAI/UI-Venus-2-9B | 9B | Apache-2.0 (no field on card; 1.5 family is Apache) | **73.0** | 78.5 (OSWorld-G-R) | **70.8** | Sept 2026, built on Qwen3.5-9B; also WebVoyager 90.8, OSHarm ASR 11.3% |
| inclusionAI/UI-Venus-1.5-8B | 8B | Apache-2.0 | 68.4 | — | — | Feb 2026, Qwen3-VL-8B base; ScreenSpot-V2 ~96 |
| inclusionAI/UI-Venus-1.5-2B | 2B | Apache-2.0 | 57.7 | — | — | Same tech report; fits 6GB at bf16 (~4.5GB) |
| xlangai/OpenCUA-7B | 7B | MIT | 50.0 | 55.3 | 27.9 @50 | Qwen2.5-VL base; end-to-end agent (planning+grounding+reflection); vLLM-supported |
| Salesforce/GTA1-7B-2507 | 7B | Apache-2.0 | 55.5* | 60.1* | 45.2 w/ o3 planner | *GTA1 (earlier) numbers; 2507 rel. lower (50.1/55.1). Grounding-only model — needs external planner |
| ByteDance-Seed/UI-TARS-1.5-7B | 7B | Apache-2.0 | 61.6 | — | 27.4 @100 | Apr 2025; ScreenSpot-V2 94.2; end-to-end |
| ByteDance/UI-TARS-2B-SFT | 2B | Apache-2.0 | ~mid-30s | — | weak | Original UI-TARS; superseded |
| Qwen/Qwen3-VL-4B-Instruct | 4B | Apache-2.0 | — | — | — | General VLM w/ GUI grounding support in card; no ScreenSpot number published |
| OS-Copilot/OS-Atlas-Base-4B | 4B | Apache-2.0 | low | — | — | Nov 2024; superseded by Venus/OpenCUA |
| showlab/ShowUI-2B | 2B | MIT | ~40s | — | — | Original plan's pick; clearly beaten now |

### GUI pick
**Primary: UI-Venus-2-9B** — by far the strongest sub-10B GUI agent verified (OSWorld-V 70.8, ScreenSpot-Pro 73.0, beats Qwen-UI-Agent-27B and OpenCUA-32B). Caveat: 9B bf16 needs ~18GB — must run **Q4/Q5 GGUF (~6-7GB) in llama.cpp with partial CPU offload**, or FP8 (~9GB) won't fit. Feasible on 6GB VRAM + 24GB RAM at usable-but-not-fast speed (~5-15 tok/s est.). GGUF builds exist (bartowski, mradermacher). Verify its license on the card before shipping (1.5 family is Apache-2.0; the 2.0 card lacks an explicit field).

**Fallback if Venus-2-9B is too heavy/slow: OpenCUA-7B (MIT)** — cleanest license in the class, native vLLM support, OSWorld-V 27.9 @50 steps, ScreenSpot-Pro 50.0; Q4 ~5GB fits 6GB directly, or exl2 build. **Small alternative: UI-Venus-1.5-2B** — bf16 ~4.5GB, ScreenSpot-Pro 57.7 (beats OpenCUA-7B and UI-TARS-1.5-7B on that metric), best pure-per-GB grounding; pair with the router for planning.

ShowUI-2B (original pick): no longer defensible — UI-Venus-1.5-2B beats it at the same size with a current (2026) tech report.

---

## C. HOW IT PLUGS INTO PRIME AGENT

Prime Agent's only model tool is a persistent IPython kernel — local models are just Python callables:

- Serve **UI-Venus-2-9B** (or OpenCUA-7B) via `llama.cpp` server (`--jinja` chat template, OpenAI-compatible `/v1/chat/completions`) or vLLM; call it from a registered **Python skill** (`screenshot -> action JSON`) in the REPL.
- **FunctionGemma-270M** runs in-process via llama-cpp-python or a second llama.cpp instance on CPU — call it directly in the routing skill; no GPU needed.
- Wrap each as a `SKILL.md` + Python callable (`navi_route(text)->json`, `navi_see(png_path)->action`); the agent calls them like any other programmatic tool, and a supervisor subagent (`rlm.spawn`) can own the heavy GUI loop.
- Escalation path: router skill returns `{escalate:true}` -> REPL hands the task to the cloud-supervised Prime Agent planner.

---

## Risks / unverified
- **UI-Venus-2-9B license** not stated on the HF card (1.5 line is Apache-2.0; verify before shipping).
- **FunctionGemma** Gemma license has use-restriction clauses (not Apache); check acceptable-use for NAVI's context.
- All GUI benchmark numbers are vendor-reported; UI-Venus-2's own card notes OSWorld-V comparisons "may use model-specific action scaffolds."
- 9B-at-Q4-on-6GB latency is an estimate — needs a real measurement on the 4050.
- Serper was down; no independent third-party eval (e.g. latest BFCL ranking for the small models) was reachable — BFCL-v3 61.9 for Qwen3-4B-Instruct-2507 is from Qwen's own card.
