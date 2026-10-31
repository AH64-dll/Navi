"""Synthetic speech smoke checks, not an accent/noisy-room acceptance claim."""
import json
import argparse
import os
os.environ["NEEDLE_TELEMETRY"]="0"
os.environ["DO_NOT_TRACK"]="1"
from pathlib import Path
import subprocess
import tempfile
import time
import wave

import needle
import numpy as np
import sherpa_onnx

DATA=Path.home()/".local/share/navi"
ROOT=Path(__file__).resolve().parents[1]


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("--piper-dir",type=Path)
    args=parser.parse_args()
    os.nice(10)
    tts=None
    if args.piper_dir:
        folder=args.piper_dir
        tts=sherpa_onnx.OfflineTts(sherpa_onnx.OfflineTtsConfig(model=sherpa_onnx.OfflineTtsModelConfig(vits=sherpa_onnx.OfflineTtsVitsModelConfig(model=str(folder/"en_US-amy-low.onnx"),tokens=str(folder/"tokens.txt"),data_dir=str(folder/"espeak-ng-data")),num_threads=1)))
    whistle=needle.Whistle(weights=DATA/"models/whistle/whistle.cact")
    folder=DATA/"models/wake/sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01"
    spotter=sherpa_onnx.KeywordSpotter(tokens=str(folder/"tokens.txt"),encoder=str(folder/"encoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx"),decoder=str(folder/"decoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx"),joiner=str(folder/"joiner-epoch-12-avg-2-chunk-16-left-64.int8.onnx"),keywords_file=str(folder/"navi-keywords.txt"),num_threads=1,keywords_score=1,keywords_threshold=.25)
    moonshine_folder=DATA/"models/moonshine/sherpa-onnx-moonshine-tiny-en-quantized-2026-02-27"
    moonshine=sherpa_onnx.OfflineRecognizer.from_moonshine_v2(encoder=str(moonshine_folder/"encoder_model.ort"),decoder=str(moonshine_folder/"decoder_model_merged.ort"),tokens=str(moonshine_folder/"tokens.txt"),num_threads=2)
    folder=DATA/"models/whisper-tiny.en"
    whisper=sherpa_onnx.OfflineRecognizer.from_whisper(encoder=str(folder/"tiny.en-encoder.int8.onnx"),decoder=str(folder/"tiny.en-decoder.int8.onnx"),tokens=str(folder/"tiny.en-tokens.txt"),num_threads=2,tail_paddings=1000)
    results=[]
    with tempfile.TemporaryDirectory(prefix="navi-voice-") as temporary:
        source=Path(temporary)/"source.wav";audio=Path(temporary)/"audio.wav"
        for phrase in ["Open Codex.","Open Claude.","Launch Claude Code.","Start Pi.","Open a terminal.","Switch to Review.","Open the browser.","Stop.","Cancel.","Hey Navi. Open Codex.","Hey Navi. Open Claude.","Hey Navi. Switch to Review.","The weather is nice today."]:
            if tts:
                samples=np.asarray(tts.generate(phrase).samples,dtype=np.float32)
            else:
                subprocess.run(["espeak-ng","-v","en-us","-s","140","-w",str(source),phrase],check=True)
                subprocess.run(["ffmpeg","-hide_banner","-loglevel","error","-y","-i",str(source),"-ar","16000","-ac","1",str(audio)],check=True)
                with wave.open(str(audio)) as stream:
                    samples=np.frombuffer(stream.readframes(stream.getnframes()),dtype=np.int16).astype(np.float32)/32768
            started=time.perf_counter();transcript=whistle.transcribe(samples,language="en",keywords=["Navi","Codex","Claude","Pi"])
            elapsed=(time.perf_counter()-started)*1000
            started=time.perf_counter();moon_stream=moonshine.create_stream();moon_stream.accept_waveform(16000,samples);moonshine.decode_stream(moon_stream);moon_text=moon_stream.result.text;moon_elapsed=(time.perf_counter()-started)*1000
            started=time.perf_counter();whisper_stream=whisper.create_stream();whisper_stream.accept_waveform(16000,samples);whisper.decode_stream(whisper_stream);whisper_text=whisper_stream.result.text;whisper_elapsed=(time.perf_counter()-started)*1000
            wake=spotter.create_stream();detected=[]
            for offset in range(0,len(samples),1600):
                wake.accept_waveform(16000,samples[offset:offset+1600])
                while spotter.is_ready(wake):
                    spotter.decode_stream(wake)
                    if result:=spotter.get_result(wake):detected.append(result);spotter.reset_stream(wake)
            wake.accept_waveform(16000,np.zeros(16000,dtype=np.float32))
            while spotter.is_ready(wake):
                spotter.decode_stream(wake)
                if result:=spotter.get_result(wake):detected.append(result);spotter.reset_stream(wake)
            results.append({"phrase":phrase,"transcript":transcript,"elapsed_ms":round(elapsed,1),"moonshine":moon_text,"moonshine_ms":round(moon_elapsed,1),"whisper":whisper_text,"whisper_ms":round(whisper_elapsed,1),"wake":detected})
    silence=whistle.transcribe(np.zeros(16000,dtype=np.float32),language="en")
    report={"kind":"Piper Amy low synthetic English fixture; no microphone" if tts else "eSpeak synthetic speech; no microphone","wake_score":1,"wake_threshold":.25,"results":results,"silence":silence}
    (ROOT/("evidence/voice-piper.json" if tts else "evidence/voice-espeak-current.json")).write_text(json.dumps(report,indent=2)+"\n")
    print(json.dumps(report,indent=2))


if __name__=="__main__":main()
