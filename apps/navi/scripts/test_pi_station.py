"""Real Station daemon and fake native Pi process. No provider calls."""
import json
import os
from pathlib import Path
import tempfile
import unittest
import test_assistant

FAKE = '''#!/usr/bin/python3
import json,sys
assert '--mode' in sys.argv and sys.argv[sys.argv.index('--mode')+1]=='rpc'
for flag in ['--no-tools','--no-extensions','--no-skills','--no-context-files','--no-prompt-templates','--no-themes']:
 assert flag in sys.argv
assert sys.argv[sys.argv.index('--model')+1]=='offline/fake'
m=json.loads(sys.stdin.buffer.readline())
def send(v):print(json.dumps(v),flush=True)
while m['type'].startswith('set_auto_'):
 send({'type':'response','id':m['id'],'success':True})
 m=json.loads(sys.stdin.buffer.readline())
send({'type':'response','id':m['id'],'success':True})
send({'type':'agent_start'})
send({'type':'turn_start'})
send({'type':'message_update','assistantMessageEvent':{'type':'text_delta','delta':'native Pi answer'}})
send({'type':'message_end','message':{'role':'assistant','stopReason':'stop','content':[{'type':'text','text':'native Pi answer'}]}})
send({'type':'agent_settled'})
'''


class NativePiTests(test_assistant.AssistantTests):
    def setUp(self):
        self.pi_temp = tempfile.TemporaryDirectory()
        self.fake = Path(self.pi_temp.name) / "fake pi"
        self.fake.write_text(FAKE)
        self.fake.chmod(0o700)
        self.previous = {k: os.environ.get(k) for k in ("NAVI_PI_EXECUTABLE", "NAVI_PI_MODEL")}
        os.environ["NAVI_PI_EXECUTABLE"] = str(self.fake)
        os.environ["NAVI_PI_MODEL"] = "offline/fake"
        super().setUp()

    def tearDown(self):
        super().tearDown()
        for key, value in self.previous.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value
        self.pi_temp.cleanup()

    def test_pi_capabilities_describe_native_tool_free_route(self):
        status = self.call("harnesses")
        encoded = json.dumps(status)
        self.assertNotIn("pi-acp", encoded)
        self.assertIn("pi-rpc", encoded)

    def test_native_pi_route_correlates_and_streams_without_opencode(self):
        reply = self.call("command", {"text": "what is two plus two", "harness": "pi"}, timeout=10)
        self.assertTrue(reply["ok"], reply)
        answer = reply["result"]["reply"]
        self.assertEqual(answer["text"], "native Pi answer")
        self.assertEqual(answer["source"], "pi-rpc")
        self.assertTrue(answer["turn"].startswith("turn-"))
        state = self.call("status")["result"]
        self.assertFalse(state["busy"])
        self.assertEqual(state["turn_state"]["turn"], answer["turn"])
        self.assertFalse(self.posts("/prompt"))

    def test_truncated_native_pi_answer_carries_visible_warning(self):
        self.fake.write_text(FAKE.replace("'stopReason':'stop'", "'stopReason':'length'"))
        reply = self.call("command", {"text": "what is two plus two", "harness": "pi"}, timeout=10)
        self.assertTrue(reply["ok"], reply)
        answer = reply["result"]["reply"]
        self.assertEqual(answer["finish"], "length")
        self.assertTrue(answer["truncated"])
        self.assertIn("incomplete", answer["warnings"][0])

    def test_native_pi_failure_does_not_make_a_success_reply(self):
        self.fake.write_text("#!/bin/sh\nexit 1\n")
        reply = self.call("command", {"text": "what is two plus two", "harness": "pi"}, timeout=10)
        self.assertFalse(reply["ok"], reply)
        self.assertFalse(self.posts("/prompt"))

    def test_native_pi_cancel_reaps_child_and_never_sends_opencode_prompt(self):
        import threading
        import time
        pid_file = self.folder / "cancel-provider.pid"
        self.fake.write_text("#!/usr/bin/python3\nimport os,signal,sys,time,json\nsignal.signal(signal.SIGTERM,signal.SIG_IGN)\nopen(" + repr(str(pid_file)) + ", 'w').write(str(os.getpid()))\njson.loads(sys.stdin.buffer.readline())\ntime.sleep(60)\n")
        result = {}
        def run():
            result["reply"] = self.call("command", {"text": "what is two plus two", "harness": "pi"}, timeout=10)
        thread = threading.Thread(target=run)
        thread.start()
        for _ in range(100):
            if self.call("status")["result"]["busy"]:
                break
            time.sleep(.02)
        for _ in range(100):
            if pid_file.exists():
                break
            time.sleep(.02)
        self.assertTrue(pid_file.exists())
        cancelled = self.call("cancel")
        self.assertTrue(cancelled["ok"], cancelled)
        thread.join(timeout=5)
        self.assertFalse(thread.is_alive())
        self.assertTrue(result["reply"]["ok"], result)
        self.assertTrue(result["reply"]["result"]["reply"]["stopped"])
        self.assertFalse(self.call("status")["result"]["busy"])
        with self.assertRaises(ProcessLookupError):
            os.kill(int(pid_file.read_text()), 0)
        self.assertFalse(self.posts("/prompt"))

    def test_native_pi_error_reaps_provider_before_returning_failure(self):
        import signal
        import time
        pid_file = self.folder / "provider.pid"
        prefix = "import os,signal,time\nsignal.signal(signal.SIGTERM,signal.SIG_IGN)\nopen(" + repr(str(pid_file)) + ", 'w').write(str(os.getpid()))\n"
        self.fake.write_text(FAKE.replace("m=json.loads", prefix + "m=json.loads", 1)
                             .replace("send({'type':'message_update'", "send({'type':'tool_execution_start'})\nsend({'type':'message_update'", 1)
                             + "\ntime.sleep(60)\n")
        reply = self.call("command", {"text": "what is two plus two", "harness": "pi"}, timeout=10)
        self.assertFalse(reply["ok"], reply)
        pid = int(pid_file.read_text())
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            pass
        else:
            os.kill(pid, signal.SIGKILL)
            self.fail("Provider survived the failed turn")
        self.assertFalse(self.posts("/prompt"))

    def test_native_pi_text_attachment_is_forwarded_with_prompt(self):
        note = self.folder / "pi-note.txt"
        note.write_text("attachment fixture")
        self.fake.write_text(FAKE.replace("send({'type':'agent_start'})", "assert 'attachment fixture' in m['message'] and 'untrusted_text' in m['message']\nsend({'type':'agent_start'})"))
        reply = self.call("command", {"text": "what is two plus two", "harness": "pi",
                                      "attachments": [{"path": str(note), "name": "pi-note.txt"}]}, timeout=10)
        self.assertTrue(reply["ok"], reply)
        self.assertFalse(self.posts("/prompt"))

    def test_native_pi_unsupported_attachment_is_error(self):
        note = self.folder / "pi-note.png"
        note.write_bytes(b"not an image")
        reply = self.call("command", {"text": "what is two plus two", "harness": "pi",
                                      "attachments": [{"path": str(note), "name": "pi-note.png"}]}, timeout=10)
        self.assertFalse(reply["ok"], reply)
        self.assertIn("attachment", reply["error"])
        self.assertFalse(self.posts("/prompt"))

    def test_decoded_text_scratch_is_removed_on_success_and_error(self):
        import base64
        for name, data, success in [("note.txt", b"fixture", True), ("image.png", b"bad", False)]:
            reply = self.call("command", {"text": "what is two plus two", "harness": "pi",
                "attachments": [{"name": name, "data": base64.b64encode(data).decode()}]}, timeout=10)
            self.assertEqual(reply["ok"], success, reply)
            self.assertEqual(list(self.runtime.glob("attach-*")), [])

    def test_partial_attachment_failure_removes_only_owned_scratch(self):
        import base64
        user_file = self.runtime / "attach-user-note.txt"
        user_file.write_text("user-owned")
        reply = self.call("command", {"text": "what is two plus two", "harness": "pi", "attachments": [
            {"name": "note.txt", "data": base64.b64encode(b"fixture").decode()},
            {"path": str(self.folder / "missing.txt")}]}, timeout=10)
        self.assertFalse(reply["ok"], reply)
        self.assertEqual(list(self.runtime.glob("attach-*")), [user_file])
        self.assertEqual(user_file.read_text(), "user-owned")

    def test_path_attachments_reject_symlink_fifo_oversize_and_dual_input(self):
        source = self.folder / "source.txt"
        source.write_text("original")
        link = self.folder / "link.txt"
        link.symlink_to(source)
        fifo = self.folder / "pipe.txt"
        os.mkfifo(fifo)
        big = self.folder / "big.txt"
        big.write_bytes(b"x" * (2 * 1024 * 1024 + 1))
        for item in [{"path": str(link)}, {"path": str(fifo)}, {"path": str(big)},
                     {"path": str(source), "data": "aGk="}]:
            reply = self.call("command", {"text": "what is two plus two", "harness": "pi",
                                          "attachments": [item]}, timeout=10)
            self.assertFalse(reply["ok"], reply)
            self.assertEqual(list(self.runtime.glob("attach-*")), [])
        self.assertEqual(source.read_text(), "original")

    def test_text_snapshot_keeps_unicode_file_content_and_user_file(self):
        note = self.folder / ("ع" * 70 + ".txt")
        note.write_text("snapshot fixture", encoding="utf-8")
        self.fake.write_text(FAKE.replace("send({'type':'agent_start'})", "assert 'snapshot fixture' in m['message']\nsend({'type':'agent_start'})"))
        reply = self.call("command", {"text": "what is two plus two", "harness": "pi",
                                      "attachments": [{"path": str(note)}]}, timeout=10)
        self.assertTrue(reply["ok"], reply)
        self.assertEqual(note.read_text(), "snapshot fixture")
        self.assertEqual(list(self.runtime.glob("attach-*")), [])

    def test_native_pi_needs_explicit_model_without_opencode_fallback(self):
        self.process.terminate()
        self.process.wait(timeout=3)
        self.environment.pop("NAVI_PI_MODEL", None)
        import subprocess
        import time
        self.process = subprocess.Popen([str(test_assistant.BINARY), "serve"], env=self.environment,
                                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(100):
            try:
                self.call("ping")
                break
            except OSError:
                time.sleep(.02)
        reply = self.call("command", {"text": "what is two plus two", "harness": "pi"}, timeout=10)
        self.assertFalse(reply["ok"], reply)
        self.assertIn("NAVI_PI_MODEL", reply["error"])
        self.assertFalse(self.posts("/prompt"))

    def test_opt_in_pi_session_reuses_only_private_project_model_file(self):
        import subprocess
        import time
        self.process.terminate()
        self.process.wait(timeout=3)
        self.environment["NAVI_PI_SESSION_DIR"] = str(self.folder / "pi-sessions")
        log = self.folder / "pi-session-paths.jsonl"
        prefix = "import pathlib\nargs=sys.argv\npath=args[args.index('--session')+1]\npathlib.Path(path).touch(mode=0o600,exist_ok=True)\nwith open(" + repr(str(log)) + ", 'a') as f:f.write(json.dumps(path)+'\\n')\n"
        self.fake.write_text(FAKE.replace("m=json.loads", prefix + "m=json.loads", 1))
        self.process = subprocess.Popen([str(test_assistant.BINARY), "serve"], env=self.environment,
                                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(100):
            try:
                self.call("ping")
                break
            except OSError:
                time.sleep(.02)
        for _ in range(2):
            reply = self.call("command", {"text": "what is two plus two", "harness": "pi"}, timeout=10)
            self.assertTrue(reply["ok"], reply)
        paths = [json.loads(line) for line in log.read_text().splitlines()]
        self.assertEqual(len(paths), 2)
        self.assertEqual(paths[0], paths[1])
        self.assertEqual(Path(paths[0]).stat().st_mode & 0o777, 0o600)


def load_tests(loader, tests, pattern):
    suite = unittest.TestSuite()
    for name in ("test_pi_capabilities_describe_native_tool_free_route",
                 "test_native_pi_route_correlates_and_streams_without_opencode",
                 "test_truncated_native_pi_answer_carries_visible_warning",
                 "test_native_pi_failure_does_not_make_a_success_reply",
                 "test_native_pi_cancel_reaps_child_and_never_sends_opencode_prompt",
                 "test_native_pi_error_reaps_provider_before_returning_failure",
                 "test_native_pi_text_attachment_is_forwarded_with_prompt",
                 "test_native_pi_unsupported_attachment_is_error",
                 "test_decoded_text_scratch_is_removed_on_success_and_error",
                 "test_partial_attachment_failure_removes_only_owned_scratch",
                 "test_path_attachments_reject_symlink_fifo_oversize_and_dual_input",
                 "test_text_snapshot_keeps_unicode_file_content_and_user_file",
                 "test_native_pi_needs_explicit_model_without_opencode_fallback",
                 "test_opt_in_pi_session_reuses_only_private_project_model_file"):
        suite.addTest(NativePiTests(name))
    return suite


if __name__ == "__main__":
    unittest.main()
