# apps/navi/scripts/test_project_review_testrun.py
import hashlib
import io
import json
import os
import shutil
import stat
import sys
import tempfile
import unittest
from unittest import mock

_WORKERS_DIR = os.path.abspath(
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "workers")
)
if _WORKERS_DIR not in sys.path:
    sys.path.insert(0, _WORKERS_DIR)

import project_review_testrun

REV_ID = "review-" + "0123456789abcdef" * 2
DIGEST = "a" * 64


def _mkdir700(path):
    os.makedirs(path, mode=0o700, exist_ok=True)
    os.chmod(path, 0o700)
    return path


def _write_file(path, data):
    _mkdir700(os.path.dirname(path))
    with open(path, "wb") as handle:
        handle.write(data)
    os.chmod(path, 0o600)


def _snapshot_tree(root):
    dirs = []
    files = {}
    for cur, subdirs, names in os.walk(root, followlinks=False):
        subdirs.sort()
        rel_cur = os.path.relpath(cur, root)
        if rel_cur != ".":
            dirs.append(rel_cur)
        for name in sorted(names):
            full = os.path.join(cur, name)
            rel = os.path.relpath(full, root)
            if os.path.islink(full):
                files[rel] = ("link", os.readlink(full))
            else:
                with open(full, "rb") as handle:
                    files[rel] = ("file", handle.read())
    return dirs, files


class ProjectReviewTestrunTests(unittest.TestCase):
    def setUp(self):
        raw_root = tempfile.mkdtemp(
            prefix="navi-test-", dir=os.path.expanduser("~")
        )
        os.chmod(raw_root, 0o700)
        self.root = os.path.realpath(raw_root)
        self.data_root = _mkdir700(os.path.join(self.root, "data"))
        self.ws_root = _mkdir700(os.path.join(self.root, "ws"))

    def tearDown(self):
        shutil.rmtree(self.root, ignore_errors=True)

    def _mk_home_dir(self, prefix):
        raw = tempfile.mkdtemp(prefix=prefix, dir=self.root)
        os.chmod(raw, 0o700)
        return os.path.realpath(raw)

    def test_copy_workspace_tree_filters_and_modes(self):
        src = self._mk_home_dir("src-")
        dst = self._mk_home_dir("dst-")
        _write_file(os.path.join(src, "src", "a.py"), b"x=1\n")
        _write_file(os.path.join(src, "tests", "test_a.py"), b"pass\n")
        _write_file(os.path.join(src, ".git", "config"), b"git\n")
        _write_file(os.path.join(src, "node_modules", "p", "i.js"), b"js\n")
        _write_file(os.path.join(src, "__pycache__", "a.pyc"), b"pyc\n")
        _write_file(os.path.join(src, ".venv", "bin", "py"), b"py\n")
        _write_file(os.path.join(src, ".env"), b"K=1\n")
        _write_file(os.path.join(src, ".env.local"), b"K=2\n")
        _write_file(os.path.join(src, "my_SECRET.txt"), b"sec\n")
        _write_file(os.path.join(src, "gcp_Credentials.json"), b"cred\n")
        _write_file(os.path.join(src, "id_rsa"), b"key\n")
        os.symlink(os.path.join(src, "src", "a.py"), os.path.join(src, "link.py"))
        os.symlink(os.path.join(src, "src"), os.path.join(src, "link_dir"))

        count, total = project_review_testrun.copy_workspace_tree(src, dst)
        self.assertEqual(count, 2)
        self.assertEqual(total, len(b"x=1\n") + len(b"pass\n"))

        copied_a = os.path.join(dst, "src", "a.py")
        copied_t = os.path.join(dst, "tests", "test_a.py")
        with open(copied_a, "rb") as handle:
            self.assertEqual(handle.read(), b"x=1\n")
        with open(copied_t, "rb") as handle:
            self.assertEqual(handle.read(), b"pass\n")
        self.assertEqual(stat.S_IMODE(os.lstat(copied_a).st_mode), 0o600)
        self.assertEqual(
            stat.S_IMODE(os.lstat(os.path.join(dst, "src")).st_mode), 0o700
        )

        for skipped in (
            ".git",
            "node_modules",
            "__pycache__",
            ".venv",
            ".env",
            ".env.local",
            "my_SECRET.txt",
            "gcp_Credentials.json",
            "id_rsa",
            "link.py",
            "link_dir",
        ):
            self.assertFalse(os.path.lexists(os.path.join(dst, skipped)), skipped)

    def test_copy_workspace_tree_enforces_limits(self):
        src_files = self._mk_home_dir("lim-files-")
        _write_file(os.path.join(src_files, "a.txt"), b"1")
        _write_file(os.path.join(src_files, "b.txt"), b"2")
        with self.assertRaises(project_review_testrun._PreviewFailure) as cm:
            project_review_testrun.copy_workspace_tree(
                src_files, self._mk_home_dir("dst-f-"), max_files=1
            )
        self.assertEqual(cm.exception.reason, project_review_testrun.REASON_TREE_TOO_LARGE)

        src_bytes = self._mk_home_dir("lim-bytes-")
        _write_file(os.path.join(src_bytes, "big.bin"), b"0123456789")
        with self.assertRaises(project_review_testrun._PreviewFailure) as cm:
            project_review_testrun.copy_workspace_tree(
                src_bytes, self._mk_home_dir("dst-b-"), max_bytes=5
            )
        self.assertEqual(cm.exception.reason, project_review_testrun.REASON_TREE_TOO_LARGE)

        src_depth = self._mk_home_dir("lim-depth-")
        _write_file(os.path.join(src_depth, "d1", "d2", "f.py"), b"x")
        with self.assertRaises(project_review_testrun._PreviewFailure) as cm:
            project_review_testrun.copy_workspace_tree(
                src_depth, self._mk_home_dir("dst-d-"), max_depth=1
            )
        self.assertEqual(cm.exception.reason, project_review_testrun.REASON_TREE_TOO_LARGE)

    def test_apply_proposed_edit_and_reject_bad_paths(self):
        dst = self._mk_home_dir("edit-")
        _write_file(os.path.join(dst, "src", "a.py"), b"x=1\n")

        project_review_testrun._apply_proposed_edit(dst, "src/a.py", b"x=2\n")
        with open(os.path.join(dst, "src", "a.py"), "rb") as handle:
            self.assertEqual(handle.read(), b"x=2\n")
        self.assertEqual(
            stat.S_IMODE(os.lstat(os.path.join(dst, "src", "a.py")).st_mode), 0o600
        )

        project_review_testrun._apply_proposed_edit(dst, "new/sub/b.py", b"y=3\n")
        with open(os.path.join(dst, "new", "sub", "b.py"), "rb") as handle:
            self.assertEqual(handle.read(), b"y=3\n")
        self.assertEqual(
            stat.S_IMODE(os.lstat(os.path.join(dst, "new", "sub")).st_mode), 0o700
        )

        for bad in (
            "",
            "../x.py",
            "src/../x.py",
            "/etc/passwd",
            "/a.py",
            "src\\a.py",
            "src//a.py",
            "src/a.py/",
            "./a.py",
            "a\x00b.py",
        ):
            with self.assertRaises(project_review_testrun._PreviewFailure, msg=bad):
                project_review_testrun._apply_proposed_edit(dst, bad, b"z\n")

    def test_cleanup_temp_dir_restrictions(self):
        uid = os.geteuid()
        good = self._mk_home_dir("navi-testrun-")
        _write_file(os.path.join(good, "f.txt"), b"hi")
        st = os.lstat(good)
        project_review_testrun._cleanup_temp_dir(good, (st.st_dev, st.st_ino), uid)
        self.assertFalse(os.path.exists(good))

        other = self._mk_home_dir("other-dir-")
        st_other = os.lstat(other)
        project_review_testrun._cleanup_temp_dir(
            other, (st_other.st_dev, st_other.st_ino), uid
        )
        self.assertTrue(os.path.isdir(other))

        target = self._mk_home_dir("target-")
        sym = os.path.join(self.root, "navi-testrun-symlink")
        os.symlink(target, sym)
        project_review_testrun._cleanup_temp_dir(sym, None, uid)
        self.assertTrue(os.path.islink(sym))
        self.assertTrue(os.path.isdir(target))

        wrong_id = self._mk_home_dir("navi-testrun-")
        project_review_testrun._cleanup_temp_dir(wrong_id, (-1, -1), uid)
        self.assertTrue(os.path.isdir(wrong_id))

    def test_map_sandbox_reply(self):
        long_out = "A" * 500 + "/host/secret/path " + "B" * 4200
        reply = project_review_testrun._map_sandbox_reply(
            REV_ID,
            {
                "schema": "navi-test-sandbox-v1",
                "ok": True,
                "reason": None,
                "exit_code": 0,
                "passed": True,
                "stdout_tail": long_out,
                "stderr_tail": "err at /host/secret/path",
            },
            ["/host/secret/path"],
        )
        self.assertEqual(
            set(reply.keys()),
            {"schema", "review_id", "ok", "passed", "exit_code", "stdout_tail", "stderr_tail"},
        )
        self.assertTrue(reply["ok"])
        self.assertTrue(reply["passed"])
        self.assertEqual(reply["exit_code"], 0)
        self.assertEqual(len(reply["stdout_tail"]), 4000)
        self.assertEqual(reply["stdout_tail"], "B" * 4000)
        self.assertNotIn("/host/secret/path", reply["stdout_tail"])
        self.assertNotIn("/host/secret/path", reply["stderr_tail"])
        self.assertIn("<redacted>", reply["stderr_tail"])

        for known in (
            "invalid_request",
            "sandbox_unavailable",
            "busy",
            "timed_out",
            "output_too_large",
            "failed_to_start",
        ):
            r = project_review_testrun._map_sandbox_reply(
                REV_ID, {"ok": False, "reason": known}, []
            )
            self.assertFalse(r["ok"])
            self.assertEqual(r["reason"], known)

        unk = project_review_testrun._map_sandbox_reply(
            REV_ID, {"ok": False, "reason": "something_weird"}, []
        )
        self.assertEqual(unk["reason"], "sandbox_unavailable")

        for bad in (
            None,
            "oops",
            {"ok": True, "passed": 1, "exit_code": 0, "stdout_tail": "", "stderr_tail": ""},
            {"ok": True, "passed": True, "exit_code": True, "stdout_tail": "", "stderr_tail": ""},
            {"ok": True, "passed": True, "exit_code": 0, "stdout_tail": 9, "stderr_tail": ""},
        ):
            r = project_review_testrun._map_sandbox_reply(REV_ID, bad, [])
            self.assertFalse(r["ok"])
            self.assertEqual(r["reason"], "sandbox_unavailable")

    def test_parse_request_frame(self):
        base = {
            "schema": project_review_testrun.REQUEST_SCHEMA,
            "data_root": self.data_root,
            "workspace_root": self.ws_root,
            "review_id": REV_ID,
            "proposal_digest": DIGEST,
        }
        parsed = project_review_testrun._parse_request_frame(
            (json.dumps(base) + "\n").encode("utf-8")
        )
        self.assertEqual(parsed[2], REV_ID)
        self.assertEqual(parsed[3], DIGEST)

        extra = dict(base, extra=1)
        wrong_schema = dict(base, schema="wrong-v1")
        dup = (
            '{"schema":"navi-review-testrun-request-v1","data_root":'
            + json.dumps(self.data_root)
            + ',"workspace_root":'
            + json.dumps(self.ws_root)
            + ',"review_id":"'
            + REV_ID
            + '","review_id":"'
            + REV_ID
            + '","proposal_digest":"'
            + DIGEST
            + '"}\n'
        ).encode("utf-8")
        nan_frame = (
            '{"schema":"navi-review-testrun-request-v1","data_root":'
            + json.dumps(self.data_root)
            + ',"workspace_root":'
            + json.dumps(self.ws_root)
            + ',"review_id":NaN,"proposal_digest":"'
            + DIGEST
            + '"}\n'
        ).encode("utf-8")
        no_nl = json.dumps(base).encode("utf-8")

        for bad_frame in (
            (json.dumps(extra) + "\n").encode("utf-8"),
            dup,
            nan_frame,
            no_nl,
            (json.dumps(wrong_schema) + "\n").encode("utf-8"),
        ):
            with self.assertRaises(project_review_testrun._RequestError) as cm:
                project_review_testrun._parse_request_frame(bad_frame)
            self.assertEqual(cm.exception.reason, project_review_testrun.REASON_INVALID_REQUEST)

    def test_run_review_testrun_fake_review_and_stale(self):
        _write_file(os.path.join(self.ws_root, "src", "a.py"), b"x=1\n")
        _write_file(os.path.join(self.ws_root, "tests", "test_a.py"), b"pass\n")
        before_snap = _snapshot_tree(self.ws_root)
        before_sha = hashlib.sha256(b"x=1\n").hexdigest()
        seen = {}

        def fake_sandbox(raw):
            req = json.loads(raw.decode("utf-8"))
            seen["req"] = req
            tree_root = req["tree_root"]
            with open(os.path.join(tree_root, "src", "a.py"), "rb") as handle:
                seen["copy_bytes"] = handle.read()
            with open(os.path.join(self.ws_root, "src", "a.py"), "rb") as handle:
                seen["real_during"] = handle.read()
            return {
                "schema": "navi-test-sandbox-v1",
                "ok": True,
                "reason": None,
                "exit_code": 0,
                "passed": True,
                "stdout_tail": f"ran {tree_root} and {self.ws_root}",
                "stderr_tail": "",
            }

        with mock.patch.object(
            project_review_testrun,
            "_inspect_and_read_review",
            return_value=("src/a.py", before_sha, b"x=1\n", b"x=2\n"),
        ), mock.patch.object(
            project_review_testrun, "_check_workspace_stale", return_value=False
        ), mock.patch.object(
            project_review_testrun.project_test_sandbox,
            "handle_request",
            side_effect=fake_sandbox,
        ):
            req_frame = (
                json.dumps(
                    {
                        "schema": project_review_testrun.REQUEST_SCHEMA,
                        "data_root": self.data_root,
                        "workspace_root": self.ws_root,
                        "review_id": REV_ID,
                        "proposal_digest": DIGEST,
                    }
                )
                + "\n"
            ).encode("utf-8")
            out = io.BytesIO()
            rc = project_review_testrun.main([], stdin=io.BytesIO(req_frame), stdout=out)

        self.assertEqual(rc, 0)
        raw_out = out.getvalue()
        self.assertTrue(raw_out.endswith(b"\n"))
        reply = json.loads(raw_out.decode("utf-8"))
        self.assertTrue(reply["ok"])
        self.assertTrue(reply["passed"])
        self.assertEqual(reply["exit_code"], 0)
        self.assertEqual(seen["req"]["test_argv_id"], "unittest_discover_tests")
        self.assertEqual(seen["req"]["timeout_seconds"], 30)
        self.assertEqual(seen["copy_bytes"], b"x=2\n")
        self.assertEqual(seen["real_during"], b"x=1\n")
        self.assertFalse(os.path.exists(seen["req"]["tree_root"]))
        self.assertNotIn(seen["req"]["tree_root"], raw_out.decode("utf-8"))
        self.assertNotIn(self.ws_root, raw_out.decode("utf-8"))
        self.assertEqual(_snapshot_tree(self.ws_root), before_snap)

        calls = []
        with mock.patch.object(
            project_review_testrun,
            "_inspect_and_read_review",
            return_value=("src/a.py", before_sha, b"x=1\n", b"x=2\n"),
        ), mock.patch.object(
            project_review_testrun, "_check_workspace_stale", return_value=True
        ), mock.patch.object(
            project_review_testrun.project_test_sandbox,
            "handle_request",
            side_effect=lambda raw: calls.append(raw),
        ):
            stale_reply = project_review_testrun.run_review_testrun(
                self.data_root, self.ws_root, REV_ID, DIGEST
            )
        self.assertFalse(stale_reply["ok"])
        self.assertEqual(stale_reply["reason"], project_review_testrun.REASON_STALE_TARGET)
        self.assertEqual(calls, [])
        self.assertEqual(_snapshot_tree(self.ws_root), before_snap)


if __name__ == "__main__":
    unittest.main()
