#!/usr/bin/python3
"""Pinned decision artifacts. Download to .part, verify, then publish atomically."""
import hashlib
from pathlib import Path
import subprocess

DATA = Path.home() / ".local/share/navi"
REVISION = "bb1e436ea78eb96a3f1acb6da865f70c2fbeb563"
ARTIFACTS = [
    ("d1-3B-Q4_K_M.gguf", 1674456672, "16aff27ea2eefdc32b9897f43854a5d3170c1dc8dccb9c756905af30a4e22402"),
    ("mmproj-d1-3B-Q8_0.gguf", 583109728, "2505920ce464b7c54acd8b7b74a931c0c671f06d0c8833760b2fbceeb253e92a"),
]


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(8 * 1024 * 1024):
            value.update(chunk)
    return value.hexdigest()


def main():
    folder = DATA / "models/d1-3B"
    folder.mkdir(parents=True, exist_ok=True)
    for name, size, checksum in ARTIFACTS:
        path = folder / name
        if path.exists() and path.stat().st_size == size and digest(path) == checksum:
            print(f"Verified {name}")
            continue
        temporary = path.with_suffix(path.suffix + ".part")
        if not temporary.exists() or temporary.stat().st_size != size:
            subprocess.run(["curl","-fL","--retry","5","--retry-all-errors","--speed-time","30","--speed-limit","1000","-C","-",f"https://huggingface.co/LiquidAI/d1-3B-GGUF/resolve/{REVISION}/{name}","-o",str(temporary)],check=True)
        if temporary.stat().st_size != size or digest(temporary) != checksum:
            raise RuntimeError(f"Checksum verification failed for {name}; preserve the partial artifact for inspection")
        temporary.replace(path)
        print(f"Verified {name}")


if __name__ == "__main__":
    main()
