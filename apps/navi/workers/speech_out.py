#!/usr/bin/env python3
"""speech_out.py - speak assistant replies through the selected persona.

Sanitizes text for TTS, then delegates playback to workers/tts.py when it is
importable (lazy import so the module still works if tts.py or its deps are
absent). Falls back to logging the utterance.
"""
from __future__ import annotations

import json
import sys

import persona

_tts = None
_tts_checked = False


def _get_tts():
    """Lazy-import tts.py; return the module or None."""
    global _tts, _tts_checked
    if not _tts_checked:
        _tts_checked = True
        try:
            import tts as _mod
            _tts = _mod
        except Exception as e:
            sys.stderr.write(f"[speech_out] tts unavailable: {e}\n")
            _tts = None
    return _tts


def speak(text: str, persona_name: str | None = None, cancel_event=None,
          language: str | None = None) -> dict:
    """Sanitize *text* for TTS, speak it via tts.speak, return the payload.

    The full sanitized reply goes back in ``text`` for the UI/transcript and
    only the first few sentences reach ``tts.speak``. ``language`` selects a
    matching voice when the persona default does not speak it (BUG-057).
    """
    p = persona.get(persona_name)
    if not isinstance(text, str):
        text = str(text or "")
    utterance = persona.sanitize_for_tts(text)
    spoken = persona.first_sentences(utterance)
    payload = {
        "persona": p.name,
        "voice": p.voice,
        "rate": p.rate,
        "text": utterance,
        "spoken_text": spoken,
        "spoken": False,
    }
    tts_mod = _get_tts()
    if tts_mod is not None and spoken:
        try:
            kwargs = {"language": language} if language else {}
            payload["spoken"] = bool(
                tts_mod.speak(
                    spoken,
                    voice=p.voice,
                    rate=p.rate,
                    cancel_event=cancel_event,
                    **kwargs,
                )
            )
        except Exception as e:
            sys.stderr.write(f"[speech_out] tts.speak failed: {e}\n")
    sys.stderr.write("[speech_out] " + json.dumps(payload) + "\n")
    sys.stderr.flush()
    return payload


if __name__ == "__main__":
    name = sys.argv[1] if len(sys.argv) > 1 else "navi"
    text = sys.argv[2] if len(sys.argv) > 2 else "All systems nominal, sir."
    print(json.dumps(speak(text, name)))
