"""Hex transcription bridge: availability checks, stub engine, optional live run.

The stub executable speaks the same CLI surface as `hex`
(`hex model status`, `hex model transcribe <wav>`), so no audio devices or
731 MB model are needed here. A real-engine check runs only when
NAVI_HEX_LIVE=1 and NAVI_HEX_SAMPLE points at a WAV file.
"""
import os
from pathlib import Path
import stat
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "workers"))
import hex_asr


def stub(folder, transcript="hello from hex", status="installed\tParakeet Unified English",
         exit_code=0, check_wav=False):
    path = Path(folder) / "hex"
    check = 'head -c 4 "$3" | grep -q RIFF || exit 9\n' if check_wav else ""
    path.write_text(
        "#!/bin/bash\n"
        'case "$1 $2" in\n'
        f'  "model status") echo -e "{status}"; exit 0;;\n'
        '  "model transcribe")\n'
        f'    {check}echo "{transcript}"; exit {exit_code};;\n'
        "esac\n"
        "exit 1\n"
    )
    path.chmod(path.stat().st_mode | stat.S_IXUSR)
    return path


class StubEngineTests(unittest.TestCase):
    def setUp(self):
        self.folder = tempfile.TemporaryDirectory()
        hex_asr.reset_cache()

    def tearDown(self):
        hex_asr.reset_cache()
        self.folder.cleanup()

    def test_installed_engine_is_detected(self):
        binary = stub(self.folder.name)
        self.assertTrue(hex_asr.available(executable=binary))

    def test_missing_model_is_not_available(self):
        binary = stub(self.folder.name, status="missing\tParakeet Unified English")
        self.assertFalse(hex_asr.available(executable=binary))

    def test_missing_binary_is_not_available(self):
        self.assertFalse(hex_asr.available(executable=Path(self.folder.name) / "nope"))

    def test_probe_failure_retries_after_the_negative_cache(self):
        """BUG-068: a transient failure is not a permanent demotion."""
        import time
        binary = stub(self.folder.name)
        # Force a probe failure without a missing binary: a dead stub path
        # is permanent, so simulate by marking _installed with an old float.
        self.assertTrue(hex_asr.available(executable=binary))
        hex_asr._installed = time.monotonic() - hex_asr.NEGATIVE_CACHE_SECONDS - 1
        # Expired negative: probes again and caches the positive.
        self.assertTrue(hex_asr.available(executable=binary))
        self.assertIs(hex_asr._installed, True)

    def test_fresh_probe_failure_is_cached_for_sixty_seconds(self):
        """BUG-068: inside the cache window no second probe is spawned."""
        import subprocess
        calls = []

        def boom(*a, **k):
            calls.append(1)
            raise subprocess.TimeoutExpired("hex", 10)

        binary = stub(self.folder.name)
        old_run = subprocess.run
        try:
            hex_asr._installed = None
            subprocess.run = boom
            self.assertFalse(hex_asr.available(executable=binary))
            self.assertFalse(hex_asr.available(executable=binary))
            self.assertEqual(len(calls), 1)
            # The second call uses the cache; a real probe only happens
            # after NEGATIVE_CACHE_SECONDS has elapsed.
            import time
            subprocess.run = old_run
            hex_asr._installed = time.monotonic() - hex_asr.NEGATIVE_CACHE_SECONDS - 1
            self.assertTrue(hex_asr.available(executable=binary))
        finally:
            subprocess.run = old_run

    def test_missing_binary_does_not_probe(self):
        """BUG-068: 'binary missing' is permanent, no subprocess spawned."""
        import subprocess
        calls = []
        old_run = subprocess.run
        try:
            subprocess.run = lambda *a, **k: calls.append(1) or None
            self.assertFalse(hex_asr.available(executable=Path(self.folder.name) / "nope"))
            self.assertFalse(hex_asr.available(executable=Path(self.folder.name) / "nope"))
            self.assertEqual(calls, [])
        finally:
            subprocess.run = old_run

    def test_disabled_by_environment(self):
        binary = stub(self.folder.name)
        old = os.environ.get("NAVI_HEX_DISABLE")
        os.environ["NAVI_HEX_DISABLE"] = "1"
        try:
            self.assertFalse(hex_asr.available(executable=binary))
        finally:
            if old is None:
                os.environ.pop("NAVI_HEX_DISABLE", None)
            else:
                os.environ["NAVI_HEX_DISABLE"] = old

    def test_transcribe_returns_text_and_sends_a_wav(self):
        import numpy as np

        binary = stub(self.folder.name, check_wav=True)
        audio = [np.zeros(1600, dtype="float32")]
        self.assertEqual(hex_asr.transcribe(audio, executable=binary), "hello from hex")

    def test_transcribe_failure_returns_none(self):
        import numpy as np

        binary = stub(self.folder.name, exit_code=3)
        self.assertIsNone(hex_asr.transcribe([np.zeros(1600, dtype="float32")], executable=binary))

    def test_transcribe_empty_output_returns_none(self):
        import numpy as np

        binary = stub(self.folder.name, transcript="")
        self.assertIsNone(hex_asr.transcribe([np.zeros(1600, dtype="float32")], executable=binary))


class LiveEngineTests(unittest.TestCase):
    @unittest.skipUnless(os.environ.get("NAVI_HEX_LIVE") == "1", "set NAVI_HEX_LIVE=1 to use the real engine")
    def test_real_hex_transcribes_a_sample(self):
        sample = os.environ.get("NAVI_HEX_SAMPLE")
        self.assertTrue(sample and Path(sample).is_file(), "NAVI_HEX_SAMPLE must point at a WAV")
        import numpy as np
        import wave

        with wave.open(sample, "rb") as handle:
            frames = handle.readframes(handle.getnframes())
            rate = handle.getframerate()
        samples = np.frombuffer(frames, dtype="<i2").astype(np.float32) / 32768.0
        self.assertTrue(hex_asr.available(refresh=True))
        text = hex_asr.transcribe([samples], rate)
        self.assertTrue(text, "hex returned no transcript")
        self.assertTrue(any(word in text.lower() for word in ("navi", "capital", "france")), text)


if __name__ == "__main__":
    unittest.main()
