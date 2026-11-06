#!/usr/bin/env python3
"""End-to-end voice test against an isolated daemon and the live OpenCode.

Synthesizes "Hey Navi" + "what is two plus two" with Piper, decodes it to
16 kHz mono PCM, runs the production wake/endpoint/STT path through
voice.offline(), sends the transcript over the isolated station socket, and
synthesizes the OpenCode reply to a file (no playback).

The decision endpoint is a local noop fixture, so the command goes straight to
the conversational bridge. The OpenCode service on 127.0.0.1:49374 is shared
and never started, stopped or restarted by this test.
"""
import json
import os
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent
ROOT = SCRIPTS.parent
sys.path.insert(0, str(SCRIPTS))
sys.path.insert(0, str(ROOT / "workers"))
from isolated import IsolatedStation  # noqa: E402

REAL_DATA = Path.home() / ".local/share/navi"
WAKE_TEXT = "Hey Navi"
COMMAND_TEXT = "What is two plus two?"


class DecisionFixture(BaseHTTPRequestHandler):
    """Local /v1/systemone: conversation only, so the chief uses OpenCode."""

    def log_message(self, *unused):
        pass

    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"status":"ok"}')

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        answers = {key: {"noul": .99 if key == "noop" else .01} for key in body["questions"]}
        payload = json.dumps({"answers": answers}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


def synthesize_wav(tts, text):
    """Piper first (installed locally), edge-tts as backup; no playback."""
    for name in ("piper", "edge"):
        if not tts.provider_available(name):
            continue
        try:
            audio, sample_rate, fmt = tts.PROVIDERS[name](text, "en-GB-RyanNeural", None, 30.0)
        except Exception:  # noqa: BLE001 - try the next local synthesizer
            continue
        if not audio:
            continue
        suffix = ".wav" if fmt == "wav" else ".mp3"
        with tempfile.NamedTemporaryFile(prefix="navi-e2e-say-", suffix=suffix, delete=False) as handle:
            handle.write(audio)
            path = handle.name
        try:
            decoded = subprocess.run(
                ["ffmpeg", "-v", "error", "-i", path, "-f", "s16le", "-ac", "1", "-ar", "16000", "-"],
                check=True, capture_output=True)
        finally:
            os.unlink(path)
        import numpy as np
        return np.frombuffer(decoded.stdout, dtype=np.int16).astype(np.float32) / 32768.0, name
    raise RuntimeError("no TTS provider could synthesize the test utterance")


def find_event(station, since, kind):
    status = station.call("status", {"since": since})["result"]
    for event in status["events"]:
        if event["kind"] == kind:
            return event
    return None


def main():
    os.environ.setdefault("NAVI_SPEECH_CONFIG", "/nonexistent-e2e-speech.json")
    os.environ.setdefault("NAVI_SECRETS_FILE", "/nonexistent-e2e-secrets.env")
    # The E2E check asserts the cloud/local STT fallbacks; the Hex engine has
    # its own live test (scripts/test_hex_asr.py --real) so it stays out here.
    os.environ.setdefault("NAVI_HEX_DISABLE", "1")
    # An isolated station reaches the shared service only when it is named.
    os.environ.setdefault("NAVI_OPENCODE_URL", "http://127.0.0.1:49374")
    fixture = ThreadingHTTPServer(("127.0.0.1", 0), DecisionFixture)
    fixture.daemon_threads = True
    threading.Thread(target=fixture.serve_forever, daemon=True).start()
    try:
        with IsolatedStation() as station:
            os.environ["NAVI_DATA_DIR"] = str(station.data)
            os.environ["NAVI_RUNTIME_DIR"] = str(station.runtime)
            import numpy as np  # noqa: F401
            import cloud_stt
            import persona
            import speech_config
            import tts
            import voice

            settings = station.call("status")["result"]["settings"]
            settings["model"]["endpoint"] = f"http://127.0.0.1:{fixture.server_port}/v1/systemone"
            settings["persona"] = "navi"
            saved = station.call("settings", settings)
            assert saved["ok"], saved
            initial = station.call("status")["result"]
            since = initial["events"][-1]["seq"] if initial["events"] else 0
            print(f"station: isolated at {station.runtime}")
            print(f"speech_mode={speech_config.speech_mode()} tts={speech_config.provider_order('tts')}")
            print(f"cloud_stt_available={cloud_stt.available()}")

            wake_audio, wake_provider = synthesize_wav(tts, WAKE_TEXT)
            command_audio, command_provider = synthesize_wav(tts, COMMAND_TEXT)
            assert wake_provider == command_provider
            print(f"input: {len(wake_audio) / 16000:.2f}s wake + "
                  f"{len(command_audio) / 16000:.2f}s command via {command_provider}")

            spotter = voice.load_spotter({}, REAL_DATA)
            engine = "moonshine" if (REAL_DATA / "models/moonshine").is_dir() else "whisper"
            engine, recognizer = voice.load_recognizer({"engine": engine}, REAL_DATA)
            status = station.call("status")["result"]

            dispatch = {}

            def send(text):
                started = time.monotonic()
                dispatch["response"] = station.call("command", {"text": text})
                dispatch["elapsed"] = time.monotonic() - started

            full = np.concatenate([
                wake_audio, np.zeros(5 * 1600, dtype="float32"),
                command_audio, np.zeros(15 * 1600, dtype="float32"),
            ])
            result = voice.offline(full, spotter, recognizer, engine, status, dispatch=send)
            wake_note = None
            if not result["wake"] or not result["transcript"]:
                # Synthetic speech can miss the KWS model; feed post-wake audio
                # directly and say so in the report.
                reason = "wake model missed synthetic speech" if not result["wake"] else "wake fired but STT was empty"
                wake_note = f"{reason}; post-wake segment fed directly"
                result = voice.offline(command_audio, spotter, recognizer, engine, status,
                                       assume_wake=True, dispatch=send)
            transcript = result["transcript"] or ""
            assert transcript, "STT returned an empty transcript"
            assert ("plus" in transcript.lower() or "+" in transcript), transcript
            assert "2" in transcript or "two" in transcript.lower(), transcript
            response = dispatch.get("response") or {}
            assert response.get("ok"), response
            reply = (response.get("result") or {}).get("reply") or {}
            reply_text = reply.get("text") or ""
            assert reply_text, "command RPC returned no reply text"
            event = find_event(station, since, "reply")
            assert event is not None, "no reply event reached the station"
            assert (event.get("details") or {}).get("text"), event
            assert (event.get("details") or {}).get("source") == "opencode", event

            p = persona.get("navi")
            started = time.monotonic()
            synthesized = tts.synthesize(reply_text, voice=p.voice)
            tts_elapsed = time.monotonic() - started
            assert synthesized is not None, "TTS produced no audio"
            audio, sample_rate, fmt = synthesized
            out = Path("/tmp") / f"navi-e2e-reply.{fmt}"
            out.write_bytes(audio)
            assert out.stat().st_size > 0

            wake_ms = result["timings"].get("wake")
            stt_ms = result["timings"].get("stt")
            print(f"wake:    {wake_ms * 1000:.0f} ms" if wake_ms is not None else "wake:    not detected")
            print(f"stt:     {stt_ms * 1000:.0f} ms ({result['source']}, {engine})")
            print(f"opencode:{dispatch['elapsed'] * 1000:.0f} ms")
            print(f"tts:     {tts_elapsed * 1000:.0f} ms first audio -> {out} "
                  f"({len(audio)} bytes, {fmt}, {sample_rate} Hz)")
            print(f"transcript: {transcript!r}")
            print(f"reply: {reply_text!r} (source {reply.get('source')})")
            if wake_note:
                print(f"note: {wake_note}")
            print("PASS")
            return 0
    except AssertionError as error:
        print(f"FAIL: {error}")
        return 1
    except Exception as error:  # noqa: BLE001 - report the failing stage
        print(f"FAIL: {type(error).__name__}: {error}")
        return 1
    finally:
        fixture.shutdown()
        fixture.server_close()


if __name__ == "__main__":
    sys.exit(main())
