"""Harness session lifecycle tests (plan 05, BUG-069..082).

Starts navid isolated with a temp HOME and stub executables; every assertion
runs against the real daemon over the real socket and a real tmux server.
"""
import json
import os
from pathlib import Path
import re
import socket
import sqlite3
import stat
import subprocess
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("NAVI_TEST_BINARY", "/tmp/navi-cargo-target/debug/navid"))


class Harness:
    """Stub executables + a fixture HOME for one test."""

    def __init__(self, folder):
        self.folder = folder
        self.home = folder / "home"
        self.stubbin = self.home / ".local/bin"
        self.stubbin.mkdir(parents=True)
        (self.home / ".npm-global/bin").mkdir(parents=True)
        self.out = folder / "out"
        self.out.mkdir()

    def stub(self, name, text, mode=0o755, where=None):
        path = (where or self.stubbin) / name
        path.write_text(text)
        path.chmod(mode)
        return path

    def write_stubs(self):
        claude = self.stub("claude", f"""#!/usr/bin/env python3
import json, os, sys, pathlib, re, time
args = sys.argv[1:]
if "--version" in args:
    print("claude-stub 9.9"); sys.exit(0)
sid = None
for i, a in enumerate(args):
    if a == "--session-id" and i + 1 < len(args):
        sid = args[i + 1]
if sid is None and "--resume" in args:
    sid = args[args.index("--resume") + 1]
out = pathlib.Path(os.environ["STUB_OUT"])
(out / "claude.env").write_text("\\n".join(sorted(
    f"{{k}}={{v}}" for k, v in os.environ.items())))
(out / "claude.argv").write_text(json.dumps(args))
home = pathlib.Path(os.environ["HOME"])
(home / ".claude/sessions").mkdir(parents=True, exist_ok=True)
(home / ".claude/sessions" / ("%d.json" % os.getpid())).write_text(json.dumps(
    {{"sessionId": sid or "none", "pid": os.getpid(),
      "cwd": os.getcwd(), "status": "running", "name": "stub"}}))
# Bracketed-paste TUI: print a prompt glyph, swallow the paste, record it.
sys.stdout.write("\\x1b[?2004hCLAUDE-READY\\r\\n> "); sys.stdout.flush()
# Raw mode: the canonical tty queue would drop a 4 KiB+ paste.
import tty, select
tty.setraw(sys.stdin.fileno())
data = b""
end = time.monotonic() + 10
while time.monotonic() < end:
    r, _, _ = select.select([sys.stdin], [], [], 0.2)
    if not r:
        if b"\\x1b[201~" in data:
            break
        continue
    chunk = os.read(sys.stdin.fileno(), 65536)
    if not chunk:
        break
    data += chunk
    if b"\\x1b[201~" in data and data.rstrip(b"\\r").endswith(b"\\x1b[201~"):
        break
(out / "claude.stdin").write_bytes(data)
sys.stdout.write("DONE\\r\\n"); sys.stdout.flush()
""")
        versioned = self.stub("codex", "#!/bin/sh\necho codex-stub 1.0\nsleep 60\n",
                              where=self.home / ".npm-global/bin")
        self.stub("agy", "#!/bin/sh\necho agy-stub 2.0\nsleep 60\n",
                  where=self.home / ".npm-global/bin")
        self.stub("dier", "#!/bin/sh\nexit 3\n")
        self.stub("noexec", "#!/bin/sh\nexit 0\n", mode=0o644)
        (self.folder / "adir").mkdir()
        slow = self.stub("slowclaude", """#!/usr/bin/env python3
import os, time, pathlib
time.sleep(1.4)
os.write(1, b"BOOTED\\n> ")
# One tty read: the line discipline hands over whatever arrived. A buffered
# read(n) would block until n bytes or EOF and never see the paste.
data = os.read(0, 65536)
pathlib.Path(os.environ["STUB_OUT"], "slow.stdin").write_bytes(data)
""")
        trap = self.stub("trapper", """#!/usr/bin/env python3
import signal, sys, time
signal.signal(signal.SIGINT, signal.SIG_IGN)
while True:
    time.sleep(0.2)
""")
        return {"claude": str(claude), "codex": str(versioned)}


class HarnessTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="navi-harness-")
        self.folder = Path(self.temporary.name)
        self.data = self.folder / "data"
        self.runtime = self.folder / "runtime"
        self.fx = Harness(self.folder)
        stubs = self.fx.write_stubs()
        env = dict(os.environ)
        env.update({
            "NAVI_ISOLATED": "1",
            "NAVI_DATA_DIR": str(self.data),
            "NAVI_RUNTIME_DIR": str(self.runtime),
            "NAVI_APP_DIR": str(ROOT),
            "HOME": str(self.fx.home),
            "STUB_OUT": str(self.fx.out),
            "NAVI_SYSTEM_PATH": "/nope-system",
            "NAVI_USER_PATH_DEFAULT": "",
            "SHELL": "/bin/bash",
            "PATH": f"{self.fx.stubbin}:/usr/bin:/bin",
        })
        for key in ("NAVI_MANAGED_TERMINALS", "NAVI_TEST_WORKER_SCRIPT"):
            env.pop(key, None)
        self.environment = env
        self.stubs = stubs
        self.start()

    def start(self):
        self.process = subprocess.Popen(
            [str(BINARY), "serve"], env=self.environment,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(150):
            try:
                self.call("ping")
                return
            except OSError:
                time.sleep(.02)
        self.fail("Daemon did not start")

    def call(self, op, params=None, timeout=15):
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(timeout)
            connection.connect(str(self.runtime / "station.sock"))
            connection.sendall(
                (json.dumps({"op": op, "params": params or {}}) + "\n").encode())
            return json.loads(connection.makefile("rb").readline())

    def tmux(self, *args):
        return subprocess.run(
            ["tmux", "-S", str(self.data / "terminals.sock"), *args],
            capture_output=True, text=True)

    def pane_command(self, target):
        out = self.tmux("display-message", "-p", "-t", target,
                        "#{pane_start_command}")
        return out.stdout.strip()

    def pane_field(self, target, field):
        out = self.tmux("display-message", "-p", "-t", target, f"#{{{field}}}")
        return out.stdout.strip()

    def wait_state(self, session_id, wanted, timeout=8):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            status = self.call("status")["result"]
            for row in status["sessions"]:
                if row["id"] == session_id and row["state"] == wanted:
                    return row
            time.sleep(.15)
        self.fail(f"{session_id} never reached {wanted}")

    def sessions(self):
        return self.call("status")["result"]["sessions"]

    def settings(self):
        return self.call("status")["result"]["settings"]

    def put_profiles(self, profiles):
        """Upsert test profiles alongside the defaults (removal is opt-in)."""
        settings = self.settings()
        by_id = {p["id"]: p for p in settings["profiles"]}
        for profile in profiles:
            by_id[profile["id"]] = profile
        settings["profiles"] = list(by_id.values())
        return settings

    def tearDown(self):
        self.process.terminate()
        self.process.wait(timeout=3)
        subprocess.run(["tmux", "-S", str(self.data / "terminals.sock"),
                        "kill-server"], stdout=subprocess.DEVNULL,
                       stderr=subprocess.DEVNULL)
        self.temporary.cleanup()

    # ---------------------------------------------------------------
    # BUG-069/070/072/075/076: launch env/argv/geometry, preseed, pipe
    # log, bracketed inject with readiness.
    # ---------------------------------------------------------------
    def test_launch_env_argv_geometry_and_inject(self):
        settings = self.put_profiles([{
            "id": "claude", "name": "Claude", "harness": "claude",
            "executable": self.stubs["claude"],
            "args": ["--flag", "with space"],
            "env": {"STUB_OUT": str(self.fx.out),
                    "NAVI_TEST_TOKEN": "sekrit"},
            "cwd": "inherit", "resume_default": "new",
            "size": {"cols": 97, "rows": 33},
            "interrupt_keys": []}])
        self.assertTrue(self.call("settings", settings)["ok"])

        created = self.call("create", {
            "profile": "claude", "name": "work",
            "cwd": str(self.folder), "allow_unregistered": True,
            "prompt": "first question"})
        self.assertTrue(created["ok"], created)
        result = created["result"]
        self.assertEqual(result["state"], "starting")
        self.assertTrue(result["running"])
        target = result["id"]

        # Exact argv: the progress link's registration, then
        # --session-id <uuid> -n work --flag 'with space'
        argv = json.loads((self.fx.out / "claude.argv").read_text())
        self.assertEqual(argv[0], "--mcp-config")
        start = argv.index("--session-id")
        uuid = argv[start + 1]
        self.assertRegex(uuid, r"^[0-9a-f-]{36}$")
        self.assertEqual(argv[start + 2:], ["-n", "work", "--flag", "with space"])
        self.assertEqual(result["native_id"], uuid)

        # Exact env: env -i base + profile env; secrets present in pane but
        # never stored anywhere the daemon writes.
        env_map = {}
        for line in (self.fx.out / "claude.env").read_text().splitlines():
            if "=" in line:
                env_map[line.split("=", 1)[0]] = line.split("=", 1)[1]
        self.assertEqual(env_map["HOME"], str(self.fx.home))
        self.assertTrue(env_map["PATH"].startswith(str(self.fx.stubbin)),
                        env_map["PATH"])
        self.assertEqual(env_map["TERM"], "tmux-256color")
        self.assertEqual(env_map["COLORTERM"], "truecolor")
        self.assertEqual(env_map["STUB_OUT"], str(self.fx.out))
        self.assertEqual(env_map["NAVI_TEST_TOKEN"], "sekrit")
        self.assertNotIn("TMUX", env_map)

        # Geometry from profile size.
        self.assertEqual(self.pane_field(target, "window_width"), "97")
        self.assertEqual(self.pane_field(target, "window_height"), "33")

        # env_keys stored, values never.
        db = sqlite3.connect(self.data / "station.sqlite3")
        row = db.execute(
            "SELECT env_keys, argv, native_id FROM sessions WHERE id=?",
            (target,)).fetchone()
        keys = json.loads(row[0])
        self.assertIn("NAVI_TEST_TOKEN", keys)
        self.assertNotIn("sekrit", row[1])
        self.assertEqual(row[2], uuid)
        db.close()

        # Pipe log exists 0600 with stub output; session_output strips ANSI.
        log = self.data / "sessions" / f"{target}.log"
        self.assertTrue(log.exists())
        self.assertEqual(stat.S_IMODE(log.stat().st_mode) & 0o777, 0o600)
        deadline = time.monotonic() + 8
        text = ""
        while time.monotonic() < deadline:
            out = self.call("session_output", {"id": target})
            self.assertTrue(out["ok"], out)
            text = out["result"]["text"]
            if "DONE" in text and "CLAUDE-READY" in text:
                break
            time.sleep(.2)
        self.assertIn("CLAUDE-READY", text)
        self.assertIn("DONE", text)
        self.assertNotIn("\x1b[", text)

        # Bracketed paste: the exact bytes the stub read.
        pasted = (self.fx.out / "claude.stdin").read_bytes()
        self.assertIn(b"\x1b[200~", pasted)
        self.assertIn(b"\x1b[201~", pasted)
        self.assertIn(b"first question", pasted)

        # session_events record the prompt with a readiness flag.
        events = self.call("session_events", {"id": target})["result"]["events"]
        kinds = [e["kind"] for e in events]
        self.assertEqual(kinds[:2], ["created", "launch_argv"])
        self.assertIn("native_captured", kinds)
        prompt = next(e for e in events if e["kind"] == "prompt")
        self.assertTrue(prompt["payload"]["ok"])
        self.assertTrue(prompt["payload"]["ready"])

    # ---------------------------------------------------------------
    # BUG-076: 5000-byte bracketed payload, newlines, leading -x.
    # ---------------------------------------------------------------
    def test_large_payload_bracketed_and_leading_dash(self):
        settings = self.put_profiles([{
            "id": "claude", "name": "Claude", "harness": "claude",
            "executable": self.stubs["claude"], "args": [], "env": {
                "STUB_OUT": str(self.fx.out)},
            "cwd": "inherit", "resume_default": "new",
            "size": {"cols": 80, "rows": 24}, "interrupt_keys": []}])
        self.assertTrue(self.call("settings", settings)["ok"])
        created = self.call("create", {
            "profile": "claude", "name": "big",
            "cwd": str(self.folder), "allow_unregistered": True})
        target = created["result"]["id"]
        # Wait for the stub TUI to reach its prompt glyph.
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            screen = self.tmux("capture-pane", "-p", "-t", target).stdout
            if "> " in screen:
                break
            time.sleep(.1)
        payload = "-x " + ("q" * 4988) + "a\nb\nc\nend"
        self.assertEqual(len(payload.encode()), 5000)
        sent = self.call("prompt", {"id": target, "text": payload})
        self.assertTrue(sent["ok"], sent)
        deadline = time.monotonic() + 8
        while not (self.fx.out / "claude.stdin").exists():
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.1)
        time.sleep(.4)
        data = (self.fx.out / "claude.stdin").read_bytes()
        body = data.split(b"\x1b[200~", 1)[1].split(b"\x1b[201~", 1)[0]
        # The stub puts its tty in raw mode, and tmux writes each pasted
        # newline to the pty as CR (a raw line discipline does not map CR back
        # to LF), so the transport shows CR: fold it to LF, which is how the
        # payload's three logical newlines are defined.
        body = body.replace(b"\r\n", b"\n").replace(b"\r", b"\n")
        self.assertTrue(body.startswith(b"-x "), body[:20])
        self.assertIn(b"a\nb\nc\nend", body)
        self.assertGreater(len(body), 4990)
        # Reject control characters and over-cap payloads.
        bad = self.call("prompt", {"id": target, "text": "a\x07b"})
        self.assertFalse(bad["ok"])
        huge = self.call("prompt", {"id": target, "text": "x" * 33000})
        self.assertFalse(huge["ok"])

    # ---------------------------------------------------------------
    # BUG-069: session_resume reuses the preseeded native id; restart
    # chains replaced_by; discover dedupes external rows.
    # ---------------------------------------------------------------
    def test_resume_restart_and_external_dedupe(self):
        settings = self.put_profiles([{
            "id": "claude", "name": "Claude", "harness": "claude",
            "executable": self.stubs["claude"], "args": [], "env": {
                "STUB_OUT": str(self.fx.out)},
            "cwd": "inherit", "resume_default": "new",
            "size": {"cols": 80, "rows": 24}, "interrupt_keys": []}])
        self.assertTrue(self.call("settings", settings)["ok"])
        created = self.call("create", {
            "profile": "claude", "name": "main",
            "cwd": str(self.folder), "allow_unregistered": True})
        first = created["result"]["id"]
        uuid = created["result"]["native_id"]

        resumed = self.call("session_resume", {"id": first})
        self.assertTrue(resumed["ok"], resumed)
        second = resumed["result"]["id"]
        self.assertNotEqual(second, first)
        self.assertIn(f"--resume {uuid}", self.pane_command(second),
                      self.pane_command(second))
        self.assertEqual(resumed["result"]["native_id"], uuid)

        restarted = self.call("session_restart", {"id": first})
        self.assertTrue(restarted["ok"], restarted)
        third = restarted["result"]["id"]
        self.assertNotEqual(third, first)
        rows = {r["id"]: r for r in self.sessions()}
        self.assertEqual(rows[first]["replaced_by"], third)
        self.assertIn("--resume", self.pane_command(third))

        # External rows seeded under the fixture HOME: the navi row wins the
        # dedupe; a second registry file appears as external.
        ext = self.fx.home / ".claude/sessions/424242.json"
        ext.write_text(json.dumps({
            "sessionId": "ext-uuid-9", "pid": 99999999,
            "cwd": "/elsewhere", "status": "running", "name": "external"}))
        # Force the 5 s cache to miss by waiting it out.
        self.call("session_discover")
        time.sleep(5.2)
        found = self.call("session_discover")["result"]["sessions"]
        by_native = [s for s in found
                     if s.get("native_id") == uuid]
        self.assertTrue(by_native)
        self.assertTrue(all(s["origin"] == "navi" for s in by_native))
        external = [s for s in found
                    if s.get("native_id") == "ext-uuid-9"]
        self.assertEqual(len(external), 1)
        self.assertEqual(external[0]["origin"], "external")
        self.assertEqual(external[0]["harness"], "claude")

    # ---------------------------------------------------------------
    # BUG-073/080: exit capture, kill, remove.
    # ---------------------------------------------------------------
    def test_exit_capture_kill_and_remove(self):
        settings = self.put_profiles([
            {"id": "dier", "name": "Dier", "harness": "dier",
             "executable": str(self.fx.stubbin / "dier"), "args": [],
             "env": {}, "cwd": "inherit", "resume_default": "new",
             "size": {"cols": 80, "rows": 24}, "interrupt_keys": []},
            {"id": "terminal", "name": "Terminal", "harness": "bash",
             "executable": "/bin/bash", "args": [], "env": {},
             "cwd": "inherit", "resume_default": "new",
             "size": {"cols": 80, "rows": 24}, "interrupt_keys": []}])
        self.assertTrue(self.call("settings", settings)["ok"])

        died = self.call("create", {
            "profile": "dier", "name": "boom", "cwd": str(self.folder),
            "allow_unregistered": True})
        self.assertTrue(died["ok"], died)
        result = died["result"]
        self.assertFalse(result["running"])
        self.assertEqual(result["exit_status"], 3)
        row = self.wait_state(result["id"], "exited")
        self.assertEqual(row["exit_status"], 3)
        log = self.data / "sessions" / f"{result['id']}.log"
        self.assertIn("\0SCREENSHOT", log.read_text(errors="replace"))

        alive = self.call("create", {
            "profile": "terminal", "name": "stay", "cwd": str(self.folder),
            "allow_unregistered": True})
        target = alive["result"]["id"]
        killed = self.call("session_kill", {"id": target})
        self.assertTrue(killed["ok"])
        self.assertEqual(killed["result"]["state"], "exited")
        self.wait_state(target, "exited")
        removed = self.call("session_remove", {"id": target})
        self.assertTrue(removed["ok"])
        self.assertTrue(all(r["id"] != target for r in self.sessions()))
        events = self.call("session_events", {"id": target})["result"]["events"]
        self.assertIn("removed", [e["kind"] for e in events])

    # ---------------------------------------------------------------
    # BUG-071/072: discovery classification + executable errors.
    # ---------------------------------------------------------------
    def test_discovery_and_executable_classification(self):
        settings = self.put_profiles([
            {"id": "claude", "name": "Claude", "harness": "claude",
             "executable": self.stubs["claude"], "args": [], "env": {},
             "cwd": "inherit", "resume_default": "new",
             "size": {"cols": 80, "rows": 24}, "interrupt_keys": []},
            {"id": "noexec", "name": "NoExec", "harness": "noexec",
             "executable": str(self.fx.stubbin / "noexec"), "args": [],
             "env": {}, "cwd": "inherit", "resume_default": "new",
             "size": {"cols": 80, "rows": 24}, "interrupt_keys": []},
            {"id": "adir", "name": "ADir", "harness": "adir",
             "executable": str(self.folder / "adir"), "args": [], "env": {},
             "cwd": "inherit", "resume_default": "new",
             "size": {"cols": 80, "rows": 24}, "interrupt_keys": []},
            {"id": "missing", "name": "Missing", "harness": "missing",
             "executable": "/definitely/not/a/harness", "args": [], "env": {},
             "cwd": "inherit", "resume_default": "new",
             "size": {"cols": 80, "rows": 24}, "interrupt_keys": []}])
        self.assertTrue(self.call("settings", settings)["ok"])
        time.sleep(5.2)  # harness cache TTL is 60 s; seed first, then check
        harnesses = self.call("harnesses")["result"]["harnesses"]
        by_name = {h["name"]: h for h in harnesses}
        self.assertTrue(by_name["claude"]["found"])
        self.assertEqual(by_name["claude"]["version"], "claude-stub 9.9")
        self.assertTrue(by_name["claude"]["in_navi_path"])
        self.assertTrue(by_name["claude"]["profile_exists"])
        self.assertTrue(by_name["claude"]["capabilities"]["preseed_id"])
        self.assertTrue(by_name["claude"]["capabilities"]["resume_id"])
        self.assertTrue(by_name["codex"]["found"])
        self.assertFalse(by_name["codex"]["in_navi_path"])
        self.assertTrue(by_name["agy"]["found"])
        self.assertTrue(by_name["agy"]["capabilities"]["resume_last"])
        self.assertTrue(by_name["pi"]["found"] is False)

        for profile, fragment in [
            ("noexec", "not executable"),
            ("adir", "directory"),
            ("missing", "not installed"),
        ]:
            result = self.call("create", {"profile": profile,
                                          "cwd": str(self.folder),
                                          "allow_unregistered": True})
            self.assertFalse(result["ok"], result)
            self.assertIn(fragment, result["error"], result["error"])
        self.assertEqual(self.sessions(), [])

    # ---------------------------------------------------------------
    # BUG-081: create writes last_launched, never the voice target.
    # ---------------------------------------------------------------
    def test_last_launched_vs_active_session(self):
        settings = self.put_profiles([{
            "id": "terminal", "name": "Terminal", "harness": "bash",
            "executable": "/bin/bash", "args": [], "env": {},
            "cwd": "inherit", "resume_default": "new",
            "size": {"cols": 80, "rows": 24}, "interrupt_keys": []}])
        self.assertTrue(self.call("settings", settings)["ok"])
        first = self.call("create", {
            "profile": "terminal", "name": "one", "cwd": str(self.folder),
            "allow_unregistered": True})["result"]["id"]
        second = self.call("create", {
            "profile": "terminal", "name": "two", "cwd": str(self.folder),
            "allow_unregistered": True})["result"]["id"]
        settings = self.settings()
        self.assertIsNone(settings["active_session"])
        self.assertEqual(settings["last_launched"], second)
        self.assertTrue(self.call("session_target", {"id": first})["ok"])
        self.assertEqual(self.settings()["active_session"], first)
        third = self.call("create", {
            "profile": "terminal", "name": "three", "cwd": str(self.folder),
            "allow_unregistered": True})["result"]["id"]
        self.assertEqual(self.settings()["active_session"], first)
        self.assertEqual(self.settings()["last_launched"], third)

    # ---------------------------------------------------------------
    # BUG-082/076: interrupt returns to the shell ≤1 s; hard kill for a
    # signal-ignoring harness; slow harness still gets its prompt.
    # ---------------------------------------------------------------
    def test_interrupt_soft_hard_and_slow_ready(self):
        settings = self.put_profiles([
            {"id": "terminal", "name": "Terminal", "harness": "bash",
             "executable": "/bin/bash", "args": [], "env": {},
             "cwd": "inherit", "resume_default": "new",
             "size": {"cols": 80, "rows": 24}, "interrupt_keys": []},
            {"id": "trapper", "name": "Trapper", "harness": "trapper",
             "executable": str(self.fx.stubbin / "trapper"), "args": [],
             "env": {}, "cwd": "inherit", "resume_default": "new",
             "size": {"cols": 80, "rows": 24}, "interrupt_keys": []},
            {"id": "slow", "name": "Slow", "harness": "claude",
             "executable": str(self.fx.stubbin / "slowclaude"), "args": [],
             "env": {"STUB_OUT": str(self.fx.out)},
             "cwd": "inherit", "resume_default": "new",
             "size": {"cols": 80, "rows": 24}, "interrupt_keys": []}])
        self.assertTrue(self.call("settings", settings)["ok"])

        term = self.call("create", {
            "profile": "terminal", "name": "busy", "cwd": str(self.folder),
            "allow_unregistered": True})["result"]["id"]
        self.tmux("send-keys", "-t", term, "-l", "sleep 30")
        self.tmux("send-keys", "-t", term, "Enter")
        time.sleep(.4)
        start = time.monotonic()
        self.assertTrue(self.call("session_interrupt", {"id": term})["ok"])
        deadline = time.monotonic() + 1
        command = ""
        while time.monotonic() < deadline:
            command = self.pane_field(term, "pane_current_command")
            if command == "bash":
                break
            time.sleep(.05)
        self.assertEqual(command, "bash")
        self.assertLess(time.monotonic() - start, 1.0)
        events = self.call("session_events", {"id": term})["result"]["events"]
        self.assertIn("state", [e["kind"] for e in events])

        trap = self.call("create", {
            "profile": "trapper", "name": "stubborn", "cwd": str(self.folder),
            "allow_unregistered": True})["result"]["id"]
        hard = self.call("session_interrupt", {"id": trap, "hard": True},
                         timeout=10)
        self.assertTrue(hard["ok"], hard)
        self.assertTrue(hard["result"]["killed"])
        self.wait_state(trap, "exited")

        started = time.monotonic()
        slow = self.call("create", {
            "profile": "slow", "name": "slow", "cwd": str(self.folder),
            "allow_unregistered": True,
            "prompt": "after banner"}, timeout=15)
        self.assertTrue(slow["ok"], slow)
        elapsed = time.monotonic() - started
        self.assertGreaterEqual(elapsed, 1.2)
        deadline = time.monotonic() + 8
        while not (self.fx.out / "slow.stdin").exists():
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.1)
        data = (self.fx.out / "slow.stdin").read_bytes()
        self.assertIn(b"after banner", data)

    # ---------------------------------------------------------------
    # BUG-078: rename keeps the id; removal is opt-in and orphans rows.
    # ---------------------------------------------------------------
    def test_profile_rename_and_removal_guard(self):
        settings = self.put_profiles([{
            "id": "terminal", "name": "Terminal", "harness": "bash",
            "executable": "/bin/bash", "args": [], "env": {},
            "cwd": "inherit", "resume_default": "new",
            "size": {"cols": 80, "rows": 24}, "interrupt_keys": []}])
        self.assertTrue(self.call("settings", settings)["ok"])
        created = self.call("create", {
            "profile": "terminal", "name": "stay", "cwd": str(self.folder),
            "allow_unregistered": True})
        target = created["result"]["id"]

        renamed = self.settings()
        for p in renamed["profiles"]:
            if p["id"] == "terminal":
                p["name"] = "Renamed Shell"
        self.assertTrue(self.call("settings", renamed)["ok"])
        by_id = {p["id"]: p for p in self.settings()["profiles"]}
        self.assertEqual(by_id["terminal"]["name"], "Renamed Shell")
        self.assertTrue(self.call("select", {"id": target})["ok"])

        # Duplicate ids and silent removal are rejected.
        broken = self.settings()
        broken["profiles"].append(dict(broken["profiles"][0]))
        self.assertFalse(self.call("settings", broken)["ok"])
        gutted = self.settings()
        gutted["profiles"] = []
        rejected = self.call("settings", gutted)
        self.assertFalse(rejected["ok"])
        gutted["allow_profile_removal"] = True
        accepted = self.call("settings", gutted)
        self.assertTrue(accepted["ok"], accepted)
        db = sqlite3.connect(self.data / "station.sqlite3")
        orphan = db.execute(
            "SELECT orphan FROM sessions WHERE id=?", (target,)).fetchone()[0]
        self.assertEqual(orphan, 1)
        db.close()
        self.assertNotIn("allow_profile_removal",
                         self.call("status")["result"]["settings"])


if __name__ == "__main__":
    unittest.main()
