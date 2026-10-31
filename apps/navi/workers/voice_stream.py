"""Bounded command capture and dispatch, independent of audio devices and ASR."""
from collections import deque
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass


@dataclass(frozen=True)
class CaptureEvent:
    kind: str
    blocks: tuple = ()


class CaptureBuffer:
    sample_rate = 16000
    block_size = 1600
    pre_roll_blocks = 30
    minimum_blocks = 15
    maximum_blocks = 200
    quiet_blocks = 9

    def __init__(self, spotter):
        self.spotter = spotter
        self.pre_roll = deque(maxlen=self.pre_roll_blocks)
        self.reset()

    def reset(self):
        self.pre_roll.clear()
        self.recording = None
        self.quiet = 0
        self.stream = self.spotter.create_stream()

    def start_recording(self):
        """Begin a command without a fresh wake phrase (barge-in)."""
        self.recording = list(self.pre_roll)
        self.pre_roll.clear()
        self.quiet = 0

    def feed(self, samples, *, quiet, overflow=False):
        if overflow:
            # Discontinuity invalidates both the wake hypothesis and its audio.
            self.reset()
            return CaptureEvent("overflow")
        if len(samples) != self.block_size:
            raise ValueError("Capture requires 100 ms mono blocks at 16 kHz")
        samples = samples.copy()
        if self.recording is None:
            self.pre_roll.append(samples)
            self.stream.accept_waveform(self.sample_rate, samples)
            while self.spotter.is_ready(self.stream):
                self.spotter.decode_stream(self.stream)
                if self.spotter.get_result(self.stream):
                    self.recording = list(self.pre_roll)
                    self.pre_roll.clear()
                    self.quiet = 0
                    return CaptureEvent("wake")
            return None
        self.recording.append(samples)
        self.quiet = self.quiet + 1 if quiet else 0
        if ((self.quiet >= self.quiet_blocks and len(self.recording) >= self.minimum_blocks)
                or len(self.recording) >= self.maximum_blocks):
            blocks = tuple(self.recording)
            self.reset()
            return CaptureEvent("utterance", blocks)
        return None


class CommandRouter:
    """One in-flight chief request; Stop bypasses it and nothing is queued."""
    def __init__(self, rpc, enabled, on_result=None):
        self.rpc = rpc
        self.enabled = enabled
        self.on_result = on_result
        self.pool = ThreadPoolExecutor(max_workers=1)
        self.pending = None

    def poll(self):
        if self.pending is not None and self.pending.done():
            completed = self.pending
            self.pending = None
            result = completed.result()
            if self.on_result is not None:
                self.on_result(result)

    def dispatch(self, transcript, speech):
        status = self.rpc("status", timeout=2)
        if not self.enabled(status) or status["settings"].get("speech", {}) != speech:
            return "discarded"
        if transcript.lower() in ("stop", "cancel", "stop navi"):
            self.rpc("cancel", timeout=2)
            return "stopped"
        self.poll()
        if self.pending is not None:
            return "busy"
        self.pending = self.pool.submit(self.rpc, "command", {"text": transcript})
        return "deciding"

    def close(self):
        self.pool.shutdown(wait=False, cancel_futures=True)
