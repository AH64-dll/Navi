"""Bounded command capture and dispatch, independent of audio devices and ASR.

CaptureBuffer produces trimmed utterances: the audio that detected the wake
(pre-roll, wake word, the post-wake pause) is never part of the cloud-bound
buffer (BUG-049). Overflow is a marked gap, not a reset (BUG-055). A hard
cap emits a truncated utterance instead of silently dropping dictation, and
long captures stream out as ~10 s segments (BUG-053). AdaptiveVad replaces
the fixed .008 RMS constant with a calibrated noise floor (BUG-051) and
short commands commit on a fast endpoint (BUG-052). SttQueue keeps
recognition off the capture thread (BUG-054), StatusSnapshot keeps status
polling off it (BUG-064), and CommandRouter queues a follow-up transcript
instead of dropping it (BUG-062).
"""
from collections import deque
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
import queue
import sys
import threading
import time


class MicWatchdog:
    """A microphone read blocks with no timeout, so a stalled audio device (after
    a suspend, or a PipeWire restart) would leave Navi deaf with nothing to show
    for it. The capture loop marks each read; `stalled` reports one that never
    returned, and the worker exits so the daemon restarts it."""

    limit = 15.0

    def __init__(self):
        self.reading_since = None

    def begin(self, now=None):
        self.reading_since = time.monotonic() if now is None else now

    def end(self):
        self.reading_since = None

    def stalled(self, now=None):
        now = time.monotonic() if now is None else now
        return self.reading_since is not None and now - self.reading_since > self.limit


class SilenceWatch:
    """A real microphone is never digitally zero. Exact zeros for this many blocks
    mean the stream is dead or muted upstream, and reopening it is the fix."""

    limit_blocks = 300

    def __init__(self):
        self.zero_blocks = 0

    def feed(self, samples):
        """True once per dead run, when the stream should be reopened."""
        if any(samples):
            self.zero_blocks = 0
            return False
        self.zero_blocks += 1
        if self.zero_blocks >= self.limit_blocks:
            self.zero_blocks = 0
            return True
        return False


@dataclass(frozen=True)
class CaptureEvent:
    kind: str
    blocks: tuple = ()
    gap: bool = False
    truncated: bool = False


class AdaptiveVad:
    """Noise-floor calibrated energy gate for speech detection (BUG-051).

    The floor is a low percentile of the last ~30 s of block energy. Speaking
    cannot raise it, because even continuous speech has quiet gaps between
    words and those set the percentile. A lasting change in the room does move
    it, within the window. Blending speech into the floor is what used to end a
    command mid-sentence: the threshold climbed past the voice in about a second.
    A block is "speech" when its RMS exceeds floor x factor, never below a
    hardware minimum so a silent room cannot drive the threshold to zero.
    """

    def __init__(self, floor=0.004, minimum=0.001, factor=3.0,
                 calibrate_blocks=10, window_blocks=300, percentile=0.1, ceiling=0.05):
        self.floor = floor
        self.minimum = minimum
        self.factor = factor
        self.calibrate_blocks = calibrate_blocks
        self.percentile = percentile
        self.ceiling = ceiling
        self.recent = deque(maxlen=window_blocks)
        self.calibrated = False

    def feed(self, samples):
        rms = float((sum(float(s) * float(s) for s in samples) / max(len(samples), 1)) ** .5)
        threshold = max(self.floor * self.factor, self.minimum)
        if not self.calibrated:
            # While calibrating, energy above the default floor still counts
            # as speech so the first utterance is not lost to a loud room.
            threshold = max(self.floor, self.minimum)
        self.recent.append(rms)
        if len(self.recent) >= self.calibrate_blocks:
            ordered = sorted(self.recent)
            quiet = ordered[int(len(ordered) * self.percentile)]
            self.floor = max(self.minimum, min(quiet, self.ceiling))
            self.calibrated = True
        return rms > threshold

    def state(self):
        return {"floor": round(self.floor, 5), "calibrated": self.calibrated}


class SpeechVad:
    """Neural speech detection (Silero), the live gate when its model is installed.

    An energy gate needs speech to be several times louder than the room. A laptop's
    digital microphone can have a room at RMS 0.001 and a voice at 0.004, where the
    gate closes on soft syllables and ends a 24 s sentence at 1.5 s. Silero recognises
    speech rather than loudness, and held that sentence at every level from 0.002 to
    0.05 while staying quiet on fan noise and mains hum.

    The threshold is low on purpose: a false "speech" costs a slightly later endpoint,
    and a false "quiet" cuts the user off. sherpa keeps speech in its own buffer until
    a segment closes, so segments are discarded and the longest one is bounded just
    above the 5 min capture cap.
    """

    def __init__(self, model, threshold=0.3):
        import sherpa_onnx
        config = sherpa_onnx.VadModelConfig()
        config.silero_vad.model = str(model)
        config.silero_vad.threshold = threshold
        config.silero_vad.min_silence_duration = 0.25
        config.silero_vad.min_speech_duration = 0.1
        config.silero_vad.max_speech_duration = 302
        config.silero_vad.window_size = 512
        config.sample_rate = 16000
        config.num_threads = 1
        self.detector = sherpa_onnx.VoiceActivityDetector(config, buffer_size_in_seconds=305)

    def feed(self, samples):
        self.detector.accept_waveform(samples)
        while not self.detector.empty():
            self.detector.pop()
        return bool(self.detector.is_speech_detected())

    def state(self):
        return {"kind": "silero"}


def agc(blocks, target=0.1, max_gain=12.0):
    """Soft automatic gain before STT (BUG-051): lift quiet captures toward
    *target* RMS, capped at +12 dB-equivalent, never attenuating."""
    import math
    total = 0.0
    count = 0
    for block in blocks:
        for sample in block:
            total += float(sample) * float(sample)
            count += 1
    rms = math.sqrt(total / count) if count else 0.0
    if rms <= 0 or rms >= target:
        return tuple(blocks)
    gain = min(target / rms, max_gain)
    return tuple([float(s) * gain for s in block] for block in blocks)


def trim_span(blocks, speech_start, last_speech, wake_mark=-1,
              *, lead_blocks=2, tail_blocks=3):
    """BUG-049: the cloud-bound window of a raw recording.

    `speech_start`/`last_speech` are indices of the first/last non-quiet
    captured block (None when nothing registered). The slice begins ~200 ms
    before onset — enough lead-in not to clip soft first syllables — clamped
    to the wake mark so wake-word audio can never reach STT, and ends ~300 ms
    after the last speech block. With no detected speech the raw capture is
    returned so the STT layer still decides.
    """
    if speech_start is None:
        return tuple(blocks)
    start = max(0, speech_start - lead_blocks, wake_mark + 1)
    end = len(blocks) if last_speech is None else min(
        len(blocks), last_speech + 1 + tail_blocks)
    return tuple(blocks[start:end])


class CaptureBuffer:
    sample_rate = 16000
    block_size = 1600
    pre_roll_blocks = 30
    minimum_blocks = 15        # wake path: >=1.5 s captured before endpointing
    maximum_blocks = 3000      # 5 min dictation cap (BUG-053), only a safety stop
    quiet_blocks = 40          # 4 s without speech commits an utterance
    segment_blocks = 100       # stream a "segment" every ~10 s of speech
    segment_overlap = 2        # 200 ms overlap to de-duplicate word boundaries
    # BUG-052 fast endpoint: short utterances commit once >=600 ms was captured.
    # The wait matches the long one on purpose. People pause longest right after a
    # short opening ("Can you open the project ... and check"), and any shorter
    # wait kept those first words, sent them, and lost the rest of the sentence.
    fast_quiet_blocks = 40
    fast_total_blocks = 6
    fast_speech_cap = 20       # utterances with <=2 s speech are "short"
    lead_blocks = 2
    tail_blocks = 5            # soft word endings often fall under the threshold
    # After a wake, a natural pause before the request must not end the
    # command. Wait this long for speech to start, then endpoint normally.
    wake_grace_blocks = 60
    # The acknowledgement chime plays right after a wake. These blocks count as
    # quiet so the chime cannot end the command as if it were speech; they stay
    # in the recording, so a command spoken straight away keeps its first word.
    ack_blocks = 2

    def __init__(self, spotter, fast_endpoint=True):
        self.spotter = spotter
        self.fast_endpoint = fast_endpoint
        self.pre_roll = deque(maxlen=self.pre_roll_blocks)
        self._barge_in = False
        self._wake_mark = -1
        self.reset()

    def reset(self):
        self.pre_roll.clear()
        self.recording = None
        self.quiet = 0
        self.speech_blocks = 0
        self.speech_start = None
        self.last_speech = None
        self.awaiting_speech = False
        self.wake_quiet = 0
        self.gap = False
        self.truncated = False
        self._segment_mark = 0
        self._wake_mark = -1
        self._barge_in = False
        self._ack_left = 0
        self.stream = self.spotter.create_stream()

    def start_recording(self):
        """Begin a command without a fresh wake phrase (barge-in).

        _wake_mark stays -1: there is no wake-word audio to exclude, so every
        appended block counts toward the endpoint minimum and only the VAD
        speech span is trimmed (the captured pre-roll still holds the tail
        of Navi's reply, which the trim drops as non-speech).
        """
        self._begin()
        self._barge_in = True

    def start_follow_up(self):
        """Listen for the answer to a spoken question without a wake word.

        Like a fresh wake, silence only counts against the grace window, so
        an unanswered question ends as `no_command` instead of an empty
        utterance.
        """
        self._begin()
        self.awaiting_speech = True
        self._barge_in = True

    @property
    def barge_in(self):
        return self._barge_in

    def _captured(self):
        """Blocks appended since the mark (post-wake command audio)."""
        if self.recording is None:
            return 0
        return len(self.recording) - self._wake_mark - 1

    def _trim_start(self):
        return max(0, (self.speech_start or 0) - self.lead_blocks,
                   self._wake_mark + 1)

    def finish_now(self):
        """The user pressed Transcribe now: send what was said, without waiting for
        silence. Nothing said yet ends the capture instead of sending silence."""
        if self.recording is None:
            return None
        if self.speech_start is None:
            self.reset()
            return CaptureEvent("no_command")
        return self._emit_utterance(keep_tail=True)

    def _emit_utterance(self, keep_tail=False):
        start = self._trim_start()
        # A press can land on the last word, before the detector has marked it.
        end = len(self.recording) if self.last_speech is None or keep_tail else min(
            len(self.recording), self.last_speech + 1 + self.tail_blocks)
        event = CaptureEvent(
            "utterance", tuple(self.recording[start:end]),
            gap=self.gap, truncated=self.truncated)
        self.reset()
        return event

    def _begin(self):
        """Shared wake/barge-in recording initialisation."""
        self.recording = list(self.pre_roll)
        self.pre_roll.clear()
        self.quiet = 0
        self.speech_blocks = 0
        self.speech_start = None
        self.last_speech = None
        self.awaiting_speech = False
        self.wake_quiet = 0
        self.gap = False
        self.truncated = False
        self._segment_mark = 0
        self._wake_mark = -1
        self._ack_left = 0

    def _heard_wake(self, samples):
        """Run one block through the KWS. The caller keeps it live in every state."""
        self.stream.accept_waveform(self.sample_rate, samples)
        while self.spotter.is_ready(self.stream):
            self.spotter.decode_stream(self.stream)
            if self.spotter.get_result(self.stream):
                return True
        return False

    def _wake(self):
        """Start a command after a wake. The KWS restarts so it can hear the next."""
        self.stream = self.spotter.create_stream()
        self._barge_in = False
        self._begin()
        self.awaiting_speech = True
        self._wake_mark = len(self.recording) - 1
        self._ack_left = self.ack_blocks
        return CaptureEvent("wake", tuple(self.recording))

    def feed(self, samples, *, quiet, overflow=False):
        if len(samples) != self.block_size:
            raise ValueError("Capture requires 100 ms mono blocks at 16 kHz")
        samples = samples.copy()
        if overflow:
            # A lost block is a gap, not a reason to erase the command
            # (BUG-055). The KWS stream state is stale, so it is rebuilt;
            # captured speech is kept and flagged. Only a pre-wake pre-roll
            # is invalidated, since it feeds the wake hypothesis.
            self.stream = self.spotter.create_stream()
            if self.recording is not None:
                self.gap = True
                return CaptureEvent("gap")
            self.pre_roll.clear()
            return CaptureEvent("overflow")
        if self.recording is None:
            self.pre_roll.append(samples)
            return self._wake() if self._heard_wake(samples) else None
        if self._heard_wake(samples):
            # Before any speech, this is a repeated call. Once speech has begun it is
            # a word in the sentence ("open Navi settings", or "navy" misheard), and
            # starting over would throw away everything said so far.
            if self.awaiting_speech:
                self.pre_roll.append(samples)
                return self._wake()
            self.stream = self.spotter.create_stream()
        if self._ack_left:
            self._ack_left -= 1
            quiet = True
        if self.awaiting_speech:
            if quiet:
                # The pause is stored (it becomes the ~200 ms lead-in before
                # speech onset, BUG-049) but counts only against the grace,
                # never the endpoint run. A pause beyond the grace abandons
                # the wake hypothesis.
                self.recording.append(samples)
                self.wake_quiet += 1
                if self.wake_quiet >= self.wake_grace_blocks:
                    self.reset()
                    return CaptureEvent("no_command")
                return None
            self.awaiting_speech = False
            self.wake_quiet = 0
            self.quiet = 0
        self.recording.append(samples)
        if quiet:
            self.quiet += 1
        else:
            self.quiet = 0
            self.speech_blocks += 1
            index = len(self.recording) - 1
            if self.speech_start is None:
                self.speech_start = index
            self.last_speech = index
        captured = self._captured()
        quiet_limit = self.quiet_blocks
        minimum = self.minimum_blocks
        if self.fast_endpoint and not self._barge_in and self.speech_blocks <= self.fast_speech_cap:
            quiet_limit = self.fast_quiet_blocks
            minimum = self.fast_total_blocks
        if len(self.recording) >= self.maximum_blocks:
            self.truncated = True
            return self._emit_utterance()
        if self.quiet >= quiet_limit and captured >= minimum:
            return self._emit_utterance()
        over = captured - self._segment_mark
        if self.speech_blocks and over >= self.segment_blocks and (
                self.quiet > 0 or over >= self.segment_blocks * 2):
            # Long capture: emit the completed ~10 s span for streaming STT.
            # Prefer a pause boundary; continuous speech cuts at 2x the span
            # with a 200 ms overlap to de-duplicate word edges (BUG-053).
            mark_index = self._wake_mark + 1 + self._segment_mark
            start = max(self._trim_start(), mark_index - self.segment_overlap)
            end = len(self.recording) - self.quiet if self.quiet else len(self.recording)
            self._segment_mark = end - self._wake_mark - 1
            return CaptureEvent("segment", tuple(self.recording[start:end]), gap=self.gap)
        return None


class SttQueue:
    """BUG-054: single-consumer STT worker; the capture thread only enqueues.

    Bounded pending slots (default 2): a new job while full drops the oldest
    *pending* job, never the running one. A ring of recent mic blocks keeps
    the last ~3 s so a wake fired during transcription still has pre-roll.
    """

    def __init__(self, recognize, on_result=None, bound=2, ring_blocks=30):
        self.recognize = recognize
        self.on_result = on_result
        self.bound = bound
        self.jobs = queue.Queue(maxsize=bound)
        self.ring = deque(maxlen=ring_blocks)
        self.dropped = 0
        self.busy = threading.Event()
        self._cancel = threading.Event()
        self._worker = threading.Thread(target=self._run, daemon=True)
        self._worker.start()

    def remember(self, samples):
        """Ring of recent mic blocks for a wake that lands during STT."""
        self.ring.append(samples)

    def submit(self, event, *args, **kwargs):
        self._cancel.clear()
        job = (event, args, kwargs)
        try:
            self.jobs.put_nowait(job)
        except queue.Full:
            try:
                self.jobs.get_nowait()
                self.dropped += 1
            except queue.Empty:
                pass
            self.jobs.put_nowait(job)

    def cancel_pending(self):
        """New wake: pending transcripts are stale; the running job finishes
        but its result is dropped."""
        self._cancel.set()
        while True:
            try:
                self.jobs.get_nowait()
                self.dropped += 1
            except queue.Empty:
                return

    def _run(self):
        while True:
            event, args, kwargs = self.jobs.get()
            if event is None:
                return
            if self._cancel.is_set():
                self.dropped += 1
                continue
            self.busy.set()
            try:
                result = self.recognize(event.blocks, *args, **kwargs)
            except Exception:  # noqa: BLE001 - a bad utterance kills its job only
                result = None
            finally:
                self.busy.clear()
            if self._cancel.is_set():
                self.dropped += 1
                continue
            if self.on_result is not None and result:
                self.on_result(result, event)

    def close(self):
        self.jobs.put((None, (), {}))
        self._worker.join(timeout=2)


class StatusSnapshot:
    """BUG-064: daemon status polled on its own thread; readers never block.

    `value` is the last successful status dict (None until the first poll
    completes) and `age` is seconds since it arrived. `refresh()` asks for a
    poll soon; it never performs one inline.
    """

    def __init__(self, rpc, interval=0.4, timeout=2):
        self.rpc = rpc
        self.interval = interval
        self.timeout = timeout
        self._value = None
        self._stamp = 0.0
        self._lock = threading.Lock()
        self._stop = threading.Event()
        self._poke = threading.Event()
        self._thread = threading.Thread(target=self._poll, daemon=True)
        self._thread.start()

    def refresh(self):
        self._poke.set()

    @property
    def value(self):
        with self._lock:
            return self._value

    @property
    def age(self):
        return time.monotonic() - self._stamp if self._stamp else float("inf")

    def _poll(self):
        while not self._stop.is_set():
            try:
                status = self.rpc("status", timeout=self.timeout)
            except Exception:  # noqa: BLE001 - a stall must not kill the poller
                status = None
            if status is not None:
                with self._lock:
                    self._value = status
                    self._stamp = time.monotonic()
            self._poke.wait(self.interval)
            self._poke.clear()

    def close(self):
        self._stop.set()
        self._poke.set()
        self._thread.join(timeout=2)


class CommandRouter:
    """One in-flight chief request plus a bounded follow-up queue (BUG-062).

    A transcript while the chief runs waits in `queued` (cap 2); overflow
    keeps the newest and drops the oldest. Stop bypasses the queue and drops
    it. `dispatch` reads a status snapshot rather than polling (BUG-064).
    """

    queue_cap = 2

    def __init__(self, rpc, enabled, on_result=None):
        self.rpc = rpc
        self.enabled = enabled
        self.on_result = on_result
        self.pool = ThreadPoolExecutor(max_workers=1)
        self.pending = None
        self.queued = deque(maxlen=self.queue_cap)
        self.dropped = 0

    def poll(self):
        if self.pending is not None and self.pending.done():
            completed = self.pending
            self.pending = None
            try:
                result = completed.result()
            except (OSError, RuntimeError) as error:
                # A model turn can outlast the RPC timeout while the daemon keeps
                # working. Raising here would stop the capture loop for seconds.
                sys.stderr.write(f"[voice] command reply lost: {error}\n")
                sys.stderr.flush()
                result = None
            if self.on_result is not None:
                self.on_result(result)
        if self.pending is None and self.queued:
            transcript = self.queued.popleft()
            self.pending = self.pool.submit(self.rpc, "command", {"text": transcript})
            return "queued->deciding"
        return None

    def dispatch(self, transcript, speech, status=None):
        """Route a transcript; `status` is a snapshot dict, not an RPC call."""
        if status is None:
            status = self.rpc("status", timeout=2)
        if not self.enabled(status) or status["settings"].get("speech", {}) != speech:
            return "discarded"
        if transcript.lower() in ("stop", "cancel", "stop navi"):
            self.queued.clear()
            self.rpc("cancel", timeout=2)
            return "stopped"
        self.poll()
        if self.pending is not None:
            if len(self.queued) == self.queue_cap:
                self.dropped += 1
            self.queued.append(transcript)
            return "queued"
        self.pending = self.pool.submit(self.rpc, "command", {"text": transcript})
        return "deciding"

    def close(self):
        self.pool.shutdown(wait=False, cancel_futures=True)
