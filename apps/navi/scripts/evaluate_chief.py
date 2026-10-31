"""Live typed-decision evaluation. No harness executions or paid provider requests."""
import json
import argparse
import os
from pathlib import Path
import socket
import statistics
import subprocess
import tempfile
import time
from isolated import IsolatedStation, free_port

ROOT=Path(__file__).resolve().parents[1]
BINARY=Path(os.environ.get("NAVI_TEST_BINARY",str(Path.home()/".local/share/navi/bin/navid")))
DATA=Path.home()/".local/share/navi"
CASES=[
    ("Open Codex","launch:codex"), ("Launch Claude Code","launch:claude"),
    ("Start Pi","launch:pi"), ("Open a terminal","launch:terminal"),
    ("Create a Codex session called Review","launch:codex"),
    ("Launch Claude in Navi","launch:claude"),
    ("Open the browser","browser"), ("Bring up Zen","browser"),
    ("Don't open Claude", "noop"), ("Do not launch Codex","noop"),
    ("Cancel that", "noop"), ("Hello Navi", "noop"),
    ("What's the weather?","clarify"), ("Open that one","clarify"),
    ("Launch an unknown harness called Solaris","clarify"),
    ("Delete all my files","clarify"), ("Tell me a joke","noop"),
    ("Switch to the Design session","switch:DESIGN"),
    ("Go back to Review","switch:REVIEW"),
    ("Create a NEW terminal session","launch:terminal"),
    ("Switch to the existing terminal called Review","switch:REVIEW"),
    ("Switch to missing session Blue","clarify"),
    ("Open GitHub","favorite:0"), ("Open my Docs favorite","favorite:1"),
    ("Don't open the browser","noop"),
    ("Start Codex, please","launch:codex"),
    ("Launch a fresh Claude Code terminal","launch:claude"),
    ("Keep the current session","noop"),
]

HELD_OUT = [
    ("Please open Codex.", "launch:codex"), ("Could you launch Claude Code?", "launch:claude"),
    ("Open a new Pi session in Navi", "launch:pi"), ("Start a terminal called Notes", "launch:terminal"),
    ("Create Codex named Release", "launch:codex"), ("Open Claude in Navi, please.", "launch:claude"),
    ("Resume Design", "switch:DESIGN"), ("Select the Review session", "switch:REVIEW"),
    ("Return to Design", "switch:DESIGN"), ("Focus the existing Review session", "switch:REVIEW"),
    ("Launch Zen please", "browser"), ("Start the browser", "browser"),
    ("Please open GitHub", "favorite:0"), ("Open Docs in my browser", "favorite:1"),
    ("Never launch Pi", "noop"), ("Don't switch to Design", "noop"),
    ("Stop opening terminals", "noop"), ("Do not open GitHub", "noop"),
    ("Cancel opening Zen", "noop"), ("Keep current terminal", "noop"),
    ("Open Codex in Atlantis", "clarify|noop"), ("Switch to the Blue session", "clarify|noop"),
    ("Switch to Design and Review", "clarify|noop"), ("Open a picture", "clarify|noop"),
    ("Open Cloud Code", "clarify|noop"), ("Open the secret session", "clarify|noop"),
    ("Delete Review", "clarify|noop"), ("Please explain Codex", "clarify|noop"),
    ("No need to launch Claude", "noop|clarify"), ("Maybe later we can open Codex", "noop|clarify"),
    ("I opened Claude yesterday", "noop|clarify"), ("What does switch to Review mean?", "noop|clarify"),
]


def call(runtime,op,params=None):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(90);connection.connect(str(runtime/"station.sock"))
        connection.sendall((json.dumps({"op":op,"params":params or {}})+"\n").encode())
        return json.loads(connection.makefile("rb").readline())


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--held-out", action="store_true")
    args = parser.parse_args()
    os.nice(10)
    with IsolatedStation() as station:
        directory=station.folder;runtime=station.runtime;data=station.data
        try:
            for _ in range(100):
                try:status=call(runtime,"status")["result"];break
                except OSError:time.sleep(.02)
            settings=status["settings"]
            settings["model"].update({"weights":str(DATA/"models/d1-3B/d1-3B-Q4_K_M.gguf"),"projection":"","server":str(DATA/"runtime/llama-b11540/llama-b11540/llama-server"),"endpoint":f"http://127.0.0.1:{free_port()}/v1/systemone","gpu_layers":0,"threads":2})
            settings["favorites"]=[{"name":"GitHub","url":"https://github.com"},{"name":"Docs","url":"https://docs.rs"}]
            call(runtime,"settings",settings)
            replacements={}
            for name in ("Design","Review"):
                created=call(runtime,"create",{"profile":"terminal","name":name,"cwd":str(directory)})
                replacements[name.upper()]=created["result"]["id"]
            results=[]
            for text,expected in HELD_OUT if args.held_out else CASES:
                for key,value in replacements.items():expected=expected.replace(key,value)
                response=call(runtime,"decide",{"text":text})
                if not response["ok"]:
                    results.append({"text":text,"expected":expected,"error":response["error"],"pass":False});continue
                decision=response["result"]["trace"][0]
                selected=decision["selected"]
                results.append({"text":text,"expected":expected,"selected":selected,"pass":selected in expected.split("|"),"elapsed_ms":decision["elapsed_ms"],"answer":decision["raw"]["answers"]})
                print(text,"→",selected,decision["elapsed_ms"],"ms",flush=True)
            latency=sorted(row["elapsed_ms"] for row in results if "elapsed_ms" in row)
            report={"model":"d1-3B Q4_K_M text decisions","runtime":"llama.cpp b11540 CPU, two threads, separate endpoint","kind":"expanded command regression after request validation fixes" if args.held_out else "original command regression","cases":len(results),"correct":sum(row["pass"] for row in results),"accuracy":sum(row["pass"] for row in results)/len(results),"median_ms":statistics.median(latency) if latency else None,"p95_ms":latency[min(len(latency)-1,round(.95*len(latency)))] if latency else None,"results":results}
            (ROOT/("evidence/chief-heldout.json" if args.held_out else "evidence/chief-isolated.json")).write_text(json.dumps(report,indent=2)+"\n")
            print(json.dumps({key:value for key,value in report.items() if key!="results"},indent=2))
        finally:
            station.stop()


if __name__=="__main__":main()
