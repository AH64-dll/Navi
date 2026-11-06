"""Voice policy checks without importing speech engines or opening audio devices."""
import os
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "workers"))
os.environ["NAVI_HEX_DISABLE"] = "1"  # this suite asserts the cloud/local order
from voice import (ReplySpeaker, event_speech, watch_replies, command_for, command_text, enabled, ignored_wake,
                   input_device, speech_keywords)


class _NullRecognizer:
    def create_stream(self):
        return self

    def accept_waveform(self, rate, samples):
        pass

    def decode_stream(self, stream):
        pass

    class result:
        text = ""


class VoicePolicyTests(unittest.TestCase):
    def test_microphone_and_window_policy(self):
        status = {"settings": {"microphone": False, "listening": "background"}, "gui_open": True}
        self.assertFalse(enabled(status))
        status["settings"]["microphone"] = True
        self.assertTrue(enabled(status))
        status["settings"]["listening"] = "gui"
        status["gui_open"] = False
        self.assertFalse(enabled(status))
        status["gui_open"] = True
        self.assertTrue(enabled(status))

    def test_wake_prefix_and_stop_punctuation(self):
        self.assertEqual(command_text("Hey Navi, Stop!"), "Stop")
        self.assertEqual(command_text("Hey, Navi, stop"), "stop")
        self.assertEqual(command_text("Hay Navi, Open terminal."), "Open terminal")
        self.assertEqual(command_text("Hey Navi. Open Codex."), "Open Codex")
        self.assertEqual(command_text("Navi, Open terminal."), "Open terminal")
        self.assertEqual(command_text("Navi open Codex"), "open Codex")
        self.assertEqual(command_text("Navy, what is the capital of France"), "what is the capital of France")
        self.assertEqual(command_text("Hey Nadi, open Codex"), "open Codex")
        self.assertEqual(command_text("Open Claude Code."), "Open Claude Code")
        self.assertEqual(command_text("Hey Navigator, open Codex"), "Hey Navigator, open Codex")
        self.assertEqual(command_text("navigation tips"), "navigation tips")

    def test_navigation_words_are_not_treated_as_commands(self):
        self.assertTrue(ignored_wake("navigation tips"))
        self.assertTrue(ignored_wake("Navigate to the settings page"))
        self.assertTrue(ignored_wake("Navigator window"))
        self.assertFalse(ignored_wake("Navi, open terminal"))
        self.assertFalse(ignored_wake("open terminal"))

    def test_ignored_wake_evaluates_the_raw_transcript(self):
        """BUG-067: "Navi navigate to X" is a command; bare "navigation" is
        a false wake only when the KWS actually fired."""
        self.assertEqual(command_for("navigation is hard", wake_triggered=True), "")
        self.assertEqual(command_for("Navi navigate to the files page", wake_triggered=True),
                         "navigate to the files page")
        self.assertEqual(command_for("Hey Navi, navigate home", wake_triggered=True),
                         "navigate home")
        # Without a wake trigger the words are kept verbatim.
        self.assertEqual(command_for("navigation tips", wake_triggered=False),
                         "navigation tips")
        self.assertEqual(command_for("Navy deployment docs", wake_triggered=False),
                         "Navy deployment docs")

    def test_mishearing_table_from_evaluation_fixtures(self):
        """BUG-050: the strip is a wake-only safety net over KWS variants."""
        for raw, expected in (
            ("Hey, Navi.", ""),
            ("Hey Navi navigate to X", "navigate to X"),
            ("Navy what time is it", "what time is it"),
            ("Nadi open Codex", "open Codex"),
            ("Navi, run the tests!", "run the tests"),
        ):
            self.assertEqual(command_for(raw, wake_triggered=True), expected, raw)
        # A non-wake transcript is never stripped, so real words survive.
        self.assertEqual(command_for("Navy deployment docs", wake_triggered=False),
                         "Navy deployment docs")

    def test_hex_runs_before_local_and_when_cloud_unavailable(self):
        import numpy as np
        import cloud_stt
        import voice

        old = voice.hex_asr.transcribe
        old_available = cloud_stt.available
        voice.hex_asr.transcribe = lambda blocks, *args, **kwargs: "from hex"
        cloud_stt.available = lambda order=None: False
        try:
            text, source, meta = voice.recognize(
                [np.zeros(1600, dtype="float32")], None, None, {}, cloud=True
            )
        finally:
            voice.hex_asr.transcribe = old
            cloud_stt.available = old_available
        self.assertEqual((text, source), ("from hex", "hex"))
        self.assertEqual(meta["raw"], "from hex")

    def test_empty_hex_result_falls_through_to_local(self):
        from unittest.mock import MagicMock

        import numpy as np
        import cloud_stt
        import voice

        old_hex = voice.hex_asr.transcribe
        old_cloud = cloud_stt.available
        voice.hex_asr.transcribe = lambda blocks, *args, **kwargs: None
        cloud_stt.available = lambda: False
        recognizer = MagicMock()
        recognizer.create_stream.return_value.result.text = ""
        try:
            text, source, _meta = voice.recognize(
                [np.zeros(1600, dtype="float32")], "moonshine", recognizer, {}, cloud=True
            )
        finally:
            voice.hex_asr.transcribe = old_hex
            cloud_stt.available = old_cloud
        self.assertEqual((text, source), ("", "local"))

    def test_hex_is_skipped_for_non_english_languages(self):
        """BUG-057: an English-only model is bypassed when speech is fr."""
        import numpy as np
        import voice

        calls = []
        old = voice.hex_asr.transcribe
        try:
            voice.hex_asr.transcribe = lambda *a, **k: calls.append(1) or "hex"
            text, source, meta = voice.recognize(
                [np.zeros(1600, dtype="float32")], "moonshine",
                _NullRecognizer(), {}, cloud=False, language="fr")
        finally:
            voice.hex_asr.transcribe = old
        self.assertEqual(calls, [])   # Hex never runs for a French request
        self.assertEqual(meta["language"], "fr")

    def test_registered_speech_names_are_bounded_and_active_session_is_prioritized(self):
        status = {"settings": {"active_session": "active", "profiles": [{"name": "CLAUDE"}], "projects": [{"name": "Navi"}]}, "sessions": [
            {"id": "ended", "name": "Ended", "running": False},
            *[{"id": str(i), "name": f"Review {i}", "running": True} for i in range(40)],
            {"id": "active", "name": "Sandbox", "running": True},
        ]}
        names = speech_keywords(status)
        self.assertEqual(names[:5], ["Navi", "Codex", "Claude", "Pi", "Sandbox"])
        self.assertEqual(len(names), 24)
        self.assertNotIn("Ended", names)
        status["sessions"] = []
        status["settings"]["projects"] = [{"name": "Navi"}, {"name": "Line\n Break"}, {"name": "x" * 121}, {"name": None}, {}]
        self.assertEqual(speech_keywords(status), ["Navi", "Codex", "Claude", "Pi", "Line Break"])

    def test_input_device_prefers_pipewire_and_falls_back_to_default(self):
        class Devices:
            def __init__(self, devices):
                self.devices = devices
            def query_devices(self):
                return self.devices

        self.assertEqual(input_device(Devices([
            {"name": "HDA Intel", "max_input_channels": 2},
            {"name": "PipeWire Sound Server", "max_input_channels": 2},
        ])), 1)
        self.assertIsNone(input_device(Devices([
            {"name": "HDA Intel", "max_input_channels": 2},
        ])))
        self.assertIsNone(input_device(Devices([])))
        self.assertIsNone(input_device(Devices([{"name": "Built-in Mic", "max_input_channels": 1}])))

    def test_cloud_stt_runs_before_local_and_falls_back(self):
        import numpy as np
        import cloud_stt
        import speech_config
        import voice

        class Local:
            calls = 0

            def transcribe(self, samples, language=None, keywords=None):
                Local.calls += 1
                return {"text": "hey navi local answer"}

        status = {"settings": {"profiles": [], "projects": [], "favorites": []}, "sessions": []}
        audio = [np.zeros(1600, dtype=np.float32)]
        old_available, old_result = cloud_stt.available, cloud_stt.transcribe_result
        old_cache = speech_config._speech_cache
        try:
            speech_config._speech_cache = dict(speech_config.DEFAULTS)
            cloud_stt.available = lambda order=None: True
            cloud_stt.transcribe_result = lambda *a, **k: ("hey navi cloud answer", {"confidence": 0.9})
            self.assertEqual(voice.recognize(audio, "whistle", Local(), status),
                             ("cloud answer", "cloud", {"confidence": 0.9, "provider": None,
                                                        "raw": "hey navi cloud answer",
                                                        "language": speech_config.language()}))
            self.assertEqual(Local.calls, 0)
            cloud_stt.transcribe_result = lambda *a, **k: (None, None)
            self.assertEqual(voice.recognize(audio, "whistle", Local(), status)[0:2],
                             ("local answer", "local"))
            self.assertEqual(Local.calls, 1)
            speech_config._speech_cache = dict(speech_config.DEFAULTS, speech_mode="local-only")
            cloud_stt.transcribe_result = lambda *a, **k: ("should not be used", {})
            self.assertEqual(voice.recognize(audio, "whistle", Local(), status)[0:2],
                             ("local answer", "local"))
            self.assertEqual(Local.calls, 2)
        finally:
            cloud_stt.available, cloud_stt.transcribe_result = old_available, old_result
            speech_config._speech_cache = old_cache


class ReplySpeechTests(unittest.TestCase):
    def test_a_reply_says_its_spoken_form(self):
        self.assertEqual(event_speech({"kind": "reply", "details": {"text": "long list", "speech": "short"}}), "short")
        self.assertEqual(event_speech({"kind": "reply", "details": {"text": "Opened."}}), "Opened.")

    def test_stops_and_other_events_say_nothing(self):
        self.assertIsNone(event_speech({"kind": "reply", "details": {"text": "Stopped.", "stopped": True}}))
        self.assertIsNone(event_speech({"kind": "command", "details": {"text": "x"}}))
        self.assertIsNone(event_speech({"kind": "error", "message": "boom", "details": {"op": "status"}}))

    def test_a_failed_command_says_why(self):
        event = {"kind": "error", "message": "Connect your email first.", "details": {"op": "command"}}
        self.assertEqual(event_speech(event), "Connect your email first.")

    def test_a_slow_turn_says_one_moment_once(self):
        speaker = ReplySpeaker()
        speaker.feed([{"kind": "working"}], 10.0)
        self.assertIsNone(speaker.next_line(11.0))
        self.assertEqual(speaker.next_line(13.1), "One moment.")
        self.assertIsNone(speaker.next_line(20.0))

    def test_a_quick_reply_needs_no_one_moment(self):
        speaker = ReplySpeaker()
        speaker.feed([{"kind": "working"}, {"kind": "reply", "details": {"text": "Opened."}}], 10.0)
        self.assertEqual(speaker.next_line(10.0), "Opened.")
        self.assertIsNone(speaker.next_line(30.0))

    def test_only_the_newest_waiting_reply_is_kept(self):
        speaker = ReplySpeaker()
        speaker.feed([{"kind": "reply", "details": {"text": "First."}}], 1.0)
        speaker.feed([{"kind": "reply", "details": {"text": "Second."}}], 2.0)
        self.assertEqual(speaker.next_line(2.0), "Second.")

    def test_watcher_starts_at_now_and_speaks_only_new_replies(self):
        said = []

        class FakePlayback:
            def speaking(self):
                return False

            def cancel(self):
                said.append("cancelled")

            def start(self, text, persona_name=None, language=None):
                said.append(text)

        class StopAfter:
            def __init__(self, ticks):
                self.ticks = ticks

            def wait(self, _interval):
                self.ticks -= 1
                return self.ticks < 0

        pages = [
            {"boot_id": "a", "events_latest": 7, "generation": 0, "settings": {"speak": True},
             "events": [{"seq": 7, "kind": "reply", "details": {"text": "Old."}}]},
            {"boot_id": "a", "generation": 0, "settings": {"speak": True},
             "events": [{"seq": 8, "kind": "reply", "details": {"text": "New."}}]},
            {"boot_id": "a", "generation": 0, "settings": {"speak": False}, "events": []},
        ]
        asked = []

        def rpc_call(op, params, timeout):
            asked.append(params["since"])
            return pages.pop(0)

        watch_replies(rpc_call, FakePlayback(), StopAfter(3))
        self.assertEqual(said, ["New.", "cancelled"])
        self.assertEqual(asked, [0, 7, 8])


if __name__ == "__main__":
    unittest.main()
