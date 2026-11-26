#!/usr/bin/env python3
"""acp_client.py - Agent Client Protocol (ACP) v1 client for Navi (Phase 6).

Spawns an ACP-speaking harness (Claude Code, Codex, Pi, Gemini CLI, etc.),
speaks JSON-RPC 2.0 over stdin/stdout, and emits the same normalized record
stream as opencode_gateway.py (delta, reasoning, tool_status, tool, permission,
done, error).
"""
import argparse
import json
import os
import subprocess
import sys
import threading
import uuid


class AcpError(Exception):
    pass


def _safe_path(cwd, target):
    base = os.path.realpath(cwd)
    resolved = os.path.realpath(os.path.join(base, target) if not os.path.isabs(target) else target)
    if resolved != base and not resolved.startswith(base + os.sep):
        raise PermissionError(f"path outside session cwd: {target}")
    return resolved


def _emit(payload):
    print(json.dumps(payload), flush=True)


class AcpClient:
    def __init__(self, command, cwd, emit=None):
        self.command = list(command)
        self.cwd = os.path.abspath(cwd or os.getcwd())
        self.emit = emit or _emit
        self.proc = None
        self._next_id = 1
        self._write_lock = threading.Lock()
        self._pending = {}
        self._pending_lock = threading.Lock()
        self._permission_lock = self._pending_lock
        self._permission_counter = 0
        self._perm_events = {}
        self._perm_answers = {}
        self._perm_choices = {}
        self.permission_ack = None
        self._tools = {}
        self.session_id = None
        self._reader_thread = None
        self._stderr_thread = None
        self._closed = False

    def start(self):
        if self.proc is not None:
            raise AcpError("ACP client cannot be restarted")
        self.proc = subprocess.Popen(
            self.command,
            cwd=self.cwd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        self._reader_thread = threading.Thread(target=self._read_loop, daemon=True)
        self._reader_thread.start()
        self._stderr_thread = threading.Thread(target=self._drain_stderr, daemon=True)
        self._stderr_thread.start()

    def _drain_stderr(self):
        # Drain without retaining potentially sensitive harness diagnostics.
        try:
            while self.proc.stderr.read(4096):
                pass
        except (OSError, ValueError):
            pass

    def stop(self):
        self._fail_pending("ACP agent stopped")
        self._cancel_permissions()
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()

    def _send(self, msg):
        if self._closed or not self.proc or not self.proc.stdin:
            raise AcpError("ACP agent not running")
        line = json.dumps(msg) + "\n"
        try:
            with self._write_lock:
                self.proc.stdin.write(line)
                self.proc.stdin.flush()
        except (OSError, ValueError) as exc:
            self._fail_pending("ACP agent write failed")
            self._cancel_permissions()
            raise AcpError("ACP agent write failed") from exc

    def _fail_pending(self, message):
        with self._pending_lock:
            self._closed = True
            pairs = list(self._pending.values())
            self._pending.clear()
        for ev, box in pairs:
            box["error"] = {"message": message}
            ev.set()

    def request(self, method, params=None, timeout=30.0):
        ev, box = threading.Event(), {}
        with self._pending_lock:
            if self._closed:
                raise AcpError("ACP agent connection closed")
            req_id = self._next_id
            self._next_id += 1
            self._pending[req_id] = (ev, box)
        try:
            try:
                self._send({"jsonrpc": "2.0", "id": req_id, "method": method, "params": params or {}})
            except (OSError, ValueError) as exc:
                raise AcpError(f"ACP {method} could not be sent") from exc
            if not ev.wait(timeout):
                raise AcpError(f"ACP {method} timed out")
            if "error" in box:
                err = box["error"]
                msg = err.get("message") if isinstance(err, dict) else str(err)
                raise AcpError(f"ACP {method} failed: {msg}")
            return box.get("result")
        finally:
            with self._pending_lock:
                self._pending.pop(req_id, None)

    def notify(self, method, params=None):
        self._send({"jsonrpc": "2.0", "method": method, "params": params or {}})

    def _reply(self, req_id, result=None, error=None):
        msg = {"jsonrpc": "2.0", "id": req_id}
        if error is not None:
            msg["error"] = error
        else:
            msg["result"] = result if result is not None else {}
        try:
            self._send(msg)
            return True
        except AcpError:
            return False  # Connection failure already settles all pending callers.

    def reply_permission(self, perm_id, decision):
        key = str(perm_id)
        with self._permission_lock:
            ev = self._perm_events.get(key)
            if ev is None or ev.is_set():
                return False
            if decision in ("allow", "always") and decision not in self._perm_choices.get(key, []):
                self._perm_answers[key] = "cancel"
                ev.set()
                return False
            self._perm_answers[key] = decision if decision in ("allow", "always", "deny") else "cancel"
            ev.set()
            return True

    def _cancel_permissions(self):
        with self._permission_lock:
            for key, ev in self._perm_events.items():
                self._perm_answers[key] = "cancel"
                ev.set()

    def _handle_agent_request(self, req_id, method, params):
        try:
            if method == "fs/read_text_file":
                path = _safe_path(self.cwd, params.get("path", ""))
                with open(path, "r", encoding="utf-8", errors="replace") as f:
                    lines = f.readlines()
                offset = max(0, int(params.get("line") or 1) - 1)
                limit = params.get("limit")
                if limit is not None:
                    lines = lines[offset:offset + int(limit)]
                else:
                    lines = lines[offset:]
                self._reply(req_id, {"content": "".join(lines)})
                return

            if method == "fs/write_text_file":
                path = _safe_path(self.cwd, params.get("path", ""))
                os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
                with open(path, "w", encoding="utf-8") as f:
                    f.write(str(params.get("content", "")))
                self._reply(req_id, {})
                return

            if method == "session/request_permission":
                tool_call = params.get("toolCall") or {}
                options = params.get("options") or []
                if not isinstance(options, list):
                    options = []
                options = [o for o in options if isinstance(o, dict) and isinstance(o.get("optionId"), str) and o["optionId"]]
                ev = threading.Event()
                with self._permission_lock:
                    if self._closed:
                        return
                    self._permission_counter += 1
                    perm_id = "acp-perm-" + uuid.uuid4().hex
                    self._perm_events[perm_id] = ev
                    self._perm_choices[perm_id] = [decision for decision, kind in [("allow", "allow_once"), ("always", "allow_always")]
                                                   if any(o.get("kind") == kind for o in options)]
                try:
                    self.emit({
                        "type": "permission", "id": perm_id, "session": self.session_id,
                        "tool": tool_call.get("title") or tool_call.get("kind") or "tool",
                        "input": {"rawInput": tool_call.get("rawInput"), "options": options},
                        "choices": [decision for decision, kind in [("allow", "allow_once"), ("always", "allow_always"), ("deny", "reject_once")]
                                    if decision == "deny" or any(o.get("kind") == kind for o in options)],
                    })
                    answered = ev.wait(180.0)
                    with self._permission_lock:
                        # Close the reply window atomically with timeout selection.
                        self._perm_events.pop(perm_id, None)
                        decision = self._perm_answers.pop(perm_id, "cancel") if answered else "cancel"
                    kind = {"allow": "allow_once", "always": "allow_always", "deny": "reject_once"}.get(decision)
                    opt = next((o for o in options if o.get("kind") == kind), None) if kind else None
                    outcome = {"outcome": "selected", "optionId": opt["optionId"]} if opt else {"outcome": "cancelled"}
                    sent = self._reply(req_id, {"outcome": outcome})
                    if self.permission_ack:
                        self.permission_ack({"type": "permission_result", "id": perm_id,
                                             "replied": answered and sent is True and decision in ("allow", "always", "deny")})
                finally:
                    with self._permission_lock:
                        self._perm_events.pop(perm_id, None)
                        self._perm_answers.pop(perm_id, None)
                        self._perm_choices.pop(perm_id, None)
                return

            self._reply(req_id, error={"code": -32601, "message": f"Unsupported method: {method}"})
        except PermissionError as exc:
            self._reply(req_id, error={"code": -32602, "message": str(exc)})
        except Exception as exc:
            self._reply(req_id, error={"code": -32000, "message": str(exc)})

    def _handle_notification(self, method, params):
        if method != "session/update":
            return
        update = (params or {}).get("update") or {}
        if not isinstance(update, dict):
            return
        kind = update.get("sessionUpdate")
        if kind == "agent_message_chunk":
            content = update.get("content") or {}
            text = content.get("text") if isinstance(content, dict) else None
            if text:
                self.emit({"type": "delta", "text": text})
        elif kind == "agent_thought_chunk":
            content = update.get("content") or {}
            text = content.get("text") if isinstance(content, dict) else None
            if text:
                self.emit({"type": "reasoning", "text": text})
        elif kind in ("tool_call", "tool_call_update"):
            cid = str(update.get("toolCallId") or "")
            if not cid:
                return
            rec = self._tools.setdefault(cid, {"id": cid, "name": None, "input": None})
            if update.get("title"):
                rec["name"] = update["title"]
            if update.get("rawInput") is not None:
                rec["input"] = update["rawInput"]
            status = update.get("status") or "running"
            if status in ("completed", "failed", "error"):
                out = update.get("rawOutput")
                if out is None and isinstance(update.get("content"), list):
                    out = "\n".join(
                        str((c.get("content") or {}).get("text", ""))
                        for c in update["content"] if isinstance(c, dict) and isinstance(c.get("content"), dict)
                    ).strip()
                self.emit({
                    "type": "tool", "id": cid, "name": rec["name"],
                    "status": status, "input": rec["input"],
                    "output": out, "final": True,
                })
            else:
                self.emit({
                    "type": "tool_status", "id": cid, "name": rec["name"],
                    "status": status, "input": rec["input"],
                })

    def _read_loop(self):
        try:
            for raw in self.proc.stdout:
                line = raw.strip()
                if not line:
                    continue
                try:
                    msg = json.loads(line)
                except ValueError:
                    continue
                if not isinstance(msg, dict):
                    continue
                ident = msg.get("id")
                if "id" in msg and (not isinstance(ident, (int, str)) or isinstance(ident, bool)):
                    continue
                if "id" in msg and ("result" in msg or "error" in msg):
                    with self._pending_lock:
                        pair = self._pending.pop(ident, None)
                    if pair:
                        ev, box = pair
                        if "error" in msg:
                            box["error"] = msg["error"]
                        else:
                            box["result"] = msg.get("result")
                        ev.set()
                elif isinstance(msg.get("method"), str):
                    params = msg.get("params") or {}
                    if not isinstance(params, dict):
                        continue
                    if "id" in msg:
                        threading.Thread(target=self._handle_agent_request,
                                         args=(ident, msg["method"], params), daemon=True).start()
                    else:
                        self._handle_notification(msg["method"], params)
        finally:
            self._fail_pending("ACP agent connection closed")
            self._cancel_permissions()

    def initialize(self):
        return self.request("initialize", {
            "protocolVersion": 1,
            "clientCapabilities": {
                "fs": {"readTextFile": True, "writeTextFile": True},
                "terminal": False,
            },
            "clientInfo": {"name": "navi", "version": "0.1.0"},
        })

    def new_session(self):
        res = self.request("session/new", {"cwd": self.cwd, "mcpServers": []})
        self.session_id = (res or {}).get("sessionId")
        return self.session_id

    def prompt(self, text, timeout=180.0):
        if not self.session_id:
            self.new_session()
        res = self.request("session/prompt", {
            "sessionId": self.session_id,
            "prompt": [{"type": "text", "text": text}],
        }, timeout=timeout)
        return res or {}

    def cancel(self):
        self._cancel_permissions()
        if self.session_id and not self._closed:
            try:
                self.notify("session/cancel", {"sessionId": self.session_id})
            except AcpError:
                pass


def main(argv):
    parser = argparse.ArgumentParser(prog="acp_client")
    parser.add_argument("--cwd", default=os.getcwd())
    parser.add_argument("--prompt", required=True)
    parser.add_argument("--timeout", type=float, default=180.0)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    cmd = list(args.command)
    if cmd and cmd[0] == "--":
        cmd = cmd[1:]
    if not cmd:
        _emit({"type": "error", "text": "missing ACP agent command"})
        return 1
    client = AcpClient(cmd, args.cwd)
    chunks = []

    def capture(rec):
        if rec.get("type") == "delta" and rec.get("text"):
            chunks.append(rec["text"])
        _emit(rec)

    client.emit = capture
    client.permission_ack = _emit

    def _stdin_replies():
        for raw in sys.stdin:
            try:
                msg = json.loads(raw.strip())
            except ValueError:
                continue
            if not isinstance(msg, dict):
                continue
            if msg.get("type") == "permission_reply" and msg.get("id"):
                accepted = client.reply_permission(msg["id"], msg.get("decision", "deny"))
                if not accepted:
                    _emit({"type": "permission_result", "id": msg["id"], "replied": False})
            elif msg.get("type") == "cancel":
                client.cancel()
        client.stop()  # Owner pipe closed, never leave a detached ACP agent.

    try:
        client.start()
        threading.Thread(target=_stdin_replies, daemon=True).start()
        client.initialize()
        client.new_session()
        res = client.prompt(args.prompt, timeout=args.timeout)
        # A fast agent can answer the prompt as soon as its permission reply
        # arrives. Publish the owner acknowledgement before the terminal record.
        import time
        until = time.monotonic() + 1.0
        while time.monotonic() < until:
            with client._permission_lock:
                if not client._perm_choices:
                    break
            time.sleep(.001)
        _emit({"type": "done", "text": "".join(chunks).strip(), "session": client.session_id,
               "finish": res.get("stopReason") or "end_turn"})
        return 0
    except AcpError as err:
        _emit({"type": "error", "text": str(err)})
        return 1
    finally:
        client.stop()


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
