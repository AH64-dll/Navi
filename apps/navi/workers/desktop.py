#!/usr/bin/python3
"""Portal-authorized PipeWire capture. One JPEG slot; model inference is request driven."""
import base64
import json
import os
import socket
import threading
import time
import zlib

import gi
gi.require_version("Gst", "1.0")
gi.require_version("Atspi", "2.0")
from gi.repository import Atspi, Gio, GLib, Gst
from ipc import RUNTIME, rpc
from windows import Windows

BUS = "org.freedesktop.portal.Desktop"
PATH = "/org/freedesktop/portal/desktop"
REMOTE = "org.freedesktop.portal.RemoteDesktop"
CAST = "org.freedesktop.portal.ScreenCast"


def capture_pipeline(source, height):
    return f"{source} ! videorate drop-only=true ! video/x-raw,framerate=2/1 ! videoscale ! videoconvert ! video/x-raw,format=RGB,width=768,height={height} ! jpegenc quality=60 ! appsink name=frame emit-signals=true max-buffers=1 drop=true sync=false"


class Desktop:
    def __init__(self):
        Gst.init(None)
        self.bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
        self.loop = GLib.MainLoop()
        self.session = None
        self.node = None
        self.pipeline = None
        self.fd = None
        self.revision = 0
        self.targets = {}
        self.lock = threading.Lock()
        self.latest = {"status": "awaiting consent"}
        self.previous = None
        self.previous_controls = None
        self.width, self.height = 1920, 1080
        self.token = 0
        self.properties = {}
        self.windows = Windows(self.bus)

    def request(self, interface, method, args, callback):
        self.token += 1
        token = f"navi_{os.getpid()}_{self.token}"
        sender = self.bus.get_unique_name()[1:].replace(".", "_")
        request_path = f"/org/freedesktop/portal/desktop/request/{sender}/{token}"
        subscription = None
        def response(connection, sender, path, interface_name, signal_name, parameters, unused):
            self.bus.signal_unsubscribe(subscription)
            code, values = parameters.unpack()
            if code:
                self.fail("Desktop consent was declined or cancelled")
            else:
                callback(values)
        subscription = self.bus.signal_subscribe(BUS, "org.freedesktop.portal.Request", "Response", request_path, None, Gio.DBusSignalFlags.NONE, response, None)
        args[-1]["handle_token"] = GLib.Variant("s", token)
        signatures = {"CreateSession":"(a{sv})", "SelectDevices":"(oa{sv})", "SelectSources":"(oa{sv})", "Start":"(osa{sv})"}
        def complete(connection, result, unused):
            try:
                connection.call_finish(result)
            except GLib.Error as error:
                self.bus.signal_unsubscribe(subscription)
                self.fail(str(error))
        self.bus.call(BUS, PATH, interface, method, GLib.Variant(signatures[method], tuple(args)), None, Gio.DBusCallFlags.NONE, -1, None, complete, None)

    def start(self):
        self.request(REMOTE, "CreateSession", [{"session_handle_token": GLib.Variant("s", f"station_{os.getpid()}")}], self.created)
        threading.Thread(target=self.publish, daemon=True).start()
        threading.Thread(target=self.action_server, daemon=True).start()
        self.loop.run()

    def created(self, result):
        self.session = result["session_handle"]
        self.request(REMOTE, "SelectDevices", [self.session, {"types": GLib.Variant("u", 3)}], self.devices)

    def devices(self, result):
        self.request(CAST, "SelectSources", [self.session, {"types": GLib.Variant("u", 3), "multiple": GLib.Variant("b", False), "cursor_mode": GLib.Variant("u", 2)}], self.sources)

    def sources(self, result):
        self.request(REMOTE, "Start", [self.session, "", {}], self.started)

    def started(self, result):
        try:
            streams = result.get("streams", [])
            if not streams:
                raise RuntimeError("Portal returned no screen stream")
            self.node, properties = streams[0]
            self.properties = properties
            self.width, self.height = properties.get("size", (1920, 1080))
            reply, descriptors = self.bus.call_with_unix_fd_list_sync(BUS, PATH, CAST, "OpenPipeWireRemote", GLib.Variant("(oa{sv})", (self.session, {})), GLib.VariantType.new("(h)"), Gio.DBusCallFlags.NONE, -1, None, None)
            self.fd = descriptors.get(reply.unpack()[0])
            height = max(128, round(768 * self.height / self.width))
            self.pipeline = Gst.parse_launch(capture_pipeline(f"pipewiresrc fd={self.fd} path={self.node} do-timestamp=true", height))
            self.pipeline.get_by_name("frame").connect("new-sample", self.frame)
            bus = self.pipeline.get_bus()
            bus.add_signal_watch()
            bus.connect("message::error", lambda bus, message: self.fail(str(message.parse_error()[0])))
            self.pipeline.set_state(Gst.State.PLAYING)
        except (GLib.Error, RuntimeError, ValueError) as error:
            self.fail(str(error))

    def controls(self):
        targets = {}
        controls = []
        count = 0
        def walk(node, depth):
            nonlocal count
            count += 1
            if depth > 7 or count > 450 or len(controls) >= 100:
                return
            try:
                state = node.get_state_set()
                if state.contains(Atspi.StateType.SHOWING) and node.get_name():
                    action = node.get_action_iface()
                    if action and action.get_n_actions():
                        key = f"control-{len(controls)}"
                        controls.append({"id": key, "name": node.get_name()[:100], "role": node.get_role_name()})
                        targets[key] = action
                for index in range(min(node.get_child_count(), 60)):
                    walk(node.get_child_at_index(index), depth + 1)
            except (GLib.Error, AttributeError):
                return
        try:
            walk(Atspi.get_desktop(0), 0)
        except GLib.Error:
            pass
        return controls, targets

    def frame(self, sink):
        sample = sink.emit("pull-sample")
        buffer = sample.get_buffer()
        success, mapped = buffer.map(Gst.MapFlags.READ)
        if not success:
            return Gst.FlowReturn.OK
        jpeg = bytes(mapped.data)
        buffer.unmap(mapped)
        checksum = zlib.crc32(jpeg)
        controls, targets = self.controls()
        with self.lock:
            if controls != self.previous_controls:
                self.revision += 1
                self.previous_controls = controls
            self.previous = checksum
            self.targets = targets
            self.latest = {"status": "connected", "image": "data:image/jpeg;base64," + base64.b64encode(jpeg).decode(), "frame_time": round(time.time() * 1000), "revision": self.revision, "controls": controls, "capture": "PipeWire → CPU RGB/JPEG", "fps_limit": 2, "queued_frames": 1, "size": [self.width, self.height], "stream_properties": self.properties}
        return Gst.FlowReturn.OK

    def publish(self):
        while True:
            with self.lock:
                state = self.latest.copy()
            try:
                rpc("desktop_state", state, timeout=2)
            except (OSError, RuntimeError):
                pass
            time.sleep(.5)

    def action_server(self):
        path = RUNTIME / "desktop.sock"
        path.unlink(missing_ok=True)
        with socket.socket(socket.AF_UNIX) as server:
            server.bind(str(path))
            server.listen(4)
            while True:
                connection, _ = server.accept()
                with connection:
                    connection.settimeout(3)
                    try:
                        request = json.loads(connection.makefile("rb").readline(4096))
                        done = threading.Event()
                        reply = {}
                        deadline = time.monotonic() + 2
                        def execute():
                            try:
                                if time.monotonic() > deadline:
                                    raise RuntimeError("Desktop action expired before dispatch")
                                reply.update(self.execute(request))
                            except (GLib.Error, RuntimeError, ValueError) as error:
                                reply.update({"ok": False, "error": str(error)})
                            done.set()
                            return False
                        GLib.idle_add(execute)
                        done.wait(5)
                        connection.sendall((json.dumps(reply or {"ok": False, "error": "Desktop action outcome unknown"}) + "\n").encode())
                    except (OSError, ValueError):
                        pass

    def execute(self, action):
        if action.get("expires", round(time.time() * 1000) + 1) < time.time() * 1000:
            raise RuntimeError("Desktop action expired before dispatch")
        if "generation" in action and rpc("status", timeout=2)["generation"] != action["generation"]:
            raise RuntimeError("Desktop action was cancelled before dispatch")
        if action["type"]=="windows":return {"ok":True,"windows":self.windows.run()}
        if action["type"]=="window":
            windows=self.windows.run(action["id"])
            if not any(window["id"]==action["id"] and window["active"] for window in windows):raise RuntimeError("Window ended or could not be activated")
            return {"ok":True,"message":"Native window selected"}
        if self.latest["status"] != "connected":
            raise RuntimeError("Desktop is disconnected")
        if action["type"] == "activate":
            with self.lock:
                if action.get("revision") != self.revision or action.get("id") not in self.targets:
                    raise RuntimeError("Desktop target changed; observe again")
                target = self.targets[action["id"]]
            if not target.do_action(0):
                raise RuntimeError("Desktop control rejected the action")
        elif action["type"] == "key":
            key = {"ESC": 1, "ENTER": 28}.get(action.get("key"))
            if key is None:
                raise ValueError("Unregistered desktop key")
            for state in (1, 0):
                self.bus.call_sync(BUS, PATH, REMOTE, "NotifyKeyboardKeycode", GLib.Variant("(oa{sv}iu)", (self.session, {}, key, state)), None, Gio.DBusCallFlags.NONE, -1, None)
        elif action["type"] == "chord":
            keys = {"CTRL":29,"SHIFT":42,"ALT":56,"L":38,"T":20,"W":17,"V":47,"Y":21,"F":33,"ENTER":28,"ESC":1,"TAB":15,"BACKSPACE":14,"F6":64}
            selected = action.get("keys", [])
            if not 1 <= len(selected) <= 4 or any(key not in keys for key in selected):
                raise ValueError("Unregistered desktop shortcut")
            for state, sequence in [(1,selected),(0,list(reversed(selected)))]:
                for key in sequence:
                    self.bus.call_sync(BUS, PATH, REMOTE, "NotifyKeyboardKeycode", GLib.Variant("(oa{sv}iu)", (self.session, {}, keys[key], state)), None, Gio.DBusCallFlags.NONE, -1, None)
        elif action["type"] == "text":
            text = action.get("text", "")
            if not isinstance(text,str) or len(text)>2000 or any(ord(char)<32 for char in text):
                raise ValueError("Invalid literal text")
            for char in text:
                key = ord(char) if ord(char)<256 else 0x01000000 | ord(char)
                for state in (1,0):
                    self.bus.call_sync(BUS, PATH, REMOTE, "NotifyKeyboardKeysym", GLib.Variant("(oa{sv}iu)", (self.session, {}, key, state)), None, Gio.DBusCallFlags.NONE, -1, None)
        elif action["type"] == "pointer":
            x,y = action.get("x"),action.get("y")
            if not isinstance(x,(int,float)) or not isinstance(y,(int,float)) or not (0<=x<self.width and 0<=y<self.height):
                raise ValueError("Pointer outside the connected screen")
            self.bus.call_sync(BUS, PATH, REMOTE, "NotifyPointerMotionAbsolute", GLib.Variant("(oa{sv}udd)", (self.session, {}, self.node, float(x), float(y))), None, Gio.DBusCallFlags.NONE, -1, None)
            for state in (1,0):
                self.bus.call_sync(BUS, PATH, REMOTE, "NotifyPointerButton", GLib.Variant("(oa{sv}iu)", (self.session, {}, 272, state)), None, Gio.DBusCallFlags.NONE, -1, None)
        elif action["type"] == "scroll":
            amount = float(action["dy"])
            if abs(amount) > 3:
                raise ValueError("Scroll bound exceeded")
            self.bus.call_sync(BUS, PATH, REMOTE, "NotifyPointerAxis", GLib.Variant("(oa{sv}dd)", (self.session, {}, 0.0, amount * 100)), None, Gio.DBusCallFlags.NONE, -1, None)
        else:
            raise ValueError("Unknown desktop action")
        return {"ok": True, "message": "Desktop action delivered"}

    def fail(self, error):
        with self.lock:
            self.latest = {"status": "disconnected", "error": error}
        if self.pipeline:
            self.pipeline.set_state(Gst.State.NULL)
        try:
            rpc("desktop_state", self.latest, timeout=2)
        except (OSError, RuntimeError):
            pass
        self.loop.quit()


if __name__ == "__main__":
    try:
        Desktop().start()
    finally:
        (RUNTIME / "desktop.pid").unlink(missing_ok=True)
