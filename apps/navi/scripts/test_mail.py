#!/usr/bin/env python3
"""mail.py against fake IMAP and SMTP servers. Touches no real account."""
import imaplib
import json
import os
from pathlib import Path
import smtplib
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

WORKERS = Path(__file__).resolve().parents[1] / "workers"
sys.path.insert(0, str(WORKERS))

import mail  # noqa: E402
import speech_config  # noqa: E402

PLAIN = (
    b"From: Sara Ali <sara@example.com>\r\nSubject: Lunch\r\nDate: Fri, 6 Nov 2026 10:00:00 +0000\r\n"
    b"Content-Type: text/plain\r\n\r\nSee you at one.\r\nBring the notes.\r\n\r\nOn Thu someone wrote:\r\n> older text\r\n"
)
HTML = (
    b"From: News <news@example.com>\r\nSubject: Deals\r\nContent-Type: text/html\r\n\r\n"
    b"<html><head><style>p{}</style></head><body><p>Hello there.</p><script>x()</script></body></html>\r\n"
)
HEADERS = b"From: Sara Ali <sara@example.com>\r\nSubject: Lunch\r\n\r\n"


class FakeImap:
    """The IMAP calls mail.py makes. `refuse` makes the login fail."""

    refuse = False
    unseen = [b"1", b"2", b"3"]
    everything = [b"1", b"2", b"3"]
    message = PLAIN
    from_hits = {"sara": [b"2"]}
    made = []

    def __init__(self, host, timeout=None, ssl_context=None):
        self.host = host
        self.context = ssl_context
        self.fetches = []
        self.selected = None
        FakeImap.made.append(self)

    def login(self, user, password):
        if FakeImap.refuse:
            raise imaplib.IMAP4.error(f"bad login for {user} with {password}")

    def select(self, box, readonly=False):
        self.selected = (box, readonly)
        return "OK", [b"3"]

    def search(self, _charset, *criteria):
        if criteria[0] == "UNSEEN":
            found = FakeImap.unseen
        elif criteria[0] == "ALL":
            found = FakeImap.everything
        else:
            found = FakeImap.from_hits.get(criteria[1].strip('"').lower(), [])
        return "OK", [b" ".join(found)]

    def fetch(self, number, what):
        self.fetches.append(what)
        return "OK", [(b"1 (BODY[])", HEADERS if "HEADER.FIELDS" in what else FakeImap.message), b")"]

    def logout(self):
        pass


class FakeSmtp:
    refuse = False
    sent = []

    def __init__(self, host, port, timeout=None, context=None):
        self.host = host
        self.port = port
        self.context = context

    def login(self, _user, _password):
        if FakeSmtp.refuse:
            raise smtplib.SMTPAuthenticationError(535, b"no")

    def send_message(self, message):
        FakeSmtp.sent.append(message)

    def quit(self):
        pass


class MailTests(unittest.TestCase):
    def setUp(self):
        self.folder = tempfile.TemporaryDirectory()
        self.secrets = Path(self.folder.name) / "config" / "secrets.env"
        self.patches = [
            mock.patch.object(speech_config, "SECRETS_FILE", self.secrets),
            mock.patch.object(imaplib, "IMAP4_SSL", FakeImap),
            mock.patch.object(smtplib, "SMTP_SSL", FakeSmtp),
            mock.patch.dict(os.environ, {}, clear=False),
        ]
        for patch in self.patches:
            patch.start()
        for key in (*mail.KEYS, "NAVI_ISOLATED"):
            os.environ.pop(key, None)
        speech_config.reset_caches()
        FakeImap.refuse = FakeSmtp.refuse = False
        FakeImap.message = PLAIN
        FakeImap.made = []
        FakeImap.unseen = [b"1", b"2", b"3"]
        FakeImap.everything = [b"1", b"2", b"3"]
        FakeSmtp.sent = []

    def tearDown(self):
        for patch in self.patches:
            patch.stop()
        speech_config.reset_caches()
        self.folder.cleanup()

    def connect(self, address="me@gmail.com", password="app pass word"):
        return mail.run("connect", {"address": address, "password": password})

    def test_not_connected_until_a_login_works(self):
        self.assertEqual(mail.run("status", {})["connected"], False)
        self.assertEqual(self.connect()["connected"], True)
        status = mail.run("status", {})
        self.assertEqual((status["address"], status["imap"], status["smtp"]),
                         ("me@gmail.com", "imap.gmail.com", "smtp.gmail.com"))

    def test_the_password_is_private_and_never_returned(self):
        reply = self.connect()
        self.assertNotIn("app pass word", json.dumps(reply))
        self.assertNotIn("app pass word", json.dumps(mail.run("status", {})))
        self.assertEqual(stat.S_IMODE(self.secrets.stat().st_mode), 0o600)
        self.assertEqual(stat.S_IMODE(self.secrets.parent.stat().st_mode), 0o700)

    def test_a_refused_login_stores_nothing_and_does_not_echo_the_password(self):
        FakeImap.refuse = True
        reply = self.connect()
        self.assertFalse(reply["ok"])
        self.assertIn("app password", reply["error"])
        self.assertNotIn("app pass word", reply["error"])
        self.assertFalse(self.secrets.exists())

    def test_a_refused_smtp_login_stores_nothing(self):
        FakeSmtp.refuse = True
        self.assertFalse(self.connect()["ok"])
        self.assertFalse(self.secrets.exists())

    def test_connect_keeps_the_other_secrets_and_disconnect_removes_only_mail(self):
        self.secrets.parent.mkdir(parents=True)
        self.secrets.write_text("GROQ_API_KEY=keep\n")
        self.connect()
        self.assertIn("GROQ_API_KEY=keep", self.secrets.read_text())
        self.assertEqual(mail.run("disconnect", {})["connected"], False)
        self.assertEqual(self.secrets.read_text(), "GROQ_API_KEY=keep\n")
        self.assertEqual(mail.run("status", {})["connected"], False)

    def test_bad_input_is_refused_before_any_connection(self):
        self.assertFalse(mail.run("connect", {"address": "nope", "password": "x"})["ok"])
        self.assertFalse(mail.run("connect", {"address": "a@b.com", "password": 'x"\ny'})["ok"])
        self.assertEqual(FakeImap.made, [])

    def test_isolated_station_never_reaches_a_real_account(self):
        self.connect()
        with mock.patch.dict(os.environ, {"NAVI_ISOLATED": "1"}):
            self.assertEqual(mail.run("status", {})["connected"], False)
            self.assertFalse(mail.run("check", {})["ok"])
            self.assertFalse(mail.run("connect", {"address": "a@b.com", "password": "x"})["ok"])

    def test_servers_are_verified_and_custom_hosts_are_kept(self):
        reply = mail.run("connect", {"address": "me@corp.example", "password": "x", "imap_host": "mx.corp.example"})
        self.assertEqual((reply["imap"], reply["smtp"]), ("mx.corp.example", "smtp.corp.example"))
        self.assertIsNotNone(FakeImap.made[0].context)

    def test_check_counts_unread_and_names_the_newest_without_marking_read(self):
        self.connect()
        reply = mail.run("check", {})
        self.assertEqual(reply["count"], 3)
        self.assertIn("You have 3 unread emails.", reply["text"])
        self.assertIn("- Sara Ali: Lunch", reply["text"])
        self.assertEqual(reply["speech"], "You have 3 unread emails. The newest is from Sara Ali, about Lunch.")
        client = FakeImap.made[-1]
        self.assertEqual(client.selected, ("INBOX", True))
        self.assertTrue(all("PEEK" in fetch for fetch in client.fetches))

    def test_an_empty_inbox_is_said_plainly(self):
        self.connect()
        FakeImap.unseen = []
        self.assertEqual(mail.run("check", {})["text"], "You have no unread email.")

    def test_read_leaves_out_quoted_history(self):
        self.connect()
        reply = mail.run("read", {})
        self.assertIn("See you at one.", reply["text"])
        self.assertNotIn("older text", reply["text"])
        self.assertTrue(reply["speech"].startswith("From Sara Ali, about Lunch. See you at one."))
        self.assertIn("PEEK", FakeImap.made[-1].fetches[0])

    def test_read_turns_html_into_text_without_its_code(self):
        self.connect()
        FakeImap.message = HTML
        reply = mail.run("read", {})
        self.assertIn("Hello there.", reply["text"])
        self.assertNotIn("x()", reply["text"])
        self.assertNotIn("p{}", reply["text"])

    def test_contact_finds_one_address_and_quotes_nothing_from_the_request(self):
        self.connect()
        self.assertEqual(mail.run("contact", {"name": "sara"})["address"], "sara@example.com")
        self.assertIn("no address", mail.run("contact", {"name": 'nobody" OR ALL'})["error"])

    def test_send_goes_out_with_a_single_line_subject(self):
        self.connect()
        reply = mail.run("send", {"to": "bob@example.com", "subject": "hi\r\nBcc: evil@example.com", "body": "Hello"})
        self.assertTrue(reply["sent"])
        message = FakeSmtp.sent[0]
        self.assertEqual(message["To"], "bob@example.com")
        self.assertEqual(message["Subject"], "hi Bcc: evil@example.com")
        self.assertIsNone(message["Bcc"])

    def test_send_refuses_a_header_in_the_address(self):
        self.connect()
        self.assertFalse(mail.run("send", {"to": "a@b.com\r\nBcc: c@d.com", "body": "x"})["ok"])
        self.assertEqual(FakeSmtp.sent, [])

    def test_a_network_failure_names_no_details(self):
        self.connect()
        with mock.patch.object(imaplib, "IMAP4_SSL", side_effect=OSError("no route")):
            reply = mail.run("check", {})
        self.assertFalse(reply["ok"])
        self.assertIn("Could not reach imap.gmail.com", reply["error"])

    def test_the_command_line_speaks_json_and_takes_the_password_on_stdin(self):
        environment = {**os.environ, "NAVI_SECRETS_FILE": str(self.secrets)}
        out = subprocess.run([sys.executable, str(WORKERS / "mail.py"), "status"], input="{}", text=True,
                             capture_output=True, env=environment, check=True, timeout=20)
        self.assertEqual(json.loads(out.stdout), {"ok": True, "connected": False})


if __name__ == "__main__":
    unittest.main()
