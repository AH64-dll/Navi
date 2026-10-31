#!/usr/bin/env python3
"""Unit tests for workers/cloud_stt.py provider chain. Mock HTTP servers
only; no real API calls, no credentials required."""
import json
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
import cloud_stt  # noqa: E402

ALL_KEYS = ("GROQ_API_KEY", "CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_API_TOKEN", "GEMINI_API_KEY")
ORIG_PROVIDERS = dict(cloud_stt.PROVIDERS)
ORIG_AVAILABLE = cloud_stt.provider_available
ORIG_TIMEOUT = speech_config.timeout_seconds


def reset():
    speech_config.reset_caches()
    cloud_stt._BREAKERS.clear()
    cloud_stt.PROVIDERS.clear()
    cloud_stt.PROVIDERS.update(ORIG_PROVIDERS)
    cloud_stt.provider_available = ORIG_AVAILABLE
    for name in ALL_KEYS:
        os.environ.pop(name, None)
    speech_config.timeout_seconds = ORIG_TIMEOUT


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        if self.server.delay:
            time.sleep(self.server.delay)
        length = int(self.headers.get("Content-Length", "0"))
        self.rfile.read(length)
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        if "/groq" in self.path:
            self.wfile.write(json.dumps({"text": "groq transcript"}).encode())
        elif "/cf/" in self.path:
            self.wfile.write(json.dumps({"success": True,
                                         "result": {"text": "cf transcript"}}).encode())
        else:  # gemini
            self.wfile.write(json.dumps({"steps": [{"content": [
                {"type": "text", "text": "gemini transcript"}]}]}).encode())


def start_server(delay=0.0):
    server = HTTPServer(("127.0.0.1", 0), Handler)
    server.delay = delay
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


PCM = b"\x00\x01" * 8000  # 1 s of 16 kHz s16le (not RIFF)


class TestChain(unittest.TestCase):
    def setUp(self):
        reset()
        self.server = start_server()
        port = self.server.server_port
        cloud_stt.GROQ_URL = f"http://127.0.0.1:{port}/groq"
        cloud_stt.CF_STT_URL = f"http://127.0.0.1:{port}/cf/{{account}}"
        cloud_stt.GEMINI_URL = f"http://127.0.0.1:{port}/gemini"

    def tearDown(self):
        self.server.shutdown()
        reset()

    def test_wav_wrapping(self):
        wav = cloud_stt.pcm16_to_wav(PCM, 16000)
        self.assertEqual(wav[:4], b"RIFF")
        self.assertEqual(wav[8:12], b"WAVE")
        self.assertEqual(len(wav), 44 + len(PCM))
        # real WAV passes through unchanged
        self.assertIs(cloud_stt.pcm16_to_wav(wav, 16000), wav)

    def test_groq_primary(self):
        os.environ["GROQ_API_KEY"] = "k"
        speech_config.reset_caches()
        self.assertEqual(cloud_stt.transcribe(PCM), "groq transcript")

    def test_failover_to_cloudflare(self):
        def fail(*a):
            raise RuntimeError("groq down")
        cloud_stt.PROVIDERS["groq"] = fail
        cloud_stt.provider_available = (
            lambda n, orig=cloud_stt.provider_available:
                not cloud_stt._breaker_open(n) and (n == "groq" or orig(n)))
        os.environ["CLOUDFLARE_ACCOUNT_ID"] = "a"
        os.environ["CLOUDFLARE_API_TOKEN"] = "t"
        speech_config.reset_caches()
        # groq is attempted, fails, breaker opens; cloudflare answers.
        self.assertEqual(cloud_stt.transcribe(PCM, order=["groq", "cloudflare"]),
                         "cf transcript")
        self.assertTrue(cloud_stt._breaker_open("groq"))

    def test_gemini_third_hop(self):
        os.environ["GEMINI_API_KEY"] = "k"
        speech_config.reset_caches()
        self.assertEqual(cloud_stt.transcribe(PCM, order=["gemini"]), "gemini transcript")

    def test_no_creds_returns_none(self):
        self.assertIsNone(cloud_stt.transcribe(PCM))

    def test_breaker_skips_provider(self):
        attempts = []
        def fail(*a):
            attempts.append(1)
            raise RuntimeError("x")
        cloud_stt.PROVIDERS["groq"] = fail
        cloud_stt.provider_available = lambda n: not cloud_stt._breaker_open(n)
        self.assertIsNone(cloud_stt.transcribe(PCM, order=["groq"]))
        self.assertIsNone(cloud_stt.transcribe(PCM, order=["groq"]))
        self.assertEqual(len(attempts), 1)

    def test_timeout_falls_back(self):
        self.server.shutdown()
        slow = start_server(delay=2.0)
        cloud_stt.GROQ_URL = f"http://127.0.0.1:{slow.server_port}/groq"
        os.environ["GROQ_API_KEY"] = "k"
        speech_config.reset_caches()
        speech_config.timeout_seconds = lambda: 0.4
        start = time.monotonic()
        out = cloud_stt.transcribe(PCM, order=["groq"])
        elapsed = time.monotonic() - start
        slow.shutdown()
        self.assertIsNone(out)
        self.assertLess(elapsed, 1.5)
        self.assertTrue(cloud_stt._breaker_open("groq"))

    def test_local_only_mode_returns_none(self):
        cfg = dict(speech_config.DEFAULTS, speech_mode="local-only")
        speech_config._speech_cache = cfg
        os.environ["GROQ_API_KEY"] = "k"
        self.assertEqual(speech_config.provider_order("stt"), [])
        self.assertIsNone(cloud_stt.transcribe(PCM))

    def test_empty_audio(self):
        self.assertIsNone(cloud_stt.transcribe(b""))

    def test_available_is_false_without_credentials(self):
        self.assertFalse(cloud_stt.available())

    def test_available_true_when_any_hop_has_credentials(self):
        os.environ["GEMINI_API_KEY"] = "k"
        speech_config.reset_caches()
        self.assertTrue(cloud_stt.available())

    def test_language_forwarded_to_gemini(self):
        seen = {}
        def spy(wav, language, timeout):
            seen["language"] = language
            seen["wav_riff"] = wav[:4]
            return "ok"
        cloud_stt.PROVIDERS["gemini"] = spy
        cloud_stt.provider_available = lambda n: True
        self.assertEqual(cloud_stt.transcribe(PCM, language="en", order=["gemini"]), "ok")
        self.assertEqual(seen["language"], "en")
        self.assertEqual(seen["wav_riff"], b"RIFF")


if __name__ == "__main__":
    unittest.main(verbosity=2)
