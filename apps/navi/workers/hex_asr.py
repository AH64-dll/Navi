"""hex_asr.py - local transcription through the installed Hex engine.

Hex (https://anoma.ly/hex) ships a one-shot CLI transcriber:

    hex model transcribe <WAV>   # transcript on stdout

Navi keeps owning the microphone; this module only hands completed
utterances to Hex's local model (Parakeet Unified English, Vulkan/CPU) and
returns the text. No Hex desktop process, hotkey, or paste is involved.

Environment:
  NAVI_HEX           override the hex executable (tests use a stub script)
  NAVI_HEX_DISABLE   "1" disables the Hex engine entirely
  NAVI_HEX_TIMEOUT   per-utterance timeout in seconds (default 45)

Availability caching (BUG-068): a positive probe is cached for the process;
a "binary missing" answer is permanent until the executable's mtime changes;
a failed probe (timeout, non-zero exit, missing model) is a transient and is
retried after NEGATIVE_CACHE_SECONDS (60 s) rather than disabling the engine
for the rest of the session.
"""
import io
import os
from pathlib import Path
import subprocess
import tempfile
import time
import wave

HEX = Path(os.environ.get("NAVI_HEX") or Path.home() / ".local/bin/hex")
TIMEOUT = float(os.environ.get("NAVI_HEX_TIMEOUT", "45"))
MODEL = "parakeet_unified_en"
LANGUAGES = ("en",)  # parakeet_unified_en is English-only (BUG-057)
NEGATIVE_CACHE_SECONDS = 60.0

# None = never probed; "missing" = no binary (re-checked on mtime change);
# True = usable; float = transient probe failure, retry after 60 s.
_installed = None
_binary_mtime = None


def enabled():
    return os.environ.get("NAVI_HEX_DISABLE") != "1"


def supports_language(language):
    """Hex is skipped entirely when the configured language is not English."""
    return (language or "en").split("-")[0].split("_")[0] in LANGUAGES


def _probe(binary):
    """Return True (installed) or False (probe failed: timeout/non-zero/
    missing model). A missing *file* is handled by the caller."""
    try:
        result = subprocess.run(
            [str(binary), "model", "status"],
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, timeout=10, text=True,
        )
        return result.returncode == 0 and not result.stdout.lstrip().startswith("missing")
    except (OSError, subprocess.SubprocessError):
        return False


def available(refresh=False, executable=None):
    """True when Hex and its dictation model are installed.

    Positives and "binary missing" cache for the process (missing is
    re-checked only when the executable's mtime changes, e.g. an install).
    Probe failures cache for NEGATIVE_CACHE_SECONDS then retry (BUG-068).
    """
    global _installed, _binary_mtime
    if not enabled():
        return False
    binary = Path(executable or HEX)
    now = time.monotonic()
    if _installed is True and not refresh:
        return True
    if _installed == "missing" and not refresh:
        try:
            if binary.stat().st_mtime == _binary_mtime:
                return False
        except OSError:
            return False
        # The file changed: fall through and probe again.
    if isinstance(_installed, float) and not refresh and now - _installed < NEGATIVE_CACHE_SECONDS:
        return False
    if not binary.is_file():
        _installed = "missing"
        try:
            _binary_mtime = binary.stat().st_mtime
        except OSError:
            _binary_mtime = None
        return False
    if _probe(binary):
        _installed = True
        return True
    _installed = now  # transient failure: probe again after the cache window
    return False


def reset_cache():
    """Drop the cached availability; used by tests and worker restarts."""
    global _installed, _binary_mtime
    _installed = None
    _binary_mtime = None


def wav_bytes(blocks, sample_rate=16000):
    """Encode float32 mono blocks as a 16-bit PCM WAV in memory."""
    import numpy as np
    samples = np.concatenate(blocks) if blocks else np.zeros(0, dtype="float32")
    pcm = (np.clip(samples, -1, 1) * 32767).astype("<i2").tobytes()
    buffer = io.BytesIO()
    with wave.open(buffer, "wb") as handle:
        handle.setnchannels(1)
        handle.setsampwidth(2)
        handle.setframerate(int(sample_rate))
        handle.writeframes(pcm)
    return buffer.getvalue()


def transcribe(blocks, sample_rate=16000, timeout=None, executable=None):
    """Transcribe captured float32 blocks with Hex. Returns text or None.

    None means "Hex did not produce a transcript" (missing, failed, timed
    out, or silence); callers fall back to the other STT hops.
    """
    if not available(executable=executable):
        return None
    binary = Path(executable or HEX)
    audio = wav_bytes(blocks, sample_rate)
    if not audio:
        return None
    handle = tempfile.NamedTemporaryFile(suffix=".wav", prefix="navi-hex-", delete=False)
    try:
        handle.write(audio)
        handle.close()
        result = subprocess.run(
            [str(binary), "model", "transcribe", handle.name],
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, timeout=timeout or TIMEOUT, text=True,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    finally:
        try:
            os.unlink(handle.name)
        except OSError:
            pass
    if result.returncode != 0:
        return None
    transcript = result.stdout.strip()
    return transcript or None


if __name__ == "__main__":
    import sys
    path = sys.argv[1]
    print(f"available={available()} binary={HEX} model={MODEL}")
    with wave.open(path, "rb") as handle:
        frames = handle.readframes(handle.getnframes())
    import numpy as np
    samples = np.frombuffer(frames, dtype="<i2").astype("float32") / 32767
    print(transcribe([samples], handle.getframerate()))
