"""Project graph and harness tail tests (plan 06, BUG-087/089/090).

Runs `navid` isolated with a fixture HOME, a stub harness profile that prints a
canned line, and one real tmux server. Every assertion is made over the socket,
so the daemon's own store and scoring are what is being measured.
"""
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("NAVI_TEST_BINARY", "/tmp/navi-cargo-target/debug/navid"))

# The stub stands in for a delegated harness: it prints its report once and then
# keeps the pane alive, exactly like an agent that is done thinking but attached.
STUB = """#!/bin/sh
printf '%s\\n' "${STUB_TAIL:-}"
while :; do sleep 0.2; done
"""


class GraphTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="navi-graph-")
        self.folder = Path(self.temporary.name)
        self.data = self.folder / "data"
        self.runtime = self.folder / "runtime"
        self.home = self.folder / "home"
        self.stubbin = self.home / ".local/bin"
        self.stubbin.mkdir(parents=True)
        (self.home / ".npm-global/bin").mkdir(parents=True)
        self.stub = self.stubbin / "stub-harness"
        self.stub.write_text(STUB)
        self.stub.chmod(0o755)
        self.project = self.folder / "alpha"
        self.project.mkdir()
        env = dict(os.environ)
        env.update({
            "NAVI_ISOLATED": "1",
            "NAVI_DATA_DIR": str(self.data),
            "NAVI_RUNTIME_DIR": str(self.runtime),
            "NAVI_APP_DIR": str(ROOT),
            "HOME": str(self.home),
            "NAVI_SYSTEM_PATH": "/nope-system",
            "NAVI_USER_PATH_DEFAULT": "",
            "SHELL": "/bin/bash",
            "PATH": f"{self.stubbin}:/usr/bin:/bin",
            # Compress 5 s / every 10 s / 90 s into 0.2 s / 0.2 s / 1.2 s so the
            # sampler is proven without a 90 s test (BUG-087).
            "NAVI_TAIL_SAMPLE_MS": "200",
        })
        for key in ("NAVI_MANAGED_TERMINALS", "NAVI_TEST_WORKER_SCRIPT", "TMUX", "TMUX_PANE"):
            env.pop(key, None)
        self.environment = env
        self.start()
        added = self.call("project_add", {"name": "Alpha", "path": str(self.project.resolve())})
        self.assertTrue(added["ok"], added)
        self.alpha = added["result"]["project"]

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

    def call(self, op, params=None, timeout=20):
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(timeout)
            connection.connect(str(self.runtime / "station.sock"))
            connection.sendall(
                (json.dumps({"op": op, "params": params or {}}) + "\n").encode())
            return json.loads(connection.makefile("rb").readline())

    def tearDown(self):
        self.process.terminate()
        self.process.wait(timeout=3)
        subprocess.run(["tmux", "-S", str(self.data / "terminals.sock"), "kill-server"],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.temporary.cleanup()

    # ---------------------------------------------------------------
    # helpers
    # ---------------------------------------------------------------
    def profile(self, identifier, line):
        return {
            "id": identifier,
            "name": identifier,
            "harness": "shell",
            "executable": str(self.stub),
            "args": [],
            "env": {"STUB_TAIL": line, "STUB_OUT": str(self.folder)},
            "cwd": "inherit",
            "resume_default": "new",
            "size": {"cols": 110, "rows": 30},
            "interrupt_keys": [],
        }

    def put_profiles(self, profiles):
        settings = self.call("status")["result"]["settings"]
        by_id = {profile["id"]: profile for profile in settings["profiles"]}
        for profile in profiles:
            by_id[profile["id"]] = profile
        settings["profiles"] = list(by_id.values())
        saved = self.call("settings", settings)
        self.assertTrue(saved["ok"], saved)

    def session(self, profile, name):
        created = self.call("create", {
            "profile": profile, "name": name, "cwd": str(self.project.resolve())})
        self.assertTrue(created["ok"], created)
        self.assertTrue(created["result"]["id"], created)
        return created["result"]

    def pane_text(self, session_id):
        run = subprocess.run(
            ["tmux", "-S", str(self.data / "terminals.sock"), "capture-pane",
             "-p", "-J", "-S", "-40", "-t", session_id],
            capture_output=True, text=True)
        return run.stdout

    def wait_pane(self, session_id, needle, timeout=10):
        """Wait on the pane itself, so the daemon's first read is the one that counts."""
        deadline = time.monotonic() + timeout
        text = ""
        while time.monotonic() < deadline:
            text = self.pane_text(session_id)
            if needle in text:
                return text
            time.sleep(0.1)
        self.fail(f"{needle!r} never appeared in {session_id}: {text!r}")

    def tail(self, session_id, **extra):
        result = self.call("harness_tail", {"id": session_id, "lines": 40, **extra})
        self.assertTrue(result["ok"], result)
        return result["result"]

    def graph(self):
        result = self.call("graph_get", {"project_id": self.alpha["id"]})
        self.assertTrue(result["ok"], result)
        return result["result"]

    def aspect_with_task(self, title="Tests", task="Wire the server"):
        aspect = self.call("aspect_add", {"project_id": self.alpha["id"], "title": title})
        self.assertTrue(aspect["ok"], aspect)
        created = self.call("task_add", {
            "project_id": self.alpha["id"], "aspect_id": aspect["result"]["id"],
            "title": task})
        self.assertTrue(created["ok"], created)
        return aspect["result"]["id"], created["result"]["id"]

    # ---------------------------------------------------------------
    # BUG-087: harness output becomes evidence, or nothing at all
    # ---------------------------------------------------------------
    def test_passed_line_is_a_verified_signal(self):
        aspect_id, task_id = self.aspect_with_task()
        self.put_profiles([self.profile("stub-pass", "PASSED 12")])
        session = self.session("stub-pass", "pass run")
        self.wait_pane(session["id"], "PASSED 12")
        body = self.tail(session["id"], task_id=task_id)
        self.assertTrue(body["changed"], body)
        self.assertEqual(body["rules"], {"family": "shell", "version": 1}, body)
        self.assertEqual(len(body["signals"]), 1, body)
        signal = body["signals"][0]
        self.assertEqual(signal["kind"], "test_pass", signal)
        self.assertEqual(signal["source"], "harness", signal)
        self.assertEqual(signal["evidence"], "PASSED 12", signal)
        self.assertEqual(signal["task_id"], task_id, signal)
        graph = self.graph()
        self.assertEqual(graph["percent"], 100.0, graph)
        self.assertEqual(graph["percent_null"], False, graph)
        self.assertEqual(graph["verified"], 100.0, graph)
        self.assertEqual(graph["signal_count"], 1, graph)
        self.assertEqual([row["kind"] for row in graph["signals"]], ["test_pass"], graph)
        self.assertEqual(graph["signals"][0]["evidence"], "PASSED 12", graph["signals"])
        self.assertEqual(graph["signals"][0]["source"], "harness", graph["signals"])
        self.assertIs(graph["signals"][0]["assistant"], False, graph["signals"])
        self.assertEqual(len(graph["aspects"]), 1, graph)
        self.assertEqual(graph["aspects"][0]["id"], aspect_id, graph)
        self.assertEqual(graph["aspects"][0]["percent"], 100.0, graph)
        self.assertEqual(graph["aspects"][0]["tasks"], 1, graph)
        self.assertEqual(graph["tasks"]["done"], 0, graph)

    def test_failed_line_blocks_the_task(self):
        _aspect_id, task_id = self.aspect_with_task()
        self.put_profiles([self.profile("stub-fail", "FAILED 3")])
        session = self.session("stub-fail", "fail run")
        self.wait_pane(session["id"], "FAILED 3")
        body = self.tail(session["id"], task_id=task_id)
        self.assertEqual(len(body["signals"]), 1, body)
        self.assertEqual(body["signals"][0]["kind"], "test_fail", body)
        self.assertEqual(body["signals"][0]["evidence"], "FAILED 3", body)
        self.assertTrue(body["blocked"], body)
        self.assertEqual(body["task"]["state"], "blocked", body)
        graph = self.graph()
        self.assertEqual(graph["tasks"]["blocked"], 1, graph)
        self.assertEqual(graph["percent"], 0.0, graph)
        self.assertEqual(graph["verified"], 100.0, graph)
        self.assertIn("blocked", " ".join(graph["risks"]), graph)
        self.assertEqual([row["kind"] for row in graph["signals"]], ["test_fail"], graph)

    def test_random_output_emits_nothing(self):
        _aspect_id, task_id = self.aspect_with_task()
        self.put_profiles([self.profile("stub-noise", "nothing to report here")])
        session = self.session("stub-noise", "noise run")
        self.wait_pane(session["id"], "nothing to report")
        body = self.tail(session["id"], task_id=task_id)
        self.assertTrue(body["changed"], body)
        self.assertEqual(body["signals"], [], body)
        self.assertFalse(body["blocked"], body)
        graph = self.graph()
        self.assertEqual(graph["signal_count"], 0, graph)
        self.assertEqual(graph["tasks"]["blocked"], 0, graph)
        # Nothing was recorded, so every task is flagged unknown and the score
        # cannot be explained: the UI renders "—" for exactly this state.
        self.assertEqual(len(graph["aspects"]), 1, graph)
        self.assertEqual(graph["aspects"][0]["unknown"], 1, graph)
        self.assertIn("no evidence", " ".join(graph["risks"]), graph)

    def test_identical_tail_is_stored_once(self):
        _aspect_id, task_id = self.aspect_with_task()
        self.put_profiles([self.profile("stub-pass-once", "PASSED 12")])
        session = self.session("stub-pass-once", "dedupe run")
        self.wait_pane(session["id"], "PASSED 12")
        first = self.tail(session["id"], task_id=task_id)
        self.assertTrue(first["changed"], first)
        self.assertEqual(len(first["signals"]), 1, first)
        second = self.tail(session["id"], task_id=task_id)
        self.assertFalse(second["changed"], second)
        self.assertTrue(second["duplicate"], second)
        self.assertEqual(second["signals"], [], second)
        self.assertEqual(second["hash"], first["hash"], (first, second))
        self.assertEqual(self.graph()["signal_count"], 1)

    def test_trailer_binds_the_task(self):
        _aspect_id, task_id = self.aspect_with_task()
        self.put_profiles([self.profile("stub-trailer", f"PASSED 12\nnavi:task={task_id}")])
        session = self.session("stub-trailer", "trailer run")
        self.wait_pane(session["id"], task_id)
        body = self.tail(session["id"])
        self.assertEqual(len(body["signals"]), 1, body)
        self.assertEqual(body["signals"][0]["task_id"], task_id, body)
        self.assertEqual(body["task"]["id"], task_id, body)
        graph = self.graph()
        self.assertEqual(graph["percent"], 100.0, graph)

    def test_sampler_reads_the_prompted_pane(self):
        self.put_profiles([self.profile("stub-sample", "PASSED 7")])
        session = self.session("stub-sample", "sample run")
        self.wait_pane(session["id"], "PASSED 7")
        # No prompt yet: nothing may sample the pane on its own.
        time.sleep(1.5)
        self.assertEqual(self.graph()["signal_count"], 0)
        prompted = self.call("prompt", {"id": session["id"], "text": "run the suite"})
        self.assertTrue(prompted["ok"], prompted)
        deadline = time.monotonic() + 6
        graph = self.graph()
        while time.monotonic() < deadline and graph["signal_count"] == 0:
            time.sleep(0.1)
            graph = self.graph()
        self.assertEqual(graph["signal_count"], 1, graph)
        self.assertEqual(graph["signals"][0]["kind"], "test_pass", graph)
        self.assertEqual(graph["signals"][0]["evidence"], "PASSED 7", graph)
        # The sampler keeps reading for its window; the hash stops every repeat.
        time.sleep(0.8)
        self.assertEqual(self.graph()["signal_count"], 1)

    def test_unknown_session_is_rejected(self):
        result = self.call("harness_tail", {"id": "navi-does-not-exist"})
        self.assertFalse(result["ok"], result)
        self.assertIn("Unknown session", result["error"], result)

    def test_unknown_task_binding_is_rejected(self):
        self.put_profiles([self.profile("stub-bad-task", "PASSED 12")])
        session = self.session("stub-bad-task", "bad task run")
        self.wait_pane(session["id"], "PASSED 12")
        result = self.call("harness_tail", {
            "id": session["id"], "lines": 40, "task_id": "tsk-does-not-exist"})
        self.assertFalse(result["ok"], result)
        self.assertIn("Unknown task", result["error"], result)

    def test_assistant_scope_binds_the_project_directory(self):
        beta = self.folder / "beta"
        beta.mkdir()
        added = self.call("project_add", {"name": "Beta", "path": str(beta.resolve())})
        self.assertTrue(added["ok"], added)
        beta_project = added["result"]["project"]
        scoped = self.call("assistant_scope", {"project_id": self.alpha["id"]})
        self.assertTrue(scoped["ok"], scoped)
        self.assertEqual(scoped["result"]["project_id"], self.alpha["id"], scoped)
        self.assertEqual(scoped["result"]["directory"], self.alpha["path"], scoped)
        self.assertIsNone(scoped["result"]["session"], scoped)
        switched = self.call("assistant_scope", {"project_id": beta_project["id"]})
        self.assertTrue(switched["ok"], switched)
        self.assertEqual(switched["result"]["project_id"], beta_project["id"], switched)
        self.assertEqual(switched["result"]["directory"], beta_project["path"], switched)
        unknown = self.call("assistant_scope", {"project_id": "prj-does-not-exist"})
        self.assertFalse(unknown["ok"], unknown)

    def test_git_probe_emits_only_on_change(self):
        # BUG-086: commits and dirty transitions are evidence; a repeat adds none.
        repo = self.folder / "repo"
        repo.mkdir()

        def git(*args):
            run = subprocess.run(["git", "-C", str(repo), *args],
                                 capture_output=True, text=True)
            self.assertEqual(run.returncode, 0, run.stderr)

        git("init", "-q")
        (repo / "a.txt").write_text("one\n")
        git("add", "a.txt")
        git("-c", "user.name=Navi", "-c", "user.email=navi@example.invalid",
            "commit", "-qm", "first")
        added = self.call("project_add", {"name": "Repo", "path": str(repo.resolve())})
        self.assertTrue(added["ok"], added)
        project = self.call("project_probe", {"id": added["result"]["project"]["id"]})
        self.assertTrue(project["ok"], project)
        project = project["result"]["project"]
        self.assertFalse(project["git_dirty"], project)
        self.assertTrue(project["git_head"], project)
        kinds = [row["kind"] for row in
                 self.call("graph_get", {"project_id": project["id"]})["result"]["signals"]]
        self.assertEqual(kinds.count("git_commit"), 1, kinds)
        self.assertNotIn("git_dirty", kinds, kinds)

        # A second probe sees the same HEAD and clean tree: nothing is added.
        self.assertTrue(self.call("project_probe", {"id": project["id"]})["ok"])
        kinds = [row["kind"] for row in
                 self.call("graph_get", {"project_id": project["id"]})["result"]["signals"]]
        self.assertEqual(kinds.count("git_commit"), 1, kinds)

        (repo / "a.txt").write_text("two\n")
        self.call("project_probe", {"id": project["id"]})
        kinds = [row["kind"] for row in
                 self.call("graph_get", {"project_id": project["id"]})["result"]["signals"]]
        self.assertEqual(kinds.count("git_dirty"), 1, kinds)

        git("add", "a.txt")
        git("-c", "user.name=Navi", "-c", "user.email=navi@example.invalid",
            "commit", "-qm", "second")
        self.call("project_probe", {"id": project["id"]})
        kinds = [row["kind"] for row in
                 self.call("graph_get", {"project_id": project["id"]})["result"]["signals"]]
        self.assertEqual(kinds.count("git_commit"), 2, kinds)
        # Only the newest working-tree flip is listed: the current state.
        self.assertEqual(kinds.count("git_clean"), 1, kinds)
        self.assertEqual(kinds.count("git_dirty"), 0, kinds)

        # A non-repository directory is path state only, never a fake signal.
        plain = self.folder / "plain"
        plain.mkdir()
        added = self.call("project_add", {"name": "Plain", "path": str(plain.resolve())})
        self.assertTrue(added["ok"], added)
        self.assertIsNone(added["result"]["project"]["git_root"], added)
        self.assertEqual(self.call("graph_get", {"project_id": added["result"]["project"]["id"]})
                         ["result"]["signal_count"], 0)

    def test_empty_project_reports_null_not_zero(self):
        graph = self.graph()
        self.assertEqual(graph["aspects"], [], graph)
        self.assertEqual(graph["percent"], None, graph)
        self.assertIs(graph["percent_null"], True, graph)
        self.assertEqual(graph["signals"], [], graph)
        # Two projects are registered, so an unscoped graph is an error instead
        # of a silent pick of the first one (BUG-083/090).
        unscoped = self.call("graph_get", {})
        self.assertFalse(unscoped["ok"], unscoped)
        self.assertIn("Select a project", unscoped["error"], unscoped)


if __name__ == "__main__":
    unittest.main(verbosity=2)
