"""Real daemon/tmux lifecycle tests in isolated user-owned directories."""
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import unittest
from concurrent.futures import ThreadPoolExecutor
from http.server import ThreadingHTTPServer
import threading

from test_browser_live import Fixture

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("NAVI_TEST_BINARY", "/tmp/navi-cargo-target/debug/navid"))


class StationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="navi-test-")
        self.folder = Path(self.temporary.name)
        self.data = self.folder / "data"
        self.runtime = self.folder / "runtime"
        self.environment = {**os.environ, "NAVI_ISOLATED":"1", "NAVI_DATA_DIR":str(self.data), "NAVI_RUNTIME_DIR":str(self.runtime), "NAVI_APP_DIR":str(ROOT)}
        for key in ("NAVI_MANAGED_TERMINALS", "DISPLAY", "WAYLAND_DISPLAY", "DBUS_SESSION_BUS_ADDRESS", "XAUTHORITY"):
            self.environment.pop(key, None)
        self.start()

    def start(self):
        self.process = subprocess.Popen([str(BINARY), "serve"], env=self.environment, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(100):
            try:
                self.call("ping")
                return
            except OSError:
                time.sleep(.02)
        self.fail("Daemon did not start")

    def call(self, op, params=None):
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(5)
            connection.connect(str(self.runtime / "station.sock"))
            connection.sendall((json.dumps({"op":op,"params":params or {}})+"\n").encode())
            return json.loads(connection.makefile("rb").readline())

    def tearDown(self):
        self.process.terminate()
        self.process.wait(timeout=3)
        subprocess.run(["tmux","-S",str(self.data / "terminals.sock"),"kill-server"],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        for name in ("voice", "desktop"):
            path = self.runtime / f"{name}.pid"
            if path.exists():
                try:
                    os.kill(int(path.read_text()), 15)
                except ProcessLookupError:
                    pass
        self.temporary.cleanup()

    def test_session_process_survives_daemon_restart_and_switch(self):
        first=self.call("create",{"profile":"terminal","name":"first","cwd":str(self.folder)})
        second=self.call("create",{"profile":"terminal","name":"second","cwd":str(self.folder)})
        self.assertTrue(first["ok"], first)
        self.assertTrue(second["ok"], second)
        target=first["result"]["id"]
        before=subprocess.check_output(["tmux","-S",str(self.data / "terminals.sock"),"display-message","-p","-t",target,"#{pane_pid}"])
        self.assertTrue(self.call("select",{"id":target})["ok"])
        self.process.terminate();self.process.wait(timeout=3);self.start()
        after=subprocess.check_output(["tmux","-S",str(self.data / "terminals.sock"),"display-message","-p","-t",target,"#{pane_pid}"])
        self.assertEqual(before,after)
        status=self.call("status")["result"]
        self.assertEqual(status["settings"]["active_session"],target)
        self.assertTrue(all(item["running"] for item in status["sessions"]))

    def test_missing_executable_has_no_session_side_effect(self):
        settings=self.call("status")["result"]["settings"]
        settings["profiles"].append({"id":"missing","name":"Missing","executable":"/definitely/not/a/harness","args":[]})
        self.assertTrue(self.call("settings",settings)["ok"])
        self.assertFalse(self.call("create",{"profile":"missing","cwd":str(self.folder)})["ok"])
        self.assertEqual(self.call("status")["result"]["sessions"],[])

    def test_unregistered_session_and_remote_chief_are_rejected(self):
        self.assertFalse(self.call("select",{"id":"other-user-session"})["ok"])
        settings=self.call("status")["result"]["settings"]
        settings["model"]["endpoint"]="https://example.com/v1/systemone"
        self.assertFalse(self.call("settings",settings)["ok"])
        self.assertTrue(self.call("cancel")["ok"])
        self.assertFalse(self.call("command",{"text":"Open Codex"})["ok"])
        self.assertEqual(self.call("status")["result"]["sessions"],[])

    def test_isolated_mode_cannot_open_desktop_or_microphone(self):
        status = self.call("status")["result"]
        self.assertTrue(status["isolated"])
        self.assertFalse(self.call("desktop_start")["ok"])
        self.assertFalse(self.call("voice_start")["ok"])
        settings = status["settings"]
        settings["microphone"] = True
        self.assertFalse(self.call("settings", settings)["ok"])
        self.assertFalse((self.runtime / "desktop.pid").exists())
        self.assertFalse((self.runtime / "voice.pid").exists())

    def test_stop_interrupts_inference_and_releases_command_lock(self):
        server = ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
        server.daemon_threads = True
        threading.Thread(target=server.serve_forever, daemon=True).start()
        settings = self.call("status")["result"]["settings"]
        settings["model"]["endpoint"] = f"http://127.0.0.1:{server.server_port}/v1/systemone"
        self.assertTrue(self.call("settings", settings)["ok"])
        Fixture.delay = 3
        Fixture.inference_started.clear()
        try:
            with ThreadPoolExecutor(max_workers=1) as pool:
                pending = pool.submit(self.call, "command", {"text": "Open terminal"})
                self.assertTrue(Fixture.inference_started.wait(timeout=2))
                started = time.monotonic()
                self.assertTrue(self.call("cancel")["ok"])
                self.assertFalse(pending.result(timeout=1)["ok"])
                self.assertLess(time.monotonic() - started, .5)
            self.assertEqual(self.call("status")["result"]["sessions"], [])
            Fixture.delay = 0
            self.assertTrue(self.call("decide", {"text": "Open terminal"})["ok"])
        finally:
            Fixture.delay = 0
            server.shutdown()
            server.server_close()

    def test_native_window_choice_is_bound_to_handles_and_input_is_isolated(self):
        server = ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        settings = self.call("status")["result"]["settings"]
        settings["model"]["endpoint"] = f"http://127.0.0.1:{server.server_port}/v1/systemone"
        self.assertTrue(self.call("settings", settings)["ok"])
        self.assertTrue(self.call("desktop_state", {"status": "connected"})["ok"])
        window = "{00000000-0000-0000-0000-000000000001}"
        with socket.socket(socket.AF_UNIX) as worker:
            worker.bind(str(self.runtime / "desktop.sock"))
            worker.listen(2)
            requests = []
            def observe():
                for _ in range(2):
                    connection, _ = worker.accept()
                    with connection:
                        request = json.loads(connection.makefile("rb").readline())
                        requests.append(request)
                        connection.sendall((json.dumps({"ok": True, "windows": [{"id": window, "name": "Editor fixture", "app": "TestEditor"}]}) + "\n").encode())
            thread = threading.Thread(target=observe, daemon=True)
            thread.start()
            try:
                result = self.call("decide", {"text": "Focus TestEditor window"})
                self.assertTrue(result["ok"], result)
                self.assertEqual(result["result"]["trace"][0]["action"], {"kind": "desktop_action", "action": {"type": "window", "id": window}})
                executed = self.call("command", {"text": "Focus TestEditor window"})
                self.assertFalse(executed["ok"], executed)
                thread.join(timeout=2)
                self.assertEqual([request["type"] for request in requests], ["windows", "windows"])
            finally:
                server.shutdown()
                server.server_close()


if __name__ == "__main__":
    unittest.main()
