# NAVI Voice Stack Research — Local Arabic + English Voice Pipeline

**Date:** 2026-10-10 · **Scope:** Always-on, fully-local voice stack for "Hey NAVI" assistant.
**Requirements:** Egyptian Arabic + English code-switching STT, custom "Hey NAVI" wake phrase, tiny always-on footprint (Ryzen 5 8645HS CPU / RTX 4050 6GB VRAM / 24 GB RAM, Linux Wayland), low-latency TTS with Arabic, barge-in, no cloud STT/TTS.

**Bottom line:** Rejecting whisper.cpp is right — Whisper is *terrible* at Egyptian Arabic/code-switching (63–74% WER in the best Egyptian benchmark below). The current plan misses a 2025–2026 wave of better options: **Qwen3-ASR** (Apache-2.0, Arabic + streaming), its Egyptian fine-tune **QwenCleo-ASR**, **NVIDIA Nemotron 3.5 ASR** (0.6B, *native streaming*, Arabic "transcription-ready"), **SILMA TTS** (Apache-2.0 bilingual ar/en), **Chatterbox Multilingual** (MIT, ar in 23 langs), **Piper** (still the smallest Arabic-capable footprint), and **sherpa-onnx open-vocabulary KWS** (custom "Hey NAVI" *with zero training*). VAD: **TEN-VAD** or **Silero**.

---

## 1. STT — Speech-to-Text

### 1.1 Head-to-head comparison

| Model | Size/params | Arabic | Egyptian + code-switching | Streaming | License | Runtime fit | Notes |
|---|---|---|---|---|---|---|---|
| **QwenCleo-ASR** ⭐ | 1.7B (~3.4 GB bf16; ~2 GB Q4) | Yes (tuned) | **Best measured: 19.85% WER** on Egyptian+CS test set (3,699 utts) | Token-streaming via vLLM | Apache-2.0 | GPU: fine on 6 GB; CPU: too slow as decoder-LLM (~1.5B tokens/s limits) | Fine-tune of Qwen3-ASR-1.7B on Egyptian podcasts; keeps English terms in Latin script. Released ~2026, only 16★ — young, but beats everything tested below. [GitHub](https://github.com/MohammedAly22/QwenCleo-ASR) |
| **Qwen3-ASR-1.7B** ⭐ | ~2B (encoder + Qwen3 LLM) | Yes (`ar` listed) | ~41.5% WER on Egyptian+CS (2× worse than QwenCleo) | Yes — streaming + offline in one model (vLLM) | Apache-2.0 | GPU fp16 ~5 GB; Q4 ~2 GB | Alibaba, released **Jan 30, 2026**. 52 languages/dialects incl. Arabic. First-class streaming. [HF](https://huggingface.co/Qwen/Qwen3-ASR-1.7B) · [report](https://arxiv.org/abs/2601.21337) |
| **Qwen3-ASR-0.6B** | ~0.6B | Yes (`ar`) | Vendor claims "accuracy-efficiency trade-off"; no Egyptian-specific numbers. GGUF exists (`memoravox/Qwen3-ASR-0.6B-gguf`) | Yes (vLLM) | Apache-2.0 | GPU ~1.5–3 GB; GGUF on CPU plausible | Same release; community Arabic-dialectal fine-tune exists (`oddadmix/qwen3-asr-0.6b-arabic-dialectal`). [HF](https://huggingface.co/Qwen/Qwen3-ASR-0.6B) |
| **NVIDIA Nemotron 3.5 ASR** | 0.6B (FastConformer-RNNT) | Yes — `ar-AR` in **"Transcription-ready" tier** (19 locales) | Measured 38% WER on Egyptian+CS set (worse than QwenCleo but ≈2× better than Whisper v3) | **Native streaming** (cache-aware, 80 ms–1120 ms chunks) — best streaming architecture here | OpenMDW-1.1 (permissive-ish, check terms) | 0.6B, NIM/NeMo, GGUF exists → CPU/GPU both fine | Released **Jun 2026**, 1.2M downloads, NVIDIA-maintained. Real production-grade streaming. [HF](https://huggingface.co/nvidia/nemotron-3.5-asr-streaming-0.6b) |
| **Whisper large-v3-turbo** (faster-whisper / whisper.cpp) | 0.81B (~1.5 GB int8) | Yes (99 langs) | **Bad**: 73.8% WER on Egyptian+CS (base); Arabic-dialectal FT variant ~50.8% | Chunked only (30 s window) | MIT | whisper.cpp ~0.25–1× RTF CPU; faster-whisper int8 ~real-time | User is right to reject: weakest Egyptian/CS handling of any serious option. [HF](https://huggingface.co/openai/whisper-large-v3-turbo) · [QwenCleo bench](https://github.com/MohammedAly22/QwenCleo-ASR) |
| **Whisper large-v3** | 1.54B | Yes | 63.9% WER Egyptian+CS | No | MIT | GPU preferred | Worse than turbo-FT and everything above. |
| **NVIDIA Parakeet-TDT-0.6B-v3** | 0.6B | **No** — 25 *European* languages only | — | No (offline) | CC-BY-4.0 (weights) | Excellent CPU ONNX port exists | Fast + accurate but useless for Arabic. [HF](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3) |
| **NVIDIA Canary-1B-v2** | 1B | **No** — 25 European languages | — | No | CC-BY-4.0 | GPU/CPU ok | Same limitation as Parakeet v3. [HF](https://huggingface.co/nvidia/canary-1b-v2) |
| **Fun-ASR-MLT-Nano-2512** | 0.8B (SenseVoice encoder + Qwen3-0.6B decoder) | **Yes** — `ar` in 31-lang list | Arabic included but "emphasis on East/SE Asian languages"; no Egyptian WER published | Streaming-capable toolkit | Apache-2.0 | GPU preferred; `funasr` toolkit | Interesting multilingual alt; Arabic likely MSA-leaning. [HF](https://huggingface.co/FunAudioLLM/Fun-ASR-MLT-Nano-2512) |
| **GLM-ASR-Nano-2512** (Zhipu) | 1.5B | **No confirmed** — 17 langs, focus zh/en/Cantonese | — | No (offline; vLLM support planned) | MIT | GPU | Lowest avg WER of open ASR (4.10) but Arabic not advertised. [GitHub](https://github.com/zai-org/GLM-ASR) · [card](https://github.com/wildminder/awesome-ai-voice/blob/main/models/glm-asr-nano.md) |
| **SenseVoice-Small** (FunASR) | ~234M | **No** — zh/en/ja/ko/yue (+LID for a few more); Arabic not in supported set | — | Streaming via FunASR | MIT-ish (check per-model) | Tiny, great CPU RTF | Popular but **not Arabic**. [GitHub](https://github.com/FunAudioLLM/SenseVoice) |
| **Kyutai STT** (`stt-1b-en_fr`, `stt-2.6b-en`) | 1B / 2.6B | **No** — English/French only | — | Yes (delayed-streams, word timestamps) | CC-BY-4.0 | CPU/GPU ok | Beautiful streaming design but no Arabic. [kyutai.org/stt](https://kyutai.org/stt) |
| **Vosk Arabic** (`vosk-model-ar-0.22-linto-1.1.0`) | ~1.3 GiB | Yes (Kaldi, MSA) | Weak — classic acoustic model, no CS | Yes (built-in streaming) | Apache-2.0 | Tiny CPU | Fallback/offline only; quality far below neural models. [Vosk model list](https://alphacephei.com/vosk/models) |
| **distil-whisper / Moonshine** | small | **English only** | — | Moonshine has streaming | MIT | Very fast | Exclude (no Arabic). |
| **wav2vec2 / HuBERT Arabic FTs** | various | Yes | `omarxadel/hubert-large-arabic-egyptian` (Egyptian, CTC) exists | No | varies | CPU ok | Legacy now — Qwen3-ASR/Nemotron beat these by a wide margin. [HF](https://huggingface.co/omarxadel/hubert-large-arabic-egyptian) |

### 1.2 Recommendation

- **Primary: QwenCleo-ASR (Qwen3-ASR-1.7B Egyptian/CS fine-tune).** It's the only model specifically trained for exactly NAVI's use case — Arabic↔English code-switching where English words stay Latin (`YouTube`, `React`, `soft skills`) instead of being mangled into Arabic script. 19.85% WER is *half* the base model's and ~3.5× better than Whisper-turbo on the same set. Apache-2.0. [Benchmark + samples](https://github.com/MohammedAly22/QwenCleo-ASR)
- **Streaming/general fallback: Qwen3-ASR-0.6B or NVIDIA Nemotron 3.5 ASR-0.6B.** If QwenCleo's decoder-LLM latency is too heavy for always-on barge-in, Nemotron 3.5 is the best *native* streamer (cache-aware, 80 ms chunks, `ar-AR` transcription-ready, language-ID tagging built in). Qwen3-ASR-0.6B is lighter than 1.7B and also streams via vLLM. Run on GPU (both fit in 6 GB comfortably at fp16; GGUF/quant for CPU).
- **Always-on wake loop** should *not* run the big ASR — see wake-word section; STT only activates after wake.

**Key risk:** QwenCleo is new (16★, single author) — verify on your own Egyptian-accented mic audio before committing. Its benchmark uses a self-collected test set (932+3,699 clips), which is still better evidence than every other model has for Egyptian CS.

**Sources:** QwenCleo bench table & samples [GitHub](https://github.com/MohammedAly22/QwenCleo-ASR) · Qwen3-ASR release [Alibaba blog](https://www.alibabacloud.com/blog/qwen3-asr-&-qwen3-forcedaligner-is-now-open-sourced-robust-streaming-and-multilingual_602843), [arXiv 2601.21337](https://arxiv.org/abs/2601.21337), [HF card](https://huggingface.co/Qwen/Qwen3-ASR-1.7B) · Nemotron 3.5 [HF card](https://huggingface.co/nvidia/nemotron-3.5-asr-streaming-0.6b) · Parakeet/Canary 25-lang limitation [HF](https://huggingface.co/nvidia/canary-1b-v2), [arxiv 2509.14128](https://arxiv.org/html/2509.14128v2) · Fun-ASR-MLT [HF card](https://huggingface.co/FunAudioLLM/Fun-ASR-MLT-Nano-2512) · GGUF [memoravox](https://huggingface.co/memoravox/Qwen3-ASR-0.6B-gguf).

---

## 2. Wake-word — "Hey NAVI"

| Engine | Custom "Hey NAVI" without training? | Always-on footprint | Arabic-aware | License / cost | Verdict |
|---|---|---|---|---|---|
| **sherpa-onnx KWS** ⭐ | **YES — zero training.** Open-vocabulary keyword spotting: write `▁HE Y ▁N A V I` tokens into `keywords.txt` (use `sherpa-onnx-cli text2token` on a `gigaspeech`/bpe model) and it becomes a tiny ASR that can *only* emit your phrase. Tunable boosting score + trigger threshold per keyword. | ~3.3M-param zipformer, sub-1% CPU | English tokens; can encode "Navi" phonetically | Apache-2.0 | **Best pick**: free, offline, no training, adjustable false-accept rate. [Docs](https://k2-fsa.github.io/sherpa/onnx/kws/index.html) |
| **openWakeWord** | Requires training (~75–90 min on Colab, or via openwakeword.com web trainer with synthetic TTS + your real voice). 20+ langs supported via multilingual TTS synthesis. | ~200 KB ONNX per model, runs fine on 1 CPU core; can hit ~100% of one core on very weak devices (Pi Zero) | Trained via synthetic voices; Arabic TTS (SILMA/Chatterbox ar) could synthesize samples | Apache-2.0 (models MIT) | Most battle-tested; better robustness than sherpa KWS once trained. [Repo](https://github.com/dscripka/openWakeWord) · [2026 trainer](https://github.com/alfiedennen/openwakeword-colab-2026) · [CoreWorxLab Kokoro+real-voice pipeline](https://github.com/CoreWorxLab/openwakeword-training) |
| **Picovoice Porcupine** | Custom `.ppn` trained via Console/Model API in seconds — no data needed | Smallest footprint | **Arabic NOT supported** — only en/zh/fr/de/it/ja/ko/pt/es; other languages "commercial customers only" | Free tier for personal; offline models download; some free-tier changes flagged in issues | Best accuracy/footprint ratio but **Arabic gap + proprietary**. [README](https://github.com/Picovoice/porcupine) · [docs](https://picovoice.ai/docs/model-api/porcupine/) |
| **microWakeWord** | Needs training (TensorFlow, synthetic gen) | Designed for ESP32-S3 microcontrollers | English-centric | Apache-2.0 | Overkill-tiny for a laptop; use only if pushing to embedded. [GitHub](https://github.com/kahrendt/microWakeWord) |
| **Mycroft Precise** | Training required (old TF tooling) | Light | No | Apache-2.0 but **project effectively dead** (Mycroft archived) | Skip. [GitHub](https://github.com/mycroftai/mycroft-precise) |

**Recommendation:**
1. **Start with sherpa-onnx open-vocabulary KWS** — "Hey NAVI" in `keywords.txt` works tonight with zero training; tweak `boosting_score`/`trigger_threshold` to trade false-accepts vs misses.
2. **Harden with openWakeWord** once you have real "Hey NAVI" + Egyptian-accented recordings — its synthetic+real-voice training gives the most reliable always-on detector and it's the community standard (used by Home Assistant/OpenVoiceOS).
3. Porcupine is technically excellent but **excluded** (no Arabic, proprietary, free-tier churn).

---

## 3. VAD

| Model | Size | Speed | Accuracy | License | Notes |
|---|---|---|---|---|---|
| **TEN-VAD** ⭐ | tiny (ONNX) | real-time, frame-level | **Beats Silero & WebRTC on precision** in TEN's tests; independent issue shows better acc but ~2× Silero's time | Apache-2.0 | Newer (2025), enterprise-oriented, from TEN-framework. [GitHub](https://github.com/TEN-framework/ten-vad) · [HF](https://huggingface.co/TEN-framework/ten-vad) |
| **Silero VAD** | ~2 MB ONNX | ~1 ms per 30 ms chunk on 1 CPU thread | Standard, robust, >95% | **MIT** (repo; PyPI badge wrongly shows CC BY-NC — the LICENSE file is MIT) | Industry default, plugs into Pipecat/LiveKit/whisper.cpp. [GitHub](https://github.com/snakers4/silero-vad) · [metrics](https://github.com/snakers4/silero-vad/wiki/Quality-Metrics) |
| **WebRTC VAD** | KB | fastest | Rule-of-thumb, weak in noise (~58% on noisy set) | BSD | Legacy; only as last resort. |
| **pyannote segmentation** | ~few MB | heavier (torch) | Best for overlapped speech/diarization | MIT (model gated on HF) | Overkill for a turn-taking VAD; use for diarization if ever needed. |

**Recommendation:** **Silero VAD** (proven, MIT, 2 MB, everywhere-compatible) — keep **TEN-VAD** as upgrade if Silero misses in noisy Egyptian household audio (better precision per TEN's benchmarks and independent tests). Both trivially swap in any framework.

---

## 4. TTS — Arabic + English, low-latency, local

| Engine | Size | Arabic | Egyptian quality | RTF/latency | License | Notes |
|---|---|---|---|---|---|---|
| **SILMA TTS v1** ⭐ | 150M | **Native bilingual ar+en**, MSA/Fusha + tashkeel | MSA-strong; Egyptian *accent* not guaranteed (Fusha read with Egyptian-ish phonology). Clone an Egyptian voice with <8 s ref audio to bias accent. | **RTF ~0.12** on RTX 4090 (vendor claim); 150M params → plausible ~real-time on 4050, slower on CPU | **Apache-2.0** (fixes F5-TTS's non-commercial problem) | Released **Mar 2026**, purpose-built to fill the Arabic gap. Best license+Arabic combo. [HF blog](https://huggingface.co/blog/silma-ai/opensource-arabic-english-text-to-speech-model) · [HF model](https://huggingface.co/silma-ai/silma-tts) |
| **Chatterbox Multilingual (V3)** ⭐ | 500M | **Yes — `ar` in 23-lang list** | Arabic supported; Egyptian accent depends on voice-clone reference. Community Egyptian fine-tune exists (`AliAbdallah/egyptian-arabic-tts-chatterbox`) | Sub-200 ms cloud; local ~real-time on GPU | **MIT** | Most mature open TTS (26.6k★), benchmarks vs ElevenLabs. No dedicated Egyptian pack but clone-friendly. [HF](https://huggingface.co/ResembleAI/chatterbox) · [GitHub](https://github.com/resemble-ai/chatterbox) |
| **Piper** | 20–63 MB per voice | Yes — **only `ar_JO` (Jordanian) "kareem"** low+medium; **no `ar_EG`** | Jordanian ≈ close-ish Levantine, not Egyptian | **Fastest**: RTF well under 1.0 on a Raspberry Pi 4; sub-100 ms first audio on Ryzen | MIT | Still the right choice for ultra-low-footprint/barge-in-friendly **English** + acceptable Arabic. [VOICES.md](https://github.com/rhasspy/piper/blob/master/VOICES.md) · [piper.ttstool.com](https://piper.ttstool.com/) |
| **KaniTTS-2** | 450M | `ar` in 8-lang list (en,de,fr,zh,ko,ja,ar,es) | Unknown; "optimized primarily for English" per card | ~1 s for 15 s audio on RTX 5080 (GPU-bound) | **lfm1.0 on the `-pt` weights (custom Liquid license); `-ft` builds Apache-2.0** — check per-checkpoint | Promising modular LM+codec design. [HF](https://huggingface.co/nineninesix/kani-tts-450m-0.2-pt) |
| **XTTS-v2 (Coqui)** | ~1.8 GB | 17 langs incl. Arabic | Community `aungmoe/arabic-tts-xtts-v2` improves Arabic | Slow-ish (autoregressive) | **Coqui Public Model License — non-commercial**; Coqui the company is dead, model still usable | Only if you need a known-quantity voice-clone; licensing is the blocker. [HF](https://huggingface.co/coqui/XTTS-v2) |
| **F5-TTS / E2-TTS** | ~300M | Architecture only (zh/en shipped); Arabic needs fine-tune | — | Real-time on GPU | **Code MIT, weights CC-BY-NC-4.0** | SILMA TTS *is* the Arabic F5 — just use that. [GitHub](https://github.com/SWivid/F5-TTS) · [HF](https://huggingface.co/SWivid/F5-TTS) |
| **Kyutai Pocket TTS** | 100M | **No** — 6 languages (en/fr + 4 EU) | — | Real-time on laptop CPU | CC-BY-4.0 | Beautiful tiny CPU TTS, but no Arabic (as of Aug-2026 roadmap note). [kyutai.org](https://kyutai.org/pocket-tts/) |
| **Kokoro-82M** | 82M | **No** — 8 langs, no `ar` | — | Very fast CPU | Apache-2.0 | Great English default if Arabic handled elsewhere. [HF](https://huggingface.co/hexgrad/Kokoro-82M) |
| **Orpheus multilingual** | 3B | **No** — fr/de/es/it/zh/ko/hi only | — | GPU | research release | No Arabic. [HF collection](https://huggingface.co/collections/canopylabs/orpheus-multilingual-research-release) |
| **Sesame CSM-1B** | 1B | **English only** (community asking for `ar`) | — | GPU | Apache-2.0 | Skip. [HF](https://huggingface.co/sesame/csm-1b) |
| **Parler-TTS-mini / Matcha / KittenTTS / MeloTTS** | small | No Arabic (KittenTTS is en-only) | — | Fast CPU | various | Exclude for Arabic. |
| **edge-tts** (Microsoft cloud) | — | Excellent ar-EG voices (free) | **Best Egyptian voices available** | Network-bound | Free but **cloud = privacy leak + needs internet** | Emergency fallback only if local Arabic quality is unacceptable. |

**Recommendation:**
- **Primary Arabic: SILMA TTS v1** (Apache-2.0, purpose-built bilingual ar/en, 150M, tashkeel-aware, voice-clone to bias toward Egyptian) — the single best license+Arabic fit in 2026.
- **Upgrade/quality option: Chatterbox Multilingual V3** (MIT) for more natural, clonable voices; fine-tune path to Egyptian already demonstrated by community.
- **English fast path / always-on acknowledgements: Piper** (`en_US` voice) — millisecond latency, bulletproof, cheap enough to always generate instantly for barge-in-friendly short replies. Use `ar_JO-kareem` for Arabic short replies when SILMA/Chatterbox latency isn't needed.
- **Voice-identity plan:** pick *one* cloned Egyptian speaker across SILMA/Chatterbox so NAVI sounds consistent in both languages.

**Key risk:** No open TTS currently ships a *native* Egyptian voice off the shelf — SILMA is MSA-fluent, Chatterbox `ar` is generic. Egyptian accent comes from **voice cloning** (reference clip) or fine-tuning, both already demonstrated.

---

## 5. Full-duplex / voice-agent frameworks

| Framework | Local models | Barge-in | Transport | Maturity | Fit for NAVI |
|---|---|---|---|---|---|
| **Pipecat** ⭐ | Yes — Whisper/local STT, Piper/local TTS, Silero VAD plugins; frame-based pipeline `VAD → STT → LLM → TTS` | **Yes, first-class** ("interruptions" doc page; per-frame `interruptible`) | WebRTC (SmallWebRTC, Daily, LiveKit transports), WebSocket | 16k★, maintained by Daily, Thoughtworks "Assess" 2026 | **Best fit** — most flexible local pipeline, Python, swap Qwen3-ASR/Nemotron + SILMA easily. [GitHub](https://github.com/pipecat-ai/pipecat) · [interruptions](https://docs.pipecat.ai/pipecat/fundamentals/interruptions) |
| **LiveKit Agents** | Yes — Silero VAD plugin, custom STT/TTS plugins, fully self-hosted LiveKit server possible | Yes (turn detection + interruptions) | WebRTC (own server or cloud) | Very mature, production-grade | Strong alternative; heavier infra (needs a LiveKit SFU) but best WebRTC transport. [docs](https://docs.livekit.io/agents) · [silero plugin](https://docs.livekit.io/agents/logic/turns/vad) · [local plugins](https://github.com/CoreWorxLab/local-livekit-plugins) |
| **TEN-framework** | Yes — pluggable; authors of TEN-VAD | Yes | RTC + WebSocket | 11k★, active 2026 | Good but younger ecosystem than Pipecat/LiveKit. [GitHub](https://github.com/TEN-framework/ten-framework) |
| **FastRTC** | Yes — wraps any Python fn | Basic | WebRTC via Gradio/Cloudflare | HF-backed, lighter | Prototyping/simple apps; less turnkey turn-taking logic than Pipecat. [HF blog](https://huggingface.co/blog/fastrtc) |

**Recommendation: Pipecat** — the frame-based `VAD→STT→LLM→TTS` pipeline maps exactly to NAVI's architecture, interruptions/barge-in are first-class, it runs fully local (WebSocket or WebRTC transport, no cloud dependency), and every component in this report already has or can get a Pipecat service wrapper. Use **LiveKit Agents** only if you later need multi-device remote access (its SFU is the strongest). FastRTC for quick demos.

---

## 6. Proposed NAVI voice stack (revised)

| Component | Current plan | **Recommended** |
|---|---|---|
| Wake word | Sherpa-ONNX / openWakeWord | **sherpa-onnx open-vocab KWS** (zero-training "Hey NAVI") → upgrade to **openWakeWord** trained model |
| VAD | Silero | **Silero VAD** (MIT, 2 MB); TEN-VAD if more precision needed |
| STT | Whisper.cpp ❌ | **QwenCleo-ASR** (1.7B, Apache-2.0, best Egyptian+CS) on GPU; streaming fallback **Nemotron-3.5-ASR-0.6B** (native streaming, `ar` transcription-ready) or **Qwen3-ASR-0.6B** |
| TTS | Piper only | **Piper** for ultra-fast English/short replies + **SILMA TTS v1** (ar/en bilingual, Apache-2.0) for Arabic — optional **Chatterbox V3** for cloned Egyptian voice quality |
| Framework | hand-rolled | **Pipecat** (local pipeline + native barge-in) |

**VRAM check:** RTX 4050 6 GB holds QwenCleo-1.7B (≈3.4 GB fp16 / ~2 GB Q4) + SILMA-150M + Piper + VAD + KWS with room to spare; Nemotron-0.6B is even lighter. Always-on loop (KWS + Silero VAD + Piper) is ~100 MB RAM and <2% CPU.

**Open verification items (do before committing):**
1. Run QwenCleo-ASR vs Nemotron-3.5 on your own Egyptian mic recordings — pick per measured accuracy/latency.
2. Test sherpa-onnx KWS false-accept rate for "Hey NAVI" in Arabic-accented speech; train openWakeWord if needed.
3. Audition SILMA `ar` vs Chatterbox `ar` + Egyptian reference clip for NAVI's voice.
