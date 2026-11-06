"""Background wake-word policy and window pop-up; no audio devices."""
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "workers"))
import voice


def status(**settings):
    base = {"microphone": True, "listening": "background", "speech": {}}
    base.update(settings)
    return {"settings": base, "gui_open": False}


class BackgroundPolicyTests(unittest.TestCase):
    def test_background_listening_needs_no_window(self):
        self.assertTrue(voice.enabled(status()))

    def test_microphone_toggle_is_always_required(self):
        self.assertFalse(voice.enabled(status(microphone=False)))

    def test_gui_mode_requires_an_open_window(self):
        self.assertFalse(voice.enabled(status(listening="gui")))
        value = status(listening="gui")
        value["gui_open"] = True
        self.assertTrue(voice.enabled(value))

    def test_background_mode_works_with_or_without_a_window(self):
        value = status()
        self.assertTrue(voice.enabled(value))
        value["gui_open"] = True
        self.assertTrue(voice.enabled(value))


class PopWindowTests(unittest.TestCase):
    def test_pops_only_for_background_listening_with_no_window(self):
        self.assertTrue(voice.pops_window(status()))
        self.assertFalse(voice.pops_window(status(microphone=False)))
        self.assertFalse(voice.pops_window(status(listening="gui")))
        value = status()
        value["gui_open"] = True
        self.assertFalse(voice.pops_window(value))


class SpawnTests(unittest.TestCase):
    def setUp(self):
        self.old_popen = subprocess.Popen
        self.old_spawned = voice.window_spawned
        self.launched = []
        voice.window_spawned = 0.0

        class Spawned:
            def __init__(self, args, **kwargs):
                self.args, self.kwargs = args, kwargs

        subprocess.Popen = lambda args, **kwargs: self.launched.append(Spawned(args, **kwargs))

    def tearDown(self):
        subprocess.Popen = self.old_popen
        voice.window_spawned = self.old_spawned

    def test_pop_window_launches_the_station_launcher_detached(self):
        voice.pop_window()
        self.assertEqual(self.launched[0].args, [str(voice.LAUNCHER)])
        self.assertTrue(self.launched[0].kwargs["start_new_session"])

    def test_pop_window_does_not_spawn_twice_within_the_cooldown(self):
        voice.pop_window()
        voice.pop_window()
        self.assertEqual(len(self.launched), 1)

    def test_pop_window_survives_a_missing_launcher(self):
        def denied(*args, **kwargs):
            raise OSError("no display")
        subprocess.Popen = denied
        voice.pop_window()  # must not raise


class ListenWakeTests(unittest.TestCase):
    """Drive the real listen() loop with a fake microphone and spotter."""

    def setUp(self):
        import types
        self.types = types
        self.saved = {name: getattr(voice, name) for name in
                      ("rpc", "load_spotter", "load_recognizer", "CaptureBuffer",
                       "pops_window", "pop_window", "state")}
        self.saved_sd = sys.modules.get("sounddevice")
        self.pops = []
        self.popped = []
        self.status = {"settings": {"microphone": True, "listening": "background",
                                    "speech": {}}, "gui_open": False}

        class Wake:
            kind = "wake"

        class Done(Exception):
            pass

        self.Done = Done
        outer = self

        class FakeCapture:
            def __init__(self, spotter):
                self.woken = False

            def feed(self, samples, quiet=False, overflow=False):
                if self.woken:
                    raise outer.Done
                self.woken = True
                return Wake()

            def reset(self):
                pass

            def start_recording(self):
                pass

        class FakeStream:
            def __enter__(self):
                return self

            def __exit__(self, *args):
                return False

            def read(self, frames):
                import numpy as np
                return np.zeros((frames, 1), dtype="float32"), False

        voice.rpc = lambda op, params=None, timeout=90: outer.status
        voice.load_spotter = lambda speech: object()
        voice.load_recognizer = lambda speech: ("whistle", object())
        voice.CaptureBuffer = FakeCapture
        voice.pops_window = lambda status: outer.pops.append(status) or True
        voice.pop_window = lambda: outer.popped.append(1)
        voice.state = lambda *args, **kwargs: None
        sys.modules["sounddevice"] = self.types.SimpleNamespace(InputStream=lambda **kwargs: FakeStream())

    def tearDown(self):
        for name, value in self.saved.items():
            setattr(voice, name, value)
        if self.saved_sd is None:
            sys.modules.pop("sounddevice", None)
        else:
            sys.modules["sounddevice"] = self.saved_sd

    def test_wake_with_no_window_pops_the_station(self):
        class Playback:
            def speaking(self):
                return False

        class Commands:
            def poll(self):
                pass

        with self.assertRaises(self.Done):
            voice.listen(Commands(), Playback())
        self.assertEqual(len(self.pops), 1)
        self.assertEqual(self.popped, [1])

    def test_wake_with_an_open_window_does_not_pop(self):
        self.status["gui_open"] = True
        voice.pops_window = lambda status: False

        class Playback:
            def speaking(self):
                return False

        class Commands:
            def poll(self):
                pass

        with self.assertRaises(self.Done):
            voice.listen(Commands(), Playback())
        self.assertEqual(self.popped, [])


class OfflineTrimTests(unittest.TestCase):
    """BUG-049/053: the PCM that would upload starts at the command."""

    def test_offline_trims_wake_and_pause_from_cloud_audio(self):
        import numpy as np
        from voice_stream import CaptureBuffer

        class Spotter:
            def create_stream(self):
                return self

            def is_ready(self, stream):
                return False

            def decode_stream(self, stream):
                pass

            def get_result(self, stream):
                return ""

        status = {"settings": {"speech": {}, "profiles": [], "projects": [], "favorites": []}, "sessions": []}
        captured = {}

        def fake_recognize(blocks, engine, recognizer, st, **kw):
            captured["blocks"] = len(blocks)
            return ("answer", "fake", {})

        # 3 s silence + 0.5 s of "command" energy, assume the wake already fired.
        samples = np.concatenate([
            np.zeros(3 * 1600, dtype=np.float32),
            np.full(5 * 1600, 0.3, dtype=np.float32),
        ])
        result = voice.offline(samples, Spotter(), None, None, status,
                               assume_wake=True, recognize_fn=fake_recognize,
                               quiet=0.05)
        self.assertEqual(result["transcript"], "answer")
        # ~0.5 s of speech + ~0.5 s of lead/tail -> far below the old 3.5 s buffer.
        self.assertLess(captured["blocks"], 20)
        # The emitted PCM starts <=300 ms before the first speech-energy block.
        first = [block[0] for block in result["blocks_sent"]].index(0.3)
        self.assertLessEqual(first, 3)

    def test_segments_concatenate_for_long_dictation(self):
        """BUG-053: ~10 s segments join into one transcript, unflagged."""
        import numpy as np
        status = {"settings": {"speech": {}, "profiles": [], "projects": [], "favorites": []}, "sessions": []}
        parts = []

        def fake_recognize(blocks, engine, recognizer, st, **kw):
            text = f"part{len(parts)}"
            parts.append(text)
            return (text, "fake", {})

        # 45 s of continuous speech: two hard-cut segments plus the tail.
        samples = np.full(450 * 1600, 0.3, dtype=np.float32)
        result = voice.offline(
            samples, _NullSpotter(), None, None, status, assume_wake=True,
            recognize_fn=fake_recognize, quiet=0.05)
        self.assertGreaterEqual(result["segments"], 2)
        self.assertFalse(result["truncated"])
        self.assertEqual(result["transcript"], " ".join(parts))

    def test_offline_flags_truncated_dictation(self):
        import numpy as np
        from voice_stream import CaptureBuffer
        status = {"settings": {"speech": {}, "profiles": [], "projects": [], "favorites": []}, "sessions": []}
        old_max = CaptureBuffer.maximum_blocks
        try:
            CaptureBuffer.maximum_blocks = 25  # shrink the cap
            samples = np.full(40 * 1600, 0.3, dtype=np.float32)
            result = voice.offline(
                samples, _NullSpotter(), None, None, status, assume_wake=True,
                recognize_fn=lambda *a, **k: ("x", "fake", {}), quiet=0.05)
            self.assertTrue(result["truncated"])
        finally:
            CaptureBuffer.maximum_blocks = old_max


class _NullSpotter:
    def create_stream(self):
        return self

    def is_ready(self, stream):
        return False

    def decode_stream(self, stream):
        pass

    def get_result(self, stream):
        return ""


class SelfWakeTests(unittest.TestCase):
    """BUG-060: Navi must not wake on its own voice."""

    def test_self_wake_likely_during_own_playback(self):
        playback = voice.Playback()
        playback.last_spoken = "Navi here, all systems nominal."
        playback.ended_at = time.monotonic()
        self.assertTrue(playback.self_wake_likely())
        playback.ended_at = 0.0
        self.assertFalse(playback.self_wake_likely())
        playback.last_spoken = "Everything is fine."
        playback.ended_at = time.monotonic()
        self.assertFalse(playback.self_wake_likely())

    def test_stop_words_detected_during_playback(self):
        self.assertTrue(voice.is_stop("stop"))
        self.assertTrue(voice.is_stop("Stop."))
        self.assertTrue(voice.is_stop("Navi stop"))
        self.assertFalse(voice.is_stop("stopwatch review"))
        self.assertFalse(voice.is_stop("please stop the build tomorrow"))

    def test_playback_cancel_event_kills_pending_audio(self):
        playback = voice.Playback()
        seen = []

        def fake_speak(text, persona_name=None, cancel_event=None, language=None):
            for _ in range(200):
                if cancel_event is not None and cancel_event.is_set():
                    return
                time.sleep(0.01)
            seen.append("finished")

        old = voice.speech_out.speak
        try:
            voice.speech_out.speak = fake_speak
            self.assertTrue(playback.start("a long reply"))
            self.assertTrue(playback.speaking())
            playback.cancel()
            started = time.monotonic()
            playback.thread.join(timeout=3)
            self.assertLess(time.monotonic() - started, 3)
            self.assertFalse(playback.speaking())
            self.assertEqual(seen, [])
        finally:
            voice.speech_out.speak = old


class PlaybackWakeGuardTests(unittest.TestCase):
    """BUG-060/061: during playback, a self-voice wake needs two hits."""

    def setUp(self):
        import types
        self.saved = {name: getattr(voice, name) for name in
                      ("rpc", "load_spotter", "load_recognizer", "CaptureBuffer",
                       "pops_window", "pop_window", "state")}
        self.saved_sd = sys.modules.get("sounddevice")
        self.status = {"settings": {"microphone": True, "listening": "background",
                                    "speech": {}}, "gui_open": False}

        class Done(Exception):
            pass

        self.Done = Done
        outer = self

        class FakeCapture:
            def __init__(self, spotter):
                self.events = list(outer.script)

            def feed(self, samples, quiet=False, overflow=False):
                kind = self.events.pop(0) if self.events else None
                if kind == "done":
                    raise Done()
                if kind is None:
                    return None
                return type("E", (), {"kind": kind, "blocks": (),
                                      "gap": False, "truncated": False})()

            def reset(self):
                pass

            def start_recording(self):
                pass

        class FakeStream:
            def __enter__(self):
                return self

            def __exit__(self, *args):
                return False

            def read(self, frames):
                import numpy as np
                return np.zeros((frames, 1), dtype="float32"), False

        class FakePlayback:
            def __init__(self):
                self.cancelled = 0

            def speaking(self):
                return True

            def self_wake_likely(self):
                return True

            def recently_spoke(self):
                return True

            def cancel(self):
                self.cancelled += 1

            @property
            def confirm_interrupted(self):
                return False

            @confirm_interrupted.setter
            def confirm_interrupted(self, value):
                pass

        class Commands:
            def poll(self):
                pass

        self.FakePlayback = FakePlayback
        self.Commands = Commands
        voice.rpc = lambda op, params=None, timeout=90: outer.status
        voice.load_spotter = lambda speech: object()
        voice.load_recognizer = lambda speech: ("whistle", object())
        voice.CaptureBuffer = FakeCapture
        voice.pops_window = lambda status: False
        voice.pop_window = lambda: None
        voice.state = lambda *args, **kwargs: None
        sys.modules["sounddevice"] = types.SimpleNamespace(InputStream=lambda **kwargs: FakeStream())

    def tearDown(self):
        for name, value in self.saved.items():
            setattr(voice, name, value)
        if self.saved_sd is None:
            sys.modules.pop("sounddevice", None)
        else:
            sys.modules["sounddevice"] = self.saved_sd

    def test_single_wake_hit_does_not_cancel_playback(self):
        self.script = ["wake", None, None, "done"]
        playback = self.FakePlayback()
        with self.assertRaises(self.Done):
            voice.listen(self.Commands(), playback)
        self.assertEqual(playback.cancelled, 0)

    def test_two_wake_hits_cancel_playback(self):
        self.script = ["wake", "wake", "done"]
        playback = self.FakePlayback()
        with self.assertRaises(self.Done):
            voice.listen(self.Commands(), playback)
        self.assertEqual(playback.cancelled, 1)


class HarnessRouteTests(unittest.TestCase):
    """BUG-065: 'tell <session> to <text>' resolves a running session."""

    def status(self):
        return {"settings": {"active_session": "s2"},
                "sessions": [{"id": "s1", "name": "Claude", "running": True},
                             {"id": "s2", "name": "terminal", "running": True},
                             {"id": "s3", "name": "ended", "running": False}]}

    def test_named_session_resolves(self):
        route = voice.harness_target("tell Claude to run the tests", self.status())
        self.assertEqual(route["id"], "s1")
        self.assertEqual(route["text"], "run the tests")

    def test_active_alias_uses_the_active_session(self):
        route = voice.harness_target("ask the active session to continue", self.status())
        self.assertEqual(route["id"], "s2")

    def test_busy_session_is_left_to_the_daemon_that_holds_the_prompt(self):
        status = self.status()
        status["sessions"][0]["state"] = "busy"
        self.assertIsNone(voice.harness_target("tell Claude to run the tests", status))

    def test_non_harness_phrases_pass_through(self):
        self.assertIsNone(voice.harness_target("what time is it", self.status()))
        self.assertIsNone(voice.harness_target("tell nobody to stop", self.status()))

    def test_route_sends_the_prompt_op_to_the_session(self):
        """BUG-065: the resolved route reaches the daemon's prompt op."""
        import speech_config
        calls = []

        class Snap:
            @property
            def value(self):
                return {"settings": {"microphone": True, "listening": "background",
                                     "speech": {}, "active_session": "s1"},
                        "sessions": [{"id": "s1", "name": "Claude", "running": True}]}

        class Commands:
            def dispatch(self, text, speech, status=None):
                return "deciding"

        old_rpc = voice.rpc
        old_seconds = speech_config._speech_cache
        try:
            voice.rpc = lambda op, params=None, timeout=90: calls.append((op, params)) or {}
            speech_config._speech_cache = dict(speech_config.DEFAULTS,
                                               confirm_seconds=0.0)
            outcome = voice._handle_transcript(
                "tell Claude to run the tests", "cloud", {}, Commands(), {},
                Snap(), voice.Playback())
            self.assertEqual(outcome, "sent to Claude")
            self.assertEqual(calls, [("prompt", {"id": "s1", "text": "run the tests"})])
        finally:
            voice.rpc = old_rpc
            speech_config._speech_cache = old_seconds


class ConfirmWindowTests(unittest.TestCase):
    """BUG-056: the transcript holds in a confirm window before dispatch."""

    def test_confirming_state_then_dispatch(self):
        import speech_config
        states = []
        outcomes = []

        class Snap:
            @property
            def value(self):
                return {"settings": {"microphone": True, "listening": "background",
                                     "speech": {"engine": "whistle"}},
                        "gui_open": False}

        class Commands:
            def dispatch(self, text, speech, status=None):
                outcomes.append(text)
                return "deciding"

        playback = voice.Playback()
        old_state = voice.state
        old_seconds = speech_config._speech_cache
        try:
            voice.state = lambda status, **kw: states.append(status)
            speech_config._speech_cache = dict(speech_config.DEFAULTS,
                                               confirm_seconds=0.05)
            outcome = voice._handle_transcript(
                "open terminal", "cloud", {"confidence": 0.9},
                Commands(), {"engine": "whistle"}, Snap(), playback)
            self.assertEqual(outcome, "deciding")
            self.assertIn("confirming", states)
            self.assertEqual(outcomes, ["open terminal"])
        finally:
            voice.state = old_state
            speech_config._speech_cache = old_seconds

    def test_wake_interrupt_abandons_the_dispatch(self):
        import speech_config

        class Snap:
            @property
            def value(self):
                return {"settings": {"microphone": True, "listening": "background",
                                     "speech": {}}, "gui_open": False}

        outcomes = []

        class Commands:
            def dispatch(self, text, speech, status=None):
                outcomes.append(text)
                return "deciding"

        playback = voice.Playback()
        playback.confirm_interrupted = True
        old_seconds = speech_config._speech_cache
        try:
            speech_config._speech_cache = dict(speech_config.DEFAULTS,
                                               confirm_seconds=0.05)
            outcome = voice._handle_transcript(
                "open terminal", "cloud", {}, Commands(), {}, Snap(), playback)
            self.assertEqual(outcome, "interrupted")
            self.assertEqual(outcomes, [])
        finally:
            speech_config._speech_cache = old_seconds


class FatalExitTests(unittest.TestCase):
    """BUG-063: an unexpected worker error exits non-zero deliberately."""

    def test_unexpected_error_exits_70(self):
        import os
        import subprocess
        import sys
        workers = Path(__file__).resolve().parents[1] / "workers"
        env = dict(os.environ, NAVI_ISOLATED="1")
        result = subprocess.run(
            [sys.executable, str(workers / "voice.py")],
            env=env, capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, 70)
        self.assertIn("fatal worker error", result.stderr)


class CancelSignalTests(unittest.TestCase):
    def test_cancel_stamp_tracks_daemon_writes(self):
        with tempfile.TemporaryDirectory() as folder:
            previous = voice.CANCEL_FILE
            voice.CANCEL_FILE = Path(folder) / "voice.cancel"
            try:
                first = voice.cancel_stamp()
                voice.CANCEL_FILE.write_text("1")
                second = voice.cancel_stamp()
                self.assertNotEqual(first, second)
                self.assertGreater(second, 0)
            finally:
                voice.CANCEL_FILE = previous


class ExactlyOnceDispatchTests(unittest.TestCase):
    """BUG-056: the UI claim and the hold deadline never both dispatch."""

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="navi-claim-")
        folder = Path(self.temporary.name)
        self.previous = (voice.CLAIM_FILE, voice.CONFIRM_FILE)
        self.claim = folder / "voice.claim"
        self.confirm = folder / "voice.confirm"
        voice.CLAIM_FILE, voice.CONFIRM_FILE = self.claim, self.confirm

    def tearDown(self):
        voice.CLAIM_FILE, voice.CONFIRM_FILE = self.previous
        self.temporary.cleanup()

    def fixture(self, hold):
        """A held transcript: returns (run, dispatched texts) for one capture."""
        import speech_config
        self.addCleanup(setattr, speech_config, "_speech_cache",
                        speech_config._speech_cache)
        speech_config._speech_cache = dict(speech_config.DEFAULTS,
                                           confirm_seconds=hold)
        self.addCleanup(setattr, voice, "state", voice.state)
        voice.state = lambda status, **kw: None
        dispatched = []

        class Snap:
            @property
            def value(self):
                return {"settings": {"microphone": True, "listening": "background",
                                     "speech": {}}, "gui_open": False}

        class Commands:
            def dispatch(self, text, speech, status=None):
                dispatched.append(text)
                return "deciding"

        def run():
            return voice._handle_transcript(
                "open terminal", "cloud", {"confidence": 0.9}, Commands(), {},
                Snap(), voice.Playback())

        return run, dispatched

    def test_headless_deadline_dispatches_exactly_once(self):
        run, dispatched = self.fixture(0.1)
        self.assertEqual(run(), "deciding")
        self.assertEqual(dispatched, ["open terminal"])
        # The winning claim survives until the next capture: a late Enter must
        # see it and refuse to dispatch again (BUG-056).
        self.assertTrue(self.claim.exists())
        self.assertFalse(self.confirm.exists())
        self.assertFalse(voice.claim_dispatch())
        self.assertEqual(dispatched, ["open terminal"])
        voice.clear_confirm()
        self.assertFalse(self.claim.exists())

    def test_ui_claim_during_the_hold_leaves_no_worker_dispatch(self):
        run, dispatched = self.fixture(0.4)
        worker = {}
        thread = threading.Thread(target=lambda: worker.update(outcome=run()))
        thread.start()
        time.sleep(0.1)
        with open(self.claim, "x", encoding="utf-8") as file:
            file.write("1")
        thread.join(5)
        self.assertFalse(thread.is_alive())
        self.assertEqual(worker["outcome"], "confirmed by UI")
        self.assertEqual(dispatched, [])
        self.assertTrue(self.claim.exists())

    def test_cancel_during_the_hold_sends_nothing(self):
        run, dispatched = self.fixture(0.4)
        worker = {}
        thread = threading.Thread(target=lambda: worker.update(outcome=run()))
        thread.start()
        time.sleep(0.1)
        self.confirm.write_text('{"accept": false}', encoding="utf-8")
        thread.join(5)
        self.assertFalse(thread.is_alive())
        self.assertEqual(worker["outcome"], "interrupted")
        self.assertEqual(dispatched, [])
        self.assertFalse(self.confirm.exists())

    def test_stale_markers_from_the_last_capture_do_not_block_dispatch(self):
        self.claim.write_text("1", encoding="utf-8")
        self.confirm.write_text('{"accept": false}', encoding="utf-8")
        run, dispatched = self.fixture(0.1)
        self.assertEqual(run(), "deciding")
        self.assertEqual(dispatched, ["open terminal"])


if __name__ == "__main__":
    unittest.main()
