"""Private local RPC shared by native audio, capture, and desktop presentation."""
import json
import os
from pathlib import Path
import socket

DATA = Path(os.environ.get("NAVI_DATA_DIR", Path.home() / ".local/share/navi"))
RUNTIME = Path(os.environ.get("NAVI_RUNTIME_DIR", Path(os.environ.get("XDG_RUNTIME_DIR", f"/tmp/navi-{os.getuid()}")) / "navi-station"))


# A command runs a whole assistant turn. The daemon ends it at 30 minutes, so a caller
# that gives up sooner reports a failure while the assistant is still working.
COMMAND_TIMEOUT = 1830


def rpc(op, params=None, timeout=90):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(timeout)
        connection.connect(str(RUNTIME / "station.sock"))
        connection.sendall((json.dumps({"op": op, "params": params or {}}) + "\n").encode())
        with connection.makefile("rb") as stream:
            result = json.loads(stream.readline(4 * 1024 * 1024))
        if not result["ok"]:
            raise RuntimeError(result["error"])
        return result["result"]
