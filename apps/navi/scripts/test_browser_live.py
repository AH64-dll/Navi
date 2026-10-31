"""Real Firefox/native/DOM checks in a disposable headless profile, never user Zen."""
import argparse
from concurrent.futures import ThreadPoolExecutor
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import subprocess
import threading
import time
import urllib.request

from isolated import IsolatedStation, ROOT, free_port

PAGE = b'''<!doctype html><title>Navi isolated browser validation</title>
<button id="complete" onclick="this.textContent='Completed';window.clicks=(window.clicks||0)+1">Mark complete</button>
<button id="guarded" onfocus="this.textContent='Changed on focus'" onclick="window.traps=(window.traps||0)+1">Focus trap</button>
<div hidden>SECRET_HIDDEN</div><input type="password" value="SECRET_PASSWORD">
<div contenteditable>SECRET_EDITABLE</div><button disabled>Disabled action</button>'''


class Fixture(BaseHTTPRequestHandler):
    delay = 0
    inference_started = threading.Event()

    def log_message(self, *unused):
        pass

    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/html" if self.path.startswith("/page") else "application/json")
        self.end_headers()
        self.wfile.write(PAGE if self.path.startswith("/page") else b'{"status":"ok"}')

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.inference_started.set()
        time.sleep(self.delay)
        result = {"answers": {key: {"noul": .01 if key == "noop" else .99} for key in request["questions"]}}
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        try:
            self.wfile.write(json.dumps(result).encode())
        except BrokenPipeError:
            pass


class WebDriver:
    def __init__(self, port):
        self.base = f"http://127.0.0.1:{port}"
        self.session = ""

    def request(self, method, path, value=None):
        request = urllib.request.Request(self.base + path, data=None if value is None else json.dumps(value).encode(), headers={"Content-Type": "application/json"}, method=method)
        with urllib.request.urlopen(request, timeout=45) as reply:
            return json.load(reply)["value"]

    def call(self, method, path, value=None):
        return self.request(method, "/session/" + self.session + path, value)

    def script(self, source, asynchronous=False):
        return self.call("POST", "/execute/" + ("async" if asynchronous else "sync"), {"script": source, "args": []})


def wait_for(read, accept, seconds=12):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        result = read()
        if accept(result):
            return result
        time.sleep(.1)
    raise AssertionError(f"Condition timed out: {result}")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--geckodriver", default=os.environ.get("NAVI_GECKODRIVER", "geckodriver"))
    parser.add_argument("--real-model", action="store_true", help="Use real d1 CPU for the first DOM click")
    args = parser.parse_args()
    os.nice(10)
    server = ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    with IsolatedStation() as station:
        settings = station.call("status")["result"]["settings"]
        settings["model"]["endpoint"] = f"http://127.0.0.1:{server.server_port}/v1/systemone"
        if args.real_model:
            assets = Path.home() / ".local/share/navi"
            settings["model"].update(endpoint=f"http://127.0.0.1:{free_port()}/v1/systemone", server=str(assets / "runtime/llama-b11540/llama-b11540/llama-server"), weights=str(assets / "models/d1-3B/d1-3B-Q4_K_M.gguf"), projection="", gpu_layers=0, threads=2)
        assert station.call("settings", settings)["ok"]
        profile = station.folder / "firefox-profile"
        profile.mkdir(mode=0o700)
        port = free_port()
        webdriver = WebDriver(port)
        with (station.folder / "geckodriver.log").open("w") as log:
            driver = subprocess.Popen([args.geckodriver, "--host", "127.0.0.1", "--port", str(port), "--allow-system-access"], env=station.environment, stdout=log, stderr=log)
            try:
                for _ in range(50):
                    try:
                        webdriver.request("GET", "/status")
                        break
                    except OSError:
                        time.sleep(.1)
                session = webdriver.request("POST", "/session", {"capabilities": {"alwaysMatch": {"browserName": "firefox", "moz:firefoxOptions": {"binary": "/usr/lib64/firefox/firefox", "args": ["-headless", "-no-remote", "-profile", str(profile)], "prefs": {"browser.shell.checkDefaultBrowser": False, "datareporting.policy.dataSubmissionEnabled": False, "browser.sessionstore.resume_from_crash": False}}}}})
                webdriver.session = session["sessionId"]
                webdriver.call("POST", "/url", {"url": f"http://127.0.0.1:{server.server_port}/page?secret=SECRET_URL#SECRET_FRAGMENT"})
                addon = webdriver.call("POST", "/moz/addon/install", {"path": str(ROOT / "browser"), "temporary": True})
                assert addon == "station@navi.local", addon
                webdriver.call("POST", "/moz/context", {"context": "chrome"})
                # Consent is automated solely inside this disposable profile.
                # The shipped manifest and real user's permissions are unchanged.
                granted = webdriver.script('''const done=arguments[arguments.length-1];
const {ExtensionPermissions}=ChromeUtils.importESModule("resource://gre/modules/ExtensionPermissions.sys.mjs");
const extension=WebExtensionPolicy.getByID("station@navi.local").extension;
ExtensionPermissions.add(extension.id,{origins:["http://127.0.0.1/*"],permissions:[]},extension).then(()=>done(true),error=>done(String(error)));''', asynchronous=True)
                assert granted is True, granted
                webdriver.call("POST", "/moz/context", {"context": "content"})
                status = wait_for(lambda: station.call("status")["result"], lambda value: value["browser_connected"] and bool(value["browser"].get("tabs")))
                tab = next(tab for tab in status["browser"]["tabs"] if tab["title"] == "Navi isolated browser validation")
                attached = station.call("attach_tab", {"id": tab["id"]})
                assert attached["ok"] and attached["result"]["ok"], attached
                browser = wait_for(lambda: station.call("status")["result"]["browser"], lambda value: bool(value.get("elements")))
                assert "SECRET_" not in json.dumps(browser), browser
                assert browser["attached"] == tab["id"], browser
                Fixture.delay = 2
                result = station.call("command", {"text": "Click Mark complete"})
                assert result["ok"], result
                assert webdriver.script("return window.clicks") == 1
                assert webdriver.script("return document.querySelector('#complete').textContent") == "Completed"
                if args.real_model:
                    settings["model"]["endpoint"] = f"http://127.0.0.1:{server.server_port}/v1/systemone"
                    assert station.call("settings", settings)["ok"]
                wait_for(lambda: station.call("status")["result"]["browser"], lambda value: any(item["name"] == "Completed" for item in value.get("elements", [])))
                Fixture.delay = 1
                Fixture.inference_started.clear()
                with ThreadPoolExecutor(max_workers=1) as pool:
                    pending = pool.submit(station.call, "command", {"text": "Click Completed"})
                    assert Fixture.inference_started.wait(timeout=3)
                    webdriver.script("document.querySelector('#complete').textContent='Changed control'")
                    failed = pending.result(timeout=6)
                assert failed["ok"] is False, failed
                assert webdriver.script("return window.clicks") == 1
                Fixture.delay = 0
                focused = station.call("command", {"text": "Click Focus trap"})
                assert not focused["ok"], focused
                assert webdriver.script("return window.traps||0") == 0
                station.stop()
                station.start()
                wait_for(lambda: station.call("status")["result"], lambda value: value["browser_connected"])
                webdriver.call("POST", "/url", {"url": f"http://127.0.0.1:{server.server_port}/other"})
                wait_for(lambda: station.call("status")["result"]["browser"], lambda value: value.get("attached") is None)
                denied = station.call("attach_tab", {"id": tab["id"]})
                assert not denied["ok"] or not denied["result"]["ok"], denied
                report = {"headless_disposable_profile": True, "private_station": True, "test_only_permission_grant": True, "native_connection": True, "explicit_attach": True, "typed_decision_to_real_dom_click": True, "real_d1_to_dom_click": args.real_model, "model": "real d1 CPU for first click; deterministic provider for fault checks" if args.real_model else "deterministic provider", "unchanged_observation_survives_slow_inference": True, "hidden_secret_editable_text_excluded": True, "url_query_and_fragment_redacted": True, "changed_target_rejected_without_retry": True, "focus_handler_cannot_retarget_click": True, "daemon_restart_reconnect": True, "navigation_revokes_page_grant": True}
                (ROOT / "evidence/browser-isolation.json").write_text(json.dumps(report, indent=2) + "\n")
                print(json.dumps(report, indent=2))
            finally:
                if webdriver.session:
                    webdriver.call("DELETE", "")
                driver.terminate()
                driver.wait(timeout=5)
    server.shutdown()
    server.server_close()


if __name__ == "__main__":
    main()
