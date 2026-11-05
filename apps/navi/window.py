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
import time

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
ISOLATED = os.environ.get("NAVI_ISOLATED") == "1"
DEVTOOLS = os.environ.get("NAVI_DEVTOOLS") == "1"
VERBOSE = os.environ.get("NAVI_VERBOSE") == "1"
MIN_SIZE = (720, 560)          # the plan's floor; the CSS stacks below 900
PREFERRED_SIZE = (1440, 920)
GUTTER = 48                    # room for the desktop's own panels around the work area
SAVE_DEBOUNCE_MS = 600
CRASH_WINDOW = 60.0            # repeated WebProcess deaths inside this window escalate
PUSH_ACTIVE_MS = 40            # a turn is open: the answer must arrive as it streams
PUSH_IDLE_MS = 700             # nothing is happening: one event page per poll budget
PUSH_TIMEOUT = 10.0            # a stuck status RPC must not stall the feed
PUSH_LOG = os.environ.get("NAVI_PUSH_LOG") == "1"   # one stderr line per decision


def describe(error):
    return str(error) or error.__class__.__name__


def warn(message):
    print(f"navi-station: {message}", file=sys.stderr, flush=True)


def turn_open(payload):
    """The daemon's own truth: `busy` is published with `turn_state` (BUG-098)."""
    state = payload.get("turn_state") if isinstance(payload, dict) else None
    if not isinstance(state, dict) or state.get("turn") is None:
        return False
    if isinstance(payload.get("busy"), bool):
        return payload["busy"]
    return state.get("done") is not True


class Station(Gtk.Window):
    def __init__(self):
        super().__init__(title="Navi · Orchestration Station")
        self.closed = False
        self.ready = False
        self.save_timeout = None
        self.terminations = 0
        self.last_termination = 0.0
        self.page_loads = 0
        self.load_failed = False
        self.uri = (ROOT / "ui/index.html").as_uri()
        # Geometry has its own RPC: a window move must never rewrite settings.
        self.saved = self.load_window_state()
        self.apply_window_geometry()
        self.pool = ThreadPoolExecutor(max_workers=4)
        # Host-driven status push (BUG-015/098): the page keeps
        # `connect-src 'none'`, so the host is the only clock. Idle polls are
        # cheap and event-only; a live turn is polled at 40 ms and every changed
        # page is delivered through `navi.push`.
        self.push_enabled = os.environ.get("NAVI_PUSH") != "0"
        self.push_timer = None
        self.push_inflight = False
        self.push_seed = True
        self.push_cursor = 0
        self.push_signature = None
        self.push_active = False
        self.push_delivered = 0
        self.push_skipped = 0
        self.push_errors = 0
        self.push_unhandled = 0
        self.push_bytes = 0
        self.push_started = time.monotonic()
        self.master = None
        self.attachment = None
        self.read_watch = None
        self.write_watch = None
        self.pending_input = bytearray()
        self.geometry = (100, 30)
        manager = WebKit2.UserContentManager()
        manager.register_script_message_handler("navi")
        manager.connect("script-message-received::navi", self.on_message)
        self.inject_host_flags(manager)
        data = WebKit2.WebsiteDataManager(base_data_directory=str(DATA / "gui/data"), base_cache_directory=str(DATA / "gui/cache"))
        context = WebKit2.WebContext.new_with_website_data_manager(data)
        self.view = WebKit2.WebView(web_context=context, user_content_manager=manager)
        self.view.set_background_color(Gdk.RGBA(.012, .024, .05, 1))
        settings = self.view.get_settings()
        settings.set_enable_page_cache(False)
        settings.set_enable_webgl(True)
        if DEVTOOLS:
            # Developer extras expose the inspector; console messages reach the
            # terminal so a broken page is diagnosable without a debugger.
            settings.set_enable_developer_extras(True)
            settings.set_enable_write_console_messages_to_stdout(True)
            self.view.connect("key-press-event", self.on_key_press)
        self.error_pane = Gtk.Label()
        self.error_pane.set_line_wrap(True)
        self.error_pane.set_justify(Gtk.Justification.CENTER)
        self.error_pane.set_halign(Gtk.Align.CENTER)
        self.error_pane.set_valign(Gtk.Align.CENTER)
        self.error_pane.set_margin_start(24)
        self.error_pane.set_margin_end(24)
        self.error_pane.set_no_show_all(True)
        self.overlay = Gtk.Overlay()
        self.overlay.add(self.view)
        self.overlay.add_overlay(self.error_pane)
        self.overlay.set_overlay_pass_through(self.error_pane, True)
        self.add(self.overlay)
        self.view.connect("decide-policy", self.policy)
        self.view.connect("load-failed", self.on_load_failed)
        self.view.connect("load-changed", self.on_load_changed)
        self.view.connect("web-process-terminated", self.on_web_process_terminated)
        self.view.load_uri(self.uri)
        self.connect("destroy", self.close)
        self.connect("configure-event", self.on_configure)
        self.connect("window-state-event", self.on_window_state)
        display = self.display()
        if display is not None:
            display.connect("monitor-added", self.on_monitors_changed)
            display.connect("monitor-removed", self.on_monitors_changed)
        if os.environ.get("NAVI_QA_OUTPUT"):
            delay = int(os.environ.get("NAVI_QA_DELAY", "8"))
            GLib.timeout_add_seconds(max(1, delay), self.capture_qa)

    # ---- geometry: the monitor at the pointer, clamped to its work area ----

    @staticmethod
    def display():
        return Gdk.Display.get_default()

    @staticmethod
    def pointer_monitor(display):
        """The monitor under the pointer is where the user expects a window."""
        try:
            pointer = display.get_default_seat().get_pointer()
            _, x, y = pointer.get_position()
            monitor = display.get_monitor_at_point(int(x), int(y))
            if monitor is not None:
                return monitor
        except (AttributeError, TypeError, ValueError):
            pass
        return None

    def target_monitor(self):
        display = self.display()
        if display is None:
            return None
        index = (self.saved or {}).get("monitor")
        if isinstance(index, int) and 0 <= index < display.get_n_monitors():
            return display.get_monitor(index)  # reopen where it was left
        monitor = self.pointer_monitor(display) or display.get_primary_monitor()
        if monitor is None and display.get_n_monitors():
            monitor = display.get_monitor(0)
        return monitor

    def choose_size(self, area, scale):
        """Fit the work area: never wider than it, never below the minimums."""
        saved_width = (self.saved or {}).get("width")
        saved_height = (self.saved or {}).get("height")
        if isinstance(saved_width, int) and isinstance(saved_height, int):
            return (min(area.width, max(min(area.width, MIN_SIZE[0]), saved_width)),
                    min(area.height, max(min(area.height, MIN_SIZE[1]), saved_height)))
        if scale >= 2:
            return area.width, area.height  # HiDPI: the roomy default is the work area
        width = min(area.width, max(min(area.width, MIN_SIZE[0]), min(PREFERRED_SIZE[0], area.width - GUTTER)))
        height = min(area.height, max(min(area.height, MIN_SIZE[1]), min(PREFERRED_SIZE[1], area.height - GUTTER)))
        return width, height

    def apply_window_geometry(self):
        self.set_default_size(*PREFERRED_SIZE)
        self.set_size_request(*MIN_SIZE)
        monitor = self.target_monitor()
        if monitor is None:
            return
        width, height = self.choose_size(monitor.get_workarea(), monitor.get_scale_factor())
        self.set_default_size(width, height)
        self.set_position(Gtk.WindowPosition.CENTER)
        if (self.saved or {}).get("maximized"):
            self.maximize()

    def refine_geometry(self):
        """Re-clamp after mapping: the compositor may pick another monitor."""
        if self.closed:
            return False
        self.ready = True
        monitor = self.target_monitor()
        if monitor is None:
            return False
        area = monitor.get_workarea()
        width, height = self.get_size()
        if width <= 1 or height <= 1:
            width, height = self.get_default_size()
        width, height = min(width, area.width), min(height, area.height)
        if (width, height) != self.get_size():
            self.resize(width, height)
        if not self.is_maximized():
            self.move(area.x + max(0, (area.width - width) // 2),
                      area.y + max(0, (area.height - height) // 2))
        return False

    def on_monitors_changed(self, *unused):
        if self.ready and not self.closed:
            GLib.idle_add(self.refine_geometry)

    # ---- geometry persistence (window_state, never save_settings) ----

    def load_window_state(self):
        try:
            state = rpc("window_state")
        except Exception as error:
            if VERBOSE:
                warn(f"window_state restore skipped: {describe(error)}")
            return {}
        return state if isinstance(state, dict) else {}

    def store_window_state(self, state):
        try:
            rpc("window_state", state)
        except Exception as error:
            if VERBOSE:
                warn(f"window_state save failed: {describe(error)}")

    def window_state(self):
        width, height = self.get_size()
        gdk_window = self.get_window()
        display = self.display()
        maximized = bool(gdk_window is not None and gdk_window.get_state() & Gdk.WindowState.MAXIMIZED)
        monitor = display.get_monitor_at_window(gdk_window) if display is not None and gdk_window is not None else None
        index = None
        if display is not None and monitor is not None:
            for position in range(display.get_n_monitors()):
                if display.get_monitor(position) == monitor:
                    index = position
                    break
        return {"width": int(width), "height": int(height), "maximized": maximized, "monitor": index}

    def schedule_state_save(self):
        if self.save_timeout is not None:
            GLib.source_remove(self.save_timeout)
        self.save_timeout = GLib.timeout_add(SAVE_DEBOUNCE_MS, self.save_window_state)

    def save_window_state(self):
        self.save_timeout = None
        if self.closed or not self.ready:
            return False
        state = self.window_state()
        self.saved = state
        try:
            self.pool.submit(self.store_window_state, state)
        except RuntimeError:  # the pool outlived the window
            pass
        return False

    def on_configure(self, widget, event):
        if self.ready:
            self.schedule_state_save()
        return False

    def on_window_state(self, widget, event):
        if self.ready:
            self.schedule_state_save()
        return False

    # ---- the page bridge ----

    def inject_host_flags(self, manager):
        flags = {"devtools": DEVTOOLS, "isolated": ISOLATED}
        if ISOLATED:
            # Test hooks are namespaced and never exist outside isolated mode.
            if os.environ.get("NAVI_TEST_DROP_REPLY"):
                try:
                    flags["dropReply"] = max(0, int(os.environ["NAVI_TEST_DROP_REPLY"]))
                except ValueError:
                    flags["dropReply"] = 1
            if os.environ.get("NAVI_FORCE_WEBGL_FAIL") == "1":
                flags["webglFail"] = True
        manager.add_script(WebKit2.UserScript.new(
            "window.naviHost=" + json.dumps(flags) + ";",
            WebKit2.UserContentInjectedFrames.TOP_FRAME,
            WebKit2.UserScriptInjectionTime.START,
            None,
            None,
        ))

    def evaluate(self, source, callback=None):
        if not self.closed:
            self.view.evaluate_javascript(source, -1, None, None, None, callback or self.reported, None)
        return False

    def reported(self, view, result, unused):
        """A fire-and-forget evaluation still has a finish callback, so a dead
        page is reported instead of silently dropping the update."""
        try:
            view.evaluate_javascript_finish(result)
        except GLib.Error as error:
            warn(f"evaluate_javascript failed: {error.message}")

    def reply(self, ticket, payload):
        """Serialise in one try, deliver in another: a reply attempt always happens."""
        try:
            encoded = json.dumps(payload)
        except BaseException as error:
            encoded = json.dumps({"ok": False, "error": f"unserialisable reply: {describe(error)}"})
        try:
            self.evaluate(f"navi.reply({json.dumps(ticket)},{encoded})")
        except BaseException as error:
            warn(f"reply for ticket {ticket} was not delivered: {describe(error)}")
        return len(encoded)

    def run_rpc(self, ticket, request):
        started = time.monotonic()
        op, ok, size = "?", False, 0
        error = None
        try:
            op = request["op"]
            result = {"ok": True, "result": rpc(op, request.get("params"))}
            ok = True
        except BaseException as caught:  # a malformed request or a dead daemon still answers
            result = {"ok": False, "error": describe(caught)}
            error = result["error"]
        size = self.reply(ticket, result)
        if VERBOSE:
            # op, latency, outcome, payload size — never params: they carry prompts.
            note = f" error={error!r}" if error else ""
            print(f"rpc {op} {int((time.monotonic() - started) * 1000)}ms ok={ok} bytes={size}{note}",
                  file=sys.stderr, flush=True)

    def on_message(self, manager, result):
        ticket = None
        try:
            message = json.loads(result.get_js_value().to_string())
            kind = message["type"]
            if kind == "rpc":
                ticket, request = message["ticket"], message["request"]
                try:
                    self.pool.submit(self.run_rpc, ticket, request)
                except RuntimeError as error:
                    self.reply(ticket, {"ok": False, "error": describe(error)})
            elif kind == "reload":
                self.reload()
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
                if ISOLATED:
                    raise ValueError("Opening desktop applications is disabled in isolated mode")
                subprocess.Popen(["xdg-open", str(ROOT / "browser")], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        except (KeyError, ValueError, TypeError, OSError) as error:
            # A malformed message must still settle the request that sent it.
            if ticket is not None:
                self.reply(ticket, {"ok": False, "error": describe(error)})
            else:
                self.evaluate(f"navi.notice({json.dumps(describe(error))})")

    def reload(self):
        if not self.closed:
            self.view.reload()

    def on_key_press(self, widget, event):
        control = bool(event.state & Gdk.ModifierType.CONTROL_MASK)
        shift = bool(event.state & Gdk.ModifierType.SHIFT_MASK)
        if event.keyval == Gdk.KEY_F12 or (control and shift and event.keyval in (Gdk.KEY_I, Gdk.KEY_i)):
            self.view.get_inspector().show()
            return True
        return False

    def on_load_failed(self, view, event, uri, error):
        warn(f"load failed for {uri}: {error.message}")
        self.load_failed = True
        self.error_pane.set_text(f"The interface failed to load.\n{uri}\n{error.message}")
        self.error_pane.show()
        return True  # the pane above is the visible failure, not a blank window

    def on_load_changed(self, view, event):
        if event == WebKit2.LoadEvent.STARTED:
            self.load_failed = False
        elif event == WebKit2.LoadEvent.FINISHED:
            self.page_loads += 1
            # A failed load still finishes empty; the pane must outlive it.
            if not self.load_failed:
                self.error_pane.hide()
            # A fresh document owns no snapshot: re-prime the cursor and feed it.
            self.push_seed = True
            self.push_cursor = 0
            self.push_signature = None
            self.push_active = False
            self.schedule_push(PUSH_ACTIVE_MS)

    def on_web_process_terminated(self, view, reason):
        now = time.monotonic()
        self.terminations = self.terminations + 1 if now - self.last_termination < CRASH_WINDOW else 1
        self.last_termination = now
        name = getattr(reason, "value_nick", str(reason))
        warn(f"web process terminated ({name}); attempt {self.terminations} within {CRASH_WINDOW:.0f}s")
        expected = self.page_loads
        if self.terminations == 1:
            view.reload()
        elif self.terminations == 2:
            view.load_uri(self.uri)
        else:
            warn(f"destroying the window after {self.terminations} web process terminations: {name}")
            self.destroy()
            return
        GLib.timeout_add(1500, self.confirm_recovery, expected)

    def confirm_recovery(self, expected):
        """Reloading during the crash handshake can be dropped; force a load then."""
        if not self.closed and self.page_loads <= expected:
            warn("the recovery reload produced no page; loading the UI again")
            self.view.load_uri(self.uri)
        return False

    def policy(self, view, decision, kind):
        if kind in (WebKit2.PolicyDecisionType.NAVIGATION_ACTION, WebKit2.PolicyDecisionType.NEW_WINDOW_ACTION):
            if decision.get_navigation_action().get_request().get_uri().split("#")[0] != self.uri:
                decision.ignore()
                return True
        return False

    # ---- host-driven status push (BUG-015/098) ----

    def schedule_push(self, delay):
        """One push at a time; the cadence follows whether a turn is open."""
        if self.closed or not self.push_enabled:
            return False
        if self.push_timer is not None:
            GLib.source_remove(self.push_timer)
        self.push_timer = GLib.timeout_add(max(10, int(delay)), self.push_poll)
        return False

    def push_poll(self):
        self.push_timer = None
        if self.closed:
            return False
        if self.push_inflight:  # a slow RPC must never stack requests
            self.schedule_push(PUSH_ACTIVE_MS if self.push_active else PUSH_IDLE_MS)
            return False
        self.push_inflight = True
        try:
            self.pool.submit(self.run_push, self.push_cursor)
        except RuntimeError:  # the pool outlived the window
            self.push_inflight = False
        return False

    def run_push(self, cursor):
        """Pool thread: one status page, delivered on the main loop.

        The first page after a load is a full snapshot so the page starts from
        real state; every later page is event-only, which keeps a 40 ms poll
        free of tmux forks and settings blobs (BUG-094/015).
        """
        params = {"gui": True, "since": cursor}
        if not self.push_seed:
            params["sections"] = []
        try:
            payload = rpc("status", params, timeout=PUSH_TIMEOUT)
        except BaseException as error:  # a dead daemon is reported, never raised
            self.push_errors += 1
            if VERBOSE or PUSH_LOG:
                warn(f"push status failed: {describe(error)}")
            GLib.idle_add(self.push_finish, PUSH_IDLE_MS)
            return
        GLib.idle_add(self.deliver_push, payload)

    def push_finish(self, delay):
        self.push_inflight = False
        self.schedule_push(delay)
        return False

    def signature(self, payload):
        """What the page would notice: a new event, a different turn, or a phase
        change. An identical page is not delivered at all (BUG-015)."""
        state = payload.get("turn_state") if isinstance(payload.get("turn_state"), dict) else {}
        text = state.get("text") if isinstance(state.get("text"), str) else ""
        return (payload.get("boot_id"), payload.get("generation"),
                payload.get("config_version"), payload.get("has_more"),
                self.push_cursor, state.get("turn"), state.get("done"),
                len(text), hash(text))

    def deliver_push(self, payload):
        if self.closed or not isinstance(payload, dict):
            self.push_finish(PUSH_IDLE_MS)
            return False
        delay = PUSH_IDLE_MS
        try:
            delay = self.handle_push(payload)
        except BaseException as error:
            # The feed outlives a bad page: one broken push must not freeze it.
            self.push_errors += 1
            warn(f"push page failed: {describe(error)}")
        self.push_finish(delay)
        return False

    def handle_push(self, payload):
        """Decide and deliver one page; returns the delay before the next poll."""
        self.push_active = turn_open(payload)
        events = payload.get("events") or []
        before = self.push_cursor
        for event in events:
            seq = event.get("seq")
            if isinstance(seq, int):
                self.push_cursor = max(self.push_cursor, seq)
        if self.push_seed:
            # The seed is a full snapshot so the page starts from real state;
            # its history stays with the page's own hydration, so the event page
            # is dropped and any backlog is skipped instead of replayed.
            if payload.get("has_more") is True:
                return PUSH_ACTIVE_MS
            self.push_seed = False
            payload = dict(payload, events=[])
            self.push_signature = self.signature(payload)
            self.push_state(payload, "seed")
        else:
            signature = self.signature(payload)
            if signature == self.push_signature:
                self.push_skipped += 1
                if PUSH_LOG:
                    state = payload.get("turn_state") or {}
                    warn(f"push idle seq={self.push_cursor} turn={state.get('turn')}")
            else:
                self.push_signature = signature
                self.push_state(payload, "events" if events else "state")
        # An undrained page is asked again immediately: `has_more` only counts
        # when the cursor actually moved, so a stuck page cannot spin.
        backlog = payload.get("has_more") is True and self.push_cursor > before
        return PUSH_ACTIVE_MS if self.push_active or backlog else PUSH_IDLE_MS

    def push_state(self, payload, note=""):
        """Deliver one page through `navi.push`; a page without the hook keeps
        driving the feed itself and is reported once, never per push."""
        try:
            encoded = json.dumps(payload)
        except BaseException as error:
            self.push_errors += 1
            warn(f"push payload was not serialisable: {describe(error)}")
            return
        self.push_bytes += len(encoded)
        self.push_delivered += 1
        if PUSH_LOG:
            state = payload.get("turn_state") or {}
            warn(f"push {note} seq={self.push_cursor} bytes={len(encoded)} "
                 f"turn={state.get('turn')} done={state.get('done')}")
        self.evaluate(
            "(function(v){if(window.navi&&typeof navi.push==='function')"
            "{navi.push(v);return true;}return false;})(" + encoded + ")",
            self.pushed,
        )

    def pushed(self, view, result, unused):
        try:
            handled = bool(view.evaluate_javascript_finish(result).to_boolean())
        except GLib.Error:  # the page raised inside navi.push
            self.push_errors += 1
            return
        except (AttributeError, ValueError):  # a result shape we cannot read
            return
        if not handled:
            self.push_unhandled += 1
            if self.push_unhandled == 1:
                warn("the page has no navi.push; it keeps driving the status feed itself")

    def push_metrics(self):
        elapsed = max(0.001, time.monotonic() - self.push_started)
        return {"delivered": self.push_delivered, "skipped": self.push_skipped,
                "errors": self.push_errors, "unhandled": self.push_unhandled,
                "bytes": self.push_bytes, "cursor": self.push_cursor,
                "active": self.push_active,
                "rate": round(self.push_delivered / elapsed, 2)}

    # ---- harness terminal ----

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

    # ---- evidence capture ----

    def capture_qa(self):
        path = Path(os.environ["NAVI_QA_OUTPUT"])
        def save(view, result, unused):
            try:
                view.get_snapshot_finish(result).write_to_png(str(path))
            except GLib.Error as error:
                warn(f"snapshot failed: {error.message}")
            def inspect(view, result, unused):
                try:
                    value = json.loads(view.evaluate_javascript_finish(result).to_string())
                    if not isinstance(value, dict):
                        value = {"value": value}
                except (GLib.Error, ValueError) as error:
                    value = {"error": f"inspect failed: {describe(error)}"}
                # Host-side truth the page cannot know: the push feed's health.
                value["host"] = {"push": self.push_metrics()}
                path.with_suffix(".json").write_text(json.dumps(value))
                self.destroy()
            self.evaluate("JSON.stringify(navi.inspect())", inspect)
        self.view.get_snapshot(WebKit2.SnapshotRegion.VISIBLE, WebKit2.SnapshotOptions.NONE, None, save, None)
        return False

    def close(self, *unused):
        self.closed = True
        if self.save_timeout is not None:
            GLib.source_remove(self.save_timeout)
            self.save_timeout = None
        if self.push_timer is not None:
            GLib.source_remove(self.push_timer)
            self.push_timer = None
        if self.push_delivered or self.push_skipped or self.push_errors:
            metrics = self.push_metrics()
            warn(f"status push: {metrics['delivered']} delivered, {metrics['skipped']} unchanged, "
                 f"{metrics['errors']} errors, {metrics['bytes']} bytes, {metrics['rate']}/s")
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
    GLib.timeout_add(150, station.refine_geometry)
    Gtk.main()
