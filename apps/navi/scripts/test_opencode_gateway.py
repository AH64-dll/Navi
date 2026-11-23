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
import urllib.parse
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
    active_sessions = {}
    session_list = []
    saved_permissions = []
    mcp_servers = {}
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

    def route(self):
        return self.path.split("?", 1)[0]

    def checked(self, method):
        """(body, ok): record the request, then reject a body OpenCode would drop."""
        body = self.body() if method in ("POST", "PATCH", "PUT") else None
        self.record({"path": self.path, "body": body, "method": method})
        operation = operation_for(method, self.path)
        errors = ([f"{method} {self.path} is not in the contract"] if operation is None
                  else opencode_contract.validate(CONTRACT, operation, body))
        if errors:
            FakeOpenCode.contract_errors.extend(errors)
            self.send_json({"error": errors}, 400)
            return body, False
        return body, True

    def push_events(self, events):
        with self.server.sse_condition:
            for queue in self.server.sse_queues:
                queue.extend(events)
            self.server.sse_condition.notify_all()

    def serve_events(self):
        """GET /api/event: hold the SSE stream, draining this connection's queue.
        A queue per connection, so a client that has died cannot steal the events
        pushed for the next one."""
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        mine = []
        with self.server.sse_condition:
            self.server.sse_queues.append(mine)
        try:
            while True:
                with self.server.sse_condition:
                    self.server.sse_condition.wait(timeout=0.2)
                    batch, mine[:] = list(mine), []
                for event in batch:
                    self.wfile.write(f"data: {json.dumps(event)}\n\n".encode())
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, OSError):
            return
        finally:
            with self.server.sse_condition:
                self.server.sse_queues[:] = [queue for queue in self.server.sse_queues
                                             if queue is not mine]

    def do_GET(self):
        route = self.route()
        self.record({"path": self.path, "body": None, "method": "GET"})
        if operation_for("GET", self.path) is None:
            FakeOpenCode.contract_errors.append(f"GET {self.path} is not in the contract")
        if route == "/api/event":
            self.serve_events()
        elif route == "/api/permission/request":
            self.send_json(FakeOpenCode.pending_permissions)
        elif route == "/api/permission/saved":
            self.send_json(FakeOpenCode.saved_permissions)
        elif route == "/api/agent":
            self.send_json([
                {"id": "build", "name": "Build", "mode": "primary", "hidden": False},
                {"id": "general", "name": "General", "mode": "subagent", "hidden": False},
                {"id": "navi", "name": "navi", "mode": "primary", "hidden": False},
            ])
        elif route.startswith("/api/agent/"):
            self.send_json({"id": route.rsplit("/", 1)[-1], "name": "Build",
                            "mode": "primary", "hidden": False})
        elif route == "/api/model/default":
            self.send_json({"id": "deepseek/deepseek-v4.1-flash",
                            "modelID": "deepseek/deepseek-v4.1-flash",
                            "providerID": "commandcode", "name": "DeepSeek", "type": "chat"})
        elif route == "/api/model":
            self.send_json([
                {"providerID": "opencode", "modelID": "glm-5.3-flash-free", "name": "GLM Free",
                 "type": "chat", "settings": {"apiKey": "public"}},
                {"providerID": "commandcode", "modelID": "deepseek/deepseek-v4.1-flash",
                 "name": "DeepSeek", "type": "chat", "settings": {"apiKey": "sk-model"}},
                {"providerID": "x", "modelID": "embed", "name": "Embed", "type": "embedding",
                 "settings": {"apiKey": "sk-embed"}},
            ])
        elif route == "/api/command":
            self.send_json([{"name": "review", "description": "review changes"},
                            {"name": "init", "description": "guided AGENTS.md setup"}])
        elif route == "/api/skill":
            self.send_json([{"id": "opencode", "name": "OpenCode", "description": "about OpenCode"}])
        elif route == "/api/provider":
            self.send_json([{"id": "commandcode", "name": "CommandCode", "activation": "enabled",
                             "settings": {"apiKey": "sk-plaintext"}}])
        elif route == "/api/credential":
            self.send_json([{"id": "cred_1", "integrationID": "commandcode", "label": "CommandCode",
                             "active": True, "value": {"type": "key", "key": "user-plaintext"}}])
        elif route == "/api/integration":
            self.send_json([{"id": "commandcode", "name": "CommandCode",
                             "methods": [{"type": "key"}], "connections": []}])
        elif re.match(r"^/api/integration/[^/]+/connect/oauth/[^/]+$", route):
            self.send_json({"status": "pending", "time": {"created": 1, "expires": 2}})
        elif route == "/api/mcp":
            self.send_json([{"name": "brain-memory", "status": {"status": "connected"}}])
        elif route == "/api/config":
            self.send_json([{"type": "document", "path": "/home/fake/opencode.json",
                             "info": {"provider": {"apiKey": "sk-plaintext"}}}])
        elif route == "/api/plugin":
            self.send_json([{"id": "opencode.config.mcp", "source": {"type": "builtin"},
                             "features": {"server": True}, "state": {"status": "active"}}])
        elif route == "/api/experimental/usage/limits":
            self.send_json({"limits": []})
        elif route == "/api/experimental/session/stats":
            self.send_json({"sessions": 3})
        elif route == "/api/info":
            self.send_json({"version": "2.2.5", "pid": 1, "urls": [],
                            "paths": {"tmp": "/tmp/opencode"}})
        elif route == "/api/session/active":
            self.send_json(FakeOpenCode.active_sessions)
        elif route == "/api/session":
            self.send_json(FakeOpenCode.session_list)
        elif re.match(r"^/api/session/[^/]+/message$", route):
            session = route.split("/")[3]
            reply = FakeOpenCode.replies.get(session)
            messages = [] if not reply else [{
                "id": f"msg_{session}_{reply['n']}", "time": {"completed": True},
                "content": [{"type": "text", "text": reply["text"]}],
            }]
            self.send_json(messages)
        elif re.match(r"^/api/session/[^/]+$", route):
            session = route.rsplit("/", 1)[-1]
            self.send_json({"id": session, "parentID": FakeOpenCode.parents.get(session),
                            "agent": "build",
                            "model": {"providerID": "opencode", "id": "glm-5.3-flash-free",
                                      "variant": "default"},
                            "cost": 0.5, "tokens": {"input": 1, "output": 1},
                            "time": {"created": 1, "updated": 2},
                            "location": {"directory": FakeOpenCode.last_directory or "/home/fake"}})
        else:
            self.send_json(None, 404)

    def do_POST(self):
        path = self.path
        body, ok = self.checked("POST")
        if not ok:
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
        elif re.match(r"^/api/session/[^/]+/fork$", path):
            FakeOpenCode.session_counter += 1
            self.send_json({"id": f"ses_fork{FakeOpenCode.session_counter}",
                            "title": "forked", "agent": "build",
                            "model": {"providerID": "opencode", "id": "glm-5.3-flash-free"},
                            "cost": 0, "tokens": {"input": 0, "output": 0},
                            "time": {"created": 1, "updated": 2},
                            "location": {"directory": "/home/fake"}})
        elif re.match(r"^/api/session/[^/]+/compact$", path):
            self.send_json({"id": "msg_compact", "sessionID": path.split("/")[3],
                            "type": "compaction"})
        elif re.match(r"^/api/credential/[^/]+/activate$", path):
            self.send_json(None)
        elif re.match(r"^/api/integration/[^/]+/connect/oauth$", path):
            self.send_json({"attemptID": "att_1", "url": "https://example.test/oauth",
                            "instructions": "Sign in", "mode": "auto",
                            "time": {"created": 1, "expires": 2}})
        elif re.match(r"^/api/integration/[^/]+/connect/oauth/[^/]+/complete$", path):
            self.send_json(None)
        elif re.match(r"^/api/integration/[^/]+/connect/key$", path):
            self.send_json(None)
        elif re.match(r"^/api/experimental/mcp/[^/]+/(connect|disconnect)$", path):
            self.send_json(None)
        elif path == "/api/plugin/update":
            self.send_json(None)
        else:
            self.send_json(None, 404)

    def do_PATCH(self):
        body, ok = self.checked("PATCH")
        if not ok:
            return
        route = self.route()
        if re.match(r"^/api/session/[^/]+$", route):
            self.send_json(None)
        elif re.match(r"^/api/permission/saved/[^/]+$", route):
            self.send_json({"id": route.rsplit("/", 1)[-1], "projectID": "p",
                            "action": (body or {}).get("action") or "bash",
                            "resource": (body or {}).get("resource") or "ls",
                            "time": {"created": 1, "updated": 2}})
        else:
            self.send_json(None, 404)

    def do_PUT(self):
        body, ok = self.checked("PUT")
        if not ok:
            return
        route = self.route()
        if re.match(r"^/api/experimental/mcp/[^/]+$", route):
            FakeOpenCode.mcp_servers[route.rsplit("/", 1)[-1]] = body
            self.send_json(None)
        else:
            self.send_json(None, 404)

    def do_DELETE(self):
        _, ok = self.checked("DELETE")
        if not ok:
            return
        route = self.route()
        if re.match(r"^/api/session/[^/]+/form/[^/]+$", route):
            self.send_json(None)
        elif re.match(r"^/api/session/[^/]+$", route):
            self.send_json(None)
        elif re.match(r"^/api/experimental/mcp/[^/]+$", route):
            FakeOpenCode.mcp_servers.pop(route.rsplit("/", 1)[-1], None)
            self.send_json(None)
        elif re.match(r"^/api/permission/saved/[^/]+$", route):
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
        FakeOpenCode.active_sessions = {}
        FakeOpenCode.session_list = [
            {"id": "ses_list1", "title": "Navi · projA", "cost": 0.5,
             "tokens": {"input": 1, "output": 1}, "time": {"created": 1, "updated": 2},
             "location": {"directory": "/home/fake"}},
            {"id": "ses_list2", "title": "Older", "cost": 0.1,
             "tokens": {"input": 1, "output": 1}, "time": {"created": 1, "updated": 1},
             "location": {"directory": "/home/fake"}},
        ]
        FakeOpenCode.saved_permissions = [
            {"id": "psv_1", "projectID": "p", "action": "bash", "resource": "ls",
             "time": {"created": 1, "updated": 2}}]
        FakeOpenCode.mcp_servers = {}
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), FakeOpenCode)
        self.server.sse_condition = threading.Condition()
        self.server.sse_queues = []
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
        return [entry for entry in FakeOpenCode.requests
                if entry["path"].endswith(suffix) and entry.get("method", "POST") == "POST"]

    def session_map(self):
        return json.loads(Path(self.session_file).read_text())

    # -- pre-existing behaviour -------------------------------------------------

    def test_ask_sets_model_agent_and_returns_the_reply(self):
        reply = self.gateway.ask("hello")
        self.assertEqual(reply, "echo: hello")
        self.assertIn("/api/session/ses_fake1/model", self.paths())
        self.assertEqual(self.posts("/model")[0]["body"],
                         {"model": {"providerID": "commandcode", "id": "deepseek/deepseek-v4.1-flash"}})
        self.assertEqual(self.posts("/agent")[0]["body"], {"agent": "navi"})
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

    # -- Phase 1: sessions --------------------------------------------------------

    def test_sessions_lists_the_projects_sessions_and_the_running_ones(self):
        FakeOpenCode.active_sessions = {"ses_list2": {"type": "running"}}
        directory = str(Path(self.folder.name) / "projA")
        gateway = Gateway(base_url=self.gateway.base_url, password="pw",
                          session_file=self.session_file, directory=directory)
        listing = gateway.sessions(search="navi", archived=False, limit=5)
        self.assertEqual([item["id"] for item in listing["sessions"]],
                         ["ses_list1", "ses_list2"])
        self.assertEqual(listing["active"], {"ses_list2": {"type": "running"}})
        query = next(entry["path"] for entry in FakeOpenCode.requests
                     if entry["path"].startswith("/api/session?"))
        self.assertIn("order=desc", query)
        self.assertIn("search=navi", query)
        self.assertIn("archived=false", query)
        self.assertIn("limit=5", query)
        self.assertIn(urllib.parse.urlencode({"directory": os.path.abspath(directory)}), query)

    def test_sessions_asks_for_no_archive_filter_by_default(self):
        self.gateway.sessions()
        query = next(entry["path"] for entry in FakeOpenCode.requests
                     if entry["path"].startswith("/api/session?"))
        self.assertNotIn("archived", query)
        self.assertNotIn("search", query)

    def test_session_get_returns_the_session(self):
        info = self.gateway.session_info("ses_list1")
        self.assertEqual(info["id"], "ses_list1")
        self.assertEqual(info["agent"], "build")

    def test_session_messages_come_in_conversation_order(self):
        FakeOpenCode.replies["ses_list1"] = {"n": 1, "text": "hello"}
        messages = self.gateway.session_messages("ses_list1", limit=20)
        self.assertEqual(messages[0]["content"][0]["text"], "hello")
        path = next(entry["path"] for entry in FakeOpenCode.requests if "/message" in entry["path"])
        self.assertIn("order=asc", path)
        self.assertIn("limit=20", path)

    def test_session_select_stores_it_and_adopts_what_it_runs(self):
        directory = str(Path(self.folder.name) / "projA")
        gateway = Gateway(base_url=self.gateway.base_url, password="pw",
                          session_file=self.session_file, directory=directory)
        self.assertEqual(gateway.select_session("ses_list1"), "ses_list1")
        stored = self.session_map()[os.path.abspath(directory)]
        self.assertEqual(stored["id"], "ses_list1")
        self.assertEqual(stored["model"], "opencode/glm-5.3-flash-free")
        self.assertEqual(stored["agent"], "build")
        mark = len(FakeOpenCode.requests)
        gateway.ask("hello")
        self.assertEqual([entry["path"] for entry in FakeOpenCode.requests[mark:]
                          if entry["path"].endswith("/prompt")],
                         ["/api/session/ses_list1/prompt"])
        # Continuing a session must not silently reconfigure it.
        self.assertEqual([entry["path"] for entry in FakeOpenCode.requests[mark:]
                          if entry["path"].endswith(("/model", "/agent"))
                          and entry["method"] == "POST"], [])

    def test_session_select_refuses_an_id_the_server_does_not_know(self):
        with self.assertRaises(gateway_module.GatewayError):
            self.gateway.select_session("not-a-session")

    def test_session_rename_patches_the_title(self):
        self.assertEqual(self.gateway.session_rename("Renamed", "ses_list1"),
                         {"session": "ses_list1", "title": "Renamed"})
        patch = FakeOpenCode.requests[-1]
        self.assertEqual((patch["method"], patch["path"]), ("PATCH", "/api/session/ses_list1"))
        self.assertEqual(patch["body"], {"title": "Renamed"})

    def test_session_fork_sends_before_only_when_asked(self):
        forked = self.gateway.session_fork("ses_list1")
        self.assertTrue(forked["id"].startswith("ses_fork"))
        self.assertEqual(FakeOpenCode.requests[-1]["body"], {})
        self.gateway.session_fork("ses_list1", before="msg_9")
        self.assertEqual(FakeOpenCode.requests[-1]["body"], {"before": "msg_9"})

    def test_session_archive_patches_the_flag(self):
        self.gateway.session_archive(True, "ses_list1")
        self.assertEqual(FakeOpenCode.requests[-1]["body"], {"archived": True})
        self.gateway.session_archive(False, "ses_list1")
        self.assertEqual(FakeOpenCode.requests[-1]["body"], {"archived": False})

    def test_session_delete_forgets_the_stored_id(self):
        session = self.gateway.ensure_session()
        self.assertEqual(self.session_map()["default"]["id"], session)
        self.assertEqual(self.gateway.session_delete(session),
                         {"deleted": session, "forgotten": True})
        self.assertEqual((FakeOpenCode.requests[-1]["method"],
                          FakeOpenCode.requests[-1]["path"]), ("DELETE", f"/api/session/{session}"))
        self.assertEqual(self.session_map(), {})
        self.assertIsNone(self.gateway.session_id)

    def test_session_compact_posts_an_empty_body(self):
        self.gateway.session_compact("ses_list1")
        compact = FakeOpenCode.requests[-1]
        self.assertEqual(compact["path"], "/api/session/ses_list1/compact")
        self.assertEqual(compact["body"], {})

    # -- Phase 2: settings --------------------------------------------------------

    def test_catalog_serves_every_allowlisted_name(self):
        for name in sorted(gateway_module.CATALOG):
            listing = self.gateway.catalog(name)
            self.assertEqual(listing["catalog"], name)
            self.assertIn("items", listing)

    def test_catalog_redacts_credentials_providers_and_config(self):
        self.assertEqual(self.gateway.catalog("credentials")["items"][0]["value"]["key"],
                         "[redacted]")
        self.assertEqual(self.gateway.catalog("providers")["items"][0]["settings"]["apiKey"],
                         "[redacted]")
        self.assertEqual(self.gateway.catalog("config")["items"][0]["info"]["provider"]["apiKey"],
                         "[redacted]")
        # A model's own settings can hold a key for a custom provider.
        models = self.gateway.catalog("models")["items"]
        self.assertEqual([item["settings"]["apiKey"] for item in models],
                         ["[redacted]", "[redacted]", "[redacted]"])
        # A list with nothing secret is passed through untouched.
        self.assertEqual(self.gateway.catalog("mcp")["items"][0]["name"], "brain-memory")

    def test_catalog_rejects_a_name_outside_the_allowlist(self):
        with self.assertRaisesRegex(gateway_module.GatewayError, "unknown catalog"):
            self.gateway.catalog("/api/agent")

    def test_catalog_scopes_project_lists_by_directory(self):
        directory = str(Path(self.folder.name) / "projA")
        gateway = Gateway(base_url=self.gateway.base_url, password="pw",
                          session_file=self.session_file, directory=directory)
        gateway.catalog("agents")
        path = FakeOpenCode.requests[-1]["path"]
        self.assertIn(urllib.parse.urlencode({"location[directory]": os.path.abspath(directory)}),
                      path)

    def test_switch_model_and_agent_target_the_session(self):
        self.gateway.ensure_session()
        mark = len(FakeOpenCode.requests)
        self.assertEqual(self.gateway.switch_model("opencode/glm-5.3-flash-free")["model"],
                         {"providerID": "opencode", "id": "glm-5.3-flash-free"})
        self.assertEqual(FakeOpenCode.requests[-1]["path"], "/api/session/ses_fake1/model")
        self.assertEqual(self.gateway.switch_agent("build"),
                         {"session": "ses_fake1", "agent": "build"})
        self.assertEqual(FakeOpenCode.requests[-1]["path"], "/api/session/ses_fake1/agent")
        stored = self.session_map()["default"]
        self.assertEqual((stored["model"], stored["agent"]),
                         ("opencode/glm-5.3-flash-free", "build"))
        # The next turn does not re-send what the user just chose.
        self.gateway.ask("hello")
        self.assertEqual([entry["path"] for entry in FakeOpenCode.requests[mark:]
                          if entry["path"].endswith(("/model", "/agent"))
                          and entry["method"] == "POST"],
                         ["/api/session/ses_fake1/model", "/api/session/ses_fake1/agent"])

    def test_mcp_add_puts_the_config_and_remove_deletes_it(self):
        config = {"type": "local", "command": ["npx", "-y", "server"]}
        self.assertEqual(self.gateway.mcp_add("firefox", config),
                         {"server": "firefox", "added": True})
        put = FakeOpenCode.requests[-1]
        self.assertEqual((put["method"], put["path"]), ("PUT", "/api/experimental/mcp/firefox"))
        self.assertEqual(put["body"], {"config": config})
        self.assertEqual(FakeOpenCode.mcp_servers["firefox"], {"config": config})
        self.assertEqual(self.gateway.mcp_remove("firefox"),
                         {"server": "firefox", "removed": True})
        self.assertEqual(FakeOpenCode.requests[-1]["method"], "DELETE")
        self.assertNotIn("firefox", FakeOpenCode.mcp_servers)

    def test_mcp_add_without_a_config_says_what_it_needs(self):
        with self.assertRaisesRegex(gateway_module.GatewayError, "config object"):
            self.gateway.mcp_add("firefox", None)

    def test_mcp_connect_and_disconnect(self):
        self.assertEqual(self.gateway.mcp_connect("firefox")["connected"], True)
        self.assertEqual(FakeOpenCode.requests[-1]["path"],
                         "/api/experimental/mcp/firefox/connect")
        self.assertEqual(self.gateway.mcp_connect("firefox", connect=False)["connected"], False)
        self.assertEqual(FakeOpenCode.requests[-1]["path"],
                         "/api/experimental/mcp/firefox/disconnect")

    def test_accounts_and_saved_approvals(self):
        self.assertEqual(self.gateway.credential_activate("cred_1")["activated"], True)
        self.assertEqual(FakeOpenCode.requests[-1]["path"], "/api/credential/cred_1/activate")
        self.assertEqual(self.gateway.permission_saved_remove("psv_1"), {"removed": "psv_1"})
        self.assertEqual(FakeOpenCode.requests[-1]["path"], "/api/permission/saved/psv_1")
        attempt = self.gateway.integration_oauth_connect("commandcode", {"methodID": "key"})
        self.assertEqual(attempt["attemptID"], "att_1")
        self.assertEqual(FakeOpenCode.requests[-1]["path"],
                         "/api/integration/commandcode/connect/oauth")
        status = self.gateway.integration_oauth_status("commandcode", "att_1")
        self.assertEqual(status["status"], "pending")
        self.assertTrue(self.gateway.integration_oauth_complete(
            "commandcode", "att_1", {"code": "123"})["completed"])
        self.assertEqual(FakeOpenCode.requests[-1]["body"], {"code": "123"})
        self.assertTrue(self.gateway.integration_connect_key(
            "commandcode", {"key": "sk-live"})["connected"])
        self.assertEqual(FakeOpenCode.requests[-1]["body"], {"key": "sk-live"})
        self.assertEqual(self.gateway.server_info()["version"], "2.2.5")

    def test_an_oauth_connect_without_a_method_says_what_it_needs(self):
        with self.assertRaisesRegex(gateway_module.GatewayError, "methodID"):
            self.gateway.integration_oauth_connect("commandcode", {})
        with self.assertRaisesRegex(gateway_module.GatewayError, '"key"'):
            self.gateway.integration_connect_key("commandcode", {})

    # -- Phase 3: approvals and questions -----------------------------------------

    def test_form_cancel_deletes_the_form_and_forgets_it(self):
        self.gateway.ensure_session()
        self.gateway._forms["frm_1"] = {"id": "frm_1"}
        self.assertEqual(self.gateway.form_cancel("frm_1"),
                         {"cancelled": "frm_1", "session": "ses_fake1"})
        self.assertEqual((FakeOpenCode.requests[-1]["method"], FakeOpenCode.requests[-1]["path"]),
                         ("DELETE", "/api/session/ses_fake1/form/frm_1"))
        self.assertEqual(self.gateway._forms, {})

    def test_a_pending_form_outlasts_the_silence_limit(self):
        """The turn waits for the user, so silence is not a failure while a form is open."""
        self.gateway._turn_started = self.gateway._last_activity = time.time()
        self.gateway._busy = True
        self.gateway._events_alive = True
        self.gateway.session_id = "ses"
        self.gateway._forms = {"frm_1": {"id": "frm_1"}}
        self.gateway._latest_reply = lambda previous: None
        outcome = {}

        def wait():
            try:
                outcome["reply"] = self.gateway._wait_reply(set(), timeout=0.4)
            except gateway_module.GatewayError as error:
                outcome["error"] = str(error)

        thread = threading.Thread(target=wait)
        thread.start()
        try:
            time.sleep(1.2)  # three times the silence limit
            self.assertTrue(thread.is_alive(), "the turn was interrupted while a form waited")
            self.assertNotIn("error", outcome)
        finally:
            self.gateway._handle_event("form.replied", {"sessionID": "ses", "id": "frm_1"})
            self.gateway._ended_texts = ["answered"]
            self.gateway._busy = False
            thread.join(timeout=5)
        self.assertEqual(outcome.get("reply"), "answered")

    def test_a_pending_approval_is_work_until_it_is_replied_to(self):
        self.gateway._permissions = [{"id": "per_1"}]
        self.assertTrue(self.gateway._tool_running())
        self.gateway._handle_event("permission.replied", {"sessionID": "ses", "id": "per_1"})
        self.assertEqual(self.gateway._permissions, [])
        self.assertFalse(self.gateway._tool_running())

    def test_a_form_settled_event_clears_the_pending_form(self):
        self.gateway._forms["frm_2"] = {"id": "frm_2"}
        records = []
        self.gateway._emit = records.append
        self.gateway._handle_event("form.settled", {"sessionID": "ses", "id": "frm_2",
                                                    "outcome": "expired"})
        self.assertEqual(self.gateway._forms, {})
        self.assertEqual(records, [{"type": "form_settled", "id": "frm_2", "outcome": "expired"}])

    # -- contract: every body matches the pinned OpenCode spec ------------------------

    def test_the_fake_rejects_a_field_opencode_would_drop(self):
        """Guards the guard: the old `directory` body must count as a violation."""
        self.assertTrue(opencode_contract.validate(CONTRACT, "session.create", {"title": "x", "directory": "/tmp"}))
        self.assertTrue(opencode_contract.validate(CONTRACT, "session.prompt", {"text": "x", "parts": []}))
        self.assertTrue(opencode_contract.validate(CONTRACT, "session.permission.reply", {"decision": "allow"}))
        self.assertEqual(opencode_contract.validate(
            CONTRACT, "session.create", {"title": "x", "location": {"directory": "/tmp"}}), [])

    def test_every_operation_the_gateway_calls_is_pinned(self):
        """Every /api path the gateway names is a pinned operation. The fake server
        rejects an unpinned request at runtime; this catches a path no test reaches."""
        source = (WORKERS / "opencode_gateway.py").read_text()
        called = set(re.findall(r'"(/api/[^"]*)"', source))
        self.assertTrue(called)

        def shape(path):
            return re.sub(r"\{[^}]*\}", "x", path.split("?")[0]).rstrip("/")

        pinned = {shape(entry["path"]) for entry in CONTRACT["operations"].values()}
        for path in sorted(called):
            wanted = shape(path)
            self.assertTrue(
                any(candidate == wanted or candidate.startswith(wanted + "/")
                    for candidate in pinned),
                f"{path} is not a pinned operation")
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
        replacement.sse_queues = []
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
            self.server.sse_queues = []
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
        FakeOpenCode.active_sessions = {}
        FakeOpenCode.session_list = [
            {"id": "ses_list1", "title": "Navi · projA", "cost": 0.5,
             "tokens": {"input": 1, "output": 1}, "time": {"created": 1, "updated": 2},
             "location": {"directory": "/home/fake"}},
        ]
        FakeOpenCode.saved_permissions = []
        FakeOpenCode.mcp_servers = {}
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), FakeOpenCode)
        cls.server.sse_condition = threading.Condition()
        cls.server.sse_queues = []
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

    def test_ask_cli_with_session_continues_that_session(self):
        with tempfile.TemporaryDirectory() as folder:
            result = self.run_cli("ask", "hi", "--session", "ses_list1", data_dir=folder)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(result.stdout)["session"], "ses_list1")
            stored = json.loads((Path(folder) / "opencode-session").read_text())
            self.assertEqual(stored["default"]["id"], "ses_list1")
        prompts = [entry for entry in FakeOpenCode.requests if entry["path"].endswith("/prompt")]
        self.assertEqual(prompts[-1]["path"], "/api/session/ses_list1/prompt")

    def test_sessions_cli_prints_one_json_object(self):
        with tempfile.TemporaryDirectory() as folder:
            result = self.run_cli("sessions", "--limit", "5", data_dir=folder)
        self.assertEqual(result.returncode, 0, result.stderr)
        payload = json.loads(result.stdout)
        self.assertEqual([item["id"] for item in payload["sessions"]], ["ses_list1"])

    def test_catalog_cli_prints_the_list(self):
        with tempfile.TemporaryDirectory() as folder:
            result = self.run_cli("catalog", "agents", data_dir=folder)
        self.assertEqual(result.returncode, 0, result.stderr)
        payload = json.loads(result.stdout)
        self.assertEqual(payload["catalog"], "agents")
        self.assertEqual([item["id"] for item in payload["items"]], ["build", "general", "navi"])

    def test_mcp_add_cli_takes_the_config_object(self):
        config = {"type": "local", "command": ["npx", "-y", "server"]}
        with tempfile.TemporaryDirectory() as folder:
            result = self.run_cli("mcp_add", "--name", "firefox",
                                  "--payload", json.dumps(config), data_dir=folder)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), {"server": "firefox", "added": True})
        puts = [entry for entry in FakeOpenCode.requests
                if entry["method"] == "PUT" and entry["path"] == "/api/experimental/mcp/firefox"]
        self.assertEqual(puts[-1]["body"], {"config": config})

    def test_form_cancel_cli(self):
        with tempfile.TemporaryDirectory() as folder:
            result = self.run_cli("form_cancel", "--id", "frm_3", "--session", "ses_list1",
                                  data_dir=folder)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["cancelled"], "frm_3")
        self.assertEqual(FakeOpenCode.requests[-1]["path"],
                         "/api/session/ses_list1/form/frm_3")

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
