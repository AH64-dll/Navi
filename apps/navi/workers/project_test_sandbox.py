# apps/navi/workers/project_test_sandbox.py
import errno, fcntl, json, os, re, resource, select, shutil, signal, socket, subprocess as sp, sys, time
from stat import S_IMODE, S_ISDIR, S_ISREG

REQ_SCHEMA = "navi-test-sandbox-request-v1"
REP_SCHEMA = "navi-test-sandbox-v1"
REQ_KEYS = {"schema", "tree_root", "test_argv_id", "timeout_seconds"}
INV, UNAVAIL = "invalid_request", "sandbox_unavailable"
REASONS = {INV, UNAVAIL, "busy", "timed_out", "output_too_large", "failed_to_start"}
ALLOWED_TEST_ARGVS = {
  "unittest_discover_tests": ("python3", "-I", "-m", "unittest", "discover", "-s", "tests"),
  "unittest_discover_root": ("python3", "-I", "-m", "unittest", "discover", "-s", "."),
}
MAX_STREAM = 16384
NOF = os.O_NOFOLLOW | os.O_CLOEXEC
DIRF = os.O_RDONLY | os.O_DIRECTORY | NOF
ABS_PATH_RE = re.compile(r"(?<![A-Za-z0-9_.-])/(?:[^\s'\"`()\[\]{}<>,;:]+)")

def _ok_dir(st, uid):
  return S_ISDIR(st.st_mode) and st.st_uid == uid and S_IMODE(st.st_mode) == 0o700

def _reply(ok, reason=None, rc=None, passed=False, out="", err=""):
  return {
    "schema": REP_SCHEMA, "ok": ok,
    "reason": None if ok else (reason if reason in REASONS else UNAVAIL),
    "exit_code": rc, "passed": bool(passed),
    "stdout_tail": out, "stderr_tail": err,
  }

def read_frame(fd=0, limit=8192):
  buf = bytearray()
  while len(buf) < limit:
    try: ch = os.read(fd, 1)
    except InterruptedError: continue
    except OSError: return None
    if not ch: return None
    if ch == b"\n": return bytes(buf)
    buf.extend(ch)
  return None

def sanitize_output(raw, extra):
  s = raw.decode("utf-8", errors="replace")
  for p in sorted({x.rstrip("/") for x in extra if x.startswith("/") and x.rstrip("/")}, key=len, reverse=True):
    s = re.sub(re.escape(p) + r"(?:/[^\s'\"`()\[\]{}<>,;:]*)?", "<path>", s)
  return ABS_PATH_RE.sub("<path>", s)[-4000:]

def _validate(req):
  if not isinstance(req, dict) or set(req) != REQ_KEYS or req["schema"] != REQ_SCHEMA: return None
  aid, t, root = req["test_argv_id"], req["timeout_seconds"], req["tree_root"]
  if type(aid) is not str or aid not in ALLOWED_TEST_ARGVS or type(t) is not int or not 1 <= t <= 60: return None
  if type(root) is not str or "\x00" in root or not root.startswith("/") or ".." in root.split("/"): return None
  clean = root.rstrip("/")
  if not clean: return None
  try:
    fd = os.open(clean, DIRF)
    try:
      st, real = os.fstat(fd), os.path.realpath(clean)
      if real != "/" and _ok_dir(st, os.getuid()):
        rst = os.lstat(real)
        if (rst.st_dev, rst.st_ino) == (st.st_dev, st.st_ino): return root, real, ALLOWED_TEST_ARGVS[aid], t
    finally: os.close(fd)
  except OSError: pass
  return None

def _find_bwrap():
  p = shutil.which("bwrap")
  if not p: return None
  try:
    real = os.path.realpath(p)
    st, pst = os.stat(real), os.stat(os.path.dirname(real))
  except OSError: return None
  if not S_ISREG(st.st_mode) or not os.access(real, os.X_OK) or (st.st_mode & 0o022) or (pst.st_mode & 0o002): return None
  return real

def _acquire_lock():
  uid = os.getuid()
  ldir, old = f"/tmp/navi-test-sandbox-{uid}", os.umask(0o077)
  try: os.mkdir(ldir, 0o700)
  except FileExistsError: pass
  except OSError: return None, UNAVAIL
  finally: os.umask(old)
  try:
    dfd = os.open(ldir, DIRF)
    try:
      if not _ok_dir(os.fstat(dfd), uid): return None, UNAVAIL
      lfd = os.open("sandbox.lock", os.O_RDWR | os.O_CREAT | NOF, 0o600, dir_fd=dfd)
    finally: os.close(dfd)
  except OSError: return None, UNAVAIL
  try:
    lst = os.fstat(lfd)
    if not S_ISREG(lst.st_mode) or lst.st_uid != uid or (S_IMODE(lst.st_mode) & 0o077):
      os.close(lfd); return None, UNAVAIL
    fcntl.flock(lfd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    return lfd, None
  except OSError as e:
    os.close(lfd)
    return None, "busy" if e.errno in (errno.EAGAIN, errno.EWOULDBLOCK) else UNAVAIL

def _uid_tasks(uid):
  n = 0
  try:
    for e in os.scandir("/proc"):
      if e.name.isdigit():
        try:
          if e.stat(follow_symlinks=False).st_uid == uid: n += len(os.listdir(f"/proc/{e.name}/task"))
        except OSError: pass
  except OSError: return 0
  return n

def _make_preexec(t, nproc):
  R = resource
  lims = ((R.RLIMIT_FSIZE, 16 << 20), (R.RLIMIT_NPROC, nproc), (R.RLIMIT_CPU, t + 2), (R.RLIMIT_AS, 512 << 20), (R.RLIMIT_NOFILE, 256), (R.RLIMIT_CORE, 0))
  def _apply():
    for k, lim in lims:
      _, h = R.getrlimit(k)
      v = lim if h == R.RLIM_INFINITY else min(lim, h)
      R.setrlimit(k, (v, v))
  return _apply

def _kill_pg(p, my_pg):
  if p.returncode is not None: return
  try:
    pg = os.getpgid(p.pid)
    if pg > 0 and pg == p.pid and pg != my_pg: os.killpg(pg, signal.SIGKILL)
  except ProcessLookupError: pass

def _bwrap_cmd(bwrap, root, argv):
  c = [bwrap, "--unshare-all", "--die-with-parent", "--new-session", "--clearenv",
       "--setenv", "PATH", "/usr/bin:/bin", "--setenv", "HOME", "/tmp/home", "--setenv", "LANG", "C.UTF-8"]
  for d in ("/usr", "/lib", "/lib64", "/bin"):
    if os.path.isdir(d): c += ["--ro-bind", d, d]
  return c + ["--ro-bind", root, "/work", "--tmpfs", "/tmp", "--dir", "/tmp/home", "--proc", "/proc", "--dev", "/dev", "--chdir", "/work", "--", *argv]

def _run_bounded(cmd, t, nproc):
  my_pg = os.getpgrp()
  try: p = sp.Popen(cmd, stdin=sp.DEVNULL, stdout=sp.PIPE, stderr=sp.PIPE, env={}, close_fds=True, start_new_session=True, preexec_fn=_make_preexec(t, nproc))
  except OSError: return "failed_to_start", None, b"", b""
  ob, eb = bytearray(), bytearray()
  fds = {p.stdout.fileno(): ob, p.stderr.fileno(): eb}
  for fd in fds: os.set_blocking(fd, False)
  end, st, rc = time.monotonic() + t, "ok", None
  try:
    while fds and st == "ok":
      rem = end - time.monotonic()
      if rem <= 0: st = "timed_out"; break
      try: rdy, _, _ = select.select(fds, (), (), rem)
      except InterruptedError: continue
      if not rdy: st = "timed_out"; break
      for fd in rdy:
        buf = fds[fd]
        try: ch = os.read(fd, MAX_STREAM + 1 - len(buf))
        except BlockingIOError: continue
        except OSError: ch = b""
        if not ch: del fds[fd]
        else:
          buf.extend(ch)
          if len(buf) > MAX_STREAM: del buf[MAX_STREAM:]; st = "output_too_large"; break
    if st == "ok":
      try: rc = p.wait(timeout=max(0.0, end - time.monotonic()))
      except sp.TimeoutExpired: st = "timed_out"
  finally:
    if p.returncode is None: _kill_pg(p, my_pg)
    for s in (p.stdout, p.stderr):
      try: s.close()
      except OSError: pass
    if p.returncode is None:
      try: p.wait(timeout=5)
      except sp.TimeoutExpired: _kill_pg(p, my_pg); p.wait()
  return st, rc, bytes(ob), bytes(eb)

def _self_check(bwrap, root, nproc):
  probe, port = None, 0
  try:
    probe = socket.socket(); probe.bind(("127.0.0.1", 0)); probe.listen(1); port = probe.getsockname()[1]
  except OSError:
    if probe: probe.close(); probe = None
  code = (
    "import errno as E,os,socket as S,sys\n"
    "e=os.environ;assert os.path.isdir('/work') and e.get('HOME')=='/tmp/home' and set(e)<={'PATH','HOME','LANG','PWD'}\n"
    "p=f'/work/.ro_{os.getpid()}'\n"
    "try:\n os.close(os.open(p,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600))\n"
    " try: os.unlink(p)\n except OSError: pass\n sys.exit(2)\n"
    "except OSError as x:\n if x.errno not in (E.EROFS,E.EACCES,E.EPERM): sys.exit(3)\n"
    "def c(a,ok):\n s=S.socket();s.settimeout(1)\n try: s.connect(a);sys.exit(4)\n"
    " except OSError as x:\n  if ok and x.errno not in ok: sys.exit(5)\n finally: s.close()\n"
    "c(('1.1.1.1',80),(E.ENETUNREACH,E.EHOSTUNREACH,E.EACCES,E.EPERM))\n"
    + (f"c(('127.0.0.1',{port}),())\n" if port else "") + "print(1)\n"
  )
  try:
    st, rc, out, err = _run_bounded(_bwrap_cmd(bwrap, root, ("python3", "-I", "-c", code)), 5, nproc)
    return st == "ok" and rc == 0 and out == b"1\n" and err == b""
  finally:
    if probe: probe.close()

def handle_request(raw):
  try: v = _validate(json.loads(raw)) if raw else None
  except (UnicodeDecodeError, json.JSONDecodeError): v = None
  if not v: return _reply(False, INV)
  root, real, argv, t = v
  bwrap = _find_bwrap()
  if not bwrap: return _reply(False, UNAVAIL)
  lfd, lerr = _acquire_lock()
  if lerr or lfd is None: return _reply(False, lerr or UNAVAIL)
  try:
    tasks = _uid_tasks(os.getuid())
    nproc = max(64, tasks + 128) if tasks else 256
    if not _self_check(bwrap, real, nproc): return _reply(False, UNAVAIL)
    st, rc, out, err = _run_bounded(_bwrap_cmd(bwrap, real, argv), t, nproc)
  finally: os.close(lfd)
  try: cwd = os.getcwd()
  except OSError: cwd = ""
  ex = (root, real, os.environ.get("HOME", ""), cwd)
  ot, et = sanitize_output(out, ex), sanitize_output(err, ex)
  return _reply(True, None, rc, rc == 0, ot, et) if st == "ok" else _reply(False, st, None, False, ot, et)

def main():
  sys.stdout.write(json.dumps(handle_request(read_frame(0)), separators=(",", ":")) + "\n")
  sys.stdout.flush(); return 0

if __name__ == "__main__": raise SystemExit(main())
