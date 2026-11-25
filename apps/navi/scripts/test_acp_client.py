#!/usr/bin/env python3
"""ACP client against a fake agent (no harness installed, no network, no paid calls).

The fake agent is a real subprocess speaking JSON-RPC 2.0 over stdin/stdout, the
same wire `opencode acp` or `@agentclientprotocol/claude-agent-acp` uses. Every
message it receives is appended to a JSONL log the test reads back, so assertions
are about what actually crossed the pipe, not about the client's bookkeeping.

Run: NAVI_ISOLATED=1 ~/.local/share/navi/venv/bin/python scripts/test_acp_client.py
"""
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import threading
import time
import unittest

WORKERS = Path(__file__).resolve().parents[1] / "workers"
sys.path.insert(0, str(WORKERS))
from acp_client import AcpClient  # noqa: E402

# Scenarios are selected by the prompt text so one agent covers every case:
#   "stream"          -> thought chunk, two message chunks, tool_call + completion
#   "read:<path>"     -> fs/read_text_file request, reply logged
#   "write:<path>:<c>"-> fs/write_text_file request, reply logged
#   "permission"      -> session/request_permission with allow/always/reject options
FAKE_AGENT = r'''
import json
import sys
import threading

LOG = sys.argv[1]
_lock = threading.Lock()
_pending = {}
_pending_lock = threading.Lock()
_next_id = [100]


def log(entry):
    with _lock:
        with open(LOG, "a", encoding="utf-8") as handle:
            handle.write(json.dumps(entry) + "\n")
            handle.flush()


def send(msg):
    with _lock:
        sys.stdout.write(json.dumps(msg) + "\n")
        sys.stdout.flush()


def request(method, params, timeout=20.0):
    _next_id[0] += 1
    rid = _next_id[0]
    event = threading.Event()
    box = {}
    with _pending_lock:
        _pending[rid] = (event, box)
    send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
    event.wait(timeout)
    return box.get("msg")


def update(session, payload):
    send({"jsonrpc": "2.0", "method": "session/update",
          "params": {"sessionId": session, "update": payload}})


def stream(session):
    update(session, {"sessionUpdate": "agent_thought_chunk",
                     "content": {"type": "text", "text": "planning the answer"}})
    update(session, {"sessionUpdate": "agent_message_chunk",
                     "content": {"type": "text", "text": "hello "}})
    update(session, {"sessionUpdate": "tool_call", "toolCallId": "call_1",
                     "title": "Read file", "kind": "read", "status": "pending",
                     "rawInput": {"path": "inside.txt"}})
    update(session, {"sessionUpdate": "tool_call_update", "toolCallId": "call_1",
                     "status": "completed", "rawOutput": "file body"})
    update(session, {"sessionUpdate": "agent_message_chunk",
                     "content": {"type": "text", "text": "world"}})


def handle_prompt(rid, session, text):
    if text.startswith("read:"):
        log({"dir": "out", "msg": {"method": "fs/read_text_file", "path": text[5:]}})
        log({"dir": "reply", "msg": request("fs/read_text_file", {"path": text[5:]})})
    elif text.startswith("write:"):
        _, path, content = text.split(":", 2)
        log({"dir": "out", "msg": {"method": "fs/write_text_file", "path": path}})
        log({"dir": "reply", "msg": request("fs/write_text_file",
                                            {"path": path, "content": content})})
    elif text == "permission":
        log({"dir": "reply", "msg": request("session/request_permission", {
            "sessionId": session,
            "toolCall": {"toolCallId": "tc_perm", "title": "Run shell",
                         "kind": "execute", "rawInput": {"command": "ls"}},
            "options": [
                {"optionId": "allow-once", "name": "Allow once", "kind": "allow_once"},
                {"optionId": "always", "name": "Always", "kind": "allow_always"},
                {"optionId": "reject-once", "name": "Reject", "kind": "reject_once"},
            ],
        })})
    else:
        stream(session)
    send({"jsonrpc": "2.0", "id": rid, "result": {"stopReason": "end_turn"}})


def main():
    for raw in sys.stdin:
        line = raw.strip()
        if not line:
            continue
        msg = json.loads(line)
        log({"dir": "in", "msg": msg})
        method = msg.get("method")
        if method and "id" in msg:
            params = msg.get("params") or {}
            if method == "initialize":
                send({"jsonrpc": "2.0", "id": msg["id"],
                      "result": {"protocolVersion": 1,
                                 "agentCapabilities": {"loadSession": False}}})
            elif method == "session/new":
                send({"jsonrpc": "2.0", "id": msg["id"],
                      "result": {"sessionId": "sess_fake_1"}})
            elif method == "session/prompt":
                text = "".join(part.get("text", "") for part in params.get("prompt") or []
                               if isinstance(part, dict))
                threading.Thread(target=handle_prompt,
                                 args=(msg["id"], params.get("sessionId"), text),
                                 daemon=True).start()
            else:
                send({"jsonrpc": "2.0", "id": msg["id"],
                      "error": {"code": -32601, "message": "unsupported " + method}})
        elif method:
            pass  # notifications (session/cancel) are only logged
        else:
            with _pending_lock:
                pair = _pending.pop(msg.get("id"), None)
            if pair:
                event, box = pair
                box["msg"] = msg
                event.set()


main()
'''


class AcpClientTest(unittest.TestCase):
    def setUp(self):
        self.root = Path(os.path.realpath(tempfile.mkdtemp(prefix="navi-acp-")))
        self.session_dir = self.root / "session"
        self.session_dir.mkdir()
        self.agent = self.root / "fake_agent.py"
        self.agent.write_text(FAKE_AGENT, encoding="utf-8")
        self.log_path = self.root / "agent.log"
        self.records = []
        self.client = AcpClient([sys.executable, str(self.agent), str(self.log_path)],
                                str(self.session_dir), emit=self.records.append)

    def tearDown(self):
        self.client.stop()
        # AcpClient.stop() ends the process but leaves its pipes to the GC; close
        # them here so a suite run does not end in ResourceWarnings.
        proc = self.client.proc
        if proc is not None:
            for stream in (proc.stdin, proc.stdout, proc.stderr):
                if stream is not None:
                    try:
                        stream.close()
                    except (OSError, ValueError):
                        pass
        shutil.rmtree(self.root, ignore_errors=True)

    # -- helpers ---------------------------------------------------------
    def start(self):
        self.client.start()
        self.client.initialize()
        self.assertEqual(self.client.new_session(), "sess_fake_1")

    def entries(self):
        if not self.log_path.exists():
            return []
        text = self.log_path.read_text(encoding="utf-8")
        return [json.loads(line) for line in text.splitlines() if line.strip()]

    def wait_for(self, predicate, what, timeout=10.0):
        deadline = time.time() + timeout
        while time.time() < deadline:
            for entry in self.entries():
                if predicate(entry):
                    return entry
            time.sleep(0.02)
        self.fail(f"timed out waiting for {what}; log={self.entries()}")

    def wait_for_record(self, predicate, what, timeout=10.0):
        deadline = time.time() + timeout
        while time.time() < deadline:
            for record in list(self.records):
                if predicate(record):
                    return record
            time.sleep(0.02)
        self.fail(f"timed out waiting for {what}; records={self.records}")

    def records_of(self, kind):
        return [record for record in self.records if record.get("type") == kind]

    # -- 1. handshake and streaming --------------------------------------
    def test_initialize_and_session_new(self):
        self.client.start()
        info = self.client.initialize()
        self.assertEqual(info["protocolVersion"], 1)
        self.assertEqual(self.client.new_session(), "sess_fake_1")
        self.assertEqual(self.client.session_id, "sess_fake_1")

        init = self.wait_for(lambda e: e["dir"] == "in" and e["msg"].get("method") == "initialize",
                             "initialize request")
        params = init["msg"]["params"]
        self.assertEqual(params["protocolVersion"], 1)
        self.assertEqual(params["clientCapabilities"]["fs"],
                         {"readTextFile": True, "writeTextFile": True})
        self.assertFalse(params["clientCapabilities"]["terminal"])
        self.assertEqual(params["clientInfo"]["name"], "navi")

        new = self.wait_for(lambda e: e["dir"] == "in" and e["msg"].get("method") == "session/new",
                            "session/new request")
        self.assertEqual(new["msg"]["params"]["cwd"], str(self.session_dir))
        self.assertEqual(new["msg"]["params"]["mcpServers"], [])

    def test_prompt_streams_normalized_records(self):
        self.start()
        result = self.client.prompt("stream")
        self.assertEqual(result.get("stopReason"), "end_turn")

        self.assertEqual([r["text"] for r in self.records_of("reasoning")],
                         ["planning the answer"])
        self.assertEqual([r["text"] for r in self.records_of("delta")], ["hello ", "world"])

        status = self.records_of("tool_status")
        self.assertEqual(len(status), 1, status)
        self.assertEqual(status[0]["id"], "call_1")
        self.assertEqual(status[0]["name"], "Read file")
        self.assertEqual(status[0]["status"], "pending")
        self.assertEqual(status[0]["input"], {"path": "inside.txt"})

        tool = self.records_of("tool")
        self.assertEqual(len(tool), 1, tool)
        self.assertIs(tool[0]["final"], True)
        self.assertEqual(tool[0]["id"], "call_1")
        self.assertEqual(tool[0]["name"], "Read file")
        self.assertEqual(tool[0]["status"], "completed")
        self.assertEqual(tool[0]["output"], "file body")
        self.assertEqual(tool[0]["input"], {"path": "inside.txt"})

        prompt = self.wait_for(lambda e: e["dir"] == "in" and e["msg"].get("method") == "session/prompt",
                               "session/prompt request")
        self.assertEqual(prompt["msg"]["params"]["sessionId"], "sess_fake_1")
        self.assertEqual(prompt["msg"]["params"]["prompt"], [{"type": "text", "text": "stream"}])

    # -- 2. fs requests stay inside cwd ----------------------------------
    def test_fs_read_text_file_inside_cwd(self):
        (self.session_dir / "inside.txt").write_text("line one\nline two\n", encoding="utf-8")
        self.start()
        self.client.prompt("read:inside.txt")
        reply = self.wait_for(lambda e: e["dir"] == "reply" and "result" in e["msg"],
                              "fs/read_text_file reply")
        self.assertEqual(reply["msg"]["result"], {"content": "line one\nline two\n"})

    def test_fs_write_text_file_inside_cwd(self):
        self.start()
        self.client.prompt("write:notes/out.txt:hello from the agent")
        reply = self.wait_for(lambda e: e["dir"] == "reply" and "result" in e["msg"],
                              "fs/write_text_file reply")
        self.assertEqual(reply["msg"]["result"], {})
        self.assertEqual((self.session_dir / "notes" / "out.txt").read_text(encoding="utf-8"),
                         "hello from the agent")

    def test_fs_read_outside_cwd_is_rejected(self):
        (self.root / "escape.txt").write_text("secret\n", encoding="utf-8")
        self.start()
        self.client.prompt("read:../escape.txt")
        reply = self.wait_for(lambda e: e["dir"] == "reply" and "error" in e["msg"],
                              "fs/read_text_file rejection")
        self.assertEqual(reply["msg"]["error"]["code"], -32602)
        self.assertIn("outside session cwd", reply["msg"]["error"]["message"])
        self.assertNotIn("result", reply["msg"])

    def test_fs_write_outside_cwd_is_rejected(self):
        self.start()
        self.client.prompt("write:../escape.txt:leak")
        reply = self.wait_for(lambda e: e["dir"] == "reply" and "error" in e["msg"],
                              "fs/write_text_file rejection")
        self.assertEqual(reply["msg"]["error"]["code"], -32602)
        self.assertIn("outside session cwd", reply["msg"]["error"]["message"])
        self.assertFalse((self.root / "escape.txt").exists())

    # -- 3. permission round trip ----------------------------------------
    def ask_permission(self, decision):
        self.start()
        box = {}
        thread = threading.Thread(target=lambda: box.update(self.client.prompt("permission")))
        thread.start()
        try:
            record = self.wait_for_record(lambda r: r.get("type") == "permission",
                                          "permission record")
            self.assertTrue(record["id"].startswith("acp-perm-"))
            self.assertEqual(record["session"], "sess_fake_1")
            self.assertEqual(record["tool"], "Run shell")
            self.assertEqual(record["input"]["rawInput"], {"command": "ls"})
            self.assertEqual([o["optionId"] for o in record["input"]["options"]],
                             ["allow-once", "always", "reject-once"])
            self.client.reply_permission(record["id"], decision)
            thread.join(10)
            self.assertFalse(thread.is_alive(), "session/prompt never completed")
            self.assertEqual(box["stopReason"], "end_turn")
        finally:
            thread.join(0)
        reply = self.wait_for(lambda e: e["dir"] == "reply" and "result" in e["msg"],
                              "session/request_permission reply")
        return reply["msg"]["result"]["outcome"]

    def test_permission_allow_replies_selected_option(self):
        outcome = self.ask_permission("allow")
        self.assertEqual(outcome, {"outcome": "selected", "optionId": "allow-once"})

    def test_permission_deny_replies_reject_option(self):
        outcome = self.ask_permission("deny")
        self.assertEqual(outcome, {"outcome": "selected", "optionId": "reject-once"})

    # -- 4. cancel --------------------------------------------------------
    def test_cancel_sends_session_cancel_notification(self):
        self.start()
        self.client.cancel()
        entry = self.wait_for(
            lambda e: e["dir"] == "in" and e["msg"].get("method") == "session/cancel",
            "session/cancel notification")
        self.assertNotIn("id", entry["msg"])
        self.assertEqual(entry["msg"]["params"], {"sessionId": "sess_fake_1"})

    def test_cancel_without_session_sends_nothing(self):
        self.client.start()
        self.client.initialize()
        self.client.cancel()
        time.sleep(0.2)
        self.assertFalse([e for e in self.entries()
                          if e["msg"].get("method") == "session/cancel"])


if __name__ == "__main__":
    unittest.main()
