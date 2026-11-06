"""opencode_gateway.py - the real OpenCode client behind Navi.

Talks to the running `opencode serve` HTTP API (basic auth, JSON envelope
{"data": ...}) and exposes the pieces Navi needs:

  ask        - send a prompt, stream structured records, return the reply
  slash      - run an OpenCode slash command (/init, /review, /help, custom)
  models     - list chat models available to the account
  agents     - list primary agents
  commands   - list slash commands OpenCode knows about
  new        - start a fresh Navi session for the current project directory
  interrupt  - stop the current turn, retrying once
  permission - reply to a gated-tool approval request (endpoint unverified)

Streaming events come from GET /api/event (SSE). Verified upstream schema
(live probe): session.text.started/delta/ended, session.reasoning.*,
session.tool.input.started (carries id + name), session.tool.called (id +
input, NO name), session.tool.progress, session.tool.success (content +
metadata.exit), session.step.started (actual agent + model{id,providerID,
variant}), session.step.streamed, session.step.ended (tokens incl. reasoning,
cost, finish), session.usage.updated (cumulative), session.execution.succeeded
(observed terminal event), session.execution.failed, shell.created/exited.
`tool.called.executed` is NOT an approval signal (it was false while a shell
ran). `session.permission.*` is unverified and matched defensively.

Structured records are emitted as JSON lines on stdout (only with --stream,
except "warning"/"error"/"done" which are always printed):

  {"type":"delta","text":...}                       assistant text chunk
  {"type":"reasoning","text":...}                   model reasoning chunk
  {"type":"tool_status","id":..,"status":..,..}     transient tool progress
  {"type":"tool","id":..,"name":..,"status":..,     exactly ONE final record
     "input":..,"output":..,"exit":..,"final":true}   per tool-call id
  {"type":"permission","id","tool","input"}         approval request (defensive)
  {"type":"warning","text":...}                     e.g. ignored directory field
  {"type":"done","text":reply,"session":id,         terminal record
     "usage":{...},"session_usage":{...},
     "finish":..,"actual_model":{...},"actual_agent":..}
  {"type":"error","text":...}                       fatal failure

The final reply prefers `session.text.ended` (it carries the full text) once
the turn is terminal; the message store is the fallback so a dropped SSE
connection never loses text. An incomplete store message is never returned.

Sessions are stored per working directory in
$NAVI_DATA_DIR/opencode-session as {"<directory>": {"id","model","agent"}};
a legacy plain-id file is migrated on first use. Model/agent are POSTed only
when the tuple changes (one configure per session unless settings move).

The session-create body carries "directory" (unverified upstream). After
create, GET /api/location verifies it; when the server ignores the field the
prompt is prefixed with "[working directory: <path>]" and a warning record
is emitted. Never assume the field works.

Config via env:
  NAVI_OPENCODE_URL       base URL, default http://127.0.0.1:49374
  NAVI_OPENCODE_PASSWORD  basic-auth password; default from
                          OPENCODE_SERVER_PASSWORD, then
                          ~/.config/opencode/service.json
  NAVI_OPENCODE_MODEL     "providerID/modelID" default
                          commandcode/deepseek/deepseek-v4.1-flash
  NAVI_OPENCODE_AGENT     session agent, default "navi" when present
  NAVI_OPENCODE_AUTOSPAWN "0" disables starting a server
  NAVI_PROJECT_DIR        working directory for the session (--directory wins)

One server: on the default URL a stopped server is started through the
navi-opencode.service user unit, which everything else on the machine shares.
A plain `opencode serve` is only the fallback for a source checkout without it.
"""
import base64
import http.client
import json
import mimetypes
import os
import re
import shutil
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

DEFAULT_URL = "http://127.0.0.1:49374"
DEFAULT_MODEL = "commandcode/deepseek/deepseek-v4.1-flash"
DEFAULT_AGENT = "navi"
SERVICE_FILE = os.path.expanduser("~/.config/opencode/service.json")
SERVER_UNIT = "navi-opencode.service"
SESSION_TITLE = "Navi"
PROMPT_TIMEOUT_S = 180.0
POLL_INTERVAL_S = 0.25
SPAWN_TIMEOUT_S = 15.0
WAIT_IDLE_TIMEOUT_S = 15.0
REPLY_SETTLE_S = 5.0
TOOL_OUTPUT_CAP = 8192
ATTACHMENT_LIMIT = 4
ATTACHMENT_MAX_BYTES = 2 * 1024 * 1024
INTERRUPT_RETRY_S = 1.0
IMAGE_SUFFIXES = {".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp"}

# Every observed terminal execution event, plus plausible siblings; the
# catch-all below also clears busy on any unknown `session.execution.*`.
TERMINAL_EXECUTION = {
    "session.execution.succeeded",
    "session.execution.failed",
    "session.execution.idle",
    "session.execution.cancelled",
    "session.execution.canceled",
    "session.execution.interrupted",
    "session.execution.aborted",
    "session.execution.stopped",
    "session.execution.ended",
}
# Unverified permission event family (plan BUG-006): accept plausible names.
PERMISSION_EVENTS = {
    "session.permission.requested",
    "session.permission.ask",
    "session.permission.pending",
    "session.permission.required",
    "session.approval.requested",
    "session.approval.pending",
    "session.tool.permission",
    "session.tool.ask",
}
# Candidate reply endpoints for a permission decision; the first one that the
# server accepts wins and is reported (plan BUG-006, schema unverified).
PERMISSION_ENDPOINTS = [
    "/api/session/{session}/permission",
    "/api/permission/{id}",
    "/api/session/{session}/permission/{id}",
    "/api/permission/{id}/reply",
    "/api/session/{session}/approval/{id}",
]
SENSITIVE = re.compile(r"(?i)(token|secret|key|password)")
SENSITIVE_ASSIGNMENT = re.compile(
    r"(?i)((?:token|secret|key|password)[\w.-]*\s*[:=]\s*)\S+")


class GatewayError(Exception):
    def __init__(self, message, status=None):
        super().__init__(message)
        self.status = status


def _log(msg):
    print(f"[opencode_gateway] {msg}", file=sys.stderr, flush=True)


def service_password(path=None):
    path = path or os.environ.get("NAVI_OPENCODE_SERVICE") or SERVICE_FILE
    try:
        with open(path, "r", encoding="utf-8") as handle:
            value = json.load(handle).get("password")
    except (OSError, ValueError):
        return None
    return value if isinstance(value, str) and value else None


def _session_file():
    base = os.environ.get("NAVI_DATA_DIR") or os.path.expanduser("~/.local/share/navi")
    return os.path.join(base, "opencode-session")


def parse_ref(ref):
    """'provider/model' -> {'providerID': provider, 'id': model}."""
    provider, _, model = ref.partition("/")
    if not provider or not model:
        raise GatewayError(f"bad model ref {ref!r}; expected provider/model")
    return {"providerID": provider, "id": model}


def _truncate(text, limit=TOOL_OUTPUT_CAP):
    if text is None:
        return None
    text = str(text)
    if len(text) <= limit:
        return text
    return text[:limit] + f"\n…[truncated {len(text) - limit} chars]"


def _redact_text(text):
    """Redact `secret=…`-style values inside a string."""
    if not isinstance(text, str):
        return text
    return SENSITIVE_ASSIGNMENT.sub(r"\1[redacted]", text)


def _redact(value):
    """Deep-redact keys/values matching token|secret|key|password."""
    if isinstance(value, dict):
        out = {}
        for key, item in value.items():
            if SENSITIVE.search(str(key)):
                out[key] = "[redacted]"
            else:
                out[key] = _redact(item)
        return out
    if isinstance(value, list):
        return [_redact(item) for item in value]
    return _redact_text(value)


def _event_session(data):
    """Session id for an SSE payload, wherever the server puts it."""
    if not isinstance(data, dict):
        return None
    for key in ("sessionID", "sessionId", "session_id", "session"):
        if isinstance(data.get(key), str):
            return data[key]
    inner = data.get("session")
    if isinstance(inner, dict) and isinstance(inner.get("id"), str):
        return inner["id"]
    return None


def _call_id(data):
    if not isinstance(data, dict):
        return None
    for key in ("id", "callID", "callId", "toolCallId", "call", "toolID", "toolId"):
        if data.get(key) is not None:
            return str(data[key])
    inner = data.get("call")
    if isinstance(inner, dict) and inner.get("id") is not None:
        return str(inner["id"])
    return None


def _guess_mime(path):
    mime, _ = mimetypes.guess_type(path)
    if mime:
        return mime
    suffix = os.path.splitext(path)[1].lower()
    if suffix in IMAGE_SUFFIXES:
        return f"image/{suffix.lstrip('.').replace('jpg', 'jpeg')}"
    return "text/plain"


def _attachment_part(path):
    """Build one prompt part; {"skip": reason} when the file cannot travel."""
    mime = _guess_mime(path)
    kind = "image" if mime.startswith("image/") else "file"
    name = os.path.basename(path) or path
    try:
        size = os.path.getsize(path)
    except OSError as error:
        return {"skip": f"{path}: {error}"}
    if size > ATTACHMENT_MAX_BYTES:
        return {"skip": f"{path}: {size} bytes over {ATTACHMENT_MAX_BYTES}"}
    try:
        with open(path, "rb") as handle:
            raw = handle.read()
    except OSError as error:
        return {"skip": f"{path}: {error}"}
    if kind == "image":
        return {"type": "image", "name": name, "mime": mime, "filename": name,
                "url": f"data:{mime};base64,{base64.b64encode(raw).decode()}"}
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError:
        text = raw.decode("utf-8", errors="replace")
    return {"type": "file", "name": name, "mime": mime, "filename": name,
            "text": text, "url": f"file://{os.path.abspath(path)}"}


class Gateway:
    def __init__(self, base_url=None, password=None, model=None, agent=None,
                 autospawn=None, session_file=None, directory=None):
        chosen_url = base_url or os.environ.get("NAVI_OPENCODE_URL")
        self.base_url = (chosen_url or DEFAULT_URL).rstrip("/")
        # An isolated station is a test. It never starts a server, and it reaches one
        # only when handed a URL: a server it started would sit on the user's port with
        # a password the user's own Navi does not know.
        isolated = os.environ.get("NAVI_ISOLATED") == "1"
        self.sealed = isolated and not chosen_url
        self.password = (password if password is not None
                         else os.environ.get("NAVI_OPENCODE_PASSWORD")
                         or os.environ.get("OPENCODE_SERVER_PASSWORD")
                         or service_password() or "navi")
        self.model_ref = model or os.environ.get("NAVI_OPENCODE_MODEL") or DEFAULT_MODEL
        self.agent = agent or os.environ.get("NAVI_OPENCODE_AGENT") or DEFAULT_AGENT
        self.autospawn = not isolated and (autospawn if autospawn is not None
                                           else os.environ.get("NAVI_OPENCODE_AUTOSPAWN", "1") != "0")
        self.session_file = session_file if session_file is not None else _session_file()
        directory = directory if directory is not None else os.environ.get("NAVI_PROJECT_DIR")
        self.directory = os.path.abspath(os.path.expanduser(directory)) if directory else ""
        self.session_id = None
        self.warnings = []
        self._token = base64.b64encode(f"opencode:{self.password}".encode()).decode()
        self._records = []
        self._records_lock = threading.Lock()
        self._events_running = False
        self._events_ready = threading.Event()
        self._events_alive = False
        self._busy = False
        self._terminal = None          # last execution terminal kind (short name)
        self._failure = None
        self._last_activity = 0.0
        self._text_open = False
        self._ended_texts = []
        self._tools = {}               # call id -> merged record
        self._pending = {}             # call id -> buffered input bits
        self._permissions = []
        self._shell_cwds = []
        self._last_usage = None        # step.ended tokens/cost/finish
        self._session_usage = None     # usage.updated cumulative
        self._actual_model = None
        self._actual_agent = None
        self._dir_prefix = False       # server ignored `directory`: prefix prompts
        self.last_result = None        # dict handed to the CLI "done" line

    # -- HTTP ---------------------------------------------------------------

    def _request(self, method, path, body=None, timeout=10.0):
        if self.sealed:
            raise GatewayError("An isolated station has no OpenCode server")
        data = json.dumps(body).encode() if body is not None else None
        request = urllib.request.Request(self.base_url + path, data=data, method=method)
        request.add_header("Authorization", f"Basic {self._token}")
        if data is not None:
            request.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                payload = json.loads(response.read().decode() or "null")
        except urllib.error.HTTPError as error:
            detail = error.read().decode(errors="replace")[:300]
            raise GatewayError(f"{method} {path} -> {error.code}: {detail}", status=error.code)
        except (urllib.error.URLError, OSError) as error:
            reason = getattr(error, "reason", error)
            raise GatewayError(f"{method} {path} unreachable: {reason}")
        if isinstance(payload, dict) and "data" in payload:
            return payload["data"]
        return payload

    def _get(self, path, timeout=10.0):
        return self._request("GET", path, timeout=timeout)

    def _post(self, path, body=None, timeout=10.0):
        return self._request("POST", path, body=body, timeout=timeout)

    # -- warnings -------------------------------------------------------------

    def _warn(self, text):
        _log(f"warning: {text}")
        self.warnings.append(text)
        _emit_line({"type": "warning", "text": text})

    # -- lifecycle ------------------------------------------------------------

    def reachable(self):
        try:
            self._get("/api/agent", timeout=3.0)
            return True
        except GatewayError:
            return False

    def _start_shared_server(self):
        """Ask systemd for the one server everything shares. False when it isn't installed."""
        if self.base_url != DEFAULT_URL or not shutil.which("systemctl"):
            return False
        try:
            started = subprocess.run(["systemctl", "--user", "start", SERVER_UNIT], stdin=subprocess.DEVNULL,
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=SPAWN_TIMEOUT_S)
        except (OSError, subprocess.SubprocessError):
            return False
        return started.returncode == 0

    def _wait_reachable(self, failure):
        deadline = time.time() + SPAWN_TIMEOUT_S
        while time.time() < deadline:
            if self.reachable():
                return
            time.sleep(0.4)
        raise GatewayError(failure)

    def spawn(self):
        if self._start_shared_server():
            _log(f"started {SERVER_UNIT}")
            self._wait_reachable(f"{SERVER_UNIT} did not answer at {self.base_url} "
                                 f"(its log: journalctl --user -u {SERVER_UNIT})")
            return
        if not shutil.which("opencode"):
            raise GatewayError("OpenCode is not installed: there is no `opencode` on PATH to start its server")
        # Keep the password stable: a later Navi process reads the same
        # service.json/env and must authenticate to the server we started.
        env = dict(os.environ, OPENCODE_SERVER_PASSWORD=self.password)
        port = self.base_url.rsplit(":", 1)[-1]
        command = ["opencode", "serve"]
        if port.isdigit():
            command += ["--port", port]
        with open("/tmp/navi-opencode-serve.log", "ab") as log:
            subprocess.Popen(command, env=env, stdout=log, stderr=subprocess.STDOUT,
                             stdin=subprocess.DEVNULL, start_new_session=True)
        _log(f"spawned opencode serve ({' '.join(command)})")
        self._wait_reachable(f"OpenCode did not start at {self.base_url} (its log: /tmp/navi-opencode-serve.log)")

    def ensure_up(self):
        try:
            self._get("/api/agent", timeout=3.0)
            return
        except GatewayError as error:
            # A server answers and refuses us, so a second one could only fail on
            # the port this one holds.
            if error.status in (401, 403):
                raise GatewayError(f"Another OpenCode server is holding {self.base_url} and refused Navi's "
                                   "password. Stop that server and try again.") from None
        if not self.autospawn:
            raise GatewayError(f"OpenCode is not running at {self.base_url}")
        self.spawn()

    def known_agents(self):
        try:
            agents = self._get("/api/agent", timeout=5.0) or []
        except GatewayError:
            return []
        return [a.get("id") for a in agents if isinstance(a, dict)]

    def server_directory(self):
        """GET /api/location -> reported cwd, or None when unverifiable."""
        try:
            data = self._get("/api/location", timeout=5.0)
        except GatewayError:
            return None
        if isinstance(data, dict):
            for key in ("directory", "cwd", "path", "worktree"):
                if isinstance(data.get(key), str) and data[key]:
                    return data[key]
        elif isinstance(data, str) and data:
            return data
        return None

    def _title(self):
        name = os.path.basename(self.directory.rstrip("/")) if self.directory else ""
        return f"{SESSION_TITLE} · {name}" if name else SESSION_TITLE

    # -- session persistence ----------------------------------------------------
    # File shape: {"<directory or 'default'>": {"id","model","agent"}}.
    # A legacy file containing a bare "ses…" id is migrated in place.

    def _session_map(self):
        try:
            with open(self.session_file, "r", encoding="utf-8") as handle:
                raw = handle.read().strip()
        except OSError:
            return {}
        if not raw:
            return {}
        try:
            data = json.loads(raw)
        except ValueError:
            data = None
        if isinstance(data, dict):
            return {str(k): v for k, v in data.items() if isinstance(v, dict)}
        if raw.startswith("ses"):  # legacy plain-id file
            return {"default": {"id": raw}}
        return {}

    def _write_session_map(self, mapping):
        try:
            os.makedirs(os.path.dirname(self.session_file) or ".", exist_ok=True)
            with open(self.session_file, "w", encoding="utf-8") as handle:
                handle.write(json.dumps(mapping))
        except OSError as error:
            _log(f"could not persist session map: {error}")

    def _session_key(self):
        return self.directory or "default"

    def _stored_entry(self):
        entry = self._session_map().get(self._session_key())
        if not entry and self.directory:
            # A migrated legacy id belongs to whatever directory is active.
            entry = self._session_map().get("default")
        if isinstance(entry, dict) and str(entry.get("id", "")).startswith("ses"):
            return entry
        return None

    def _load_session(self):
        """Return a reusable stored session id for this directory, or None."""
        entry = self._stored_entry()
        if not entry:
            return None
        session_id = entry["id"]
        try:
            self._get(f"/api/session/{session_id}", timeout=5.0)
        except GatewayError:
            return None
        _log(f"reusing session {session_id} for {self._session_key()}")
        return session_id

    def _save_session(self):
        mapping = self._session_map()
        # A legacy "default" slot holding THIS session id migrates to the
        # directory key; a different project's default session stays.
        if (self._session_key() != "default"
                and mapping.get("default", {}).get("id") == self.session_id):
            mapping.pop("default")
        mapping[self._session_key()] = {
            "id": self.session_id,
            "model": self.model_ref,
            "agent": self.agent,
        }
        self._write_session_map(mapping)

    # -- session setup ----------------------------------------------------------

    def _create_session(self):
        body = {"title": self._title()}
        if self.directory:
            # Unverified upstream field; verified via /api/location afterwards.
            body["directory"] = self.directory
        data = self._post("/api/session", body)
        self.session_id = data["id"]
        self._dir_prefix = False
        if self.directory:
            reported = self.server_directory()
            if reported is None:
                self._dir_prefix = True
                self._warn("OpenCode does not expose /api/location; "
                           f"cannot verify directory={self.directory}; "
                           "prompts will carry a [working directory] prefix")
            elif os.path.abspath(reported) != os.path.abspath(self.directory):
                self._dir_prefix = True
                self._warn(f"OpenCode ignored directory={self.directory} "
                           f"(server reports {reported}); prompts will carry "
                           "a [working directory] prefix")
        self._save_session()
        _log(f"session {self.session_id} dir={self._session_key()}")
        return self.session_id

    def _configure(self, force=False):
        """Apply model/agent only when the stored tuple differs (BUG-011)."""
        if not self.session_id:
            return
        entry = self._stored_entry() or {}
        wants = {"model": self.model_ref, "agent": self.agent}
        applied = {"model": entry.get("model"), "agent": entry.get("agent")}
        if not force and applied == wants:
            return
        if entry.get("model") != self.model_ref or force:
            self._post(f"/api/session/{self.session_id}/model",
                       {"model": parse_ref(self.model_ref)})
            entry["model"] = self.model_ref
        if entry.get("agent") != self.agent or force:
            if self.agent and self.agent in self.known_agents():
                self._post(f"/api/session/{self.session_id}/agent",
                           {"agent": self.agent})
            else:
                _log(f"agent {self.agent!r} not loaded; session keeps its agent")
            entry["agent"] = self.agent
        mapping = self._session_map()
        mapping[self._session_key()] = {
            "id": self.session_id, "model": entry.get("model"),
            "agent": entry.get("agent"),
        }
        self._write_session_map(mapping)

    def ensure_session(self):
        self.ensure_up()
        if not self.session_id:
            self.session_id = self._load_session()
            if not self.session_id:
                self._create_session()
                self._configure(force=True)
                return self.session_id
        self._configure()
        return self.session_id

    def new_session(self):
        self.ensure_up()
        self._create_session()
        self._configure(force=True)
        return self.session_id

    def wait_idle(self, timeout=WAIT_IDLE_TIMEOUT_S):
        """Leave no wedged turn behind: interrupt, wait for a terminal event,
        rotate to a fresh session on timeout (BUG-008). Returns True when the
        session is usable."""
        if self._busy and self._events_alive:
            try:
                self.interrupt()
            except GatewayError as error:
                _log(f"wait_idle interrupt failed: {error}")
            deadline = time.time() + timeout
            while time.time() < deadline:
                if not self._busy:
                    break
                time.sleep(0.1)
        if self._busy and self._events_alive:
            # The server refused to end the turn; the session is poisoned.
            self._warn("turn did not stop; rotating to a fresh session")
            try:
                self.new_session()
            except GatewayError as error:
                raise GatewayError(f"session still busy and reset failed: {error}")
        self._busy = False
        self._terminal = None
        return True

    def interrupt(self):
        """POST /interrupt, check the response, retry once after 1 s. Stop never starts
        a server: with none running, there is no turn to stop."""
        if not self.session_id:
            self.session_id = self._load_session()
        if not self.session_id:
            return False
        path = f"/api/session/{self.session_id}/interrupt"
        try:
            self._post(path)
        except GatewayError as first:
            time.sleep(INTERRUPT_RETRY_S)
            try:
                self._post(path)
            except GatewayError as second:
                _log(f"interrupt failed twice: {first}; {second}")
                return False
        self._busy = False
        return True

    def permission_reply(self, permission_id, decision):
        """POST a permission decision to candidate endpoints (schema
        unverified, plan BUG-006). Returns {"replied":bool,"endpoint","detail"}."""
        self.ensure_session()
        decision = str(decision or "").strip().lower()
        if decision not in ("allow", "deny"):
            raise GatewayError("decision must be allow or deny")
        body = {"id": permission_id, "decision": decision,
                "approved": decision == "allow"}
        errors = []
        for template in PERMISSION_ENDPOINTS:
            path = template.format(session=self.session_id, id=permission_id)
            try:
                self._post(path, body, timeout=10.0)
                return {"replied": True, "endpoint": path}
            except GatewayError as error:
                errors.append(f"{path}: {error}")
        return {"replied": False, "error": "; ".join(errors)}

    # -- SSE records ------------------------------------------------------------

    def start_event_stream(self):
        """Background SSE reader turning server events into records."""
        if self._events_running:
            return
        self._events_running = True
        threading.Thread(target=self._read_events, daemon=True).start()

    def _emit(self, record):
        with self._records_lock:
            self._records.append(record)
        self._last_activity = time.time()

    def _tool_record(self, call_id):
        return self._tools.setdefault(str(call_id), {
            "id": str(call_id), "name": None, "status": "pending",
            "input": None, "output": None, "exit": None, "finished": False})

    def _handle_event(self, kind, data):
        if not isinstance(data, dict):
            data = {}
        sid = _event_session(data)
        # Events carrying another session's id are ignored; id-less events
        # (some shell.* payloads) are accepted — this gateway owns one session.
        if sid and self.session_id and sid != self.session_id:
            return
        self._last_activity = time.time()

        if kind == "session.step.started":
            self._busy = True
            self._terminal = None
            agent = data.get("agent") or (data.get("step") or {}).get("agent")
            model = data.get("model") or (data.get("step") or {}).get("model")
            if isinstance(model, dict) and model:
                self._actual_model = {k: model.get(k) for k in
                                      ("id", "providerID", "variant") if model.get(k)}
            if isinstance(agent, str) and agent:
                self._actual_agent = agent
            return
        if kind == "session.step.streamed":
            self._busy = True
            return
        if kind == "session.step.ended":
            step = data.get("step") if isinstance(data.get("step"), dict) else data
            usage = {}
            tokens = step.get("tokens")
            if isinstance(tokens, dict):
                usage["tokens"] = dict(tokens)
            for key in ("cost", "finish", "reason"):
                if step.get(key) is not None:
                    usage[key] = step[key]
            if usage:
                self._last_usage = usage
            return
        if kind == "session.usage.updated":
            usage = data.get("usage") if isinstance(data.get("usage"), dict) else data
            self._session_usage = {k: v for k, v in usage.items()
                                   if k not in ("sessionID", "sessionId")}
            return
        if kind.startswith("session.execution."):
            name = kind[len("session.execution."):]
            self._busy = False
            self._terminal = name
            if kind == "session.execution.failed" or "fail" in name or "error" in name:
                error = data.get("error") or data.get("message") or {}
                self._failure = (
                    error.get("message") if isinstance(error, dict) else str(error)
                ) or "OpenCode failed to run the model"
            return
        if kind == "session.text.started":
            self._text_open = True
            self._busy = True
            return
        if kind == "session.text.delta":
            if data.get("delta"):
                self._emit({"type": "delta", "text": data["delta"]})
            return
        if kind == "session.text.ended":
            self._text_open = False
            text = data.get("text") or data.get("delta")
            if isinstance(text, str) and text:
                self._ended_texts.append(text)
            return
        if kind == "session.reasoning.started":
            return
        if kind == "session.reasoning.delta":
            if data.get("delta"):
                self._emit({"type": "reasoning", "text": data["delta"]})
            return
        if kind == "session.reasoning.ended":
            return
        if kind == "session.tool.input.started":
            call_id = _call_id(data)
            if call_id is None:
                return
            pending = self._pending.setdefault(call_id, {})
            for key in ("name", "tool", "toolName"):
                if data.get(key):
                    pending["name"] = str(data[key])
                    break
            record = self._tool_record(call_id)
            if pending.get("name") and not record["name"]:
                record["name"] = pending["name"]
            return
        if kind == "session.tool.input.ended":
            call_id = _call_id(data)
            if call_id is None:
                return
            pending = self._pending.setdefault(call_id, {})
            text = data.get("text") or data.get("input")
            if isinstance(text, str) and text:
                pending["input_text"] = text
            elif isinstance(text, dict):
                pending["input"] = text
            return
        if kind == "session.tool.called":
            call_id = _call_id(data)
            if call_id is None:
                return
            record = self._tool_record(call_id)
            pending = self._pending.pop(call_id, {})
            if pending.get("name") and not record["name"]:
                record["name"] = pending["name"]
            payload = data.get("input")
            if payload is None and pending.get("input") is not None:
                payload = pending["input"]
            if payload is None and pending.get("input_text"):
                try:
                    payload = json.loads(pending["input_text"])
                except ValueError:
                    payload = pending["input_text"]
            if payload is not None:
                record["input"] = _redact(payload)
            record["status"] = "running"
            self._emit({"type": "tool_status", "id": call_id,
                        "name": record["name"], "status": "running",
                        "input": record["input"]})
            return
        if kind == "session.tool.progress":
            call_id = _call_id(data)
            if call_id is None:
                return
            record = self._tool_record(call_id)
            progress = data.get("progress") or data.get("text") or data.get("output")
            self._emit({"type": "tool_status", "id": call_id,
                        "name": record["name"], "status": "running",
                        "progress": _truncate(_redact_text(str(progress)))
                        if progress is not None else None})
            return
        if kind in ("session.tool.success", "session.tool.failure",
                    "session.tool.error", "session.tool.failed"):
            call_id = _call_id(data)
            if call_id is None:
                return
            record = self._tool_record(call_id)
            failed = kind != "session.tool.success"
            content = data.get("content", data.get("output"))
            if isinstance(content, list):
                content = "\n".join(str(part.get("text", part)) if isinstance(part, dict)
                                    else str(part) for part in content)
            if content is not None:
                record["output"] = _truncate(_redact_text(str(content)))
            error = data.get("error") or data.get("message")
            if failed and error:
                record["output"] = _truncate(_redact_text(
                    str(error.get("message") if isinstance(error, dict) else error)))
            metadata = data.get("metadata") if isinstance(data.get("metadata"), dict) else {}
            if metadata.get("exit") is not None:
                record["exit"] = metadata["exit"]
            record["status"] = ("failed" if failed else
                                "completed" if record["exit"] in (None, 0) else
                                f"exit {record['exit']}")
            record["finished"] = True
            # Exactly one final record per tool-call id (plan BUG-001).
            self._emit({"type": "tool", "id": call_id, "name": record["name"],
                        "status": record["status"], "input": record["input"],
                        "output": record["output"], "exit": record["exit"],
                        "final": True})
            return
        if kind == "shell.created":
            shell = data.get("shell") if isinstance(data.get("shell"), dict) else data
            cwd = (shell.get("cwd") or (shell.get("info") or {}).get("cwd")
                   if isinstance(shell, dict) else None)
            if isinstance(cwd, str) and cwd:
                self._shell_cwds.append(cwd)
                # shell cwd is ground truth for the directory fallback: if the
                # server actually honored `directory`, drop the prompt prefix.
                if (self.directory and self._dir_prefix
                        and os.path.abspath(cwd) == os.path.abspath(self.directory)):
                    self._dir_prefix = False
                    _log("shell.created confirms server-side directory")
            return
        if kind == "shell.exited":
            return
        if kind in PERMISSION_EVENTS or ".permission." in kind or kind.endswith(".permission"):
            call_data = data.get("permission") if isinstance(data.get("permission"), dict) else data
            self._permissions.append(call_data)
            self._emit({"type": "permission",
                        "id": _call_id(call_data) or call_data.get("id"),
                        "tool": call_data.get("tool") or call_data.get("name"),
                        "input": _redact(call_data.get("input"))})
            return

    def _read_events(self):
        parsed = urllib.parse.urlsplit(self.base_url)
        connection = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=90)
        try:
            connection.request("GET", "/api/event", headers={
                "Authorization": f"Basic {self._token}", "Accept": "text/event-stream"})
            response = connection.getresponse()
            self._events_alive = True
            self._events_ready.set()
            while True:
                line = response.readline()
                if not line:
                    break
                line = line.decode(errors="replace").strip()
                if not line.startswith("data:"):
                    continue
                try:
                    event = json.loads(line[5:].strip())
                except ValueError:
                    continue
                kind = event.get("type")
                if not isinstance(kind, str):
                    continue
                try:
                    self._handle_event(kind, event.get("data") or {})
                except Exception as error:  # a bad event must not kill the reader
                    _log(f"dropped event {kind}: {error}")
        except (OSError, http.client.HTTPException):
            pass
        finally:
            self._events_alive = False

    def _drain_records(self):
        with self._records_lock:
            out, self._records = self._records, []
        return out

    # -- prompting --------------------------------------------------------------

    def _assistant_messages(self, limit=3):
        messages = self._get(
            f"/api/session/{self.session_id}/message"
            f"?type=assistant&order=desc&limit={limit}")
        return messages if isinstance(messages, list) else []

    @staticmethod
    def _text(message):
        return "\n".join(
            part.get("text", "") for part in (message or {}).get("content") or []
            if part.get("type") == "text" and part.get("text")
        ).strip()

    def _latest_reply(self, previous_ids):
        """Newest *completed* store message — an incomplete message is never
        a reply (BUG-008)."""
        for message in self._assistant_messages(limit=5):
            if message.get("id") in previous_ids:
                continue
            if not (message.get("time") or {}).get("completed"):
                continue
            text = self._text(message)
            if text:
                return text
        return None

    def _wait_reply(self, previous_ids, timeout=PROMPT_TIMEOUT_S, emit=None):
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self._failure:
                raise GatewayError(f"OpenCode: {self._failure}")
            if emit is not None:
                for record in self._drain_records():
                    emit(record)
            # Preferred terminator (plan BUG-001 step 6): once the turn is
            # terminal, the last session.text.ended carries the full text.
            if not self._busy and not self._text_open and self._ended_texts:
                reply = self._ended_texts[-1].strip()
                if reply:
                    return reply
            if not self._busy or not self._events_alive:
                reply = self._latest_reply(previous_ids)
                if reply:
                    return reply
            # SSE can miss the terminal event: after REPLY_SETTLE_S of radio
            # silence with no open text, trust the store alone.
            if self._busy and self._events_alive and not self._text_open:
                quiet = time.time() - max(self._last_activity, self._turn_started)
                if quiet > REPLY_SETTLE_S:
                    reply = self._latest_reply(previous_ids)
                    if reply:
                        self._warn("SSE terminal event missing; "
                                   "reply read from the message store")
                        return reply
            time.sleep(POLL_INTERVAL_S)
        raise GatewayError(f"no assistant reply within {timeout:.0f}s")

    def _begin_turn(self):
        if self._events_running:
            self._events_ready.wait(2.0)
        self.wait_idle()
        self._busy = True
        self._terminal = None
        self._failure = None
        self._text_open = False
        self._ended_texts = []
        self._tools = {}
        self._pending = {}
        self._permissions = []
        self._last_usage = None
        self._actual_model = None
        self._actual_agent = None
        self._turn_started = time.time()
        self._last_activity = self._turn_started
        self._drain_records()

    def _finish_turn(self, reply, source):
        usage = self._last_usage or {}
        self.last_result = {
            "text": reply,
            "source": source,
            "session": self.session_id,
            "usage": usage or None,
            "session_usage": self._session_usage,
            "finish": usage.get("finish") or self._terminal,
            "actual_model": self._actual_model,
            "actual_agent": self._actual_agent,
            "permissions": self._permissions or None,
            "warnings": self.warnings or None,
        }
        return self.last_result

    def ask(self, text, timeout=PROMPT_TIMEOUT_S, emit=None, attachments=None):
        """Send a prompt; return the completed assistant reply."""
        self.ensure_session()
        self._begin_turn()
        if self._dir_prefix and self.directory:
            text = f"[working directory: {self.directory}]\n\n{text}"
        previous = {message.get("id") for message in self._assistant_messages(limit=5)}
        body = {"text": text}
        parts = None
        if attachments:
            parts = [{"type": "text", "text": text}]
            skipped = []
            for path in attachments[:ATTACHMENT_LIMIT]:
                part = _attachment_part(path)
                if part.get("skip"):
                    skipped.append(part["skip"])
                else:
                    parts.append(part)
            for note in skipped:
                self._warn(f"attachment skipped: {note}")
            if len(parts) > 1:
                body["parts"] = parts
            else:
                parts = None
        if parts:
            try:
                self._post(f"/api/session/{self.session_id}/prompt", body, timeout=30.0)
                posted = "parts"
            except GatewayError as error:
                self._warn(f"server rejected prompt parts ({error}); "
                           "falling back to text-only prompt")
                self._post(f"/api/session/{self.session_id}/prompt",
                           {"text": text}, timeout=30.0)
                posted = "text-fallback"
        else:
            self._post(f"/api/session/{self.session_id}/prompt", body, timeout=30.0)
            posted = "text"
        self._turn_started = self._last_activity = time.time()
        reply = self._wait_reply(previous, timeout=timeout, emit=emit)
        if emit is not None:
            for record in self._drain_records():
                emit(record)
        result = self._finish_turn(reply, "opencode")
        result["attachments"] = posted if attachments else None
        return reply

    def slash(self, text, timeout=PROMPT_TIMEOUT_S, emit=None):
        """Run "/name rest" as an OpenCode command; return the reply."""
        self.ensure_session()
        self._begin_turn()
        stripped = text.strip().lstrip("/")
        name, _, rest = stripped.partition(" ")
        if not name:
            raise GatewayError("missing command name")
        previous = {message.get("id") for message in self._assistant_messages(limit=5)}
        self._post(f"/api/session/{self.session_id}/command",
                   {"name": name, "text": rest.strip()}, timeout=30.0)
        self._turn_started = self._last_activity = time.time()
        reply = self._wait_reply(previous, timeout=timeout, emit=emit)
        if emit is not None:
            for record in self._drain_records():
                emit(record)
        self._finish_turn(reply, "command")
        return reply

    # -- catalog ----------------------------------------------------------------
    # Catalog reads go through ensure_up so a late-starting server is still
    # consulted; NAVI_OPENCODE_AUTOSPAWN=0 keeps them a fast, clear failure.

    def commands(self):
        self.ensure_up()
        return self._get("/api/command") or []

    def models(self):
        self.ensure_up()
        models = self._get("/api/model") or []
        out = [m for m in models if isinstance(m, dict) and m.get("type") == "chat"]
        out.sort(key=lambda m: (m.get("providerID", ""), m.get("name") or m.get("modelID", "")))
        return [{"providerID": m.get("providerID"), "id": m.get("modelID"),
                 "name": m.get("name") or m.get("modelID")} for m in out]

    def agents(self):
        self.ensure_up()
        agents = self._get("/api/agent") or []
        return [{"id": a.get("id"), "name": a.get("name") or a.get("id"),
                 "description": a.get("description", "")}
                for a in agents
                if isinstance(a, dict) and not a.get("hidden")
                and a.get("mode") in ("primary", "all")]


def _emit_line(payload):
    print(json.dumps(payload), flush=True)


def main(argv):
    import argparse
    parser = argparse.ArgumentParser(prog="opencode_gateway")
    parser.add_argument("mode", choices=["ask", "slash", "models", "agents", "commands",
                                         "new", "interrupt", "permission", "status"])
    parser.add_argument("text", nargs="?", default="")
    parser.add_argument("--model", default=None)
    parser.add_argument("--agent", default=None)
    parser.add_argument("--directory", default=None,
                        help="project directory for the session (or NAVI_PROJECT_DIR)")
    parser.add_argument("--attachment", action="append", default=[],
                        help="attach a file/image to the prompt (repeatable)")
    parser.add_argument("--id", dest="permission_id", default=None,
                        help="permission request id (permission mode)")
    parser.add_argument("--decision", choices=["allow", "deny"], default=None,
                        help="permission decision (permission mode)")
    parser.add_argument("--stream", action="store_true",
                        help="emit streaming records as JSON lines")
    parser.add_argument("--timeout", type=float, default=PROMPT_TIMEOUT_S)
    args = parser.parse_args(argv)
    gateway = Gateway(model=args.model, agent=args.agent, directory=args.directory)

    try:
        if args.mode == "status":
            # Whether an assistant turn could run now, without starting anything:
            # a server answers, or `ask` would find the CLI to spawn one.
            print(json.dumps({"reachable": gateway.reachable(),
                              "spawnable": bool(gateway.autospawn and shutil.which("opencode"))}))
            return 0
        if args.mode == "models":
            print(json.dumps(gateway.models()))
            return 0
        if args.mode == "agents":
            print(json.dumps(gateway.agents()))
            return 0
        if args.mode == "commands":
            print(json.dumps(gateway.commands()))
            return 0
        if args.mode == "new":
            print(json.dumps({"session": gateway.new_session()}))
            return 0
        if args.mode == "interrupt":
            print(json.dumps({"interrupted": gateway.interrupt()}))
            return 0
        if args.mode == "permission":
            if not args.permission_id or not args.decision:
                raise GatewayError("permission needs --id and --decision allow|deny")
            print(json.dumps(gateway.permission_reply(args.permission_id, args.decision)))
            return 0
        if not args.text.strip():
            raise GatewayError("missing text")
        emitted = []

        def emit(record):
            emitted.append(record)
            if args.stream:
                _emit_line(record)

        if args.mode == "slash":
            gateway.start_event_stream()
            reply = gateway.slash(args.text, timeout=args.timeout, emit=emit)
        else:
            gateway.start_event_stream()
            reply = gateway.ask(args.text, timeout=args.timeout, emit=emit,
                                attachments=args.attachment)
    except GatewayError as error:
        _emit_line({"type": "error", "text": str(error)})
        return 1
    result = dict(gateway.last_result or {})
    result.update({"type": "done", "text": reply,
                   "session": gateway.session_id,
                   "deltas": sum(1 for r in emitted if r.get("type") == "delta")})
    result.setdefault("source", "command" if args.mode == "slash" else "opencode")
    _emit_line(result)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
