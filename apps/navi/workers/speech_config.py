"""Speech provider configuration: secrets, provider order, speech_mode.

Keys are read from the process environment, falling back to
~/.config/navi/secrets.env (simple KEY=VALUE lines). Secrets are never
logged or returned in dicts meant for display.

Provider tuning comes from ~/.config/navi/speech.json, e.g.:

    {
      "speech_mode": "cloud-first",
      "tts_order": ["edge", "cloudflare", "gemini", "piper"],
      "stt_order": ["groq", "cloudflare", "gemini"],
      "disabled": ["gemini"],
      "timeout_seconds": 3.0,
      "breaker_seconds": 300
    }

speech_mode: "cloud-first" (default, cloud providers then local),
"local-only" (skip every cloud hop), "cloud-only" (never use local).
"""
import json
import os
from pathlib import Path

CONFIG_DIR = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config")) / "navi"
SECRETS_FILE = Path(os.environ.get("NAVI_SECRETS_FILE", CONFIG_DIR / "secrets.env"))
SPEECH_FILE = Path(os.environ.get("NAVI_SPEECH_CONFIG", CONFIG_DIR / "speech.json"))

# Credentials each provider needs before it is attempted.
KEY_NAMES = ("GROQ_API_KEY", "CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_API_TOKEN", "GEMINI_API_KEY")

CLOUD_TTS = ("edge", "cloudflare", "gemini")
LOCAL_TTS = ("piper",)
CLOUD_STT = ("groq", "cloudflare", "gemini")
LOCAL_STT = ("sherpa",)

DEFAULTS = {
    "speech_mode": "cloud-first",
    "tts_order": list(CLOUD_TTS) + list(LOCAL_TTS),
    "stt_order": list(CLOUD_STT),
    "disabled": [],
    "timeout_seconds": 3.0,
    "breaker_seconds": 300.0,
    "tts_voice": "en-GB-RyanNeural",
}

MODES = ("cloud-first", "local-only", "cloud-only")

_secrets_cache = None
_speech_cache = None


def _read_env_file(path):
    values = {}
    try:
        text = path.read_text()
    except OSError:
        return values
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, _, value = line.partition("=")
        key = key.strip()
        value = value.strip().strip('"').strip("'")
        if key:
            values[key] = value
    return values


def secrets(reload=False):
    """Return dict of secrets: process env wins over secrets.env. Never log."""
    global _secrets_cache
    if _secrets_cache is not None and not reload:
        return _secrets_cache
    file_values = _read_env_file(SECRETS_FILE)
    merged = {}
    for name in KEY_NAMES:
        merged[name] = os.environ.get(name) or file_values.get(name) or ""
    # Any extra KEY=VALUE pairs in secrets.env are also exposed.
    for key, value in file_values.items():
        merged.setdefault(key, os.environ.get(key, value))
    _secrets_cache = merged
    return merged


def key(name, default=""):
    """Single secret lookup: env first, then secrets.env."""
    return os.environ.get(name) or secrets().get(name) or default


def _load_speech_file():
    try:
        data = json.loads(SPEECH_FILE.read_text())
    except (OSError, ValueError):
        return {}
    return data if isinstance(data, dict) else {}


def speech(reload=False):
    """Merged speech config dict (defaults <- speech.json)."""
    global _speech_cache
    if _speech_cache is not None and not reload:
        return _speech_cache
    merged = dict(DEFAULTS)
    merged.update(_load_speech_file())
    if merged.get("speech_mode") not in MODES:
        merged["speech_mode"] = DEFAULTS["speech_mode"]
    _speech_cache = merged
    return merged


def speech_mode():
    return speech()["speech_mode"]


def timeout_seconds():
    try:
        return float(speech()["timeout_seconds"])
    except (TypeError, ValueError):
        return DEFAULTS["timeout_seconds"]


def breaker_seconds():
    try:
        return float(speech()["breaker_seconds"])
    except (TypeError, ValueError):
        return DEFAULTS["breaker_seconds"]


def provider_order(kind):
    """Ordered provider names for 'tts' or 'stt' honoring mode + disabled.

    local-only: cloud hops removed. cloud-only: local hops removed.
    """
    cfg = speech()
    mode = cfg["speech_mode"]
    disabled = set(cfg.get("disabled") or [])
    order = list(cfg.get(f"{kind}_order") or [])
    cloud = CLOUD_TTS if kind == "tts" else CLOUD_STT
    local = LOCAL_TTS if kind == "tts" else LOCAL_STT
    if mode == "local-only":
        order = [p for p in order if p not in cloud]
    elif mode == "cloud-only":
        order = [p for p in order if p not in local]
    return [p for p in order if p not in disabled]


def reset_caches():
    global _secrets_cache, _speech_cache
    _secrets_cache = None
    _speech_cache = None
