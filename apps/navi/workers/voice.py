"""Wake-word capture, trimmed-utterance STT, then the Rust chief.

The capture thread only reads the microphone, runs the adaptive VAD and the
KWS, and enqueues trimmed utterances; recognition runs on a bounded STT
queue (BUG-054), status polling lives on its own thread (BUG-064), and
overflow marks a gap instead of erasing the command (BUG-055).
"""
import os
os.environ["NEEDLE_TELEMETRY"] = "0"
os.environ["DO_NOT_TRACK"] = "1"
from pathlib import Path
import json
import queue
import re
import subprocess
import sys
import threading
import time
import traceback

from ipc import DATA, RUNTIME, rpc
import hex_asr
import speech_config
import speech_out
from voice_stream import (AdaptiveVad, CaptureBuffer, CommandRouter, SttQueue,
                          StatusSnapshot, agc)

CANCEL_FILE = RUNTIME / "voice.cancel"
# BUG-056: one settlement per capture. The daemon creates `voice.claim` for a
# UI Enter (and removes it on cancel); `voice.confirm` only ever carries a
# cancel decision. Both are cleared when a capture starts; a winning claim
# outlives the turn so a late Enter cannot dispatch the same text again.
CLAIM_FILE = RUNTIME / "voice.claim"
CONFIRM_FILE = RUNTIME / "voice.confirm"


def clear_confirm():
    """A new capture starts neutral; the caller clears a finished capture."""
    for path in (CLAIM_FILE, CONFIRM_FILE):
        try:
            path.unlink()
        except OSError:
            pass


def confirm_signal():
    """'accept' if the UI claimed the transcript, 'cancel' if it withdrew it."""
    if CLAIM_FILE.exists():
        return "accept"
    try:
        marker = json.loads(CONFIRM_FILE.read_text(encoding="utf-8") or "{}")
    except (OSError, ValueError):
        return None
    if isinstance(marker, dict) and marker.get("accept") is False:
        return "cancel"
    return None


def claim_dispatch():
    """True for the single side allowed to dispatch the held transcript.

    `open(..., "x")` is O_EXCL, so the UI's Enter and this deadline can never
    both win. Without a writable runtime dir there is no UI to arbitrate with,
    so the headless path keeps dispatching instead of silently dropping it.
    """
    try:
        with open(CLAIM_FILE, "x", encoding="utf-8") as file:
            file.write(str(time.time_ns()))
        return True
    except FileExistsError:
        return False
    except OSError:
        return True



def cancel_stamp():
    """The daemon writes this file on Stop; a 100 ms block read keeps stop latency
    under one capture block instead of one status poll (BUG-061)."""
    try:
        return CANCEL_FILE.stat().st_mtime_ns
    except OSError:
        return 0

engine_library = DATA / "runtime/needle3/libneedle.so"
if engine_library.is_file():
    os.environ["NEEDLE3_LIB_PATH"] = str(engine_library)

WAKE_FOLDER = Path("models/wake/sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01")
LAUNCHER = Path.home() / ".local/bin/navi-station"
WINDOW_COOLDOWN = 10
window_spawned = 0.0
# Both the bare name and the classic phrase wake Navi. Token spelling comes
# from the sherpa KWS model's tokens.txt (▁NA VI / ▁HE Y ▁NA VI).
WAKE_KEYWORDS = ("▁NA VI", "▁HE Y ▁NA VI")
WAKE_WORD = "Navi"
STOP_WORDS = ("stop", "cancel", "stop navi", "navi stop", "quiet", "shut up")


def enabled(status):
    settings = status["settings"]
    return settings["microphone"] and (settings["listening"] == "background" or status["gui_open"])


def pops_window(status):
    """Background listening with no Station window: wake should open one."""
    settings = status["settings"]
    return settings["microphone"] and settings["listening"] == "background" and not status["gui_open"]


def pop_window():
    """Open the Station window after a wake word. Never raises, never doubles."""
    global window_spawned
    if time.monotonic() - window_spawned < WINDOW_COOLDOWN:
        return
    window_spawned = time.monotonic()
    try:
        subprocess.Popen([str(LAUNCHER)], stdin=subprocess.DEVNULL,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                         start_new_session=True)
    except OSError:
        pass


# ------------------------------------------------------------------ state

_state_queue = queue.Queue(maxsize=8)
_state_thread = None


def _state_writer():
    """Single writer drains voice_state RPCs; producers never block (BUG-064)."""
    while True:
        payload = _state_queue.get()
        if payload is None:
            return
        try:
            rpc("voice_state", payload, timeout=2)
        except (OSError, RuntimeError):
            pass


def state(status, **details):
    """Fire-and-forget voice_state; never blocks the capture thread."""
    global _state_thread
    if _state_thread is None or not _state_thread.is_alive():
        _state_thread = threading.Thread(target=_state_writer, daemon=True)
        _state_thread.start()
    payload = {"status": status, **details}
    try:
        _state_queue.put_nowait(payload)
    except queue.Full:
        pass


def _stop_state_writer():
    try:
        _state_queue.put_nowait(None)
    except queue.Full:
        pass


# ------------------------------------------------------------- transcript

def _wake_variants():
    """Strip alternates derived from the KWS keyword list plus common
    mishearings — not a hardcoded engine list (BUG-050)."""
    names = set()
    for keyword in WAKE_KEYWORDS:
        words = keyword.replace("▁", " ").split()
        names.add("".join(words))
    names.discard("")
    mishearings = ("navy", "nadi", "nabi", "nave")
    alternates = "|".join(sorted(names | set(mishearings), key=len, reverse=True))
    return alternates or "navi"


_STRIP_RE = re.compile(
    rf"^(?:h(?:ey|ay)[\s,.!?]+)?(?:{_wake_variants()})\b[\s,.!?]*",
    flags=re.IGNORECASE)


def command_text(transcript):
    """Drop a leading wake phrase: "hey navi ..." or just "navi ...".

    Safety net only (BUG-050): the wake word was already trimmed from the
    audio, so this runs on wake-triggered captures and the caller logs any
    residue it strips.
    """
    return _STRIP_RE.sub("", (transcript or "").strip()).strip(" ,.!?")


def ignored_wake(transcript):
    """Words that acoustically trip the bare 'Navi' keyword but are not a wake.

    The 3.3M KWS model hears "navigation"/"navigate" as the same prefix as
    "Navi". Evaluated on the RAW transcript (BUG-067): "Navi navigate to X"
    strips to a real command, while a bare "navigation..." transcript means
    the keyword fired on a word that is not Navi.
    """
    return bool(re.match(r"^(?:navigation|navigator|navigate)\b", transcript or "",
                         flags=re.IGNORECASE))


def command_for(raw, wake_triggered):
    """Decide the dispatch text from a raw STT transcript.

    BUG-067/050: a wake-triggered capture whose raw text starts with a
    navigation word was a false wake (returns ""); a confirmed wake still
    strips its leading wake phrase; non-wake (barge-in/segment) captures
    keep their text so legitimate "navy"/"navigate" words survive.
    """
    raw = (raw or "").strip()
    if not raw:
        return ""
    if wake_triggered and ignored_wake(raw):
        return ""
    return command_text(raw) if wake_triggered else raw


def speech_keywords(status):
    settings = status.get("settings", {})
    sessions = [session for session in status.get("sessions", []) if session["running"]]
    sessions.sort(key=lambda session: session["id"] != settings.get("active_session"))
    candidates = ["Navi", "Codex", "Claude", "Pi"] + [session["name"] for session in sessions]
    for key in ("profiles", "projects", "favorites"):
        candidates.extend(item.get("name") for item in settings.get(key, []))
    keywords = []
    seen = set()
    for candidate in candidates:
        if not isinstance(candidate, str):
            continue
        name = " ".join(candidate.split())
        if name and len(name) <= 120 and name.casefold() not in seen:
            keywords.append(name)
            seen.add(name.casefold())
            if len(keywords) == 24:
                break
    return keywords


# ------------------------------------------------------- harness routing

_HARNESS_PATTERNS = (
    # "tell <multi word session> to <text>" — "to" separates target and text.
    re.compile(r"^(?:tell|ask|send|message)\s+(?P<target>.+?)\s+to\s+(?P<text>.+)$",
               flags=re.IGNORECASE),
    # "tell <session> <text>" — single-word target, no "to" marker.
    re.compile(r"^(?:tell|ask|send|message)\s+(?P<target>\S+)\s+(?P<text>.+)$",
               flags=re.IGNORECASE),
    # "continue in <session> <text>".
    re.compile(r"^continue\s+in\s+(?P<target>\S+)\s*:?\s*(?P<text>.+)$",
               flags=re.IGNORECASE),
)


def harness_target(transcript, status):
    """BUG-065: 'tell Claude to run the tests' -> {id, text} for the prompt op.

    Resolves the named session among running harnesses ('this'/'active' uses
    the active session). Returns None when the phrase or target is not a
    harness route. The session `state` field, when the daemon reports it,
    must be idle|needs-input — anything else reports busy instead of typing
    over a running job.
    """
    wanted = text = None
    for pattern in _HARNESS_PATTERNS:
        match = pattern.match((transcript or "").strip())
        if match:
            wanted = match.group("target").strip().casefold()
            text = match.group("text").strip()
            break
    if not wanted:
        return None
    settings = status.get("settings", {})
    sessions = [s for s in status.get("sessions", []) if s.get("running")]
    if wanted in ("this", "active", "the active session", "it"):
        wanted_id = settings.get("active_session")
        target = next((s for s in sessions if s.get("id") == wanted_id), None)
    else:
        target = None
        for session in sessions:
            name = (session.get("name") or "").casefold()
            if name and (name == wanted or name.startswith(wanted) or wanted.startswith(name)):
                target = session
                break
    if target is None:
        return None
    session_state = str(target.get("state") or "idle").lower()
    if session_state not in ("idle", "needs-input", "needs_input"):
        return {"busy": True, "target": target.get("name")}
    return {"id": target["id"], "name": target.get("name"), "text": text}


def waiting_transitions(previous, sessions):
    """Sessions that just started waiting for input.

    Returns the new state map and the names to announce. A session seen for
    the first time is never announced, so a restart does not read out every
    harness that was already waiting.
    """
    states = {}
    names = []
    for session in sessions:
        if not session.get("running"):
            continue
        state = str(session.get("state") or "")
        states[session.get("id")] = state
        before = previous.get(session.get("id"))
        if before is not None and before != "needs_input" and state == "needs_input":
            names.append(session.get("name") or "A harness")
    return states, names


def watch_waiting(snapshot, playback, stop, interval=1.0):
    """Say "Codex is waiting for your input" when Speak replies is on."""
    states = None
    while not stop.wait(interval):
        status = snapshot.value
        if not isinstance(status, dict):
            continue
        current, names = waiting_transitions(states or {}, status.get("sessions") or [])
        if states is None:
            names = []
        states = current
        settings = status.get("settings") or {}
        if not names or not settings.get("speak"):
            continue
        verb = "is" if len(names) == 1 else "are"
        playback.start(f"{' and '.join(names)} {verb} waiting for your input",
                       settings.get("persona"),
                       language=(settings.get("speech") or {}).get("language"))


# ------------------------------------------------------------------ loading

def load_spotter(speech, data=DATA):
    """Build the local wake-word spotter; wake detection never uses the cloud."""
    import sherpa_onnx
    folder = Path(data) / WAKE_FOLDER
    keywords = folder / "navi-keywords.txt"
    wanted = "\n".join(WAKE_KEYWORDS) + "\n"
    try:
        if keywords.read_text() != wanted:
            keywords.write_text(wanted)
    except OSError:
        pass
    return sherpa_onnx.KeywordSpotter(
        tokens=str(folder / "tokens.txt"),
        encoder=str(folder / "encoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx"),
        decoder=str(folder / "decoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx"),
        joiner=str(folder / "joiner-epoch-12-avg-2-chunk-16-left-64.int8.onnx"),
        keywords_file=str(folder / "navi-keywords.txt"),        num_threads=1,
        keywords_score=speech.get("wake_score", 1),
        keywords_threshold=speech.get("wake_threshold", .25),
        provider="cpu",
    )


def load_recognizer(speech, data=DATA):
    """Build the local STT fallback; returns (engine, recognizer)."""
    import sherpa_onnx
    engine = speech.get("engine", "whistle")
    if engine == "whistle":
        import needle
        recognizer = needle.Whistle(weights=speech.get("weights", Path(data) / "models/whistle/whistle.cact"))
    elif engine == "moonshine":
        folder = Path(speech.get("folder") or Path(data) / "models/moonshine/sherpa-onnx-moonshine-tiny-en-quantized-2026-02-27")
        recognizer = sherpa_onnx.OfflineRecognizer.from_moonshine_v2(
            encoder=str(folder / "encoder_model.ort"),
            decoder=str(folder / "decoder_model_merged.ort"),
            tokens=str(folder / "tokens.txt"),
            num_threads=2,
        )
    elif engine == "whisper":
        folder = Path(speech.get("folder") or Path(data) / "models/whisper-tiny.en")
        recognizer = sherpa_onnx.OfflineRecognizer.from_whisper(
            encoder=str(folder / "tiny.en-encoder.int8.onnx"),
            decoder=str(folder / "tiny.en-decoder.int8.onnx"),
            tokens=str(folder / "tiny.en-tokens.txt"),
            num_threads=2,
            tail_paddings=1000,
        )
    else:
        raise ValueError("Unregistered speech engine")
    return engine, recognizer


# --------------------------------------------------------------------- STT

def pcm16(blocks):
    """Concatenate float32 blocks into raw s16le mono PCM for cloud STT."""
    import numpy as np
    samples = np.concatenate(blocks)
    return (np.clip(samples, -1, 1) * 32767).astype("<i2").tobytes()


def recognize(blocks, engine, recognizer, status, cloud=True, *,
              wake_triggered=True, language=None, keywords=None):
    """Cloud first when credentials exist, then Hex, then the loaded engine.

    Returns (command_text, source, meta) where meta carries the raw
    transcript and the provider confidence when reported (BUG-059). Audio is
    AGC-normalised (BUG-051); the configured language threads through every
    hop (BUG-057) and keywords reach every provider that accepts them
    (BUG-058). BUG-067: the false-wake guard runs on the raw transcript.
    """
    raw = ""
    source = "local"
    meta = {"confidence": None}
    import numpy as np
    language = language or speech_config.language()
    keywords = keywords if keywords is not None else speech_keywords(status)
    mode = speech_config.speech_mode()
    blocks = agc(tuple(blocks))
    if cloud and mode != "local-only":
        try:
            import cloud_stt
            if cloud_stt.available():
                raw, cloud_meta = cloud_stt.transcribe_result(
                    pcm16(blocks), 16000, language=language, keywords=keywords)
                raw = raw or ""
                if raw:
                    source = "cloud"
                    meta["confidence"] = (cloud_meta or {}).get("confidence")
                    meta["provider"] = (cloud_meta or {}).get("provider")
        except Exception:  # noqa: BLE001 - cloud STT is an optional hop
            raw = ""
    if not raw and cloud and mode != "cloud-only":
        # Hex is the preferred local engine — English-only, so a configured
        # non-English language skips straight to the local hop (BUG-057).
        if hex_asr.supports_language(language):
            try:
                raw = hex_asr.transcribe(blocks) or ""
                if raw:
                    source = "hex"
            except Exception:  # noqa: BLE001 - Hex is an optional local hop
                raw = ""
    if not raw:
        source = "local"
        if engine == "whistle":
            raw = recognizer.transcribe(
                np.concatenate(blocks), language=language,
                keywords=keywords,
            )["text"]
        else:
            utterance = recognizer.create_stream()
            utterance.accept_waveform(16000, np.concatenate(blocks))
            recognizer.decode_stream(utterance)
            raw = utterance.result.text
    meta["raw"] = raw
    meta["language"] = language
    return command_for(raw, wake_triggered), source, meta


def offline(samples, spotter, recognizer, engine, status=None, *, assume_wake=False,
            dispatch=None, quiet=.008, tail_blocks=15, vad=None, language=None,
            recognize_fn=None):
    """Run the live wake/endpoint/STT path over recorded float32 samples.

    `samples` is mono 16 kHz float32. `tail_blocks` of silence flush the last
    utterance. When the wake detector misses synthetic speech, call again
    with `assume_wake=True` and only the post-wake audio. Returns the
    transcript, source, wake flag, per-stage timings, capture events, and
    the trimmed blocks/PCM actually handed to STT (BUG-049) so tests can
    assert the wake audio never reaches the cloud.
    """
    import numpy as np
    status = status or {"settings": {"speech": {}, "profiles": [], "projects": [], "favorites": [], "sessions": []}}
    vad = vad or AdaptiveVad(floor=quiet)
    recognize_fn = recognize_fn or recognize
    capture = CaptureBuffer(spotter)
    if assume_wake:
        capture.start_recording()
        # assume_wake is a fired wake (no wake audio in the fixture), not a
        # barge-in: the fast endpoint applies.
        capture._barge_in = False
    block_size = CaptureBuffer.block_size
    padding = (-len(samples)) % block_size
    if padding:
        samples = np.concatenate([samples, np.zeros(padding, dtype=np.float32)])
    tail = [np.zeros(block_size, dtype=np.float32)] * tail_blocks
    result = {"wake": assume_wake, "transcript": None, "source": None,
              "timings": {}, "events": [], "segments": 0,
              "truncated": False, "gap": False, "meta": {}}
    segments = []
    started = time.monotonic()
    for block in [samples[index:index + block_size] for index in range(0, len(samples), block_size)] + tail:
        quiet_now = not vad.feed(block)
        event = capture.feed(block, quiet=quiet_now)
        if event is None:
            continue
        result["events"].append(event.kind)
        if event.kind == "wake":
            result["wake"] = True
            result["timings"]["wake"] = time.monotonic() - started
            continue
        if event.kind == "segment":
            result["segments"] += 1
            stt_started = time.monotonic()
            part, _src, _meta = recognize_fn(
                event.blocks, engine, recognizer, status,
                wake_triggered=False, language=language)
            result["timings"]["stt"] = result["timings"].get("stt", 0) + time.monotonic() - stt_started
            if part:
                segments.append(part)
            continue
        if event.kind != "utterance":
            continue
        result["truncated"] = event.truncated
        result["gap"] = event.gap
        result["blocks_sent"] = event.blocks
        result["pcm"] = pcm16(event.blocks)
        stt_started = time.monotonic()
        transcript, source, meta = recognize_fn(
            event.blocks, engine, recognizer, status,
            wake_triggered=True, language=language)
        result["timings"]["stt"] = result["timings"].get("stt", 0) + time.monotonic() - stt_started
        text = " ".join(part for part in segments + [transcript] if part)
        result["transcript"], result["source"], result["meta"] = text, source, meta
        if dispatch is not None and text:
            dispatch(text)
        break
    return result


# ---------------------------------------------------------------- playback

class Playback:
    """Speak replies off the capture thread so wake-word barge-in stays live.

    BUG-060: `self_wake_likely`/`recently_spoke` suppress self-wake — while
    Navi is talking (and briefly after, when echo rings down) a wake must be
    confirmed by a second KWS hit before it is honoured. `cancel_event`
    still kills the player process for real barge-in (BUG-061).
    """

    suppress_after_seconds = 0.6

    def __init__(self):
        self.lock = threading.Lock()
        self.thread = None
        self.cancel_event = threading.Event()
        self.last_spoken = ""
        self.ended_at = 0.0
        self.confirm_interrupted = False

    def start(self, text, persona_name=None, language=None):
        with self.lock:
            if self.thread is not None and self.thread.is_alive():
                return False
            self.cancel_event = threading.Event()
            event = self.cancel_event
            self.last_spoken = text or ""
            self.thread = threading.Thread(
                target=self._speak, args=(text, persona_name, event, language),
                daemon=True)
            self.thread.start()
            return True

    def _speak(self, text, persona_name, cancel_event, language=None):
        try:
            speech_out.speak(text, persona_name, cancel_event=cancel_event,
                             language=language)
        except (OSError, RuntimeError):
            pass
        finally:
            with self.lock:
                self.ended_at = time.monotonic()

    def speaking(self):
        with self.lock:
            return self.thread is not None and self.thread.is_alive()

    def recently_spoke(self):
        """True while talking or inside the post-speech echo window."""
        with self.lock:
            if self.thread is not None and self.thread.is_alive():
                return True
            return bool(self.ended_at) and \
                time.monotonic() - self.ended_at < self.suppress_after_seconds

    def self_wake_likely(self):
        """Navi's own voice probably fired the KWS: playback is live (or just
        ended) and the spoken text itself names the wake word (BUG-060)."""
        return self.recently_spoke() and "navi" in self.last_spoken.lower()

    def cancel(self):
        with self.lock:
            self.cancel_event.set()


class StopDetector:
    """BUG-061: bare "stop" during playback, when the KWS only knows "Navi".

    A short speech burst during playback (0.3-1.5 s of non-quiet audio) is
    submitted to STT as a stop-check job; a stop-word result cancels
    playback. Cheap: it only inspects audio while Navi is speaking.
    """

    min_blocks = 3
    max_blocks = 15

    def __init__(self):
        self.burst = 0

    def feed(self, quiet):
        """Returns True when a speech burst ends during playback."""
        if not quiet:
            self.burst += 1
            if self.burst > self.max_blocks:
                self.burst = 0
                return True
            return False
        if self.burst >= self.min_blocks:
            self.burst = 0
            return True
        self.burst = 0
        return False


def is_stop(transcript):
    text = (transcript or "").strip().lower().strip(" .!?")
    return text in STOP_WORDS


class _StopCheck:
    """Marker event for a playback stop-word STT job (BUG-061)."""
    kind = "stop_check"
    truncated = False
    gap = False

    def __init__(self, blocks):
        self.blocks = blocks


# -------------------------------------------------------------------- live

def main():
    if os.environ.get("NAVI_ISOLATED") == "1":
        raise RuntimeError("Live microphone capture is disabled in isolated mode")
    playback = Playback()
    snapshot = StatusSnapshot(rpc)

    def on_result(result):
        if not isinstance(result, dict):
            return
        reply = result.get("reply")
        if isinstance(reply, dict):
            reply = reply.get("text")
        if not reply:
            return
        status = snapshot.value
        if status is not None:
            settings = status.get("settings", {})
        else:
            try:
                settings = rpc("status", timeout=2)["settings"]
            except (OSError, RuntimeError):
                settings = {}
        if not settings.get("speak"):
            return
        playback.start(reply, settings.get("persona"),
                       language=settings.get("speech", {}).get("language"))

    commands = CommandRouter(rpc, enabled, on_result)
    stop_watch = threading.Event()
    threading.Thread(target=watch_waiting, args=(snapshot, playback, stop_watch),
                     daemon=True).start()
    try:
        listen(commands, playback, snapshot)
    finally:
        stop_watch.set()
        commands.close()
        snapshot.close()


def _handle_transcript(transcript, source, meta, commands, loaded_speech,
                       snapshot, playback, event=None):
    """Harness route (BUG-065), confirm hold (BUG-056), then dispatch.

    The confirm window gives the UI speech_config.confirm_seconds() to show
    the transcript in the composer before it runs. Exactly one side dispatches
    it: the UI's Enter claims `voice.claim` first and this hold claims it at
    the deadline otherwise; a cancel (or a new wake) sends nothing at all.
    Returns the outcome string for voice_state.
    """
    clear_confirm()
    try:
        status = snapshot.value
        if not status or "settings" not in status:
            try:
                status = rpc("status", timeout=2)
            except (OSError, RuntimeError):
                status = None
        if status is None:
            return "discarded"
        route = harness_target(transcript, status)
        if route is not None:
            if route.get("busy"):
                return "session busy"
            try:
                rpc("prompt", {"id": route["id"], "text": route["text"]}, timeout=2)
                return f"sent to {route['name']}"
            except (OSError, RuntimeError):
                return "session unreachable"
        hold = speech_config.confirm_seconds()
        confidence = (meta or {}).get("confidence")
        # BUG-059: low-confidence text is confirmed, not silently executed —
        # it gets a tripled window flagged unsure so a wake can still veto it.
        unsure = confidence is not None and confidence < speech_config.min_confidence()
        if unsure:
            hold = hold * 3 if hold > 0 else 4.5
        details = {"transcript": transcript, "source": source,
                   "confidence": confidence, "unsure": unsure}
        if event is not None:
            details["truncated"] = event.truncated
            details["gap"] = event.gap
        if hold > 0:
            state("confirming", confirm_seconds=hold, **details)
            deadline = time.monotonic() + hold
            while time.monotonic() < deadline:
                if playback.confirm_interrupted:
                    return "interrupted"
                signal = confirm_signal()
                if signal == "cancel":
                    return "interrupted"
                if signal == "accept":
                    # The UI dispatched this text itself; dispatching again
                    # here would run the command twice (BUG-056).
                    return "confirmed by UI"
                time.sleep(.025)
            # Nobody accepted it: only the side that wins the O_EXCL claim may
            # run it, so an accept racing this deadline cannot double-run it.
            if not claim_dispatch():
                return "confirmed by UI"
        outcome = commands.dispatch(transcript, loaded_speech, status=status)
        if outcome == "queued":
            state("queued", **details)
        dropped = getattr(commands, "dropped", 0)
        if dropped:
            state("dropped earlier request", dropped=dropped)
        return outcome
    finally:
        # A winning claim outlives this turn until the next capture starts
        # (see clear_confirm in the wake path). Removing it here lets an Enter
        # pressed after the deadline claim afresh and dispatch the same text a
        # second time (BUG-056); the stale-claim test covers the next capture.
        if not CLAIM_FILE.exists():
            clear_confirm()


def listen(commands, playback=None, snapshot=None):
    playback = playback or Playback()
    owns_snapshot = snapshot is None
    snapshot = snapshot or StatusSnapshot(rpc)
    # Do not load speech libraries or open the microphone while disabled.
    spotter = recognizer = None
    engine = "whistle"
    language = "en"
    loaded_speech = None
    stt_queue = None

    def on_stt(payload, event):
        transcript, source, meta = payload
        if event.kind == "stop_check":
            # BUG-061: a bare "stop" spoken over playback cancels it.
            if is_stop(transcript):
                playback.cancel()
                playback.confirm_interrupted = True
                state("stopped")
                # Audible acknowledgement once the player thread unwinds.
                threading.Thread(
                    target=lambda: (time.sleep(0.15),
                                    playback.start("Stopped", language=language)),
                    daemon=True).start()
            return
        if event.kind == "segment":
            if transcript:
                state("transcribing", partial=transcript, source=source)
            return
        if not transcript:
            state("listening", wake_word="Navi")
            return
        playback.confirm_interrupted = False
        if getattr(event, "truncated", False):
            # BUG-053: the chat marker that the capture hit its 60 s cap.
            state("recording limit reached — text may be incomplete")
        outcome = _handle_transcript(
            transcript, source, meta, commands, loaded_speech, snapshot,
            playback, event)
        state("chief busy; say Navi, stop" if outcome == "busy" else outcome,
              transcript=transcript, source=source,
              confidence=meta.get("confidence"))
        state("listening", wake_word="Navi")

    try:
        while True:
            try:
                commands.poll()
                status = snapshot.value or rpc("status", timeout=2)
                if not enabled(status):
                    state("off" if not status["settings"]["microphone"] else "waiting for window")
                    time.sleep(.5)
                    continue
                import numpy as np
                import sounddevice as sd
                speech = status["settings"].get("speech", {})
                if spotter is None or speech != loaded_speech:
                    state("loading speech")
                    spotter = load_spotter(speech)
                    engine, recognizer = load_recognizer(speech)
                    loaded_speech = speech.copy()
                    language = speech.get("language") or speech_config.language()
                    if stt_queue is not None:
                        stt_queue.close()

                    def stt_job(blocks, **_unused):
                        return recognize(blocks, engine, recognizer, status,
                                         wake_triggered=True,
                                         language=language)

                    stt_queue = SttQueue(stt_job, on_result=on_stt)
                vad = AdaptiveVad()
                capture = CaptureBuffer(spotter)
                stop_detector = StopDetector()
                barge_in = False
                wake_hits = 0
                wake_last = 0.0
                last_gen = None
                last_cancel = cancel_stamp()
                state("listening", wake_word="Navi", vad=vad.state())
                with sd.InputStream(channels=1, dtype="float32", samplerate=16000,
                                    blocksize=1600, device=input_device(sd)) as microphone:
                    while True:
                        commands.poll()
                        stamp = cancel_stamp()
                        if stamp != last_cancel:
                            last_cancel = stamp
                            playback.cancel()
                        status = snapshot.value
                        if status is None:
                            # Cold start only; afterwards the last known
                            # snapshot serves until the poller recovers.
                            status = rpc("status", timeout=2)
                        generation = status.get("generation")
                        if last_gen is not None and generation is not None and generation != last_gen:
                            # A cancel/voice_stop on the daemon also silences
                            # playback (BUG-061 worker half).
                            playback.cancel()
                        if generation is not None:
                            last_gen = generation
                        if not enabled(status) or status["settings"].get("speech", {}) != loaded_speech:
                            break  # closes the physical capture stream
                        samples, overflow = microphone.read(1600)
                        samples = samples[:, 0]
                        quiet_now = not vad.feed(samples)
                        if stt_queue is not None:
                            stt_queue.remember(samples)
                        if playback.speaking():
                            # BUG-060/061: while Navi talks, a wake must be
                            # confirmed by a second hit and a bare "stop"
                            # burst goes to STT as a stop-check.
                            event = capture.feed(samples, quiet=quiet_now, overflow=overflow)
                            confirmed = False
                            if event is not None and event.kind == "wake":
                                wake_hits += 1
                                wake_last = time.monotonic()
                            elif wake_hits and time.monotonic() - wake_last > 2.0:
                                wake_hits = 0
                            needed = 2 if playback.self_wake_likely() else 1
                            if wake_hits >= needed and wake_hits > 0:
                                confirmed = True
                            if confirmed:
                                wake_hits = 0
                                playback.cancel()
                                if pops_window(status):
                                    pop_window()
                                barge_in = True
                                playback.confirm_interrupted = True
                                state("interrupted; listening")
                            elif stop_detector.feed(quiet_now) and stt_queue is not None:
                                blocks = tuple(stt_queue.ring)[-stop_detector.max_blocks:]
                                if blocks:
                                    stt_queue.submit(_StopCheck(blocks))
                            continue
                        wake_hits = 0
                        if barge_in:
                            # Drop Navi's trailing output, then take the rest
                            # of the user's command without another wake word.
                            barge_in = False
                            capture.reset()
                            capture.start_recording()
                            continue
                        event = capture.feed(samples, quiet=quiet_now, overflow=overflow)
                        if event is None:
                            continue
                        if event.kind == "overflow":
                            # Pre-wake hiccup: pre-roll dropped, keep listening.
                            state("listening", wake_word="Navi")
                        elif event.kind == "gap":
                            # BUG-055: a mid-command gap keeps capturing; the
                            # utterance still dispatches flagged.
                            state("audio gap; still listening")
                        elif event.kind == "no_command":
                            state("listening", wake_word="Navi")
                        elif event.kind == "wake":
                            playback.confirm_interrupted = True
                            # A new capture starts unclaimed (BUG-056).
                            clear_confirm()
                            if stt_queue is not None:
                                stt_queue.cancel_pending()
                            if pops_window(status):
                                pop_window()
                            state("hearing command")
                        elif event.kind == "segment":
                            # BUG-056: long captures stream ~10 s parts to STT.
                            if stt_queue is not None:
                                stt_queue.submit(event)
                            state("transcribing")
                        elif event.kind == "utterance":
                            # BUG-054: STT runs on its worker; the mic never stops.
                            state("transcribing", vad=vad.state())
                            if stt_queue is not None:
                                stt_queue.submit(event)
                            else:
                                payload = recognize(
                                    event.blocks, engine, recognizer, status,
                                    wake_triggered=True, language=language)
                                on_stt(payload, event)
            except (OSError, RuntimeError, ImportError, ValueError) as error:
                state("error", error=str(error))
                time.sleep(3)
    finally:
        if stt_queue is not None:
            stt_queue.close()
        if owns_snapshot:
            snapshot.close()
        _stop_state_writer()


def input_device(sd):
    """Prefer a PipeWire echo-cancel source, then PipeWire, then default.

    With PipeWire's module-echo-cancel loaded, the 'echo-cancel' source
    already subtracts Navi's own playback from the mic signal (BUG-060).
    """
    try:
        devices = sd.query_devices()
        for index, device in enumerate(devices):
            if device.get("max_input_channels", 0) > 0 and "echo" in (device.get("name") or "").casefold():
                return index
        for index, device in enumerate(devices):
            if device.get("max_input_channels", 0) > 0 and "pipewire" in (device.get("name") or "").casefold():
                return index
    except Exception:  # noqa: BLE001 - device enumeration is best effort
        pass
    return None


if __name__ == "__main__":
    try:
        main()
    except (KeyboardInterrupt, SystemExit):
        raise
    except BaseException:  # noqa: BLE001 - BUG-063: die loudly so the supervisor restarts
        traceback.print_exc()
        sys.stderr.write("[voice] fatal worker error; exiting non-zero\n")
        sys.stderr.flush()
        (RUNTIME / "voice.pid").unlink(missing_ok=True)
        os._exit(70)  # EX_SOFTWARE: deliberate non-zero exit
    finally:
        (RUNTIME / "voice.pid").unlink(missing_ok=True)
