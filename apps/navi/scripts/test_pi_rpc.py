#!/usr/bin/env python3
"""Offline subprocess tests for the native Pi RPC transport."""
import json
from pathlib import Path
import sys
import tempfile
import time
import threading
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "workers"))
from pi_rpc import PiError, PiRpcClient, command_for_pi, project_session, prompt_with_attachments, token_usage

FAKE = r'''
import json,sys,time
mode=sys.argv[1]
def emit(v):print(json.dumps(v,ensure_ascii=False),flush=True)
m=json.loads(sys.stdin.buffer.readline())
if mode=='reject':
 emit({'type':'response','id':m['id'],'success':False});time.sleep(1)
elif mode=='hang':time.sleep(10)
elif mode=='oversize':print('x'*2000,flush=True);time.sleep(1)
elif mode=='malformed':print('[]',flush=True);time.sleep(1)
elif mode=='eof':sys.exit(0)
elif mode=='wireflood':
 for _ in range(100):emit({'type':'irrelevant','padding':'x'*700})
 time.sleep(60)
elif mode=='deep':print('{\"a\":'*1200+'0'+'}'*1200,flush=True);time.sleep(60)
else:
 emit({'type':'response','id':m['id'],'success':True})
 emit({'type':'agent_start'})
 emit({'type':'turn_start'})
 emit({'type':'message_update','assistantMessageEvent':{'type':'text_delta','delta':'hello\u2028world'}})
 if mode=='tool':emit({'type':'tool_execution_start','toolName':'bash'})
 if mode=='ui':emit({'type':'extension_ui_request','method':'confirm'})
 if mode=='toolcall':emit({'type':'message_end','message':{'role':'assistant','stopReason':'toolUse','content':[{'type':'toolCall','name':'bash','arguments':{}}]}})
 if mode=='turns':emit({'type':'turn_start'})
 if mode!='noanswer':emit({'type':'message_end','message':{'role':'assistant','stopReason':'error' if mode=='error' else ('length' if mode=='length' else ('deferred' if mode=='deferred' else 'stop')),'content':[{'type':'text','text':'authoritative\u2028answer'}]}})
 emit({'type':'agent_end','messages':[]})
 if mode=='endonly':time.sleep(10)
 emit({'type':'agent_settled'})
 time.sleep(10)
'''


class PiTests(unittest.TestCase):
    def test_command_requires_explicit_model_and_disables_discovery(self):
        for model in [None, "", "automatic", "/model", "provider/", "provider/model name"]:
            with self.assertRaises(PiError):
                command_for_pi("pi", model)
        command = command_for_pi("/path with spaces/pi", "provider/model")
        self.assertEqual(command[0], "/path with spaces/pi")
        for flag in ["--no-tools", "--no-extensions", "--no-context-files", "--no-session"]:
            self.assertIn(flag, command)
        self.assertNotIn("--api-key", command)

    def test_cancel_interrupts_a_blocked_provider(self):
        with tempfile.TemporaryDirectory() as cwd:
            client = PiRpcClient([sys.executable, "-u", "-c", FAKE, "hang"], cwd)
            client.start()
            timer = threading.Timer(.05, client.cancel)
            timer.start()
            try:
                with self.assertRaisesRegex(PiError, "cancelled"):
                    client.prompt("hello", timeout=1)
            finally:
                timer.join()
                client.stop()

    def test_stop_closes_pipes_even_when_a_descendant_ignores_term(self):
        with tempfile.TemporaryDirectory() as cwd:
            script = "import subprocess,sys,time; subprocess.Popen([sys.executable,'-c', 'import signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN);time.sleep(60)']); time.sleep(60)"
            client = PiRpcClient([sys.executable, "-u", "-c", script], cwd)
            client.start()
            time.sleep(.05)
            started = time.monotonic()
            client.stop()
            self.assertLess(time.monotonic() - started, 2)
            self.assertIsNotNone(client.proc.poll())

    def test_stop_is_idempotent_and_cancel_never_signals_a_reaped_pid(self):
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as cwd:
            client = PiRpcClient([sys.executable, "-u", "-c", "import time;time.sleep(60)"], cwd)
            client.start()
            client.stop()
            with patch("pi_rpc.os.killpg") as signal_group:
                client.stop()
                client.cancel()
                signal_group.assert_not_called()

    def test_group_kill_precedes_reaping_even_when_wrapper_exits(self):
        from unittest.mock import patch
        import os
        with tempfile.TemporaryDirectory() as cwd:
            client = PiRpcClient([sys.executable, "-u", "-c", "pass"], cwd)
            client.start()
            time.sleep(.05)
            real_kill = os.killpg
            def checked_kill(pid, sig):
                self.assertIsNone(client.proc.returncode)
                return real_kill(pid, sig)
            with patch("pi_rpc.os.killpg", side_effect=checked_kill):
                client.stop()
            self.assertIsNotNone(client.proc.returncode)

    def test_prompt_deadline_includes_child_that_never_reads_stdin(self):
        with tempfile.TemporaryDirectory() as cwd:
            client = PiRpcClient([sys.executable, "-u", "-c", "import time;time.sleep(60)"], cwd)
            client.start()
            started = time.monotonic()
            try:
                with self.assertRaisesRegex(PiError, "timed out"):
                    client.prompt("x" * 30000, timeout=.1)
            finally:
                client.stop()
            self.assertLess(time.monotonic() - started, 2)

    def test_opt_in_session_is_stable_and_project_model_isolated(self):
        with tempfile.TemporaryDirectory() as cwd:
            directory = str(Path(cwd) / "sessions")
            with project_session(directory, cwd, "provider/a") as first:
                self.assertTrue(first.endswith(".jsonl"))
                command = command_for_pi("pi", "provider/a", first)
                self.assertIn("--session", command)
                self.assertNotIn("--no-session", command)
                with self.assertRaisesRegex(PiError, "in use"):
                    with project_session(directory, cwd, "provider/a"):
                        pass
            with project_session(directory, cwd, "provider/a") as again:
                self.assertEqual(first, again)
            with project_session(directory, cwd, "provider/b") as other:
                self.assertNotEqual(first, other)
            with project_session(directory, cwd + "/different", "provider/a") as other:
                self.assertNotEqual(first, other)

    def test_session_directory_and_files_must_be_private_not_symlinks(self):
        with tempfile.TemporaryDirectory() as cwd:
            root = Path(cwd) / "sessions"
            root.mkdir(mode=0o755)
            with self.assertRaisesRegex(PiError, "0700"):
                with project_session(str(root), cwd, "provider/a"):
                    pass
            root.chmod(0o700)
            with project_session(str(root), cwd, "provider/a") as transcript:
                pass
            Path(transcript).symlink_to(Path(cwd) / "elsewhere")
            with self.assertRaisesRegex(PiError, "symlink"):
                with project_session(str(root), cwd, "provider/a"):
                    pass

    def test_default_session_is_ephemeral(self):
        with project_session(None, "/project", "provider/a") as session:
            self.assertIsNone(session)

    def test_preflight_disables_retries_and_compaction_before_prompt(self):
        script = "import json,sys; commands=['set_auto_retry','set_auto_compaction']; " + "\nfor expected in commands:\n m=json.loads(sys.stdin.buffer.readline()); assert m['type']==expected and m['enabled'] is False;print(json.dumps({'type':'response','id':m['id'],'success':True}),flush=True)\nimport time;time.sleep(60)"
        with tempfile.TemporaryDirectory() as cwd:
            client = PiRpcClient([sys.executable, "-u", "-c", script], cwd)
            client.start()
            try:
                client.configure(timeout=.5)
            finally:
                client.stop()

    def test_preflight_rejection_is_error_not_a_model_prompt(self):
        script = "import json,sys,time;m=json.loads(sys.stdin.buffer.readline());print(json.dumps({'type':'response','id':m['id'],'success':False}),flush=True);time.sleep(60)"
        with tempfile.TemporaryDirectory() as cwd:
            client = PiRpcClient([sys.executable, "-u", "-c", script], cwd)
            client.start()
            try:
                with self.assertRaisesRegex(PiError, "rejected"):
                    client.configure(timeout=.5)
            finally:
                client.stop()

    def test_preflight_cancel_does_not_wait_for_settings_deadline(self):
        with tempfile.TemporaryDirectory() as cwd:
            client = PiRpcClient([sys.executable, "-u", "-c", "import time;time.sleep(60)"], cwd)
            client.start()
            timer = threading.Timer(.03, client.cancel)
            timer.start()
            started = time.monotonic()
            try:
                with self.assertRaisesRegex(PiError, "cancelled"):
                    client.configure(timeout=5)
            finally:
                timer.join()
                client.stop()
            self.assertLess(time.monotonic() - started, 2)

    def test_preflight_deadline_includes_stdin_send(self):
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as cwd:
            client = PiRpcClient([sys.executable, "-u", "-c", "import time;time.sleep(60)"], cwd)
            client.start()
            send = client.send
            def large_send(value):
                send({**value, "padding": "x" * 100000})
            started = time.monotonic()
            try:
                with patch.object(client, "send", side_effect=large_send):
                    with self.assertRaisesRegex(PiError, "timed out"):
                        client.configure(timeout=.05)
            finally:
                client.stop()
            self.assertLess(time.monotonic() - started, 2)

    def test_cancelled_preflight_cannot_accept_queued_acknowledgements(self):
        with tempfile.TemporaryDirectory() as cwd:
            client = PiRpcClient([sys.executable, "-u", "-c", "import time;time.sleep(60)"], cwd)
            client.start()
            for command in ("set_auto_retry", "set_auto_compaction"):
                client.events.put({"type": "response", "id": "setup-" + command, "success": True})
            client.cancel()
            try:
                with self.assertRaisesRegex(PiError, "cancelled"):
                    client.configure()
            finally:
                client.stop()

    def test_text_attachments_preserve_content_without_discovery(self):
        with tempfile.TemporaryDirectory() as cwd:
            path = Path(cwd) / "note.md"
            path.write_text("sample text\nالعربية\n", encoding="utf-8")
            result = prompt_with_attachments("question", [str(path)])
            record = json.loads(result.splitlines()[-1])
            self.assertEqual(record["untrusted_text"], path.read_text())
            self.assertEqual(record["attachment"], "note.md")
            self.assertIn("untrusted data", result)
            self.assertNotIn(cwd, result)

    def test_text_attachments_reject_binary_unsupported_and_oversize(self):
        with tempfile.TemporaryDirectory() as cwd:
            for name, data in [("note.png", b"image"), ("note.txt", b"x" * 8193),
                               ("bad.md", b"\xff"), ("binary.txt", b"a\x00b"), ("delete.txt", b"a\x7fb")]:
                path = Path(cwd) / name
                path.write_bytes(data)
                with self.assertRaises(PiError):
                    prompt_with_attachments("question", [str(path)])

    def test_text_attachments_reject_symlinks_and_bound_total(self):
        with tempfile.TemporaryDirectory() as cwd:
            path = Path(cwd) / "note.txt"
            path.write_text("x" * 8192)
            link = Path(cwd) / "link.txt"
            link.symlink_to(path)
            with self.assertRaises(OSError):
                prompt_with_attachments("q", [str(link)])
            with self.assertRaisesRegex(PiError, "24 KiB"):
                prompt_with_attachments("q", [str(path)] * 4)
            with self.assertRaises(PiError):
                prompt_with_attachments("q", [str(path)] * 5)

    def test_token_usage_never_double_counts_reasoning_or_invents_cost(self):
        usage = token_usage({"input": 10, "output": 8, "reasoning": 3, "cacheRead": 2,
                             "cacheWrite": 0, "totalTokens": 20, "cost": {"total": 99}})
        self.assertEqual(sum(usage["tokens"].values()), 20)
        self.assertNotIn("cost", usage)
        self.assertIsNone(token_usage({"input": True, "output": float("nan"), "cacheRead": -1}))

    def run_case(self, mode, **kwargs):
        with tempfile.TemporaryDirectory() as cwd:
            records = []
            client = PiRpcClient([sys.executable, "-u", "-c", FAKE, mode], cwd,
                                 records.append, max_bytes=1024)
            client.start()
            try:
                result = client.prompt("test", timeout=.3, **kwargs)
                return result, records
            finally:
                client.stop()
                self.assertIsNotNone(client.proc.poll())

    def test_final_message_is_authoritative_and_unicode_is_not_a_delimiter(self):
        result, records = self.run_case("ok")
        self.assertEqual(result["text"], "authoritative\u2028answer")
        self.assertEqual(records, [{"type": "delta", "text": "hello\u2028world"}])

    def test_agent_end_is_not_settled(self):
        with self.assertRaisesRegex(PiError, "timed out"):
            self.run_case("endonly")

    def test_rejection_is_not_success(self):
        with self.assertRaisesRegex(PiError, "rejected"):
            self.run_case("reject")

    def test_hang_has_deadline(self):
        start = time.monotonic()
        with self.assertRaisesRegex(PiError, "timed out"):
            self.run_case("hang")
        self.assertLess(time.monotonic() - start, 2)

    def test_eof_is_not_empty_success(self):
        with self.assertRaisesRegex(PiError, "closed"):
            self.run_case("eof")

    def test_oversized_record_is_rejected(self):
        with self.assertRaisesRegex(PiError, "limit"):
            self.run_case("oversize")

    def test_total_wire_budget_includes_nontext_records(self):
        with self.assertRaisesRegex(PiError, "budget|cancelled"):
            self.run_case("wireflood")

    def test_deep_json_is_rejected_without_reader_thread_crash(self):
        with tempfile.TemporaryDirectory() as cwd:
            client = PiRpcClient([sys.executable, "-u", "-c", FAKE, "deep"], cwd)
            client.start()
            try:
                with self.assertRaisesRegex(PiError, "invalid JSON"):
                    client.prompt("hello", timeout=1)
            finally:client.stop()

    def test_truncated_answer_is_labelled_not_full_completion(self):
        result, _ = self.run_case("length")
        self.assertEqual(result["finish"], "length")
        self.assertTrue(result["truncated"])
        self.assertIn("incomplete", result["warnings"][0])

    def test_deferred_result_is_not_synchronous_success(self):
        with self.assertRaisesRegex(PiError, "synchronously"):
            self.run_case("deferred")

    def test_nonobject_record_is_rejected(self):
        with self.assertRaisesRegex(PiError, "non-object"):
            self.run_case("malformed")

    def test_unexpected_tool_execution_is_stopped(self):
        with self.assertRaisesRegex(PiError, "Unexpected"):
            self.run_case("tool")

    def test_extension_question_cannot_authorize_tools(self):
        with self.assertRaisesRegex(PiError, "Unexpected"):
            self.run_case("ui")

    def test_turn_budget(self):
        with self.assertRaisesRegex(PiError, "budget"):
            self.run_case("turns", max_turns=1)

    def test_unexpected_toolcall_message_is_not_success(self):
        with self.assertRaisesRegex(PiError, "tool"):
            self.run_case("toolcall")

    def test_provider_error_is_not_final_success(self):
        with self.assertRaisesRegex(PiError, "failed"):
            self.run_case("error")

    def test_settled_without_answer_is_error(self):
        with self.assertRaisesRegex(PiError, "without"):
            self.run_case("noanswer")


if __name__ == "__main__":
    unittest.main()
