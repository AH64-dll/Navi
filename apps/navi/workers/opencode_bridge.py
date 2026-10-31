"""opencode_bridge.py - minimal HTTP client for `opencode serve`.

Speaks to a running `opencode serve` HTTP API (basic auth, user "opencode",
JSON envelope {"data": ...}). Owns one reusable session titled "Navi" so the
assistant keeps conversational context across requests.

Config via env:
  NAVI_OPENCODE_URL       base URL, default http://127.0.0.1:49374
  NAVI_OPENCODE_PASSWORD  basic-auth password; default from
                          OPENCODE_SERVER_PASSWORD, then
                          ~/.config/opencode/service.json
  NAVI_OPENCODE_MODEL     "providerID/modelID" or "providerID/provider/modelID"
                          default opencode/deepseek-v4.1-flash-free
  NAVI_OPENCODE_AUTOSPAWN "0" disables auto-spawning `opencode serve`
"""

import base64
import json
import os
import secrets
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request

DEFAULT_URL = "http://127.0.0.1:49374"
DEFAULT_MODEL = "opencode/deepseek-v4.1-flash-free"
FALLBACK_MODEL = "commandcode/deepseek/deepseek-v4.1-flash"
SERVICE_FILE = os.path.expanduser("~/.config/opencode/service.json")
SESSION_TITLE = "Navi"
PROMPT_TIMEOUT_S = 120.0
SPAWN_TIMEOUT_S = 15.0
POLL_INTERVAL_S = 0.5
MODEL_RETRY_DELAYS_S = (0.4, 0.8, 1.6)

_bridge = None


class BridgeError(Exception):
    pass


def _log(msg):
    print(f"[opencode_bridge] {msg}", file=sys.stderr, flush=True)


def service_password(path=None):
    """Read the password of the user `opencode serve` service.

    The service writes it to ~/.config/opencode/service.json. Returns None
    when the file is missing or malformed.
    """
    path = path or os.environ.get("NAVI_OPENCODE_SERVICE") or SERVICE_FILE
    try:
        with open(path, "r", encoding="utf-8") as handle:
            value = json.load(handle).get("password")
    except (OSError, ValueError):
        return None
    return value if isinstance(value, str) and value else None


def _session_file():
    base = os.environ.get("NAVI_DATA_DIR") or os.path.expanduser(
        "~/.local/share/navi"
    )
    return os.path.join(base, "opencode-session")


def _split_model(ref):
    parts = ref.split("/")
    if len(parts) < 2:
        raise BridgeError(f"bad model ref {ref!r}; expected providerID/modelID")
    # The v2.2.x session model route takes Model.Ref: {providerID, id}, not
    # the providerID/modelID pair used in prompts and docs.
    return {"providerID": parts[0], "id": "/".join(parts[1:])}


class OpenCodeBridge:
    def __init__(
        self, base_url=None, password=None, model=None, autospawn=None, session_file=None
    ):
        self.base_url = (
            base_url or os.environ.get("NAVI_OPENCODE_URL") or DEFAULT_URL
        ).rstrip("/")
        self.password = (
            password
            if password is not None
            else os.environ.get("NAVI_OPENCODE_PASSWORD")
            or os.environ.get("OPENCODE_SERVER_PASSWORD")
            or service_password()
            or "navi"
        )
        self.model_ref = model or os.environ.get("NAVI_OPENCODE_MODEL") or DEFAULT_MODEL
        self.autospawn = (
            autospawn
            if autospawn is not None
            else os.environ.get("NAVI_OPENCODE_AUTOSPAWN", "1") != "0"
        )
        self.session_file = session_file if session_file is not None else _session_file()
        self.session_id = None
        self._token = base64.b64encode(
            f"opencode:{self.password}".encode()
        ).decode()

    # -- HTTP -------------------------------------------------------------

    def _request(self, method, path, body=None, timeout=10.0):
        url = self.base_url + path
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(url, data=data, method=method)
        req.add_header("Authorization", f"Basic {self._token}")
        if data is not None:
            req.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(req, timeout=timeout) as res:
                payload = json.loads(res.read().decode() or "null")
        except urllib.error.HTTPError as e:
            detail = e.read().decode(errors="replace")[:300]
            raise BridgeError(f"{method} {path} -> {e.code}: {detail}")
        except urllib.error.URLError as e:
            raise BridgeError(f"{method} {path} unreachable: {e.reason}")
        if isinstance(payload, dict) and "data" in payload:
            return payload["data"]
        return payload

    def _get(self, path, timeout=10.0):
        return self._request("GET", path, timeout=timeout)

    def _post(self, path, body, timeout=10.0):
        return self._request("POST", path, body=body, timeout=timeout)

    # -- lifecycle ----------------------------------------------------------

    def reachable(self):
        try:
            self._get("/api/agent", timeout=3.0)
            return True
        except BridgeError:
            return False

    def spawn(self):
        """Start `opencode serve` ourselves with a known password."""
        if not shutil.which("opencode"):
            raise BridgeError("opencode CLI not on PATH; cannot auto-spawn serve")
        self.password = secrets.token_urlsafe(24)
        self._token = base64.b64encode(
            f"opencode:{self.password}".encode()
        ).decode()
        env = dict(os.environ, OPENCODE_SERVER_PASSWORD=self.password)
        port = self.base_url.rsplit(":", 1)[-1]
        cmd = ["opencode", "serve"]
        if port.isdigit():
            cmd += ["--port", port]
        log = open("/tmp/navi-opencode-serve.log", "ab")
        subprocess.Popen(
            cmd, env=env, stdout=log, stderr=subprocess.STDOUT,
            stdin=subprocess.DEVNULL, start_new_session=True,
        )
        _log(f"spawned opencode serve ({' '.join(cmd)})")
        deadline = time.time() + SPAWN_TIMEOUT_S
        while time.time() < deadline:
            if self.reachable():
                return
            time.sleep(0.4)
        raise BridgeError(
            f"spawned opencode serve but {self.base_url} did not come up "
            f"within {SPAWN_TIMEOUT_S:.0f}s (see /tmp/navi-opencode-serve.log)"
        )

    def ensure_up(self):
        if self.reachable():
            return
        if not self.autospawn:
            raise BridgeError(
                f"opencode serve unreachable at {self.base_url}; "
                "start it or enable autospawn"
            )
        self.spawn()

    def ensure_session(self):
        self.ensure_up()
        if not self.session_id:
            self.session_id = self._load_session()
        if not self.session_id:
            data = self._post("/api/session", {"title": SESSION_TITLE})
            self.session_id = data["id"]
            self._save_session(self.session_id)
            _log(f"session {self.session_id}")
        try:
            self._set_model(self.model_ref)
        except BridgeError as error:
            if self.model_ref == FALLBACK_MODEL:
                raise
            _log(f"{self.model_ref} unavailable ({error}); using {FALLBACK_MODEL}")
            self._set_model(FALLBACK_MODEL)
            self.model_ref = FALLBACK_MODEL

    def _load_session(self):
        try:
            with open(self.session_file, "r", encoding="utf-8") as handle:
                session_id = handle.read().strip()
        except OSError:
            return None
        if not session_id.startswith("ses"):
            return None
        try:
            self._get(f"/api/session/{session_id}", timeout=5.0)
        except BridgeError:
            return None
        _log(f"reusing session {session_id}")
        return session_id

    def _save_session(self, session_id):
        try:
            os.makedirs(os.path.dirname(self.session_file), exist_ok=True)
            with open(self.session_file, "w", encoding="utf-8") as handle:
                handle.write(session_id)
        except OSError as error:
            _log(f"could not persist session id: {error}")

    def _set_model(self, ref):
        body = {"model": _split_model(ref)}
        for i, delay in enumerate([0.0] + list(MODEL_RETRY_DELAYS_S)):
            if delay:
                time.sleep(delay)
            try:
                self._post(f"/api/session/{self.session_id}/model", body)
                _log(f"model -> {ref}")
                return
            except BridgeError as e:
                if i == len(MODEL_RETRY_DELAYS_S):
                    raise
                _log(f"model set failed ({e}); retrying")

    # -- prompting ----------------------------------------------------------

    def _latest_assistant(self):
        msgs = self._get(
            f"/api/session/{self.session_id}/message"
            "?type=assistant&order=desc&limit=1"
        )
        return msgs[0] if isinstance(msgs, list) and msgs else None

    @staticmethod
    def _assistant_text(msg):
        parts = []
        for part in (msg or {}).get("content") or []:
            if part.get("type") == "text" and part.get("text"):
                parts.append(part["text"])
        return "\n".join(parts).strip()

    def prompt(self, text, system_prompt=None, timeout=PROMPT_TIMEOUT_S):
        """Send a user message; block until the assistant completes; return text."""
        self.ensure_session()
        prev = self._latest_assistant()
        prev_id = prev["id"] if prev else None

        full_text = f"{system_prompt}\n\n{text}" if system_prompt else text
        self._post(
            f"/api/session/{self.session_id}/prompt",
            {"text": full_text},
            timeout=30.0,
        )

        deadline = time.time() + timeout
        while time.time() < deadline:
            time.sleep(POLL_INTERVAL_S)
            msg = self._latest_assistant()
            if not msg or msg["id"] == prev_id:
                continue
            if msg.get("time", {}).get("completed"):
                out = self._assistant_text(msg)
                if out:
                    return out
        raise BridgeError(f"no assistant reply within {timeout:.0f}s")


def get_bridge():
    global _bridge
    if _bridge is None:
        _bridge = OpenCodeBridge()
    return _bridge


def ask(text, system_prompt=None):
    """Convenience one-shot: send text (with optional persona preamble)."""
    return get_bridge().prompt(text, system_prompt=system_prompt)


if __name__ == "__main__":
    import sys

    args = list(sys.argv[1:])
    persona_name = None
    if "--persona" in args:
        i = args.index("--persona")
        persona_name = args[i + 1] if i + 1 < len(args) else None
        args = args[:i] + args[i + 2 :]
    q = " ".join(args) or "Say exactly: ok"
    try:
        import persona
        p = persona.get(persona_name)
        system_prompt = p.system_prompt or None
    except ImportError:
        system_prompt = None
    print(ask(q, system_prompt=system_prompt))
