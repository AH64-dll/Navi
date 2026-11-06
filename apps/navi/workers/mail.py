"""Email for Navi over IMAP and SMTP, standard library only.

    mail.py <op>   with a JSON object on stdin; prints one JSON object

Ops: status, connect, disconnect, check, read, contact, send.

The password arrives on stdin, never on the command line where `ps` shows it,
and is kept in ~/.config/navi/secrets.env (mode 0600). No op returns it.

Reading uses BODY.PEEK, so Navi hearing a mail does not mark it as read.
"""
import email
import email.policy
import email.utils
import html.parser
import imaplib
import json
import os
import re
import smtplib
import ssl
import sys
import tempfile
from email.message import EmailMessage
from pathlib import Path

import persona
import speech_config

TIMEOUT = 12
KEYS = ("NAVI_MAIL_ADDRESS", "NAVI_MAIL_PASSWORD", "NAVI_MAIL_IMAP_HOST", "NAVI_MAIL_SMTP_HOST")
ADDRESS = re.compile(r"^[A-Za-z0-9._+-]+@[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)+$")

# domain -> (IMAP host, SMTP host). Anything else tries imap.<domain>, smtp.<domain>.
HOSTS = {
    "gmail.com": ("imap.gmail.com", "smtp.gmail.com"),
    "googlemail.com": ("imap.gmail.com", "smtp.gmail.com"),
    "outlook.com": ("outlook.office365.com", "smtp-mail.outlook.com"),
    "hotmail.com": ("outlook.office365.com", "smtp-mail.outlook.com"),
    "live.com": ("outlook.office365.com", "smtp-mail.outlook.com"),
    "msn.com": ("outlook.office365.com", "smtp-mail.outlook.com"),
    "yahoo.com": ("imap.mail.yahoo.com", "smtp.mail.yahoo.com"),
    "icloud.com": ("imap.mail.me.com", "smtp.mail.me.com"),
    "me.com": ("imap.mail.me.com", "smtp.mail.me.com"),
    "aol.com": ("imap.aol.com", "smtp.aol.com"),
    "fastmail.com": ("imap.fastmail.com", "smtp.fastmail.com"),
    "zoho.com": ("imap.zoho.com", "smtp.zoho.com"),
}


class MailError(Exception):
    """A failure whose message is fit to show and say to the user."""


def hosts_for(address, imap_host="", smtp_host=""):
    domain = address.rsplit("@", 1)[-1].lower()
    known = HOSTS.get(domain, (f"imap.{domain}", f"smtp.{domain}"))
    return imap_host or known[0], smtp_host or known[1]


def isolated():
    """A test station must never reach the user's real account."""
    return os.environ.get("NAVI_ISOLATED") == "1" and not os.environ.get("NAVI_SECRETS_FILE")


def account():
    if isolated():
        return None
    address = speech_config.key("NAVI_MAIL_ADDRESS")
    password = speech_config.key("NAVI_MAIL_PASSWORD")
    if not address or not password:
        return None
    imap_host, smtp_host = hosts_for(
        address, speech_config.key("NAVI_MAIL_IMAP_HOST"), speech_config.key("NAVI_MAIL_SMTP_HOST"))
    return {"address": address, "password": password, "imap": imap_host, "smtp": smtp_host}


def need_account():
    found = account()
    if found is None:
        raise MailError("Your email is not connected. Add it in Settings under Connections.")
    return found


def login_error(host):
    return MailError(
        f"{host} refused that login. Most providers need an app password here, "
        "not your normal password. Gmail: myaccount.google.com/apppasswords.")


def open_imap(found):
    try:
        client = imaplib.IMAP4_SSL(found["imap"], timeout=TIMEOUT, ssl_context=ssl.create_default_context())
    except OSError as error:
        raise MailError(f"Could not reach {found['imap']}: {error}") from error
    try:
        client.login(found["address"], found["password"])
    except imaplib.IMAP4.error as error:
        raise login_error(found["imap"]) from error
    return client


def open_smtp(found):
    """Implicit TLS on 465, else STARTTLS on 587; both verify the certificate."""
    context = ssl.create_default_context()
    last = None
    for port, secure in ((465, True), (587, False)):
        try:
            if secure:
                client = smtplib.SMTP_SSL(found["smtp"], port, timeout=TIMEOUT, context=context)
            else:
                client = smtplib.SMTP(found["smtp"], port, timeout=TIMEOUT)
                client.starttls(context=context)
        except (OSError, smtplib.SMTPException) as error:
            last = error
            continue
        try:
            client.login(found["address"], found["password"])
        except smtplib.SMTPAuthenticationError as error:
            client.quit()
            raise login_error(found["smtp"]) from error
        return client
    raise MailError(f"Could not reach {found['smtp']}: {last}")


def write_secrets(values):
    """Set or (with None) remove keys in secrets.env, keeping every other line.

    The temp file is created 0600 and renamed over, so the password is never
    readable by others, not even for a moment.
    """
    path = Path(speech_config.SECRETS_FILE)
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    try:
        lines = path.read_text().splitlines()
    except OSError:
        lines = []
    kept = [line for line in lines if line.partition("=")[0].strip() not in values]
    kept += [f'{key}="{value}"' for key, value in values.items() if value is not None]
    handle, temporary = tempfile.mkstemp(prefix=".secrets-", dir=path.parent)
    try:
        with os.fdopen(handle, "w") as out:
            out.write("\n".join(kept) + ("\n" if kept else ""))
        os.replace(temporary, path)
    except BaseException:
        os.unlink(temporary)
        raise
    speech_config.reset_caches()


class _Text(html.parser.HTMLParser):
    def __init__(self):
        super().__init__()
        self.parts = []
        self.skipping = 0

    def handle_starttag(self, tag, attrs):
        if tag in ("script", "style", "head"):
            self.skipping += 1

    def handle_endtag(self, tag):
        if tag in ("script", "style", "head") and self.skipping:
            self.skipping -= 1
        if tag in ("p", "div", "br", "tr", "li"):
            self.parts.append("\n")

    def handle_data(self, data):
        if not self.skipping:
            self.parts.append(data)


def html_text(markup):
    parser = _Text()
    parser.feed(markup)
    return "".join(parser.parts)


def body_text(message):
    part = message.get_body(preferencelist=("plain", "html"))
    if part is None:
        return ""
    try:
        content = part.get_content()
    except (LookupError, ValueError):
        return ""
    if part.get_content_type() == "text/html":
        content = html_text(content)
    # Quoted history and signatures are not what the person wrote today.
    lines = []
    for line in content.splitlines():
        if line.startswith(">") or line.strip() == "--" or re.match(r"^On .+ wrote:\s*$", line):
            break
        lines.append(line.rstrip())
    return re.sub(r"\n{3,}", "\n\n", "\n".join(lines)).strip()


def sender_name(header):
    name, address = email.utils.parseaddr(str(header or ""))
    return name or address or "someone unknown"


def search(client, *criteria):
    status, data = client.search(None, *criteria)
    if status != "OK" or not data or not data[0]:
        return []
    return data[0].split()


def headers_of(client, number):
    status, data = client.fetch(number, "(BODY.PEEK[HEADER.FIELDS (FROM SUBJECT DATE)])")
    if status != "OK":
        return None
    for item in data:
        if isinstance(item, tuple):
            return email.message_from_bytes(item[1], policy=email.policy.default)
    return None


def select_inbox(client):
    status, _ = client.select("INBOX", readonly=True)
    if status != "OK":
        raise MailError("Could not open your inbox.")


def op_status(_params):
    found = account()
    if found is None:
        return {"connected": False}
    return {"connected": True, "address": found["address"], "imap": found["imap"], "smtp": found["smtp"]}


def op_connect(params):
    if isolated():
        raise MailError("Connecting an email account is disabled in isolated mode.")
    address = str(params.get("address") or "").strip()
    password = str(params.get("password") or "")
    if not ADDRESS.match(address):
        raise MailError("That does not look like an email address.")
    if not password.strip() or any(c in password for c in "\r\n\"'"):
        raise MailError("Enter the app password as it was given to you.")
    imap_host, smtp_host = hosts_for(
        address, str(params.get("imap_host") or "").strip(), str(params.get("smtp_host") or "").strip())
    found = {"address": address, "password": password, "imap": imap_host, "smtp": smtp_host}
    # Nothing is stored until both servers accept the login.
    open_imap(found).logout()
    open_smtp(found).quit()
    write_secrets({
        "NAVI_MAIL_ADDRESS": address,
        "NAVI_MAIL_PASSWORD": password,
        "NAVI_MAIL_IMAP_HOST": imap_host,
        "NAVI_MAIL_SMTP_HOST": smtp_host,
    })
    return {"connected": True, "address": address, "imap": imap_host, "smtp": smtp_host}


def op_disconnect(_params):
    write_secrets(dict.fromkeys(KEYS))
    return {"connected": False}


def op_check(_params):
    client = open_imap(need_account())
    try:
        select_inbox(client)
        unseen = search(client, "UNSEEN")
        if not unseen:
            return {"count": 0, "text": "You have no unread email."}
        rows = []
        for number in reversed(unseen[-5:]):
            message = headers_of(client, number)
            if message is not None:
                rows.append((sender_name(message["from"]), str(message["subject"] or "no subject")))
    finally:
        client.logout()
    count = len(unseen)
    plural = "email" if count == 1 else "emails"
    listing = "\n".join(f"- {who}: {subject}" for who, subject in rows)
    speech = f"You have {count} unread {plural}."
    if rows:
        speech += f" The newest is from {rows[0][0]}, about {rows[0][1]}."
    return {"count": count, "text": f"You have {count} unread {plural}.\n{listing}", "speech": speech}


def op_read(_params):
    client = open_imap(need_account())
    try:
        select_inbox(client)
        numbers = search(client, "ALL")
        if not numbers:
            return {"text": "Your inbox is empty."}
        # A large attachment must not be downloaded to read the first lines.
        status, data = client.fetch(numbers[-1], "(BODY.PEEK[]<0.300000>)")
        raw = next((item[1] for item in data if isinstance(item, tuple)), None) if status == "OK" else None
    finally:
        client.logout()
    if raw is None:
        raise MailError("Could not read your latest email.")
    message = email.message_from_bytes(raw, policy=email.policy.default)
    who = sender_name(message["from"])
    subject = str(message["subject"] or "no subject")
    body = body_text(message)
    shown = body if len(body) <= 3000 else body[:3000].rsplit(" ", 1)[0] + " …"
    said = persona.first_sentences(body, count=4, maximum=500) or "It has no text."
    return {
        "from": who,
        "subject": subject,
        "text": f"From {who}: {subject}\n\n{shown}".rstrip(),
        "speech": f"From {who}, about {subject}. {said}",
    }


def op_contact(params):
    """The address of someone who has written to the user, by name."""
    name = re.sub(r"[^\w .'-]", "", str(params.get("name") or "")).strip()
    if not name:
        raise MailError("Who should it go to?")
    client = open_imap(need_account())
    try:
        select_inbox(client)
        numbers = search(client, "FROM", f'"{name}"')
        found = {}
        for number in reversed(numbers[-20:]):
            message = headers_of(client, number)
            if message is None:
                continue
            shown, address = email.utils.parseaddr(str(message["from"] or ""))
            if address:
                found.setdefault(address.lower(), shown)
    finally:
        client.logout()
    if not found:
        return {"error": f"I have no address for {name}. Say the full address instead."}
    if len(found) > 1:
        listed = ", ".join(list(found)[:3])
        return {"error": f"{name} could be {listed}. Say the full address."}
    return {"address": next(iter(found)), "name": next(iter(found.values())) or name}


def op_send(params):
    found = need_account()
    to = str(params.get("to") or "").strip()
    if not ADDRESS.match(to):
        raise MailError(f"“{to}” is not an email address.")
    message = EmailMessage()
    message["From"] = found["address"]
    message["To"] = to
    # One line: a line break in the subject would write headers of its own.
    message["Subject"] = " ".join(str(params.get("subject") or "").split())
    message["Date"] = email.utils.formatdate(localtime=True)
    message["Message-ID"] = email.utils.make_msgid()
    message.set_content(str(params.get("body") or ""))
    client = open_smtp(found)
    try:
        client.send_message(message)
    except smtplib.SMTPException as error:
        raise MailError(f"The mail server would not send it: {error}") from error
    finally:
        client.quit()
    return {"sent": True, "to": to}


OPS = {
    "status": op_status,
    "connect": op_connect,
    "disconnect": op_disconnect,
    "check": op_check,
    "read": op_read,
    "contact": op_contact,
    "send": op_send,
}


def run(op, params):
    handler = OPS.get(op)
    if handler is None:
        return {"ok": False, "error": f"Unknown mail op {op}"}
    try:
        return {"ok": True, **handler(params)}
    except MailError as error:
        return {"ok": False, "error": str(error)}
    except (OSError, imaplib.IMAP4.error, smtplib.SMTPException) as error:
        # Type only: a server's message can quote the login it refused.
        return {"ok": False, "error": f"The mail server failed ({type(error).__name__}). Try again in a moment."}


def main():
    op = sys.argv[1] if len(sys.argv) > 1 else ""
    try:
        params = json.load(sys.stdin)
    except ValueError:
        params = {}
    print(json.dumps(run(op, params if isinstance(params, dict) else {})))


if __name__ == "__main__":
    main()
