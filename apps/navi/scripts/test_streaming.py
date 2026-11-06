"""Live-stream correctness: navid + gateway + FakeOpenCode (BUG-094/098/099/015).

A turn that emits 500 deltas in 300 ms must reach a slow client as the exact
answer: the producer coalesces, `turn_state` is authoritative, and `done` clears
busy without a terminal event being seen. A second turn gets its own id, a
client that lost events resyncs from `events_from`, and the status contract
carries sections/versions so an idle poll stays cheap.
"""
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
import unittest
from http.server import ThreadingHTTPServer

from test_browser_live import Fixture
from test_opencode_gateway import FakeOpenCode, sse_event

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("NAVI_TEST_BINARY", "/tmp/navi-cargo-target/debug/navid"))

CHUNKS = 500
BATCHES = 10
BATCH_GAP = 0.03                       # 10 batches × 30 ms ≈ the plan's 300 ms
# No trailing whitespace: the gateway trims the final answer, so the exact
# concatenation is what the client must end up holding.
CHUNK_TEXT = [f"chunk{index:03d} " for index in range(CHUNKS - 1)] + [f"chunk{CHUNKS - 1:03d}"]
DELTA_TEXT = "".join(CHUNK_TEXT)
LEAN_SECTIONS = ("settings", "sessions", "projects", "workers", "browser",
                 "desktop", "voice", "active_session_tail", "cache")


def push_events(server, events):
    with server.sse_condition:
        server.pending_events.extend(events)
        server.sse_condition.notify_all()


def paced_events(server, session, text, batches=BATCHES, gap=BATCH_GAP):
    """One realistic turn whose 500 deltas land across ~300 ms."""
    chunks = CHUNK_TEXT
    push_events(server, [
        sse_event("session.step.started", session, agent="navi",
                  model={"id": "deepseek/deepseek-v4.1-flash",
                         "providerID": "commandcode"}),
        sse_event("session.step.streamed", session),
        sse_event("session.text.started", session),
    ])
    size = len(chunks) // batches
    for start in range(0, len(chunks), size):
        push_events(server, [
            sse_event("session.text.delta", session, delta=chunk)
            for chunk in chunks[start:start + size]
        ])
        time.sleep(gap)
    push_events(server, [
        sse_event("session.text.ended", session, text=text),
        sse_event("session.step.ended", session,
                  tokens={"input": 10, "output": 5, "reasoning": 0, "cache": 0},
                  cost=0.001, finish="stop"),
        sse_event("session.usage.updated", session, totalTokens=15, cost=0.004),
        sse_event("session.execution.succeeded", session),
    ])


class StreamingTests(unittest.TestCase):
    def setUp(self):
        FakeOpenCode.requests = []
        FakeOpenCode.replies = {}
        FakeOpenCode.session_counter = 0
        FakeOpenCode.message_counter = 0
        FakeOpenCode.reject_parts = False
        FakeOpenCode.honor_directory = True
        FakeOpenCode.next_events = None
        FakeOpenCode.last_directory = None
        self.opencode = ThreadingHTTPServer(("127.0.0.1", 0), FakeOpenCode)
        self.opencode.sse_condition = threading.Condition()
        self.opencode.pending_events = []
        self.opencode.daemon_threads = True
        threading.Thread(target=self.opencode.serve_forever, daemon=True).start()
        # Isolated mode refuses the real decision endpoint (BUG-014): give the
        # daemon a second local server that answers /health and /v1/systemone.
        self.chief = ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
        self.chief.daemon_threads = True
        threading.Thread(target=self.chief.serve_forever, daemon=True).start()
        self.temporary = tempfile.TemporaryDirectory(prefix="navi-streaming-")
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
        for _ in range(200):
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
        saved = self.call("settings", settings)
        self.assertTrue(saved["ok"], saved)

    def tearDown(self):
        self.process.terminate()
        self.process.wait(timeout=5)
        # The daemon leaves its panes running by design, and deleting the socket's
        # folder would strand the server with nothing able to reach it.
        subprocess.run(["tmux", "-S", str(self.data / "terminals.sock"), "kill-server"],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.opencode.shutdown()
        self.opencode.server_close()
        self.chief.shutdown()
        self.chief.server_close()
        self.temporary.cleanup()

    def call(self, op, params=None, timeout=10):
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(timeout)
            connection.connect(str(self.runtime / "station.sock"))
            connection.sendall(
                (json.dumps({"op": op, "params": params or {}}) + "\n").encode())
            return json.loads(connection.makefile("rb").readline())

    def status(self, since=None):
        params = {"gui": True, "sections": []}
        if since is not None:
            params["since"] = since
        return self.call("status", params)["result"]

    def full_status(self):
        return self.call("status")["result"]

    # -- helpers ---------------------------------------------------------------

    def await_prompt(self, timeout=15):
        """The gateway's prompt POST names the session the stream belongs to."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            for entry in list(FakeOpenCode.requests):
                if entry["path"].endswith("/prompt"):
                    return entry["path"].split("/")[3]
            time.sleep(.01)
        self.fail("The gateway never posted its prompt")

    def run_paced_turn(self, text="drain the stream"):
        """Start a turn and sample it while its deltas arrive over ~300 ms."""
        FakeOpenCode.next_events = []      # the test paces the stream itself
        outcome = {}

        def command():
            outcome["reply"] = self.call("command", {"text": text}, timeout=45)

        caller = threading.Thread(target=command, daemon=True)
        caller.start()
        session = self.await_prompt()
        threading.Thread(
            target=paced_events, args=(self.opencode, session, DELTA_TEXT),
            daemon=True).start()
        pages, since, deadline = [], 0, time.monotonic() + 45
        while caller.is_alive() and time.monotonic() < deadline:
            page = self.status(since)
            for event in page["events"]:
                since = max(since, event["seq"])
            pages.append(page)
            time.sleep(.02)
        caller.join(timeout=45)
        self.assertIn("reply", outcome, "the command never returned")
        self.assertTrue(outcome["reply"]["ok"], outcome["reply"])
        return outcome["reply"]["result"]["reply"], pages

    def deltas(self, pages):
        seen, events = set(), []
        for page in pages:
            for event in page["events"]:
                if event["kind"] != "reply_delta" or event["seq"] in seen:
                    continue
                seen.add(event["seq"])
                events.append(event)
        return events

    # -- BUG-098 ---------------------------------------------------------------

    def test_five_hundred_deltas_arrive_as_the_exact_answer(self):
        reply, pages = self.run_paced_turn()
        self.assertEqual(reply["text"], DELTA_TEXT)

        # The producer coalesced: 500 chunks never become 500 events.
        deltas = self.deltas(pages)
        self.assertTrue(deltas, "no reply_delta reached the client during the turn")
        self.assertLess(len(deltas), CHUNKS)
        turn_id = self.full_status()["turn_state"]["turn"]
        self.assertEqual(reply["turn"], turn_id)
        previous = ""
        for event in deltas:
            details = event["details"]
            self.assertEqual(details["turn"], turn_id)
            self.assertFalse(details["done"])
            # Cumulative, never a fragment: each payload contains the last one.
            self.assertTrue(details["text"].startswith(previous), details["text"][:40])
            previous = details["text"]
        self.assertTrue(previous)
        self.assertTrue(DELTA_TEXT.startswith(previous))

        # Busy was observable while the turn streamed, never after it.
        self.assertTrue(any(page["busy"] for page in pages))
        state = self.full_status()
        self.assertEqual(state["turn_state"]["text"], DELTA_TEXT)
        self.assertTrue(state["turn_state"]["done"])
        self.assertFalse(state["busy"])

    def test_slow_client_sees_the_same_final_row_after_five_seconds(self):
        self.run_paced_turn("sampled slowly")
        time.sleep(5)
        page = self.full_status()
        self.assertEqual(page["turn_state"]["text"], DELTA_TEXT)
        self.assertTrue(page["turn_state"]["done"])
        self.assertFalse(page["busy"])
        # The terminal reply survived the ring, so the row can be rebuilt whole.
        replies = [event for event in page["events"] if event["kind"] == "reply"]
        self.assertTrue(replies)
        self.assertEqual(replies[-1]["details"]["text"], DELTA_TEXT)

    def test_settings_change_does_not_cancel_an_open_turn(self):
        # BUG-092: settings bump config_version, never the cancel generation.
        FakeOpenCode.next_events = []      # pace the turn ourselves
        outcome = {}

        def command():
            outcome["reply"] = self.call("command", {"text": "survive config"},
                                         timeout=45)

        caller = threading.Thread(target=command, daemon=True)
        caller.start()
        session = self.await_prompt()
        before = self.full_status()
        settings = dict(before["settings"])
        # Favorites were one of the old generation triggers; changing one must
        # configure the next turn without cancelling this one.
        settings["favorites"] = list(settings.get("favorites") or []) + [
            "navi-config-probe"]
        saved = self.call("settings", settings)
        self.assertTrue(saved["ok"], saved)
        threading.Thread(
            target=paced_events, args=(self.opencode, session, DELTA_TEXT),
            daemon=True).start()
        caller.join(timeout=45)
        self.assertFalse(caller.is_alive())
        reply = outcome.get("reply", {})
        self.assertTrue(reply.get("ok"), reply)
        self.assertEqual(reply["result"]["reply"]["text"], DELTA_TEXT)
        after = self.full_status()
        self.assertEqual(after["generation"], before["generation"])
        self.assertGreater(after["config_version"], before["config_version"])
        self.assertFalse(after["busy"])

    def test_second_turn_gets_a_new_id(self):
        self.run_paced_turn("first turn")
        first = self.full_status()["turn_state"]["turn"]
        FakeOpenCode.next_events = None    # the fake server's default turn
        reply = self.call("command", {"text": "second turn"}, timeout=45)
        self.assertTrue(reply["ok"], reply)
        second = self.full_status()["turn_state"]
        self.assertNotEqual(second["turn"], first)
        self.assertTrue(second["done"])
        self.assertFalse(self.full_status()["busy"])
        self.assertIn("second turn", second["text"])

    # -- BUG-099 ---------------------------------------------------------------

    def test_dropped_client_resyncs_from_events_from(self):
        self.run_paced_turn("resync me")
        page = self.status(0)
        oldest = page["events_from"]
        self.assertGreater(oldest, 0)
        self.assertGreaterEqual(page["events"][0]["seq"], oldest)
        # A client whose cursor is behind the retained window asks from the
        # oldest seq the daemon can still serve and drains `has_more` pages.
        since = oldest - 1
        found = None
        for _ in range(20):
            page = self.status(since)
            for event in page["events"]:
                since = max(since, event["seq"])
                if event["kind"] == "reply" and event["details"].get("text") == DELTA_TEXT:
                    found = event
            if found or not page["has_more"]:
                break
        self.assertIsNotNone(found, "the terminal reply was not recoverable")
        self.assertEqual(found["epoch"], self.full_status()["boot_id"])

    # -- BUG-094 ---------------------------------------------------------------

    def test_status_sections_versions_and_shared_tmux_cache(self):
        full = self.full_status()
        for key in ("settings", "sessions", "projects", "workers", "browser",
                    "desktop", "voice", "active_session_tail", "cache",
                    "turn_state", "busy", "versions", "has_more", "events_from"):
            self.assertIn(key, full)
        self.assertFalse(full["has_more"])

        lean = self.status(0)
        for absent in LEAN_SECTIONS:
            self.assertNotIn(absent, lean, f"{absent} must not ride an event poll")
        for key in ("events", "events_from", "boot_id", "turn_state", "busy", "versions"):
            self.assertIn(key, lean)

        version = full["versions"]["settings"]
        unchanged = self.call("status", {"sections": ["settings"],
                                         "versions": {"settings": version}})["result"]
        self.assertNotIn("settings", unchanged)
        self.assertEqual(unchanged["versions"]["settings"], version)
        changed = self.call("status", {"sections": ["settings"],
                                       "versions": {"settings": 1}})["result"]
        self.assertEqual(changed["settings"], full["settings"])

        # Five polls inside one TTL share one `list-panes` fork (BUG-094).
        first = self.call("status", {"sections": ["cache", "sessions"]})
        self.assertTrue(first["ok"], first)
        before = first["result"]["cache"]["panes"]
        last = before
        for _ in range(5):
            last = self.call("status", {"sections": ["cache", "sessions"]})["result"]["cache"]["panes"]
        self.assertLessEqual(last["misses"] - before["misses"], 1)
        self.assertGreater(last["hits"], before["hits"])
        self.assertLess(last["age_ms"], 1000)


if __name__ == "__main__":
    unittest.main()
