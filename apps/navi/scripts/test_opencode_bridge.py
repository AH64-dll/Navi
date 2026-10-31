"""OpenCodeBridge checks against a fake in-process `opencode serve`."""
import base64
import json
import os
import re
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "workers"))
import opencode_bridge as bridge


class FakeOpenCode(BaseHTTPRequestHandler):
    """Minimal stand-in for `opencode serve` v2.2.x.

    Auth: Basic opencode:test-password. Every mutating call returns
    {"data": ...} like the real envelope. The assistant message is completed
    after `prompt_delay_s`, so the bridge's poll loop sees the same shape as
    the live server.
    """

    next_session = 0
    prompt_delay_s = 0.0
    requests = []
    lock = threading.Lock()

    def log_message(self, *unused):
        pass

    def _auth_ok(self):
        expected = "Basic " + base64.b64encode(b"opencode:test-password").decode()
        return self.headers.get("Authorization") == expected

    def _json(self, payload, code=200):
        body = json.dumps({"data": payload}).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _unauth(self):
        self.send_response(401)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_GET(self):
        if not self._auth_ok():
            return self._unauth()
        with self.lock:
            self.requests.append(("GET", self.path))
        if self.path == "/api/agent":
            return self._json([{"name": "default"}])
        m = re.match(r"^/api/session/(ses_\w+)$", self.path)
        if m:
            session_id = m.group(1)
            if session_id not in self.server.assistant_msgs:
                return self._json(None, code=404)
            return self._json({"id": session_id, "title": "Navi"})
        m = re.match(r"^/api/session/(ses_\w+)/message", self.path)
        if m:
            session_id = m.group(1)
            return self._json(self.server.assistant_msgs.get(session_id, []))
        return self._json(None, code=404)

    def do_POST(self):
        if not self._auth_ok():
            return self._unauth()
        length = int(self.headers.get("Content-Length") or 0)
        body = json.loads(self.rfile.read(length) or b"{}")
        with self.lock:
            self.requests.append(("POST", self.path, body))
        if self.path == "/api/session":
            with self.lock:
                FakeOpenCode.next_session += 1
                session_id = f"ses_{FakeOpenCode.next_session:04d}"
            self.server.assistant_msgs.setdefault(session_id, [])
            return self._json({"id": session_id, "title": body.get("title")})
        m = re.match(r"^/api/session/(ses_\w+)/model$", self.path)
        if m:
            if body.get("model", {}).get("providerID") in self.server.rejected:
                return self._json({"message": "model unavailable"}, code=400)
            self.server.models[m.group(1)] = body.get("model")
            return self._json({"ok": True})
        m = re.match(r"^/api/session/(ses_\w+)/prompt$", self.path)
        if m:
            session_id = m.group(1)
            self.server.last_prompt = (session_id, body)
            self.server.assistant_msgs.setdefault(session_id, [])
            # Queue an assistant reply that "completes" on the next poll.
            msg_id = f"msg_{len(self.server.assistant_msgs[session_id]) + 1:04d}"
            self.server.assistant_msgs[session_id].insert(0, {
                "id": msg_id,
                "type": "assistant",
                "time": {"completed": 12345},
                "content": [{"type": "text", "text": "ok"}],
            })
            return self._json({"id": "user-msg", "content": [{"type": "text", "text": body.get("text", "")}]})
        return self._json(None, code=404)


class BridgeTests(unittest.TestCase):
    def setUp(self):
        FakeOpenCode.next_session = 0
        FakeOpenCode.prompt_delay_s = 0.0
        FakeOpenCode.requests = []
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), FakeOpenCode)
        self.server.daemon_threads = True
        self.server.assistant_msgs = {}
        self.server.models = {}
        self.server.rejected = set()
        self.server.last_prompt = None
        self._tmp = tempfile.TemporaryDirectory()
        self.session_file = Path(self._tmp.name) / "opencode-session"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.url = f"http://127.0.0.1:{self.server.server_port}"
        self.bridge = bridge.OpenCodeBridge(
            base_url=self.url,
            password="test-password",
            autospawn=False,
            session_file=self.session_file,
        )

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self._tmp.cleanup()

    def test_auth_sent(self):
        self.bridge.ensure_up()
        self.assertTrue(any(r[0] == "GET" and r[1] == "/api/agent" for r in FakeOpenCode.requests))

    def test_rejects_wrong_password(self):
        wrong = bridge.OpenCodeBridge(base_url=self.url, password="wrong", autospawn=False)
        self.assertFalse(wrong.reachable())

    def test_session_and_model_setup(self):
        self.bridge.ensure_session()
        self.assertIsNotNone(self.bridge.session_id)
        session_id = self.bridge.session_id
        self.assertEqual(self.server.models[session_id]["providerID"], "opencode")
        self.assertEqual(self.server.models[session_id]["id"], "deepseek-v4.1-flash-free")

    def test_prompt_returns_assistant_text(self):
        out = self.bridge.prompt("say hi")
        self.assertEqual(out, "ok")
        session_id, prompt_body = self.server.last_prompt
        self.assertEqual(session_id, self.bridge.session_id)
        self.assertEqual(prompt_body["text"], "say hi")

    def test_system_prompt_prepended(self):
        self.bridge.prompt("hello", system_prompt="You are Jarvis.")
        _, body = self.server.last_prompt
        self.assertTrue(body["text"].startswith("You are Jarvis.\n\n"))

    def test_model_parsed_three_part(self):
        model = bridge._split_model("commandcode/deepseek/deepseek-v4.1-flash")
        self.assertEqual(model["providerID"], "commandcode")
        self.assertEqual(model["id"], "deepseek/deepseek-v4.1-flash")

    def test_model_rejects_bare_name(self):
        with self.assertRaises(bridge.BridgeError):
            bridge._split_model("novendor")

    def test_unreachable_without_autospawn(self):
        dead = bridge.OpenCodeBridge(base_url="http://127.0.0.1:1", password="x", autospawn=False)
        self.assertFalse(dead.reachable())
        with self.assertRaises(bridge.BridgeError):
            dead.ensure_up()

    def test_envelope_unwrap(self):
        self.bridge.ensure_up()
        agents = self.bridge._get("/api/agent")
        self.assertEqual(agents[0]["name"], "default")

    def test_session_id_persisted_for_next_process(self):
        self.bridge.ensure_session()
        session_id = self.bridge.session_id
        self.assertEqual(self.session_file.read_text(), session_id)
        again = bridge.OpenCodeBridge(
            base_url=self.url,
            password="test-password",
            autospawn=False,
            session_file=self.session_file,
        )
        again.ensure_session()
        self.assertEqual(again.session_id, session_id)

    def test_falls_back_to_commandcode_when_default_model_is_rejected(self):
        self.server.rejected = {"opencode"}
        self.bridge.ensure_session()
        self.assertEqual(self.bridge.model_ref, bridge.FALLBACK_MODEL)
        model = self.server.models[self.bridge.session_id]
        self.assertEqual(model, {"providerID": "commandcode", "id": "deepseek/deepseek-v4.1-flash"})

    def test_service_password_file(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "service.json"
            path.write_text(json.dumps({"password": "from-service"}))
            self.assertEqual(bridge.service_password(str(path)), "from-service")
            self.assertIsNone(bridge.service_password(str(Path(folder) / "missing")))

    def test_password_precedence_env_before_service_file(self):
        saved = {key: os.environ.get(key) for key in ("NAVI_OPENCODE_PASSWORD",)}
        try:
            os.environ["NAVI_OPENCODE_PASSWORD"] = "explicit"
            client = bridge.OpenCodeBridge(
                base_url=self.url, autospawn=False, session_file=self.session_file
            )
            self.assertEqual(client.password, "explicit")
        finally:
            for key, value in saved.items():
                if value is None:
                    os.environ.pop(key, None)
                else:
                    os.environ[key] = value


if __name__ == "__main__":
    unittest.main()
