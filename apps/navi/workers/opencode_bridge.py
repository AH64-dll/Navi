#!/usr/bin/env python3
"""opencode_bridge.py - long-lived OpenCode worker for Navi (Phase 4).

Holds one global GET /api/event SSE stream for the whole OpenCode server and
serves JSON-line requests + unsolicited server events over a Unix socket at
$NAVI_RUNTIME_DIR/opencode.sock.
"""
import json
import os
import signal
import socket
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from opencode_gateway import Gateway, GatewayError  # noqa: E402
import opencode_contract  # noqa: E402


def _runtime_dir():
    return os.environ.get("NAVI_RUNTIME_DIR") or os.path.expanduser("~/.local/share/navi/run")


def _socket_path():
    return os.path.join(_runtime_dir(), "opencode.sock")


class BridgeServer:
    def __init__(self, socket_path=None, gateway=None):
        self.socket_path = socket_path or _socket_path()
        self.gateway = gateway or Gateway()
        self._subscribers = []
        self._sub_lock = threading.Lock()
        self._stop = threading.Event()
        self._server_sock = None

        self.gateway._on_event = self._on_sse_event
        self.gateway._on_connect = self._on_sse_connect

    def _broadcast(self, payload):
        line = (json.dumps(payload) + "\n").encode()
        dead = []
        with self._sub_lock:
            subs = list(self._subscribers)
        for conn, lock in subs:
            try:
                with lock:
                    conn.sendall(line)
            except OSError:
                dead.append((conn, lock))
        if dead:
            with self._sub_lock:
                for item in dead:
                    if item in self._subscribers:
                        self._subscribers.remove(item)

    def _on_sse_event(self, kind, data):
        self._broadcast({"event": kind, "data": data})

    def _on_sse_connect(self):
        self._broadcast({"event": "bridge.reconnected", "data": {}})

    def _send(self, conn, write_lock, payload):
        line = (json.dumps(payload) + "\n").encode()
        with write_lock:
            conn.sendall(line)

    def _resolve_session(self, args):
        sid = args.get("session")
        if sid:
            return sid
        directory = args.get("directory")
        gw = Gateway(
            base_url=self.gateway.base_url,
            password=self.gateway.password,
            model=args.get("model") or self.gateway.model_ref,
            agent=args.get("agent") or self.gateway.agent,
            autospawn=self.gateway.autospawn,
            session_file=self.gateway.session_file,
            directory=directory,
        )
        gw.ensure_session()
        return gw.session_id

    def _run_op(self, op, args, emit=None):
        gw = self.gateway
        if op == "ping":
            return {"pong": True}
        if op == "status":
            return {
                "reachable": gw.reachable(),
                "spawnable": bool(gw.autospawn),
                "events_alive": gw._events_alive,
            }
        if op == "server_info":
            return gw.server_info()
        if op == "contract_check":
            spec, version = opencode_contract.fetch_live()
            problems = opencode_contract.problems(spec)
            return {"opencode": version, "problems": problems}
        if op == "models":
            return gw.models()
        if op == "agents":
            return gw.agents()
        if op == "commands":
            return gw.commands()
        if op == "catalog":
            return gw.catalog(args.get("name") or "")
        if op == "new":
            sub = Gateway(
                base_url=gw.base_url, password=gw.password,
                model=args.get("model") or gw.model_ref,
                agent=args.get("agent") or gw.agent,
                autospawn=gw.autospawn, session_file=gw.session_file,
                directory=args.get("directory"),
            )
            return {"session": sub.new_session()}
        if op == "interrupt":
            sid = args.get("session")
            if not sid and args.get("directory"):
                sub = Gateway(
                    base_url=gw.base_url, password=gw.password,
                    autospawn=False, session_file=gw.session_file,
                    directory=args.get("directory"),
                )
                return {"interrupted": sub.interrupt()}
            return {"interrupted": gw.interrupt(session_id=sid)}
        if op == "permission":
            sub = Gateway(base_url=gw.base_url, password=gw.password, autospawn=False,
                          session_file=gw.session_file, directory=args.get("directory"))
            return sub.permission_reply(args.get("id"), args.get("decision"), args.get("session"))
        if op == "form":
            sub = Gateway(base_url=gw.base_url, password=gw.password, autospawn=False,
                          session_file=gw.session_file, directory=args.get("directory"))
            return sub.form_reply(args.get("id"), args.get("answer"), args.get("session"))
        if op == "form_cancel":
            sub = Gateway(base_url=gw.base_url, password=gw.password, autospawn=False,
                          session_file=gw.session_file, directory=args.get("directory"))
            return sub.form_cancel(args.get("id"), args.get("session"), args.get("message"))
        if op == "sessions":
            sub = Gateway(base_url=gw.base_url, password=gw.password, autospawn=False,
                          session_file=gw.session_file, directory=args.get("directory"))
            return sub.sessions(
                search=args.get("search"),
                archived=args.get("archived"),
                limit=args.get("limit"),
            )
        if op == "session_get":
            return gw.session_info(args.get("session"))
        if op == "session_messages":
            return gw.session_messages(args.get("session"), limit=args.get("limit"))
        if op == "session_select":
            sub = Gateway(
                base_url=gw.base_url, password=gw.password,
                autospawn=gw.autospawn, session_file=gw.session_file,
                directory=args.get("directory"),
            )
            return {"session": sub.select_session(args.get("session"))}
        if op == "session_rename":
            return gw.session_rename(args.get("session"), args.get("title") or args.get("text") or "")
        if op == "session_fork":
            return gw.session_fork(args.get("session"), before=args.get("before"))
        if op == "session_archive":
            return gw.session_archive(args.get("session"), archived=bool(args.get("archived", True)))
        if op == "session_delete":
            return gw.session_delete(args.get("session"))
        if op == "session_compact":
            return gw.session_compact(args.get("session"))
        if op == "switch_model":
            sub = Gateway(base_url=gw.base_url, password=gw.password, autospawn=gw.autospawn,
                          session_file=gw.session_file, directory=args.get("directory"))
            return sub.switch_model(args.get("model"), session_id=args.get("session"))
        if op == "switch_agent":
            sub = Gateway(base_url=gw.base_url, password=gw.password, autospawn=gw.autospawn,
                          session_file=gw.session_file, directory=args.get("directory"))
            return sub.switch_agent(args.get("agent"), session_id=args.get("session"))
        if op == "mcp_add":
            return gw.mcp_add(args.get("name"), args.get("config") or args.get("payload"))
        if op == "mcp_remove":
            return gw.mcp_remove(args.get("name"))
        if op == "mcp_connect":
            return gw.mcp_connect(args.get("name"))
        if op == "mcp_disconnect":
            return gw.mcp_disconnect(args.get("name"))
        if op == "permission_saved_remove":
            return gw.permission_saved_remove(args.get("id"))
        if op == "credential_activate":
            return gw.credential_activate(args.get("id"))
        if op == "integration_oauth_connect":
            return gw.integration_oauth_connect(args.get("id"), args.get("payload") or {})
        if op == "integration_oauth_status":
            return gw.integration_oauth_status(args.get("id"), args.get("attempt"))
        if op == "integration_oauth_complete":
            return gw.integration_oauth_complete(args.get("id"), args.get("attempt"), args.get("payload") or {})
        if op == "integration_connect_key":
            return gw.integration_connect_key(args.get("id"), args.get("payload") or {})
        if op == "ask":
            sid = self._resolve_session(args)
            return gw.ask_session(
                sid,
                args.get("text") or "",
                timeout=float(args.get("timeout") or 180.0),
                emit=emit,
                attachments=args.get("attachments"),
            )
        if op == "slash":
            sid = self._resolve_session(args)
            return gw.slash_session(
                sid,
                args.get("text") or "",
                timeout=float(args.get("timeout") or 180.0),
                emit=emit,
            )
        raise GatewayError(f"Unknown bridge op: {op}")

    def _handle_request(self, conn, write_lock, req):
        req_id = req.get("id")
        op = req.get("op")
        args = req.get("args") or {}
        if op == "subscribe":
            with self._sub_lock:
                if (conn, write_lock) not in self._subscribers:
                    self._subscribers.append((conn, write_lock))
            if req_id is not None:
                self._send(conn, write_lock, {"id": req_id, "ok": True, "result": {"subscribed": True}})
            return

        def emit(record):
            if req_id is not None and args.get("stream"):
                self._send(conn, write_lock, {"id": req_id, "stream": record})

        try:
            result = self._run_op(op, args, emit=emit if args.get("stream") else None)
            if req_id is not None:
                self._send(conn, write_lock, {"id": req_id, "ok": True, "result": result})
        except GatewayError as error:
            if req_id is not None:
                self._send(conn, write_lock, {"id": req_id, "ok": False, "error": str(error)})
        except Exception as error:
            if req_id is not None:
                self._send(conn, write_lock, {"id": req_id, "ok": False, "error": f"Bridge error: {error}"})

    def _serve_client(self, conn):
        write_lock = threading.Lock()
        try:
            reader = conn.makefile("r", encoding="utf-8", errors="replace")
            for raw in reader:
                line = raw.strip()
                if not line:
                    continue
                try:
                    req = json.loads(line)
                except ValueError:
                    continue
                if req.get("op") == "subscribe":
                    self._handle_request(conn, write_lock, req)
                else:
                    threading.Thread(
                        target=self._handle_request,
                        args=(conn, write_lock, req),
                        daemon=True,
                    ).start()
        except OSError:
            pass
        finally:
            with self._sub_lock:
                self._subscribers = [item for item in self._subscribers if item[0] is not conn]
            try:
                conn.close()
            except OSError:
                pass

    def start(self):
        os.makedirs(os.path.dirname(self.socket_path) or ".", exist_ok=True)
        try:
            os.unlink(self.socket_path)
        except FileNotFoundError:
            pass
        self._server_sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._server_sock.bind(self.socket_path)
        os.chmod(self.socket_path, 0o600)
        self._server_sock.listen(64)
        self._server_sock.settimeout(0.5)
        self.gateway.start_event_stream()

    def serve_forever(self):
        self.start()
        while not self._stop.is_set():
            try:
                conn, _ = self._server_sock.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            threading.Thread(target=self._serve_client, args=(conn,), daemon=True).start()

    def stop(self):
        self._stop.set()
        if self._server_sock is not None:
            try:
                self._server_sock.close()
            except OSError:
                pass
        try:
            os.unlink(self.socket_path)
        except OSError:
            pass


def main():
    server = BridgeServer()

    def _shutdown(_signum, _frame):
        server.stop()
        sys.exit(0)

    signal.signal(signal.SIGTERM, _shutdown)
    signal.signal(signal.SIGINT, _shutdown)
    server.serve_forever()


if __name__ == "__main__":
    main()
