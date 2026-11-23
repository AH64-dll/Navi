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
  permission - reply to a gated-tool approval request
  form       - answer a question form the assistant opened
  form_cancel- withdraw a question still waiting for an answer
  sessions   - this project's sessions, plus which are running now
  session_get / session_messages / session_select / session_rename /
  session_fork / session_archive / session_delete / session_compact
             - the session sidebar: read one, continue one, rename, fork,
               archive, delete, compact
  catalog    - one OpenCode settings list by name (models, agents, providers,
               credentials, integrations, commands, skills, mcp,
               permissions_saved, config, plugins, usage_limits, session_stats)
  switch_model / switch_agent - change what one session runs
  mcp_add / mcp_remove / mcp_connect / mcp_disconnect
             - MCP servers (experimental endpoints)
  permission_saved_remove / credential_activate /
  integration_oauth_connect / integration_oauth_status /
  integration_oauth_complete / integration_connect_key
             - saved approvals and provider accounts
  server_info- the server's version and capabilities

Every HTTP call here is listed in opencode_contract.py, which pins the request
shapes from the server's own OpenAPI spec; the tests reject a body that does
not match it, because the server drops unknown fields without an error.

Streaming events come from GET /api/event (SSE). Verified upstream schema
(live probe): session.text.started/delta/ended, session.reasoning.*,
session.tool.input.started (carries id + name), session.tool.called (id +
input, NO name), session.tool.progress, session.tool.success (content +
metadata.exit), session.step.started (actual agent + model{id,providerID,
variant}), session.step.streamed, session.step.ended (tokens incl. reasoning,
cost, finish), session.usage.updated (cumulative), session.execution.succeeded
(observed terminal event), session.execution.failed, shell.created/exited.
`tool.called.executed` is NOT an approval signal (it was false while a shell
ran). Approvals arrive as permission.asked (a Permission.Request) and questions
as form.created (a Form.Info); both may come from a subagent's child session.

Structured records are emitted as JSON lines on stdout (only with --stream,
except "warning"/"error"/"done" which are always printed):

  {"type":"delta","text":...}                       assistant text chunk
  {"type":"reasoning","text":...}                   model reasoning chunk
  {"type":"tool_status","id":..,"status":..,..}     transient tool progress
  {"type":"tool","id":..,"name":..,"status":..,     exactly ONE final record
     "input":..,"output":..,"exit":..,"final":true}   per tool-call id
  {"type":"permission","id","session","tool",       approval request
     "input":{"resources","metadata"},"message"}
  {"type":"form","id","session","title","fields"}   the assistant asks a question
  {"type":"form_settled","id","outcome"}            answered or cancelled
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

The session-create body carries location.directory, and the created session
reports the directory it got. When that differs, prompts are prefixed with
"[working directory: <path>]" and a warning record is emitted.

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
# A turn fails after this long with nothing happening, not this long in total:
# a long job that keeps working is not a failure.
PROMPT_TIMEOUT_S = 180.0
# The backstop for a tool that runs but never ends.
TURN_LIMIT_S = 1800.0
POLL_INTERVAL_S = 0.25
EVENT_RECONNECT_S = 1.0
SPAWN_TIMEOUT_S = 15.0
WAIT_IDLE_TIMEOUT_S = 15.0
REPLY_SETTLE_S = 5.0
TOOL_OUTPUT_CAP = 8192
ATTACHMENT_LIMIT = 4
ATTACHMENT_MAX_BYTES = 2 * 1024 * 1024
INTERRUPT_RETRY_S = 1.0

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
# Navi's decision words -> OpenCode's Permission.Reply.
DECISIONS = {"allow": "once", "always": "always", "deny": "reject"}
# The settings lists Navi reads, by the name the UI asks for:
# name -> (path, project-scoped, carries secrets). Models, providers, credentials,
# integrations and the config document can hold plaintext API keys, so those are
# redacted on the way out.
CATALOG = {
    "models": ("/api/model", True, True),
    "model_default": ("/api/model/default", True, True),
    "agents": ("/api/agent", True, False),
    "providers": ("/api/provider", True, True),
    "credentials": ("/api/credential", False, True),
    "integrations": ("/api/integration", True, True),
    "commands": ("/api/command", True, False),
    "skills": ("/api/skill", True, False),
    "mcp": ("/api/mcp", True, False),
    "permissions_saved": ("/api/permission/saved", False, False),
    "config": ("/api/config", True, True),
    "plugins": ("/api/plugin", True, False),
    "usage_limits": ("/api/experimental/usage/limits", True, False),
    "session_stats": ("/api/experimental/session/stats", False, False),
}
# A subagent runs in a child session; its approvals and questions belong to this turn.
SESSION_ANCESTRY_LIMIT = 4
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


def _query(**params):
    """Query string for the params asked for; the rest are dropped. `location` is a
    deepObject in the spec, so it travels as location[directory]=<path>."""
    pairs = []
    for key, value in params.items():
        if value is None or value == "":
            continue
        if key == "location":
            pairs.extend((f"location[{inner}]", str(item))
                         for inner, item in value.items() if item is not None)
        else:
            pairs.append((key, str(value)))
    return ("?" + urllib.parse.urlencode(pairs)) if pairs else ""


def _payload_arg(raw):
    """--payload as a JSON object, or None when it was not given."""
    if raw is None:
        return None
    try:
        value = json.loads(raw)
    except ValueError:
        raise GatewayError("--payload must be JSON") from None
    if not isinstance(value, dict):
        raise GatewayError("--payload must be a JSON object")
    return value


def _named(value, what):
    """A path segment that must be present and must not escape its place."""
    name = str(value or "").strip()
    if not name or "/" in name or name in (".", ".."):
        raise GatewayError(f"bad {what}: {value!r}")
    return name


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


def _payload(data, *wrappers):
    """The object an event carries, whether bare or under one wrapper key. The spec
    names events but does not type their data."""
    for key in wrappers:
        if isinstance(data.get(key), dict):
            return data[key]
    return data


def _attachment_file(path):
    """One PromptInput.FileAttachment; {"skip": reason} when the file cannot travel.
    The server runs on this machine: it reads the file:// URI itself and works out
    the type, images included."""
    try:
        size = os.path.getsize(path)
        readable = os.access(path, os.R_OK) and os.path.isfile(path)
    except OSError as error:
        return {"skip": f"{path}: {error}"}
    if not readable:
        return {"skip": f"{path}: not a readable file"}
    if size > ATTACHMENT_MAX_BYTES:
        return {"skip": f"{path}: {size} bytes over {ATTACHMENT_MAX_BYTES}"}
    absolute = os.path.abspath(path)
    return {"uri": "file://" + urllib.parse.quote(absolute), "name": os.path.basename(absolute)}

class SessionState:
    """Per-session turn and SSE state so multiple sessions can run concurrently."""
    def __init__(self, session_id=None):
        self.session_id = session_id
        self.turn_lock = threading.Lock()
        self.records = []
        self.records_lock = threading.Lock()
        self.busy = False
        self.terminal = None
        self.failure = None
        self.last_activity = 0.0
        self.turn_started = 0.0
        self.text_open = False
        self.ended_texts = []
        self.tools = {}
        self.pending = {}
        self.permissions = []
        self.forms = {}
        self.shell_cwds = []
        self.last_usage = None
        self.session_usage = None
        self.actual_model = None
        self.actual_agent = None
        self.last_result = None
        self.warnings = []

    def emit(self, record):
        with self.records_lock:
            self.records.append(record)
        self.last_activity = time.time()

    def drain_records(self):
        with self.records_lock:
            out, self.records = self.records, []
        return out

    def tool_record(self, call_id):
        return self.tools.setdefault(str(call_id), {
            "id": str(call_id), "name": None, "status": "pending",
            "input": None, "output": None, "exit": None, "finished": False})

    def tool_running(self):
        if self.permissions or self.forms:
            return True
        return any(not record["finished"] and record["name"] != "question"
                   for record in self.tools.values())

    def begin_turn_state(self):
        self.busy = True
        self.terminal = None
        self.failure = None
        self.text_open = False
        self.ended_texts = []
        self.tools = {}
        self.pending = {}
        self.permissions = []
        self.forms = {}
        self.last_usage = None
        self.actual_model = None
        self.actual_agent = None
        self.turn_started = time.time()
        self.last_activity = self.turn_started
        self.drain_records()



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
        self._sessions = {}
        self._sessions_lock = threading.Lock()
        self._events_running = False
        self._events_ready = threading.Event()
        self._events_alive = False
        self._on_event = None
        self._on_connect = None
        self._family = {}              # session id -> parent session id or False
        self._dir_prefix = False       # server ignored `directory`: prefix prompts

    def _session_state(self, session_id=None):
        key = session_id or self.session_id or "_default"
        with self._sessions_lock:
            if key == self.session_id and "_default" in self._sessions and key not in self._sessions:
                state = self._sessions.pop("_default")
                state.session_id = key
                self._sessions[key] = state
            state = self._sessions.get(key)
            if state is None:
                state = SessionState(key)
                self._sessions[key] = state
            return state

    @property
    def _records(self): return self._session_state().records
    @_records.setter
    def _records(self, v): self._session_state().records = v
    @property
    def _records_lock(self): return self._session_state().records_lock
    @property
    def _busy(self): return self._session_state().busy
    @_busy.setter
    def _busy(self, v): self._session_state().busy = v
    @property
    def _terminal(self): return self._session_state().terminal
    @_terminal.setter
    def _terminal(self, v): self._session_state().terminal = v
    @property
    def _failure(self): return self._session_state().failure
    @_failure.setter
    def _failure(self, v): self._session_state().failure = v
    @property
    def _last_activity(self): return self._session_state().last_activity
    @_last_activity.setter
    def _last_activity(self, v): self._session_state().last_activity = v
    @property
    def _turn_started(self): return self._session_state().turn_started
    @_turn_started.setter
    def _turn_started(self, v): self._session_state().turn_started = v
    @property
    def _text_open(self): return self._session_state().text_open
    @_text_open.setter
    def _text_open(self, v): self._session_state().text_open = v
    @property
    def _ended_texts(self): return self._session_state().ended_texts
    @_ended_texts.setter
    def _ended_texts(self, v): self._session_state().ended_texts = v
    @property
    def _tools(self): return self._session_state().tools
    @_tools.setter
    def _tools(self, v): self._session_state().tools = v
    @property
    def _pending(self): return self._session_state().pending
    @_pending.setter
    def _pending(self, v): self._session_state().pending = v
    @property
    def _permissions(self): return self._session_state().permissions
    @_permissions.setter
    def _permissions(self, v): self._session_state().permissions = v
    @property
    def _forms(self): return self._session_state().forms
    @_forms.setter
    def _forms(self, v): self._session_state().forms = v
    @property
    def _shell_cwds(self): return self._session_state().shell_cwds
    @_shell_cwds.setter
    def _shell_cwds(self, v): self._session_state().shell_cwds = v
    @property
    def _last_usage(self): return self._session_state().last_usage
    @_last_usage.setter
    def _last_usage(self, v): self._session_state().last_usage = v
    @property
    def _session_usage(self): return self._session_state().session_usage
    @_session_usage.setter
    def _session_usage(self, v): self._session_state().session_usage = v
    @property
    def _actual_model(self): return self._session_state().actual_model
    @_actual_model.setter
    def _actual_model(self, v): self._session_state().actual_model = v
    @property
    def _actual_agent(self): return self._session_state().actual_agent
    @_actual_agent.setter
    def _actual_agent(self, v): self._session_state().actual_agent = v
    @property
    def last_result(self): return self._session_state().last_result
    @last_result.setter
    def last_result(self, v): self._session_state().last_result = v

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

    def _patch(self, path, body=None, timeout=10.0):
        return self._request("PATCH", path, body=body, timeout=timeout)

    def _put(self, path, body=None, timeout=10.0):
        return self._request("PUT", path, body=body, timeout=timeout)

    def _delete(self, path, timeout=10.0):
        return self._request("DELETE", path, timeout=timeout)

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
            body["location"] = {"directory": self.directory}
        data = self._post("/api/session", body)
        self.session_id = data["id"]
        self._dir_prefix = False
        if self.directory:
            # The session says where it runs. GET /api/location is the server's own
            # directory, not the session's, so it cannot confirm this.
            reported = ((data.get("location") or {}).get("directory")
                        if isinstance(data.get("location"), dict) else None)
            if not reported or os.path.abspath(reported) != os.path.abspath(self.directory):
                self._dir_prefix = True
                self._warn(f"OpenCode put the session in {reported or 'an unknown directory'}, "
                           f"not {self.directory}; prompts will carry a [working directory] prefix")
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

    # -- sessions (Phase 1) -----------------------------------------------------
    # The session list is OpenCode's, not Navi's: the same sessions a terminal
    # `opencode` shows for this project, with the same ids.

    def _session_arg(self, session_id=None):
        """The session a command works on: the one given, else this directory's."""
        chosen = session_id or self.session_id or self._load_session()
        if not chosen:
            raise GatewayError("no OpenCode session to work on")
        return chosen

    def _remember(self, session_id, model=None, agent=None):
        """Record what a session now runs, so the next turn leaves it alone."""
        mapping = self._session_map()
        key = self._session_key()
        entry = mapping.get(key) or {}
        if entry.get("id") != session_id:
            return
        if model is not None:
            entry["model"] = model
        if agent is not None:
            entry["agent"] = agent
        mapping[key] = entry
        self._write_session_map(mapping)

    def select_session(self, session_id):
        """Make `session_id` this directory's session, now and on later runs.
        The session's own model and agent are adopted, so continuing it does not
        silently reconfigure it."""
        chosen = _named(session_id, "session id")
        if not chosen.startswith("ses"):
            raise GatewayError(f"not an OpenCode session id: {session_id!r}")
        self.ensure_up()
        info = self._get(f"/api/session/{urllib.parse.quote(chosen)}", timeout=5.0) or {}
        model = info.get("model") if isinstance(info.get("model"), dict) else None
        if model and model.get("providerID") and model.get("id"):
            self.model_ref = f"{model['providerID']}/{model['id']}"
        if isinstance(info.get("agent"), str) and info["agent"]:
            self.agent = info["agent"]
        self.session_id = chosen
        self._save_session()
        _log(f"selected session {chosen} for {self._session_key()}")
        return chosen

    def sessions(self, search=None, archived=None, limit=None):
        """This project's sessions, and which of them are running right now."""
        self.ensure_up()
        query = _query(limit=limit, order="desc", search=search,
                       archived=None if archived is None else ("true" if archived else "false"),
                       directory=self.directory or None)
        listed = self._get("/api/session" + query)
        active = self._get("/api/session/active")
        return {"sessions": listed if isinstance(listed, list) else [],
                "active": active if isinstance(active, dict) else {}}

    def session_info(self, session_id=None):
        self.ensure_up()
        return self._get(f"/api/session/{urllib.parse.quote(self._session_arg(session_id))}") or {}

    def session_messages(self, session_id=None, limit=50):
        """The conversation in order, so the UI can render a session it did not run."""
        self.ensure_up()
        path = (f"/api/session/{urllib.parse.quote(self._session_arg(session_id))}/message"
                + _query(order="asc", limit=limit))
        messages = self._get(path)
        return messages if isinstance(messages, list) else []

    def session_rename(self, title, session_id=None):
        """session.update carries `title` in 2.2.5, so rename is a plain PATCH."""
        title = str(title or "").strip()
        if not title:
            raise GatewayError("a session title cannot be empty")
        self.ensure_up()
        chosen = self._session_arg(session_id)
        self._patch(f"/api/session/{urllib.parse.quote(chosen)}", {"title": title})
        return {"session": chosen, "title": title}

    def session_fork(self, session_id=None, before=None):
        """Copy a session up to `before` (or all of it) into a new one."""
        self.ensure_up()
        chosen = self._session_arg(session_id)
        body = {"before": before} if before else {}
        return self._post(f"/api/session/{urllib.parse.quote(chosen)}/fork", body) or {}

    def session_archive(self, archived, session_id=None):
        self.ensure_up()
        chosen = self._session_arg(session_id)
        self._patch(f"/api/session/{urllib.parse.quote(chosen)}", {"archived": bool(archived)})
        return {"session": chosen, "archived": bool(archived)}

    def session_delete(self, session_id=None):
        """Delete it and forget it here, so no later turn reuses a dead id."""
        self.ensure_up()
        chosen = self._session_arg(session_id)
        self._delete(f"/api/session/{urllib.parse.quote(chosen)}")
        mapping = self._session_map()
        forgotten = [key for key, entry in mapping.items() if entry.get("id") == chosen]
        for key in forgotten:
            mapping.pop(key)
        if forgotten:
            self._write_session_map(mapping)
        if self.session_id == chosen:
            self.session_id = None
        return {"deleted": chosen, "forgotten": bool(forgotten)}

    def session_compact(self, session_id=None):
        self.ensure_up()
        chosen = self._session_arg(session_id)
        return self._post(f"/api/session/{urllib.parse.quote(chosen)}/compact", {}) or {}

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

    def interrupt(self, session_id=None):
        """POST /interrupt, check the response, retry once after 1 s. Stop never starts
        a server: with none running, there is no turn to stop."""
        sid = session_id or self.session_id or self._load_session()
        if not session_id and not self.session_id and sid:
            self.session_id = sid
        if not sid:
            return False
        path = f"/api/session/{sid}/interrupt"
        try:
            self._post(path)
        except GatewayError as first:
            time.sleep(INTERRUPT_RETRY_S)
            try:
                self._post(path)
            except GatewayError as second:
                _log(f"interrupt failed twice: {first}; {second}")
                return False
        self._session_state(sid).busy = False
        return True

    def _pending_permission_session(self, permission_id):
        """The session a pending approval belongs to: a subagent asks from its own."""
        try:
            pending = self._get("/api/permission/request", timeout=5.0) or []
        except GatewayError:
            return None
        for request in pending if isinstance(pending, list) else []:
            if isinstance(request, dict) and request.get("id") == permission_id:
                return request.get("sessionID")
        return None

    def permission_reply(self, permission_id, decision, session_id=None):
        """POST an approval decision. Returns {"replied":bool,"endpoint"} or {"replied":False,"error"}."""
        decision = str(decision or "").strip().lower()
        if decision not in DECISIONS:
            raise GatewayError("decision must be " + ", ".join(DECISIONS))
        self.ensure_up()
        session_id = (session_id or self._pending_permission_session(permission_id)
                      or self.session_id or self._load_session())
        if not session_id:
            return {"replied": False, "error": "No OpenCode session is waiting for this approval"}
        path = f"/api/session/{session_id}/permission/{urllib.parse.quote(permission_id)}/reply"
        try:
            self._post(path, {"decision": DECISIONS[decision]}, timeout=10.0)
        except GatewayError as error:
            return {"replied": False, "error": str(error)}
        return {"replied": True, "endpoint": path}

    def form_reply(self, form_id, answer, session_id=None):
        """Answer a question form: `answer` maps each field key to its value."""
        if not isinstance(answer, dict):
            raise GatewayError("a form answer is an object of field key -> value")
        self.ensure_up()
        session_id = session_id or self.session_id or self._load_session()
        if not session_id:
            return {"replied": False, "error": "No OpenCode session is waiting for this answer"}
        path = f"/api/session/{session_id}/form/{urllib.parse.quote(form_id)}/reply"
        try:
            self._post(path, {"answer": answer}, timeout=10.0)
        except GatewayError as error:
            return {"replied": False, "error": str(error)}
        return {"replied": True, "endpoint": path}

    def form_cancel(self, form_id, session_id=None, message=None):
        """Withdraw a question still waiting for an answer."""
        self.ensure_up()
        chosen = self._session_arg(session_id)
        form = _named(form_id, "form id")
        self._delete(f"/api/session/{urllib.parse.quote(chosen)}/form/{urllib.parse.quote(form)}"
                     + _query(message=message))
        self._forms.pop(form, None)
        return {"cancelled": form, "session": chosen}

    # -- SSE records ------------------------------------------------------------

    def start_event_stream(self):
        """Background SSE reader turning server events into records."""
        if self._events_running:
            return
        self._events_running = True
        threading.Thread(target=self._read_events, daemon=True).start()

    def _emit(self, record, state=None):
        (state or self._session_state()).emit(record)

    def _emit_to(self, state, record):
        if state is self._session_state():
            self._emit(record)
        else:
            state.emit(record)

    def _tool_record(self, call_id, state=None):
        return (state or self._session_state()).tool_record(call_id)

    def _target_state(self, kind, sid):
        if self.session_id:
            if sid and sid != self.session_id:
                if not (kind.startswith(("permission.", "form.")) and self._descends_from_own(sid)):
                    return None
            return self._session_state(self.session_id)
        if sid:
            with self._sessions_lock:
                if sid in self._sessions:
                    return self._sessions[sid]
            if kind.startswith(("permission.", "form.")):
                parent = self._parent_session_id(sid)
                if parent:
                    with self._sessions_lock:
                        if parent in self._sessions:
                            return self._sessions[parent]
        if self._on_event is None:
            return self._session_state("_default")
        return None

    def _handle_event(self, kind, data):
        if not isinstance(data, dict):
            data = {}
        if self._on_event is not None:
            try:
                self._on_event(kind, data)
            except Exception as error:
                _log(f"on_event error for {kind}: {error}")
        sid = _event_session(data)
        state = self._target_state(kind, sid)
        if state is None:
            return
        if kind.startswith(("session.", "shell.")):
            state.last_activity = time.time()

        if kind == "session.step.started":
            state.busy = True
            state.terminal = None
            agent = data.get("agent") or (data.get("step") or {}).get("agent")
            model = data.get("model") or (data.get("step") or {}).get("model")
            if isinstance(model, dict) and model:
                state.actual_model = {k: model.get(k) for k in
                                      ("id", "providerID", "variant") if model.get(k)}
            if isinstance(agent, str) and agent:
                state.actual_agent = agent
            return
        if kind == "session.step.streamed":
            state.busy = True
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
                state.last_usage = usage
            return
        if kind == "session.usage.updated":
            usage = data.get("usage") if isinstance(data.get("usage"), dict) else data
            state.session_usage = {k: v for k, v in usage.items()
                                   if k not in ("sessionID", "sessionId")}
            return
        if kind.startswith("session.execution."):
            name = kind[len("session.execution."):]
            if name == "started":
                state.busy = True
                return
            state.busy = False
            state.terminal = name
            if kind == "session.execution.failed" or "fail" in name or "error" in name:
                error = data.get("error") or data.get("message") or {}
                state.failure = (
                    error.get("message") if isinstance(error, dict) else str(error)
                ) or "OpenCode failed to run the model"
            return
        if kind == "session.text.started":
            state.text_open = True
            state.busy = True
            return
        if kind == "session.text.delta":
            if data.get("delta"):
                self._emit_to(state, {"type": "delta", "text": data["delta"]})
            return
        if kind == "session.text.ended":
            state.text_open = False
            text = data.get("text") or data.get("delta")
            if isinstance(text, str) and text:
                state.ended_texts.append(text)
            return
        if kind == "session.reasoning.started":
            return
        if kind == "session.reasoning.delta":
            if data.get("delta"):
                self._emit_to(state, {"type": "reasoning", "text": data["delta"]})
            return
        if kind == "session.reasoning.ended":
            return
        if kind == "session.tool.input.started":
            call_id = _call_id(data)
            if call_id is None:
                return
            pending = state.pending.setdefault(call_id, {})
            for key in ("name", "tool", "toolName"):
                if data.get(key):
                    pending["name"] = str(data[key])
                    break
            record = state.tool_record(call_id)
            if pending.get("name") and not record["name"]:
                record["name"] = pending["name"]
            return
        if kind == "session.tool.input.ended":
            call_id = _call_id(data)
            if call_id is None:
                return
            pending = state.pending.setdefault(call_id, {})
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
            record = state.tool_record(call_id)
            pending = state.pending.pop(call_id, {})
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
            self._emit_to(state, {"type": "tool_status", "id": call_id,
                        "name": record["name"], "status": "running",
                        "input": record["input"]})
            return
        if kind == "session.tool.progress":
            call_id = _call_id(data)
            if call_id is None:
                return
            record = state.tool_record(call_id)
            progress = data.get("progress") or data.get("text") or data.get("output")
            self._emit_to(state, {"type": "tool_status", "id": call_id,
                        "name": record["name"], "status": "running",
                        "progress": _truncate(_redact_text(str(progress)))
                        if progress is not None else None})
            return
        if kind in ("session.tool.success", "session.tool.failure",
                    "session.tool.error", "session.tool.failed"):
            call_id = _call_id(data)
            if call_id is None:
                return
            record = state.tool_record(call_id)
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
            self._emit_to(state, {"type": "tool", "id": call_id, "name": record["name"],
                        "status": record["status"], "input": record["input"],
                        "output": record["output"], "exit": record["exit"],
                        "final": True})
            return
        if kind == "shell.created":
            shell = data.get("shell") if isinstance(data.get("shell"), dict) else data
            cwd = (shell.get("cwd") or (shell.get("info") or {}).get("cwd")
                   if isinstance(shell, dict) else None)
            if isinstance(cwd, str) and cwd:
                state.shell_cwds.append(cwd)
                if (self.directory and self._dir_prefix
                        and os.path.abspath(cwd) == os.path.abspath(self.directory)):
                    self._dir_prefix = False
                    _log("shell.created confirms server-side directory")
            return
        if kind == "shell.exited":
            return
        if kind == "permission.asked":
            request = _payload(data, "request", "permission")
            if not request.get("id"):
                return
            state.permissions.append(request)
            self._emit_to(state, {"type": "permission", "id": request["id"],
                        "session": request.get("sessionID") or sid,
                        "tool": request.get("action"),
                        "input": _redact({"resources": request.get("resources") or [],
                                          "metadata": request.get("metadata") or {}}),
                        "message": request.get("message")})
            return
        if kind == "permission.replied":
            request = _payload(data, "request", "permission")
            answered = ((request.get("id") if isinstance(request, dict) else None)
                        or data.get("requestID") or data.get("id"))
            if answered:
                state.permissions = [item for item in state.permissions
                                     if item.get("id") != answered]
            return
        if kind == "form.created":
            form = _payload(data, "form")
            if not form.get("id"):
                return
            state.forms[form["id"]] = form
            self._emit_to(state, {"type": "form", "id": form["id"], "session": form.get("sessionID") or sid,
                        "title": form.get("title") or "", "fields": form.get("fields") or [],
                        "metadata": form.get("metadata") or {}})
            return
        if kind in ("form.replied", "form.cancelled", "form.settled"):
            form = _payload(data, "form")
            form_id = form.get("id") or data.get("formID")
            if form_id:
                state.forms.pop(form_id, None)
                self._emit_to(state, {"type": "form_settled", "id": form_id,
                            "outcome": ("answered" if kind == "form.replied" else
                                        "cancelled" if kind == "form.cancelled" else
                                        str(data.get("outcome") or "settled"))})
            return

    def _parent_session_id(self, session_id):
        current = session_id
        for _ in range(SESSION_ANCESTRY_LIMIT):
            try:
                current = (self._get(f"/api/session/{current}", timeout=3.0) or {}).get("parentID")
            except GatewayError:
                break
            if not current:
                break
            with self._sessions_lock:
                if current in self._sessions:
                    return current
        return None

    def _descends_from_own(self, session_id):
        """True when `session_id` is a subagent session under this gateway's session."""
        if session_id in self._family:
            return self._family[session_id]
        current, mine = session_id, False
        for _ in range(SESSION_ANCESTRY_LIMIT):
            try:
                current = (self._get(f"/api/session/{current}", timeout=3.0) or {}).get("parentID")
            except GatewayError:
                break
            if not current:
                break
            if current == self.session_id:
                mine = True
                break
        self._family[session_id] = mine
        return mine
    def _read_events(self):
        """Stay on the stream for the life of the turn: without it, nothing shows
        that a long tool is still running."""
        while True:
            self._read_event_stream()
            time.sleep(EVENT_RECONNECT_S)

    def _read_event_stream(self):
        parsed = urllib.parse.urlsplit(self.base_url)
        connection = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=90)
        try:
            connection.request("GET", "/api/event", headers={
                "Authorization": f"Basic {self._token}", "Accept": "text/event-stream"})
            response = connection.getresponse()
            self._events_alive = True
            self._events_ready.set()
            if self._on_connect is not None:
                try:
                    self._on_connect()
                except Exception as error:
                    _log(f"on_connect error: {error}")
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

    def _assistant_messages(self, limit=3, session_id=None):
        sid = session_id or self.session_id
        messages = self._get(
            f"/api/session/{sid}/message"
            f"?type=assistant&order=desc&limit={limit}")
        return messages if isinstance(messages, list) else []

    @staticmethod
    def _text(message):
        return "\n".join(
            part.get("text", "") for part in (message or {}).get("content") or []
            if part.get("type") == "text" and part.get("text")
        ).strip()

    def _latest_reply(self, previous_ids, session_id=None):
        """Newest *completed* store message — an incomplete message is never
        a reply (BUG-008)."""
        msgs = (self._assistant_messages(limit=5, session_id=session_id)
                if (session_id and session_id != self.session_id)
                else self._assistant_messages(limit=5))
        for message in msgs:
            if message.get("id") in previous_ids:
                continue
            if not (message.get("time") or {}).get("completed"):
                continue
            if message.get("finish") in ("tool-calls", "tool_calls"):
                continue
            text = self._text(message)
            if text:
                return text
        return None

    def _wait_reply(self, previous_ids, timeout=PROMPT_TIMEOUT_S, emit=None, state=None):
        state = state or self._session_state()
        sid = state.session_id or self.session_id
        started = time.time()
        while time.time() - started < TURN_LIMIT_S:
            silent = time.time() - max(state.last_activity, started)
            if silent >= timeout and not state.tool_running():
                break
            if state.failure:
                raise GatewayError(f"OpenCode: {state.failure}")
            if emit is not None:
                for record in state.drain_records():
                    emit(record)
            if state.terminal == "interrupted":
                raise GatewayError("The turn was interrupted before it finished")
            if not state.busy and not state.text_open and state.ended_texts:
                reply = state.ended_texts[-1].strip()
                if reply:
                    return reply
            if not state.busy or not self._events_alive:
                reply = (self._latest_reply(previous_ids, session_id=sid)
                         if (sid and sid != "_default" and sid != self.session_id)
                         else self._latest_reply(previous_ids))
                if reply:
                    return reply
            if state.busy and self._events_alive and not state.text_open:
                quiet = time.time() - max(state.last_activity, state.turn_started)
                if quiet > REPLY_SETTLE_S:
                    reply = (self._latest_reply(previous_ids, session_id=sid)
                             if (sid and sid != "_default" and sid != self.session_id)
                             else self._latest_reply(previous_ids))
                    if reply:
                        self._warn("SSE terminal event missing; "
                                   "reply read from the message store")
                        return reply
            if state.terminal and not state.text_open and silent > REPLY_SETTLE_S:
                raise GatewayError("The assistant ended its turn without a reply")
            time.sleep(POLL_INTERVAL_S)
        over_limit = time.time() - started >= TURN_LIMIT_S
        stopped = self.interrupt(session_id=sid)
        raise GatewayError("no assistant reply: "
                           + (f"still running after {TURN_LIMIT_S / 60:.0f} minutes" if over_limit
                              else f"nothing happened for {timeout:.0f}s")
                           + (", so the turn was stopped" if stopped else ""))

    def _tool_running(self, state=None):
        return (state or self._session_state()).tool_running()

    def _begin_turn(self, state=None):
        state = state or self._session_state()
        if self._events_running:
            self._events_ready.wait(2.0)
        self.wait_idle()
        state.begin_turn_state()

    def _finish_turn(self, reply, source, state=None):
        state = state or self._session_state()
        usage = state.last_usage or {}
        state.last_result = {
            "text": reply,
            "source": source,
            "session": state.session_id or self.session_id,
            "usage": usage or None,
            "session_usage": state.session_usage,
            "finish": usage.get("finish") or state.terminal,
            "actual_model": state.actual_model,
            "actual_agent": state.actual_agent,
            "permissions": state.permissions or None,
            "warnings": self.warnings or None,
        }
        return state.last_result

    def ask_session(self, session_id, text, timeout=PROMPT_TIMEOUT_S, emit=None, attachments=None):
        """Run one turn on an explicit session under its per-session turn_lock."""
        state = self._session_state(session_id)
        with state.turn_lock:
            if self._events_running:
                self._events_ready.wait(2.0)
            state.begin_turn_state()
            previous = {m.get("id") for m in self._assistant_messages(limit=5, session_id=session_id)}
            body = {"text": text}
            files = []
            for path in (attachments or [])[:ATTACHMENT_LIMIT]:
                att = _attachment_file(path)
                if att.get("skip"):
                    self._warn(f"attachment skipped: {att['skip']}")
                else:
                    files.append(att)
            if files:
                body["files"] = files
                try:
                    self._post(f"/api/session/{session_id}/prompt", body, timeout=30.0)
                    posted = "files"
                except GatewayError as error:
                    self._warn(f"server rejected the attachments ({error}); falling back to text-only prompt")
                    self._post(f"/api/session/{session_id}/prompt", {"text": text}, timeout=30.0)
                    posted = "text-fallback"
            else:
                self._post(f"/api/session/{session_id}/prompt", body, timeout=30.0)
                posted = "text"
            state.turn_started = state.last_activity = time.time()
            reply = self._wait_reply(previous, timeout=timeout, emit=emit, state=state)
            if emit is not None:
                for record in state.drain_records():
                    emit(record)
            result = self._finish_turn(reply, "opencode", state=state)
            result["attachments"] = posted if attachments else None
            return result

    def slash_session(self, session_id, text, timeout=PROMPT_TIMEOUT_S, emit=None):
        """Run one slash command on an explicit session under its per-session turn_lock."""
        state = self._session_state(session_id)
        with state.turn_lock:
            if self._events_running:
                self._events_ready.wait(2.0)
            state.begin_turn_state()
            stripped = text.strip().lstrip("/")
            name, _, rest = stripped.partition(" ")
            if not name:
                raise GatewayError("missing command name")
            previous = {m.get("id") for m in self._assistant_messages(limit=5, session_id=session_id)}
            self._post(f"/api/session/{session_id}/command", {"name": name, "text": rest.strip()}, timeout=30.0)
            state.turn_started = state.last_activity = time.time()
            reply = self._wait_reply(previous, timeout=timeout, emit=emit, state=state)
            if emit is not None:
                for record in state.drain_records():
                    emit(record)
            return self._finish_turn(reply, "command", state=state)

    def ask(self, text, timeout=PROMPT_TIMEOUT_S, emit=None, attachments=None):
        """Send a prompt; return the completed assistant reply."""
        self.ensure_session()
        self._begin_turn()
        if self._dir_prefix and self.directory:
            text = f"[working directory: {self.directory}]\n\n{text}"
        previous = {message.get("id") for message in self._assistant_messages(limit=5)}
        body = {"text": text}
        files = []
        for path in (attachments or [])[:ATTACHMENT_LIMIT]:
            attachment = _attachment_file(path)
            if attachment.get("skip"):
                self._warn(f"attachment skipped: {attachment['skip']}")
            else:
                files.append(attachment)
        if files:
            body["files"] = files
            try:
                self._post(f"/api/session/{self.session_id}/prompt", body, timeout=30.0)
                posted = "files"
            except GatewayError as error:
                self._warn(f"server rejected the attachments ({error}); "
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

    def _location(self):
        """The location deepObject the project-scoped reads take."""
        return {"directory": self.directory} if self.directory else None

    def catalog(self, name):
        """One settings list from OpenCode, by the name the UI uses. The allowlist
        is fixed: an unknown name is an error, not a path."""
        entry = CATALOG.get(str(name or "").strip())
        if entry is None:
            raise GatewayError(f"unknown catalog {name!r}; one of "
                               + ", ".join(sorted(CATALOG)))
        path, scoped, secrets = entry
        self.ensure_up()
        data = self._get(path + (_query(location=self._location()) if scoped else ""))
        return {"catalog": name, "items": _redact(data) if secrets else data}

    # -- settings writes (Phase 2) ----------------------------------------------
    # Experimental writes are gated on their operation being in the contract:
    # `opencode_contract.py check` fails when one moves or disappears.

    def switch_model(self, model_ref, session_id=None):
        self.ensure_up()
        chosen = self._session_arg(session_id)
        model = parse_ref(model_ref)
        self._post(f"/api/session/{urllib.parse.quote(chosen)}/model", {"model": model})
        self.model_ref = model_ref
        self._remember(chosen, model=model_ref)
        return {"session": chosen, "model": model}

    def switch_agent(self, agent, session_id=None):
        self.ensure_up()
        chosen = self._session_arg(session_id)
        agent = _named(agent, "agent id")
        self._post(f"/api/session/{urllib.parse.quote(chosen)}/agent", {"agent": agent})
        self.agent = agent
        self._remember(chosen, agent=agent)
        return {"session": chosen, "agent": agent}

    def mcp_add(self, server, config):
        self.ensure_up()
        name = _named(server, "MCP server name")
        if not isinstance(config, dict):
            raise GatewayError('mcp_add needs a config object, e.g. '
                               '{"type": "local", "command": ["npx", "-y", "pkg"]}')
        self._put(f"/api/experimental/mcp/{urllib.parse.quote(name)}",
                  {"config": config}, timeout=30.0)
        return {"server": name, "added": True}

    def mcp_remove(self, server):
        self.ensure_up()
        name = _named(server, "MCP server name")
        self._delete(f"/api/experimental/mcp/{urllib.parse.quote(name)}")
        return {"server": name, "removed": True}

    def mcp_connect(self, server, connect=True):
        self.ensure_up()
        name = _named(server, "MCP server name")
        if connect:
            self._post(f"/api/experimental/mcp/{urllib.parse.quote(name)}/connect", timeout=30.0)
        else:
            self._post(f"/api/experimental/mcp/{urllib.parse.quote(name)}/disconnect", timeout=30.0)
        return {"server": name, "connected": bool(connect)}

    def permission_saved_remove(self, saved_id):
        self.ensure_up()
        chosen = _named(saved_id, "saved permission id")
        self._delete(f"/api/permission/saved/{urllib.parse.quote(chosen)}")
        return {"removed": chosen}

    def credential_activate(self, credential_id):
        self.ensure_up()
        chosen = _named(credential_id, "credential id")
        self._post(f"/api/credential/{urllib.parse.quote(chosen)}/activate", timeout=30.0)
        return {"credential": chosen, "activated": True}

    def integration_oauth_connect(self, integration_id, body):
        """Start an OAuth attempt; the reply carries the URL to open."""
        self.ensure_up()
        chosen = _named(integration_id, "integration id")
        if not isinstance(body, dict) or not body.get("methodID"):
            raise GatewayError('an OAuth connect needs {"methodID": "<method>"}')
        return self._post(f"/api/integration/{urllib.parse.quote(chosen)}/connect/oauth"
                          + _query(location=self._location()), body, timeout=30.0) or {}

    def integration_oauth_status(self, integration_id, attempt_id):
        self.ensure_up()
        chosen = _named(integration_id, "integration id")
        attempt = _named(attempt_id, "attempt id")
        return self._get(f"/api/integration/{urllib.parse.quote(chosen)}/connect/oauth/"
                         f"{urllib.parse.quote(attempt)}"
                         + _query(location=self._location())) or {}

    def integration_oauth_complete(self, integration_id, attempt_id, body=None):
        self.ensure_up()
        chosen = _named(integration_id, "integration id")
        attempt = _named(attempt_id, "attempt id")
        self._post(f"/api/integration/{urllib.parse.quote(chosen)}/connect/oauth/"
                   f"{urllib.parse.quote(attempt)}/complete"
                   + _query(location=self._location()),
                   body if body is not None else {}, timeout=30.0)
        return {"integration": chosen, "attempt": attempt, "completed": True}

    def integration_connect_key(self, integration_id, body):
        self.ensure_up()
        chosen = _named(integration_id, "integration id")
        if not isinstance(body, dict) or not body.get("key"):
            raise GatewayError('a key connection needs {"key": "<api key>"}')
        self._post(f"/api/integration/{urllib.parse.quote(chosen)}/connect/key"
                   + _query(location=self._location()), body, timeout=30.0)
        return {"integration": chosen, "connected": True}

    def server_info(self):
        """Version and capabilities: what `opencode_contract.py check` runs against."""
        self.ensure_up()
        return self._get("/api/info", timeout=5.0) or {}


def _emit_line(payload):
    print(json.dumps(payload), flush=True)


def main(argv):
    import argparse
    parser = argparse.ArgumentParser(prog="opencode_gateway")
    parser.add_argument("mode", choices=["ask", "slash", "models", "agents", "commands",
                                         "new", "interrupt", "permission", "form", "status",
                                         "sessions", "session_get", "session_messages",
                                         "session_select", "session_rename", "session_fork",
                                         "session_archive", "session_delete", "session_compact",
                                         "catalog", "switch_model", "switch_agent",
                                         "mcp_add", "mcp_remove", "mcp_connect", "mcp_disconnect",
                                         "permission_saved_remove", "credential_activate",
                                         "integration_oauth_connect", "integration_oauth_status",
                                         "integration_oauth_complete", "integration_connect_key",
                                         "server_info", "form_cancel"])
    parser.add_argument("text", nargs="?", default="",
                        help="prompt text, or the catalog name / new session title")
    parser.add_argument("--model", default=None)
    parser.add_argument("--agent", default=None)
    parser.add_argument("--directory", default=None,
                        help="project directory for the session (or NAVI_PROJECT_DIR)")
    parser.add_argument("--attachment", action="append", default=[],
                        help="attach a file/image to the prompt (repeatable)")
    parser.add_argument("--id", dest="permission_id", default=None,
                        help="permission request, form, saved permission or credential id")
    parser.add_argument("--session", default=None,
                        help="session to work on: the target of ask/slash, or the session a "
                             "permission/form belongs to when it came from a subagent")
    parser.add_argument("--decision", choices=sorted(DECISIONS), default=None,
                        help="permission decision (permission mode)")
    parser.add_argument("--answer", default=None,
                        help='form answer as JSON, {"<field key>": value} (form mode)')
    parser.add_argument("--search", default=None, help="filter the session list (sessions mode)")
    parser.add_argument("--archived", choices=["true", "false"], default=None,
                        help="list or set archived sessions (sessions and session_archive modes)")
    parser.add_argument("--limit", type=int, default=None,
                        help="page size for sessions and session_messages")
    parser.add_argument("--before", default=None,
                        help="fork up to this message id (session_fork mode)")
    parser.add_argument("--name", default=None, help="MCP server name (mcp_* modes)")
    parser.add_argument("--attempt", default=None, help="OAuth attempt id (integration_oauth_*)")
    parser.add_argument("--payload", default=None,
                        help="JSON: the MCP server config (mcp_add) or the request body "
                             "(integration_connect_key, integration_oauth_connect/complete)")
    parser.add_argument("--stream", action="store_true",
                        help="emit streaming records as JSON lines")
    parser.add_argument("--timeout", type=float, default=PROMPT_TIMEOUT_S)
    args = parser.parse_args(argv)
    gateway = Gateway(model=args.model, agent=args.agent, directory=args.directory)
    archived = None if args.archived is None else args.archived == "true"

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
                raise GatewayError("permission needs --id and --decision " + "|".join(DECISIONS))
            print(json.dumps(gateway.permission_reply(args.permission_id, args.decision, args.session)))
            return 0
        if args.mode == "form":
            try:
                answer = json.loads(args.answer or "")
            except ValueError:
                raise GatewayError("form needs --answer as a JSON object") from None
            if not args.permission_id:
                raise GatewayError("form needs --id")
            print(json.dumps(gateway.form_reply(args.permission_id, answer, args.session)))
            return 0
        if args.mode == "form_cancel":
            if not args.permission_id:
                raise GatewayError("form_cancel needs --id")
            print(json.dumps(gateway.form_cancel(args.permission_id, args.session)))
            return 0
        if args.mode == "sessions":
            print(json.dumps(gateway.sessions(search=args.search, archived=archived,
                                              limit=args.limit)))
            return 0
        if args.mode == "session_get":
            print(json.dumps(gateway.session_info(args.session)))
            return 0
        if args.mode == "session_messages":
            print(json.dumps(gateway.session_messages(args.session, limit=args.limit or 50)))
            return 0
        if args.mode == "session_select":
            print(json.dumps({"session": gateway.select_session(args.session)}))
            return 0
        if args.mode == "session_rename":
            print(json.dumps(gateway.session_rename(args.text, args.session)))
            return 0
        if args.mode == "session_fork":
            print(json.dumps(gateway.session_fork(args.session, before=args.before)))
            return 0
        if args.mode == "session_archive":
            if archived is None:
                raise GatewayError("session_archive needs --archived true|false")
            print(json.dumps(gateway.session_archive(archived, args.session)))
            return 0
        if args.mode == "session_delete":
            print(json.dumps(gateway.session_delete(args.session)))
            return 0
        if args.mode == "session_compact":
            print(json.dumps(gateway.session_compact(args.session)))
            return 0
        if args.mode == "catalog":
            print(json.dumps(gateway.catalog(args.text)))
            return 0
        if args.mode == "switch_model":
            if not args.model:
                raise GatewayError("switch_model needs --model provider/model")
            print(json.dumps(gateway.switch_model(args.model, args.session)))
            return 0
        if args.mode == "switch_agent":
            if not args.agent:
                raise GatewayError("switch_agent needs --agent <id>")
            print(json.dumps(gateway.switch_agent(args.agent, args.session)))
            return 0
        if args.mode in ("mcp_add", "mcp_remove", "mcp_connect", "mcp_disconnect"):
            if not args.name:
                raise GatewayError(f"{args.mode} needs --name <server>")
            if args.mode == "mcp_add":
                print(json.dumps(gateway.mcp_add(args.name, _payload_arg(args.payload))))
            elif args.mode == "mcp_remove":
                print(json.dumps(gateway.mcp_remove(args.name)))
            else:
                print(json.dumps(gateway.mcp_connect(args.name, args.mode == "mcp_connect")))
            return 0
        if args.mode == "permission_saved_remove":
            if not args.permission_id:
                raise GatewayError("permission_saved_remove needs --id")
            print(json.dumps(gateway.permission_saved_remove(args.permission_id)))
            return 0
        if args.mode == "credential_activate":
            if not args.permission_id:
                raise GatewayError("credential_activate needs --id")
            print(json.dumps(gateway.credential_activate(args.permission_id)))
            return 0
        if args.mode == "integration_oauth_connect":
            if not args.permission_id:
                raise GatewayError("integration_oauth_connect needs --id <integration>")
            print(json.dumps(gateway.integration_oauth_connect(
                args.permission_id, _payload_arg(args.payload) or {})))
            return 0
        if args.mode == "integration_oauth_status":
            if not args.permission_id or not args.attempt:
                raise GatewayError("integration_oauth_status needs --id and --attempt")
            print(json.dumps(gateway.integration_oauth_status(args.permission_id, args.attempt)))
            return 0
        if args.mode == "integration_oauth_complete":
            if not args.permission_id or not args.attempt:
                raise GatewayError("integration_oauth_complete needs --id and --attempt")
            print(json.dumps(gateway.integration_oauth_complete(
                args.permission_id, args.attempt, _payload_arg(args.payload))))
            return 0
        if args.mode == "integration_connect_key":
            if not args.permission_id:
                raise GatewayError("integration_connect_key needs --id <integration>")
            body = _payload_arg(args.payload) or {}
            print(json.dumps(gateway.integration_connect_key(args.permission_id, body)))
            return 0
        if args.mode == "server_info":
            print(json.dumps(gateway.server_info()))
            return 0
        if not args.text.strip():
            raise GatewayError("missing text")
        if args.session:
            # Continue a chosen session instead of this directory's stored one.
            gateway.select_session(args.session)
            if args.model:
                gateway.model_ref = args.model
            if args.agent:
                gateway.agent = args.agent
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
