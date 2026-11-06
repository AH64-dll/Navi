"""TTS provider chain for Navi: {CHAIN}.

Public API:
    synthesize(text, voice=None, rate=None, language=None) -> (audio_bytes, sample_rate, fmt) | None
    speak(text, voice=None, rate=None, cancel_event=None, language=None) -> bool

fmt is "mp3" or "wav". Cloud hop timeouts are configurable via
speech_config.timeout_seconds (default ~3 s) and a circuit breaker disables
a failed provider for speech_config.breaker_seconds (default 5 min).
Providers without credentials or dependencies are skipped. When *voice* is
not given, an edge/piper voice matching speech_config.language() is
selected (BUG-057). Playback uses the first available of
mpv/ffplay/pw-play/paplay; setting cancel_event (threading.Event) kills
the player for barge-in.
"""
import asyncio
import base64
import io
import json
import os
import re
import shutil
import subprocess
import tempfile
import threading
import time
import urllib.request
import wave
from pathlib import Path


import persona
import speech_config
__doc__ = __doc__.replace(
    "{CHAIN}", " -> ".join(
        list(speech_config.CLOUD_TTS) + list(speech_config.LOCAL_TTS)))

# Default edge voice per language when a persona does not pin one (BUG-057).
LANGUAGE_VOICES = {
    "en": "en-GB-RyanNeural",
    "fr": "fr-FR-DeniseNeural",
    "de": "de-DE-KatjaNeural",
    "es": "es-ES-ElviraNeural",
    "it": "it-IT-ElsaNeural",
    "pt": "pt-PT-RaquelNeural",
    "ar": "ar-EG-SalmaNeural",
    "nl": "nl-NL-ColetteNeural",
    "pl": "pl-PL-ZofiaNeural",
    "ja": "ja-JP-NanamiNeural",
    "zh": "zh-CN-XiaoxiaoNeural",
}


DEFAULT_VOICE = "en-GB-RyanNeural"


def voice_for(voice=None, language=None):
    """Resolve the TTS voice (BUG-057): an explicit *voice* wins unless a
    *language* was requested it does not speak; then the configured
    tts_voice when it matches; then the language's default."""
    lang = language or speech_config.language()
    lang = (lang or "en").split("-")[0].split("_")[0]
    configured = speech_config.speech().get("tts_voice")
    chosen = voice or configured
    if chosen and chosen.split("-")[0].split("_")[0] == lang:
        return chosen
    if language is None and chosen:
        return chosen
    return LANGUAGE_VOICES.get(lang, DEFAULT_VOICE)


EDGE_RATE = "24khz"  # edge-tts default output is 24 kHz mp3
GEMINI_URL = os.environ.get(
    "NAVI_GEMINI_URL", "https://generativelanguage.googleapis.com/v1beta/interactions")
CF_TTS_URL = os.environ.get(
    "NAVI_CF_TTS_URL",
    "https://api.cloudflare.com/client/v4/accounts/{account}/ai/run/@cf/myshell-ai/melotts")

PIPER_DIRS = [
    Path(os.environ.get("NAVI_PIPER_MODELS", "")) if os.environ.get("NAVI_PIPER_MODELS") else None,
    Path.home() / ".local/share/navi/models/piper",
]

# Personas name their edge voice. Piper ships different ids, so translate the
# edge voice to the installed fallback model. A piper id passed straight
# through ("en_GB-alan-medium") is used as-is.
PIPER_VOICES = {
    "en-GB-RyanNeural": "en_GB-alan-medium",
    "en-US-AriaNeural": "en_GB-alan-medium",
    "en-US-JennyNeural": "en_GB-alan-medium",
    "en-US-GuyNeural": "en_GB-alan-medium",
    "en-US-ChristopherNeural": "en_GB-alan-medium",
}
PIPER_DEFAULT = "en_GB-alan-medium"

# name -> monotonic timestamp until which the provider is disabled.
_BREAKERS = {}
# name -> failures in a row since the provider last worked.
_FAILURES = {}


def _breaker_open(name, now=None):
    until = _BREAKERS.get(name, 0)
    return (now or time.monotonic()) < until


def _breaker_fail(name):
    """Open after several failures in a row (`breaker_failures`). One slow reply
    from a good network should not switch voices for five minutes."""
    _FAILURES[name] = _FAILURES.get(name, 0) + 1
    if _FAILURES[name] >= speech_config.breaker_failures():
        _FAILURES.pop(name, None)
        _BREAKERS[name] = time.monotonic() + speech_config.breaker_seconds()


def _breaker_ok(name):
    _BREAKERS.pop(name, None)
    _FAILURES.pop(name, None)


def _to_edge_rate(rate):
    """rate may be a float multiplier (1.25) or an edge string ("+25%")."""
    if rate is None:
        return "+0%"
    if isinstance(rate, str):
        return rate if rate.endswith("%") else f"{rate}%"
    return f"{int(round((float(rate) - 1.0) * 100)):+d}%"


# ---------------------------------------------------------------- providers

def _synth_edge(text, voice, rate, timeout):
    import edge_tts

    async def run():
        chunks = []
        communicate = edge_tts.Communicate(text, voice or DEFAULT_VOICE, rate=_to_edge_rate(rate))
        async for chunk in communicate.stream():
            if chunk["type"] == "audio":
                chunks.append(chunk["data"])
        return b"".join(chunks)

    last_error = None
    for _ in range(2):  # retry once: intermittent NoAudioReceived / 403
        try:
            audio = asyncio.run(asyncio.wait_for(run(), timeout=timeout))
            if audio:
                return audio, 24000, "mp3"
            last_error = RuntimeError("edge-tts returned no audio")
        except Exception as error:  # noqa: BLE001 - provider boundary, any failure falls through
            last_error = error
            if isinstance(error, asyncio.TimeoutError):
                break
            time.sleep(0.3)
    raise last_error or RuntimeError("edge-tts failed")


def _synth_cloudflare(text, voice, rate, timeout):
    account = speech_config.key("CLOUDFLARE_ACCOUNT_ID")
    token = speech_config.key("CLOUDFLARE_API_TOKEN")
    if not account or not token:
        raise RuntimeError("missing Cloudflare credentials")
    url = CF_TTS_URL.format(account=account)
    lang = "en"
    if voice and re.match(r"^[a-z]{2,3}(-|_)", voice):
        lang = voice.split("-")[0].split("_")[0]
    body = json.dumps({"prompt": text, "lang": lang}).encode()
    request = urllib.request.Request(url, data=body, headers={
        "Authorization": f"Bearer {token}", "Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=timeout) as response:
        content_type = response.headers.get("content-type", "")
        raw = response.read()
    if "json" in content_type:
        raw = base64.b64decode(json.loads(raw)["result"]["audio"])
    if not raw:
        raise RuntimeError("Cloudflare melotts returned no audio")
    return raw, 24000, "mp3"


def _synth_gemini(text, voice, rate, timeout):
    api_key = speech_config.key("GEMINI_API_KEY")
    if not api_key:
        raise RuntimeError("missing GEMINI_API_KEY")
    gemini_voice = speech_config.speech().get("gemini_voice", "Kore")
    payload = {
        "model": "gemini-3.8-flash-lite-tts",
        "input": [{"type": "user_input", "content": [{"type": "text", "text": text}]}],
        "response_format": {"type": "audio", "mime_type": "audio/wav"},
        "generation_config": {"speech_config": [{"voice": gemini_voice}]},
    }
    request = urllib.request.Request(GEMINI_URL, data=json.dumps(payload).encode(), headers={
        "x-goog-api-key": api_key, "Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=timeout) as response:
        out = json.load(response)
    for step in out.get("steps", []):
        for content in step.get("content", []):
            if content.get("type") == "audio" and content.get("data"):
                return base64.b64decode(content["data"]), 24000, "wav"
    raise RuntimeError("Gemini TTS returned no audio")


_PIPER_VOICES = {}
_PIPER_LOCK = threading.Lock()


def _piper_voice_id(voice=None):
    """Map a persona edge voice to an installed Piper model id."""
    if not voice:
        return PIPER_DEFAULT
    if re.match(r"^[a-z]{2}_[A-Z]{2}-", voice):
        return voice
    return PIPER_VOICES.get(voice, PIPER_DEFAULT)


def _find_piper_model(voice=None):
    wanted = _piper_voice_id(voice).casefold()
    fallback = None
    for folder in PIPER_DIRS:
        if not folder or not folder.is_dir():
            continue
        for onnx in sorted(folder.glob("*.onnx")):
            if not (onnx.with_suffix(".onnx.json").exists() or Path(str(onnx) + ".json").exists()):
                continue
            if fallback is None:
                fallback = onnx
            if onnx.stem.casefold() == wanted:
                return onnx
    return fallback


def _load_piper(voice=None):
    wanted = _piper_voice_id(voice)
    with _PIPER_LOCK:
        if wanted in _PIPER_VOICES:
            return _PIPER_VOICES[wanted]
        model = _find_piper_model(wanted)
        if model is None:
            return None
        from piper import PiperVoice
        loaded = PiperVoice.load(str(model))
        _PIPER_VOICES[wanted] = loaded
        return loaded


def _synth_piper(text, voice, rate, timeout):
    piper_voice = _load_piper(voice)
    if piper_voice is None:
        raise RuntimeError("piper not installed or no voice model")
    buffer = io.BytesIO()
    with wave.open(buffer, "wb") as wav:
        piper_voice.synthesize_wav(text, wav)
    sample_rate = getattr(getattr(piper_voice, "config", None), "sample_rate", 22050)
    return buffer.getvalue(), sample_rate, "wav"


PROVIDERS = {
    "edge": _synth_edge,
    "cloudflare": _synth_cloudflare,
    "gemini": _synth_gemini,
    "piper": _synth_piper,
}


def provider_available(name):
    """Cheap pre-flight: creds/deps present and breaker closed."""
    if _breaker_open(name):
        return False
    if name == "edge":
        try:
            import edge_tts  # noqa: F401
            return True
        except ImportError:
            return False
    if name == "cloudflare":
        return bool(speech_config.key("CLOUDFLARE_ACCOUNT_ID") and speech_config.key("CLOUDFLARE_API_TOKEN"))
    if name == "gemini":
        return bool(speech_config.key("GEMINI_API_KEY"))
    if name == "piper":
        if _find_piper_model() is None:
            return False
        try:
            import piper  # noqa: F401
            return True
        except ImportError:
            return False
    return False


def synthesize(text, voice=None, rate=None, order=None, language=None):
    """Return (audio_bytes, sample_rate, fmt) from the first working provider.

    fmt is "mp3" or "wav". Returns None when every provider in the chain is
    unavailable or fails inside its timeout. When *voice* is unset, a voice
    matching *language* (or speech_config.language()) is used (BUG-057).
    """
    if not text or not text.strip():
        return None
    # Cloud voices return a whole sentence at once: measured on edge, 14 chars
    # took 2.1 s and 222 chars 3.3 s, so a flat limit fails only long sentences.
    timeout = speech_config.timeout_seconds() + len(text) / 120
    resolved = voice_for(voice, language)
    for name in (order or speech_config.provider_order("tts")):
        provider = PROVIDERS.get(name)
        if provider is None or not provider_available(name):
            continue
        try:
            result = provider(text, resolved, rate, timeout)
        except Exception:  # noqa: BLE001 - fall through to next hop
            _breaker_fail(name)
            continue
        _breaker_ok(name)
        return result
    return None


# ---------------------------------------------------------------- playback

# pw-play only parses RIFF wav; keep order preference mpv > ffplay > pw-play > paplay.
_PLAYERS = {
    "wav": [
        ("mpv", ["mpv", "--really-quiet", "--no-video"]),
        ("ffplay", ["ffplay", "-nodisp", "-autoexit", "-loglevel", "error"]),
        ("pw-play", ["pw-play"]),
        ("paplay", ["paplay"]),
    ],
    "mp3": [
        ("mpv", ["mpv", "--really-quiet", "--no-video"]),
        ("ffplay", ["ffplay", "-nodisp", "-autoexit", "-loglevel", "error"]),
        ("paplay", ["paplay"]),  # modern libsndfile decodes mp3
    ],
}


def _player_command(fmt):
    for name, argv in _PLAYERS.get(fmt, _PLAYERS["wav"]):
        if shutil.which(name):
            return argv
    return None


def _play_bytes(audio, fmt, cancel_event=None):
    """Play one buffer. Returns True if audio reached the device."""
    argv = _player_command(fmt)
    if argv is None or not audio:
        return False
    suffix = ".wav" if fmt == "wav" else ".mp3"
    with tempfile.NamedTemporaryFile(prefix="navi-tts-", suffix=suffix, delete=False) as handle:
        handle.write(audio)
        path = handle.name
    try:
        process = subprocess.Popen(argv + [path], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        while process.poll() is None:
            if cancel_event is not None and cancel_event.is_set():
                process.terminate()
                try:
                    process.wait(timeout=0.5)
                except subprocess.TimeoutExpired:
                    process.kill()
                return True  # barge-in still counts as spoken
            time.sleep(0.02)
        return True
    finally:
        try:
            os.unlink(path)
        except OSError:
            pass


def _sentences(text):
    parts = [sentence for line in text.splitlines() for sentence in persona.split_sentences(line)]
    return parts or [text]


def warm():
    """Load the local voice in the background. Its first load takes about 5 s
    (measured), which would otherwise land on the first reply after a network
    failure, when the user is already waiting."""
    if "piper" in speech_config.provider_order("tts") and provider_available("piper"):
        try:
            _load_piper(voice_for())
        except Exception:  # noqa: BLE001 - a broken local voice shows when used
            pass


def speak(text, voice=None, rate=None, cancel_event=None, language=None):
    """Synthesize and play text. First sentence goes out first for latency;
    the next sentence is synthesized while the current one plays.

    Returns True when any audio reached the player; False when every
    provider failed, no player exists, or playback was cancelled before
    the first chunk finished.
    """
    chunks = _sentences(text)
    order = speech_config.provider_order("tts")

    def synth(index):
        return synthesize(chunks[index], voice=voice, rate=rate, order=order,
                          language=language)

    played = False
    result = synth(0)
    for index, _ in enumerate(chunks):
        if result is None:
            continue
        audio, _sr, fmt = result
        # Prefetch the next sentence while this one plays.
        next_result = [None]
        prefetch = None
        if index + 1 < len(chunks):
            def fetch(i=index + 1, out=next_result):
                out[0] = synth(i)
            prefetch = threading.Thread(target=fetch, daemon=True)
            prefetch.start()
        if _play_bytes(audio, fmt, cancel_event=cancel_event):
            played = True
        if prefetch is not None:
            prefetch.join()
            result = next_result[0]
        if cancel_event is not None and cancel_event.is_set():
            break
    return played
