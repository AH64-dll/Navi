"""Test processes with private state and no connection to the user's desktop."""
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("NAVI_TEST_BINARY", "/tmp/navi-cargo-target/debug/navid"))


def free_port():
    with socket.socket() as connection:
        connection.bind(("127.0.0.1", 0))
        return connection.getsockname()[1]


class IsolatedStation:
    def __enter__(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="navi-isolated-")
        self.folder = Path(self.temporary.name)
        self.data = self.folder / "data"
        self.runtime = self.folder / "runtime"
        desktop_runtime = self.folder / "desktop-runtime"
        desktop_runtime.mkdir(mode=0o700)
        self.environment = os.environ.copy()
        for key in ("DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY", "DBUS_SESSION_BUS_ADDRESS", "DBUS_SESSION_BUS_PID", "PULSE_SERVER", "PIPEWIRE_REMOTE", "TMUX", "TMUX_PANE", "NAVI_MANAGED_TERMINALS"):
            self.environment.pop(key, None)
        self.environment.update(
            NAVI_ISOLATED="1", NAVI_DATA_DIR=str(self.data), NAVI_RUNTIME_DIR=str(self.runtime), NAVI_APP_DIR=str(ROOT),
            DBUS_SESSION_BUS_ADDRESS=f"unix:path={self.folder}/no-desktop-bus",
            LIBGL_ALWAYS_SOFTWARE="1", LP_NUM_THREADS="2", OMP_NUM_THREADS="2",
            XDG_CONFIG_HOME=str(self.folder / "config"), XDG_CACHE_HOME=str(self.folder / "cache"),
            XDG_RUNTIME_DIR=str(desktop_runtime),
        )
        self.log = (self.folder / "station.log").open("w")
        self.start()
        return self

    def start(self):
        self.process = subprocess.Popen([str(BINARY), "serve"], env=self.environment, stdout=self.log, stderr=self.log, start_new_session=True)
        for _ in range(100):
            try:
                self.call("ping")
                return
            except OSError:
                if self.process.poll() is not None:
                    break
                time.sleep(.02)
        self.stop()
        raise RuntimeError("Isolated station did not start")

    def call(self, op, params=None, timeout=90):
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(timeout)
            connection.connect(str(self.runtime / "station.sock"))
            connection.sendall((json.dumps({"op": op, "params": params or {}}) + "\n").encode())
            return json.loads(connection.makefile("rb").readline(4 * 1024 * 1024))

    def stop(self):
        # This process group contains only this test's daemon and its children.
        try:
            os.killpg(self.process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            self.process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            os.killpg(self.process.pid, signal.SIGKILL)
            self.process.wait(timeout=3)

    def __exit__(self, *unused):
        self.stop()
        subprocess.run(["tmux", "-S", str(self.data / "terminals.sock"), "kill-server"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=self.environment)
        self.log.close()
        self.temporary.cleanup()
