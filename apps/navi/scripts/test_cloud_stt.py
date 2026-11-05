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

ALL_KEYS = ("DEEPGRAM_API_KEY", "GROQ_API_KEY", "CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_API_TOKEN", "GEMINI_API_KEY")
ORIG_PROVIDERS = dict(cloud_stt.PROVIDERS)
ORIG_AVAILABLE = cloud_stt.provider_available
ORIG_TIMEOUT = speech_config.timeout_seconds


def reset():
    speech_config.reset_caches()
    cloud_stt._BREAKERS.clear()
    cloud_stt._FAILURES.clear()
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
        if "/deepgram" in self.path:
            self.wfile.write(json.dumps({"results": {"channels": [
                {"alternatives": [{"transcript": "deepgram transcript"}]}]}}).encode())
        elif "/groq" in self.path:
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
        cloud_stt.DEEPGRAM_URL = f"http://127.0.0.1:{port}/deepgram?model=nova-3"
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

    def test_deepgram_primary(self):
        os.environ["DEEPGRAM_API_KEY"] = "k"
        os.environ["GROQ_API_KEY"] = "k"
        speech_config.reset_caches()
        self.assertEqual(cloud_stt.transcribe(PCM), "deepgram transcript")

    def test_failover_to_cloudflare(self):
        def fail(*a, **k):
            raise RuntimeError("groq down")
        cloud_stt.PROVIDERS["groq"] = fail
        cloud_stt.provider_available = (
            lambda n, orig=cloud_stt.provider_available:
                not cloud_stt._breaker_open(n) and (n == "groq" or orig(n)))
        os.environ["CLOUDFLARE_ACCOUNT_ID"] = "a"
        os.environ["CLOUDFLARE_API_TOKEN"] = "t"
        speech_config.reset_caches()
        # A non-transient failure still counts toward the breaker, but the
        # next hop answers in the same call.
        self.assertEqual(cloud_stt.transcribe(PCM, order=["groq", "cloudflare"]),
                         "cf transcript")
        self.assertEqual(cloud_stt._FAILURES.get("groq"), 1)

    def test_gemini_third_hop(self):
        os.environ["GEMINI_API_KEY"] = "k"
        speech_config.reset_caches()
        self.assertEqual(cloud_stt.transcribe(PCM, order=["gemini"]), "gemini transcript")

    def test_no_creds_returns_none(self):
        self.assertIsNone(cloud_stt.transcribe(PCM))

    def test_breaker_skips_provider(self):
        """BUG-059: the breaker opens only after consecutive failures."""
        attempts = []
        def fail(*a, **k):
            attempts.append(1)
            raise RuntimeError("x")
        cloud_stt.PROVIDERS["groq"] = fail
        cloud_stt.provider_available = lambda n: not cloud_stt._breaker_open(n)
        self.assertIsNone(cloud_stt.transcribe(PCM, order=["groq"]))
        self.assertFalse(cloud_stt._breaker_open("groq"))  # one miss != outage
        self.assertIsNone(cloud_stt.transcribe(PCM, order=["groq"]))
        self.assertTrue(cloud_stt._breaker_open("groq"))
        self.assertIsNone(cloud_stt.transcribe(PCM, order=["groq"]))
        self.assertEqual(len(attempts), 2)

    def test_transient_error_retries_once_then_recovers(self):
        """BUG-059: one 500 then success -> two attempts, breaker closed."""
        import urllib.error
        attempts = []
        def flaky(*a, **k):
            attempts.append(1)
            if len(attempts) == 1:
                raise urllib.error.HTTPError("u", 500, "x", {}, None)
            return "recovered"
        cloud_stt.PROVIDERS["groq"] = flaky
        cloud_stt.provider_available = lambda n: True
        speech_config._speech_cache = dict(speech_config.DEFAULTS, retry_attempts=1)
        self.assertEqual(cloud_stt.transcribe(PCM, order=["groq"]), "recovered")
        self.assertEqual(len(attempts), 2)
        self.assertFalse(cloud_stt._breaker_open("groq"))

    def test_auth_error_opens_the_breaker_immediately(self):
        """BUG-059: 401 is a credential problem, not a transient one."""
        import urllib.error
        attempts = []
        def denied(*a, **k):
            attempts.append(1)
            raise urllib.error.HTTPError("u", 401, "x", {}, None)
        cloud_stt.PROVIDERS["groq"] = denied
        cloud_stt.provider_available = lambda n: not cloud_stt._breaker_open(n)
        self.assertIsNone(cloud_stt.transcribe(PCM, order=["groq"]))
        self.assertTrue(cloud_stt._breaker_open("groq"))
        self.assertEqual(len(attempts), 1)  # no retry on auth failures

    def test_transcribe_result_reports_provider_and_confidence(self):
        """BUG-059: confidence survives to the caller."""
        cloud_stt.PROVIDERS["deepgram"] = lambda *a, **k: {
            "text": "low confidence", "confidence": 0.3}
        cloud_stt.provider_available = lambda n: True
        text, meta = cloud_stt.transcribe_result(PCM, order=["deepgram"])
        self.assertEqual(text, "low confidence")
        self.assertEqual(meta["provider"], "deepgram")
        self.assertEqual(meta["confidence"], 0.3)

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
        self.assertIsNone(out)
        # One retry on the timeout keeps the call bounded (BUG-059); a
        # single incident no longer trips the breaker...
        self.assertLess(elapsed, 2.0)
        self.assertFalse(cloud_stt._breaker_open("groq"))
        # ...but a second consecutive timed-out call does.
        self.assertIsNone(cloud_stt.transcribe(PCM, order=["groq"]))
        self.assertTrue(cloud_stt._breaker_open("groq"))
        slow.shutdown()

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
        """BUG-057: the configured language reaches the provider."""
        seen = {}
        def spy(wav, language, timeout, keywords=None):
            seen["language"] = language
            seen["wav_riff"] = wav[:4]
            return "ok"
        cloud_stt.PROVIDERS["gemini"] = spy
        cloud_stt.provider_available = lambda n: True
        self.assertEqual(cloud_stt.transcribe(PCM, language="fr", order=["gemini"]), "ok")
        self.assertEqual(seen["language"], "fr")
        self.assertEqual(seen["wav_riff"], b"RIFF")

    def test_keywords_reach_providers_that_support_them(self):
        """BUG-058: the hotword list is forwarded, capped at 20 terms."""
        seen = {}
        def spy(wav, language, timeout, keywords=None):
            seen["keywords"] = keywords
            return "ok"
        cloud_stt.PROVIDERS["deepgram"] = spy
        cloud_stt.provider_available = lambda n: True
        self.assertEqual(cloud_stt.transcribe(
            PCM, order=["deepgram"],
            keywords=["Claude", "Navi", "project-x"]), "ok")
        self.assertEqual(seen["keywords"], ["Claude", "Navi", "project-x"])

    def test_docstring_lists_every_configured_provider(self):
        """BUG-066: the module docstring cannot drift from CLOUD_STT."""
        doc = cloud_stt.__doc__
        for name in speech_config.CLOUD_STT:
            self.assertIn(name, doc)
        self.assertIn(" -> ".join(speech_config.CLOUD_STT), doc)
        self.assertNotIn("{CHAIN}", doc)


if __name__ == "__main__":
    unittest.main(verbosity=2)
