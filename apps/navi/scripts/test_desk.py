#!/usr/bin/env python3
"""Desk commands through a real daemon. The browser is a script that writes
down what it was asked to open, so no window opens and no site is contacted."""
import json
import os
from pathlib import Path
import stat
import subprocess
import tempfile
import time
import unittest

from isolated import BINARY, IsolatedStation


class DeskTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.folder = tempfile.TemporaryDirectory(prefix="navi-desk-")
        root = Path(cls.folder.name)
        cls.opened = root / "opened.txt"
        browser = root / "browser"
        browser.write_text(f'#!/bin/sh\nprintf "%s\\n" "$*" >> "{cls.opened}"\n')
        browser.chmod(browser.stat().st_mode | stat.S_IXUSR)
        cls.home = root / "home"
        cls.project = cls.home / "Documents" / "zen-relay"
        (cls.project / ".git").mkdir(parents=True)
        (cls.home / "Documents" / "holiday").mkdir()
        cls.report = cls.home / "Documents" / "tax-report-2026.pdf"
        cls.report.write_text("x")
        (cls.home / ".cache").mkdir()
        (cls.home / ".cache" / "tax-report-old.pdf").write_text("x")
        cls.saved = {key: os.environ.get(key) for key in ("NAVI_BROWSER", "HOME", "NAVI_PROJECT_ROOTS", "PATH")}
        os.environ.update(NAVI_BROWSER=str(browser), HOME=str(cls.home),
                          NAVI_PROJECT_ROOTS=str(cls.home / "Documents"))
        cls.station = IsolatedStation().__enter__()

    @classmethod
    def tearDownClass(cls):
        cls.station.__exit__()
        for key, value in cls.saved.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value
        cls.folder.cleanup()

    def setUp(self):
        self.opened.unlink(missing_ok=True)

    def command(self, text, **params):
        reply = self.station.call("command", {"text": text, **params})
        self.assertTrue(reply["ok"], reply)
        return reply["result"]["reply"]

    def failure(self, text, **params):
        reply = self.station.call("command", {"text": text, **params})
        self.assertFalse(reply["ok"], reply)
        return reply["error"]

    def browser_got(self, count=1):
        for _ in range(100):
            if self.opened.exists():
                lines = self.opened.read_text().splitlines()
                if len(lines) >= count:
                    return lines
            time.sleep(.02)
        self.fail("the browser was not asked to open anything")

    def test_a_search_opens_the_browser_and_says_so(self):
        reply = self.command("search the web for flights to Cairo")
        self.assertEqual(self.browser_got(), ["https://www.google.com/search?q=flights%20to%20Cairo"])
        self.assertEqual(reply["text"], "Searching Google for flights to Cairo.")

    def test_a_search_keeps_its_own_and(self):
        self.command("search for cats and dogs")
        self.assertEqual(self.browser_got(), ["https://www.google.com/search?q=cats%20and%20dogs"])

    def test_two_real_steps_still_split(self):
        self.command("search for cats and open reddit")
        self.assertEqual(self.browser_got(2), ["https://www.google.com/search?q=cats", "https://www.reddit.com/"])

    def test_a_social_page_opens_by_name(self):
        self.command("check my x notifications")
        self.assertEqual(self.browser_got(), ["https://x.com/notifications"])

    def test_a_post_only_fills_the_box_and_keeps_its_words(self):
        reply = self.command("post cats and dogs then birds on x")
        self.assertEqual(self.browser_got(), ["https://x.com/intent/post?text=cats%20and%20dogs%20then%20birds"])
        self.assertIn("until you press Post", reply["text"])

    def fast_action(self, text):
        """The direct action this text would run, without running anything."""
        reply = self.station.call("decide", {"text": text})
        self.assertTrue(reply["ok"], reply)
        decision = reply["result"]["trace"][0]
        return decision["action"] if decision.get("selected") == "fast" else None

    def test_a_negated_command_is_not_a_search(self):
        self.assertIsNone(self.fast_action("don't search for cats"))

    def test_email_not_connected_opens_webmail_and_says_how(self):
        reply = self.command("check my email")
        self.assertEqual(self.browser_got(), ["https://mail.google.com/"])
        self.assertIn("Connections", reply["text"])

    def test_sending_while_not_connected_only_drafts(self):
        reply = self.command("email bob at example dot com saying see you then and bring it")
        line = self.browser_got()[0]
        self.assertTrue(line.startswith("https://mail.google.com/mail/?view=cm&to=bob%40example.com"), line)
        self.assertIn("see%20you%20then%20and%20bring%20it", line)
        self.assertIn("until you press Send", reply["text"])

    def test_a_shell_can_never_send_mail(self):
        # What the assistant might be talked into by a page or an email it read.
        reply = self.station.call("desk", {"text": "email bob@example.com saying hi", "source": "cli"})
        self.assertFalse(reply["ok"])
        self.assertIn("user's own command", reply["error"])
        self.assertIn("own command", self.failure("email bob@example.com saying hi", source="cli"))
        self.assertFalse(self.opened.exists())

    def test_the_desk_command_runs_no_model_turn(self):
        reply = self.station.call("desk", {"text": "what is the meaning of life"})
        self.assertFalse(reply["ok"])
        self.assertIn("no direct command", reply["error"])

    def test_the_cli_reports_a_reply_and_refuses_empty_input(self):
        environment = {**self.station.environment}
        out = subprocess.run([str(BINARY), "desk", "open", "gmail"], capture_output=True, text=True,
                             env=environment, check=True, timeout=30)
        self.assertEqual(out.stdout.strip(), "Opening Gmail.")
        self.assertEqual(self.browser_got(), ["https://mail.google.com/"])
        empty = subprocess.run([str(BINARY), "desk"], capture_output=True, text=True, env=environment, timeout=30)
        self.assertEqual(empty.returncode, 1)
        self.assertIn("Usage", empty.stderr)

    def test_an_unregistered_project_is_found_by_its_spoken_name(self):
        reply = self.command("open the zen relay project")
        self.assertIn("Switched to zen-relay", reply["text"])
        projects = self.station.call("projects")["result"]["projects"]
        found = [project for project in projects if project["name"] == "zen-relay"]
        self.assertEqual(len(found), 1)
        self.assertEqual(Path(found[0]["path"]).resolve(), self.project.resolve())
        # A second mention finds the same project instead of adding another.
        self.command("open the zen relay project")
        projects = self.station.call("projects")["result"]["projects"]
        self.assertEqual(len([project for project in projects if project["name"] == "zen-relay"]), 1)

    def test_a_harness_opens_in_a_project_named_only_by_voice(self):
        # bash stands in for the harness, so no real coding agent starts.
        settings = self.station.call("status")["result"]["settings"]
        for profile in settings["profiles"]:
            if profile["id"] == "codex":
                profile.update(executable="/bin/bash", args=[])
        self.assertTrue(self.station.call("settings", settings)["ok"])
        self.command("open codex in zen relay to fix the login bug")
        sessions = self.station.call("status")["result"]["sessions"]
        opened = [session for session in sessions if session["profile"] == "codex"]
        self.assertEqual(len(opened), 1, sessions)
        self.assertEqual(Path(opened[0]["cwd"]).resolve(), self.project.resolve())
        self.assertTrue(opened[0]["running"])

    def test_a_folder_that_is_no_project_is_not_registered(self):
        self.assertIsNone(self.fast_action("open the holiday project"))
        projects = self.station.call("projects")["result"]["projects"]
        self.assertNotIn("holiday", [project["name"] for project in projects])

    def test_a_file_search_lists_my_files_and_skips_hidden_folders(self):
        reply = self.command("find the file tax report")
        self.assertIn("tax-report-2026.pdf", reply["text"])
        self.assertNotIn("tax-report-old", reply["text"])
        self.assertTrue(reply["choices"])

    def test_a_missing_file_is_said_plainly(self):
        reply = self.command("find the file zzqx report")
        self.assertIn("could not find", reply["text"])

    def test_isolated_mode_blocks_files_apps_and_system_controls(self):
        self.assertIn("isolated", self.failure("volume up"))
        self.assertIn("isolated", self.failure("open my downloads"))
        self.assertIn("isolated", self.failure("lock the screen"))

    def test_mail_status_never_holds_a_password(self):
        status = self.station.call("mail_status")
        self.assertEqual(status["result"]["connected"], False)
        self.assertNotIn("password", json.dumps(status))


if __name__ == "__main__":
    unittest.main()
