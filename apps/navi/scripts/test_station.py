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
        first=self.call("create",{"profile":"terminal","name":"first","cwd":str(self.folder),"allow_unregistered":True})
        second=self.call("create",{"profile":"terminal","name":"second","cwd":str(self.folder),"allow_unregistered":True})
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

    def test_permission_refusal_is_an_rpc_error(self):
        """BUG-006: a refusing gateway is a failure, not an ok:true no-op."""
        app = self.folder / "stub-app"
        (app / "workers").mkdir(parents=True)
        (app / "workers" / "opencode_gateway.py").write_text(
            "import json, sys\n"
            "args = sys.argv[1:]\n"
            "ident = args[args.index('--id') + 1] if '--id' in args else ''\n"
            "if ident.startswith('refuse'):\n"
            "    print(json.dumps({'replied': False, 'error': 'a gated tool may not run this'}))\n"
            "elif ident.startswith('silent'):\n"
            "    print(json.dumps({'replied': False}))\n"
            "else:\n"
            "    print(json.dumps({'replied': True, 'endpoint': '/api/permission/ok'}))\n")
        self.process.terminate(); self.process.wait(timeout=3)
        self.environment["NAVI_APP_DIR"] = str(app)
        self.start()
        refused = self.call("permission_reply", {"id": "refuse-1", "decision": "deny"})
        self.assertFalse(refused["ok"], refused)
        self.assertIn("a gated tool may not run this", refused["error"])
        silent = self.call("permission_reply", {"id": "silent-1", "decision": "deny"})
        self.assertFalse(silent["ok"], silent)
        self.assertIn("did not accept the permission decision", silent["error"])
        allowed = self.call("permission_reply", {"id": "allow-1", "decision": "allow"})
        self.assertTrue(allowed["ok"], allowed)
        self.assertTrue(allowed["result"]["replied"])

    def test_message_pages_walk_backwards_without_gaps(self):
        """BUG-047: the rowid cursor pages past the 500-row cap."""
        for index in range(3):
            saved = self.call("message_save", {
                "id": f"m{index}", "role": "user", "time": 1000 + index,
                "parts": [{"type": "text", "text": f"question {index}"}]})
            self.assertTrue(saved["ok"], saved)
        newest = self.call("messages", {"limit": 2})
        self.assertTrue(newest["ok"], newest)
        first = newest["result"]
        self.assertEqual([item["id"] for item in first["messages"]], ["m1", "m2"])
        self.assertTrue(first["has_more"])
        cursors = [item["cursor"] for item in first["messages"]]
        self.assertEqual(cursors, sorted(cursors))
        older = self.call("messages", {"limit": 2, "before": first["messages"][0]["cursor"]})
        self.assertTrue(older["ok"], older)
        self.assertEqual([item["id"] for item in older["result"]["messages"]], ["m0"])
        self.assertFalse(older["result"]["has_more"])
        empty = self.call("messages", {"limit": 2, "before": older["result"]["messages"][0]["cursor"]})
        self.assertEqual(empty["result"]["messages"], [])
        self.assertFalse(empty["result"]["has_more"])
        project = self.call("project_add", {"name": "Paged", "path": str(self.folder)})["result"]["project"]
        self.call("message_save", {"id": "p0", "role": "assistant", "time": 1003,
                                   "project_id": project["id"],
                                   "parts": [{"type": "text", "text": "scoped"}]})
        scoped = self.call("messages", {"project_id": project["id"], "limit": 2})["result"]
        self.assertEqual([item["id"] for item in scoped["messages"]], ["p0"])
        self.assertEqual(scoped["messages"][0]["project_id"], project["id"])
        self.assertFalse(scoped["has_more"])

    def test_voice_confirm_claims_atomically_and_cancel_withdraws(self):
        """BUG-056: exactly one side wins the claim that runs the transcript."""
        claim = self.runtime / "voice.claim"
        confirm = self.runtime / "voice.confirm"
        self.assertFalse(claim.exists())
        first = self.call("voice_confirm", {"accept": True})
        self.assertTrue(first["ok"], first)
        self.assertTrue(first["result"]["claimed"])
        self.assertTrue(claim.exists())
        second = self.call("voice_confirm", {"accept": True})
        self.assertTrue(second["ok"], second)
        self.assertFalse(second["result"]["claimed"])
        self.assertTrue(second["result"]["already"])
        confirm.write_text('{"accept": false}')
        cancelled = self.call("voice_confirm", {"accept": False})
        self.assertTrue(cancelled["ok"], cancelled)
        self.assertTrue(cancelled["result"]["cancelled"])
        self.assertEqual(json.loads(confirm.read_text()), {"accept": False})
        self.assertFalse(claim.exists())
        self.assertFalse(self.call("voice_confirm", {})["ok"])

    def test_transcribe_now_leaves_the_signal_for_the_voice_worker(self):
        signal = self.runtime / "voice.finish"
        self.assertFalse(signal.exists())
        reply = self.call("voice_finish")
        self.assertTrue(reply["ok"], reply)
        self.assertTrue(reply["result"]["finishing"])
        self.assertTrue(signal.exists())

    def test_unregistered_session_and_remote_chief_are_rejected(self):
        self.assertFalse(self.call("select",{"id":"other-user-session"})["ok"])
        settings=self.call("status")["result"]["settings"]
        settings["model"]["endpoint"]="https://example.com/v1/systemone"
        self.assertFalse(self.call("settings",settings)["ok"])
        self.assertTrue(self.call("cancel")["ok"])
        # Launches run without a model now, so ask for what still needs one: a desktop
        # action. It also keeps this test from starting the machine's real Codex.
        self.assertTrue(self.call("desktop_state", {"status": "connected"})["ok"])
        self.assertFalse(self.call("command",{"text":"Scroll down"})["ok"])
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

    def test_navi_defaults_and_settings_validation(self):
        settings = self.call("status")["result"]["settings"]
        self.assertTrue(settings["speak"])
        self.assertIn("opencode", [profile["id"] for profile in settings["profiles"]])
        self.assertEqual(settings["persona"], "navi")
        self.assertIn("Navi", settings["system_prompt"])
        self.assertNotIn("Jarvis", settings["system_prompt"])
        self.assertEqual(settings["assistant"]["agent"], "navi")
        self.assertIn("/", settings["assistant"]["model"])
        for field, value in [
            ("assistant", {"model": "nonsense", "agent": "navi"}),
            ("assistant", {"model": settings["assistant"]["model"], "agent": "bad agent!"}),
            ("system_prompt", "x" * 8001),
            ("system_prompt", "   "),
            ("speak", "yes"),
        ]:
            broken = json.loads(json.dumps(settings))
            broken[field] = value
            self.assertFalse(self.call("settings", broken)["ok"], field)

    def test_stop_interrupts_inference_and_releases_command_lock(self):
        server = ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
        server.daemon_threads = True
        threading.Thread(target=server.serve_forever, daemon=True).start()
        settings = self.call("status")["result"]["settings"]
        settings["model"]["endpoint"] = f"http://127.0.0.1:{server.server_port}/v1/systemone"
        self.assertTrue(self.call("settings", settings)["ok"])
        # A launch runs without a model now; a desktop action still asks one.
        self.assertTrue(self.call("desktop_state", {"status": "connected"})["ok"])
        Fixture.delay = 3
        Fixture.inference_started.clear()
        try:
            with ThreadPoolExecutor(max_workers=1) as pool:
                pending = pool.submit(self.call, "command", {"text": "Scroll down"})
                self.assertTrue(Fixture.inference_started.wait(timeout=2))
                started = time.monotonic()
                self.assertTrue(self.call("cancel")["ok"])
                self.assertFalse(pending.result(timeout=1)["ok"])
                self.assertLess(time.monotonic() - started, .5)
            self.assertEqual(self.call("status")["result"]["sessions"], [])
            Fixture.delay = 0
            self.assertTrue(self.call("decide", {"text": "Scroll down"})["ok"])
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


    def test_projects_graph_and_history_survive_restart(self):
        self.assertFalse(self.call("project_add", {"name": "Missing", "path": "/definitely/not/here"})["ok"])
        created = self.call("project_add", {"name": "Fixture", "path": str(self.folder)})
        self.assertTrue(created["ok"], created)
        project = created["result"]["project"]
        self.assertEqual(project["path_state"], "ok")
        self.assertEqual(project["name"], "Fixture")
        self.assertFalse(self.call("project_add", {"name": "Fixture 2", "path": str(self.folder)})["ok"])

        graph = self.call("graph_get", {"project_id": project["id"]})["result"]
        self.assertIsNone(graph["percent"])
        self.assertTrue(graph["percent_null"])
        self.assertEqual(graph["confidence"], 0.0)

        aspect = self.call("aspect_add", {"project_id": project["id"], "title": "Parsing"})["result"]
        task = self.call("task_add", {"project_id": project["id"], "aspect_id": aspect["id"], "title": "Handle EOF"})["result"]
        self.assertTrue(self.call("task_set", {"id": task["id"], "state": "done"})["ok"])
        graph = self.call("graph_get", {"project_id": project["id"]})["result"]
        self.assertEqual(graph["percent"], 100.0)
        self.assertEqual(graph["verified"], 100.0)
        self.assertEqual(graph["confidence"], 1.0)
        self.assertEqual(graph["aspects"][0]["percent"], 100.0)

        session = self.call("create", {"profile": "terminal", "name": "graph", "cwd": str(self.folder)})
        self.assertTrue(session["ok"], session)
        self.assertEqual(session["result"]["project_id"], project["id"])
        events = self.call("session_events", {"id": session["result"]["id"]})["result"]["events"]
        kinds = [event["kind"] for event in events]
        self.assertEqual(kinds[:2], ["created", "launch_argv"])
        self.assertIn("state", kinds)
        launch = next(event for event in events if event["kind"] == "launch_argv")
        self.assertEqual(launch["payload"]["harness"], "bash")
        self.assertEqual(launch["payload"]["cwd"], str(self.folder))

        first = self.call("status")["result"]
        self.assertIsInstance(first["boot_id"], str)
        self.assertGreater(first["events_from"], 0)
        history = self.call("history", {"project_id": project["id"], "limit": 50})["result"]
        self.assertTrue(any(event["kind"] in ("project", "launched", "session_state") for event in history["events"]))

        self.process.terminate(); self.process.wait(timeout=3); self.start()
        second = self.call("status")["result"]
        self.assertNotEqual(second["boot_id"], first["boot_id"])
        self.assertEqual(self.call("graph_get", {"project_id": project["id"]})["result"]["percent"], 100.0)
        history = self.call("history", {"project_id": project["id"], "limit": 50})["result"]
        self.assertTrue(any(event["kind"] == "launched" for event in history["events"]))


    def test_crashed_worker_is_restarted_with_backoff(self):
        script = self.folder / "fake_worker.py"
        script.write_text("import sys, time\ntime.sleep(1.2)\nsys.exit(3)\n")
        self.process.terminate(); self.process.wait(timeout=3)
        self.environment["NAVI_TEST_WORKER_SCRIPT"] = str(script)
        self.start()
        self.assertTrue(self.call("voice_start")["ok"])
        history = {"events": []}
        deadline = time.monotonic() + 6
        while time.monotonic() < deadline:
            history = self.call("history", {"limit": 100})["result"]
            if any(event["kind"] == "worker_restart" for event in history["events"]):
                break
            time.sleep(.25)
        self.assertTrue(any(event["kind"] == "worker_restart" for event in history["events"]), history)
        workers = self.call("status")["result"]["workers"]
        self.assertIn("voice", workers)


if __name__ == "__main__":
    unittest.main()
