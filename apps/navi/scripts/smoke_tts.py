#!/usr/bin/env python3
"""Live smoke test for the Navi TTS chain + sherpa STT roundtrip.

- Synthesizes "Good evening. Navi online." via edge-tts -> /tmp/navi_tts.mp3
- Synthesizes the same text via Piper -> /tmp/navi_tts_piper.wav (if installed)
- Prints latency to first audio for each provider
- Decodes one file to 16 kHz PCM (ffmpeg) and transcribes with local
  sherpa-onnx moonshine, printing the transcript
- --play also plays audio through the normal speak() path
"""
import argparse
import os
import subprocess
import sys
import time
import wave
from pathlib import Path

WORKERS = Path(__file__).resolve().parents[1] / "workers"
sys.path.insert(0, str(WORKERS))

TEXT = "Good evening. Navi online."
MP3 = Path("/tmp/navi_tts.mp3")
PIPER_WAV = Path("/tmp/navi_tts_piper.wav")


def synth_one(name):
    import tts
    provider = tts.PROVIDERS[name]
    start = time.monotonic()
    try:
        audio, sample_rate, fmt = provider(TEXT, None, None, 10.0)
    except Exception as error:  # noqa: BLE001
        return None, time.monotonic() - start, error
    return (audio, sample_rate, fmt), time.monotonic() - start, None


def decode_pcm16(path):
    out = subprocess.run(
        ["ffmpeg", "-v", "error", "-i", str(path), "-f", "s16le", "-ac", "1", "-ar", "16000", "-"],
        check=True, capture_output=True)
    import numpy as np
    return np.frombuffer(out.stdout, dtype=np.int16).astype(np.float32) / 32768.0


def transcribe_local(samples):
    import sherpa_onnx
    folder = Path.home() / ".local/share/navi/models/moonshine/sherpa-onnx-moonshine-tiny-en-quantized-2026-02-27"
    recognizer = sherpa_onnx.OfflineRecognizer.from_moonshine_v2(
        encoder=str(folder / "encoder_model.ort"),
        decoder=str(folder / "decoder_model_merged.ort"),
        tokens=str(folder / "tokens.txt"),
        num_threads=2)
    stream = recognizer.create_stream()
    stream.accept_waveform(16000, samples)
    recognizer.decode_stream(stream)
    return stream.result.text


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--play", action="store_true", help="Play audio through speak()")
    args = parser.parse_args()

    import tts
    import speech_config

    print(f"speech_mode={speech_config.speech_mode()} order={speech_config.provider_order('tts')}")

    if tts.provider_available("edge"):
        result, latency, error = synth_one("edge")
        if result:
            audio, rate, fmt = result
            MP3.write_bytes(audio)
            print(f"edge-tts : {len(audio)} bytes {fmt} in {latency:.2f}s -> {MP3}")
            roundtrip(MP3)
        else:
            print(f"edge-tts : FAILED in {latency:.2f}s: {error}")
    else:
        print("edge-tts : skipped (unavailable)")

    if tts.provider_available("piper"):
        result, latency, error = synth_one("piper")
        if result:
            audio, rate, fmt = result
            PIPER_WAV.write_bytes(audio)
            print(f"piper    : {len(audio)} bytes {fmt}@{rate} in {latency:.2f}s -> {PIPER_WAV}")
            roundtrip(PIPER_WAV)
        else:
            print(f"piper    : FAILED in {latency:.2f}s: {error}")
    else:
        print("piper    : skipped (unavailable)")

    if args.play:
        start = time.monotonic()
        ok = tts.speak(TEXT)
        print(f"speak()  : played={ok} in {time.monotonic() - start:.2f}s")


def roundtrip(path):
    try:
        start = time.monotonic()
        samples = decode_pcm16(path)
        transcript = transcribe_local(samples)
        print(f"  sherpa roundtrip ({time.monotonic() - start:.2f}s): {transcript!r}")
    except Exception as error:  # noqa: BLE001
        print(f"  sherpa roundtrip failed: {error}")


if __name__ == "__main__":
    main()
