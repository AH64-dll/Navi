"""End-to-end assistant path: navid + gateway + FakeOpenCode (plan 01 bugs).

Covers BUG-001/002/004/005/006/007/014/016/018/088 against the real daemon in
isolated mode: events for tools/reasoning/usage, project directory + context
block, error-without-reply on a dead gateway, queueing, slash, permission
replies, and interrupt-before-kill on cancel.
"""
import base64
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import tempfile
import threading
import time
import unittest
from concurrent.futures import ThreadPoolExecutor
from http.server import ThreadingHTTPServer

from test_browser_live import Fixture
from test_opencode_gateway import FakeOpenCode, sse_event, turn_events

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("NAVI_TEST_BINARY", "/tmp/navi-cargo-target/debug/navid"))


def free_port():
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def push_events(server, events):
    with server.sse_condition:
        server.pending_events.extend(events)
        server.sse_condition.notify_all()


class AssistantTests(unittest.TestCase):
    def setUp(self):
        FakeOpenCode.requests = []
        FakeOpenCode.replies = {}
        FakeOpenCode.session_counter = 0
        FakeOpenCode.message_counter = 0
        FakeOpenCode.fail_interrupts = 0
        FakeOpenCode.reject_parts = False
        FakeOpenCode.honor_directory = True
        FakeOpenCode.next_events = None
        FakeOpenCode.last_directory = None
        self.opencode = ThreadingHTTPServer(("127.0.0.1", 0), FakeOpenCode)
        self.opencode.sse_condition = threading.Condition()
        self.opencode.pending_events = []
        self.opencode.daemon_threads = True
        threading.Thread(target=self.opencode.serve_forever, daemon=True).start()
        self.chief = ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
        self.chief.daemon_threads = True
        threading.Thread(target=self.chief.serve_forever, daemon=True).start()
        self.temporary = tempfile.TemporaryDirectory(prefix="navi-assistant-")
        self.folder = Path(self.temporary.name)
        self.data = self.folder / "data"
        self.runtime = self.folder / "runtime"
        self.environment = {
            **os.environ,
            "NAVI_ISOLATED": "1",
            "NAVI_DATA_DIR": str(self.data),
            "NAVI_RUNTIME_DIR": str(self.runtime),
            "NAVI_APP_DIR": str(ROOT),
            "NAVI_OPENCODE_URL": f"http://127.0.0.1:{self.opencode.server_port}",
            "NAVI_OPENCODE_PASSWORD": "pw",
            "NAVI_OPENCODE_AUTOSPAWN": "0",
        }
        for key in ("DISPLAY", "WAYLAND_DISPLAY", "DBUS_SESSION_BUS_ADDRESS", "XAUTHORITY"):
            self.environment.pop(key, None)
        self.process = subprocess.Popen(
            [str(BINARY), "serve"], env=self.environment,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(150):
            try:
                self.call("ping")
                break
            except OSError:
                time.sleep(.02)
        else:
            self.fail("Daemon did not start")
        settings = self.call("status")["result"]["settings"]
        settings["model"]["endpoint"] = (
            f"http://127.0.0.1:{self.chief.server_port}/v1/systemone")
        self.assertTrue(self.call("settings", settings)["ok"])

    def tearDown(self):
        self.process.terminate()
        self.process.wait(timeout=3)
        self.stop_tmux(self.data)
        self.opencode.shutdown()
        self.opencode.server_close()
        self.chief.shutdown()
        self.chief.server_close()
        self.temporary.cleanup()

    def stop_tmux(self, data):
        # The daemon leaves its panes running by design, and deleting the socket's
        # folder would strand the server with nothing able to reach it.
        subprocess.run(["tmux", "-S", str(data / "terminals.sock"), "kill-server"],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    def call(self, op, params=None, timeout=10):
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(timeout)
            connection.connect(str(self.runtime / "station.sock"))
            connection.sendall(
                (json.dumps({"op": op, "params": params or {}}) + "\n").encode())
            return json.loads(connection.makefile("rb").readline())

    def events(self, since=0, timeout=10):
        deadline = time.monotonic() + timeout
        while True:
            status = self.call("status", {"since": since})["result"]
            yield status["events"]
            if time.monotonic() > deadline:
                return
            time.sleep(.1)

    def collect(self, want=lambda e: True, count=None, timeout=15):
        deadline = time.monotonic() + timeout
        found = []
        since = 0
        while time.monotonic() < deadline:
            status = self.call("status", {"since": since})["result"]
            for event in status["events"]:
                since = max(since, event["seq"])
                if want(event):
                    found.append(event)
            if count is not None and len(found) >= count:
                return found
            if count is None and found:
                return found
            time.sleep(.1)
        return found

    def posts(self, suffix):
        return [entry for entry in FakeOpenCode.requests
                if entry["path"].endswith(suffix)]

    def project(self):
        projects = self.call("projects")["result"]["projects"]
        self.assertEqual(len(projects), 1)
        return projects[0]

    # BUG-001/002/004/005/009/010/088: one full turn streams everything.
    def test_turn_streams_tools_reasoning_usage_and_project_context(self):
        note = self.folder / "note.txt"
        note.write_text("attach me")
        result = self.call("command", {
            "text": "what is two plus two",
            "attachments": [{"path": str(note), "name": "note.txt"}],
        }, timeout=30)
        self.assertTrue(result["ok"], result)
        reply = result["result"]["reply"]
        self.assertIn("echo:", reply["text"])
        self.assertEqual(reply["usage"]["cost"], 0.002)
        self.assertEqual(reply["session_usage"]["totalTokens"], 17)
        self.assertEqual(reply["finish"], "stop")
        self.assertEqual(reply["actual_model"]["id"], "deepseek/deepseek-v4.1-flash")
        self.assertEqual(reply["actual_agent"], "navi")
        self.assertTrue(reply["session"].startswith("ses_"))
        self.assertEqual(reply["project_id"], self.project()["id"])

        # BUG-001/002: event stream carries reasoning, thinking only from real
        # reasoning, and exactly one final tool_use row per call id.
        events = self.collect(lambda e: e["kind"] == "reply", count=1)
        all_events = self.call("status")["result"]["events"]
        kinds = [event["kind"] for event in all_events]
        self.assertIn("working", kinds)
        self.assertIn("thinking", kinds)
        self.assertIn("reasoning", kinds)
        self.assertLess(kinds.index("working"), kinds.index("thinking"))
        tools = [e for e in all_events if e["kind"] == "tool_use"]
        running = [e for e in tools if e["details"].get("status") == "running"]
        finals = [e for e in tools if e["details"].get("final")]
        self.assertTrue(running)
        self.assertEqual(len(finals), 1)
        self.assertEqual(finals[0]["details"]["id"], "call_1")
        self.assertEqual(finals[0]["details"]["name"], "shell")
        self.assertEqual(finals[0]["details"]["status"], "completed")
        self.assertEqual(finals[0]["details"]["exit"], 0)
        reply_events = [e for e in all_events if e["kind"] == "reply"]
        self.assertEqual(reply_events[0]["details"]["usage"]["cost"], 0.002)
        self.assertEqual(reply_events[0]["details"]["actual_agent"], "navi")
        self.assertEqual(reply_events[0]["project_id"], self.project()["id"])

        # BUG-004/088: the session carries the project directory and the prompt
        # leads with the ≤600-byte project context block.
        created = self.posts("/api/session")
        self.assertEqual(created[-1]["body"]["directory"], self.project()["path"])
        prompt = self.posts("/prompt")[-1]["body"]
        self.assertTrue(prompt["text"].startswith("[project Navi"),
                        prompt["text"][:200])
        self.assertLessEqual(len(prompt["text"].split("\n\n")[0].encode()), 600)
        self.assertIn("open tasks:", prompt["text"])
        # BUG-005: the attachment rode the wire as a parts entry.
        self.assertEqual(prompt["parts"][0]["type"], "text")
        self.assertEqual(prompt["parts"][1]["type"], "file")
        self.assertEqual(prompt["parts"][1]["text"], "attach me")

        # BUG-088: the turn is remembered per project.
        db = sqlite3.connect(self.data / "station.sqlite3")
        rows = db.execute(
            "SELECT project_id, session_id, model, agent FROM assistant_sessions"
        ).fetchall()
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0][0], self.project()["id"])
        self.assertTrue(rows[0][1].startswith("ses_"))

    # BUG-014: a dead gateway is an error, never a reply.
    def test_dead_gateway_errors_without_a_reply(self):
        port = free_port()
        dead = self.folder / "dead"
        env = dict(self.environment,
                   NAVI_DATA_DIR=str(dead / "data"),
                   NAVI_RUNTIME_DIR=str(dead / "run"),
                   NAVI_OPENCODE_URL=f"http://127.0.0.1:{port}")
        process = subprocess.Popen([str(BINARY), "serve"], env=env,
                                   stdout=subprocess.DEVNULL,
                                   stderr=subprocess.DEVNULL)
        try:
            for _ in range(150):
                try:
                    with socket.socket(socket.AF_UNIX) as c:
                        c.settimeout(2)
                        c.connect(str(dead / "run" / "station.sock"))
                        break
                except OSError:
                    time.sleep(.02)
            else:
                self.fail("dead-gateway daemon did not start")

            def call(op, params=None, timeout=10):
                with socket.socket(socket.AF_UNIX) as c:
                    c.settimeout(timeout)
                    c.connect(str(dead / "run" / "station.sock"))
                    c.sendall((json.dumps({"op": op, "params": params or {}}) + "\n").encode())
                    return json.loads(c.makefile("rb").readline())

            settings = call("status")["result"]["settings"]
            settings["model"]["endpoint"] = (
                f"http://127.0.0.1:{self.chief.server_port}/v1/systemone")
            self.assertTrue(call("settings", settings)["ok"])
            # The clock answers the time locally, so ask what only the assistant can.
            result = call("command", {"text": "why is the sky blue"}, timeout=20)
            self.assertFalse(result["ok"], result)
            self.assertIn("error", result)
            status = call("status")["result"]
            self.assertFalse(any(e["kind"] == "reply" for e in status["events"]),
                             "a failure must never emit a reply event")
            self.assertTrue(any(e["kind"] == "error" for e in status["events"]))
        finally:
            process.terminate()
            process.wait(timeout=3)
            self.stop_tmux(dead / "data")

    # BUG-016/018: a second command queues while the first runs, then drains.
    def test_second_command_queues_and_runs(self):
        session = "ses_fake1"
        pre = [
            sse_event("session.step.started", session),
            sse_event("session.step.streamed", session),
            sse_event("session.text.started", session),
            sse_event("session.text.delta", session, delta="echo: first"),
            sse_event("session.text.ended", session, text="echo: first"),
        ]
        post = [
            sse_event("session.step.ended", session,
                      tokens={"input": 1, "output": 1}, cost=0.001,
                      finish="stop"),
            sse_event("session.execution.succeeded", session),
        ]
        FakeOpenCode.next_events = pre
        with ThreadPoolExecutor(max_workers=1) as pool:
            first = pool.submit(self.call, "command",
                                {"text": "first question"}, 30)
            for _ in range(100):
                if self.posts("/prompt"):
                    break
                time.sleep(.05)
            self.assertTrue(self.posts("/prompt"), "first prompt never posted")
            queued = self.call("command", {"text": "second question"})
            self.assertTrue(queued["ok"], queued)
            self.assertTrue(queued["result"]["queued"], queued)
            self.assertEqual(queued["result"]["position"], 1)
            push_events(self.opencode, post)
            done = first.result(timeout=20)
            self.assertTrue(done["ok"], done)
            self.assertIn("echo: first", done["result"]["reply"]["text"])
        # The drain thread ran the queued item: a second prompt posted and a
        # reply event for it exists.
        deadline = time.monotonic() + 15
        while len(self.posts("/prompt")) < 2 and time.monotonic() < deadline:
            time.sleep(.1)
        self.assertEqual(len(self.posts("/prompt")), 2)
        replies = self.collect(
            lambda e: e["kind"] == "reply"
            and "second" in (e["details"].get("text") or ""), timeout=15)
        self.assertTrue(replies, "queued command's reply never arrived")

    # BUG-019/006: slash posts the real command; permission replies post back.
    def test_slash_runs_through_gateway_and_permission_replies(self):
        result = self.call("command", {"text": "/review me"}, timeout=30)
        self.assertTrue(result["ok"], result)
        self.assertIn("ran review me", result["result"]["reply"]["text"])
        self.assertEqual(self.posts("/command")[-1]["body"],
                         {"name": "review", "text": "me"})
        reply = self.call("permission_reply",
                          {"id": "perm_1", "decision": "allow"})
        self.assertTrue(reply["ok"], reply)
        self.assertTrue(reply["result"]["replied"])
        permissions = [e for e in FakeOpenCode.requests
                       if "/permission" in e["path"]]
        self.assertEqual(len(permissions), 1)
        self.assertEqual(permissions[0]["body"]["decision"], "allow")
        self.assertIn("/permission", permissions[0]["path"])
        deny = self.call("permission_reply", {"id": "perm_2", "decision": "deny"})
        self.assertTrue(deny["result"]["replied"])
        bad = self.call("permission_reply", {"id": "p", "decision": "maybe"})
        self.assertFalse(bad["ok"])

    # BUG-007: cancel interrupts the server turn before the child dies.
    def test_cancel_interrupts_the_server_turn(self):
        session = "ses_fake1"
        FakeOpenCode.next_events = [
            sse_event("session.step.started", session),
            sse_event("session.step.streamed", session),
        ]
        with ThreadPoolExecutor(max_workers=1) as pool:
            pending = pool.submit(self.call, "command",
                                  {"text": "a hung turn"}, 30)
            for _ in range(100):
                if self.posts("/prompt"):
                    break
                time.sleep(.05)
            self.assertTrue(self.posts("/prompt"))
            time.sleep(.3)
            stop = self.call("cancel")
            self.assertTrue(stop["ok"], stop)
            self.assertTrue(stop["result"]["interrupted"])
            result = pending.result(timeout=20)
            self.assertTrue(result["ok"], result)
            reply = result["result"]["reply"]
            self.assertEqual(reply["source"], "stopped")
            self.assertTrue(reply["interrupted"])
        interrupts = [e for e in FakeOpenCode.requests
                      if e["path"].endswith("/interrupt")]
        self.assertGreaterEqual(len(interrupts), 1,
                                "no interrupt POST reached OpenCode")


if __name__ == "__main__":
    unittest.main()
