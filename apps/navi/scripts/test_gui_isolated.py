"""Launch the real Copland GUI on a private X display and private D-Bus."""
import argparse
from http.server import ThreadingHTTPServer
import json
import os
from pathlib import Path
import subprocess
import threading

from isolated import IsolatedStation, ROOT
from test_browser_live import Fixture


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--workflow", action="store_true", help="Exercise settings, sessions, commands and Stop in the real GUI")
    args = parser.parse_args()
    os.nice(10)
    output = ROOT / ("evidence/gui-workflow.png" if args.workflow else "evidence/isolated-gui.png")
    with IsolatedStation() as station:
        created = station.call("create", {"profile": "terminal", "name": "Isolated validation", "cwd": str(station.folder)})
        assert created["ok"], created
        environment = {**station.environment, "GDK_BACKEND": "x11"}
        host = ROOT / "window.py"
        server = None
        try:
            if args.workflow:
                server = ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
                server.daemon_threads = True
                Fixture.delay = 2
                threading.Thread(target=server.serve_forever, daemon=True).start()
                settings = station.call("status")["result"]["settings"]
                settings["model"]["endpoint"] = f"http://127.0.0.1:{server.server_port}/v1/systemone"
                settings["profiles"].append({"id": "fixture-shell", "name": "Fixture shell", "executable": "/bin/bash", "args": []})
                settings["projects"] = [{"name": "Fixture", "path": str(station.folder)}]
                assert station.call("settings", settings)["ok"]
                environment["NAVI_GUI_REPORT"] = str(output)
                host = ROOT / "scripts/gui_workflow_host.py"
            else:
                environment["NAVI_QA_OUTPUT"] = str(output)
            result = subprocess.run(["xvfb-run", "-a", "--server-args=-screen 0 1440x920x24 -nolisten tcp", "dbus-run-session", "/usr/bin/python3", str(host)], env=environment, capture_output=True, text=True, timeout=45)
        finally:
            Fixture.delay = 0
            if server is not None:
                server.shutdown()
                server.server_close()
        if result.returncode:
            raise RuntimeError(result.stderr)
        workflow = None
        if args.workflow:
            workflow = json.loads(output.with_suffix(".workflow.json").read_text())
            assert workflow["ok"], workflow
        inspection = json.loads(output.with_suffix(".json").read_text())
        assert inspection["renderer"] == "webgl", inspection
        if not args.workflow:
            assert inspection["title"] == "Isolated validation", inspection
        assert all(session["running"] for session in station.call("status")["result"]["sessions"])
        report = {"private_display": True, "private_bus": True, "software_webgl": True, "gui_close_preserves_session": True, "inspection": inspection}
        if args.workflow:
            report["workflow"] = workflow
        (ROOT / ("evidence/gui-workflow.json" if args.workflow else "evidence/gui-isolation.json")).write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
