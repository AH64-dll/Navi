"""English wake-word capture followed by CPU transcription, then the Rust chief."""
import os
os.environ["NEEDLE_TELEMETRY"] = "0"
os.environ["DO_NOT_TRACK"] = "1"
from pathlib import Path
import re
import threading
import time

from ipc import DATA, RUNTIME, rpc
import speech_config
import speech_out
from voice_stream import CaptureBuffer, CommandRouter

engine_library = DATA / "runtime/needle3/libneedle.so"
if engine_library.is_file():
    os.environ["NEEDLE3_LIB_PATH"] = str(engine_library)

WAKE_FOLDER = Path("models/wake/sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01")


def enabled(status):
    settings = status["settings"]
    return settings["microphone"] and (settings["listening"] == "background" or status["gui_open"])


def state(status, **details):
    try:
        rpc("voice_state", {"status": status, **details}, timeout=2)
    except (OSError, RuntimeError):
        pass


def command_text(transcript):
    return re.sub(r"^h(?:ey|ay)[\s,.!?]+navi\b[\s,.!?]*", "", transcript.strip(), flags=re.IGNORECASE).strip(" ,.!?")


def speech_keywords(status):
    settings = status["settings"]
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


# ------------------------------------------------------------------ loading

def load_spotter(speech, data=DATA):
    """Build the local wake-word spotter; wake detection never uses the cloud."""
    import sherpa_onnx
    folder = Path(data) / WAKE_FOLDER
    return sherpa_onnx.KeywordSpotter(
        tokens=str(folder / "tokens.txt"),
        encoder=str(folder / "encoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx"),
        decoder=str(folder / "decoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx"),
        joiner=str(folder / "joiner-epoch-12-avg-2-chunk-16-left-64.int8.onnx"),
        keywords_file=str(folder / "navi-keywords.txt"),
        num_threads=1,
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


def recognize(blocks, engine, recognizer, status, cloud=True):
    """Cloud STT first unless local-only; fall back to the loaded local engine.

    Returns (transcript, source). With no provider credentials the cloud hop
    returns immediately, before any audio conversion or upload.
    """
    transcript = ""
    source = "local"
    import numpy as np
    if cloud and speech_config.speech_mode() != "local-only":
        try:
            import cloud_stt
            if cloud_stt.available():
                transcript = cloud_stt.transcribe(pcm16(blocks), 16000, language="en") or ""
                source = "cloud"
        except Exception:  # noqa: BLE001 - cloud STT is an optional hop
            transcript = ""
    if not transcript:
        source = "local"
        if engine == "whistle":
            transcript = recognizer.transcribe(
                np.concatenate(blocks), language="en",
                keywords=speech_keywords(status),
            )["text"]
        else:
            utterance = recognizer.create_stream()
            utterance.accept_waveform(16000, np.concatenate(blocks))
            recognizer.decode_stream(utterance)
            transcript = utterance.result.text
    return command_text(transcript), source


def offline(samples, spotter, recognizer, engine, status=None, *, assume_wake=False,
            dispatch=None, quiet=.008, tail_blocks=15):
    """Run the live wake/endpoint/STT path over recorded float32 samples.

    `samples` is mono 16 kHz float32. `tail_blocks` of silence flush the last
    utterance. When the wake detector misses synthetic speech, call again with
    `assume_wake=True` and only the post-wake audio. Returns transcript,
    source, wake flag and per-stage wall-clock timings in seconds.
    """
    import numpy as np
    status = status or {"settings": {"speech": {}, "profiles": [], "projects": [], "favorites": [], "sessions": []}}
    capture = CaptureBuffer(spotter)
    if assume_wake:
        capture.start_recording()
    block_size = CaptureBuffer.block_size
    padding = (-len(samples)) % block_size
    if padding:
        samples = np.concatenate([samples, np.zeros(padding, dtype=np.float32)])
    tail = [np.zeros(block_size, dtype=np.float32)] * tail_blocks
    result = {"wake": assume_wake, "transcript": None, "source": None, "timings": {}}
    started = time.monotonic()
    for block in [samples[index:index + block_size] for index in range(0, len(samples), block_size)] + tail:
        energy = float(np.sqrt(np.mean(block ** 2)))
        event = capture.feed(block, quiet=energy < quiet)
        if event is None:
            continue
        if event.kind == "wake":
            result["wake"] = True
            result["timings"]["wake"] = time.monotonic() - started
            continue
        if event.kind != "utterance":
            continue
        stt_started = time.monotonic()
        transcript, source = recognize(event.blocks, engine, recognizer, status)
        result["timings"]["stt"] = time.monotonic() - stt_started
        result["transcript"], result["source"] = transcript, source
        if dispatch is not None and transcript:
            dispatch(transcript)
        break
    return result


# ---------------------------------------------------------------- playback

class Playback:
    """Speak replies off the capture thread so wake-word barge-in stays live."""

    def __init__(self):
        self.lock = threading.Lock()
        self.thread = None
        self.cancel_event = threading.Event()

    def start(self, text, persona_name=None):
        with self.lock:
            if self.thread is not None and self.thread.is_alive():
                return False
            self.cancel_event = threading.Event()
            event = self.cancel_event
            self.thread = threading.Thread(
                target=self._speak, args=(text, persona_name, event), daemon=True)
            self.thread.start()
            return True

    @staticmethod
    def _speak(text, persona_name, cancel_event):
        try:
            speech_out.speak(text, persona_name, cancel_event=cancel_event)
        except (OSError, RuntimeError):
            pass

    def speaking(self):
        with self.lock:
            return self.thread is not None and self.thread.is_alive()

    def cancel(self):
        with self.lock:
            self.cancel_event.set()


def input_device(sd):
    """Prefer a PipeWire input by name, otherwise use the system default."""
    try:
        for index, device in enumerate(sd.query_devices()):
            if device.get("max_input_channels", 0) > 0 and "pipewire" in (device.get("name") or "").casefold():
                return index
    except Exception:  # noqa: BLE001 - device enumeration is best effort
        pass
    return None


# -------------------------------------------------------------------- live

def main():
    if os.environ.get("NAVI_ISOLATED") == "1":
        raise RuntimeError("Live microphone capture is disabled in isolated mode")
    playback = Playback()

    def on_result(result):
        if not isinstance(result, dict):
            return
        reply = result.get("reply")
        if isinstance(reply, dict):
            reply = reply.get("text")
        if not reply:
            return
        try:
            settings = rpc("status", timeout=2)["settings"]
        except (OSError, RuntimeError):
            settings = {}
        playback.start(reply, settings.get("persona"))

    commands = CommandRouter(rpc, enabled, on_result)
    try:
        listen(commands, playback)
    finally:
        commands.close()


def listen(commands, playback=None):
    playback = playback or Playback()
    # Do not load speech libraries or open the microphone while disabled.
    spotter = recognizer = None
    engine = "whistle"
    loaded_speech = None
    while True:
        try:
            commands.poll()
            status = rpc("status", timeout=2)
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
            capture = CaptureBuffer(spotter)
            last_status = 0
            barge_in = False
            state("listening", wake_word="Hey Navi")
            with sd.InputStream(channels=1, dtype="float32", samplerate=16000,
                                blocksize=1600, device=input_device(sd)) as microphone:
                while True:
                    commands.poll()
                    if time.monotonic() - last_status > .4:
                        status = rpc("status", timeout=2)
                        last_status = time.monotonic()
                        if not enabled(status) or status["settings"].get("speech", {}) != loaded_speech:
                            break  # context manager closes the physical capture stream
                    samples, overflow = microphone.read(1600)
                    samples = samples[:, 0]
                    quiet = float(np.sqrt(np.mean(samples ** 2))) < .008
                    if playback.speaking():
                        # Mute command capture while Navi talks: only the wake
                        # detector runs, and a wake word cancels playback.
                        event = capture.feed(samples, quiet=quiet, overflow=overflow)
                        if event is not None and event.kind == "wake":
                            playback.cancel()
                            barge_in = True
                            state("interrupted; listening")
                        continue
                    if barge_in:
                        # Drop Navi's trailing output, then take the rest of the
                        # user's command without requiring another wake word.
                        barge_in = False
                        capture.reset()
                        capture.start_recording()
                        continue
                    event = capture.feed(samples, quiet=quiet, overflow=overflow)
                    if event is None:
                        continue
                    if event.kind == "overflow":
                        state("audio overflow; say Hey Navi again")
                    elif event.kind == "wake":
                        state("hearing command")
                    elif event.kind == "utterance":
                        status = rpc("status", timeout=2)
                        if not enabled(status) or status["settings"].get("speech", {}) != loaded_speech:
                            break
                        state("transcribing")
                        transcript, source = recognize(event.blocks, engine, recognizer, status)
                        if transcript:
                            # Only complete utterances reach the same Rust chief.
                            outcome = commands.dispatch(transcript, loaded_speech)
                            state("chief busy; say Hey Navi, stop" if outcome == "busy" else outcome,
                                  transcript=transcript, source=source)
                        state("listening", wake_word="Hey Navi")
        except (OSError, RuntimeError, ImportError, ValueError) as error:
            state("error", error=str(error))
            time.sleep(3)


if __name__ == "__main__":
    try:
        main()
    finally:
        (RUNTIME / "voice.pid").unlink(missing_ok=True)
