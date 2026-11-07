"""Streaming and dispatch regressions; no speech models or audio devices.

Covers BUG-049 (wake/pause audio never reaches STT), BUG-051 (adaptive VAD +
AGC), BUG-052 (fast endpoint), BUG-053 (segments + truncation marker),
BUG-054 (STT queue), BUG-055 (overflow as a gap), BUG-062 (queued commands)
and BUG-064 (status snapshot).
"""
from pathlib import Path
import sys
import threading
import time
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "workers"))
from voice import enabled
from voice_stream import (AdaptiveVad, CaptureBuffer, CommandRouter, MicWatchdog, SpeechVad,
                          SilenceWatch, StatusSnapshot, SttQueue, agc, trim_span)


class WakeStream:
    def __init__(self):
        self.ready = False
        self.wake = False

    def accept_waveform(self, sample_rate, samples):
        assert sample_rate == 16000
        self.ready = True
        self.wake = samples[0] == -1

    # A real spotter also exposes reset_stream; the buffer never relies on it.


class WakeSpotter:
    def create_stream(self):
        return WakeStream()

    def is_ready(self, stream):
        return stream.ready

    def decode_stream(self, stream):
        stream.ready = False

    def get_result(self, stream):
        return "HEY NAVI" if stream.wake else ""


class CaptureTests(unittest.TestCase):
    def setUp(self):
        self.capture = CaptureBuffer(WakeSpotter())
        # Timing tests count speech from the first block; the chime has its own test.
        self.capture.ack_blocks = 0
        self.fast = self.capture.fast_quiet_blocks
        self.slow = self.capture.quiet_blocks

    def feed(self, value=0, *, quiet=False, overflow=False):
        return self.capture.feed([value] * 1600, quiet=quiet, overflow=overflow)

    def wake(self):
        for value in range(14):
            self.assertIsNone(self.feed(value))
        self.assertEqual(self.feed(-1).kind, "wake")

    def test_no_utterance_without_wake_and_pre_roll_is_bounded(self):
        for _ in range(500):
            self.assertIsNone(self.feed())
        self.assertEqual(len(self.capture.pre_roll), 30)
        self.assertIsNone(self.capture.recording)

    def test_wake_and_pause_audio_never_reach_the_utterance(self):
        """BUG-049: pre-roll + wake word + post-wake pause are trimmed."""
        self.wake()
        for _ in range(20):  # two seconds of silence before the request
            self.assertIsNone(self.feed(quiet=True))
        self.assertEqual(len(self.capture.recording), 35)  # pause stored, not speech
        self.assertIsNone(self.feed(100))  # the request starts here
        for _ in range(self.fast - 1):
            self.assertIsNone(self.feed(quiet=True))
        event = self.feed(quiet=True)
        self.assertEqual(event.kind, "utterance")
        # Emitted audio starts ~200 ms before onset: a lead-in of pause
        # blocks, then the command, then the trimmed quiet tail. The wake
        # word and the earlier pause/pre-roll never reach STT.
        values = [block[0] for block in event.blocks]
        self.assertEqual(values, [0, 0, 100] + [0] * self.capture.tail_blocks)
        self.assertNotIn(-1, values)

    def test_lead_in_keeps_soft_onset_but_excludes_wake(self):
        """BUG-049: the ~200 ms lead-in keeps pre-onset blocks, never wake audio."""
        self.wake()  # wake mark = index 14 (the -1 block)
        self.assertIsNone(self.feed(50, quiet=True))   # captured 0
        self.assertIsNone(self.feed(60))                # captured 1 = onset
        for _ in range(self.fast - 1):
            self.assertIsNone(self.feed(quiet=True))
        event = self.feed(quiet=True)
        values = [block[0] for block in event.blocks]
        self.assertNotIn(-1, values)
        self.assertEqual(values[:2], [50, 60])  # pause lead-in, then onset
    def test_immediate_command_commits_on_the_fast_endpoint(self):
        """BUG-052: a short command ends 1 s after its last speech block."""
        self.wake()
        self.assertIsNone(self.feed(100))
        for _ in range(self.fast - 1):
            self.assertIsNone(self.feed(quiet=True))
        self.assertEqual(self.feed(quiet=True).kind, "utterance")

    def test_a_thinking_pause_does_not_end_a_short_command(self):
        """"open the ... settings": the words after an 800 ms pause are kept."""
        self.wake()
        self.assertIsNone(self.feed(1))
        for _ in range(8):
            self.assertIsNone(self.feed(quiet=True))
        self.assertIsNone(self.feed(2))
        for _ in range(self.fast - 1):
            self.assertIsNone(self.feed(quiet=True))
        values = [block[0] for block in self.feed(quiet=True).blocks]
        self.assertIn(1, values)
        self.assertIn(2, values)

    def test_chime_after_a_wake_is_not_taken_for_speech(self):
        self.capture.ack_blocks = 2
        self.wake()
        self.assertIsNone(self.feed(9))  # the chime, heard by the microphone
        self.assertIsNone(self.feed(9))
        self.assertTrue(self.capture.awaiting_speech)
        self.assertEqual(self.capture.speech_blocks, 0)
        self.assertIsNone(self.feed(5))  # the first word
        self.assertFalse(self.capture.awaiting_speech)

    def test_a_second_call_before_any_request_is_a_new_wake(self):
        self.wake()
        self.assertIsNone(self.feed(quiet=True))
        self.assertEqual(self.feed(-1).kind, "wake")
        self.assertTrue(self.capture.awaiting_speech)

    def test_the_name_inside_a_command_does_not_restart_it(self):
        """"open Navi settings": the words before the name are not thrown away."""
        self.wake()
        self.assertIsNone(self.feed(8))
        self.assertIsNone(self.feed(-1))
        self.assertIsNotNone(self.capture.recording)
        self.assertIn(8, [block[0] for block in self.capture.recording])

    def test_finish_now_sends_what_was_said_including_the_last_word(self):
        self.wake()
        for _ in range(8):
            self.feed(7)
        self.feed(9, quiet=True)
        event = self.capture.finish_now()
        self.assertEqual(event.kind, "utterance")
        self.assertIn(9, [block[0] for block in event.blocks])
        self.assertIsNone(self.capture.recording)

    def test_finish_now_with_nothing_said_sends_nothing(self):
        self.wake()
        self.feed(quiet=True)
        self.assertEqual(self.capture.finish_now().kind, "no_command")
        self.assertIsNone(self.capture.finish_now())

    def test_a_silence_under_four_seconds_never_ends_a_sentence(self):
        self.wake()
        self.feed(7)
        for _ in range(39):
            self.assertIsNone(self.feed(quiet=True))
        self.assertEqual(self.feed(quiet=True).kind, "utterance")

    def test_the_name_in_long_speech_never_restarts_the_capture(self):
        """A keyword hit mid-sentence used to start over after 8 s, losing it all."""
        self.wake()
        for _ in range(90):
            self.feed(7)
        kept = len(self.capture.recording)
        event = self.feed(-1)
        self.assertTrue(event is None or event.kind == "segment")
        self.assertEqual(len(self.capture.recording), kept + 1)

    def test_talking_for_two_minutes_is_not_stopped(self):
        self.wake()
        for _ in range(1200):
            event = self.feed(7)
            self.assertTrue(event is None or event.kind == "segment")
        self.assertIsNotNone(self.capture.recording)

    def test_short_pause_does_not_commit_or_split_the_command(self):
        self.wake()
        for _ in range(8):
            self.assertIsNone(self.feed(quiet=True))
        self.assertIsNone(self.feed(50))
        for _ in range(self.fast - 1):
            self.assertIsNone(self.feed(quiet=True))
        self.assertEqual(self.feed(quiet=True).kind, "utterance")

    def test_long_speech_waits_for_the_full_quiet_run(self):
        """BUG-052: >2 s of speech keeps the slower endpoint."""
        self.wake()
        for _ in range(25):
            self.assertIsNone(self.feed(20))  # 2.5 s of speech
        for _ in range(self.slow - 1):
            self.assertIsNone(self.feed(quiet=True))
        self.assertEqual(self.feed(quiet=True).kind, "utterance")

    def test_pause_after_wake_still_needs_speech(self):
        self.wake()
        for _ in range(self.capture.wake_grace_blocks - 1):
            self.assertIsNone(self.feed(quiet=True))
        self.assertEqual(self.feed(quiet=True).kind, "no_command")
        self.assertIsNone(self.capture.recording)

    def test_segments_stream_long_dictation_and_cap_flags_truncation(self):
        """BUG-053: ~10 s segments plus a truncated marker at the cap."""
        self.capture.maximum_blocks = 40  # shrink the cap for the test
        self.capture.segment_blocks = 10
        self.wake()
        kinds = []
        total = 0
        for _ in range(60):
            event = self.feed(7)
            total += 1
            if event is not None:
                kinds.append(event.kind)
            if event is not None and event.kind == "utterance":
                self.assertTrue(event.truncated)
                break
        self.assertIn("segment", kinds)
        self.assertEqual(kinds[-1], "utterance")
        self.assertLessEqual(total, 40 - 15 + 1)  # capped ~600 ms after onset scale

    def test_gap_marks_overflow_without_erasing_the_command(self):
        """BUG-055: mid-command overflow keeps the prefix and continues."""
        self.wake()
        self.feed(101)
        self.assertEqual(self.feed(0, overflow=True).kind, "gap")
        self.assertIsNotNone(self.capture.recording)  # no reset
        self.assertIsNone(self.feed(102))
        for _ in range(self.fast - 1):
            self.assertIsNone(self.feed(quiet=True))
        event = self.feed(quiet=True)
        self.assertEqual(event.kind, "utterance")
        self.assertTrue(event.gap)
        values = [block[0] for block in event.blocks]
        self.assertIn(101, values)
        self.assertIn(102, values)
        self.assertNotIn(-1, values)

    def test_pre_wake_overflow_drops_only_the_pre_roll(self):
        for _ in range(10):
            self.feed(3)
        stream = self.capture.stream
        self.assertEqual(self.feed(0, overflow=True).kind, "overflow")
        self.assertIsNot(self.capture.stream, stream)
        self.assertEqual(len(self.capture.pre_roll), 0)
        self.assertIsNone(self.capture.recording)

    def test_reset_discards_partial_command(self):
        self.wake()
        self.feed(42)
        self.capture.reset()
        for _ in range(20):
            self.assertIsNone(self.feed(quiet=True))

    def test_owns_copies_and_rejects_wrong_block_size(self):
        samples = [5] * 1600
        self.capture.feed(samples, quiet=False)
        samples[0] = 10
        self.assertEqual(self.capture.pre_roll[0][0], 5)
        with self.assertRaisesRegex(ValueError, "100 ms"):
            self.capture.feed([0], quiet=True)

    def test_start_recording_arms_without_a_fresh_wake(self):
        self.capture.pre_roll.append([3] * 1600)
        self.capture.start_recording()
        self.assertTrue(self.capture.barge_in)
        for _ in range(14):
            self.assertIsNone(self.feed(7))
        for _ in range(self.slow - 1):
            self.assertIsNone(self.feed(quiet=True))
        event = self.feed(quiet=True)
        self.assertEqual(event.kind, "utterance")
        # Barge-in keeps the slow endpoint (BUG-052); the pre-roll lead-in is
        # kept (echo tail), speech and the quiet tail follow.
        values = [block[0] for block in event.blocks]
        self.assertEqual(values[:15], [3] + [7] * 14)
        self.assertEqual(len(event.blocks), 15 + self.capture.tail_blocks)
        self.assertIsNone(self.capture.recording)


class TrimTests(unittest.TestCase):
    def test_trim_span_clamps_to_the_wake_mark(self):
        blocks = tuple([i] for i in range(20))
        # speech from 16 to 18; wake mark 14; lead 2 -> clamped to 15.
        out = trim_span(blocks, 16, 18, wake_mark=14, lead_blocks=2, tail_blocks=1)
        self.assertEqual(out[0][0], 15)
        self.assertEqual(out[-1][0], 19)
        self.assertEqual(len(out), 5)

    def test_trim_span_without_speech_returns_everything(self):
        blocks = tuple([i] for i in range(10))
        self.assertEqual(trim_span(blocks, None, None), blocks)


class VadTests(unittest.TestCase):
    """BUG-051: the adaptive floor hears quiet mics and noisy rooms alike."""

    def feed_pattern(self, vad, noise_rms, speech_rms, speech_blocks=20):
        import math
        events = []
        quiet_blocks = 0
        for i in range(40):
            rms = noise_rms
            if 10 <= i < 10 + speech_blocks:
                rms = speech_rms
            samples = [rms] * 1600  # constant -> RMS == rms
            speech = vad.feed(samples)
            if i >= 10 + speech_blocks:
                quiet_blocks += 0 if speech else 1
        return quiet_blocks

    def test_three_noise_levels_and_three_gains(self):
        for noise in (0.001, 0.005, 0.02):
            for speech_rms in (0.01, 0.05, 0.2):
                if speech_rms <= noise * 2:
                    continue
                vad = AdaptiveVad()
                quiet = self.feed_pattern(vad, noise, speech_rms)
                self.assertGreater(quiet, 0, f"noise={noise} speech={speech_rms}")
                self.assertLess(quiet, 20, f"speech drowned: noise={noise} speech={speech_rms}")

    def test_speaking_cannot_close_the_gate_mid_sentence(self):
        """12 s of speech at any loudness stays speech. The old floor learned from
        speech, so the threshold passed the voice and the command ended at 1-2 s."""
        for level in (0.012, 0.04, 0.1):
            vad = AdaptiveVad()
            for _ in range(30):
                vad.feed([0.003] * 1600)
            heard = [vad.feed([level if block % 7 else 0.005] * 1600) for block in range(120)]
            voiced = [said for block, said in enumerate(heard) if block % 7]
            self.assertTrue(all(voiced), f"gate closed on speech at level {level}")

    def test_floor_follows_a_lasting_change_in_the_room(self):
        vad = AdaptiveVad()
        for _ in range(30):
            vad.feed([0.003] * 1600)
        for _ in range(vad.recent.maxlen):
            vad.feed([0.02] * 1600)  # a fan turned on
        self.assertFalse(vad.feed([0.02] * 1600))
        self.assertTrue(vad.feed([0.2] * 1600))

    def test_floor_surfaces_for_diagnostics(self):
        vad = AdaptiveVad()
        for _ in range(15):
            vad.feed([0.001] * 1600)
        state = vad.state()
        self.assertTrue(state["calibrated"])
        self.assertGreaterEqual(state["floor"], 0.001)

    def test_threshold_stays_three_times_a_very_quiet_room(self):
        """A laptop mic: room at 0.0008. A clamp held the threshold at 0.006, above a
        voice at 0.004, which ended commands after the first words."""
        vad = AdaptiveVad()
        for _ in range(30):
            vad.feed([0.0008] * 1600)
        self.assertTrue(vad.feed([0.004] * 1600))
        self.assertFalse(vad.feed([0.0008] * 1600))


class SpeechVadTests(unittest.TestCase):
    """The live gate on a real speech model, not on a loudness rule."""

    MODEL = Path.home() / ".local/share/navi/models/vad/silero_vad.onnx"

    def setUp(self):
        if not self.MODEL.exists():
            self.skipTest("Silero model not installed")

    def test_noise_is_not_speech(self):
        import numpy as np
        vad = SpeechVad(self.MODEL)
        rng = np.random.default_rng(1)
        heard = [vad.feed(rng.normal(0, .01, 1600).astype(np.float32)) for _ in range(50)]
        self.assertFalse(any(heard))

    def test_long_noise_does_not_grow_or_fail(self):
        import numpy as np
        vad = SpeechVad(self.MODEL)
        for _ in range(700):
            self.assertIs(vad.feed(np.zeros(1600, dtype=np.float32)), False)
        self.assertEqual(vad.state(), {"kind": "silero"})

    def test_agc_lifts_quiet_audio_and_caps_gain(self):
        quiet = [[0.001] * 100]
        lifted = agc(quiet)
        self.assertAlmostEqual(lifted[0][0], 0.001 * 12.0, places=4)  # max 12x
        loud = [[0.5] * 100]
        self.assertEqual(agc(loud), tuple(loud))


class MicrophoneHealthTests(unittest.TestCase):
    def test_a_read_that_never_returns_is_a_stall(self):
        watchdog = MicWatchdog()
        self.assertFalse(watchdog.stalled(now=1000))
        watchdog.begin(now=1000)
        self.assertFalse(watchdog.stalled(now=1000 + watchdog.limit))
        self.assertTrue(watchdog.stalled(now=1001 + watchdog.limit))
        watchdog.end()
        self.assertFalse(watchdog.stalled(now=5000))

    def test_only_a_long_run_of_exact_zeros_reopens_the_stream(self):
        watch = SilenceWatch()
        for _ in range(watch.limit_blocks - 1):
            self.assertFalse(watch.feed([0.0] * 1600))
        self.assertFalse(watch.feed([0.001] + [0.0] * 1599))  # quiet room, not dead
        for _ in range(watch.limit_blocks - 1):
            self.assertFalse(watch.feed([0.0] * 1600))
        self.assertTrue(watch.feed([0.0] * 1600))


class SttQueueTests(unittest.TestCase):
    """BUG-054: recognition runs off-thread, bounded, cancellable."""

    def test_blocking_stt_never_stalls_the_producer(self):
        gate = threading.Event()
        results = []

        def recognize(blocks, *a, **k):
            gate.wait(5)
            return ("done", "fake", {})

        queue = SttQueue(recognize, on_result=lambda r, e: results.append(r))
        try:
            event = type("E", (), {"kind": "utterance", "blocks": ([0] * 1600,)})()
            started = time.monotonic()
            queue.submit(event)
            elapsed = time.monotonic() - started
            self.assertLess(elapsed, 0.5)  # submit returns immediately
            # capture keeps draining the mic while STT is blocked:
            for i in range(50):
                queue.remember([i] * 1600)
            self.assertEqual(len(queue.ring), 30)
            gate.set()
            deadline = time.monotonic() + 3
            while not results and time.monotonic() < deadline:
                time.sleep(0.02)
            self.assertEqual(results, [("done", "fake", {})])
        finally:
            gate.set()
            queue.close()

    def test_new_wake_cancels_pending_work(self):
        gate = threading.Event()
        results = []
        queue = SttQueue(lambda b, *a, **k: (gate.wait(2), ("x", "s", {}))[1],
                         on_result=lambda r, e: results.append(r))
        try:
            event = type("E", (), {"kind": "utterance", "blocks": ([0] * 1600,)})()
            queue.submit(event)
            queue.submit(event)  # second job queues behind the first
            queue.cancel_pending()
            gate.set()
            time.sleep(0.3)
            self.assertEqual(results, [])
            self.assertGreater(queue.dropped, 0)
        finally:
            gate.set()
            queue.close()


class SnapshotTests(unittest.TestCase):
    """BUG-064: status polling happens on the snapshot thread only."""

    def test_stalled_daemon_never_blocks_readers(self):
        calls = []
        release = threading.Event()

        def rpc(op, params=None, timeout=90):
            calls.append(op)
            release.wait(5)
            return {"settings": {}}

        snapshot = StatusSnapshot(rpc, interval=0.05)
        try:
            time.sleep(0.15)  # first poll is inside the stalled rpc
            started = time.monotonic()
            self.assertIsNone(snapshot.value)  # read never blocks on the RPC
            self.assertLess(time.monotonic() - started, 0.2)
            release.set()
            deadline = time.monotonic() + 3
            while snapshot.value is None and time.monotonic() < deadline:
                time.sleep(0.02)
            self.assertEqual(snapshot.value, {"settings": {}})
            self.assertGreaterEqual(calls.count("status"), 1)
        finally:
            release.set()
            snapshot.close()


class DispatchTests(unittest.TestCase):
    def setUp(self):
        self.status = {"settings": {"microphone": True, "listening": "background", "speech": {"engine": "whistle"}}, "gui_open": False}
        self.speech = self.status["settings"]["speech"].copy()
        self.started = threading.Event()
        self.release = threading.Event()
        self.calls = []
        self.failure = None
        self.router = CommandRouter(self.rpc, enabled)

    def tearDown(self):
        self.release.set()
        if self.router.pending:
            try:
                self.router.pending.exception(timeout=5)
            except Exception:
                pass
        self.router.close()

    def rpc(self, op, params=None, timeout=90):
        if op == "status":
            return self.status
        self.calls.append((op, params))
        if op == "command":
            self.started.set()
            if not self.release.wait(5):
                raise RuntimeError("Test did not release chief")
            if self.failure:
                raise self.failure
        return {}

    def test_second_command_is_queued_and_runs_after_the_first(self):
        """BUG-062: speech during a turn queues (cap 2), then dispatches."""
        self.assertEqual(self.router.dispatch("Open terminal", self.speech), "deciding")
        self.assertTrue(self.started.wait(2))
        self.assertEqual(self.router.dispatch("Open another terminal", self.speech), "queued")
        self.assertEqual(list(self.router.queued), ["Open another terminal"])
        self.release.set()
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            self.router.poll()
            if len(self.calls) == 2:
                break
            time.sleep(0.02)
        self.assertEqual(self.calls, [("command", {"text": "Open terminal"}),
                                      ("command", {"text": "Open another terminal"})])

    def test_queue_overflow_keeps_the_newest_and_stop_bypasses(self):
        self.assertEqual(self.router.dispatch("one", self.speech), "deciding")
        self.assertTrue(self.started.wait(2))
        self.assertEqual(self.router.dispatch("two", self.speech), "queued")
        self.assertEqual(self.router.dispatch("three", self.speech), "queued")
        self.assertEqual(self.router.dispatch("four", self.speech), "queued")
        self.assertEqual(self.router.dropped, 1)
        self.assertNotIn("two", self.router.queued)
        self.assertEqual(self.router.dispatch("Stop", self.speech), "stopped")
        self.assertEqual(len(self.router.queued), 0)
        self.assertIn(("cancel", None), self.calls)

    def test_disabled_microphone_or_hidden_gui_discards_completed_transcript(self):
        self.status["settings"]["microphone"] = False
        self.assertEqual(self.router.dispatch("Open terminal", self.speech), "discarded")
        self.status["settings"].update(microphone=True, listening="gui")
        self.assertEqual(self.router.dispatch("Open terminal", self.speech), "discarded")
        self.assertEqual(self.calls, [])

    def test_speech_replacement_discards_transcript_from_old_engine(self):
        self.status["settings"]["speech"] = {"engine": "whisper"}
        self.assertEqual(self.router.dispatch("Open terminal", self.speech), "discarded")
        self.assertEqual(self.calls, [])

    def test_dispatch_reads_the_snapshot_not_the_wire(self):
        """BUG-064: dispatch takes a status dict; no status RPC is made."""
        self.assertEqual(self.router.dispatch("hi", self.speech, status=self.status), "deciding")
        self.assertEqual(self.calls, [("command", {"text": "hi"})])

    def test_failed_request_releases_slot_for_next_command(self):
        self.failure = RuntimeError("Disconnected")
        self.release.set()
        self.router.dispatch("Open terminal", self.speech)
        self.router.pending.exception(timeout=2)
        # A lost reply must not raise: that would stop the capture loop for seconds.
        self.router.poll()
        self.assertIsNone(self.router.pending)
        self.failure = None
        self.assertEqual(self.router.dispatch("Switch to Review", self.speech), "deciding")
        self.router.pending.result(timeout=2)
        self.router.poll()


if __name__ == "__main__":
    unittest.main()
