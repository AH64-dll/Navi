"""Cloud STT chain for Navi: {CHAIN}.

Public API:
    transcribe(audio_bytes, sample_rate=16000, language=None, keywords=None)
        -> str | None
    transcribe_result(...) -> (text, {"provider": name, "confidence": float|None})

Accepts raw s16le PCM or a complete WAV; raw PCM is wrapped in a WAV
header before upload. Returns the transcript, or None when every cloud
hop is unavailable or fails so the caller can fall back to local sherpa.

Timeouts, retry counts and the breaker window are configurable through
speech.json (timeout_seconds, retry_attempts, breaker_seconds,
breaker_failures); defaults are a ~3 s timeout, one retry on
network/5xx errors with jitter, and a breaker that opens only after
consecutive failures (default 2) or immediately on 401/403. Providers
without credentials are skipped. stdlib urllib only.
"""
import base64
import io
import json
import os
import random
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
import wave

import speech_config

__doc__ = __doc__.replace("{CHAIN}", " -> ".join(speech_config.CLOUD_STT))

GROQ_URL = os.environ.get(
    "NAVI_GROQ_STT_URL", "https://api.groq.com/openai/v1/audio/transcriptions")
DEEPGRAM_URL = os.environ.get(
    "NAVI_DEEPGRAM_STT_URL",
    "https://api.deepgram.com/v1/listen?model=nova-3&smart_format=true")
CF_STT_URL = os.environ.get(
    "NAVI_CF_STT_URL",
    "https://api.cloudflare.com/client/v4/accounts/{account}/ai/run/@cf/openai/whisper-large-v3-turbo")
GEMINI_URL = os.environ.get(
    "NAVI_GEMINI_URL", "https://generativelanguage.googleapis.com/v1beta/interactions")

# name -> monotonic timestamp until which the provider is disabled.
_BREAKERS = {}
# name -> consecutive failure count; the breaker opens at breaker_failures.
_FAILURES = {}


def _breaker_open(name, now=None):
    return (now or time.monotonic()) < _BREAKERS.get(name, 0)


def _breaker_fail(name, exc=None):
    """Count a provider failure; open the breaker on auth errors or after
    speech_config.breaker_failures() consecutive failures (BUG-059)."""
    failures = _FAILURES.get(name, 0) + 1
    _FAILURES[name] = failures
    if _is_auth_error(exc) or failures >= speech_config.breaker_failures():
        _BREAKERS[name] = time.monotonic() + speech_config.breaker_seconds()


def _breaker_ok(name):
    _BREAKERS.pop(name, None)
    _FAILURES.pop(name, None)


def _is_auth_error(exc):
    return isinstance(exc, urllib.error.HTTPError) and exc.code in (401, 403)


def _is_transient(exc):
    """Network drops, timeouts, 429 and 5xx answers deserve a retry (BUG-059)."""
    if isinstance(exc, (TimeoutError, ConnectionError)):
        return True
    if isinstance(exc, urllib.error.HTTPError):
        return exc.code >= 500 or exc.code == 429
    if isinstance(exc, urllib.error.URLError):
        return True
    return isinstance(exc, OSError)


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


def _keyword_params(keywords):
    """Deepgram hotword params: repeated &keywords=term (BUG-058)."""
    if not keywords:
        return ""
    return "".join(
        f"&keywords={urllib.parse.quote(str(term))}" for term in keywords[:20])


# ---------------------------------------------------------------- providers

def _stt_deepgram(wav, language, timeout, keywords=None):
    api_key = speech_config.key("DEEPGRAM_API_KEY")
    if not api_key:
        raise RuntimeError("missing DEEPGRAM_API_KEY")
    url = DEEPGRAM_URL + (f"&language={language}" if language else "") + _keyword_params(keywords)
    request = urllib.request.Request(url, data=wav, headers={
        "Authorization": f"Token {api_key}", "Content-Type": "audio/wav"})
    with urllib.request.urlopen(request, timeout=timeout) as response:
        out = json.load(response)
    channels = (out.get("results") or {}).get("channels") or []
    alternatives = (channels[0].get("alternatives") if channels else []) or []
    first = alternatives[0] if alternatives else {}
    confidence = first.get("confidence")
    return {"text": first.get("transcript") or "",
            "confidence": float(confidence) if confidence is not None else None}


def _stt_groq(wav, language, timeout, keywords=None):
    api_key = speech_config.key("GROQ_API_KEY")
    if not api_key:
        raise RuntimeError("missing GROQ_API_KEY")
    fields = {"model": "whisper-large-v3", "response_format": "json", "temperature": "0"}
    if language:
        fields["language"] = language
    if keywords:
        # Whisper-style engines take a prompt for context biasing (BUG-058).
        fields["prompt"] = "Vocabulary: " + ", ".join(str(k) for k in keywords[:20])
    body, content_type = _multipart(fields, "a.wav", wav, "audio/wav")
    request = urllib.request.Request(GROQ_URL, data=body, headers={
        "Authorization": f"Bearer {api_key}", "Content-Type": content_type})
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return {"text": json.load(response).get("text", ""), "confidence": None}


def _stt_cloudflare(wav, language, timeout, keywords=None):
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
    return {"text": (out.get("result") or {}).get("text", ""), "confidence": None}


def _stt_gemini(wav, language, timeout, keywords=None):
    api_key = speech_config.key("GEMINI_API_KEY")
    if not api_key:
        raise RuntimeError("missing GEMINI_API_KEY")
    transcription = {"mode": "smart"}
    if language:
        transcription["language_codes"] = [language]
    else:
        transcription["language_codes"] = []
    if keywords:
        # Transcription hints bias proper nouns (BUG-058).
        transcription["hints"] = [str(k) for k in keywords[:20]]
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
                confidence = content.get("confidence")
                return {"text": content["text"],
                        "confidence": float(confidence) if confidence is not None else None}
    return {"text": "", "confidence": None}


PROVIDERS = {
    "deepgram": _stt_deepgram,
    "groq": _stt_groq,
    "cloudflare": _stt_cloudflare,
    "gemini": _stt_gemini,
}


def provider_available(name):
    if _breaker_open(name):
        return False
    if name == "deepgram":
        return bool(speech_config.key("DEEPGRAM_API_KEY"))
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


def _normalize(out):
    """Provider returns {text, confidence}; test fakes may return a bare str."""
    if isinstance(out, dict):
        return out.get("text") or "", out.get("confidence")
    return out or "", None


def transcribe_result(audio_bytes, sample_rate=16000, language=None,
                      keywords=None, order=None):
    """Transcribe and return (text, meta) where meta names the provider and
    carries its confidence when reported (BUG-059)."""
    if not audio_bytes:
        return None, None
    wav = pcm16_to_wav(audio_bytes, sample_rate)
    timeout = speech_config.timeout_seconds()
    retries = speech_config.retry_attempts()
    for name in (order or speech_config.provider_order("stt")):
        provider = PROVIDERS.get(name)
        if provider is None or not provider_available(name):
            continue
        attempts = 1 + retries
        text = confidence = None
        failed = False
        for attempt in range(attempts):
            try:
                text, confidence = _normalize(provider(wav, language, timeout, keywords))
                break
            except Exception as error:  # noqa: BLE001 - retry or fall through
                if _is_auth_error(error):
                    _breaker_fail(name, error)
                    failed = True
                    break
                if attempt + 1 >= attempts or not _is_transient(error):
                    _breaker_fail(name, error)
                    failed = True
                    break
                # Transient: retry once with jitter, bounded by the timeout.
                time.sleep(0.1 + random.random() * 0.2)
        if failed or text is None:
            continue
        _breaker_ok(name)
        text = (text or "").strip()
        if not text:
            return None, None
        return text, {"provider": name, "confidence": confidence}
    return None, None


def transcribe(audio_bytes, sample_rate=16000, language=None, order=None,
               keywords=None):
    """Transcribe s16le PCM or WAV via the first working cloud provider.

    Returns the transcript text, or None when every hop is unavailable or
    fails so the caller falls back to local sherpa STT.
    """
    text, _meta = transcribe_result(
        audio_bytes, sample_rate=sample_rate, language=language,
        keywords=keywords, order=order)
    return text
