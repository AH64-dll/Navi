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
    temporary = path.with_name(path.name + ".new")
    temporary.write_text(text)
    if executable:
        temporary.chmod(0o755)
    os.replace(temporary, path)


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
    window = str(ROOT / "window.py")
    script = (
        "#!/bin/bash\n"
        f"export NAVI_APP_DIR={shlex.quote(str(ROOT))}\n"
        f"if pgrep -f {shlex.quote('python3 ' + window)} >/dev/null 2>&1; then\n"
        "  echo 'Navi window is already open'\n"
        "  exit 0\n"
        "fi\n"
        f"if ! {shlex.quote(str(binary))} rpc '{{\"op\":\"ping\"}}' >/dev/null 2>&1; then\n"
        "  systemctl --user start navi-station.service\n"
        "fi\n"
        f"exec /usr/bin/python3 {shlex.quote(window)} \"$@\"\n"
    )
    write(launcher, script, True)
    write(Path.home() / ".local/bin/navi", script, True)
    # The assistant's prompt and the help text call `navid` by name from any shell.
    on_path = Path.home() / ".local/bin/navid"
    temporary_link = on_path.with_name("navid.new")
    temporary_link.unlink(missing_ok=True)
    temporary_link.symlink_to(binary)
    temporary_link.replace(on_path)
    native = DATA / "bin/native-host"
    write(native, f"#!/bin/bash\nexec {shlex.quote(str(binary))} native \"$@\"\n", True)
    manifest = {"name":"dev.navi.station", "description":"Navi Rust orchestration station", "path":str(native), "type":"stdio", "allowed_extensions":["station@navi.local"]}
    write(Path.home() / ".mozilla/native-messaging-hosts/dev.navi.station.json", json.dumps(manifest, indent=2)+"\n")
    write(DATA / "tmux.conf", "set -g exit-empty off\nset -g status off\nset -g mouse on\nset -g default-terminal tmux-256color\n")
    # BUG-072: no frozen PATH — the daemon composes its launch PATH at runtime
    # from NAVI_SYSTEM_PATH + this user-path snapshot + the login shell.
    home_path = str(Path.home())
    login_dirs = [p for p in os.environ.get("PATH","").split(":") if p.startswith(home_path)]
    user_default = ":".join(dict.fromkeys(login_dirs + [f"{home_path}/.local/bin"]))
    write(Path.home() / ".config/systemd/user/navi-station.service", f"[Unit]\nDescription=Navi orchestration station\nAfter=graphical-session.target navi-terminals.service\nWants=navi-terminals.service\n\n[Service]\nType=simple\nEnvironment=NAVI_APP_DIR={ROOT}\nEnvironment=NAVI_MANAGED_TERMINALS=1\nEnvironment=NAVI_SYSTEM_PATH=/usr/local/bin:/usr/bin:/bin\nEnvironment=NAVI_USER_PATH_DEFAULT={user_default}\nExecStart={binary} serve\nRestart=on-failure\nRestartSec=2\nKillMode=control-group\n\n[Install]\nWantedBy=default.target\n")
    write(Path.home() / ".local/share/applications/navi-station.desktop", f"[Desktop Entry]\nType=Application\nName=Navi Station\nComment=Local voice and terminal orchestration\nExec={launcher}\nTerminal=false\nCategories=Development;Utility;\nStartupWMClass=navi-station\n")
    subprocess.run(["systemctl","--user","daemon-reload"],check=True)
    if not args.no_start:
        subprocess.run(["systemctl","--user","enable","--now","navi-terminals.service","navi-station.service"],check=True)
    print(f"Installed {launcher} and {Path.home() / '.local/bin/navi'}")


if __name__ == "__main__":
    main()
