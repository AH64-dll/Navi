#!/usr/bin/python3
"""Install this user's station, launcher, native host, and user service."""
import json
import argparse
import os
from pathlib import Path
import shlex
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]
DATA = Path.home() / ".local/share/navi"


def write(path, text, executable=False):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    if executable:
        path.chmod(0o755)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--no-start", action="store_true", help="Install files without starting background services")
    args = parser.parse_args()
    target = Path(os.environ.get("CARGO_TARGET_DIR", "/tmp/navi-cargo-target"))
    subprocess.run(["cargo", "build", "--release", "--locked", "--manifest-path", str(ROOT / "Cargo.toml")], env={**os.environ, "CARGO_TARGET_DIR": str(target)}, check=True)
    binary = DATA / "bin/navid"
    binary.parent.mkdir(parents=True, exist_ok=True)
    temporary_binary = binary.with_suffix(".new")
    shutil.copy2(target / "release/navid", temporary_binary)
    temporary_binary.replace(binary)
    launcher = Path.home() / ".local/bin/navi-station"
    write(launcher, f"#!/bin/bash\nexport NAVI_APP_DIR={shlex.quote(str(ROOT))}\nif ! {shlex.quote(str(binary))} rpc '{{\"op\":\"ping\"}}' >/dev/null 2>&1; then\n  systemctl --user start navi-station.service\nfi\nexec /usr/bin/python3 {shlex.quote(str(ROOT / 'window.py'))} \"$@\"\n", True)
    native = DATA / "bin/native-host"
    write(native, f"#!/bin/bash\nexec {shlex.quote(str(binary))} native \"$@\"\n", True)
    manifest = {"name":"dev.navi.station", "description":"Navi Rust orchestration station", "path":str(native), "type":"stdio", "allowed_extensions":["station@navi.local"]}
    write(Path.home() / ".mozilla/native-messaging-hosts/dev.navi.station.json", json.dumps(manifest, indent=2)+"\n")
    write(DATA / "tmux.conf", "set -g exit-empty off\nset -g status off\nset -g mouse on\nset -g default-terminal tmux-256color\n")
    write(Path.home() / ".config/systemd/user/navi-terminals.service", f"[Unit]\nDescription=Navi persistent terminal server\n\n[Service]\nExecStart=/usr/bin/tmux -D -S {DATA / 'terminals.sock'} -f {DATA / 'tmux.conf'}\nRestart=on-failure\nRestartSec=1\n\n[Install]\nWantedBy=default.target\n")
    write(Path.home() / ".config/systemd/user/navi-station.service", f"[Unit]\nDescription=Navi orchestration station\nAfter=graphical-session.target navi-terminals.service\nWants=navi-terminals.service\n\n[Service]\nType=simple\nEnvironment=NAVI_APP_DIR={ROOT}\nEnvironment=NAVI_MANAGED_TERMINALS=1\nEnvironment=PATH={os.environ.get('PATH','/usr/local/bin:/usr/bin:/bin')}\nExecStart={binary} serve\nRestart=on-failure\nRestartSec=2\nKillMode=control-group\n\n[Install]\nWantedBy=default.target\n")
    write(Path.home() / ".local/share/applications/navi-station.desktop", f"[Desktop Entry]\nType=Application\nName=Navi Station\nComment=Local voice and terminal orchestration\nExec={launcher}\nTerminal=false\nCategories=Development;Utility;\nStartupWMClass=navi-station\n")
    subprocess.run(["systemctl","--user","daemon-reload"],check=True)
    if not args.no_start:
        subprocess.run(["systemctl","--user","enable","--now","navi-terminals.service","navi-station.service"],check=True)
    print(f"Installed {launcher}")


if __name__ == "__main__":
    main()
