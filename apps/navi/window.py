#!/usr/bin/python3
"""GTK/WebKit display host. Rust owns state; tmux owns harness lifetimes."""
import base64
from concurrent.futures import ThreadPoolExecutor
import fcntl
import json
import os
from pathlib import Path
import pty
import signal
import struct
import subprocess
import sys
import termios

if len(sys.argv) > 1 and sys.argv[1] == "--pty-child":
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)
    signal.signal(signal.SIGHUP, signal.SIG_DFL)
    os.execvpe(sys.argv[2], sys.argv[2:], os.environ)

import gi
gi.require_version("Gtk", "3.0")
gi.require_version("Gdk", "3.0")
gi.require_version("WebKit2", "4.1")
gi.require_version("GLibUnix", "2.0")
from gi.repository import Gdk, GLib, GLibUnix, Gtk, WebKit2
from workers.ipc import DATA, rpc

ROOT = Path(__file__).resolve().parent


class Station(Gtk.Window):
    def __init__(self):
        super().__init__(title="Navi · Orchestration Station")
        self.set_default_size(1440, 920)
        self.closed = False
        self.pool = ThreadPoolExecutor(max_workers=4)
        self.master = None
        self.attachment = None
        self.read_watch = None
        self.write_watch = None
        self.pending_input = bytearray()
        self.geometry = (100, 30)
        manager = WebKit2.UserContentManager()
        manager.register_script_message_handler("navi")
        manager.connect("script-message-received::navi", self.on_message)
        data = WebKit2.WebsiteDataManager(base_data_directory=str(DATA / "gui/data"), base_cache_directory=str(DATA / "gui/cache"))
        context = WebKit2.WebContext.new_with_website_data_manager(data)
        self.view = WebKit2.WebView(web_context=context, user_content_manager=manager)
        self.view.set_background_color(Gdk.RGBA(.012, .024, .05, 1))
        self.view.get_settings().set_enable_page_cache(False)
        self.view.get_settings().set_enable_webgl(True)
        self.view.connect("decide-policy", self.policy)
        self.view.connect("web-process-terminated", lambda *args: self.destroy())
        self.view.load_uri((ROOT / "ui/index.html").as_uri())
        self.add(self.view)
        self.connect("destroy", self.close)
        if os.environ.get("NAVI_QA_OUTPUT"):
            GLib.timeout_add_seconds(8, self.capture_qa)

    def evaluate(self, source, callback=None):
        if not self.closed:
            self.view.evaluate_javascript(source, -1, None, None, None, callback, None)
        return False

    def policy(self, view, decision, kind):
        if kind in (WebKit2.PolicyDecisionType.NAVIGATION_ACTION, WebKit2.PolicyDecisionType.NEW_WINDOW_ACTION):
            if decision.get_navigation_action().get_request().get_uri().split("#")[0] != (ROOT / "ui/index.html").as_uri():
                decision.ignore()
                return True
        return False

    def on_message(self, manager, result):
        try:
            message = json.loads(result.get_js_value().to_string())
            kind = message["type"]
            if kind == "rpc":
                ticket, request = message["ticket"], message["request"]
                def run():
                    try:
                        reply = {"ok": True, "result": rpc(request["op"], request.get("params"))}
                    except (OSError, RuntimeError, ValueError) as error:
                        reply = {"ok": False, "error": str(error)}
                    GLib.idle_add(self.evaluate, f"navi.reply({json.dumps(ticket)},{json.dumps(reply)})")
                self.pool.submit(run)
            elif kind == "attach":
                self.attach(message.get("id"))
            elif kind in ("input", "binary") and self.master is not None:
                self.pending_input.extend(message["data"].encode() if kind == "input" else base64.b64decode(message["data"], validate=True))
                self.write_input()
            elif kind == "resize":
                self.geometry = (max(20, min(500, int(message["cols"]))), max(6, min(180, int(message["rows"]))))
                if self.master is not None:
                    fcntl.ioctl(self.master, termios.TIOCSWINSZ, struct.pack("HHHH", self.geometry[1], self.geometry[0], 0, 0))
            elif kind == "fullscreen":
                if self.get_window().get_state() & Gdk.WindowState.FULLSCREEN:
                    self.unfullscreen()
                else:
                    self.fullscreen()
            elif kind == "extension-folder":
                if os.environ.get("NAVI_ISOLATED") == "1":
                    raise ValueError("Opening desktop applications is disabled in isolated mode")
                subprocess.Popen(["xdg-open", str(ROOT / "browser")], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        except (KeyError, ValueError, TypeError, OSError) as error:
            self.evaluate(f"navi.notice({json.dumps(str(error))})")

    def detach(self):
        for watch in (self.read_watch, self.write_watch):
            if watch:
                GLib.source_remove(watch)
        self.read_watch = self.write_watch = None
        self.pending_input.clear()
        if self.attachment and self.attachment.poll() is None:
            self.attachment.terminate()  # only tmux's client; never its server or pane
            try:
                self.attachment.wait(timeout=1)
            except subprocess.TimeoutExpired:
                self.attachment.kill()
        self.attachment = None
        if self.master is not None:
            os.close(self.master)
            self.master = None

    def attach(self, target):
        self.detach()
        if not target:
            return
        status = rpc("status")
        if not any(s["id"] == target and s["running"] for s in status["sessions"]):
            raise ValueError("Unknown or ended terminal session")
        self.master, slave = pty.openpty()
        cols, rows = self.geometry
        fcntl.ioctl(self.master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        environment = os.environ.copy()
        environment.pop("TMUX", None)
        environment.pop("TMUX_PANE", None)
        environment.update(TERM="xterm-256color", COLORTERM="truecolor")
        try:
            self.attachment = subprocess.Popen([sys.executable, str(ROOT / "window.py"), "--pty-child", "tmux", "-S", str(DATA / "terminals.sock"), "attach-session", "-t", target], stdin=slave, stdout=slave, stderr=slave, start_new_session=True, env=environment)
        finally:
            os.close(slave)
        os.set_blocking(self.master, False)
        self.read_watch = GLib.io_add_watch(self.master, GLib.IO_IN | GLib.IO_HUP | GLib.IO_ERR, self.read_output)

    def read_output(self, fd, condition):
        try:
            chunk = os.read(fd, 65536)
        except BlockingIOError:
            return True
        except OSError:
            chunk = b""
        if not chunk:
            self.read_watch = None
            return False
        self.evaluate(f"navi.receive('{base64.b64encode(chunk).decode()}')")
        return True

    def write_input(self, *unused):
        if self.master is None:
            return False
        try:
            while self.pending_input:
                count = os.write(self.master, self.pending_input[:65536])
                del self.pending_input[:count]
        except BlockingIOError:
            if not self.write_watch:
                self.write_watch = GLib.io_add_watch(self.master, GLib.IO_OUT, self.write_input)
            return True
        except OSError:
            self.pending_input.clear()
        self.write_watch = None
        return False

    def capture_qa(self):
        path = Path(os.environ["NAVI_QA_OUTPUT"])
        def save(view, result, unused):
            view.get_snapshot_finish(result).write_to_png(str(path))
            def inspect(view, result, unused):
                value = view.evaluate_javascript_finish(result)
                path.with_suffix(".json").write_text(value.to_string())
                self.destroy()
            self.evaluate("JSON.stringify(navi.inspect())", inspect)
        self.view.get_snapshot(WebKit2.SnapshotRegion.VISIBLE, WebKit2.SnapshotOptions.NONE, None, save, None)
        return False

    def close(self, *unused):
        self.closed = True
        self.detach()
        self.pool.shutdown(wait=False, cancel_futures=True)
        Gtk.main_quit()


if __name__ == "__main__":
    GLib.set_prgname("navi-station")
    GLib.set_application_name("Navi")
    station = Station()
    for number in (signal.SIGTERM, signal.SIGINT):
        GLibUnix.signal_add(GLib.PRIORITY_DEFAULT, number, lambda: station.destroy() or False)
    station.show_all()
    Gtk.main()
