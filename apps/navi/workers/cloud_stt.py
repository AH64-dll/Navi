"""Cloud STT chain for Navi: Groq -> Cloudflare whisper -> Gemini.

Public API:
    transcribe(audio_bytes, sample_rate=16000, language=None) -> str | None

Accepts raw s16le PCM or a complete WAV; raw PCM is wrapped in a WAV
header before upload. Returns the transcript, or None when every cloud
hop is unavailable or fails so the caller can fall back to local sherpa.

Every hop has a ~3 s timeout and a circuit breaker that disables a failed
provider for speech_config.breaker_seconds (default 5 min). Providers
without credentials are skipped. stdlib urllib only.
"""
import base64
import io
import json
import os
import time
import urllib.request
import uuid
import wave

import speech_config

GROQ_URL = os.environ.get(
    "NAVI_GROQ_STT_URL", "https://api.groq.com/openai/v1/audio/transcriptions")
CF_STT_URL = os.environ.get(
    "NAVI_CF_STT_URL",
    "https://api.cloudflare.com/client/v4/accounts/{account}/ai/run/@cf/openai/whisper-large-v3-turbo")
GEMINI_URL = os.environ.get(
    "NAVI_GEMINI_URL", "https://generativelanguage.googleapis.com/v1beta/interactions")

# name -> monotonic timestamp until which the provider is disabled.
_BREAKERS = {}


def _breaker_open(name, now=None):
    return (now or time.monotonic()) < _BREAKERS.get(name, 0)


def _breaker_fail(name):
    _BREAKERS[name] = time.monotonic() + speech_config.breaker_seconds()


def _breaker_ok(name):
    _BREAKERS.pop(name, None)


def pcm16_to_wav(audio, sample_rate):
    """Wrap raw s16le mono PCM in a RIFF WAV; pass through real WAV bytes."""
    if audio[:4] == b"RIFF" and audio[8:12] == b"WAVE":
        return audio
    buffer = io.BytesIO()
    with wave.open(buffer, "wb") as wav:
        wav.setnchannels(1)
        wav.setsampwidth(2)
        wav.setframerate(int(sample_rate))
        wav.writeframes(audio)
    return buffer.getvalue()


def _multipart(fields, file_name, file_bytes, content_type):
    boundary = uuid.uuid4().hex
    body = b"".join(
        f'--{boundary}\r\nContent-Disposition: form-data; name="{key}"\r\n\r\n{value}\r\n'.encode()
        for key, value in fields.items()
    )
    body += (
        f'--{boundary}\r\nContent-Disposition: form-data; name="file"; filename="{file_name}"\r\n'
        f"Content-Type: {content_type}\r\n\r\n"
    ).encode()
    body += file_bytes + f"\r\n--{boundary}--\r\n".encode()
    return body, f"multipart/form-data; boundary={boundary}"


# ---------------------------------------------------------------- providers

def _stt_groq(wav, language, timeout):
    api_key = speech_config.key("GROQ_API_KEY")
    if not api_key:
        raise RuntimeError("missing GROQ_API_KEY")
    fields = {"model": "whisper-large-v3-turbo", "response_format": "json", "temperature": "0"}
    if language:
        fields["language"] = language
    body, content_type = _multipart(fields, "a.wav", wav, "audio/wav")
    request = urllib.request.Request(GROQ_URL, data=body, headers={
        "Authorization": f"Bearer {api_key}", "Content-Type": content_type})
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return json.load(response).get("text", "")


def _stt_cloudflare(wav, language, timeout):
    account = speech_config.key("CLOUDFLARE_ACCOUNT_ID")
    token = speech_config.key("CLOUDFLARE_API_TOKEN")
    if not account or not token:
        raise RuntimeError("missing Cloudflare credentials")
    request = urllib.request.Request(CF_STT_URL.format(account=account), data=wav, headers={
        "Authorization": f"Bearer {token}", "Content-Type": "audio/wav"})
    with urllib.request.urlopen(request, timeout=timeout) as response:
        out = json.load(response)
    if not out.get("success"):
        raise RuntimeError("Cloudflare whisper reported failure")
    return (out.get("result") or {}).get("text", "")


def _stt_gemini(wav, language, timeout):
    api_key = speech_config.key("GEMINI_API_KEY")
    if not api_key:
        raise RuntimeError("missing GEMINI_API_KEY")
    transcription = {"mode": "smart"}
    if language:
        transcription["language_codes"] = [language]
    else:
        transcription["language_codes"] = []
    payload = {
        "model": "gemini-3.5-transcribe",
        "input": [{"type": "audio", "mime_type": "audio/wav",
                   "data": base64.b64encode(wav).decode()}],
        "generation_config": {"transcription_config": transcription},
    }
    request = urllib.request.Request(GEMINI_URL, data=json.dumps(payload).encode(), headers={
        "x-goog-api-key": api_key, "Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=timeout) as response:
        out = json.load(response)
    for step in out.get("steps", []):
        for content in step.get("content", []):
            if content.get("type") == "text" and content.get("text"):
                return content["text"]
    return ""


PROVIDERS = {
    "groq": _stt_groq,
    "cloudflare": _stt_cloudflare,
    "gemini": _stt_gemini,
}


def provider_available(name):
    if _breaker_open(name):
        return False
    if name == "groq":
        return bool(speech_config.key("GROQ_API_KEY"))
    if name == "cloudflare":
        return bool(speech_config.key("CLOUDFLARE_ACCOUNT_ID") and speech_config.key("CLOUDFLARE_API_TOKEN"))
    if name == "gemini":
        return bool(speech_config.key("GEMINI_API_KEY"))
    return False


def available(order=None):
    """True when at least one cloud hop has credentials and a closed breaker.

    Callers use this before converting or uploading audio so a machine with no
    keys pays no cost for the cloud attempt.
    """
    return any(
        provider_available(name)
        for name in (order or speech_config.provider_order("stt"))
    )


def transcribe(audio_bytes, sample_rate=16000, language=None, order=None):
    """Transcribe s16le PCM or WAV via the first working cloud provider.

    Returns the transcript text, or None when every hop is unavailable or
    fails so the caller falls back to local sherpa STT.
    """
    if not audio_bytes:
        return None
    wav = pcm16_to_wav(audio_bytes, sample_rate)
    timeout = speech_config.timeout_seconds()
    for name in (order or speech_config.provider_order("stt")):
        provider = PROVIDERS.get(name)
        if provider is None or not provider_available(name):
            continue
        try:
            text = provider(wav, language, timeout)
        except Exception:  # noqa: BLE001 - fall through to next hop
            _breaker_fail(name)
            continue
        _breaker_ok(name)
        return (text or "").strip() or None
    return None
