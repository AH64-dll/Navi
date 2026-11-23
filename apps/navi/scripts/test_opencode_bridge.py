#!/usr/bin/env python3
"""Tests for opencode_bridge.py: Unix socket RPCs, global SSE broadcast,
reconnect signal, and concurrent per-session turns."""
import json
import os
import socket
import tempfile
import threading
import time
import unittest
from http.server import ThreadingHTTPServer
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "workers"))
sys.path.insert(0, str(ROOT / "scripts"))

from opencode_bridge import BridgeServer  # noqa: E402
from opencode_gateway import Gateway  # noqa: E402
from test_opencode_gateway import FakeOpenCode, sse_event, turn_events  # noqa: E402


class BridgeTests(unittest.TestCase):
    def setUp(self):
        FakeOpenCode.requests = []
        FakeOpenCode.replies = {}
        FakeOpenCode.session_counter = 0
        FakeOpenCode.message_counter = 0
        FakeOpenCode.fail_interrupts = 0
        FakeOpenCode.reject_files = False
        FakeOpenCode.honor_directory = True
        FakeOpenCode.next_events = None
        FakeOpenCode.last_directory = None
        FakeOpenCode.contract_errors = []
        FakeOpenCode.pending_permissions = []
        FakeOpenCode.parents = {}
        self.opencode = ThreadingHTTPServer(("127.0.0.1", 0), FakeOpenCode)
        self.opencode.sse_condition = threading.Condition()
        self.opencode.sse_queues = []
        self.opencode.daemon_threads = True
        threading.Thread(target=self.opencode.serve_forever, daemon=True).start()

        self.tmp = tempfile.TemporaryDirectory(prefix="navi-bridge-")
        self.sock_path = os.path.join(self.tmp.name, "opencode.sock")
        self.session_file = os.path.join(self.tmp.name, "opencode-session")
        self.gw = Gateway(
            base_url=f"http://127.0.0.1:{self.opencode.server_port}",
            password="pw",
            autospawn=False,
            session_file=self.session_file,
        )
        self.bridge = BridgeServer(socket_path=self.sock_path, gateway=self.gw)
        self.thread = threading.Thread(target=self.bridge.serve_forever, daemon=True)
        self.thread.start()
        for _ in range(50):
            if os.path.exists(self.sock_path):
                break
            time.sleep(0.02)
        self.assertTrue(os.path.exists(self.sock_path))

    def tearDown(self):
        self.bridge.stop()
        self.opencode.shutdown()
        self.opencode.server_close()
        self.tmp.cleanup()

    def rpc(self, op, args=None):
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
            s.settimeout(10.0)
            s.connect(self.sock_path)
            s.sendall((json.dumps({"id": "1", "op": op, "args": args or {}}) + "\n").encode())
            f = s.makefile("r", encoding="utf-8")
            streams = []
            for raw in f:
                msg = json.loads(raw)
                if "stream" in msg:
                    streams.append(msg["stream"])
                elif msg.get("id") == "1":
                    return msg, streams
        raise AssertionError("socket closed without reply")

    def test_ping_and_catalog_over_socket(self):
        res, _ = self.rpc("ping")
        self.assertTrue(res["ok"], res)
        self.assertTrue(res["result"]["pong"])

        cat, _ = self.rpc("catalog", {"name": "agents"})
        self.assertTrue(cat["ok"], cat)
        self.assertEqual(cat["result"]["catalog"], "agents")
        self.assertIsInstance(cat["result"]["items"], list)

    def test_subscriber_receives_reconnect_and_unsolicited_events(self):
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sub:
            sub.settimeout(5.0)
            sub.connect(self.sock_path)
            sub.sendall((json.dumps({"id": "sub1", "op": "subscribe"}) + "\n").encode())
            f = sub.makefile("r", encoding="utf-8")
            ack = json.loads(f.readline())
            self.assertTrue(ack["ok"])

            # Simulate SSE reconnect + external session event
            self.bridge._on_sse_connect()
            self.bridge._on_sse_event("session.updated", {"sessionID": "ses_ext", "status": "running"})

            events = [json.loads(f.readline()), json.loads(f.readline())]
            names = [e.get("event") for e in events]
            self.assertIn("bridge.reconnected", names)
            self.assertIn("session.updated", names)

    def test_concurrent_turns_on_two_sessions_do_not_crosstalk(self):
        s1 = self.rpc("new", {"directory": os.path.join(self.tmp.name, "a")})[0]["result"]["session"]
        s2 = self.rpc("new", {"directory": os.path.join(self.tmp.name, "b")})[0]["result"]["session"]
        self.assertNotEqual(s1, s2)

        results = {}
        errors = []

        def run_turn(sid, text):
            try:
                res, streams = self.rpc("ask", {"session": sid, "text": text, "stream": True})
                results[sid] = (res, streams)
            except Exception as exc:
                errors.append(exc)

        t1 = threading.Thread(target=run_turn, args=(s1, "alpha"))
        t2 = threading.Thread(target=run_turn, args=(s2, "beta"))
        t1.start()
        t2.start()
        t1.join(timeout=10)
        t2.join(timeout=10)

        self.assertEqual(errors, [])
        self.assertTrue(results[s1][0]["ok"], results[s1][0])
        self.assertTrue(results[s2][0]["ok"], results[s2][0])
        self.assertIn("alpha", results[s1][0]["result"]["text"])
        self.assertIn("beta", results[s2][0]["result"]["text"])


if __name__ == "__main__":
    unittest.main()
