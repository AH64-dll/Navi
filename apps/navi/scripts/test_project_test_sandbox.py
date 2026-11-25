# apps/navi/scripts/test_project_test_sandbox.py
from contextlib import contextmanager
import fcntl, importlib.util, json, os, select, shutil, subprocess, sys, tempfile, time, unittest
from pathlib import Path
from unittest import mock

HELPER = Path(__file__).resolve().parent.parent / "workers" / "project_test_sandbox.py"
REQ_SCHEMA = "navi-test-sandbox-request-v1"
REP_SCHEMA = "navi-test-sandbox-v1"
REP_KEYS = {"schema", "ok", "reason", "exit_code", "passed", "stdout_tail", "stderr_tail"}
REASONS = {"invalid_request", "sandbox_unavailable", "busy", "timed_out", "output_too_large", "failed_to_start"}

def _bwrap_usable():
  bwrap = shutil.which("bwrap")
  if not bwrap: return False
  cmd = [bwrap, "--unshare-all", "--die-with-parent", "--new-session"]
  for d in ("/usr", "/lib", "/lib64", "/bin"):
    if os.path.isdir(d): cmd += ["--ro-bind", d, d]
  cmd += ["--tmpfs", "/tmp", "--proc", "/proc", "--dev", "/dev", "--", "python3", "-I", "-c", "print(1)"]
  try:
    r = subprocess.run(cmd, capture_output=True, timeout=5)
    return r.returncode == 0 and r.stdout == b"1\n"
  except (OSError, subprocess.SubprocessError):
    return False

BWRAP_PRESENT = shutil.which("bwrap") is not None
BWRAP_USABLE = _bwrap_usable()

def _load_helper():
  spec = importlib.util.spec_from_file_location("project_test_sandbox", HELPER)
  mod = importlib.util.module_from_spec(spec)
  spec.loader.exec_module(mod)
  return mod

class TestProjectTestSandbox(unittest.TestCase):
  @contextmanager
  def _tree(self, files):
    with tempfile.TemporaryDirectory() as root:
      os.chmod(root, 0o700)
      tdir = os.path.join(root, "tests")
      os.mkdir(tdir, 0o700)
      for name, content in files.items():
        with open(os.path.join(tdir, name), "w", encoding="utf-8") as f:
          f.write(content)
      yield root

  def _req(self, root, argv_id="unittest_discover_tests", timeout_seconds=10):
    return {"schema": REQ_SCHEMA, "tree_root": root, "test_argv_id": argv_id, "timeout_seconds": timeout_seconds}

  def _call(self, req, env=None, wait=15.0):
    payload = req if isinstance(req, bytes) else (json.dumps(req) + "\n").encode("utf-8")
    p = subprocess.Popen([sys.executable, str(HELPER)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
    try:
      p.stdin.write(payload)
      p.stdin.flush()
      # Keep stdin open while reading the single reply line.
      buf = bytearray()
      end = time.monotonic() + wait
      fd = p.stdout.fileno()
      while True:
        rem = end - time.monotonic()
        self.assertGreater(rem, 0, "helper timed out (possible EOF wait on stdin)")
        rdy, _, _ = select.select([fd], [], [], rem)
        self.assertTrue(rdy, "helper timed out waiting for reply")
        ch = os.read(fd, 1)
        if not ch or ch == b"\n": break
        buf.extend(ch)
      p.stdin.close()
      p.wait(timeout=5)
      self.assertEqual(p.returncode, 0)
    finally:
      for s in (p.stdin, p.stdout, p.stderr):
        try: s.close()
        except OSError: pass
      if p.poll() is None:
        p.kill(); p.wait()
    raw = buf.decode("utf-8")
    rep = json.loads(raw)
    self.assertEqual(set(rep), REP_KEYS)
    self.assertEqual(rep["schema"], REP_SCHEMA)
    self.assertIsInstance(rep["ok"], bool)
    self.assertIsInstance(rep["passed"], bool)
    self.assertLessEqual(len(rep["stdout_tail"]), 4000)
    self.assertLessEqual(len(rep["stderr_tail"]), 4000)
    if rep["ok"]:
      self.assertIsNone(rep["reason"])
      self.assertIsInstance(rep["exit_code"], int)
    else:
      self.assertIn(rep["reason"], REASONS)
      self.assertFalse(rep["passed"])
      self.assertIsNone(rep["exit_code"])
    return rep, raw

  def test_bwrap_absent_default_deny_and_no_spawn(self):
    canary = f"/tmp/navi_sandbox_canary_{os.getpid()}_{time.time_ns()}"
    code = f"import unittest\nclass T(unittest.TestCase):\n def test_c(self):\n  open({canary!r},'w').write('x')\n"
    no_bwrap_env = {k: v for k, v in os.environ.items() if k in ("LD_LIBRARY_PATH", "PYTHONHOME")}
    no_bwrap_env["PATH"] = "/nonexistent"
    with self._tree({"test_canary.py": code}) as root:
      envs = [no_bwrap_env] + ([None] if not BWRAP_PRESENT else [])
      for env in envs:
        rep, raw = self._call(self._req(root), env=env)
        self.assertFalse(rep["ok"])
        self.assertEqual(rep["reason"], "sandbox_unavailable")
        self.assertEqual(rep["stdout_tail"], "")
        self.assertEqual(rep["stderr_tail"], "")
        self.assertNotIn(root, raw)
        self.assertFalse(os.path.exists(canary))
      mod = _load_helper()
      with mock.patch("shutil.which", return_value=None), mock.patch("subprocess.Popen") as mp:
        rep = mod.handle_request(json.dumps(self._req(root)).encode("utf-8"))
        self.assertFalse(rep["ok"])
        self.assertEqual(rep["reason"], "sandbox_unavailable")
        mp.assert_not_called()

  @unittest.skipUnless(BWRAP_PRESENT and not BWRAP_USABLE, "only when bwrap exists but is unusable")
  def test_unusable_bwrap_default_deny(self):
    with self._tree({"test_ok.py": "import unittest\nclass T(unittest.TestCase):\n def test_a(self): pass\n"}) as root:
      rep, raw = self._call(self._req(root))
      self.assertFalse(rep["ok"])
      self.assertEqual(rep["reason"], "sandbox_unavailable")
      self.assertNotIn(root, raw)

  def test_invalid_requests(self):
    with self._tree({}) as root, tempfile.TemporaryDirectory() as other:
      os.chmod(other, 0o755)
      link = os.path.join(other, "sym")
      os.symlink(root, link)
      bad = [
        b"{not-json\n",
        {**self._req(root), "schema": "wrong"},
        {**self._req(root), "extra": 1},
        self._req(root, argv_id="custom_cmd"),
        self._req(root, timeout_seconds=0),
        self._req(root, timeout_seconds=61),
        self._req(root, timeout_seconds=True),
        self._req(other),
        self._req(link),
        self._req(link + "/"),
      ]
      for case in bad:
        rep, raw = self._call(case)
        self.assertFalse(rep["ok"])
        self.assertEqual(rep["reason"], "invalid_request")
        self.assertNotIn(root, raw)

  @unittest.skipUnless(BWRAP_USABLE, "usable bwrap unavailable")
  def test_passing_tree(self):
    code = "import unittest\nclass T(unittest.TestCase):\n def test_ok(self): self.assertEqual(2+2,4)\n"
    with self._tree({"test_ok.py": code}) as root:
      rep, raw = self._call(self._req(root))
      self.assertTrue(rep["ok"])
      self.assertTrue(rep["passed"])
      self.assertEqual(rep["exit_code"], 0)
      self.assertIn("OK", rep["stderr_tail"])
      self.assertNotIn(root, raw)

  @unittest.skipUnless(BWRAP_USABLE, "usable bwrap unavailable")
  def test_failing_tree(self):
    code = "import unittest\nclass T(unittest.TestCase):\n def test_fail(self): self.assertEqual(1,2)\n"
    with self._tree({"test_fail.py": code}) as root:
      rep, raw = self._call(self._req(root))
      self.assertTrue(rep["ok"])
      self.assertFalse(rep["passed"])
      self.assertNotEqual(rep["exit_code"], 0)
      self.assertIn("<path>", rep["stderr_tail"])
      self.assertNotIn(root, raw)
      self.assertNotIn("/work/", raw)

  @unittest.skipUnless(BWRAP_USABLE, "usable bwrap unavailable")
  def test_sleeping_times_out(self):
    code = "import time,unittest\nclass T(unittest.TestCase):\n def test_s(self): time.sleep(20)\n"
    with self._tree({"test_sleep.py": code}) as root:
      t0 = time.monotonic()
      rep, _ = self._call(self._req(root, timeout_seconds=1), wait=10.0)
      dt = time.monotonic() - t0
      self.assertFalse(rep["ok"])
      self.assertEqual(rep["reason"], "timed_out")
      self.assertLess(dt, 8.0)

  @unittest.skipUnless(BWRAP_USABLE, "usable bwrap unavailable")
  def test_write_to_work_fails(self):
    code = "import unittest\nclass T(unittest.TestCase):\n def test_w(self):\n  with open('/work/escaped.txt','w') as f: f.write('x')\n"
    with self._tree({"test_w.py": code}) as root:
      rep, raw = self._call(self._req(root))
      self.assertTrue(rep["ok"])
      self.assertFalse(rep["passed"])
      self.assertNotEqual(rep["exit_code"], 0)
      self.assertFalse(os.path.exists(os.path.join(root, "escaped.txt")))
      self.assertNotIn(root, raw)

  @unittest.skipUnless(BWRAP_USABLE, "usable bwrap unavailable")
  def test_output_cap_overflow(self):
    code = "import unittest\nclass T(unittest.TestCase):\n def test_big(self): print('A'*40000,flush=True)\n"
    with self._tree({"test_big.py": code}) as root:
      rep, _ = self._call(self._req(root))
      self.assertFalse(rep["ok"])
      self.assertEqual(rep["reason"], "output_too_large")

  @unittest.skipUnless(BWRAP_USABLE, "usable bwrap unavailable")
  def test_busy_lock(self):
    ldir, old = f"/tmp/navi-test-sandbox-{os.getuid()}", os.umask(0o077)
    try: os.mkdir(ldir, 0o700)
    except FileExistsError: pass
    finally: os.umask(old)
    lfd = os.open(os.path.join(ldir, "sandbox.lock"), os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    try:
      fcntl.flock(lfd, fcntl.LOCK_EX | fcntl.LOCK_NB)
      with self._tree({"test_ok.py": "import unittest\nclass T(unittest.TestCase):\n def test_a(self): pass\n"}) as root:
        rep, _ = self._call(self._req(root))
        self.assertFalse(rep["ok"])
        self.assertEqual(rep["reason"], "busy")
    finally:
      os.close(lfd)

if __name__ == "__main__":
  unittest.main()
