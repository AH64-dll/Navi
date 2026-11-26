"""ACP permission scope regressions, no subprocess or live model required."""
import importlib.util
from pathlib import Path
import threading
import unittest

spec = importlib.util.spec_from_file_location("navi_acp", Path(__file__).resolve().parents[1] / "workers/acp_client.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class PermissionSafetyTests(unittest.TestCase):
    def run_permission(self, decision, options):
        replies = []
        client = module.AcpClient([], ".")
        client._reply = lambda ident, result=None, error=None: replies.append((ident, result, error))
        client.emit = lambda record: client.reply_permission(record["id"], decision)
        client._handle_agent_request(1, "session/request_permission", {"toolCall": {"toolCallId": "tool"}, "options": options})
        self.assertEqual(len(replies), 1)
        self.assertEqual(client._perm_events, {})
        self.assertEqual(client._perm_answers, {})
        return replies[0][1]["outcome"]

    def test_ack_is_after_agent_reply_write_and_failure_is_not_success(self):
        for sent in (True, False):
            client = module.AcpClient([], ".")
            order = []
            client.emit = lambda record: (order.append("permission"), client.reply_permission(record["id"], "allow"))
            def reply(*args, **kwargs):
                order.append("write")
                return sent
            client._reply = reply
            client.permission_ack = lambda value: order.append(("ack", value["replied"]))
            client._handle_agent_request(1, "session/request_permission", {"options": [{"optionId": "once", "kind": "allow_once"}]})
            self.assertEqual(order, ["permission", "write", ("ack", sent)])

    def test_timeout_closes_reply_window_before_writing_cancellation(self):
        from unittest import mock
        client = module.AcpClient([], ".")
        ids = []
        client.emit = lambda value: ids.append(value["id"])
        def reply(*args, **kwargs):
            self.assertFalse(client.reply_permission(ids[0], "allow"))
            return True
        client._reply = reply
        with mock.patch.object(module.threading.Event, "wait", return_value=False):
            client._handle_agent_request(1, "session/request_permission", {"options": [{"optionId": "once", "kind": "allow_once"}]})
        self.assertEqual(client._perm_choices, {})

    def test_once_never_escalates_to_always(self):
        result = self.run_permission("allow", [{"kind": "allow_always", "optionId": "forever"}])
        self.assertEqual(result, {"outcome": "cancelled"})

    def test_deny_prefers_reject_once_not_persistent_reject(self):
        result = self.run_permission("deny", [{"kind": "reject_always", "optionId": "forever"}, {"kind": "reject_once", "optionId": "once"}])
        self.assertEqual(result, {"outcome": "selected", "optionId": "once"})

    def test_unknown_decision_cancels_without_selecting_anything(self):
        self.assertEqual(self.run_permission("yes", [{"kind": "allow_once", "optionId": "once"}]), {"outcome": "cancelled"})

    def test_stale_decisions_not_stored(self):
        client = module.AcpClient([], ".")
        client.reply_permission("stale", "allow")
        self.assertEqual(client._perm_answers, {})

    def test_duplicate_reply_cannot_replace_first_decision(self):
        replies = []
        client = module.AcpClient([], ".")
        client._reply = lambda ident, result=None, error=None: replies.append(result)
        def emit(record):
            client.reply_permission(record["id"], "deny")
            client.reply_permission(record["id"], "always")
        client.emit = emit
        client._handle_agent_request(1, "session/request_permission", {"toolCall": {"toolCallId": "tool"}, "options": [{"kind": "allow_always", "optionId": "forever"}]})
        self.assertEqual(replies, [{"outcome": {"outcome": "cancelled"}}])

    def test_cancel_settles_waiting_permission_without_allowing(self):
        client = module.AcpClient([], ".")
        client.session_id = "session"
        replies, ready = [], threading.Event()
        client._reply = lambda ident, result=None, error=None: replies.append(result)
        client.notify = lambda *args: None
        client.emit = lambda record: ready.set()
        worker = threading.Thread(target=client._handle_agent_request, args=(1, "session/request_permission", {"options": [{"kind": "allow_once", "optionId": "yes"}]}), daemon=True)
        worker.start()
        self.assertTrue(ready.wait(1))
        client.cancel()
        worker.join(1)
        self.assertFalse(worker.is_alive())
        self.assertEqual(replies, [{"outcome": {"outcome": "cancelled"}}])
        self.assertEqual(client._perm_events, {})

    def test_reused_tool_call_id_does_not_reuse_permission_identity(self):
        client = module.AcpClient([], ".")
        ids = []
        client._reply = lambda *args, **kwargs: None
        def emit(record):
            ids.append(record["id"])
            client.reply_permission(record["id"], "deny")
        client.emit = emit
        for i in range(2):
            client._handle_agent_request(i, "session/request_permission", {"toolCall": {"toolCallId": "same"}})
        self.assertNotEqual(ids[0], ids[1])

    def test_timeout_cleans_permission_state(self):
        from unittest import mock
        client = module.AcpClient([], ".", emit=lambda _record: None)
        replies = []
        client._reply = lambda ident, result=None, error=None: replies.append(result)
        with mock.patch.object(module.threading.Event, "wait", return_value=False):
            client._handle_agent_request(1, "session/request_permission", {})
        self.assertEqual(replies, [{"outcome": {"outcome": "cancelled"}}])
        self.assertEqual(client._perm_events, {})
        self.assertEqual(client._perm_answers, {})

    def test_emit_failure_cleans_permission_state(self):
        client = module.AcpClient([], ".")
        client._reply = lambda *args, **kwargs: None
        def fail(_record):
            raise RuntimeError("consumer failed")
        client.emit = fail
        client._handle_agent_request(1, "session/request_permission", {})
        self.assertEqual(client._perm_events, {})
        self.assertEqual(client._perm_answers, {})


class RequestLifecycleTests(unittest.TestCase):
    def test_failed_send_removes_pending_entry(self):
        client = module.AcpClient([], ".")
        with self.assertRaises(module.AcpError):
            client.request("initialize", timeout=0.01)
        self.assertEqual(client._pending, {})

    def test_eof_settles_pending_requests_immediately(self):
        from types import SimpleNamespace
        client = module.AcpClient([], ".")
        ev, box = threading.Event(), {}
        client._pending[1] = (ev, box)
        client.proc = SimpleNamespace(stdout=iter([]))
        client._read_loop()
        self.assertTrue(ev.is_set())
        self.assertIn("error", box)
        self.assertEqual(client._pending, {})

    def test_invalid_json_values_do_not_crash_reader(self):
        from types import SimpleNamespace
        client = module.AcpClient([], ".")
        ev, box = threading.Event(), {}
        client._pending[1] = (ev, box)
        client.proc = SimpleNamespace(stdout=iter(['null\n', '[]\n', '"text"\n', '{"jsonrpc":"2.0","id":[],"result":{}}\n', '{"jsonrpc":"2.0","id":1,"result":{"ok":true}}\n']))
        client._read_loop()
        self.assertTrue(ev.is_set())
        self.assertEqual(box.get("result"), {"ok": True})

    def test_stop_fails_all_waiting_requests(self):
        client = module.AcpClient([], ".")
        ev, box = threading.Event(), {}
        client._pending[1] = (ev, box)
        client.stop()
        self.assertTrue(ev.is_set())
        self.assertIn("error", box)
        self.assertEqual(client._pending, {})

    def test_timeout_removes_pending_request(self):
        client = module.AcpClient([], ".")
        client._send = lambda *args: None
        with self.assertRaises(module.AcpError):
            client.request("initialize", timeout=0)
        self.assertEqual(client._pending, {})

    def test_requests_after_eof_fail_without_sending(self):
        from types import SimpleNamespace
        client = module.AcpClient([], ".")
        client.proc = SimpleNamespace(stdout=iter([]))
        client._read_loop()
        sent = []
        client._send = sent.append
        with self.assertRaises(module.AcpError):
            client.request("initialize", timeout=0)
        self.assertEqual(sent, [])
        self.assertEqual(client._pending, {})

    def test_malformed_notification_does_not_drop_connection(self):
        from types import SimpleNamespace
        client = module.AcpClient([], ".", emit=lambda record: None)
        ev, box = threading.Event(), {}
        client._pending[1] = (ev, box)
        client.proc = SimpleNamespace(stdout=iter([
            '{"method":"session/update","params":{"update":[1]}}\n',
            '{"method":"session/update","params":{"update":{"sessionUpdate":"tool_call","toolCallId":"x","status":"completed","content":[{"content":[1]}]}}}\n',
            '{"id":1,"result":{"ok":true}}\n']))
        client._read_loop()
        self.assertEqual(box.get("result"), {"ok": True})

    def test_write_failure_settles_other_pending_requests(self):
        from types import SimpleNamespace
        class BrokenPipe:
            def write(self, line):
                raise BrokenPipeError("closed")
        client = module.AcpClient([], ".")
        ev, box = threading.Event(), {}
        client._pending[999] = (ev, box)
        client.proc = SimpleNamespace(stdin=BrokenPipe())
        with self.assertRaises(module.AcpError):
            client.request("initialize", timeout=0.01)
        self.assertTrue(ev.is_set())
        self.assertIn("error", box)
        self.assertEqual(client._pending, {})

    def test_stderr_flood_does_not_block_protocol(self):
        import sys
        import tempfile
        client = module.AcpClient([sys.executable, "-u", "-c", 'import sys,json; sys.stderr.write("x"*200000); sys.stderr.flush(); m=json.loads(sys.stdin.readline()); print(json.dumps({"id":m["id"],"result":{"ok":True}}),flush=True)'], tempfile.gettempdir())
        client.start()
        try:
            self.assertEqual(client.request("initialize", timeout=2), {"ok": True})
        finally:
            client.stop()
            for pipe in (client.proc.stdin, client.proc.stdout, client.proc.stderr):
                pipe.close()


if __name__ == "__main__":
    unittest.main()
