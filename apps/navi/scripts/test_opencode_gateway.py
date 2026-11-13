"""OpenCode gateway contract against a fake server (no live OpenCode needed)."""
import importlib.util
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest import mock

WORKERS = Path(__file__).resolve().parents[1] / "workers"
sys.path.insert(0, str(WORKERS))
import opencode_contract
import opencode_gateway as gateway_module
from opencode_gateway import Gateway

# The real server drops body fields it does not know and answers 200. This fake
# is stricter on purpose: a body that does not match OpenCode's own spec is a 400
# and a recorded violation, so a wrong field name fails here, not in the user's app.
CONTRACT = opencode_contract.load()
ROUTES = [(re.compile("^" + re.sub(r"\{[^}]+\}", "[^/]+", entry["path"]) + "$"), entry["method"], name)
          for name, entry in CONTRACT["operations"].items()]


def operation_for(method, path):
    path = path.split("?", 1)[0]
    return next((name for pattern, verb, name in ROUTES if verb == method and pattern.match(path)), None)


def sse_event(kind, session, **fields):
    data = {"sessionID": session}
    data.update(fields)
    return {"type": kind, "data": data}


def turn_events(session, reply, model=None, agent=None, tool=True):
    """One realistic verified-schema turn."""
    events = [
        sse_event("session.step.started", session,
                  agent=agent or "navi",
                  model=model or {"id": "deepseek/deepseek-v4.1-flash",
                                  "providerID": "commandcode", "variant": "high"}),
        sse_event("session.step.streamed", session),
        sse_event("session.reasoning.started", session),
        sse_event("session.reasoning.delta", session, delta="reasoning…"),
        sse_event("session.reasoning.ended", session),
    ]
    if tool:
        events += [
            sse_event("session.tool.input.started", session, id="call_1", name="shell"),
            sse_event("session.tool.input.ended", session, id="call_1",
                      text='{"command":"ls"}'),
            sse_event("session.tool.called", session, id="call_1",
                      input={"command": "ls"}, executed=False),
            sse_event("session.tool.progress", session, id="call_1", progress="running"),
            sse_event("session.tool.success", session, id="call_1",
                      content="README.md", metadata={"exit": 0}),
        ]
    events += [
        sse_event("session.text.started", session),
        sse_event("session.text.delta", session, delta=reply),
        sse_event("session.text.ended", session, text=reply),
        sse_event("session.step.ended", session,
                  tokens={"input": 10, "output": 5, "reasoning": 2, "cache": 1},
                  cost=0.002, finish="stop"),
        sse_event("session.usage.updated", session, totalTokens=17, cost=0.01),
        sse_event("session.execution.succeeded", session),
    ]
    return events


class FakeOpenCode(BaseHTTPRequestHandler):
    requests = []
    replies = {}
    session_counter = 0
    message_counter = 0
    last_directory = None
    reject_files = False
    honor_directory = True
    next_events = None
    contract_errors = []
    pending_permissions = []
    parents = {}
    # Event delivery is per HTTP-server instance (attached in setUp): a dead
    # server's lingering SSE handler must never steal another server's queue.

    def log_message(self, *unused):
        pass

    def send_json(self, value, status=200):
        payload = json.dumps({"data": value}).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def body(self):
        length = int(self.headers.get("Content-Length") or 0)
        return json.loads(self.rfile.read(length) or b"null")

    def record(self, entry):
        FakeOpenCode.requests.append(entry)

    def push_events(self, events):
        with self.server.sse_condition:
            self.server.pending_events.extend(events)
            self.server.sse_condition.notify_all()

    def serve_events(self):
        """GET /api/event: hold the SSE stream, draining queued events."""
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        try:
            while True:
                with self.server.sse_condition:
                    self.server.sse_condition.wait(timeout=0.2)
                    batch, self.server.pending_events = self.server.pending_events, []
                for event in batch:
                    self.wfile.write(f"data: {json.dumps(event)}\n\n".encode())
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, OSError):
            return

    def do_GET(self):
        if operation_for("GET", self.path) is None:
            FakeOpenCode.contract_errors.append(f"GET {self.path} is not in the contract")
        if self.path.startswith("/api/event"):
            self.serve_events()
        elif self.path.startswith("/api/permission/request"):
            self.send_json(FakeOpenCode.pending_permissions)
        elif self.path.startswith("/api/agent"):
            self.send_json([
                {"id": "build", "name": "Build", "mode": "primary", "hidden": False},
                {"id": "general", "name": "General", "mode": "subagent", "hidden": False},
                {"id": "navi", "name": "navi", "mode": "primary", "hidden": False},
            ])
        elif self.path.startswith("/api/model"):
            self.send_json([
                {"providerID": "opencode", "modelID": "glm-5.3-flash-free", "name": "GLM Free", "type": "chat"},
                {"providerID": "commandcode", "modelID": "deepseek/deepseek-v4.1-flash", "name": "DeepSeek", "type": "chat"},
                {"providerID": "x", "modelID": "embed", "name": "Embed", "type": "embedding"},
            ])
        elif self.path.startswith("/api/command"):
            self.send_json([{"name": "review", "description": "review changes"},
                            {"name": "init", "description": "guided AGENTS.md setup"}])
        elif re.match(r"^/api/session/[^/]+/message", self.path):
            session = self.path.split("/")[3]
            reply = FakeOpenCode.replies.get(session)
            messages = [] if not reply else [{
                "id": f"msg_{session}_{reply['n']}", "time": {"completed": True},
                "content": [{"type": "text", "text": reply["text"]}],
            }]
            self.send_json(messages)
        elif re.match(r"^/api/session/[^/]+$", self.path):
            session = self.path.rsplit("/", 1)[-1]
            self.send_json({"id": session, "parentID": FakeOpenCode.parents.get(session)})
        else:
            self.send_json(None, 404)

    def do_POST(self):
        path = self.path
        body = self.body()
        self.record({"path": path, "body": body})
        operation = operation_for("POST", path)
        errors = ([f"POST {path} is not in the contract"] if operation is None
                  else opencode_contract.validate(CONTRACT, operation, body))
        if errors:
            FakeOpenCode.contract_errors.extend(errors)
            self.send_json({"error": errors}, 400)
            return
        if path == "/api/session":
            FakeOpenCode.session_counter += 1
            session = f"ses_fake{FakeOpenCode.session_counter}"
            FakeOpenCode.replies.pop(session, None)
            FakeOpenCode.last_directory = ((body or {}).get("location") or {}).get("directory")
            given = FakeOpenCode.last_directory if FakeOpenCode.honor_directory else None
            self.send_json({"id": session, "location": {"directory": given or "/home/fake"}})
        elif re.match(r"^/api/session/[^/]+/prompt$", path):
            session = path.split("/")[3]
            if (body or {}).get("files") and FakeOpenCode.reject_files:
                self.send_json({"error": "files unsupported"}, 400)
                return
            text = (body or {}).get("text", "")
            FakeOpenCode.message_counter += 1
            FakeOpenCode.replies[session] = {
                "n": FakeOpenCode.message_counter,
                "text": "" if text == "silent" else f"echo: {text}"}
            events = FakeOpenCode.next_events
            FakeOpenCode.next_events = None
            if events is None:
                events = turn_events(session, f"echo: {text}")
            self.push_events(events)
            self.send_json(None)
        elif re.match(r"^/api/session/[^/]+/command$", path):
            session = path.split("/")[3]
            reply = f"ran {(body or {}).get('name')} {(body or {}).get('text')}".strip()
            FakeOpenCode.message_counter += 1
            FakeOpenCode.replies[session] = {"n": FakeOpenCode.message_counter,
                                             "text": reply}
            events = FakeOpenCode.next_events
            FakeOpenCode.next_events = None
            if events is None:
                events = turn_events(session, reply, tool=False)
            self.push_events(events)
            self.send_json(None)
        elif re.match(r"^/api/session/[^/]+/interrupt$", path):
            if FakeOpenCode.fail_interrupts:
                FakeOpenCode.fail_interrupts -= 1
                self.send_json({"error": "cannot interrupt"}, 500)
            else:
                self.send_json(None)
        elif re.match(r"^/api/session/[^/]+/(model|agent)$", path):
            self.send_json(None)
        elif re.match(r"^/api/session/[^/]+/(permission|form)/[^/]+/reply$", path):
            self.send_json(None)
        else:
            self.send_json(None, 404)


class GatewayTests(unittest.TestCase):
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
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), FakeOpenCode)
        self.server.sse_condition = threading.Condition()
        self.server.pending_events = []
        self.server.daemon_threads = True
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.folder = tempfile.TemporaryDirectory()
        self.session_file = str(Path(self.folder.name) / "session")
        self.gateway = Gateway(base_url=f"http://127.0.0.1:{self.server.server_port}",
                               password="pw", session_file=self.session_file)

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.folder.cleanup()
        self.assertEqual(FakeOpenCode.contract_errors, [],
                         "the gateway sent a request OpenCode's spec does not accept")

    def paths(self, suffix=None):
        return [entry["path"] for entry in FakeOpenCode.requests
                if suffix is None or entry["path"].endswith(suffix)]

    def posts(self, suffix):
        return [entry for entry in FakeOpenCode.requests if entry["path"].endswith(suffix)]

    def session_map(self):
        return json.loads(Path(self.session_file).read_text())

    # -- pre-existing behaviour -------------------------------------------------

    def test_ask_sets_model_agent_and_returns_the_reply(self):
        reply = self.gateway.ask("hello")
        self.assertEqual(reply, "echo: hello")
        self.assertIn("/api/session/ses_fake1/model", self.paths())
        model = next(entry for entry in FakeOpenCode.requests if entry["path"].endswith("/model"))
        self.assertEqual(model["body"], {"model": {"providerID": "commandcode", "id": "deepseek/deepseek-v4.1-flash"}})
        agent = next(entry for entry in FakeOpenCode.requests if entry["path"].endswith("/agent"))
        self.assertEqual(agent["body"], {"agent": "navi"})
        self.assertEqual(self.session_map()["default"]["id"], "ses_fake1")

    def test_second_gateway_reuses_the_session_file(self):
        session = self.gateway.ensure_session()
        other = Gateway(base_url=self.gateway.base_url, password="pw", session_file=self.session_file)
        self.assertEqual(other.ensure_session(), session)

    def test_legacy_plain_id_session_file_migrates(self):
        Path(self.session_file).write_text("ses_legacy9")
        other = Gateway(base_url=self.gateway.base_url, password="pw",
                        session_file=self.session_file)
        self.assertEqual(other.ensure_session(), "ses_legacy9")
        stored = self.session_map()
        self.assertEqual(stored["default"]["id"], "ses_legacy9")
        self.assertEqual(stored["default"]["model"], self.gateway.model_ref)

    def test_slash_posts_name_and_arguments(self):
        reply = self.gateway.slash("/review commit")
        self.assertEqual(reply, "ran review commit")
        command = next(entry for entry in FakeOpenCode.requests if entry["path"].endswith("/command"))
        self.assertEqual(command["body"], {"name": "review", "text": "commit"})

    def test_models_filter_chat_and_sort_by_provider(self):
        models = self.gateway.models()
        self.assertEqual([item["providerID"] for item in models], ["commandcode", "opencode"])
        self.assertTrue(all(item["id"] != "embed" for item in models))

    def test_agents_exclude_subagents_and_hidden(self):
        agents = self.gateway.agents()
        self.assertEqual([item["id"] for item in agents], ["build", "navi"])

    def test_commands_pass_through(self):
        self.assertEqual([item["name"] for item in self.gateway.commands()], ["review", "init"])

    def test_empty_reply_is_a_timeout(self):
        with self.assertRaises(gateway_module.GatewayError):
            self.gateway.ask("silent", timeout=0.6)

    def test_a_turn_that_keeps_working_outlasts_the_timeout(self):
        self.gateway._turn_started = self.gateway._last_activity = time.time()
        self.gateway._busy = True
        self.gateway._events_alive = True
        self.gateway.session_id = "ses"
        self.gateway._latest_reply = lambda previous: None
        stop = threading.Event()
        def working():
            while not stop.wait(.1):
                self.gateway._last_activity = time.time()
        thread = threading.Thread(target=working)
        thread.start()
        timer = threading.Timer(1.0, lambda: setattr(self.gateway, "_ended_texts", ["done"])
                                or setattr(self.gateway, "_busy", False))
        timer.start()
        try:
            self.assertEqual(self.gateway._wait_reply(set(), timeout=.4), "done")
        finally:
            stop.set()
            thread.join()
            timer.join()

    def test_a_sentence_before_a_tool_is_not_the_reply(self):
        """A real turn ended on "Starting the wait now." while its tool still ran."""
        step = {"id": "a", "time": {"completed": 1}, "finish": "tool-calls",
                "content": [{"type": "text", "text": "Starting the wait now."}]}
        self.gateway._assistant_messages = lambda limit=5: [step]
        self.assertIsNone(self.gateway._latest_reply(set()))
        final = {**step, "id": "b", "finish": "stop",
                 "content": [{"type": "text", "text": "all done"}]}
        self.gateway._assistant_messages = lambda limit=5: [final, step]
        self.assertEqual(self.gateway._latest_reply(set()), "all done")

    def test_execution_started_does_not_end_the_turn(self):
        self.gateway._busy = True
        self.gateway._handle_event("session.execution.started", {})
        self.assertTrue(self.gateway._busy)
        self.assertIsNone(self.gateway._terminal)
        self.gateway._handle_event("session.execution.succeeded", {})
        self.assertFalse(self.gateway._busy)

    def test_server_housekeeping_is_not_activity_in_the_turn(self):
        self.gateway._last_activity = 0
        self.gateway._handle_event("mcp.status.changed", {})
        self.gateway._handle_event("model.updated", {})
        self.assertEqual(self.gateway._last_activity, 0)
        self.gateway._handle_event("session.step.started", {})
        self.assertGreater(self.gateway._last_activity, 0)

    def test_an_interrupted_turn_says_so_at_once(self):
        self.gateway._terminal = "interrupted"
        self.gateway._busy = False
        self.gateway._last_activity = time.time()
        self.gateway._ended_texts = ["Starting the wait now."]
        started = time.time()
        with self.assertRaisesRegex(gateway_module.GatewayError, "interrupted"):
            self.gateway._wait_reply(set(), timeout=30)
        self.assertLess(time.time() - started, 2)

    def test_a_turn_that_ends_with_no_text_says_so_without_the_full_wait(self):
        self.gateway._terminal = "succeeded"
        self.gateway._busy = False
        self.gateway._events_alive = True
        self.gateway._last_activity = time.time() - gateway_module.REPLY_SETTLE_S - 1
        self.gateway._latest_reply = lambda previous: None
        started = time.time()
        with self.assertRaisesRegex(gateway_module.GatewayError, "without a reply"):
            self.gateway._wait_reply(set(), timeout=30)
        self.assertLess(time.time() - started, gateway_module.REPLY_SETTLE_S + 2)

    def test_a_running_tool_is_not_silence_but_a_question_is(self):
        tool = {"finished": False, "name": "shell"}
        self.gateway._tools = {"a": tool}
        self.assertTrue(self.gateway._tool_running())
        tool["name"] = "question"
        self.assertFalse(self.gateway._tool_running())
        tool.update(name="shell", finished=True)
        self.assertFalse(self.gateway._tool_running())

    def test_new_session_rotates_the_file(self):
        self.gateway.ensure_session()
        session = self.gateway.new_session()
        self.assertEqual(session, "ses_fake2")
        self.assertEqual(self.session_map()["default"]["id"], "ses_fake2")

    # -- BUG-011: configure only on change ---------------------------------------

    def test_model_agent_posted_once_per_session(self):
        self.gateway.ask("one")
        self.gateway.ask("two")
        self.assertEqual(len(self.posts("/model")), 1)
        self.assertEqual(len(self.posts("/agent")), 1)

    def test_changed_model_reposts_exactly_once(self):
        self.gateway.ask("one")
        self.gateway.model_ref = "opencode/glm-5.3-flash-free"
        self.gateway.ask("two")
        models = self.posts("/model")
        self.assertEqual(len(models), 2)
        self.assertEqual(models[1]["body"],
                         {"model": {"providerID": "opencode", "id": "glm-5.3-flash-free"}})
        self.assertEqual(len(self.posts("/agent")), 1)

    # -- BUG-012/004: per-directory sessions + directory field --------------------

    def test_sessions_are_stored_per_directory(self):
        dir_a = str(Path(self.folder.name) / "projA")
        dir_b = str(Path(self.folder.name) / "projB")
        gateway_a = Gateway(base_url=self.gateway.base_url, password="pw",
                            session_file=self.session_file, directory=dir_a)
        gateway_b = Gateway(base_url=self.gateway.base_url, password="pw",
                            session_file=self.session_file, directory=dir_b)
        self.assertEqual(gateway_a.ensure_session(), "ses_fake1")
        self.assertEqual(gateway_b.ensure_session(), "ses_fake2")
        session_posts = [entry for entry in FakeOpenCode.requests
                         if entry["path"] == "/api/session"]
        self.assertEqual(len(session_posts), 2)
        self.assertEqual(session_posts[0]["body"]["location"], {"directory": os.path.abspath(dir_a)})
        self.assertEqual(session_posts[0]["body"]["title"], "Navi · projA")
        # Reusing A needs no new POST.
        gateway_a2 = Gateway(base_url=self.gateway.base_url, password="pw",
                             session_file=self.session_file, directory=dir_a)
        self.assertEqual(gateway_a2.ensure_session(), "ses_fake1")
        self.assertEqual(len([e for e in FakeOpenCode.requests
                              if e["path"] == "/api/session"]), 2)
        stored = self.session_map()
        self.assertEqual(stored[os.path.abspath(dir_a)]["id"], "ses_fake1")
        self.assertEqual(stored[os.path.abspath(dir_b)]["id"], "ses_fake2")

    def test_ignored_directory_falls_back_to_prompt_prefix(self):
        FakeOpenCode.honor_directory = False
        directory = str(Path(self.folder.name) / "projA")
        gateway = Gateway(base_url=self.gateway.base_url, password="pw",
                          session_file=self.session_file, directory=directory)
        gateway.ask("hello")
        prompt = next(entry for entry in FakeOpenCode.requests
                      if entry["path"].endswith("/prompt"))
        self.assertTrue(prompt["body"]["text"].startswith(
            f"[working directory: {os.path.abspath(directory)}]"))
        self.assertTrue(gateway.warnings)

    def test_honored_directory_needs_no_prefix(self):
        directory = str(Path(self.folder.name) / "projA")
        os.makedirs(directory, exist_ok=True)
        gateway = Gateway(base_url=self.gateway.base_url, password="pw",
                          session_file=self.session_file, directory=directory)
        gateway.ask("hello")
        prompt = next(entry for entry in FakeOpenCode.requests
                      if entry["path"].endswith("/prompt"))
        self.assertNotIn("[working directory:", prompt["body"]["text"])

    # -- BUG-007/008: interrupt + wedged session ----------------------------------

    def test_interrupt_posts_once_and_reports_true(self):
        self.gateway.ensure_session()
        self.assertTrue(self.gateway.interrupt())
        interrupts = [entry for entry in FakeOpenCode.requests
                      if entry["path"].endswith("/interrupt")]
        self.assertEqual(len(interrupts), 1)

    def test_interrupt_retries_once_then_reports_false(self):
        FakeOpenCode.fail_interrupts = 5
        self.gateway.ensure_session()
        started = time.time()
        self.assertFalse(self.gateway.interrupt())
        self.assertGreaterEqual(time.time() - started, 1.0)
        interrupts = [entry for entry in FakeOpenCode.requests
                      if entry["path"].endswith("/interrupt")]
        self.assertEqual(len(interrupts), 2)

    def test_a_timeout_stops_the_turn_so_the_next_message_is_not_queued_behind_it(self):
        self.gateway.start_event_stream()
        self.gateway.ensure_session()
        session = self.gateway.session_id
        FakeOpenCode.next_events = [
            sse_event("session.step.started", session),
            sse_event("session.text.delta", session, delta="stuck "),
            # no terminal event: the turn never ends
        ]
        try:
            self.gateway.ask("hang", timeout=1.2)
        except gateway_module.GatewayError:
            pass
        self.assertFalse(self.gateway._busy)
        self.assertEqual(len([entry for entry in FakeOpenCode.requests
                              if entry["path"].endswith("/interrupt")]), 1)
        FakeOpenCode.next_events = turn_events(session, "echo: fine")
        self.assertEqual(self.gateway.ask("fine", timeout=8.0), "echo: fine")
        interrupts = [entry for entry in FakeOpenCode.requests
                      if entry["path"].endswith("/interrupt")]
        self.assertGreaterEqual(len(interrupts), 1)

    def test_succeeded_terminal_event_clears_busy_without_step_ended(self):
        """execution.succeeded alone (no step.ended) ends the turn (BUG-008)."""
        self.gateway.start_event_stream()
        self.gateway.ensure_session()
        session = self.gateway.session_id
        FakeOpenCode.next_events = [
            sse_event("session.step.started", session),
            sse_event("session.execution.succeeded", session),
        ]
        # store reply comes from the fake's echo
        reply = self.gateway.ask("go", timeout=8.0)
        self.assertEqual(reply, "echo: go")
        self.assertFalse(self.gateway._busy)

    # -- BUG-005: attachments ------------------------------------------------------

    def test_attachment_is_sent_as_a_file_uri(self):
        """`parts` was dropped by the server with a 200, so attachments never arrived."""
        attachment = Path(self.folder.name) / "my note.txt"
        attachment.write_text("attach me")
        self.gateway.ask("look", attachments=[str(attachment)])
        prompt = next(entry for entry in FakeOpenCode.requests
                      if entry["path"].endswith("/prompt"))
        self.assertNotIn("parts", prompt["body"])
        self.assertEqual(prompt["body"]["files"],
                         [{"uri": attachment.resolve().as_uri(), "name": "my note.txt"}])
        self.assertEqual(self.gateway.last_result["attachments"], "files")

    def test_a_missing_attachment_is_skipped_with_a_warning(self):
        self.gateway.ask("look", attachments=[str(Path(self.folder.name) / "absent.txt")])
        prompt = next(entry for entry in FakeOpenCode.requests if entry["path"].endswith("/prompt"))
        self.assertNotIn("files", prompt["body"])
        self.assertTrue(any("attachment skipped" in w for w in self.gateway.warnings))

    def test_rejected_files_fall_back_to_text(self):
        FakeOpenCode.reject_files = True
        attachment = Path(self.folder.name) / "note.txt"
        attachment.write_text("attach me")
        reply = self.gateway.ask("look", attachments=[str(attachment)])
        self.assertEqual(reply, "echo: look")
        prompts = [entry for entry in FakeOpenCode.requests
                   if entry["path"].endswith("/prompt")]
        self.assertEqual(len(prompts), 2)
        self.assertIn("files", prompts[0]["body"])
        self.assertNotIn("files", prompts[1]["body"])
        self.assertEqual(self.gateway.last_result["attachments"], "text-fallback")
        self.assertTrue(any("text-only" in w for w in self.gateway.warnings))

    # -- BUG-006: permission reply -------------------------------------------------

    def test_permission_reply_posts_opencodes_decision_words(self):
        self.gateway.ensure_session()
        for decision, sent in (("allow", "once"), ("always", "always"), ("deny", "reject")):
            result = self.gateway.permission_reply(f"per_{decision}", decision)
            self.assertTrue(result["replied"], result)
            self.assertEqual(result["endpoint"], f"/api/session/ses_fake1/permission/per_{decision}/reply")
            self.assertEqual(self.posts(f"/per_{decision}/reply")[0]["body"], {"decision": sent})

    def test_a_subagents_approval_is_answered_in_its_own_session(self):
        self.gateway.ensure_session()
        FakeOpenCode.pending_permissions = [{"id": "per_child", "sessionID": "ses_child",
                                             "action": "bash", "resources": ["rm x"]}]
        result = self.gateway.permission_reply("per_child", "allow")
        self.assertEqual(result["endpoint"], "/api/session/ses_child/permission/per_child/reply")

    def test_permission_event_emits_record(self):
        self.gateway.start_event_stream()
        self.gateway.ensure_session()
        session = self.gateway.session_id
        FakeOpenCode.next_events = [
            sse_event("session.step.started", session),
            sse_event("permission.asked", session, id="per_9", action="bash",
                      resources=["rm -rf /tmp/x"], metadata={"command": "rm -rf /tmp/x"},
                      message="Run rm?"),
            sse_event("session.execution.succeeded", session),
        ]
        records = []
        self.gateway.ask("go", timeout=8.0, emit=records.append)
        permissions = [r for r in records if r["type"] == "permission"]
        self.assertEqual(len(permissions), 1)
        self.assertEqual(permissions[0]["id"], "per_9")
        self.assertEqual(permissions[0]["tool"], "bash")
        self.assertEqual(permissions[0]["session"], session)
        self.assertEqual(permissions[0]["input"]["resources"], ["rm -rf /tmp/x"])
        self.assertEqual(permissions[0]["message"], "Run rm?")

    def test_approvals_from_subagents_count_and_strangers_do_not(self):
        self.gateway.start_event_stream()
        self.gateway.ensure_session()
        session = self.gateway.session_id
        FakeOpenCode.parents = {"ses_child": session, "ses_grandchild": "ses_child"}
        FakeOpenCode.next_events = [
            sse_event("session.step.started", session),
            sse_event("permission.asked", "ses_grandchild", id="per_sub", action="edit", resources=["a.py"]),
            sse_event("permission.asked", "ses_other", id="per_other", action="bash", resources=["ls"]),
            sse_event("session.execution.succeeded", session),
        ]
        records = []
        self.gateway.ask("go", timeout=8.0, emit=records.append)
        self.assertEqual([r["id"] for r in records if r["type"] == "permission"], ["per_sub"])

    def test_a_question_form_is_emitted_and_settled(self):
        self.gateway.start_event_stream()
        self.gateway.ensure_session()
        session = self.gateway.session_id
        fields = [{"key": "target", "type": "string", "options": [{"value": "a", "label": "A"}]}]
        FakeOpenCode.next_events = [
            sse_event("session.step.started", session),
            sse_event("form.created", session, id="frm_1", title="Which target?", fields=fields),
            sse_event("form.replied", session, id="frm_1"),
            sse_event("session.execution.succeeded", session),
        ]
        records = []
        self.gateway.ask("go", timeout=8.0, emit=records.append)
        forms = [r for r in records if r["type"] == "form"]
        self.assertEqual(len(forms), 1)
        self.assertEqual((forms[0]["id"], forms[0]["title"], forms[0]["fields"]), ("frm_1", "Which target?", fields))
        self.assertIn({"type": "form_settled", "id": "frm_1", "outcome": "answered"}, records)

    def test_form_reply_posts_the_answer(self):
        self.gateway.ensure_session()
        result = self.gateway.form_reply("frm_1", {"target": "a"})
        self.assertTrue(result["replied"], result)
        self.assertEqual(self.posts("/form/frm_1/reply")[0]["body"], {"answer": {"target": "a"}})

    # -- contract: every body matches the pinned OpenCode spec ------------------------

    def test_the_fake_rejects_a_field_opencode_would_drop(self):
        """Guards the guard: the old `directory` body must count as a violation."""
        self.assertTrue(opencode_contract.validate(CONTRACT, "session.create", {"title": "x", "directory": "/tmp"}))
        self.assertTrue(opencode_contract.validate(CONTRACT, "session.prompt", {"text": "x", "parts": []}))
        self.assertTrue(opencode_contract.validate(CONTRACT, "session.permission.reply", {"decision": "allow"}))
        self.assertEqual(opencode_contract.validate(
            CONTRACT, "session.create", {"title": "x", "location": {"directory": "/tmp"}}), [])

    def test_every_operation_the_gateway_calls_is_pinned(self):
        source = (WORKERS / "opencode_gateway.py").read_text()
        called = set(re.findall(r'_(?:get|post)\(\s*f?"(/api/[^"?]*)', source))
        pinned = [re.compile("^" + re.sub(r"\{[^}]+\}", "[^/]+", entry["path"]) + "$")
                  for entry in CONTRACT["operations"].values()]
        for path in called:
            concrete = re.sub(r"\{[^}]+\}", "x", path)
            self.assertTrue(any(pattern.match(concrete) for pattern in pinned), path)
        self.assertEqual(set(CONTRACT["operations"]), set(opencode_contract.OPERATIONS))
        self.assertEqual(opencode_contract.problems({"paths": {}, "components": {}})[0][:9], "operation")

    # -- BUG-017: catalog autospawn + recovery ---------------------------------------

    def test_catalog_calls_ensure_up_and_fail_fast_without_autospawn(self):
        dead = Gateway(base_url="http://127.0.0.1:1", password="pw",
                       autospawn=False, session_file=self.session_file)
        for call in (dead.commands, dead.models, dead.agents):
            with self.assertRaises(gateway_module.GatewayError):
                call()

    # A server left on the user's port by an isolated test locked their own Navi out:
    # Stop started it, the password differed, and every later call tried to start another.

    def test_stop_never_starts_a_server(self):
        dead = Gateway(base_url="http://127.0.0.1:1", password="pw", autospawn=True,
                       session_file=self.session_file)
        with mock.patch.object(Gateway, "spawn") as spawn:
            self.assertFalse(dead.interrupt())
        spawn.assert_not_called()

    def test_a_server_that_refuses_the_password_is_named_and_not_raced(self):
        class Refusing(BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(401)
                self.send_header("Content-Length", "0")
                self.end_headers()

            def log_message(self, *unused):
                pass

        server = ThreadingHTTPServer(("127.0.0.1", 0), Refusing)
        server.daemon_threads = True
        threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            gateway = Gateway(base_url=f"http://127.0.0.1:{server.server_port}", password="wrong",
                              autospawn=True, session_file=self.session_file)
            with mock.patch.object(Gateway, "spawn") as spawn:
                with self.assertRaisesRegex(gateway_module.GatewayError, "refused"):
                    gateway.ensure_up()
            spawn.assert_not_called()
        finally:
            server.shutdown()
            server.server_close()

    def test_an_isolated_station_never_starts_or_reaches_the_users_server(self):
        with (mock.patch.dict(os.environ, {"NAVI_ISOLATED": "1"}),
              mock.patch.object(Gateway, "spawn") as spawn,
              mock.patch.object(gateway_module.urllib.request, "urlopen") as opened):
            os.environ.pop("NAVI_OPENCODE_URL", None)
            gateway = Gateway(password="pw", autospawn=True, session_file=self.session_file)
            self.assertFalse(gateway.reachable())
            self.assertFalse(gateway.interrupt())
            with self.assertRaises(gateway_module.GatewayError):
                gateway.ensure_up()
            opened.assert_not_called()
            spawn.assert_not_called()
            # A named server may be used, but still is never started.
            self.assertFalse(Gateway(base_url="http://127.0.0.1:1", password="pw", autospawn=True,
                                     session_file=self.session_file).autospawn)

    # Navi and the stack's bridge each started their own kind of server, and whichever
    # reached the port first decided how the other one's traffic left the machine.

    def _spawn_with_systemctl(self, gateway, returncode=0):
        finished = subprocess.CompletedProcess([], returncode)
        with (mock.patch.object(gateway_module.shutil, "which", side_effect=lambda name: f"/bin/{name}"),
              mock.patch.object(gateway_module.subprocess, "run", return_value=finished) as run,
              mock.patch.object(gateway_module.subprocess, "Popen") as popen,
              mock.patch.object(Gateway, "_wait_reachable") as waited):
            gateway.spawn()
        return run, popen, waited

    def test_a_stopped_server_is_started_through_the_shared_unit(self):
        gateway = Gateway(password="pw", autospawn=True, session_file=self.session_file)
        run, popen, waited = self._spawn_with_systemctl(gateway)
        self.assertEqual(run.call_args.args[0], ["systemctl", "--user", "start", gateway_module.SERVER_UNIT])
        popen.assert_not_called()
        waited.assert_called_once()

    def test_without_the_unit_a_plain_server_is_the_fallback(self):
        gateway = Gateway(password="pw", autospawn=True, session_file=self.session_file)
        with mock.patch("builtins.open", mock.mock_open()):
            _, popen, _ = self._spawn_with_systemctl(gateway, returncode=5)
        self.assertEqual(popen.call_args.args[0][:2], ["opencode", "serve"])

    def test_a_server_on_another_url_never_touches_the_shared_unit(self):
        gateway = Gateway(base_url="http://127.0.0.1:1", password="pw", autospawn=True,
                          session_file=self.session_file)
        with mock.patch("builtins.open", mock.mock_open()):
            run, popen, _ = self._spawn_with_systemctl(gateway)
        run.assert_not_called()
        popen.assert_called_once()

    # Sending the server's traffic through a dead proxy pool left every turn silent.
    def test_the_shared_unit_runs_the_server_directly_and_comes_back(self):
        spec = importlib.util.spec_from_file_location("navi_install", Path(__file__).with_name("install.py"))
        installer = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(installer)
        unit = installer.opencode_unit("/bin/opencode", "/bin")
        self.assertIn("ExecStart=/bin/opencode serve --service --hostname 127.0.0.1 --port 49374\n", unit)
        self.assertIn("Restart=always\n", unit)
        self.assertNotIn("proxychains", unit)
        self.assertIn("/dev/tcp/127.0.0.1/49374", unit.split("ExecCondition=")[1])
        self.assertEqual(gateway_module.DEFAULT_URL.rsplit(":", 1)[-1], "49374")

    # The unit's condition decides whether it starts; a wrong exit code would either race
    # a running server or leave the machine with none.
    def test_the_condition_steps_aside_only_when_the_port_answers(self):
        spec = importlib.util.spec_from_file_location("navi_install", Path(__file__).with_name("install.py"))
        installer = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(installer)
        line = next(item for item in installer.opencode_unit("/bin/opencode", "/bin").splitlines()
                    if item.startswith("ExecCondition="))
        script = re.search(r"-c '(.*)'$", line).group(1)
        listener = socket.socket()
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        port = listener.getsockname()[1]
        probe = script.replace("49374", str(port))
        self.assertEqual(subprocess.run(["/usr/bin/bash", "-c", probe]).returncode, 1)
        listener.close()
        self.assertEqual(subprocess.run(["/usr/bin/bash", "-c", probe]).returncode, 0)

    def test_catalog_recovers_after_restart(self):
        port = self.server.server_port
        self.server.shutdown()
        self.server.server_close()
        replacement = ThreadingHTTPServer(("127.0.0.1", port), FakeOpenCode)
        replacement.sse_condition = threading.Condition()
        replacement.pending_events = []
        replacement.daemon_threads = True
        threading.Thread(target=replacement.serve_forever, daemon=True).start()
        try:
            recovered = Gateway(base_url=f"http://127.0.0.1:{port}", password="pw",
                                session_file=self.session_file)
            names = [item["name"] for item in recovered.commands()]
            self.assertEqual(names, ["review", "init"])
        finally:
            replacement.shutdown()
            replacement.server_close()
            self.server = ThreadingHTTPServer(("127.0.0.1", 0), FakeOpenCode)
            self.server.sse_condition = threading.Condition()
            self.server.pending_events = []
            self.server.daemon_threads = True
            threading.Thread(target=self.server.serve_forever, daemon=True).start()


class CliTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        FakeOpenCode.requests = []
        FakeOpenCode.session_counter = 0
        FakeOpenCode.fail_interrupts = 0
        FakeOpenCode.reject_files = False
        FakeOpenCode.honor_directory = True
        FakeOpenCode.next_events = None
        FakeOpenCode.contract_errors = []
        FakeOpenCode.pending_permissions = []
        FakeOpenCode.parents = {}
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), FakeOpenCode)
        cls.server.sse_condition = threading.Condition()
        cls.server.pending_events = []
        cls.server.daemon_threads = True
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()

    def tearDown(self):
        self.assertEqual(FakeOpenCode.contract_errors, [],
                         "the gateway sent a request OpenCode's spec does not accept")

    def run_cli(self, *args, data_dir):
        return subprocess.run(
            [sys.executable, str(WORKERS / "opencode_gateway.py"), *args],
            env={**os.environ,
                 "NAVI_OPENCODE_URL": f"http://127.0.0.1:{self.server.server_port}",
                 "NAVI_OPENCODE_PASSWORD": "pw",
                 "NAVI_DATA_DIR": str(data_dir)},
            capture_output=True, text=True, timeout=30,
        )

    def lines(self, result):
        return [json.loads(line) for line in result.stdout.splitlines() if line.strip()]

    def test_models_cli_prints_json(self):
        with tempfile.TemporaryDirectory() as folder:
            result = self.run_cli("models", data_dir=folder)
        self.assertEqual(result.returncode, 0, result.stderr)
        payload = json.loads(result.stdout)
        self.assertEqual(payload[0]["providerID"], "commandcode")

    def test_ask_cli_emits_done_json_with_usage_and_identity(self):
        with tempfile.TemporaryDirectory() as folder:
            result = self.run_cli("ask", "hi", "--stream", data_dir=folder)
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = self.lines(result)
        done = lines[-1]
        self.assertEqual(done["type"], "done")
        self.assertEqual(done["text"], "echo: hi")
        # BUG-001: exactly one final tool record per call id
        tools = [line for line in lines if line.get("type") == "tool"]
        self.assertEqual(len(tools), 1)
        self.assertEqual(tools[0]["id"], "call_1")
        self.assertEqual(tools[0]["name"], "shell")  # name from input.started
        self.assertEqual(tools[0]["status"], "completed")
        self.assertTrue(tools[0]["final"])
        # reasoning + usage + actual identity ride the wire (BUG-009/010)
        self.assertTrue(any(line.get("type") == "reasoning" for line in lines))
        self.assertEqual(done["usage"]["tokens"]["reasoning"], 2)
        self.assertEqual(done["usage"]["cost"], 0.002)
        self.assertEqual(done["session_usage"]["totalTokens"], 17)
        self.assertEqual(done["finish"], "stop")
        self.assertEqual(done["actual_model"]["id"], "deepseek/deepseek-v4.1-flash")
        self.assertEqual(done["actual_agent"], "navi")

    def test_slash_help_posts_the_real_command(self):
        with tempfile.TemporaryDirectory() as folder:
            result = self.run_cli("slash", "/help", data_dir=folder)
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = self.lines(result)
        self.assertEqual(lines[-1]["type"], "done")
        self.assertEqual(lines[-1]["text"], "ran help")
        posted = [entry for entry in FakeOpenCode.requests
                  if entry["path"].endswith("/command")]
        self.assertTrue(posted)
        self.assertEqual(posted[-1]["body"], {"name": "help", "text": ""})

    def test_interrupt_cli_reports_flag(self):
        with tempfile.TemporaryDirectory() as folder:
            result = self.run_cli("interrupt", data_dir=folder)
        self.assertEqual(result.returncode, 0, result.stderr)
        payload = json.loads(result.stdout)
        self.assertIn("interrupted", payload)

    def test_permission_cli(self):
        FakeOpenCode.pending_permissions = [{"id": "per_5", "sessionID": "ses_waiting",
                                             "action": "bash", "resources": ["ls"]}]
        with tempfile.TemporaryDirectory() as folder:
            result = self.run_cli("permission", "--id", "per_5",
                                  "--decision", "allow", data_dir=folder)
        FakeOpenCode.pending_permissions = []
        self.assertEqual(result.returncode, 0, result.stderr)
        payload = json.loads(result.stdout)
        self.assertTrue(payload["replied"], payload)
        self.assertEqual(payload["endpoint"], "/api/session/ses_waiting/permission/per_5/reply")

    def test_permission_cli_without_a_waiting_session_says_so(self):
        """It used to create a session just to send the decision nowhere."""
        with tempfile.TemporaryDirectory() as folder:
            result = self.run_cli("permission", "--id", "per_6", "--decision", "deny", data_dir=folder)
        self.assertEqual(result.returncode, 0, result.stderr)
        payload = json.loads(result.stdout)
        self.assertFalse(payload["replied"])
        self.assertIn("No OpenCode session", payload["error"])

    def test_form_cli_posts_the_answer(self):
        with tempfile.TemporaryDirectory() as folder:
            result = self.run_cli("form", "--id", "frm_2", "--session", "ses_fake1",
                                  "--answer", '{"target": "b"}', data_dir=folder)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(json.loads(result.stdout)["replied"])
        posted = [entry for entry in FakeOpenCode.requests if entry["path"].endswith("/form/frm_2/reply")]
        self.assertEqual(posted[-1]["body"], {"answer": {"target": "b"}})

    def test_directory_flag_scopes_sessions(self):
        with tempfile.TemporaryDirectory() as folder:
            first = self.run_cli("new", "--directory", "/tmp/projX", data_dir=folder)
            self.assertEqual(first.returncode, 0, first.stderr)
            stored = json.loads((Path(folder) / "opencode-session").read_text())
            self.assertIn("/tmp/projX", stored)
            created = [entry for entry in FakeOpenCode.requests
                       if entry["path"] == "/api/session"]
            self.assertEqual(created[-1]["body"]["location"], {"directory": "/tmp/projX"})


if __name__ == "__main__":
    unittest.main()
