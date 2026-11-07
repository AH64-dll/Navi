#!/usr/bin/python3
"""Pinned Linux x86_64 runtime/speech assets; no services or windows are started."""
import argparse
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import zipfile

DATA = Path(os.environ.get("NAVI_DATA_DIR", Path.home() / ".local/share/navi"))
ASSETS = [
    ("runtime", "https://github.com/ggml-org/llama.cpp/releases/download/b11540/llama-b11540-bin-ubuntu-vulkan-x64.tar.gz", "d639b4b98d0c028e17ace64caaa9b881937b98da433840efd0316d560ee92e8f", "runtime/llama-b11540", "tar"),
    ("wake", "https://github.com/k2-fsa/sherpa-onnx/releases/download/kws-models/sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01.tar.bz2", "f170013b4716e41b62b9bfd809687c207cef798ef9bc6534d524e17af9b6561a", "models/wake", "tar"),
    ("vad", "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx", "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6", "models/vad/silero_vad.onnx", "file"),
    ("whistle", "https://huggingface.co/Cactus-Compute/whistle/resolve/ca5287601bef25af26dcf2e1b2bdc0843a7c19e5/whistle.cact", "b6e02f048568ac5d01a2042556c658061e699acbc0aa2a1439f52f3d461dffeb", "models/whistle/whistle.cact", "file"),
    ("needle", "https://huggingface.co/Cactus-Compute/needle3/resolve/8354babbfb92d719ee559f2999d133f11fb9374c/python/cactus_needle-3.2.0-py3-none-manylinux2014_x86_64.whl", "0e8a3bce4e52968ee14e8e9d47098e65ed06dd5b7c18cef3a7fa1375756736f9", "runtime/needle3/libneedle.so", "needle"),
    ("moonshine", "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-moonshine-tiny-en-quantized-2026-02-27.tar.bz2", "9ec31b342d8fa3240c3b81b8f82e1cf7e3ac467c93ca5a999b741d5887164f8d", "models/moonshine", "tar"),
    ("whisper", "https://huggingface.co/csukuangfj/sherpa-onnx-whisper-tiny.en/resolve/d026532c022fa99fd789d6b32446a1df7b6bfc43/tiny.en-encoder.int8.onnx", "0ce578b827c94a961aacb8fa14b02f096504b337e5c94be37c36238cbe3e8bc6", "models/whisper-tiny.en/tiny.en-encoder.int8.onnx", "file"),
    ("whisper", "https://huggingface.co/csukuangfj/sherpa-onnx-whisper-tiny.en/resolve/d026532c022fa99fd789d6b32446a1df7b6bfc43/tiny.en-decoder.int8.onnx", "06c0e6ff6348d427e51839219d1c886c18cfdf411e629e33f5e1679bff9c1527", "models/whisper-tiny.en/tiny.en-decoder.int8.onnx", "file"),
    ("whisper", "https://huggingface.co/csukuangfj/sherpa-onnx-whisper-tiny.en/resolve/d026532c022fa99fd789d6b32446a1df7b6bfc43/tiny.en-tokens.txt", "306cd27f03c1a714eca7108e03d66b7dc042abe8c258b44c199a7ed9838dd930", "models/whisper-tiny.en/tiny.en-tokens.txt", "file"),
]


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(8 * 1024 * 1024):
            value.update(chunk)
    return value.hexdigest()


def publish(source, target):
    target.parent.mkdir(parents=True, exist_ok=True)
    temporary = target.with_name(target.name + ".new")
    shutil.copy2(source, temporary)
    temporary.replace(target)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--only", nargs="+", choices=sorted({item[0] for item in ASSETS}), default=["runtime", "wake", "vad", "whistle", "needle"])
    parser.add_argument("--cache", type=Path, default=DATA / "downloads")
    args = parser.parse_args()
    os.nice(10)
    args.cache.mkdir(parents=True, exist_ok=True)
    DATA.mkdir(parents=True, exist_ok=True)
    for name, url, checksum, destination, kind in ASSETS:
        if name not in args.only:
            continue
        path = args.cache / url.rsplit("/", 1)[1]
        installed = DATA / destination
        if kind == "file" and installed.exists() and digest(installed) == checksum:
            print(f"Verified {destination}")
            continue
        if not path.exists() or digest(path) != checksum:
            partial = path.with_name(path.name + ".part")
            subprocess.run(["curl", "-4", "-fL", "--retry", "3", "--connect-timeout", "10", "--max-time", "180", "-C", "-", url, "-o", str(partial)], check=True)
            if digest(partial) != checksum:
                raise RuntimeError(f"Checksum mismatch for {name}")
            partial.replace(path)
        if kind == "file":
            publish(path, installed)
        elif kind == "needle":
            installed.parent.mkdir(parents=True, exist_ok=True)
            temporary = installed.with_suffix(".new")
            with zipfile.ZipFile(path) as archive:
                temporary.write_bytes(archive.read("needle/libneedle3.so"))
            temporary.replace(installed)
        else:
            with tempfile.TemporaryDirectory(prefix=".navi-assets-", dir=DATA) as temporary:
                staging = Path(temporary)
                with tarfile.open(path) as archive:
                    archive.extractall(staging, filter="data")
                for source in staging.rglob("*"):
                    if source.is_file():
                        publish(source, installed / source.relative_to(staging))
        print(f"Verified and installed {name}")
    if "wake" in args.only:
        (DATA / "models/wake/sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01/navi-keywords.txt").write_text("▁HE Y ▁NA VI\n")


if __name__ == "__main__":
    main()
