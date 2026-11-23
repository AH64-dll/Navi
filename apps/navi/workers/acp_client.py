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
        self._perm_events = {}
        self._perm_answers = {}
        self._tools = {}
        self.session_id = None
        self._reader_thread = None

    def start(self):
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

    def stop(self):
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()

    def _send(self, msg):
        if not self.proc or not self.proc.stdin:
            raise AcpError("ACP agent not running")
        line = json.dumps(msg) + "\n"
        with self._write_lock:
            self.proc.stdin.write(line)
            self.proc.stdin.flush()

    def request(self, method, params=None, timeout=30.0):
        req_id = self._next_id
        self._next_id += 1
        ev = threading.Event()
        box = {}
        with self._pending_lock:
            self._pending[req_id] = (ev, box)
        self._send({"jsonrpc": "2.0", "id": req_id, "method": method, "params": params or {}})
        if not ev.wait(timeout):
            with self._pending_lock:
                self._pending.pop(req_id, None)
            raise AcpError(f"ACP {method} timed out")
        if "error" in box:
            err = box["error"]
            msg = err.get("message") if isinstance(err, dict) else str(err)
            raise AcpError(f"ACP {method} failed: {msg}")
        return box.get("result")

    def notify(self, method, params=None):
        self._send({"jsonrpc": "2.0", "method": method, "params": params or {}})

    def _reply(self, req_id, result=None, error=None):
        msg = {"jsonrpc": "2.0", "id": req_id}
        if error is not None:
            msg["error"] = error
        else:
            msg["result"] = result if result is not None else {}
        self._send(msg)

    def reply_permission(self, perm_id, decision):
        self._perm_answers[str(perm_id)] = decision
        ev = self._perm_events.get(str(perm_id))
        if ev:
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
                perm_id = str(tool_call.get("toolCallId") or f"acp-perm-{req_id}")
                options = params.get("options") or []
                ev = threading.Event()
                self._perm_events[perm_id] = ev
                self.emit({
                    "type": "permission",
                    "id": perm_id,
                    "session": self.session_id,
                    "tool": tool_call.get("title") or tool_call.get("kind") or "tool",
                    "input": {"rawInput": tool_call.get("rawInput"), "options": options},
                })
                if not ev.wait(180.0):
                    self._reply(req_id, {"outcome": {"outcome": "cancelled"}})
                    return
                decision = self._perm_answers.pop(perm_id, "deny")
                if decision in ("allow", "always"):
                    want_kind = "allow_always" if decision == "always" else "allow_once"
                    opt = next((o for o in options if o.get("kind") == want_kind), None) or (options[0] if options else None)
                    if opt and opt.get("optionId"):
                        self._reply(req_id, {"outcome": {"outcome": "selected", "optionId": opt["optionId"]}})
                        return
                reject = next((o for o in options if "reject" in str(o.get("kind", ""))), None)
                if reject and reject.get("optionId"):
                    self._reply(req_id, {"outcome": {"outcome": "selected", "optionId": reject["optionId"]}})
                else:
                    self._reply(req_id, {"outcome": {"outcome": "cancelled"}})
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
                        for c in update["content"] if isinstance(c, dict)
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
        for raw in self.proc.stdout:
            line = raw.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except ValueError:
                continue
            if "id" in msg and ("result" in msg or "error" in msg):
                with self._pending_lock:
                    pair = self._pending.pop(msg["id"], None)
                if pair:
                    ev, box = pair
                    if "error" in msg:
                        box["error"] = msg["error"]
                    else:
                        box["result"] = msg.get("result")
                    ev.set()
            elif "id" in msg and "method" in msg:
                threading.Thread(
                    target=self._handle_agent_request,
                    args=(msg["id"], msg["method"], msg.get("params") or {}),
                    daemon=True,
                ).start()
            elif "method" in msg:
                self._handle_notification(msg["method"], msg.get("params") or {})

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
        if self.session_id:
            self.notify("session/cancel", {"sessionId": self.session_id})


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

    def _stdin_replies():
        for raw in sys.stdin:
            try:
                msg = json.loads(raw.strip())
            except ValueError:
                continue
            if msg.get("type") == "permission_reply" and msg.get("id"):
                client.reply_permission(msg["id"], msg.get("decision", "deny"))
            elif msg.get("type") == "cancel":
                client.cancel()

    threading.Thread(target=_stdin_replies, daemon=True).start()
    try:
        client.start()
        client.initialize()
        client.new_session()
        res = client.prompt(args.prompt, timeout=args.timeout)
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
