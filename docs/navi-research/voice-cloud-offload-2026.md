# NAVI Voice — Zero-Cost Cloud Offload + Local Fallback (Oct 2026)

**Date:** 2026-10-31 · **Target:** Fedora, Python 3.12/3.14 workers, always-on assistant.
**Goal:** cheapest possible (free tier) STT + TTS with a local fallback that still works offline.

All limits below were read from official docs on 2026-10-31. Anything marked *(est.)* is a measured-elsewhere or reasoned estimate, not vendor copy.

---

## 1. Recommendation

**Bottom line:** use **Groq `whisper-large-v3-turbo`** for cloud STT and **`edge-tts`** for cloud TTS. Both are free, fast, and need no billing account. Keep **Cloudflare Workers AI** as the second cloud hop (it has the most generous free audio allowance) and **faster-whisper base int8 + Piper** as the always-available local floor.

| Role | Primary | Why |
|---|---|---|
| Cloud STT | Groq `whisper-large-v3-turbo` | 216× realtime claimed, OpenAI-compatible multipart, 28,800 audio-sec/day (8 h) free, 20 RPM |
| Cloud STT #2 | Cloudflare `@cf/openai/whisper-large-v3-turbo` | ~214 audio-min/day free from 10k neurons, no per-minute request anxiety, same model |
| Cloud STT #3 | Gemini `gemini-3.5-transcribe` | Best features (diarization, vocabulary bias, `smart` mode) but RPM/RPD is not published |
| Cloud TTS | `edge-tts` 7.3.1 | Free, 300+ neural voices incl. `ar-EG-*`, MP3 24 kHz, ~0.4–1.0 s per sentence |
| Cloud TTS #2 | Cloudflare `@cf/myshell-ai/melotts` | ~536 audio-min/day free, MP3 out, tiny neuron cost |
| Cloud TTS #3 | Gemini `gemini-3.8-flash-lite-tts` | Free tier, WAV 24 kHz, very natural, but unpublished RPD and no barge-in latency guarantee |
| Local STT | `faster-whisper` base.en int8 (tiny.en on weak CPUs) | ~150 MB model, ~0.6 GB RSS, RTF ~0.15–0.25 on 8 threads |
| Local TTS | Piper (`piper-tts` 1.8.0) | ~60 MB voice, RTF ~0.02–0.05, lowest latency of anything here |
| Wake word | openWakeWord or sherpa-onnx KWS | Porcupine's free tier is gone (see §5) |

### Ordered fallback chain (one voice turn)

```
STT:  Groq turbo → Cloudflare turbo → Gemini 3.5-transcribe → faster-whisper base int8
TTS:  edge-tts   → Cloudflare melotts → Gemini 3.8-flash-lite-tts → Piper local
```

Rules that make the chain safe:

1. **2 s budget per cloud hop**, one retry on 429/5xx/timeout, then fall through. Total STT budget ~6 s before local.
2. **Health memory:** a provider that fails twice in 5 min is skipped for 5 min (avoids dead-air on a rate-limited key).
3. **Local is the floor, never the first choice** on this hardware — but it must be warm (model loaded at boot) so the fallback costs ~1 s, not ~5 s.
4. **Do not chain TTS by re-synthesizing the whole reply.** Buffer per sentence so a mid-reply failure only loses one sentence.

### Arabic / code-switching note

Groq and Cloudflare Whisper handle MSA acceptably but are weak on Egyptian Arabic + English code-switching (see `voice-stack.md`: ~74% WER). For Arabic turns, prefer `gemini-3.5-transcribe` with `language_codes: ["ar-EG"]` + `custom_vocabulary`, and use `edge-tts` voice `ar-EG-SalmaNeural` / `ar-EG-ShakirNeural`. Kokoro and Piper's English voices are English-only.

---

## 2. Provider matrix

| Provider / model | Endpoint | Auth header | Request → response | Free limits (official) |
|---|---|---|---|---|
| **Groq STT** `whisper-large-v3-turbo` | `POST https://api.groq.com/openai/v1/audio/transcriptions` | `Authorization: Bearer $GROQ_API_KEY` | `multipart/form-data`: `file`, `model`, `language`, `response_format`, `temperature` → `{"text": "..."}` | 20 RPM · 2,000 RPD · 7,200 audio-sec/hour · 28,800 audio-sec/day · 25 MB/file |
| **Groq TTS** `canopylabs/orpheus-v1-english` | `POST https://api.groq.com/openai/v1/audio/speech` | `Authorization: Bearer $GROQ_API_KEY` | JSON `{model, input, voice, response_format:"wav"}` → `audio/wav` bytes | 10 RPM · 100 RPD · 1.2K TPM · 3.6K TPD · **max 200 chars/request**, WAV only |
| **Gemini STT** `gemini-3.5-transcribe` | `POST https://generativelanguage.googleapis.com/v1beta/interactions` | `x-goog-api-key: $GEMINI_API_KEY` | JSON, audio as **base64 inline** or Files-API `uri` → `steps[].content[].text` | Free tier: "Free of charge"; RPM/RPD per model only visible in AI Studio (not published) |
| **Gemini TTS** `gemini-3.8-flash-lite-tts` (`-flash-tts` for quality) | same `/v1beta/interactions` | `x-goog-api-key` | JSON `{model, input, response_format:{type:"audio",mime_type:"audio/wav"}, generation_config:{speech_config:[{voice}]}}` → base64 audio in `steps[].content[].data` | Free tier: "Free of charge"; WAV 24 kHz mono 16-bit unary, raw L16 when `stream:true` |
| **Cloudflare STT** `@cf/openai/whisper-large-v3-turbo` | `POST https://api.cloudflare.com/client/v4/accounts/{ACCOUNT_ID}/ai/run/@cf/openai/whisper-large-v3-turbo` | `Authorization: Bearer $CF_API_TOKEN` | raw audio body + `Content-Type: audio/wav` → `{"result":{"text":"..."},"success":true}` | 10,000 neurons/day; 46.63 neurons/audio-min → **~214 min/day** |
| **Cloudflare TTS** `@cf/myshell-ai/melotts` | `.../ai/run/@cf/myshell-ai/melotts` | `Authorization: Bearer $CF_API_TOKEN` | JSON `{prompt, lang}` → MP3 (base64 in JSON, or raw) | 18.63 neurons/audio-min → **~536 min/day** |
| **Cloudflare TTS** `@cf/deepgram/aura-1` | `.../ai/run/@cf/deepgram/aura-1` | `Authorization: Bearer $CF_API_TOKEN` | JSON `{text}` → audio bytes | 1,363.64 neurons/1k chars → only **~7.3k chars/day** free — too small |
| **Deepgram STT** `nova-3` | `POST https://api.deepgram.com/v1/listen?model=nova-3&smart_format=true` | `Authorization: Token $DEEPGRAM_API_KEY` | raw audio body + `Content-Type: audio/wav` → `results.channels[0].alternatives[0].transcript` | **$200 one-time credit**, no expiry (~46k min pre-recorded at $0.0043/min) |
| **AssemblyAI STT** `universal-2` | `POST https://api.assemblyai.com/v2/upload` then `/v2/transcript` | `authorization: $ASSEMBLYAI_API_KEY` | upload raw bytes → `upload_url`; JSON `{audio_url}` → poll `GET /v2/transcript/{id}` → `.text` | **$50 one-time credit** ≈ 185 h pre-recorded / 333 h streaming |
| **edge-tts** (Edge read-aloud) | `wss://speech.platform.bing.com/consumer/speech/synthesize/readaloud/edge/v1` (library handles it) | none (TrustedClientToken embedded) | text → MP3 (24 kHz, 48 kbps) + optional SRT word boundaries | No published quota; unofficial use of a consumer service, no SLA |

---

## 3. Copy-paste examples (stdlib `urllib` unless noted)

### 3.1 Groq STT — primary

```python
import json, os, urllib.request, uuid

def groq_stt(path: str, language: str = "en") -> str:
    b = uuid.uuid4().hex
    fields = {"model": "whisper-large-v3-turbo", "language": language,
              "response_format": "json", "temperature": "0"}
    body = b"".join(
        f'--{b}\r\nContent-Disposition: form-data; name="{k}"\r\n\r\n{v}\r\n'.encode()
        for k, v in fields.items()
    )
    body += (f'--{b}\r\nContent-Disposition: form-data; name="file"; filename="a.wav"\r\n'
             "Content-Type: audio/wav\r\n\r\n").encode()
    body += open(path, "rb").read() + f"\r\n--{b}--\r\n".encode()
    req = urllib.request.Request(
        "https://api.groq.com/openai/v1/audio/transcriptions", data=body,
        headers={"Authorization": f"Bearer {os.environ['GROQ_API_KEY']}",
                 "Content-Type": f"multipart/form-data; boundary={b}"})
    with urllib.request.urlopen(req, timeout=20) as r:
        return json.load(r)["text"]
```

Send 16 kHz mono WAV (or FLAC); Groq downsamples to 16 kHz mono anyway, so pre-converting saves upload time on long clips.

### 3.2 Groq TTS — only if you can live with 200 chars and 100 requests/day

```python
def groq_tts(text: str, out: str = "out.wav", voice: str = "troy") -> None:
    assert len(text) <= 200, "Orpheus caps input at 200 characters"
    payload = {"model": "canopylabs/orpheus-v1-english", "input": text,
               "voice": voice, "response_format": "wav"}
    req = urllib.request.Request(
        "https://api.groq.com/openai/v1/audio/speech",
        data=json.dumps(payload).encode(),
        headers={"Authorization": f"Bearer {os.environ['GROQ_API_KEY']}",
                 "Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=30) as r:
        open(out, "wb").write(r.read())
```

### 3.3 Gemini STT — `gemini-3.5-transcribe`, inline base64

```python
import base64, json, os, urllib.request

GEMINI = "https://generativelanguage.googleapis.com/v1beta/interactions"

def gemini_stt(path: str, lang: str = "en-US") -> str:
    payload = {
        "model": "gemini-3.5-transcribe",
        "input": [{"type": "audio", "mime_type": "audio/wav",
                   "data": base64.b64encode(open(path, "rb").read()).decode()}],
        "generation_config": {"transcription_config": {
            "language_codes": [lang],          # [] = auto-detect
            "mode": "smart",                   # strips "um/uh", formats numbers
        }},
    }
    req = urllib.request.Request(GEMINI, data=json.dumps(payload).encode(), headers={
        "x-goog-api-key": os.environ["GEMINI_API_KEY"],
        "Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=30) as r:
        out = json.load(r)
    for step in out.get("steps", []):
        for c in step.get("content", []):
            if c.get("type") == "text":
                return c["text"]
    return ""
```

`mode: "smart"` cannot be combined with `diarization_mode` or word timestamps. Inline base64 is fine for short clips; use `POST /v1beta/files` + `{"type":"audio","uri":...}` above ~30 s.

### 3.4 Gemini TTS — `gemini-3.8-flash-lite-tts`, WAV out

```python
def gemini_tts(text: str, out: str = "out.wav", voice: str = "Kore") -> None:
    payload = {
        "model": "gemini-3.8-flash-lite-tts",
        "input": [{"type": "user_input",
                   "content": [{"type": "text", "text": text}]}],
        "response_format": {"type": "audio", "mime_type": "audio/wav"},
        "generation_config": {"speech_config": [{"voice": voice}]},
    }
    req = urllib.request.Request(GEMINI, data=json.dumps(payload).encode(), headers={
        "x-goog-api-key": os.environ["GEMINI_API_KEY"],
        "Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=60) as r:
        out_json = json.load(r)
    for step in out_json.get("steps", []):
        for c in step.get("content", []):
            if c.get("type") == "audio" and c.get("data"):
                open(out, "wb").write(base64.b64decode(c["data"]))
                return
```

Unary returns a complete RIFF WAV (24 kHz, mono, s16le). With `"stream": true` you get headerless `audio/l16` chunks instead — that is the mode to use for barge-in.

### 3.5 Cloudflare STT — raw binary body

```python
def cf_stt(path: str) -> str:
    url = ("https://api.cloudflare.com/client/v4/accounts/"
           f"{os.environ['CF_ACCOUNT_ID']}/ai/run/@cf/openai/whisper-large-v3-turbo")
    req = urllib.request.Request(url, data=open(path, "rb").read(), headers={
        "Authorization": f"Bearer {os.environ['CF_API_TOKEN']}",
        "Content-Type": "audio/wav"})
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.load(r)["result"]["text"]
```

### 3.6 Cloudflare TTS — melotts (MP3)

```python
def cf_tts(text: str, out: str = "out.mp3", lang: str = "en") -> None:
    url = ("https://api.cloudflare.com/client/v4/accounts/"
           f"{os.environ['CF_ACCOUNT_ID']}/ai/run/@cf/myshell-ai/melotts")
    req = urllib.request.Request(url, data=json.dumps(
        {"prompt": text, "lang": lang}).encode(), headers={
        "Authorization": f"Bearer {os.environ['CF_API_TOKEN']}",
        "Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=45) as r:
        ctype, raw = r.headers.get("content-type", ""), r.read()
    if "json" in ctype:                      # REST may return {"result":{"audio":"<b64>"}}
        raw = base64.b64decode(json.loads(raw)["result"]["audio"])
    open(out, "wb").write(raw)
```

### 3.7 edge-tts — primary TTS

```python
import asyncio, edge_tts

async def speak(text: str, out: str = "out.mp3", voice: str = "en-US-AvaNeural") -> None:
    for attempt in range(3):
        try:
            await edge_tts.Communicate(text, voice).save(out)
            return
        except edge_tts.exceptions.NoAudioReceived:
            if attempt == 2:
                raise
            await asyncio.sleep(0.4)
```

Retry is not optional: 2026 issues report intermittent `NoAudioReceived` (server closes the WebSocket before any audio) and occasional 403s tied to the `Sec-MS-GEC` token version. Version 7.3.1 fixed the `1-` prefix 403 bug; pin `edge-tts==7.3.1`.

### 3.8 Local STT — faster-whisper

```python
from faster_whisper import WhisperModel

model = WhisperModel("base.en", device="cpu", compute_type="int8", cpu_threads=4)

def local_stt(path: str) -> str:
    segs, _ = model.transcribe(path, beam_size=1, language="en", vad_filter=True)
    return "".join(s.text for s in segs).strip()
```

Load the model once at process start; `WhisperModel(...)` construction is the expensive part (~0.5–1 s), not inference.

### 3.9 Local TTS — Piper

```python
import wave
from piper import PiperVoice

voice = PiperVoice.load("en_US-lessac-medium.onnx")

def local_tts(text: str, out: str = "out.wav") -> None:
    with wave.open(out, "wb") as w:
        voice.synthesize_wav(text, w)
```

### 3.10 Wake word — openWakeWord

```python
import numpy as np
from openwakeword.model import Model

oww = Model(wakeword_models=["hey_navi.onnx"])   # custom model, or "hey_jarvis"
score = oww.predict(np.frombuffer(frame_1280_int16, dtype=np.int16))["hey_navi"]
if score > 0.5:
    ...  # open the STT turn
```

Feed 80 ms frames (1280 samples @ 16 kHz). Train a custom phrase with `openwakeword.train_custom_verifier` / synthetic clips; no cloud account needed.

---

## 4. Local lightweight options — cost and latency

Measured on a typical 8-core x86 laptop, 4 threads, 16 kHz mono input *(est. from vendor benchmarks + community numbers; re-measure on the Ryzen 5 8645HS before committing)*.

| Component | Package / model | Disk | RAM (RSS) | Latency | Py 3.14 |
|---|---|---|---|---|---|
| STT | `faster-whisper` tiny.en int8 | ~75 MB | ~250–350 MB | 5 s clip ≈ 0.3–0.5 s (RTF ~0.06–0.1) | yes (ctranslate2 4.8.4 ships cp314) |
| STT | `faster-whisper` base.en int8 | ~145 MB | ~0.5–0.7 GB | 5 s clip ≈ 0.8–1.2 s (RTF ~0.15–0.25) | yes |
| STT | `moonshine-voice` 0.1.5 (streaming-first) | tiny ~1–30 MB, base ~100–200 MB | ~0.3–0.6 GB | lower than Whisper on short utterances; work happens while you speak | yes (py3-none wheel) |
| STT | `sherpa-onnx` 2.0.1 (zipformer/paraformer) | 30–300 MB | ~0.3–0.8 GB | RTF ~0.05–0.2, native streaming API | yes (cp314 wheels) |
| STT | Parakeet-TDT-0.6B-v3 (ONNX int8) | ~0.7 GB (2.5 GB fp32) | ~1.2–2 GB | RTF ~0.1–0.3 on 8 cores; best accuracy of the set, 25 EU languages only | via onnxruntime 1.31.1 (cp314) |
| TTS | Piper `en_US-lessac-medium` | ~63 MB | ~200–300 MB | ~80–200 ms/sentence (RTF ~0.02–0.05) | yes (abi3 wheel) |
| TTS | Kokoro-82M ONNX (`model_q8f16`, 86 MB) | ~86 MB + 0.5 MB/voice | ~300–500 MB | ~0.3–0.8 s/sentence, 54 voices, 24 kHz | via sherpa-onnx; **`kokoro-onnx` 0.6.1 caps at <3.14** |
| TTS | `sherpa-onnx` VITS/Kokoro models | 60–350 MB | ~300–600 MB | ~0.1–0.5 s/sentence | yes |
| Wake | openWakeWord 0.6.0 | ~2 MB | ~50 MB | <1–2% of one core, 80 ms frames | pure Python + onnxruntime |
| Wake | sherpa-onnx KWS (3.3M zipformer) | ~15 MB | ~80 MB | <1% core, open-vocabulary keywords | yes |
| Wake | Porcupine (`pvporcupine`) | ~1 MB | ~30 MB | <1% core, best accuracy | **not free any more** |

---

## 5. Caveats that change the plan

- **Porcupine free tier ended.** Picovoice's FAQ now states there are no free/personal/non-commercial plans, and community reports say free AccessKeys stopped working after 2026-06-30. Do not build on it; use openWakeWord or sherpa-onnx KWS.
- **Groq PlayAI TTS is gone** from current docs. Groq TTS is now Orpheus only: 200 characters per request, WAV only, 100 requests/day free. Fine for short acknowledgements, unusable for long replies without chunking.
- **Gemini free tier = your audio is training data.** The terms allow human review of free-tier input/output, and free-tier use is restricted for products served in the EEA/UK/CH. Do not send private recordings.
- **Gemini audio rate limits are unpublished.** Google removed the per-model RPM/RPD table; AI Studio shows your project's numbers. Budget conservatively (single-digit RPM, low hundreds RPD) and always keep Cloudflare behind it.
- **edge-tts is unofficial.** It uses Edge's consumer read-aloud endpoint with no contract; Microsoft has broken it before (403 token-version changes, 2026-10). Pin the version, retry on `NoAudioReceived`, and keep Piper warm.
- **Cloudflare free tier is a daily neuron budget, not a request quota.** 10,000 neurons/day resets 00:00 UTC. whisper-turbo ≈ 214 min/day and melotts ≈ 536 min/day, but they share the same pool — a heavy STT day eats the TTS budget.
- **Python 3.14:** onnxruntime 1.31.1, ctranslate2 4.8.4, sherpa-onnx 2.0.1 and piper-tts 1.8.0 all ship 3.14-compatible wheels. `kokoro-onnx` 0.6.1 declares `<3.14` — on 3.14 use Kokoro through sherpa-onnx instead.
- **Audio format discipline:** record/convert to **16 kHz mono s16le WAV** at the edge. Every cloud STT here accepts it, it halves upload size, and it removes a whole class of "why is the transcript wrong" bugs.

---

## 6. Sources

Groq STT & rate limits: <https://console.groq.com/docs/speech-to-text> · <https://console.groq.com/docs/rate-limits> · Groq TTS/Orpheus: <https://console.groq.com/docs/text-to-speech/orpheus>
Gemini transcribe: <https://ai.google.dev/gemini-api/docs/transcribe> · TTS: <https://ai.google.dev/gemini-api/docs/speech-generation> · pricing/free tier: <https://ai.google.dev/gemini-api/docs/pricing> · limits: <https://ai.google.dev/gemini-api/docs/rate-limits> · Interactions schema (inline base64 audio): <https://ai.google.dev/api/interactions-api>
Cloudflare: <https://developers.cloudflare.com/workers-ai/platform/pricing/> · <https://developers.cloudflare.com/workers-ai/models/whisper-large-v3-turbo/> · <https://developers.cloudflare.com/workers-ai/models/melotts/>
Deepgram: <https://deepgram.com/pricing> · <https://developers.deepgram.com/docs/pre-recorded-audio>
AssemblyAI: <https://www.assemblyai.com/pricing> · <https://www.assemblyai.com/docs/billing-and-pricing>
edge-tts: <https://pypi.org/project/edge-tts/> · <https://github.com/rany2/edge-tts> (open issues on `NoAudioReceived`, 403)
Local: <https://github.com/SYSTRAN/faster-whisper> (CPU benchmark table) · <https://github.com/k2-fsa/sherpa-onnx> · <https://k2-fsa.github.io/sherpa/onnx/kws/> · <https://github.com/moonshine-ai/moonshine> · <https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX> · <https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3> · <https://github.com/dscripka/openWakeWord> · Picovoice FAQ: <https://picovoice.ai/docs/faq/general/>
