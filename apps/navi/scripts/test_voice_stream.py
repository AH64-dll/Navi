"""Streaming and dispatch regressions; no speech models or audio devices."""
from pathlib import Path
import sys
import threading
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "workers"))
from voice import enabled
from voice_stream import CaptureBuffer, CommandRouter


class WakeStream:
    def __init__(self):
        self.ready = False
        self.wake = False

    def accept_waveform(self, sample_rate, samples):
        assert sample_rate == 16000
        self.ready = True
        self.wake = samples[0] == -1


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

    def test_preserves_wake_and_immediate_command_until_nine_quiet_blocks(self):
        self.wake()
        self.assertIsNone(self.feed(100))
        for _ in range(8):
            self.assertIsNone(self.feed(quiet=True))
        event = self.feed(quiet=True)
        self.assertEqual(event.kind, "utterance")
        self.assertEqual([block[0] for block in event.blocks[:16]], list(range(14)) + [-1, 100])
        self.assertEqual(len(event.blocks), 25)
        self.assertIsNone(self.capture.recording)
        self.assertEqual(len(self.capture.pre_roll), 0)
        self.assertIsNone(self.feed(200))  # a new command needs a new wake

    def test_short_pause_does_not_commit_or_split_the_command(self):
        self.wake()
        for _ in range(8):
            self.assertIsNone(self.feed(quiet=True))
        self.assertIsNone(self.feed(50))
        for _ in range(8):
            self.assertIsNone(self.feed(quiet=True))
        self.assertEqual(self.feed(quiet=True).kind, "utterance")

    def test_continuous_speech_commits_at_twenty_seconds(self):
        self.wake()
        for _ in range(184):
            self.assertIsNone(self.feed(20))
        event = self.feed(20)
        self.assertEqual(event.kind, "utterance")
        self.assertEqual(len(event.blocks), 200)

    def test_early_wake_keeps_minimum_buffer(self):
        self.assertEqual(self.feed(-1).kind, "wake")
        for _ in range(13):
            self.assertIsNone(self.feed(quiet=True))
        self.assertEqual(len(self.feed(quiet=True).blocks), 15)

    def test_overflow_discards_recording_pre_roll_and_faulty_block(self):
        self.wake()
        self.feed(101)
        stream = self.capture.stream
        self.assertEqual(self.feed(-1, overflow=True).kind, "overflow")
        self.assertIsNot(self.capture.stream, stream)
        self.assertIsNone(self.capture.recording)
        self.assertEqual(len(self.capture.pre_roll), 0)
        for _ in range(20):
            self.assertIsNone(self.feed(quiet=True))
        self.assertEqual(self.feed(-1).kind, "wake")
        self.assertNotIn(101, [block[0] for block in self.capture.recording])

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
        self.assertEqual([block[0] for block in self.capture.recording], [3])
        for _ in range(14):
            self.assertIsNone(self.feed(7))
        for _ in range(8):
            self.assertIsNone(self.feed(quiet=True))
        event = self.feed(quiet=True)
        self.assertEqual(event.kind, "utterance")
        self.assertEqual(len(event.blocks), 24)
        self.assertIsNone(self.capture.recording)


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
            self.router.pending.exception(timeout=5)
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

    def test_busy_commands_are_not_queued_and_stop_bypasses_the_chief(self):
        self.assertEqual(self.router.dispatch("Open terminal", self.speech), "deciding")
        self.assertTrue(self.started.wait(2))
        self.assertEqual(self.router.dispatch("Open another terminal", self.speech), "busy")
        self.assertEqual(self.router.dispatch("Stop", self.speech), "stopped")
        self.assertEqual(self.calls, [("command", {"text": "Open terminal"}), ("cancel", None)])
        self.assertFalse(self.router.pending.done())
        self.release.set()
        self.router.pending.result(timeout=2)
        self.router.poll()
        self.assertIsNone(self.router.pending)

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

    def test_failed_request_releases_slot_for_next_command(self):
        self.failure = RuntimeError("Disconnected")
        self.release.set()
        self.router.dispatch("Open terminal", self.speech)
        self.router.pending.exception(timeout=2)
        with self.assertRaisesRegex(RuntimeError, "Disconnected"):
            self.router.poll()
        self.assertIsNone(self.router.pending)
        self.failure = None
        self.assertEqual(self.router.dispatch("Switch to Review", self.speech), "deciding")
        self.router.pending.result(timeout=2)
        self.router.poll()


if __name__ == "__main__":
    unittest.main()
