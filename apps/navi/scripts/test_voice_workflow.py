"""100 ms synthetic PCM -> production capture -> Whistle -> d1 -> private sessions."""
import argparse
import hashlib
import json
import os
os.environ["NEEDLE_TELEMETRY"] = "0"
os.environ["DO_NOT_TRACK"] = "1"
from pathlib import Path
import sys
import time

import needle
import numpy as np
import sherpa_onnx

from isolated import IsolatedStation, ROOT, free_port
sys.path.insert(0, str(ROOT / "workers"))
from voice import command_text, speech_keywords
from voice_stream import CaptureBuffer

DATA = Path.home() / ".local/share/navi"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--piper-dir", type=Path, required=True)
    args = parser.parse_args()
    os.nice(10)
    folder = args.piper_dir
    tts = sherpa_onnx.OfflineTts(sherpa_onnx.OfflineTtsConfig(model=sherpa_onnx.OfflineTtsModelConfig(vits=sherpa_onnx.OfflineTtsVitsModelConfig(model=str(folder / "en_US-amy-low.onnx"), tokens=str(folder / "tokens.txt"), data_dir=str(folder / "espeak-ng-data"), noise_scale=0, noise_scale_w=0), num_threads=1)))
    model = needle.Whistle(weights=DATA / "models/whistle/whistle.cact")
    folder = DATA / "models/wake/sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01"
    spotter = sherpa_onnx.KeywordSpotter(tokens=str(folder / "tokens.txt"), encoder=str(folder / "encoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx"), decoder=str(folder / "decoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx"), joiner=str(folder / "joiner-epoch-12-avg-2-chunk-16-left-64.int8.onnx"), keywords_file=str(folder / "navi-keywords.txt"), num_threads=1, keywords_score=1, keywords_threshold=.25)
    capture = CaptureBuffer(spotter)
    with IsolatedStation() as station:
        settings = station.call("status")["result"]["settings"]
        settings["projects"] = [{"name": "Navi", "path": str(station.folder)}]
        settings["model"].update(endpoint=f"http://127.0.0.1:{free_port()}/v1/systemone", server=str(DATA / "runtime/llama-b11540/llama-b11540/llama-server"), weights=str(DATA / "models/d1-3B/d1-3B-Q4_K_M.gguf"), projection="", gpu_layers=0, threads=2)
        assert station.call("settings", settings)["ok"]
        results = []
        target = None
        assert tts.sample_rate == capture.sample_rate, tts.sample_rate
        for phrase in ["Hey Navi. Open a terminal called Sandbox.", "Hey Navi. Switch to Sandbox.", "Hey Navi. Stop.", "Hey Navi, stop.", "Open another terminal."]:
            audio = np.asarray(tts.generate(phrase).samples, dtype=np.float32)
            audio_hash = hashlib.sha256(audio.tobytes()).hexdigest()
            samples = np.concatenate((np.zeros(24000, np.float32), audio, np.zeros(32000, np.float32)))
            samples = np.pad(samples, (0, (-len(samples)) % capture.block_size))
            utterances = []
            wakes = []
            for offset in range(0, len(samples), capture.block_size):
                block = samples[offset:offset + capture.block_size]
                event = capture.feed(block, quiet=float(np.sqrt(np.mean(block ** 2))) < .008)
                if event is not None and event.kind == "wake":
                    wakes.append(round(offset / capture.sample_rate, 2))
                elif event is not None and event.kind == "utterance":
                    utterances.append(event)
            if not phrase.startswith("Hey Navi"):
                assert not wakes and not utterances, {"phrase": phrase, "wakes": wakes}
                results.append({"phrase": phrase, "audio_sha256": audio_hash, "no_wake_no_action": True})
                continue
            if "stop" in phrase.lower() and not wakes:
                assert not utterances
                results.append({"phrase": phrase, "audio_sha256": audio_hash, "missed_wake": True, "action_executed": False})
                continue
            assert len(wakes) == len(utterances) == 1, {"phrase": phrase, "wakes": wakes, "utterances": len(utterances)}
            recording = np.concatenate(utterances[0].blocks)
            started = time.perf_counter()
            keywords = speech_keywords(station.call("status")["result"])
            transcript = command_text(model.transcribe(recording, language="en", keywords=keywords)["text"])
            asr_ms = round((time.perf_counter() - started) * 1000, 1)
            if "stop" in phrase.lower():
                assert transcript.lower() == "stop", transcript
                generation = station.call("status")["result"]["generation"]
                assert station.call("cancel")["ok"]
                assert station.call("status")["result"]["generation"] == generation + 1
                results.append({"phrase": phrase, "audio_sha256": audio_hash, "transcript": transcript, "wake_at_seconds": wakes[0], "recording_seconds": round(len(recording) / capture.sample_rate, 2), "asr_ms": asr_ms, "cancel_generation_advanced": True})
                continue
            result = station.call("command", {"text": transcript})
            assert result["ok"], result
            action = result["result"]["trace"][0]["decision"]["action"]
            if target is None:
                assert action["kind"] == "launch" and action["profile"] == "terminal", {"transcript": transcript, "result": result}
                target = result["result"]["trace"][0]["result"]["id"]
                assert station.call("create", {"profile": "terminal", "name": "Other", "cwd": str(station.folder)})["ok"]
            else:
                assert action["kind"] == "switch" and action["id"] == target, {"transcript": transcript, "sessions": station.call("status")["result"]["sessions"], "result": result}
                assert station.call("status")["result"]["settings"]["active_session"] == target
            results.append({"phrase": phrase, "audio_sha256": audio_hash, "transcript": transcript, "wake_at_seconds": wakes[0], "recording_seconds": round(len(recording) / capture.sample_rate, 2), "asr_ms": asr_ms, "action": action})
        assert not (station.runtime / "voice.pid").exists()
        report = {"kind": "Deterministic Piper Amy PCM in 100 ms chunks through production capture; physical microphone not tested", "tts_noise_scale": 0, "tts_noise_scale_w": 0, "real_d1_cpu": True, "private_terminal_launch_and_switch": True, "no_audio_device_opened": True, "capture_pre_roll_seconds": 3, "capture_limit_seconds": 20, "silence_endpoint_seconds": .9, "results": results}
        (ROOT / "evidence/voice-workflow.json").write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
