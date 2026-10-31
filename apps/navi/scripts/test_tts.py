#!/usr/bin/env python3
"""Unit tests for workers/tts.py provider chain. No live network:
cloud providers hit local mock HTTP servers, edge/piper are stubbed."""
import importlib
import os
import sys
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

WORKERS = Path(__file__).resolve().parents[1] / "workers"
sys.path.insert(0, str(WORKERS))
os.environ["NAVI_SPEECH_CONFIG"] = "/nonexistent-speech.json"
os.environ["NAVI_SECRETS_FILE"] = "/nonexistent-secrets.env"

import speech_config  # noqa: E402
import tts  # noqa: E402

ALL_KEYS = ("GROQ_API_KEY", "CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_API_TOKEN", "GEMINI_API_KEY")
ORIG_PROVIDERS = dict(tts.PROVIDERS)
ORIG_AVAILABLE = tts.provider_available
ORIG_TIMEOUT = speech_config.timeout_seconds


def reset():
    speech_config.reset_caches()
    tts._BREAKERS.clear()
    tts.PROVIDERS.clear()
    tts.PROVIDERS.update(ORIG_PROVIDERS)
    tts.provider_available = ORIG_AVAILABLE
    for name in ALL_KEYS:
        os.environ.pop(name, None)
    speech_config.timeout_seconds = ORIG_TIMEOUT


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        if self.server.delay:
            time.sleep(self.server.delay)
        self.send_response(200)
        if "/cf/" in self.path:
            self.send_header("Content-Type", "audio/mpeg")
            self.end_headers()
            self.wfile.write(b"FAKEMP3DATA")
        else:  # gemini interactions
            import base64
            import json
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            body = {"steps": [{"content": [{"type": "audio",
                                           "data": base64.b64encode(b"RIFFWAV").decode()}]}]}
            self.wfile.write(json.dumps(body).encode())


def start_server(delay=0.0):
    server = HTTPServer(("127.0.0.1", 0), Handler)
    server.delay = delay
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


class TestChain(unittest.TestCase):
    def setUp(self):
        reset()
        self.server = start_server()
        tts.CF_TTS_URL = f"http://127.0.0.1:{self.server.server_port}/cf/{{account}}"
        tts.GEMINI_URL = f"http://127.0.0.1:{self.server.server_port}/gemini"

    def tearDown(self):
        self.server.shutdown()
        reset()

    def test_skip_providers_without_creds(self):
        edge_called = []
        tts.PROVIDERS["edge"] = lambda *a: edge_called.append(1) or (b"x", 1, "mp3")
        tts.provider_available = lambda n: not tts._breaker_open(n) and n != "edge_fake"
        # keep edge "available" so the stub is exercised or skipped by order only
        os.environ["GEMINI_API_KEY"] = "k"
        speech_config.reset_caches()
        out = tts.synthesize("hello", order=["cloudflare", "gemini"])
        self.assertEqual(edge_called, [])
        self.assertEqual(out, (b"RIFFWAV", 24000, "wav"))

    def test_chain_order_and_failover(self):
        calls = []
        def bad_edge(*a):
            calls.append("edge")
            raise RuntimeError("down")
        tts.PROVIDERS["edge"] = bad_edge
        tts.provider_available = lambda n: not tts._breaker_open(n) and (
            n != "cloudflare" or bool(os.environ.get("CLOUDFLARE_API_TOKEN")))
        os.environ["CLOUDFLARE_ACCOUNT_ID"] = "a"
        os.environ["CLOUDFLARE_API_TOKEN"] = "t"
        speech_config.reset_caches()
        out = tts.synthesize("hi", order=["edge", "cloudflare"])
        self.assertEqual(calls, ["edge"])
        self.assertEqual(out, (b"FAKEMP3DATA", 24000, "mp3"))

    def test_breaker_disables_after_failure(self):
        attempts = []
        def fail(*a):
            attempts.append(1)
            raise RuntimeError("boom")
        tts.PROVIDERS["edge"] = fail
        tts.provider_available = lambda n: not tts._breaker_open(n)
        self.assertIsNone(tts.synthesize("a", order=["edge"]))
        self.assertIsNone(tts.synthesize("a", order=["edge"]))
        self.assertEqual(len(attempts), 1)  # second call skipped by breaker
        tts._BREAKERS.clear()
        self.assertIsNone(tts.synthesize("a", order=["edge"]))
        self.assertEqual(len(attempts), 2)

    def test_timeout_trips_breaker_falls_through(self):
        self.server.shutdown()
        slow = start_server(delay=2.0)
        tts.GEMINI_URL = f"http://127.0.0.1:{slow.server_port}/gemini"
        os.environ["GEMINI_API_KEY"] = "k"
        speech_config.reset_caches()
        speech_config.timeout_seconds = lambda: 0.4
        start = time.monotonic()
        out = tts.synthesize("hi", order=["gemini"])
        elapsed = time.monotonic() - start
        slow.shutdown()
        self.assertIsNone(out)
        self.assertLess(elapsed, 1.5)
        self.assertTrue(tts._breaker_open("gemini"))

    def test_local_only_mode_skips_cloud(self):
        cfg = dict(speech_config.DEFAULTS, speech_mode="local-only")
        speech_config._speech_cache = cfg
        calls = []
        tts.PROVIDERS["edge"] = lambda *a: calls.append("edge") or (b"x", 1, "mp3")
        tts.PROVIDERS["piper"] = lambda *a: calls.append("piper") or (b"W", 22050, "wav")
        tts.provider_available = lambda n: n in ("edge", "piper") and not tts._breaker_open(n)
        out = tts.synthesize("hello", order=speech_config.provider_order("tts"))
        self.assertEqual(calls, ["piper"])
        self.assertEqual(out, (b"W", 22050, "wav"))

    def test_cloud_only_mode_skips_piper(self):
        cfg = dict(speech_config.DEFAULTS, speech_mode="cloud-only")
        speech_config._speech_cache = cfg
        self.assertNotIn("piper", speech_config.provider_order("tts"))

    def test_sentence_split(self):
        self.assertEqual(tts._sentences("One. Two! Three?"), ["One.", "Two!", "Three?"])
        self.assertEqual(tts._sentences("single"), ["single"])

    def test_edge_rate_format(self):
        self.assertEqual(tts._to_edge_rate(1.25), "+25%")
        self.assertEqual(tts._to_edge_rate(0.8), "-20%")
        self.assertEqual(tts._to_edge_rate(None), "+0%")
        self.assertEqual(tts._to_edge_rate("+10%"), "+10%")

    def test_piper_voice_mapping_accepts_edge_and_piper_ids(self):
        self.assertEqual(tts._piper_voice_id("en-GB-RyanNeural"), "en_GB-alan-medium")
        self.assertEqual(tts._piper_voice_id("en-US-AriaNeural"), "en_GB-alan-medium")
        self.assertEqual(tts._piper_voice_id("en-US-GuyNeural"), "en_GB-alan-medium")
        self.assertEqual(tts._piper_voice_id("en_US-amy-high"), "en_US-amy-high")
        self.assertEqual(tts._piper_voice_id(None), tts.PIPER_DEFAULT)


class TestPlayback(unittest.TestCase):
    def setUp(self):
        reset()
        self._synth = tts.synthesize
        self._play = tts._play_bytes
        self._players = {k: list(v) for k, v in tts._PLAYERS.items()}

    def tearDown(self):
        reset()
        tts.synthesize = self._synth
        tts._play_bytes = self._play
        tts._PLAYERS.clear()
        tts._PLAYERS.update(self._players)

    def test_speak_returns_false_without_provider(self):
        tts.provider_available = lambda n: False
        self.assertFalse(tts.speak("nothing"))

    def test_speak_chunking_order(self):
        tts.provider_available = lambda n: n == "edge"
        played = []
        synths = []
        tts.synthesize = lambda text, **kw: synths.append(text) or (text.encode(), 24000, "mp3")
        tts._play_bytes = lambda audio, fmt, cancel_event=None: played.append(audio) or True
        ok = tts.speak("First. Second. Third.")
        self.assertTrue(ok)
        self.assertEqual(synths, ["First.", "Second.", "Third."])
        self.assertEqual(played, [b"First.", b"Second.", b"Third."])

    def test_speak_cancel_event(self):
        tts.provider_available = lambda n: n == "edge"
        tts.synthesize = lambda text, **kw: (text.encode(), 24000, "mp3")
        event = threading.Event()
        calls = []
        def fake_play(audio, fmt, cancel_event=None):
            calls.append(audio)
            if len(calls) == 1:
                event.set()
            return True
        tts._play_bytes = fake_play
        tts.speak("One. Two. Three.", cancel_event=event)
        self.assertEqual(calls, [b"One."])  # stopped after first

    def test_no_player_binary(self):
        tts._PLAYERS["wav"] = [("definitely-missing-player", ["x"])]
        self.assertFalse(tts._play_bytes(b"x", "wav"))


if __name__ == "__main__":
    unittest.main(verbosity=2)
