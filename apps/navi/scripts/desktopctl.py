#!/usr/bin/python3
"""Native action and capture diagnostics through the authorized local portal worker."""
import argparse
import base64
import json
import os
from pathlib import Path
import socket
import sys
import time

sys.path.insert(0,str(Path(__file__).resolve().parents[1]/"workers"))
from ipc import RUNTIME, rpc


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("action",nargs="?")
    parser.add_argument("--capture")
    args=parser.parse_args()
    if args.capture:
        state=rpc("status")["desktop"]
        previous=state.get("frame_time",0)
        deadline=time.monotonic()+3
        while time.monotonic()<deadline:
            time.sleep(.1)
            state=rpc("status")["desktop"]
            if state.get("frame_time",0)>previous:break
        if "image" not in state:raise RuntimeError("Desktop has no connected frame")
        Path(args.capture).write_bytes(base64.b64decode(state["image"].split(",",1)[1]))
        print(json.dumps({k:v for k,v in state.items() if k!="image"}))
    else:
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(10);connection.connect(str(RUNTIME/"desktop.sock"))
            connection.sendall((args.action+"\n").encode())
            print(connection.makefile("r").readline())


if __name__=="__main__":main()
