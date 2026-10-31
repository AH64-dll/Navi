"""Voice policy checks without importing speech engines or opening audio devices."""
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "workers"))
from voice import command_text, enabled, input_device, speech_keywords


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
        self.assertEqual(command_text("Open Claude Code."), "Open Claude Code")
        self.assertEqual(command_text("Hey Navigator, open Codex"), "Hey Navigator, open Codex")

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
        old_available, old_transcribe = cloud_stt.available, cloud_stt.transcribe
        old_cache = speech_config._speech_cache
        try:
            speech_config._speech_cache = dict(speech_config.DEFAULTS)
            cloud_stt.available = lambda order=None: True
            cloud_stt.transcribe = lambda *a, **k: "hey navi cloud answer"
            self.assertEqual(voice.recognize(audio, "whistle", Local(), status), ("cloud answer", "cloud"))
            self.assertEqual(Local.calls, 0)
            cloud_stt.transcribe = lambda *a, **k: None
            self.assertEqual(voice.recognize(audio, "whistle", Local(), status), ("local answer", "local"))
            self.assertEqual(Local.calls, 1)
            speech_config._speech_cache = dict(speech_config.DEFAULTS, speech_mode="local-only")
            cloud_stt.transcribe = lambda *a, **k: "should not be used"
            self.assertEqual(voice.recognize(audio, "whistle", Local(), status), ("local answer", "local"))
            self.assertEqual(Local.calls, 2)
        finally:
            cloud_stt.available, cloud_stt.transcribe = old_available, old_transcribe
            speech_config._speech_cache = old_cache


if __name__ == "__main__":
    unittest.main()
