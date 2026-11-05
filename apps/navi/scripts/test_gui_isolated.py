"""Launch the real Copland GUI on a private X display and private D-Bus."""
import argparse
from http.server import ThreadingHTTPServer
import json
import os
from pathlib import Path
import signal
import subprocess
import threading
import time

from isolated import IsolatedStation, ROOT
from test_browser_live import Fixture


def webkit_processes(root):
    """WebKitWebProcess descendants of root, found by walking /proc."""
    parents, cmdlines = {}, {}
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            stat = (entry / "stat").read_text()
            cmdlines[int(entry.name)] = (entry / "cmdline").read_bytes()
        except OSError:
            continue
        fields = stat.rsplit(")", 1)[-1].split()
        if len(fields) > 1:
            parents[int(entry.name)] = int(fields[1])

    def descends(pid):
        seen = set()
        while pid and pid != 1 and pid not in seen:
            if pid == root:
                return True
            seen.add(pid)
            pid = parents.get(pid, 1)
        return False

    return [pid for pid, line in cmdlines.items() if b"WebKitWebProcess" in line and descends(pid)]


def check_crash_policy():
    """The escalation itself is pure: reload, reload the URI, then destroy."""
    import sys
    sys.path.insert(0, str(ROOT))
    import window
    station = window.Station.__new__(window.Station)
    station.uri = (ROOT / "ui/index.html").as_uri()
    station.terminations, station.last_termination, station.closed = 0, 0.0, False
    station.page_loads = 0
    calls = []

    class View:
        def reload(self):
            calls.append("reload")

        def load_uri(self, uri):
            calls.append("uri")

    station.destroy = lambda: calls.append("destroy")
    station.view = View()
    reason = window.WebKit2.WebProcessTerminationReason.CRASHED
    for _ in range(3):
        window.Station.on_web_process_terminated(station, View(), reason)
    assert calls == ["reload", "uri", "destroy"], calls
    station.terminations, station.last_termination = 1, 0.0
    calls.clear()
    window.Station.on_web_process_terminated(station, View(), reason)
    assert calls == ["reload"], calls
    station.closed = True  # deferred recovery checks must not touch anything now
    return {"escalation": ["reload", "uri", "destroy"], "after_window": "reload"}


def run_gui(argv, environment, timeout=90):
    """Run the private GUI host and return its completed process.

    `xvfb-run -a` can race another test for a display number and leave the host
    waiting forever, so a timeout kills the whole process group and one retry is
    attempted. A group kill also stops the X server and the session bus the run
    started, which a plain kill of `xvfb-run` would leak.
    """
    for attempt in (1, 2):
        process = subprocess.Popen(argv, env=environment, stdout=subprocess.PIPE,
                                   stderr=subprocess.STDOUT, text=True, start_new_session=True)
        try:
            transcript = process.communicate(timeout=timeout)[0]
            return subprocess.CompletedProcess(process.args, process.returncode, transcript, "")
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.communicate()
            if attempt == 2:
                raise RuntimeError(f"The GUI host did not finish within {timeout}s twice")
    raise RuntimeError("unreachable")


LAYOUT_TARGETS = ((1440, 920), (1280, 720), (960, 640))
LAYOUT_STATES = ("boot", "open", "closed", "reopened")


def inside(inner, outer, tolerance=1):
    """`inner` sits inside `outer`, in page pixels; rects are rounded to integers."""
    return (inner["x"] >= outer["x"] - tolerance
            and inner["y"] >= outer["y"] - tolerance
            and inner["x"] + inner["w"] <= outer["x"] + outer["width"] + tolerance
            and inner["y"] + inner["h"] <= outer["y"] + outer["height"] + tolerance)


def contains_rect(inner, outer, tolerance=2):
    """Same containment for a rect measured with `width`/`height` keys."""
    return (inner["x"] >= outer["x"] - tolerance
            and inner["y"] >= outer["y"] - tolerance
            and inner["x"] + inner["width"] <= outer["x"] + outer["width"] + tolerance
            and inner["y"] + inner["height"] <= outer["y"] + outer["height"] + tolerance)


def capture_layout(station, argv, environment, evidence):
    """Measure the shell at each target size with the dock and graph open.

    The window opens at the size stored in `window_state`, so that record is the
    knob that moves the viewport: one process per size, one measurement each.
    """
    runs = []
    for width, height in LAYOUT_TARGETS:
        # window_state holds the state itself, and only these four keys are known.
        stored = station.call("window_state", {"width": width, "height": height, "monitor": 0, "maximized": False})
        assert stored["ok"], stored
        output = Path(evidence) / f"gui-layout-{width}x{height}.png"
        environment["NAVI_GUI_REPORT"] = str(output)
        result = run_gui(argv, environment)
        if result.returncode:
            raise RuntimeError(result.stderr or result.stdout)
        report = json.loads(output.with_suffix(".workflow.json").read_text())
        assert report["ok"], report
        layout = report["layout"]
        for label in ("boot", "closed"):
            assert layout[label]["terminalDisplay"] == "none", layout[label]
        for label in ("open", "reopened"):
            state = layout[label]
            assert state["terminalDisplay"] != "none", state
            assert state["terminal"]["visible"] is True, state["terminal"]
            assert state["overlaps"] == [], state
            assert state["stage"]["height"] >= 180, state["stage"]
            assert state["chat"]["height"] >= 120, state["chat"]
            assert state["graphOpen"] is True, state
            # The panel is rendered, not a placeholder: an unknown project shows
            # an em dash and the setup state, never 0 or 100 (BUG-089).
            graph = state["graph"]
            assert graph["open"] is True, graph
            assert graph["label"] == "—", graph
            assert graph["unknown"] is True, graph
            assert graph["empty"] is True, graph
            assert graph["aspects"] == [], graph
            assert graph["as_of"] and graph["as_of"].startswith("as of "), graph
            assert state["project"]["label"], state["project"]
            assert state["project"]["options"] >= 3, state["project"]
            # Above 1280 px the rail is its own column; below it is a drawer
            # inside the stage cell. Neither may leave its cell (BUG-089).
            panel = state["graphRect"]
            assert panel and panel["visible"] is True, panel
            assert panel["width"] >= 300, panel
            if width >= 1280:
                assert panel["x"] >= state["stage"]["x"] + state["stage"]["width"] - 1, (
                    panel, state["stage"])
            else:
                assert contains_rect(panel, state["stage"]), (panel, state["stage"])
        # One click switched the graph, the default cwd, the assistant scope and
        # the status line, and Ctrl+1 switched them back (BUG-090).
        switcher = report["switcher"]
        assert switcher["label"] == "100%", switcher
        assert switcher["scope"] == switcher["alpha"], switcher
        assert switcher["alpha"] != switcher["first"], switcher
        # The art is measured by the scene, not by the layout: its own projected
        # rect has to sit inside the stage cell at every size, or a panel's edge
        # cuts a straight line through the halo (BUG-020/023).
        for label in LAYOUT_STATES:
            state = layout[label]
            art = state.get("art")
            assert art, (label, state)
            assert state["bootOverlay"]["display"] == "none", (label, state["bootOverlay"])
            assert inside(art, state["stage"]), (label, art, state["stage"])
            # The cell the scene framed for is the region less any drawer over it, so
            # it must still be inside the region (below 1280 the graph is a drawer).
            assert art["cell"], (label, art)
            assert inside(art["cell"], state["stage"]), (label, art["cell"], state["stage"])
        viewport = layout["open"]["viewport"]
        assert abs(viewport["width"] - width) <= 2 and abs(viewport["height"] - height) <= 2, (viewport, width, height)
        # The dock opens to the side only where the grid holds a second column.
        assert layout["open"]["dock"] == ("right" if width >= 1200 else "bottom"), layout["open"]["dock"]
        splitter = report["splitter"]
        assert splitter["collapsed"] == 120, splitter
        assert abs(splitter["restored"] - splitter["opened"]) <= 1, splitter
        inspection = json.loads(output.with_suffix(".json").read_text())
        assert inspection["booted"] is True, inspection
        assert inspection["renderer"] == "webgl", inspection
        assert inspection["layout"]["overlaps"] == [], inspection["layout"]
        # The capture is taken immediately after this inspection, so the same frame
        # is the evidence: the art inside the stage, and no fading boot title.
        assert inspection["art"], inspection
        assert inspection["bootOverlay"]["display"] == "none", inspection["bootOverlay"]
        assert inside(inspection["art"], inspection["layout"]["stage"]), (
            inspection["art"], inspection["layout"]["stage"])
        runs.append({
            "screenshot": f"evidence/gui-layout-{width}x{height}.png",
            "viewport": viewport,
            "dock": layout["open"]["dock"],
            "terminalDisplay": layout["open"]["terminalDisplay"],
            "phase": layout["open"]["phase"],
            "stage": layout["open"]["stage"],
            "art": layout["open"]["art"],
            "art_closed": layout["closed"]["art"],
            "chat": layout["open"]["chat"],
            "terminal": layout["open"]["terminal"],
            "sessions": layout["open"]["sessions"],
            "graph": layout["open"]["graph"],
            "overlaps": layout["open"]["overlaps"],
            "splitter": splitter,
            "switcher": switcher,
            "boot_terminal": layout["boot"]["terminalDisplay"],
            "closed_terminal": layout["closed"]["terminalDisplay"],
        })
    return runs


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--workflow", action="store_true", help="Exercise settings, sessions, commands and Stop in the real GUI")
    parser.add_argument("--reliability", action="store_true", help="Drop one RPC reply and assert the client times out and keeps polling")
    parser.add_argument("--crash", action="store_true", help="Kill the web process once and assert the window reloads with its sessions")
    parser.add_argument("--fallback", action="store_true", help="Force the stage to fail and assert the shell stays usable")
    parser.add_argument("--layout", action="store_true", help="Open the dock and graph, then assert no panel overlaps at 1440x920, 1280x720 and 960x640")
    parser.add_argument("--chat", action="store_true", help="Drive the chat contract with synthetic events: streaming, de-duplication, tools, approval, errors, markdown safety, hydration and voice")
    parser.add_argument("--screen", default="1440x920x24", help="Private display geometry, e.g. 1024x600x24")
    args = parser.parse_args()
    os.nice(10)
    if args.chat:
        # No OpenCode fixture in this mode: make every gateway call fail fast instead
        # of spending the RPC budget trying to spawn a server, and drive the page from
        # its own synthetic events rather than the host's push feed.
        os.environ["NAVI_OPENCODE_AUTOSPAWN"] = "0"
        os.environ["NAVI_PUSH"] = "0"
    screen_width, screen_height = (int(part) for part in args.screen.split("x")[:2])
    host = ROOT / "window.py"
    if args.reliability:
        output = ROOT / "evidence/gui-reliability.png"
    elif args.workflow:
        output = ROOT / "evidence/gui-workflow.png"
    elif args.chat:
        output = ROOT / "evidence/gui-chat.png"
    elif args.layout:
        width, height = LAYOUT_TARGETS[-1]
        output = ROOT / f"evidence/gui-layout-{width}x{height}.png"
    elif args.crash:
        output = ROOT / "evidence/gui-crash.png"
    elif args.fallback:
        output = ROOT / "evidence/gui-fallback.png"
    else:
        output = ROOT / "evidence/isolated-gui.png"
    with IsolatedStation() as station:
        environment = {**station.environment, "GDK_BACKEND": "x11"}
        server = None
        try:
            if args.workflow:
                server = ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
                server.daemon_threads = True
                threading.Thread(target=server.serve_forever, daemon=True).start()
                Fixture.delay = 2
                station.environment["NAVI_OPENCODE_URL"] = f"http://127.0.0.1:{server.server_port}"
                station.environment["NAVI_OPENCODE_AUTOSPAWN"] = "0"
                station.stop()
                station.start()
                settings = station.call("status")["result"]["settings"]
                settings["model"]["endpoint"] = f"http://127.0.0.1:{server.server_port}/v1/systemone"
                settings["profiles"].append({"id": "fixture-shell", "name": "Fixture shell", "executable": "/bin/bash", "args": []})
                settings["projects"] = [{"name": "Fixture", "path": str(station.folder)}]
                assert station.call("settings", settings)["ok"]
                environment["NAVI_GUI_REPORT"] = str(output)
                # Developer extras must be observable from the page, not just set.
                environment["NAVI_DEVTOOLS"] = "1"
                environment["NAVI_VERBOSE"] = "1"
                host = ROOT / "scripts/gui_workflow_host.py"
            elif args.chat:
                # Same private host as the workflow, but no OpenCode fixture: every
                # event is synthetic and the only daemon work is message save/hydrate.
                environment["NAVI_GUI_REPORT"] = str(output)
                host = ROOT / "scripts/gui_workflow_host.py"
            elif args.layout:
                # Same private host as the workflow, but no OpenCode fixture: the
                # layout checks only need a running harness session and the graph.
                host = ROOT / "scripts/gui_workflow_host.py"
            else:
                environment["NAVI_QA_OUTPUT"] = str(output)
            if args.reliability:
                # One reply is dropped on purpose; the client must expire it and
                # keep polling instead of freezing the status feed. The sweep
                # fires at 15 s; the assertions below then describe the feed.
                environment.update(NAVI_TEST_DROP_REPLY="1", NAVI_VERBOSE="1", NAVI_QA_DELAY="28")
            policy = None
            if args.fallback:
                environment["NAVI_FORCE_WEBGL_FAIL"] = "1"
            if args.crash:
                # The escalation policy is pure logic; check it without a display.
                policy = check_crash_policy()
                environment.update(NAVI_QA_OUTPUT=str(output), NAVI_QA_DELAY="16")
            # The layout mode marks itself with the session name: the production
            # host injects no QA flags beyond `isolated`, and both modes share one
            # injected script.
            name = ("Layout probe" if args.layout
                    else "Chat probe" if args.chat
                    else "Isolated validation")
            if args.layout:
                # BUG-089/090: three projects, one of them scored, so the switcher
                # has to change the graph, the default cwd and the status line.
                scored = None
                for label in ("alpha", "beta"):
                    folder = station.folder / label
                    folder.mkdir(exist_ok=True)
                    added = station.call("project_add", {
                        "name": label.title(), "path": str(folder.resolve())})
                    assert added["ok"], added
                    if label == "alpha":
                        scored = added["result"]["project"]
                assert scored is not None
                aspect = station.call("aspect_add", {
                    "project_id": scored["id"], "title": "Assistant server"})
                assert aspect["ok"], aspect
                task = station.call("task_add", {
                    "project_id": scored["id"], "aspect_id": aspect["result"]["id"],
                    "title": "Wire the server"})
                assert task["ok"], task
                done = station.call("task_set", {"id": task["result"]["id"], "state": "done"})
                assert done["ok"], done
                graph = station.call("graph_get", {"project_id": scored["id"]})["result"]
                assert graph["percent"] == 100.0, graph
                assert graph["verified"] == 100.0, graph
                assert graph["signals"], graph
            created = station.call("create", {"profile": "terminal", "name": name, "cwd": str(station.folder), "allow_unregistered": True})
            assert created["ok"], created
            stored = None
            if args.chat:
                # BUG-047 paging needs history the first page does not cover: three
                # saved rows in a project of their own, so the run's own transcript
                # stays empty at boot and the two pages are exactly these rows.
                base = int(time.time() * 1000) - 60_000
                for index in range(3):
                    saved = station.call("message_save", {
                        "id": f"paging-{index}", "role": "user",
                        "parts": [{"type": "text", "text": f"Archived message {index}"}],
                        "time": base + index, "project_id": "paging-probe"})
                    assert saved["ok"], saved
                stored = station.call("messages", {"limit": 10, "project_id": "paging-probe"})["result"]
                assert [row["id"] for row in stored["messages"]] == ["paging-0", "paging-1", "paging-2"], stored
                assert stored["has_more"] is False, stored
                assert all(isinstance(row["cursor"], int) for row in stored["messages"]), stored
                first = station.call("messages", {"limit": 2, "project_id": "paging-probe"})["result"]
                assert [row["id"] for row in first["messages"]] == ["paging-1", "paging-2"], first
                assert first["has_more"] is True, first
            # The display must hold the roomiest target; the window itself opens at
            # the size written into window_state for each run.
            display = f"{max(screen_width, 1440)}x{max(screen_height, 920)}x24" if args.layout else args.screen
            argv = ["xvfb-run", "-a", f"--server-args=-screen 0 {display} -nolisten tcp", "dbus-run-session", "/usr/bin/python3", str(host)]
            layout = None
            if args.layout:
                layout = capture_layout(station, argv, environment, ROOT / "evidence")
                result = None
            elif args.crash:
                # Kill WebKit once: the window must reload and keep its sessions.
                process = subprocess.Popen(argv, env=environment, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
                victims, deadline = [], time.monotonic() + 25
                while not victims and time.monotonic() < deadline:
                    victims = webkit_processes(process.pid)
                    time.sleep(.2)
                assert victims, "The GUI never started a web process"
                for pid in victims:
                    os.kill(pid, signal.SIGKILL)
                transcript = process.communicate(timeout=45)[0]
                result = subprocess.CompletedProcess(process.args, process.returncode, transcript, "")
            else:
                result = run_gui(argv, environment, 70 if args.reliability else 45)
        finally:
            Fixture.delay = 0
            if server is not None:
                server.shutdown()
                server.server_close()
        if result is not None and (result.stdout or result.stderr):
            # The host transcript is the only witness to a wedged request; keep it
            # beside the evidence so a failure is diagnosable after the run.
            Path("/tmp/navi-gui-transcript.log").write_text(result.stdout + result.stderr)
        if result is not None and result.returncode:
            raise RuntimeError(result.stderr or result.stdout)
        workflow = None
        if args.workflow or args.chat:
            workflow = json.loads(output.with_suffix(".workflow.json").read_text())
            assert workflow["ok"], workflow
        inspection = json.loads(output.with_suffix(".json").read_text())
        if args.workflow:
            # The workflow records the WebGL renderer first, then probes a stage
            # failure: the shell must stay usable and report the fallback honestly.
            assert workflow["renderer"] == "webgl", workflow
            assert inspection["renderer"] == "fallback", inspection
            assert inspection["degraded"] is True, inspection
            assert inspection["booted"] is True, inspection
            assert inspection["chatOpacity"] == "1", inspection
        elif args.fallback:
            # A stage that cannot start must leave a usable, honestly reported shell.
            assert inspection["renderer"] == "fallback", inspection
            assert inspection["degraded"] is True, inspection
            assert inspection["booted"] is True, inspection
            assert inspection["chatOpacity"] == "1", inspection
            assert inspection["title"] == "Navi", inspection
        else:
            assert inspection["renderer"] == "webgl", inspection
            assert inspection["title"] == "Navi", inspection
            assert inspection["booted"] is True, inspection
        assert inspection["viewport"]["width"] <= screen_width, inspection["viewport"]
        assert inspection["viewport"]["height"] <= screen_height, inspection["viewport"]
        if args.reliability:
            assert inspection["dropped"] == 1, inspection
            assert inspection["timeouts"] >= 1, inspection
            # A status poll can legitimately be in flight when the window closes:
            # its reply is queued on the WebKit bridge behind the snapshot, so
            # "pending == 0" races the 700 ms feed. The defect this guards is a
            # promise left past its delivery budget (a parked request), plus any
            # pile-up of concurrent polls.
            assert inspection["pending"] <= 1, inspection
            assert inspection["pendingStale"] == 0, inspection
            assert set(inspection["pendingOps"]) <= {"status"}, inspection
            assert inspection["polls"] >= 2, inspection
            assert any("timeout" in entry["message"] for entry in inspection["errors"]), inspection["errors"]
            assert inspection["sessions"], inspection
            # xvfb-run merges the host's stderr into stdout, so check both.
            transcript = result.stdout + result.stderr
            assert "rpc status" in transcript, transcript[-2000:]
            assert inspection["devtools"] is False, inspection
        elif not args.workflow:
            assert inspection["devtools"] is False, inspection
        else:
            assert inspection["devtools"] is True, inspection
            # Developer extras forward page errors to the terminal as well.
            console = result.stdout + result.stderr
            assert "__missingFn" in console, console[-2000:]
        if args.chat:
            # The page report is the evidence: one row per turn, one final per tool
            # id, one surface per failure, a growing stream, and a rehydrated log.
            chat = workflow["chat"]
            assert chat["status"] == "Ready", chat
            assert chat["streaming"]["second"] > chat["streaming"]["first"], chat["streaming"]
            assert chat["tool"]["status"] == "completed", chat["tool"]
            # BUG-047: three saved rows arrived as two pages joined on the cursor.
            page = chat["paging"]
            assert page["page"] == 2 and page["loaded"] == 3, page
            assert page["hasMore"] is False, page
            expected = [row["cursor"] for row in stored["messages"]]
            assert page["cursors"] == expected, (page, stored)
            assert page["before"] == expected[1], (page, stored)
            # BUG-056: Enter claimed the hold, a lost claim sent nothing, Escape cancelled.
            assert chat["voice"] == {"confirming": True, "claimed": True, "dispatched": True,
                                     "already": True, "cancel": True,
                                     "claims": 2, "cancels": 1, "commands": 1}, chat["voice"]
            # BUG-006: a refused decision is one failure surface, never a decision.
            assert chat["errors"]["rows"] == 2, chat["errors"]
            assert chat["permission"]["decided"] is False, chat["permission"]
            assert chat["permission"]["text"] == "Invalid permission id", chat["permission"]
            assert chat["permission"]["replies"] == ["deny", "allow"], chat["permission"]
            assert chat["rehydrated"] >= 3, chat
            # BUG-005: the prompt carries the file; an oversized file is refused.
            assert chat["attachments"]["sent"] == 1, chat["attachments"]
            assert chat["attachments"]["refused"] is True, chat["attachments"]
            assert chat["attachments"]["names"] == ["notes.txt"], chat["attachments"]
            assert chat["empty"] is True and chat["empty_final"] is False, chat
            assert inspection["renderer"] == "webgl", inspection
            assert inspection["booted"] is True, inspection
        if args.crash:
            transcript = result.stdout + result.stderr
            assert "web process terminated" in transcript, transcript[-3000:]
            assert "attempt 1" in transcript, transcript[-3000:]
            assert "destroying the window" not in transcript, transcript[-3000:]
            assert inspection["booted"] is True, inspection
            assert len(inspection["sessions"]) == 1, inspection["sessions"]
        assert all(session["running"] for session in station.call("status")["result"]["sessions"])
        report = {"private_display": True, "private_bus": True, "software_webgl": True, "gui_close_preserves_session": True, "inspection": inspection}
        if args.layout:
            report["layout"] = layout
        if args.workflow:
            report["workflow"] = workflow
            transcript = result.stdout + result.stderr
            report["console_errors"] = [line for line in transcript.splitlines() if "[navi]" in line][:3]
            report["rpc_log"] = [line for line in transcript.splitlines() if line.startswith("rpc ")][:80]
        if args.chat:
            report["chat"] = workflow["chat"]
            report["chat_steps"] = workflow["steps"]
            # The store's own view of the paging probe, so the cursors the UI
            # reported can be checked against the pages they came from.
            report["paging_store"] = {
                "project": "paging-probe",
                "ids": [row["id"] for row in stored["messages"]],
                "cursors": [row["cursor"] for row in stored["messages"]],
            }
        if args.crash:
            report["crash"] = {"policy": policy, "reloaded": True, "sessions": len(inspection["sessions"])}
        if args.reliability:
            report["dropped_reply"] = {"timeouts": inspection["timeouts"], "polls": inspection["polls"], "pending": inspection["pending"]}
            report["verbose_rpc_log"] = [line for line in transcript.splitlines() if line.startswith("rpc ")][:5]
        evidence = ("evidence/gui-reliability.json" if args.reliability
                    else "evidence/gui-workflow.json" if args.workflow
                    else "evidence/gui-chat.json" if args.chat
                    else "evidence/gui-layout.json" if args.layout
                    else "evidence/gui-crash.json" if args.crash
                    else "evidence/gui-fallback.json" if args.fallback
                    else "evidence/gui-isolation.json")
        if args.layout:
            # One line per size: the numbers are the evidence, not the exit code.
            for run in layout:
                viewport, stage, art = run["viewport"], run["stage"], run["art"]
                cell = art["cell"]
                print(f"layout {viewport['width']}x{viewport['height']}: dock={run['dock']} "
                      f"stage={stage['x']},{stage['y']} {stage['width']}x{stage['height']} "
                      f"cell={cell['x']:g},{cell['y']:g} {cell['w']:g}x{cell['h']:g} "
                      f"art={art['x']:g},{art['y']:g} {art['w']:g}x{art['h']:g} scale={art['scale']} "
                      f"chat={run['chat']['height']}px terminal={run['terminalDisplay']} "
                      f"boot/closed={run['boot_terminal']}/{run['closed_terminal']} "
                      f"splitter={run['splitter']['collapsed']}→{run['splitter']['restored']}px "
                      f"overlaps={run['overlaps'] or 'none'} → {run['screenshot']}")
        (ROOT / evidence).write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
