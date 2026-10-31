"""Exercise the production display host only under the isolated test environment."""
import json
import os
from pathlib import Path
import signal
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from window import Station, GLib, GLibUnix, Gtk, WebKit2


class WorkflowStation(Station):
    def __init__(self):
        if os.environ.get("NAVI_ISOLATED") != "1":
            raise RuntimeError("GUI workflow requires private state and display")
        super().__init__()
        self.deadline = time.monotonic() + 30
        self.checking = False
        self.finished = False
        self.view.connect("load-changed", self.loaded)
        GLib.timeout_add(200, self.check)

    def loaded(self, view, event):
        if event == WebKit2.LoadEvent.FINISHED:
            self.evaluate((ROOT / "scripts/gui_workflow.js").read_text())

    def check(self):
        if self.finished or self.closed:
            return False
        if self.checking:
            return True
        self.checking = True
        def inspect(view, result, unused):
            self.checking = False
            try:
                report = json.loads(view.evaluate_javascript_finish(result).to_string())
                if report is None and time.monotonic() < self.deadline:
                    return
                if report is None:
                    report = {"ok": False, "error": "GUI workflow timed out"}
            except (GLib.Error, ValueError) as error:
                report = {"ok": False, "error": str(error)}
            self.finished = True
            output = Path(os.environ["NAVI_GUI_REPORT"])
            output.with_suffix(".workflow.json").write_text(json.dumps(report, indent=2) + "\n")
            os.environ["NAVI_QA_OUTPUT"] = str(output)
            self.capture_qa()
        self.evaluate("JSON.stringify(window.naviQA || null)", inspect)
        return True


if __name__ == "__main__":
    GLib.set_prgname("navi-isolated-workflow")
    station = WorkflowStation()
    for number in (signal.SIGTERM, signal.SIGINT):
        GLibUnix.signal_add(GLib.PRIORITY_DEFAULT, number, lambda: station.destroy() or False)
    station.show_all()
    Gtk.main()
